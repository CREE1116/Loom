//! L7: headless entry point and structured benchmark projection, no UI dependency.
use crate::{
    base::TaskId,
    control::{Core, CoreEvent, ExecutionOptions, Task, TaskIntent, TaskStatus},
    models::{self, ModelPort, Need, ScriptedModel},
    worker::Budget,
};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use serde_json::{Value, json};
use std::{
    fs,
    io::{Read, Write},
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Parser)]
#[command(
    name = "loom",
    version,
    about = "Loom Native Core: isolated proposals, verified application"
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    #[arg(long, hide = true)]
    run: Option<String>,
    #[arg(long, global = true, default_value = ".")]
    cwd: PathBuf,
    #[arg(long, global = true, default_value = "local")]
    tier: String,
    #[arg(long, global = true)]
    max_cost: Option<String>,
    #[arg(long, global = true, default_value_t = 40)]
    max_steps: usize,
    #[arg(long, global = true, default_value_t = 300)]
    max_seconds: u64,
    #[arg(long, global = true, default_value_t = 1_000_000)]
    max_tokens: u64,
    #[arg(long, global = true, default_value_t = 8)]
    max_attempts: usize,
    #[arg(long, global = true)]
    read_only: bool,
    #[arg(long, global = true, default_value = "auto", value_parser = ["auto", "answer", "inspect", "edit"])]
    intent: String,
    #[arg(long, global = true)]
    no_approve: bool,
    #[arg(long, global = true)]
    check: Vec<String>,
    #[arg(long, global = true)]
    trace: Option<PathBuf>,
    #[arg(long, global = true)]
    summary_file: Option<PathBuf>,
    #[arg(long, global = true)]
    json: bool,
    #[arg(long, global = true)]
    with_context: bool,
    #[arg(long, global = true, hide = true)]
    script: Option<PathBuf>,
    #[arg(long, global = true, default_value = "native")]
    session: String,
    #[arg(long, global = true)]
    skill: Vec<PathBuf>,
    #[arg(long, global = true)]
    image: Vec<PathBuf>,
    #[arg(long, global = true)]
    effort: Option<String>,
    #[arg(long, global = true)]
    model: Option<String>,
    #[arg(long,global=true,default_value="workspace-write",value_parser=["read-only","workspace-write","full-access"])]
    sandbox: String,
    #[arg(long, global = true)]
    network: bool,
    #[arg(long, global = true, default_value_t = 96_000)]
    context_limit: usize,
}
#[derive(Subcommand)]
enum Command {
    Run {
        goal: String,
    },
    Resume {
        id: String,
    },
    Approve {
        id: String,
    },
    Status,
    Session {
        #[command(subcommand)]
        action: SessionAction,
    },
    Goal {
        action: String,
        objective: Option<String>,
        #[arg(long)]
        token_budget: Option<u64>,
    },
    Models,
}
#[derive(Subcommand)]
enum SessionAction {
    List,
    New,
    Show { id: Option<String> },
    Fork { id: String, turn: String },
    Rewind { id: String, turn: String },
    Compact { id: Option<String> },
}

pub fn model(script: Option<&std::path::Path>) -> Result<Arc<dyn ModelPort>> {
    if let Some(path) = script {
        let replies: Vec<models::Response> = serde_json::from_slice(&fs::read(path)?)?;
        Ok(Arc::new(ScriptedModel::with_candidate(
            models::router::Candidate {
                endpoint: "scripted".into(),
                model: "scripted".into(),
                tier: models::Tier::Flagship,
                cost: models::CostClass::Local,
                price: Some((0.0, 0.0)),
                tools: true,
                context: None,
            },
            replies.into_iter().map(Ok),
        )))
    } else {
        models::registry::Registry::open()?.config()?;
        let (router, _) = models::discover::discover(models::cli::ledger_path());
        Ok(Arc::new(router))
    }
}

pub fn outcome(task: &Task) -> Value {
    let remote_input_tokens = task
        .attempts
        .iter()
        .map(|a| a.remote_input_tokens)
        .collect::<Option<Vec<_>>>()
        .map(|v| v.into_iter().sum::<u64>());
    let remote_calls = task.attempts.iter().map(|a| a.remote_calls).sum::<u32>();
    let mut input = 0;
    let mut cached = 0;
    let mut output = 0;
    let mut steps = 0;
    let mut bytes = std::collections::BTreeMap::<String, u64>::new();
    let mut models = std::collections::BTreeMap::<String, u32>::new();
    let mut tiers = Vec::new();
    for attempt in &task.attempts {
        input += attempt.input_tokens;
        cached += attempt.cached_input_tokens;
        output += attempt.output_tokens;
        steps += attempt.steps;
        if tiers.last() != Some(&attempt.tier) {
            tiers.push(attempt.tier);
        }
        for (phase, n) in &attempt.phase_bytes {
            *bytes.entry(phase.clone()).or_default() += n;
        }
        for (model, n) in &attempt.models {
            *models.entry(model.clone()).or_default() += n;
        }
    }
    json!({"id":task.id,"status":task.status,"finished":task.status==TaskStatus::Accepted,"summary":task.summary,"stop_reason":format!("{:?}",task.status),"steps":steps,
        "input_tokens":input,"remote_input_tokens":remote_input_tokens,"remote_calls":remote_calls,"cached_input_tokens":cached,"output_tokens":output,"phase_bytes":bytes,"models":models,"tiers":tiers,
        "validated":task.attempts.last().is_some_and(|a|!a.validation.is_empty()&&a.validation.iter().all(|v|v.status==crate::effects::RunStatus::Passed)),
        "slice_rereads":task.attempts.iter().map(|a|a.rereads).sum::<u32>(),"slice_bytes":task.slice_bytes,"contract_recall":task.scope.as_ref().map(|s|s.recall),"scope_events":task.scope.as_ref().map(|s|&s.events),"decision":task.decision})
}

pub fn cli() -> Result<i32> {
    cli_from(std::env::args_os())
}
pub fn cli_from(
    args: impl IntoIterator<Item = impl Into<std::ffi::OsString> + Clone>,
) -> Result<i32> {
    let args = match Args::try_parse_from(args) {
        Ok(args) => args,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            error.print()?;
            return Ok(0);
        }
        Err(error) => return Err(error.into()),
    };
    let mut core = Core::open(&args.cwd)?;
    let command = args
        .command
        .or_else(|| args.run.map(|goal| Command::Run { goal }))
        .context("use loom run <goal>, resume <id>, approve <id>, or status")?;
    if matches!(command, Command::Status) {
        println!("{}", serde_json::to_string_pretty(&core.state)?);
        return Ok(0);
    }
    let sessions = crate::sessions::Sessions::open(&args.cwd)?;
    match &command {
        Command::Session { action } => {
            let result = match action {
                SessionAction::List => serde_json::to_value(sessions.list()?)?,
                SessionAction::New => serde_json::to_value(sessions.create()?)?,
                SessionAction::Show { id } => {
                    serde_json::to_value(sessions.ensure(id.as_deref().unwrap_or(&args.session))?)?
                }
                SessionAction::Fork { id, turn } => serde_json::to_value(sessions.fork(id, turn)?)?,
                SessionAction::Rewind { id, turn } => {
                    if args.read_only || args.sandbox == "read-only" {
                        bail!("read-only execution cannot rewind code");
                    }
                    serde_json::to_value(core.rewind(id, turn)?)?
                }
                SessionAction::Compact { id } => serde_json::to_value(
                    sessions.compact(id.as_deref().unwrap_or(&args.session), 2)?,
                )?,
            };
            println!("{}", serde_json::to_string_pretty(&result)?);
            return Ok(0);
        }
        Command::Goal {
            action,
            objective,
            token_budget,
        } => {
            let session = sessions.update(&args.session, |s| {
                match action.as_str() {
                    "show" => {}
                    "set" => {
                        s.goal = Some(crate::sessions::Goal {
                            objective: objective.clone().context("goal objective required")?,
                            status: "active".into(),
                            tokens_used: 0,
                            token_budget: *token_budget,
                        })
                    }
                    "pause" | "resume" => {
                        s.goal.as_mut().context("no current goal")?.status = if action == "pause" {
                            "paused"
                        } else {
                            "active"
                        }
                        .into()
                    }
                    "clear" => s.goal = None,
                    _ => bail!("goal action must be show|set|pause|resume|clear"),
                };
                Ok(())
            })?;
            println!("{}", serde_json::to_string_pretty(&session.goal)?);
            return Ok(0);
        }
        Command::Models => {
            let model = model(args.script.as_deref())?;
            println!(
                "{}",
                serde_json::to_string_pretty(
                    &json!({"profiles":model.model_profiles(),"limits":model.limits()})
                )?
            );
            return Ok(0);
        }
        _ => {}
    }
    let need = Need {
        min_tier: models::cli::parse_tier(&args.tier).context("invalid tier")?,
        tools: true,
        max_cost: match args.max_cost.as_deref() {
            Some(cost) => models::cli::parse_cost(cost).context("invalid cost class")?,
            None => models::cost_ceiling(),
        },
    };
    if need.max_cost > models::cost_ceiling() {
        bail!("requested cost exceeds provider policy; configure loom provider policy first");
    }
    let intent = match args.intent.as_str() {
        "answer" => TaskIntent::Answer,
        "inspect" => TaskIntent::Inspect,
        "edit" => TaskIntent::Edit,
        _ => TaskIntent::Auto,
    };
    if args.read_only && intent == TaskIntent::Edit {
        bail!("--intent edit conflicts with --read-only");
    }
    let options = ExecutionOptions {
        need,
        budget: Budget {
            steps: args.max_steps,
            seconds: args.max_seconds,
            tokens: args.max_tokens,
        },
        max_attempts: args.max_attempts,
        approve: !args.no_approve,
        readonly: args.read_only,
        intent,
        checks: if args.check.is_empty() {
            None
        } else {
            Some(args.check)
        },
        use_context: args.with_context,
        session: Some(args.session),
        skills: args.skill,
        images: args
            .image
            .iter()
            .map(|path| image_part(path))
            .collect::<Result<Vec<_>>>()?,
        reasoning_effort: args.effort,
        model: args.model,
        context_limit: args.context_limit,
        policy: crate::effects::ExecutionPolicy {
            sandbox: parse_sandbox(&args.sandbox)?,
            network: args.network,
        },
        ..Default::default()
    };
    let mut trace = args
        .trace
        .as_ref()
        .map(|path| {
            fs::OpenOptions::new()
                .create(true)
                .truncate(true)
                .write(true)
                .open(path)
        })
        .transpose()?;
    let mut log = |event: CoreEvent| {
        if !args.json
            && let CoreEvent::WorkerEvent { event, .. } = &event
        {
            if let Some(message) = event["progress"]["message"].as_str() {
                let batch_activity = event["progress"]["id"]
                    .as_str()
                    .is_some_and(|id| id.contains("-progress-"));
                if !batch_activity {
                    eprintln!("{message}");
                }
            } else if let Some(description) = event["tool_started"]["description"].as_str() {
                eprintln!("실행 중: {description}");
            } else if let Some(description) = event["description"].as_str() {
                let result = event["result_summary"].as_str().unwrap_or("완료");
                let result = result
                    .strip_prefix("Result:")
                    .map_or_else(|| result.to_owned(), |rest| format!("결과:{rest}"));
                eprintln!("{description} · {result}");
            }
        }
        if let Some(file) = &mut trace {
            let line = match event {
                CoreEvent::WorkerEvent { event, .. } => event,
                event => json!({"core":event}),
            };
            let _ = writeln!(file, "{line}");
        }
    };
    install_interrupt();
    let task = match command {
        Command::Approve { id } => {
            if args.read_only {
                bail!("read-only execution cannot approve an edit task");
            }
            core.approve(&TaskId(id), &CANCEL, &mut log)?
        }
        Command::Resume { id } => {
            let model = model(args.script.as_deref())?;
            core.resume(&TaskId(id), options, model.as_ref(), &CANCEL, &mut log)?
        }
        Command::Run { mut goal } => {
            if goal == "-" {
                goal.clear();
                std::io::stdin().read_to_string(&mut goal)?;
            }
            let model = model(args.script.as_deref())?;
            core.execute(&goal, options, model.as_ref(), &CANCEL, &mut log)?
        }
        Command::Status | Command::Session { .. } | Command::Goal { .. } | Command::Models => {
            unreachable!()
        }
    };
    let result = outcome(&task);
    if let Some(file) = &mut trace {
        writeln!(file, "{}", json!({"outcome":result}))?;
    }
    if let Some(path) = args.summary_file {
        fs::write(path, &task.summary)?;
    }
    if args.json {
        println!("{result}");
    } else {
        println!("{}\n{} · {:?}", task.summary, task.id, task.status);
    }
    Ok(if task.status == TaskStatus::Accepted {
        0
    } else {
        2
    })
}
static CANCEL: AtomicBool = AtomicBool::new(false);
fn install_interrupt() {
    CANCEL.store(false, Ordering::SeqCst);
    #[cfg(unix)]
    {
        extern "C" fn stop(_: libc::c_int) {
            CANCEL.store(true, Ordering::SeqCst);
        }
        unsafe {
            libc::signal(libc::SIGINT, stop as *const () as libc::sighandler_t);
            libc::signal(libc::SIGTERM, stop as *const () as libc::sighandler_t);
        }
    }
}

pub fn parse_sandbox(value: &str) -> Result<crate::effects::SandboxMode> {
    match value {
        "read-only" | "readOnly" => Ok(crate::effects::SandboxMode::ReadOnly),
        "workspace-write" | "workspaceWrite" => Ok(crate::effects::SandboxMode::WorkspaceWrite),
        "full-access" | "dangerFullAccess" => Ok(crate::effects::SandboxMode::FullAccess),
        _ => bail!("invalid sandbox mode"),
    }
}
pub fn image_part(path: &std::path::Path) -> Result<crate::models::Part> {
    crate::models::image_part(path)
}
