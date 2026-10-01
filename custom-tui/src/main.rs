use anyhow::{Context, Result, bail};
use clap::Parser;
use crossterm::{
    event::{self, Event, KeyEventKind, MouseButton, MouseEventKind},
    terminal,
};
use custom_tui::{
    agent::{AgentBackend, AgentCommand},
    app::{App, Intent, View},
    backend::{
        codex::{CodexBackend, Options},
        diagnostics,
        mock::MockCore,
    },
    core::{backend::CoreBackend, repository::RepositoryExplorer},
    engine::{Action, Terminal},
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
    #[arg(long, default_value = ".")]
    cwd: PathBuf,
    /// Search the shared local code index without Codex or a model call.
    #[arg(long, conflicts_with_all = ["demo", "probe", "list_models", "list_skills", "agent", "resume", "new", "endpoint"])]
    explore: Option<String>,
    #[arg(long, default_value_t = runtime::default_binary())]
    codex_bin: String,
    #[arg(long)]
    endpoint: Option<String>,
    /// Override the model for this session without editing Codex settings.
    #[arg(long)]
    model: Option<String>,
    /// List models offered by the installed Codex runtime without inference.
    #[arg(long, conflicts_with = "demo")]
    list_models: bool,
    /// List installed skills for this workspace without inference.
    #[arg(long, conflicts_with = "demo")]
    list_skills: bool,
    /// Resume a thread ID, or use "last" for this workspace's saved thread.
    #[arg(long)]
    resume: Option<String>,
    /// Start a separate conversation instead of reopening this folder's recent one.
    #[arg(long, conflicts_with_all = ["resume", "agent"])]
    new: bool,
    /// Read-only agent viewer; closing it does not interrupt the agent.
    #[arg(long, conflicts_with = "resume")]
    agent: Option<String>,
    /// Exercise the UI with clearly labeled fixture data; no inference or tools run.
    #[arg(long, conflicts_with = "endpoint")]
    demo: bool,
    /// Verify real runtime handshake and two-client metadata access without inference.
    #[arg(long, conflicts_with = "demo")]
    probe: bool,
}

fn main() -> Result<()> {
    let mut args = Args::parse();
    args.cwd = args
        .cwd
        .canonicalize()
        .context("Working directory does not exist")?;
    if args.list_skills {
        return diagnostics::list_skills(&args.cwd, &args.codex_bin, args.endpoint.as_deref());
    }
    if let Some(query) = &args.explore {
        let repository = RepositoryExplorer::new(&args.cwd)?;
        println!("{}", repository.explore(query, 4096)?.compact());
        return Ok(());
    }
    if args.list_models {
        return diagnostics::list_models(&args.cwd, &args.codex_bin, args.endpoint.as_deref());
    }
    if args.probe {
        return diagnostics::probe(&args.cwd, &args.codex_bin, args.endpoint.as_deref());
    }
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("Run in an interactive terminal, or use --probe to check the runtime");
    }
    let endpoint = if args.demo {
        String::new()
    } else {
        match &args.endpoint {
            Some(endpoint) => endpoint.clone(),
            None => runtime::ensure(&args.cwd, &args.codex_bin)?.endpoint,
        }
    };
    let initial = initial_thread(&args)?;
    let mut backend: Box<dyn AgentBackend> = if args.demo {
        Box::new(MockCore::new(args.agent.is_some()))
    } else {
        Box::new(CodexBackend::connect(
            &endpoint,
            Options {
                cwd: args.cwd.clone(),
                model: args.model.clone(),
                initial_session: initial,
                readonly: args.agent.is_some(),
                auto_restore: args.resume.is_none() && !args.new && args.agent.is_none(),
            },
        )?)
    };
    let mut app = App::new(args.agent.is_some());
    backend = Box::new(CoreBackend::new(
        backend,
        &args.cwd,
        args.demo || args.agent.is_some(),
    )?);
    match preferences::load(&args.cwd) {
        Ok(preferences) => {
            app.panel_open = preferences.panel_open;
            app.preferences = preferences;
        }
        Err(error) => app.error(format!("설정을 읽지 못했습니다: {error}")),
    }
    let mut terminal = Terminal::enter()?;
    let (job_tx, job_rx) = mpsc::channel::<Result<String, String>>();
    let (paste_tx, paste_rx) = mpsc::channel::<Result<String, String>>();
    let mut dirty = true;
    let mut canvas = app.render(80, 24);
    let mut last_poll = Instant::now();
    let mut last_paint = Instant::now() - Duration::from_millis(33);
    let mut drag_start: Option<(u16, u16)> = None;
    loop {
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
        while let Ok(result) = paste_rx.try_recv() {
            match result {
                Ok(text) => app.paste(&text),
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
            last_poll = Instant::now();
        }
        if let Some(intent) = app.next_queued() {
            send_intent(&mut app, backend.as_mut(), &intent);
            dirty = true;
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
                terminal.invalidate();
                dirty = true;
                None
            }
            Event::Mouse(mouse) => {
                if mouse.kind != MouseEventKind::Moved {
                    dirty = true;
                }
                match mouse.kind {
                    MouseEventKind::Moved => {
                        let hovered = canvas.hit(mouse.column, mouse.row);
                        if hovered != app.hovered {
                            app.hovered = hovered;
                            dirty = true;
                        }
                        None
                    }
                    MouseEventKind::ScrollUp => {
                        app.scroll_at(mouse.column, canvas.width, false);
                        None
                    }
                    MouseEventKind::ScrollDown => {
                        app.scroll_at(mouse.column, canvas.width, true);
                        None
                    }
                    MouseEventKind::Down(MouseButton::Left) => {
                        drag_start = None;
                        if let Some(action) = canvas.hit(mouse.column, mouse.row) {
                            if matches!(action, Action::SelectEntry(_)) {
                                drag_start = Some((mouse.column, mouse.row));
                                None
                            } else {
                                if !matches!(action, Action::Input) {
                                    app.focus = Some(action.clone());
                                }
                                app.activate(action)
                            }
                        } else {
                            drag_start = Some((mouse.column, mouse.row));
                            None
                        }
                    }
                    MouseEventKind::Up(MouseButton::Left) => {
                        if let Some(start) = drag_start.take() {
                            if start != (mouse.column, mouse.row) {
                                let text = selected_text(&canvas, start, (mouse.column, mouse.row));
                                app.clear_text_selection();
                                terminal.invalidate();
                                (!text.is_empty()).then_some(Intent::Copy(text))
                            } else {
                                canvas.hit(start.0, start.1).and_then(|action| {
                                    matches!(action, Action::SelectEntry(_))
                                        .then(|| app.activate(action))
                                        .flatten()
                                })
                            }
                        } else {
                            None
                        }
                    }
                    _ => None,
                }
            }
            _ => None,
        };
        if let Some(intent) = intent {
            dirty = true;
            match &intent {
                Intent::Quit => break,
                Intent::PasteClipboard => {
                    let tx = paste_tx.clone();
                    std::thread::spawn(move || {
                        let _ = tx.send(window::paste().map_err(|e| e.to_string()));
                    });
                }
                Intent::SaveSettings => {
                    if let Err(error) = preferences::save(&args.cwd, &app.preferences) {
                        app.error(format!("설정 저장 실패: {error}"));
                    }
                }
                Intent::OpenAgent(id) => {
                    let id = id.clone();
                    let cwd = args.cwd.clone();
                    let endpoint = endpoint.clone();
                    let tx = job_tx.clone();
                    let executable = std::env::current_exe()?;
                    let demo = args.demo;
                    app.notify("새 터미널을 여는 중…");
                    std::thread::spawn(move || {
                        let result =
                            window::open_agent_with_demo(&executable, &cwd, &endpoint, &id, demo)
                                .map(|_| "에이전트 터미널을 열었습니다.".into())
                                .map_err(|e| e.to_string());
                        let _ = tx.send(result);
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
    if !args.demo {
        println!(
            "화면 연결을 닫았습니다. 저장된 대화는 다음 실행 시 자동으로 복원됩니다.\n서버가 실행 중인 동안 진행 중인 작업도 계속됩니다.\n새 대화: custom-tui --cwd {} --new",
            window::shell_quote(&args.cwd.to_string_lossy())
        );
    }
    Ok(())
}

fn send_intent(app: &mut App, backend: &mut dyn AgentBackend, intent: &Intent) {
    if let Some(command) = app.agent_command(intent)
        && let Err(error) = backend.command(command)
    {
        match intent {
            Intent::Answer(id, _) => app.input_reply_failed(id, &error.to_string()),
            Intent::Submit(_) => app.fail_submission(error.to_string()),
            Intent::Interrupt => app.interrupt_failed(&error.to_string()),
            Intent::UpdatePermission(_, _) => app.permission_failed(&error.to_string()),
            Intent::Reply(id, _) => app.approval_reply_failed(id, &error.to_string()),
            _ => app.error(error.to_string()),
        }
    }
}

fn initial_thread(args: &Args) -> Result<Option<String>> {
    if let Some(agent) = &args.agent {
        return Ok(Some(agent.clone()));
    }
    if args.new || args.demo {
        return Ok(None);
    }
    match args.resume.as_deref() {
        Some("last") => runtime::last_thread(&args.cwd).map(Some),
        Some(id) => Ok(Some(id.to_owned())),
        None => Ok(runtime::recent_thread(&args.cwd)),
    }
}

fn selected_text(canvas: &custom_tui::engine::Canvas, from: (u16, u16), to: (u16, u16)) -> String {
    let (start, end) = if (from.1, from.0) <= (to.1, to.0) {
        (from, to)
    } else {
        (to, from)
    };
    let mut output = Vec::new();
    for y in start.1..=end.1.min(canvas.height.saturating_sub(1)) {
        let x0 = if y == start.1 { start.0 } else { 0 };
        let x1 = if y == end.1 {
            end.0.saturating_add(1).min(canvas.width)
        } else {
            canvas.width
        };
        let mut line = String::new();
        for x in x0..x1 {
            let cell = &canvas.cells[usize::from(y) * usize::from(canvas.width) + usize::from(x)];
            if !cell.continuation {
                line.push_str(&cell.text);
            }
        }
        output.push(line.trim_end().to_owned());
    }
    output.join("\n")
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn startup_automatically_reopens_recent_thread_unless_new_is_explicit() {
        let dir = tempfile::tempdir().unwrap();
        runtime::remember_thread(dir.path(), "saved-id").unwrap();
        let cwd = dir.path().to_str().unwrap();
        let args = Args::try_parse_from(["custom-tui", "--cwd", cwd]).unwrap();
        assert_eq!(initial_thread(&args).unwrap().as_deref(), Some("saved-id"));
        let args = Args::try_parse_from(["custom-tui", "--cwd", cwd, "--new"]).unwrap();
        assert_eq!(initial_thread(&args).unwrap(), None);
        let args =
            Args::try_parse_from(["custom-tui", "--cwd", cwd, "--resume", "specific-id"]).unwrap();
        assert_eq!(
            initial_thread(&args).unwrap().as_deref(),
            Some("specific-id")
        );
        let args = Args::try_parse_from(["custom-tui", "--cwd", cwd, "--demo"]).unwrap();
        assert_eq!(initial_thread(&args).unwrap(), None);
        assert!(Args::try_parse_from(["custom-tui", "--new", "--resume", "last"]).is_err());
    }
}
