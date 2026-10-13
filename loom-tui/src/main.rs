use anyhow::{Context, Result, bail};
use clap::{CommandFactory, FromArgMatches, Parser};
use crossterm::{
    event::{self, Event, KeyEventKind},
    terminal,
};
use loom_tui::{
    agent::{AgentBackend, AgentCommand},
    app::{App, Intent, View},
    backend::mock::MockCore,
    core::{backend::CoreBackend, repository::RepositoryExplorer},
    engine::Terminal,
    preferences, runtime, window,
};
use std::{
    io::IsTerminal,
    path::PathBuf,
    sync::mpsc,
    time::{Duration, Instant},
};

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Loom — a coding agent runtime with a replaceable Core"
)]
struct Args {
    /// Runtime backend: Loom's Core (default) or the earlier native worker.
    #[arg(long,default_value="loom",value_parser=["loom","native"])]
    backend: String,
    /// Developer-only native Core benchmark.
    #[arg(long, value_parser=["loom"], hide = true)]
    bench_core: Option<String>,
    /// Execution contract for headless comparisons; auto uses semantic planning.
    #[arg(long, default_value="edit", value_parser=["auto","edit","inspect","answer"], requires="bench_core")]
    core_intent: String,
    /// Approve a saved version proposal headlessly; requires --bench-core.
    #[arg(long, requires = "bench_core")]
    core_approve: Option<String>,
    #[arg(long)]
    core_state_dir: Option<PathBuf>,
    #[arg(long, hide = true)]
    core_run_reference: Option<String>,
    /// Objective checks for the developer comparison runner (repeatable).
    #[arg(long)]
    core_check: Vec<String>,
    #[arg(long, default_value_t = 3)]
    core_attempts: usize,
    /// Per-request timeout for Loom and the comparison adapter.
    #[arg(long, default_value_t = 45)]
    core_call_seconds: u64,
    #[arg(long)]
    core_effort: Option<String>,
    /// Coverage reviewer; auto prefers the strongest available eligible model.
    #[arg(long, default_value = "auto")]
    core_review_model: String,
    /// Measurement only: run candidate graph groups without claiming calibrated benefit.
    #[arg(long, requires = "bench_core")]
    core_split_experiment: bool,
    #[arg(long, requires = "bench_core")]
    core_edit_handoff: bool,
    #[arg(long, requires = "bench_core")]
    core_write_file: Vec<String>,
    #[arg(long)]
    no_approve: bool,
    #[arg(long, hide = true, requires = "backend")]
    script: Option<PathBuf>,
    #[arg(long, default_value = ".")]
    cwd: PathBuf,
    /// Search the shared local code index without a model call.
    #[arg(long, conflicts_with_all = ["demo", "list_skills", "resume", "new"])]
    explore: Option<String>,
    /// Override the model for this session.
    #[arg(long)]
    model: Option<String>,
    /// List installed skills for this workspace without inference.
    #[arg(long, conflicts_with = "demo")]
    list_skills: bool,
    /// Resume a thread ID, or use "last" for this workspace's saved thread.
    #[arg(long)]
    resume: Option<String>,
    /// Start a separate conversation instead of reopening this folder's recent one.
    #[arg(long, conflicts_with = "resume")]
    new: bool,
    /// Exercise the UI with clearly labeled fixture data; no inference or tools run.
    #[arg(long)]
    demo: bool,
    /// List reachable model sources and the cheapest-first routing order.
    #[arg(long)]
    models: bool,
    /// Route one prompt to the cheapest capable model and print the answer.
    #[arg(long, value_name = "PROMPT")]
    ask: Option<String>,
    /// Run one task with the native worker in --cwd ("-" reads the task from stdin).
    /// Only local, free and subscription models are used; paid models are blocked.
    #[arg(long, value_name = "TASK")]
    run: Option<String>,
    #[arg(long, default_value_t = 40)]
    max_steps: usize,
    /// Wall-clock budget for --run, in seconds.
    #[arg(long)]
    max_seconds: Option<u64>,
    /// Worker may only read (no edit, write or check).
    #[arg(long)]
    read_only: bool,
    #[arg(long,default_value="workspace-write",value_parser=["read-only","workspace-write","full-access"])]
    native_sandbox: String,
    #[arg(long)]
    native_network: bool,
    /// JSONL trace of every worker step.
    #[arg(long, value_name = "PATH")]
    trace: Option<PathBuf>,
    /// Write the worker's final summary here.
    #[arg(long, value_name = "PATH")]
    summary_file: Option<PathBuf>,
    /// Today's use of each provider's free tier.
    #[arg(long)]
    free_tiers: bool,
    /// Download a model: hf:owner/repo[/file.gguf] (best quant for this machine) or ollama:name.
    #[arg(long, value_name = "SPEC")]
    pull: Option<String>,
    /// Serve a GGUF path or hf: spec with llama-server on --port until interrupted.
    #[arg(long, value_name = "SPEC")]
    serve: Option<String>,
    #[arg(long, default_value_t = 8080)]
    port: u16,
    /// Per-model usage statistics from ~/.loom/model-usage.jsonl.
    #[arg(long)]
    model_stats: bool,
    /// Minimum tier for --models/--ask: local, small, medium, flagship.
    #[arg(long, default_value = "local")]
    tier: String,
    /// Highest cost class allowed: local, free, subscription, paid, any.
    #[arg(long, default_value = "any")]
    max_cost: String,
    /// Render TUI scenarios headlessly and check text, styles and click targets.
    /// Exit code 1 when any check fails. Combine with --json or --screens.
    #[arg(long)]
    self_check: bool,
    /// Self-check output as JSON (checks and styled screen dumps).
    #[arg(long, requires = "self_check")]
    json: bool,
    /// Include styled screen dumps in the text self-check report.
    #[arg(long, requires = "self_check")]
    screens: bool,
}

fn main() -> Result<()> {
    if let Some(code) = loom_tui::product_cli::dispatch(std::env::args_os().collect())? {
        std::process::exit(code);
    }
    let matches = Args::command().get_matches();
    let explicit_cwd = matches.value_source("cwd") == Some(clap::parser::ValueSource::CommandLine);
    let explicit_max_steps =
        matches.value_source("max_steps") == Some(clap::parser::ValueSource::CommandLine);
    let explicit_max_seconds =
        matches.value_source("max_seconds") == Some(clap::parser::ValueSource::CommandLine);
    let explicit_call_seconds = matches.value_source("core_call_seconds") == Some(clap::parser::ValueSource::CommandLine);
    let explicit_attempts = matches.value_source("core_attempts") == Some(clap::parser::ValueSource::CommandLine);
    let mut args = Args::from_arg_matches(&matches)?;
    if matches.value_source("max_cost") != Some(clap::parser::ValueSource::CommandLine) {
        args.max_cost = match loom_core::models::cost_ceiling() {
            loom_core::models::CostClass::Unknown => "any".into(),
            cost => format!("{cost:?}").to_ascii_lowercase(),
        };
    }
    if let Some(version) = &args.bench_core {
        use loom_tui::backend::version_core::{self as versions, Intent, Settings, Version};
        let version = match version.as_str() {
            "loom" => Version::Loom,
            _ => unreachable!(),
        };
        let settings = Settings {
            plan: false,
            hook_session_id: None,
            hook_session_start: None,
            input_control: Default::default(),
                history: vec![],
                skills: vec![],
                images: vec![],
                context_bytes: None,
            model: args.model.clone().unwrap_or_else(|| "auto".into()),
            readonly: args.read_only,
            review_model: args.core_review_model.clone(),
            steps: args.max_steps,
            seconds: args.max_seconds.unwrap_or(300),
            attempts: args.core_attempts,
            checks: args.core_check.clone(),
            state: args.core_state_dir.clone(),
            reference: args.core_run_reference.clone(),
            effort: args.core_effort.clone(),
            call_seconds: args.core_call_seconds,
            split_experiment: args.core_split_experiment,
            edit_handoff: args.core_edit_handoff,
            write_files: args.core_write_file.clone(),
            policy: loom_core::effects::ExecutionPolicy {
                sandbox: loom_core::surfaces::parse_sandbox(&args.native_sandbox)?,
                network: args.native_network,
            },
        };
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let outcome = if let Some(id) = &args.core_approve {
            versions::approve(version, &args.cwd, id, &settings, &cancel)?
        } else {
            let goal = args
                .run
                .as_ref()
                .context("--bench-core requires --run or --core-approve")?;
            let model = loom_core::surfaces::model(args.script.as_deref())?;
            let mut outcome = versions::execute(
                version,
                &args.cwd,
                goal,
                if args.read_only {
                    Intent::Analyze
                } else {
                    match args.core_intent.as_str() {
                        "auto" => Intent::Auto,
                        "inspect" => Intent::Analyze,
                        "answer" => Intent::Answer,
                        _ => Intent::Edit,
                    }
                },
                settings.clone(),
                model,
                cancel.clone(),
                std::sync::Arc::new(|_| {}),
            )?;
            if outcome.pending() && !args.no_approve {
                let applied =
                    versions::approve(version, &args.cwd, &outcome.id, &settings, &cancel)?;
                outcome.status = applied.status;
            }
            outcome
        };
        println!("{}", serde_json::to_string_pretty(&outcome)?);
        if !outcome.success() && !outcome.pending() {
            std::process::exit(2);
        }
        return Ok(());
    }
    if args.free_tiers {
        loom_tui::core::model::cli::print_free_tiers();
        return Ok(());
    }
    if let Some(spec) = &args.pull {
        loom_tui::core::model::cli::pull(spec)?;
        return Ok(());
    }
    if let Some(spec) = &args.serve {
        return loom_tui::core::model::cli::serve(spec, args.port);
    }
    if let Some(task) = &args.run {
        use loom_tui::core::model::{cli, router::Need};
        let max_cost = args.max_cost.as_str();
        let need = Need {
            min_tier: cli::parse_tier(&args.tier).context("--tier: local|small|medium|flagship")?,
            tools: true,
            max_cost: cli::parse_cost(max_cost).context("--max-cost: local|free|subscription|paid|any")?,
        };
        let mut native_args = vec![
            "loom".to_owned(),
            "run".into(),
            task.clone(),
            "--cwd".into(),
            args.cwd.display().to_string(),
            "--tier".into(),
            args.tier.clone(),
            "--max-cost".into(),
            max_cost.into(),
            "--max-steps".into(),
            args.max_steps.to_string(),
        ];
        if let Some(seconds) = args.max_seconds {
            native_args.extend(["--max-seconds".into(), seconds.to_string()]);
        }
        if args.read_only {
            native_args.push("--read-only".into());
        }
        native_args.extend(["--sandbox".into(), args.native_sandbox.clone()]);
        if args.native_network {
            native_args.push("--network".into());
        }
        if let Some(model) = &args.model {
            native_args.extend(["--model".into(), model.clone()]);
        }
        for (flag, path) in [
            ("--trace", args.trace.as_ref()),
            ("--summary-file", args.summary_file.as_ref()),
            ("--script", args.script.as_ref()),
        ] {
            if let Some(path) = path {
                native_args.extend([flag.into(), path.display().to_string()]);
            }
        }
        let _ = need;
        std::process::exit(loom_core::surfaces::cli_from(native_args)?);
    }
    if args.models || args.ask.is_some() || args.model_stats {
        use loom_tui::core::model::{cli, router::Need};
        let need = Need {
            min_tier: cli::parse_tier(&args.tier).context("--tier: local|small|medium|flagship")?,
            tools: false,
            max_cost: cli::parse_cost(&args.max_cost)
                .context("--max-cost: local|free|subscription|paid|any")?,
        };
        if args.model_stats {
            cli::print_stats();
        } else if let Some(prompt) = &args.ask {
            cli::ask(prompt, need)?;
        } else {
            cli::models(need);
        }
        return Ok(());
    }
    if args.self_check {
        let report = loom_tui::selfcheck::run();
        if args.json {
            println!("{}", serde_json::to_string_pretty(&report)?);
        } else {
            print!("{}", report.text(args.screens));
        }
        std::process::exit(i32::from(report.failures() > 0));
    }
    let interactive =
        !args.list_skills && args.explore.is_none();
    if interactive && !explicit_cwd {
        if let Some(cwd) = preferences::remembered_workspace() {
            args.cwd = cwd;
        }
    }
    args.cwd = dunce::canonicalize(&args.cwd).context("Working directory does not exist")?;
    let loaded_preferences = preferences::load(&args.cwd);
    let model_engine = if args.demo { "demo" } else { args.backend.as_str() }.to_owned();
    if interactive {
        if let Ok(preferences) = &loaded_preferences {
            preferences.resolve_model(&model_engine, &mut args.model, &mut args.core_effort);
            if !explicit_call_seconds { args.core_call_seconds = preferences.core_call_seconds; }
            if !explicit_attempts { args.core_attempts = preferences.core_attempts; }
        }
    }
    let preference_steps = loaded_preferences
        .as_ref()
        .map(|preferences| preferences.core_steps)
        .unwrap_or_else(|_| preferences::Preferences::default().core_steps);
    let preference_seconds = loaded_preferences
        .as_ref()
        .map(|preferences| preferences.core_seconds)
        .unwrap_or_else(|_| preferences::Preferences::default().core_seconds);
    let interactive_steps = if explicit_max_steps {
        args.max_steps
    } else {
        preference_steps
    };
    let interactive_seconds = if explicit_max_seconds {
        args.max_seconds.unwrap_or(300)
    } else {
        preference_seconds
    };
    if args.resume.is_none()
        && !loaded_preferences
            .as_ref()
            .map_or(true, |p| p.restore_session)
    {
        args.new = true;
    }
    if args.list_skills {
        for skill in loom_core::instructions::discover(&args.cwd)? {
            println!("{}\t{}\t{}", skill.name, skill.path.display(), skill.description);
        }
        return Ok(());
    }
    if let Some(query) = &args.explore {
        let repository = RepositoryExplorer::new(&args.cwd)?;
        println!("{}", repository.explore(query, 4096)?.compact());
        return Ok(());
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("Run in an interactive terminal, or use --run for a headless task");
    }
    let mut backend: Box<dyn AgentBackend> = if args.demo {
        Box::new(MockCore::with_model(false, args.model.as_deref()))
    } else if args.backend == "loom" {
        let initial_session = runtime::loom_initial_session(args.resume.as_deref(), args.new);
        Box::new(
            loom_tui::backend::versions::VersionBackend::new_with_session(
                loom_tui::backend::version_core::Version::Loom,
                &args.cwd,
                args.script.as_deref(),
                loom_tui::backend::version_core::Settings {
                    plan: false,
                    hook_session_id: None,
                    hook_session_start: None,
                    input_control: Default::default(),
                history: vec![],
                skills: vec![],
                images: vec![],
                context_bytes: None,
                    model: args.model.clone().unwrap_or_else(|| "auto".into()),
                    readonly: args.read_only,
                    review_model: args.core_review_model.clone(),
                    steps: interactive_steps,
                    seconds: interactive_seconds,
                    attempts: args.core_attempts,
                    checks: args.core_check.clone(),
                    state: args.core_state_dir.clone(),
                    reference: args.core_run_reference.clone(),
                    effort: args.core_effort.clone(),
                    call_seconds: args.core_call_seconds,
                    split_experiment: args.core_split_experiment,
                    edit_handoff: args.core_edit_handoff,
                    write_files: args.core_write_file.clone(),
                    policy: loom_core::effects::ExecutionPolicy {
                        sandbox: loom_core::surfaces::parse_sandbox(&args.native_sandbox)?,
                        network: args.native_network,
                    },
                },
                initial_session,
            )?,
        )
    } else if args.backend == "native" {
        Box::new(loom_tui::backend::native::NativeBackend::new(
            &args.cwd,
            args.script.as_deref(),
            loom_core::control::ExecutionOptions {
                readonly: args.read_only,
                model: args.model.clone(),
                session: if args.new {
                    Some(loom_core::sessions::Sessions::open(&args.cwd)?.create()?.id)
                } else {
                    args.resume
                        .as_ref()
                        .filter(|id| id.as_str() != "last")
                        .cloned()
                },
                policy: loom_core::effects::ExecutionPolicy {
                    sandbox: loom_core::surfaces::parse_sandbox(&args.native_sandbox)?,
                    network: args.native_network,
                },
                ..Default::default()
            },
        )?)
    } else {
        unreachable!("--backend accepts loom or native")
    };
    let mut app = App::new(false);
    app.set_workspace_root(args.cwd.clone());
    if args.demo {
        backend = Box::new(CoreBackend::new(backend, &args.cwd, true)?);
    }
    match loaded_preferences {
        Ok(preferences) => {
            app.panel_open = preferences.panel_open && preferences.activity_panel;
            app.preferences = preferences;
        }
        Err(error) => app.error(format!("설정을 읽지 못했습니다: {error}")),
    }
    app.refresh_providers();
    app.configure_model_memory(&model_engine, args.core_effort.clone());
    if !args.demo && true {
        if let Err(error) = preferences::remember_workspace(&args.cwd, &app.preferences) {
            app.error(format!("작업 폴더 저장 실패: {error}"));
        }
    }
    install_shutdown_signals();
    let mut terminal = Terminal::enter()?;
    let mut initial_model_recorded = false;
    let (job_tx, job_rx) = mpsc::channel::<Result<String, String>>();
    let update_rx = if args.demo { None } else { loom_tui::update::auto_check() };
    let (provider_tx, provider_rx) = mpsc::channel::<(Result<String, String>, bool, Option<String>)>();
    let mut provider_job_active = false;
    let (mcp_tx, mcp_rx) = mpsc::channel::<(Result<Vec<loom_tui::app::McpServer>, String>, Option<Result<String, String>>)>();
    let mcp_root = args.cwd.clone();
    let (paste_tx, paste_rx) = mpsc::channel::<Result<window::ClipboardContent, String>>();
    let mut dirty = true;
    let mut canvas = app.render(80, 24);
    let mut last_poll = Instant::now();
    let mut build = BuildWatch::start();
    let mut last_paint = Instant::now() - Duration::from_millis(33);
    let mut remote: Option<loom_tui::remote::Remote> = None;
    let mut last_publish = Instant::now();
    loop {
        // A closed terminal window (SIGHUP) or a kill request ends the loop like Quit,
        // so Native workers are stopped below.
        if SHUTDOWN.load(std::sync::atomic::Ordering::SeqCst) {
            break;
        }
        if std::mem::take(&mut app.bell_pending) {
            use std::io::Write;
            let mut stdout = std::io::stdout().lock();
            stdout.write_all(b"\x07")?;
            stdout.flush()?;
        }
        if app.tick(Instant::now()) {
            dirty = true;
        }
        let batch_start = Instant::now();
        let mut rpc_backlog = false;
        for index in 0..32 {
            if batch_start.elapsed() >= Duration::from_millis(4) {
                rpc_backlog = true;
                break;
            }
            match backend.poll() {
                Ok(Some(event)) => {
                    app.apply_event(event);
                    dirty = true;
                }
                Ok(None) => break,
                Err(error) => {
                    app.error(error.to_string());
                    dirty = true;
                    break;
                }
            }
            if index == 31 {
                rpc_backlog = true;
            }
        }
        while let Ok(result) = job_rx.try_recv() {
            match result {
                Ok(message) => app.notify(message),
                Err(error) => app.error(error),
            }
            dirty = true;
        }
        if let Some(rx) = &update_rx {
            if let Ok(Some(message)) = rx.try_recv() {
                app.notify(message);
                dirty = true;
            }
        }
        while let Ok((result, reload, selection)) = provider_rx.try_recv() {
            provider_job_active = false;
            match result { Ok(message) => app.notify(message), Err(error) => app.error(error) }
            app.refresh_providers();
            if let Some(id) = selection { app.open_provider_models(&id); }
            if reload { if let Err(error) = backend.reload_providers() { app.error(error.to_string()); } }
            dirty = true;
        }
        while let Ok((servers, message)) = mcp_rx.try_recv() {
            match message { Some(Ok(message)) => app.notify(message), Some(Err(error)) => app.error(error), None => {} }
            app.set_mcp_servers(servers);
            dirty = true;
        }
        while let Ok(result) = paste_rx.try_recv() {
            match result {
                Ok(window::ClipboardContent::Text(text)) => app.paste(&text),
                Ok(window::ClipboardContent::Images(images)) => app.attach_images(images),
                Err(error) => app.error(error),
            }
            dirty = true;
        }
        if last_poll.elapsed() > Duration::from_secs(2) {
            if !app.history_pending
                && app.thread.is_some()
                && (app.readonly || app.view == View::Agents)
                && !args.demo
            {
                let id = if app.readonly {
                    app.thread.clone()
                } else {
                    app.agents.get(app.selected_agent).map(|a| a.id.clone())
                };
                if let Some(id) = id
                    && let Err(error) = backend.command(AgentCommand::ReadActivity(id))
                {
                    app.error(error.to_string());
                    dirty = true;
                }
            }
            if let Some(message) = build.stale_notice() {
                app.notify(message);
                dirty = true;
            }
            last_poll = Instant::now();
        }
        if let Some(intent) = app.next_queued() {
            send_intent(&mut app, backend.as_mut(), &intent);
            dirty = true;
        }
        if let Some(server) = &mut remote {
            for pending in server.take_commands() {
                let result = if pending.expired() {
                    Err("요청 시간이 초과됐습니다. 상태를 확인한 뒤 다시 시도해주세요.".into())
                } else {
                    app.checked_remote_command(pending.command.clone()).and_then(|intent| {
                        if let Some(intent) = intent { send_intent_checked(&mut app, backend.as_mut(), &intent) } else { Ok(()) }
                    })
                };
                pending.complete(result);
                dirty = true;
            }
            // Browsers wait on a long poll; publishing an unchanged state wakes none.
            if last_publish.elapsed() >= Duration::from_millis(250) {
                match server.tunnel_status() {
                    Ok(Some(url)) if app.remote.as_ref().is_some_and(|view| view.url != url) => {
                        app.remote_public(url.clone(), loom_tui::remote::qr_modules(&url));
                        dirty = true;
                    }
                    Err(error) => {
                        let url = server.url().to_owned();
                        app.remote_tunnel_failed(url.clone(), loom_tui::remote::qr_modules(&url), &error);
                        dirty = true;
                    }
                    _ => {}
                }
                server.publish(&app.remote_snapshot());
                dirty |= app.remote_clients(server.clients());
                last_publish = Instant::now();
            }
        }
        if !initial_model_recorded && app.thread.is_some() {
            app.remember_model_choice();
            initial_model_recorded = true;
        }
        if app.take_runtime_reload() {
            if let Err(error) = backend.reload_providers() { app.error(error.to_string()); }
            dirty = true;
        }
        if app.take_model_memory_dirty() && true {
            if let Err(error) = preferences::save(&args.cwd, &app.preferences) {
                app.error(format!("모델 설정 저장 실패: {error}"));
                dirty = true;
            }
        }
        // Drain pending input before an expensive paint; a fast typist should
        // never wait for a complete redraw between consecutive keystrokes.
        let input_pending = event::poll(Duration::ZERO)?;
        // Continuous typing must still become visible at least every 50 ms.
        if dirty
            && (!input_pending || last_paint.elapsed() >= Duration::from_millis(50))
            && (!rpc_backlog || last_paint.elapsed() >= Duration::from_millis(33))
        {
            let (width, height) = terminal::size()?;
            canvas = app.render(width, height);
            terminal.paint(&canvas)?;
            dirty = false;
            last_paint = Instant::now();
        }
        if !input_pending
            && !event::poll(if rpc_backlog {
                Duration::ZERO
            } else {
                Duration::from_millis(16)
            })?
        {
            continue;
        }
        let intent = match event::read()? {
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                dirty = true;
                app.key(key, &canvas)
            }
            Event::Paste(text) => {
                app.paste(&text);
                dirty = true;
                None
            }
            Event::Resize(_, _) => {
                app.reset_mouse();
                terminal.invalidate();
                dirty = true;
                None
            }
            Event::Mouse(mouse) => {
                let hovered = app.hovered.clone();
                let intent = app.mouse(mouse, &canvas);
                dirty |= hovered != app.hovered || mouse.kind != event::MouseEventKind::Moved;
                intent
            }
            _ => None,
        };
        if let Some(intent) = intent {
            dirty = true;
            match &intent {
                Intent::Quit => {
                    SHUTDOWN.store(true, std::sync::atomic::Ordering::SeqCst);
                    break;
                }
                Intent::PasteClipboard => {
                    let tx = paste_tx.clone();
                    std::thread::spawn(move || {
                        let _ = tx.send(window::paste_content().map_err(|e| e.to_string()));
                    });
                }
                Intent::CancelQuestions(id) if id == loom_tui::app::providers::FORM_ID || id == loom_tui::app::model_management::DOWNLOAD_FORM_ID => {
                    app.apply_event(loom_tui::agent::AgentEvent::global(loom_tui::agent::AgentUpdate::InputResolved(id.clone())));
                    app.notify("입력을 취소했습니다.");
                }
                Intent::Answer(id, answers) if id == loom_tui::app::model_management::DOWNLOAD_FORM_ID => {
                    if let Err(error) = app.save_download_answers(answers) { app.input_reply_failed(id, &error.to_string()); }
                }
                Intent::Answer(id, answers) if id == loom_tui::app::providers::FORM_ID => {
                    match app.save_provider_answers(answers) {
                        Ok(()) => { if let Err(error) = backend.reload_providers() { app.error(error.to_string()); } }
                        Err(error) => app.input_reply_failed(id, &error.to_string()),
                    }
                }
                Intent::Mcp(argument) => {
                    let (root, tx, argument) = (mcp_root.clone(), mcp_tx.clone(), argument.clone());
                    if argument.starts_with("login ") || argument.starts_with("add ") { app.notify("브라우저에서 MCP 로그인을 계속하세요."); }
                    std::thread::spawn(move || {
                        // /mcp always shows live state: refresh, add, login and logout re-discover.
                        loom_core::extensions::clear_discovery_cache();
                        let words: Vec<_> = argument.split_whitespace().collect();
                        let message = match words.as_slice() {
                            ["add", name, rest @ ..] => Some((|| {
                                loom_core::extensions::add_preset(name, rest.first().copied())?;
                                // OAuth presets go straight to the browser; token presets are ready.
                                if let Ok(url) = loom_core::extensions::oauth_url(&root, name) {
                                    loom_core::mcp_oauth::login(&url, &mut |link| window::open_link(link))?;
                                }
                                Ok(format!("{name} 추가 완료"))
                            })()),
                            ["login", name] => Some(loom_core::extensions::oauth_url(&root, name)
                                .and_then(|url| loom_core::mcp_oauth::login(&url, &mut |link| window::open_link(link)))
                                .map(|()| format!("{name} 로그인 완료"))),
                            ["logout", name] => Some(loom_core::extensions::oauth_url(&root, name)
                                .and_then(|url| loom_core::mcp_oauth::logout(&url))
                                .map(|_| format!("{name} 로그아웃 완료"))),
                            _ => None,
                        }.map(|r| r.map_err(|e| format!("{e:#}")));
                        let servers = loom_tui::app::load_mcp_servers(&root).map_err(|e| format!("{e:#}"));
                        let _ = tx.send((servers, message));
                    });
                }
                Intent::Provider(argument) => {
                    let words: Vec<_> = argument.split_whitespace().collect();
                    let result = (|| -> Result<bool> {
                        let registry = loom_core::models::registry::Registry::open()?;
                        match words.as_slice() {
                            ["refresh"] => Ok(true),
                            [action @ ("login" | "logout" | "remove"), target] |
                            [action @ "login", target, _] => {
                                anyhow::ensure!(!provider_job_active, "진행 중인 연결 작업을 먼저 마무리하세요.");
                                let action = (*action).to_owned();
                                let id = if action == "login" {
                                    anyhow::ensure!(*target == "chatgpt", "로그인은 /provider login chatgpt [NAME]을 사용하세요.");
                                    words.get(2).copied().unwrap_or("chatgpt").to_owned()
                                } else { (*target).to_owned() };
                                let new_connection = if action == "login" { registry.prepare_chatgpt_login(&id)? } else { false };
                                let tx = provider_tx.clone();
                                provider_job_active = true;
                                app.notify(if action == "login" { "브라우저에서 ChatGPT 로그인을 계속하세요." } else { "연결을 정리하는 중…" });
                                std::thread::spawn(move || {
                                    let result = (|| -> Result<String> {
                                        match action.as_str() {
                                            "login" => {
                                                loom_core::models::chatgpt::login(registry.root(), &id, &mut |url| window::open_link(url))?;
                                                registry.finish_chatgpt_login(&id, new_connection)?;
                                                Ok(format!("{id} 연결 완료 · ChatGPT 요금제 사용"))
                                            }
                                            "logout" => {
                                                let account = registry.config()?.accounts.into_iter().find(|account| account.id == id).context("연결이 없습니다.")?;
                                                anyhow::ensure!(account.provider == "chatgpt", "API 키 연결은 삭제하면 저장된 키도 제거됩니다.");
                                                let logout = loom_core::models::chatgpt::logout(registry.root(), &id);
                                                registry.enable(&id, false)?;
                                                logout?;
                                                Ok(format!("{id} 로그아웃 완료"))
                                            }
                                            _ => { registry.remove(&id)?; Ok(format!("{id} 연결을 삭제했습니다.")) }
                                        }
                                    })().map_err(|error| error.to_string());
                                    // A revocation failure may follow successful local removal.
                                    let selection = (action == "login" && result.is_ok()).then_some(id);
                                    let _ = tx.send((result, true, selection));
                                });
                                Ok(false)
                            }
                            ["enable", id] => { registry.enable(id, true)?; Ok(true) }
                            ["disable", id] => { registry.enable(id, false)?; Ok(true) }
                            ["policy", cost] => { registry.set_max_cost(loom_core::models::provider_cli::cost_class(cost)?)?; Ok(true) }
                            ["route", id, mode] => {
                                anyhow::ensure!(matches!(*mode, "auto" | "manual"), "auto 또는 manual을 선택하세요.");
                                registry.set_automatic(id, *mode == "auto")?; Ok(true)
                            }
                            ["test", id] => {
                                let id = id.to_string(); let tx = job_tx.clone();
                                app.notify(format!("{id} 연결 확인 중…"));
                                std::thread::spawn(move || {
                                    let result = registry.probe(&id)
                                        .map(|models| format!("{id} 연결 확인 완료 · 모델 {}개", models.len()))
                                        .map_err(|error| format!("{id} 연결 확인 실패: {error}"));
                                    let _ = tx.send(result);
                                });
                                Ok(false)
                            }
                            _ => anyhow::bail!("사용법: /provider add|login chatgpt [NAME]|logout NAME|test NAME|enable NAME|disable NAME|remove NAME|refresh|policy COST"),
                        }
                    })();
                    match result {
                        Ok(reload) => {
                            app.refresh_providers();
                            if reload { if let Err(error) = backend.reload_providers() { app.error(error.to_string()); } }
                        }
                        Err(error) => app.error(error.to_string()),
                    }
                }
                Intent::SaveSettings => {
                    if let Err(error) = preferences::save(&args.cwd, &app.preferences) {
                        app.error(format!("설정 저장 실패: {error}"));
                    }
                    backend.update_model_policy(
                        if explicit_call_seconds { args.core_call_seconds } else { app.preferences.core_call_seconds },
                        if explicit_attempts { args.core_attempts } else { app.preferences.core_attempts },
                    );
                    backend.update_execution_budget(
                        if explicit_max_steps {
                            args.max_steps
                        } else {
                            app.preferences.core_steps
                        },
                        if explicit_max_seconds {
                            args.max_seconds.unwrap_or(300)
                        } else {
                            app.preferences.core_seconds
                        },
                    );
                    if !args.demo && true {
                        if let Err(error) =
                            preferences::remember_workspace(&args.cwd, &app.preferences)
                        {
                            app.error(format!("작업 폴더 저장 실패: {error}"));
                        }
                    }
                }
                Intent::Remote(Some(public)) => match loom_tui::remote::Remote::start(None) {
                    Ok(mut server) => {
                        // An internet address needs cloudflared; without it the local
                        // network address stays and the window says how to get one.
                        let tunnel = *public && server.open_tunnel();
                        let url = server.url().to_owned();
                        app.remote_started(url.clone(), loom_tui::remote::qr_modules(&url), tunnel);
                        if *public && !tunnel {
                            app.notify("인터넷 접속 주소는 cloudflared가 필요합니다 (brew install cloudflared). 지금은 같은 네트워크에서만 접속됩니다.");
                        }
                        server.publish(&app.remote_snapshot());
                        remote = Some(server);
                    }
                    Err(error) => app.error(format!("원격 조작을 켜지 못했습니다: {error}")),
                },
                Intent::Remote(None) => {
                    remote = None;
                    app.remote_stopped();
                }
                Intent::ExportDiagnostics => {
                    let path = runtime::state_dir(&args.cwd).join("diagnostics.json");
                    let result =
                        std::fs::create_dir_all(runtime::state_dir(&args.cwd)).and_then(|_| {
                            std::fs::write(
                                &path,
                                serde_json::to_vec_pretty(&app.diagnostics()).unwrap(),
                            )
                        });
                    match result {
                        Ok(()) => app.notify(format!("진단 파일 저장: {}", path.display())),
                        Err(error) => app.error(format!("진단 파일 저장 실패: {error}")),
                    }
                }
                Intent::OpenAgent(_) => app.notify("에이전트는 작업 패널에서 확인할 수 있습니다."),
                Intent::OpenLink(url) => {
                    let url = url.clone();
                    let tx = job_tx.clone();
                    std::thread::spawn(move || {
                        let _ = tx.send(
                            window::open_link(&url)
                                .map(|_| "링크를 열었습니다.".into())
                                .map_err(|e| e.to_string()),
                        );
                    });
                }
                Intent::Copy(text) => {
                    let text = text.clone();
                    let tx = job_tx.clone();
                    std::thread::spawn(move || {
                        let _ = tx.send(
                            window::copy(&text)
                                .map(|_| "복사 완료".into())
                                .map_err(|e| e.to_string()),
                        );
                    });
                }
                _ => send_intent(&mut app, backend.as_mut(), &intent),
            }
        }
    }
    drop(terminal);
    if true {
        app.remember_view();
        if let Err(error) = preferences::save(&args.cwd, &app.preferences) {
            eprintln!("설정 저장 실패: {error}");
        }
    }
    let shutdown = backend.shutdown();
    drop(backend);
    if let Err(error) = shutdown {
        eprintln!("Session cleanup: {error}");
    }
    if !args.demo {
        println!("{}", runtime::exit_notice(app.thread.as_deref()));
    }
    Ok(())
}

fn send_intent(app: &mut App, backend: &mut dyn AgentBackend, intent: &Intent) {
    let _ = send_intent_checked(app, backend, intent);
}

fn send_intent_checked(app: &mut App, backend: &mut dyn AgentBackend, intent: &Intent) -> Result<(), String> {
    if let Some(command) = app.agent_command(intent)
        && let Err(error) = backend.command(command)
    {
        match intent {
            Intent::Answer(id, _) | Intent::CancelQuestions(id) => app.input_reply_failed(id, &error.to_string()),
            Intent::Submit(_) => app.fail_submission(error.to_string()),
            Intent::Inquiry(question) => {app.editor.insert(&format!("/ask {question}"));app.error(error.to_string());},
            Intent::Interrupt => app.interrupt_failed(&error.to_string()),
            Intent::UpdatePermission(_, _) => app.permission_failed(&error.to_string()),
            Intent::Reply(id, _) => app.approval_reply_failed(id, &error.to_string()),
            Intent::Fork(_) | Intent::Rewind(_) => app.branch_failed(&error.to_string()),
            _ => app.error(error.to_string()),
        }
        return Err(error.to_string());
    }
    Ok(())
}

/// Notices when this program's own binary was rebuilt after it started.
///
/// An agent editing Loom's source rebuilds the binary, but a running window keeps the
/// old code; without this notice fixes look like they "did nothing".
struct BuildWatch {
    path: Option<std::path::PathBuf>,
    started: Option<std::time::SystemTime>,
    last_notice: Option<Instant>,
}

impl BuildWatch {
    fn start() -> Self {
        let path = std::env::current_exe().ok();
        let started = path
            .as_ref()
            .and_then(|p| std::fs::metadata(p).ok()?.modified().ok());
        Self {
            path,
            started,
            last_notice: None,
        }
    }

    /// A reminder at most every five minutes once a newer build exists.
    fn stale_notice(&mut self) -> Option<&'static str> {
        let modified = std::fs::metadata(self.path.as_ref()?)
            .ok()?
            .modified()
            .ok()?;
        if modified <= self.started?
            || self
                .last_notice
                .is_some_and(|t| t.elapsed() < Duration::from_secs(300))
        {
            return None;
        }
        self.last_notice = Some(Instant::now());
        Some("Loom이 다시 빌드되었습니다. 변경을 보려면 종료(Ctrl+Q) 후 다시 실행하세요.")
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn only_current_core_and_backups_are_launchable() {
        use clap::Parser;
        for retired in ["v1", "v2", "v3", "v4", "v5"] {
            assert!(super::Args::try_parse_from(["loom", "--backend", retired]).is_err());
        }
        assert_eq!(super::Args::try_parse_from(["loom"]).unwrap().backend, "loom");
    }


}

use loom_tui::runtime::SHUTDOWN;
/// A signal asked us to leave (not the user's Quit): arms the stuck-exit watchdog.
static SIGNALLED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn request_shutdown(_: libc::c_int) {
    // Only async-signal-safe calls: stop the owned runtime's process group now, because
    // after a hangup the input reader can spin on the dead terminal and never return to
    // the loop that would run the normal cleanup.
    SIGNALLED.store(true, std::sync::atomic::Ordering::SeqCst);
    SHUTDOWN.store(true, std::sync::atomic::Ordering::SeqCst);
}

/// Without a handler these signals end the process at once, skipping the cleanup that
/// stops Loom's workers (and a window whose terminal vanished could spin at full CPU
/// for hours).
fn install_shutdown_signals() {
    #[cfg(unix)]
    {
        unsafe {
            for signal in [libc::SIGHUP, libc::SIGTERM] {
                libc::signal(signal, request_shutdown as *const () as libc::sighandler_t);
            }
        }
        // Watchdog: a vanished parent (terminal closed without a signal reaching us) also
        // means shutdown; if the normal cleanup has not finished 2 s after a shutdown
        // request, the main thread is stuck, so leave without it.
        std::thread::spawn(|| {
            loop {
                std::thread::sleep(std::time::Duration::from_millis(250));
                if unsafe { libc::getppid() } == 1
                    && !SIGNALLED.load(std::sync::atomic::Ordering::SeqCst)
                {
                    request_shutdown(libc::SIGHUP);
                }
                if SIGNALLED.load(std::sync::atomic::Ordering::SeqCst) {
                    std::thread::sleep(std::time::Duration::from_secs(2));
                    unsafe { libc::_exit(129) };
                }
            }
        });
    }
}
