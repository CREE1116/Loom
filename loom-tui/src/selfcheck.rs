//! Headless self-check of the TUI: real `App` code, real rendering, no terminal or model.
//! 
//! Each scenario drives the app through domain events, renders it, and checks what a
//! user would see: text, styles and clickable regions. Failures are the improvement
//! backlog for `scripts/loom-selfimprove`; dumps show the screen with style markup so an
//! agent can see what is wrong without a terminal.
use crate::{
    agent::*,
    app::{App, Intent, View},
    engine::{Action, Background, Canvas, Tone},
};
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use serde::Serialize;

#[derive(Serialize)]
pub struct Check {
    pub scenario: &'static str,
    pub name: String,
    pub pass: bool,
    pub detail: String,
}

#[derive(Serialize)]
pub struct Dump {
    pub scenario: &'static str,
    pub label: String,
    pub width: u16,
    pub height: u16,
    /// Screen rows; styled runs as `[Tone:text]`.
    pub rows: Vec<String>,
}

#[derive(Serialize, Default)]
pub struct Report {
    pub checks: Vec<Check>,
    pub dumps: Vec<Dump>,
}

impl Report {
    pub fn failures(&self) -> usize {
        self.checks.iter().filter(|c| !c.pass).count()
    }

    fn check(
        &mut self,
        scenario: &'static str,
        name: impl Into<String>,
        pass: bool,
        detail: impl Into<String>,
    ) {
        self.checks.push(Check {
            scenario,
            name: name.into(),
            pass,
            detail: detail.into(),
        });
    }

    fn dump(&mut self, scenario: &'static str, canvas: &Canvas) {
        self.dumps.push(Dump {
            scenario,
            label: format!(
                "{scenario} {}x{} · {}",
                canvas.width,
                canvas.height,
                canvas.accent.key()
            ),
            width: canvas.width,
            height: canvas.height,
            rows: styled_rows(canvas)
                .into_iter()
                .zip(canvas.cells.chunks(usize::from(canvas.width)))
                .map(|(row, cells)| {
                    if cells[0].background == crate::engine::Background::Default {
                        row
                    } else {
                        format!("[Background:{:?}]{row}", cells[0].background)
                    }
                })
                .collect(),
        });
    }

    pub fn text(&self, with_dumps: bool) -> String {
        let mut out = String::new();
        let mut scenario = "";
        for check in &self.checks {
            if check.scenario != scenario {
                scenario = check.scenario;
                out.push_str(&format!("\n[{scenario}]\n"));
            }
            out.push_str(&format!(
                "  {} {}",
                if check.pass { "✓" } else { "✗" },
                check.name
            ));
            if !check.pass && !check.detail.is_empty() {
                out.push_str(&format!(" — {}", check.detail));
            }
            out.push('\n');
        }
        out.push_str(&format!(
            "\n{} checks, {} failed\n",
            self.checks.len(),
            self.failures()
        ));
        if with_dumps {
            for dump in &self.dumps {
                out.push_str(&format!("\n--- screen: {} ---\n", dump.label));
                for row in &dump.rows {
                    out.push_str(row);
                    out.push('\n');
                }
            }
        }
        out
    }
}

fn session_exit_notice(report: &mut Report) {
    const S: &str = "session_exit_notice";
    report.check(
        S,
        "새 실행은 별도 Loom 세션으로 시작",
        crate::runtime::loom_initial_session(None, false).as_deref() == Some("new"),
        "",
    );
    report.check(
        S,
        "명시적인 세션 재개 지원",
        crate::runtime::loom_initial_session(Some("session-123"), false).as_deref()
            == Some("session-123")
            && crate::runtime::loom_initial_session(Some("last"), false).is_none()
            && crate::runtime::loom_initial_session(None, true).as_deref() == Some("new"),
        "",
    );
    report.check(
        S,
        "종료 시 세션 ID만 간결하게 표시",
        crate::runtime::exit_notice(Some("session-123")) == "세션 ID: session-123"
            && crate::runtime::exit_notice(None) == "세션을 닫았습니다.",
        "",
    );
}

// ------------------------------------------------------------------ screen helpers

fn rows(canvas: &Canvas) -> Vec<Vec<(u16, &str, Tone)>> {
    canvas
        .cells
        .chunks(usize::from(canvas.width))
        .map(|row| {
            row.iter()
                .enumerate()
                .filter(|(_, c)| !c.continuation)
                .map(|(x, c)| (x as u16, c.text.as_str(), c.tone))
                .collect()
        })
        .collect()
}

fn styled_rows(canvas: &Canvas) -> Vec<String> {
    let mut out = Vec::new();
    for row in rows(canvas) {
        let mut line = String::new();
        let mut run: Option<(Tone, String)> = None;
        let flush = |line: &mut String, run: &mut Option<(Tone, String)>| {
            if let Some((tone, text)) = run.take() {
                if matches!(tone, Tone::Normal) || text.trim().is_empty() {
                    line.push_str(&text);
                } else {
                    line.push_str(&format!("[{tone:?}:{text}]"));
                }
            }
        };
        for (_, text, tone) in row {
            match &mut run {
                Some((t, s)) if *t == tone => s.push_str(text),
                _ => {
                    flush(&mut line, &mut run);
                    run = Some((tone, text.to_owned()));
                }
            }
        }
        flush(&mut line, &mut run);
        out.push(line.trim_end().to_owned());
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out
}

fn screen_text(canvas: &Canvas) -> String {
    (0..canvas.height)
        .map(|y| canvas.plain_line(y))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Tone of the first cell of `needle` on screen.
fn tone_of(canvas: &Canvas, needle: &str) -> Option<Tone> {
    let first = needle.chars().next()?.to_string();
    for row in rows(canvas) {
        let joined: String = row.iter().map(|(_, t, _)| *t).collect();
        if let Some(byte) = joined.find(needle) {
            let mut seen = 0;
            for (_, text, tone) in &row {
                if seen == byte && text.starts_with(&first) {
                    return Some(*tone);
                }
                seen += text.len();
                if seen > byte {
                    break;
                }
            }
        }
    }
    None
}

/// Clickable actions whose region covers the first cell of `needle`.
fn actions_at<'a>(canvas: &'a Canvas, needle: &str) -> Vec<&'a Action> {
    let mut found = Vec::new();
    for (y, row) in rows(canvas).into_iter().enumerate() {
        let joined: String = row.iter().map(|(_, t, _)| *t).collect();
        let Some(byte) = joined.find(needle) else {
            continue;
        };
        let mut seen = 0;
        for (x, text, _) in &row {
            if seen >= byte {
                for hit in &canvas.hits {
                    if hit.y as usize <= y
                        && y < (hit.y + hit.height) as usize
                        && hit.x <= *x
                        && *x < hit.x + hit.width
                    {
                        found.push(&hit.action);
                    }
                }
                return found;
            }
            seen += text.len();
        }
    }
    found
}

fn loaded(id: &str, title: &str) -> AgentEvent {
    AgentEvent::global(AgentUpdate::SessionLoaded {
        snapshot: SessionSnapshot {
            id: id.into(),
            title: Some(title.into()),
            status: Some("idle".into()),
            turns: Some(vec![]),
        },
        switching: true,
        model: None,
        permissions: Permissions::default(),
    })
}

fn message(id: &str, body: &str) -> AgentEvent {
    AgentEvent::scoped(
        "main",
        AgentUpdate::EntryUpdated(Entry {
            id: id.into(),
            kind: Kind::Assistant,
            title: "● 에이전트".into(),
            body: body.into(),
            status: "completed".into(),
            expanded: false,
        }),
    )
}

fn ledger_fixture() -> Vec<crate::core::model::router::CallRecord> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let call = |at: u64,
                endpoint: &str,
                model: &str,
                served: Option<&str>,
                route: Option<&str>,
                ok: bool,
                usd: Option<f64>| {
        crate::core::model::router::CallRecord {
            at,
            endpoint: endpoint.into(),
            model: model.into(),
            served_model: served.map(str::to_owned),
            route: route.map(str::to_owned),
            gateway_upstream_ms: None,
            gateway_overhead_ms: None,
            ok,
            error: (!ok).then(|| "rate limited".into()),
            input: Some(1200),
            cached_input: Some(800),
            output: Some(90),
            usd,
            latency_ms: 1500,
        }
    };
    vec![
        call(
            now,
            "openrouter",
            "auto",
            Some("gemini-3-flash"),
            Some("gemini"),
            true,
            Some(0.0),
        ),
        call(
            now,
            "openrouter",
            "auto",
            Some("gemini-3-flash"),
            Some("gemini"),
            false,
            Some(0.0),
        ),
        call(now, "ollama", "gemma-4b", None, None, true, Some(0.0)),
        call(now, "groq", "llama-x", None, None, true, None),
        call(
            now.saturating_sub(2 * 86_400),
            "groq",
            "old-model",
            None,
            None,
            true,
            Some(0.0),
        ),
    ]
}

fn app_with(events: impl IntoIterator<Item = AgentEvent>) -> App {
    let mut app = App::new(false);
    app.apply_event(loaded("main", "self-check"));
    for event in events {
        app.apply_event(event);
    }
    app
}

// ------------------------------------------------------------------ scenarios

pub const MARKDOWN: &str = "## 제목\n\n**굵게** *기울임* ~~취소선~~ `인라인` [링크](https://example.com)\n\n- [x] 완료된 작업\n- [ ] 진행 예정 작업\n\n| 항목 | 상태 |\n| --- | --- |\n| 표 | 정상 |\n\n```rust\nfn main() { println!(\"hi\"); }\n```\n\n수식 $E = mc^2$\n\n$$\\int_0^1 x\\,dx$$\n\n<details><summary>접기 요약</summary>\n\n숨은 내용\n\n</details>\n";

fn markdown(report: &mut Report) {
    const S: &str = "markdown";
    let mut app = app_with([message("md", MARKDOWN)]);
    let canvas = app.render(100, 50);
    let text = screen_text(&canvas);
    for (label, tone, raw) in [
        ("굵게", Tone::Strong, "**굵게**"),
        ("기울임", Tone::Emphasis, "*기울임*"),
        ("취소선", Tone::Strike, "~~취소선~~"),
        ("인라인", Tone::InlineCode, "`인라인`"),
    ] {
        let seen = tone_of(&canvas, label);
        report.check(
            S,
            format!("{label} → {tone:?}"),
            seen == Some(tone),
            format!("rendered as {seen:?}"),
        );
        report.check(
            S,
            format!("{label}: 원문 표식 숨김"),
            !text.contains(raw),
            format!("raw `{raw}` visible"),
        );
    }
    report.check(
        S,
        "제목: `##` 숨김",
        text.contains("제목") && !text.contains("## 제목"),
        "",
    );
    report.check(S, "체크리스트 완료 ☑", text.contains("☑ 완료된 작업"), "");
    report.check(
        S,
        "체크리스트 예정 ☐",
        text.contains("☐ 진행 예정 작업"),
        "",
    );
    report.check(
        S,
        "표: 구분선 숨김",
        text.contains("정상") && !text.contains("| --- |"),
        "",
    );
    report.check(S, "코드블럭 테두리와 언어", text.contains("┌─ rust"), "");
    report.check(S, "코드블럭: ``` 숨김", !text.contains("```"), "");
    report.check(
        S,
        "코드 하이라이트",
        tone_of(&canvas, "fn").is_some_and(|t| t == Tone::Keyword),
        format!("{:?}", tone_of(&canvas, "fn")),
    );
    report.check(
        S,
        "인라인 수식",
        text.contains("E = mc²") && !text.contains("$E"),
        "",
    );
    report.check(
        S,
        "블록 수식 기호 (\\int → ∫)",
        text.contains("∫"),
        "`\\int` not converted",
    );
    report.check(
        S,
        "수식 간격 (\\, → 공백)",
        !text.contains("x,dx"),
        "`\\,` rendered as a comma",
    );
    let link = actions_at(&canvas, "링크");
    report.check(
        S,
        "링크 클릭 가능",
        link.iter()
            .any(|a| format!("{a:?}").contains("example.com")),
        format!("actions at link: {link:?}"),
    );
    report.check(
        S,
        "접기: <details> 태그 숨김",
        !text.contains("<details>"),
        "raw HTML tag visible",
    );
    report.check(
        S,
        "접기: 요약만 보이고 내용은 접힘",
        text.contains("접기 요약") && !text.contains("숨은 내용"),
        "",
    );
    report.dump(S, &canvas);
}

fn streaming(report: &mut Report) {
    const S: &str = "streaming";
    let done = app_with([message("md", MARKDOWN)]).render(100, 50);
    let cut = (MARKDOWN.len() / 2..)
        .find(|i| MARKDOWN.is_char_boundary(*i))
        .unwrap_or(0);
    let mut app = app_with([AgentEvent::scoped(
        "main",
        AgentUpdate::TurnStarted {
            id: Some("t".into()),
        },
    )]);
    for part in [&MARKDOWN[..cut], &MARKDOWN[cut..]] {
        app.apply_event(AgentEvent::scoped(
            "main",
            AgentUpdate::EntryDelta {
                id: "md".into(),
                kind: Kind::Assistant,
                text: part.into(),
            },
        ));
        app.render(100, 50);
    }
    let streamed = app.render(100, 50);
    let a = styled_rows(&done);
    let b = styled_rows(&streamed);
    let body = |rows: &[String]| rows.iter().skip(3).take(30).cloned().collect::<Vec<_>>();
    report.check(
        S,
        "스트리밍 결과 = 완료 결과",
        body(&a) == body(&b),
        "streamed rows differ from the completed message",
    );
}

fn copy(report: &mut Report) {
    const S: &str = "copy";
    let mut app = app_with([message("md", MARKDOWN)]);
    app.render(100, 50);
    let copied = app.activate(Action::Copy("md".into()));
    report.check(
        S,
        "메시지 복사 = 원문 그대로",
        copied == Some(Intent::Copy(MARKDOWN.into())),
        format!("{copied:?}"),
    );

    let mut app = App::new(false);
    app.apply_event(AgentEvent::global(AgentUpdate::SessionLoaded {
        snapshot: SessionSnapshot {
            id: "copy-session".into(),
            title: None,
            status: Some("idle".into()),
            turns: Some(vec![TurnSnapshot {
                id: Some("turn-1".into()),
                status: "completed".into(),
                entries: vec![
                    Entry {
                        id: "user".into(),
                        kind: Kind::User,
                        title: "› 나".into(),
                        body: "로그도 복사해".into(),
                        status: "completed".into(),
                        expanded: false,
                    },
                    Entry {
                        id: "assistant".into(),
                        kind: Kind::Assistant,
                        title: "● 에이전트".into(),
                        body: "처리했습니다".into(),
                        status: "completed".into(),
                        expanded: false,
                    },
                    Entry {
                        id: "work-log".into(),
                        kind: Kind::Tool,
                        title: "작업 기록".into(),
                        body: "symbols → read → edit".into(),
                        status: "completed".into(),
                        expanded: false,
                    },
                ],
                activities: vec![],
            }]),
        },
        switching: true,
        model: None,
        permissions: Permissions::default(),
    }));
    app.editor.insert("/copy 1");
    let copied = app.activate(Action::Send);
    report.check(
        S,
        "/copy = 턴 전체 로그",
        matches!(&copied, Some(Intent::Copy(text)) if text.contains("로그도 복사해") && text.contains("처리했습니다") && text.contains("symbols → read → edit")),
        format!("{copied:?}"),
    );
}

fn drag_copy(report: &mut Report) {
    const S: &str = "drag_copy";
    let event = |kind, column, row| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    let point = |canvas: &Canvas, needle: &str| {
        (0..canvas.height)
            .find_map(|y| {
                (0..canvas.width).find_map(|x| {
                    let base = usize::from(y) * usize::from(canvas.width);
                    let tail = canvas.cells
                        [base + usize::from(x)..base + usize::from(canvas.width)]
                        .iter()
                        .filter(|cell| !cell.continuation)
                        .map(|cell| cell.text.as_str())
                        .collect::<String>();
                    tail.starts_with(needle).then_some((x, y))
                })
            })
            .unwrap()
    };
    let mut app = App::new(false);
    app.editor.insert("compose now");
    let canvas = app.render(80, 24);
    let y = canvas
        .hits
        .iter()
        .find(|h| h.action == Action::Input)
        .unwrap()
        .y;
    app.mouse(
        event(MouseEventKind::Down(MouseButton::Left), 4, y),
        &canvas,
    );
    app.mouse(
        event(MouseEventKind::Drag(MouseButton::Left), 11, y),
        &canvas,
    );
    let copied = app.mouse(event(MouseEventKind::Up(MouseButton::Left), 11, y), &canvas);
    report.check(
        S,
        "입력창 드래그 복사",
        copied == Some(Intent::Copy("compose".into())),
        format!("{copied:?}"),
    );
    report.check(
        S,
        "드래그 복사 후에도 초안 유지",
        app.editor.text == "compose now",
        &app.editor.text,
    );

    let mut app = app_with([message("words", "alpha beta gamma")]);
    let canvas = app.render(80, 24);
    let (beta_x, beta_y) = point(&canvas, "beta");
    app.mouse(
        event(MouseEventKind::Down(MouseButton::Left), beta_x + 1, beta_y),
        &canvas,
    );
    app.mouse(
        event(MouseEventKind::Up(MouseButton::Left), beta_x + 1, beta_y),
        &canvas,
    );
    app.mouse(
        event(MouseEventKind::Down(MouseButton::Left), beta_x + 1, beta_y),
        &canvas,
    );
    let selected = app.render(80, 24);
    let highlighted = (beta_x..beta_x + 4).all(|x| {
        selected.cells[usize::from(beta_y) * 80 + usize::from(x)].background == Background::Selected
    });
    let copied = app.mouse(
        event(MouseEventKind::Up(MouseButton::Left), beta_x + 1, beta_y),
        &selected,
    );
    report.check(
        S,
        "더블클릭 = 단어 전체 선택",
        highlighted && copied == Some(Intent::Copy("beta".into())),
        format!("highlighted={highlighted} copied={copied:?}"),
    );
    app.mouse(
        event(MouseEventKind::Down(MouseButton::Left), beta_x + 1, beta_y),
        &selected,
    );
    let line_selected = app.render(80, 24);
    let triple = app.mouse(
        event(MouseEventKind::Up(MouseButton::Left), beta_x + 1, beta_y),
        &line_selected,
    );
    report.check(
        S,
        "트리플클릭 = 한 줄 전체 선택",
        triple == Some(Intent::Copy("alpha beta gamma".into())),
        format!("{triple:?}"),
    );

    let mut app = App::new(false);
    app.editor.insert("alpha beta\ngamma delta");
    let canvas = app.render(80, 24);
    let (gamma_x, gamma_y) = point(&canvas, "gamma");
    let (delta_x, delta_y) = point(&canvas, "delta");
    app.mouse(
        event(MouseEventKind::Down(MouseButton::Left), gamma_x, gamma_y),
        &canvas,
    );
    app.mouse(
        event(MouseEventKind::Up(MouseButton::Left), gamma_x, gamma_y),
        &canvas,
    );
    let shift_down = MouseEvent {
        kind: MouseEventKind::Down(MouseButton::Left),
        column: delta_x + 4,
        row: delta_y,
        modifiers: KeyModifiers::SHIFT,
    };
    let shift_up = MouseEvent {
        kind: MouseEventKind::Up(MouseButton::Left),
        ..shift_down
    };
    app.mouse(shift_down, &canvas);
    let selected = app.render(80, 24);
    let shifted = app.mouse(shift_up, &selected);
    report.check(
        S,
        "입력창 Shift+클릭 = 기존 캐럿에서 선택 확장",
        matches!(&shifted, Some(Intent::Copy(text)) if text.contains("gamma") && text.contains("delt")),
        format!("{shifted:?}"),
    );

    let mut app = app_with([message(
        "long",
        &format!("시작\n{}끝", "같은 줄\n".repeat(100)),
    )]);
    app.follow = false;
    app.scroll = 0;
    let mut canvas = app.render(80, 24);
    let (x, y) = (
        4,
        (0..canvas.height)
            .find(|&y| canvas.plain_line(y).contains("시작"))
            .unwrap(),
    );
    let bottom = canvas
        .hits
        .iter()
        .filter(|h| matches!(h.action, Action::SelectEntry(_)))
        .map(|h| h.y)
        .max()
        .unwrap();
    app.mouse(
        event(MouseEventKind::Down(MouseButton::Left), x, y),
        &canvas,
    );
    let now = std::time::Instant::now();
    app.mouse(
        event(MouseEventKind::Drag(MouseButton::Left), x, bottom),
        &canvas,
    );
    for step in 1..=12 {
        app.tick(now + std::time::Duration::from_millis(step * 70));
        canvas = app.render(80, 24);
    }
    let copied = app.mouse(
        event(MouseEventKind::Up(MouseButton::Left), x, bottom),
        &canvas,
    );
    let good = matches!(&copied, Some(Intent::Copy(text)) if text.starts_with("시작") && text.matches("같은 줄").count() > 24);
    report.check(
        S,
        "화면 밖 반복 행까지 빠른 드래그 복사",
        good,
        format!("scroll={} copied={copied:?}", app.scroll),
    );
}

fn activity(report: &mut Report) {
    const S: &str = "activity";
    let task = ActivityTask {
        id: "turn-1".into(),
        title: "렌더링 개선".into(),
        status: TaskStatus::Completed,
        session: Some("main".into()),
        partition: Some(Box::new(TaskPartition {
            reason: "변경이 하나로 연결됨 — 단일 실행".into(),
            files: vec!["src/markdown.rs".into()],
            symbols: vec!["src/markdown.rs::render".into()],
            ..TaskPartition::default()
        })),
        cost: Some(TaskCost {
            orient_bytes: 80,
            validate_bytes: 15,
            edit_bytes: 5,
            model_calls: 3,
            ..TaskCost::default()
        }),
        ..ActivityTask::default()
    };
    let child = ActivityTask {
        id: "agent-sub".into(),
        parent: Some("turn-1".into()),
        title: "하위 에이전트".into(),
        status: TaskStatus::Running,
        session: Some("sub".into()),
        ..ActivityTask::default()
    };
    let mut app = app_with([
        AgentEvent::scoped("main", AgentUpdate::TaskUpdated(task)),
        AgentEvent::scoped("main", AgentUpdate::TaskUpdated(child)),
    ]);
    app.activate(Action::Agents);
    let list = app.render(120, 40);
    report.check(
        S,
        "작업 목록 요약",
        screen_text(&list).contains("세션 · 파일 1 · 탐색 80%"),
        "",
    );
    app.activate(Action::Task("turn-1".into()));
    let detail = app.render(120, 40);
    let text = screen_text(&detail);
    for label in ["작업 구분", "단일 실행", "작업 비용", "모델 호출 3회"] {
        report.check(S, format!("상세: {label}"), text.contains(label), "");
    }
    report.dump(S, &detail);
    app.activate(Action::Task("agent-sub".into()));
    app.render(120, 40);
    let entered = app.activate(Action::EnterSession("sub".into()));
    report.check(
        S,
        "세션 진입 요청",
        entered == Some(Intent::Session(Some("sub".into()))),
        format!("{entered:?}"),
    );
    app.apply_event(loaded("sub", "하위"));
    let inside = app.render(120, 40);
    report.check(
        S,
        "복귀 버튼 표시",
        inside
            .hits
            .iter()
            .any(|h| h.action == Action::ReturnSession),
        "",
    );
    let back = app.activate(Action::ReturnSession);
    report.check(
        S,
        "복귀 요청",
        back == Some(Intent::Session(Some("main".into()))),
        format!("{back:?}"),
    );
}

fn quota(report: &mut Report) {
    const S: &str = "quota";
    let mut app = app_with([AgentEvent::global(AgentUpdate::ModelsLoaded(vec![
        "model-a".into(),
        "model-b".into(),
    ]))]);
    app.activate(Action::Model("model-a".into()));
    app.apply_event(AgentEvent::scoped(
        "main",
        AgentUpdate::ExecutionLimited {
            kind: LimitKind::UsageExhausted,
            reason: "test".into(),
        },
    ));
    app.editor.insert("제한 중 보낸 메시지");
    let blocked = app.activate(Action::Send);
    report.check(
        S,
        "소진된 모델로는 전송 보류",
        blocked.is_none() && app.queued.len() == 1,
        format!("{blocked:?}"),
    );
    app.queued.clear();
    app.activate(Action::Model("model-b".into()));
    report.check(
        S,
        "다른 모델 선택 시 제한 해제",
        app.execution_limit.is_none(),
        "limit stays after switching model",
    );
    app.editor.insert("새 모델로 보낸 메시지");
    let sent = app.activate(Action::Send);
    report.check(
        S,
        "다른 모델로 바로 전송",
        matches!(sent, Some(Intent::Submit(_))),
        format!("{sent:?}"),
    );
}

fn layouts(report: &mut Report) {
    const S: &str = "layout";
    for (w, h) in [(40u16, 12u16), (80, 24), (140, 40)] {
        let mut app = app_with([message("md", MARKDOWN)]);
        let canvas = app.render(w, h);
        let text = screen_text(&canvas);
        report.check(
            S,
            format!("{w}x{h}: 입력창 표시"),
            text.contains("메시지 입력") || text.contains("›"),
            "",
        );
        report.check(
            S,
            format!("{w}x{h}: 대화 탭 표시"),
            text.contains("대화"),
            "",
        );
        report.check(
            S,
            format!("{w}x{h}: 상단 빈 줄 없이 탭과 구분선 표시"),
            canvas.plain_line(0).contains("대화")
                && canvas.plain_line(1).starts_with("──")
                && canvas
                    .hits
                    .iter()
                    .any(|hit| hit.y == 0 && hit.action == Action::Back),
            canvas.plain_line(0),
        );
    }
    for width in [80, 140] {
        for busy in [false, true] {
            let mut app = app_with([message("reply", "대화 내용")]);
            app.busy = busy;
            app.model = "model-a".into();
            app.editor.insert("첫 줄\n둘째 줄");
            let canvas = app.render(width, 32);
            let input_top = canvas
                .hits
                .iter()
                .filter(|hit| hit.action == Action::Input)
                .map(|hit| hit.y)
                .min()
                .unwrap();
            let input_bottom = canvas
                .hits
                .iter()
                .filter(|hit| hit.action == Action::Input)
                .map(|hit| hit.y)
                .max()
                .unwrap();
            let stop = canvas
                .hits
                .iter()
                .find(|hit| hit.action == Action::Interrupt);
            report.check(
                S,
                format!("{width}: 실행 중에만 입력창 경계에 중단 버튼"),
                if busy {
                    stop.is_some_and(|hit| {
                        hit.y == input_top - 1
                            && canvas.hit(hit.x + 1, hit.y) == Some(Action::Interrupt)
                            && canvas.cells
                                [usize::from(hit.y) * usize::from(width) + usize::from(hit.x + 1)]
                            .text
                                == "■"
                    })
                } else {
                    stop.is_none()
                },
                "",
            );
            for y in [input_top - 1, input_bottom + 1] {
                report.check(
                    S,
                    format!("{width}: 입력창 가로줄 유지 (busy={busy}, y={y})"),
                    canvas.cells[usize::from(y) * usize::from(width) + usize::from(width - 12)
                        ..usize::from(y + 1) * usize::from(width)]
                        .iter()
                        .enumerate()
                        .all(|(offset, cell)| {
                            let x = width - 12 + offset as u16;
                            if stop.is_some_and(|hit| {
                                hit.y == y && x >= hit.x && x < hit.x + hit.width
                            }) {
                                matches!(cell.text.as_str(), " " | "■") && cell.tone == Tone::Danger
                            } else {
                                cell.text == "─" && cell.tone == Tone::Faint
                            }
                        }),
                    canvas.plain_line(y),
                );
            }
        }
    }
}

fn ui_hierarchy(report: &mut Report) {
    const S: &str = "ui_hierarchy";
    let mut command_app = app_with([AgentEvent::scoped(
        "main",
        AgentUpdate::TurnStarted {
            id: Some("command-turn".into()),
        },
    )]);
    for event in crate::backend::codec::notification(
        "item/started",
        &serde_json::json!({"threadId":"main","item":{"id":"command","type":"commandExecution","command":"python worker.py","status":"inProgress"}}),
    ) {
        command_app.apply_event(event);
    }
    for width in [40, 100] {
        let canvas = command_app.render(width, 24);
        let text = screen_text(&canvas);
        report.check(
            S,
            format!("{width}: 명령 실행 중 표시"),
            text.contains("명령 실행 중") && text.contains("python worker.py"),
            &text,
        );
        report.check(
            S,
            format!("{width}: 명령 실행 상태 Accent"),
            tone_of(&canvas, "명령 실행 중") == Some(Tone::Accent),
            &text,
        );
        report.dump("command_running", &canvas);
    }
    for event in crate::backend::codec::notification(
        "item/commandExecution/outputDelta",
        &serde_json::json!({"threadId":"main","itemId":"command","delta":"still running\n"}),
    ) {
        command_app.apply_event(event);
    }
    report.check(
        S,
        "출력 수신 중에도 명령 실행 표시 유지",
        screen_text(&command_app.render(100, 24)).contains("명령 실행 중"),
        "command output delta",
    );
    for event in crate::backend::codec::notification(
        "item/completed",
        &serde_json::json!({"threadId":"main","item":{"id":"command","type":"commandExecution","command":"python worker.py","status":"completed"}}),
    ) {
        command_app.apply_event(event);
    }
    report.check(
        S,
        "명령 완료 후 실행 표시 해제",
        !screen_text(&command_app.render(100, 24)).contains("명령 실행 중"),
        "command complete",
    );
    for (language, label) in [
        (crate::preferences::Language::Korean, "셸 실행 중"),
        (crate::preferences::Language::English, "Shell running"),
    ] {
        command_app.preferences.language = language;
        command_app.apply_event(AgentEvent::scoped(
            "main",
            AgentUpdate::EntryUpdated(Entry {
                id: "shell".into(),
                kind: Kind::Tool,
                title: "sleep 3; python check.py".into(),
                body: String::new(),
                status: "inProgress".into(),
                expanded: false,
            }),
        ));
        command_app.apply_event(AgentEvent::scoped(
            "main",
            AgentUpdate::WorkPhaseUpdated(WorkPhase::Shell),
        ));
        for width in [40, 100] {
            let canvas = command_app.render(width, 24);
            let text = screen_text(&canvas);
            report.check(
                S,
                format!("{language:?} {width}: 셸 실행 상태 표시"),
                text.contains(label),
                &text,
            );
            report.check(
                S,
                format!("{language:?} {width}: 셸 실행 상태 Accent"),
                tone_of(&canvas, label) == Some(Tone::Accent),
                &text,
            );
            report.dump("shell_running", &canvas);
        }
    }
    let mut permission_app = app_with([]);
    for (language, label) in [
        (crate::preferences::Language::Korean, "셸 실행 중"),
        (crate::preferences::Language::English, "Shell running"),
    ] {
        for width in [40, 80, 140] {
            let mut app = app_with([]);
            app.preferences.language = language;
            app.apply_event(AgentEvent::scoped(
                "main",
                AgentUpdate::ApprovalRequested(Approval {
                    id: "recheck".into(),
                    title: "Review changes".into(),
                    summary: "Change a value".into(),
                    detail: String::new(),
                    choices: vec![Choice {
                        label: "Apply".into(),
                        result: "accept".into(),
                    }],
                    answering: false,
                    expanded: false,
                }),
            ));
            app.activate(Action::Decide("recheck".into(), 0));
            app.apply_event(AgentEvent::scoped(
                "main",
                AgentUpdate::StatusUpdated {
                    status: "승인한 변경 재검증 중".into(),
                    busy: true,
                },
            ));
            app.apply_event(AgentEvent::scoped(
                "main",
                AgentUpdate::WorkPhaseUpdated(WorkPhase::Shell),
            ));
            let canvas = app.render(width, 30);
            let text = screen_text(&canvas);
            report.check(
                S,
                format!("{language:?} {width}: 승인 응답 후 실제 셸 상태"),
                text.contains(label),
                &text,
            );
            report.check(
                S,
                format!("{language:?} {width}: 승인 재검증 실행은 Accent"),
                tone_of(&canvas, label) == Some(Tone::Accent),
                &text,
            );
            report.dump("approval_shell_running", &canvas);
        }
    }
    permission_app.sandbox_mode = Some("dangerFullAccess".into());
    permission_app.approval_mode = Some("never".into());
    for (language, expected, absent) in [
        (
            crate::preferences::Language::Korean,
            "전체 접근 · 승인 없음",
            "Full access",
        ),
        (
            crate::preferences::Language::English,
            "Full access · Never ask",
            "전체 접근",
        ),
    ] {
        permission_app.preferences.language = language;
        let summary = permission_app.render(100, 24).plain_line(1);
        report.check(
            S,
            format!("{language:?}: 권한 요약 번역"),
            summary.contains(expected) && !summary.contains(absent),
            summary,
        );
    }
    for (language, label) in [
        (crate::preferences::Language::Korean, "세션"),
        (crate::preferences::Language::English, "Sessions"),
    ] {
        for (width, active) in [(140, false), (140, true), (48, true)] {
            let mut nav_app = app_with([]);
            nav_app.preferences.language = language;
            if active {
                nav_app.activate(Action::Sessions);
            }
            let canvas = nav_app.render(width, 24);
            let nav = canvas.plain_line(0);
            report.check(
                S,
                format!("{label} {width}: 세션 버튼에 화살표 없이 클릭 유지 (active={active})"),
                nav.contains(&format!(" {label} "))
                    && !nav.contains('▾')
                    && canvas.hits.iter().any(|hit| hit.action == Action::Sessions),
                nav,
            );
        }
    }
    let mut app = app_with([
        AgentEvent::scoped(
            "main",
            AgentUpdate::TokenUsageUpdated(TokenUsage {
                total: Some(120_000),
                input: Some(90_000),
                cached_input: Some(60_000),
                output: Some(30_000),
                reasoning_output: Some(18_000),
                last: Some(71_000),
                context_window: Some(100_000),
                ..TokenUsage::default()
            }),
        ),
        AgentEvent::scoped(
            "main",
            AgentUpdate::QuotaUpdated(QuotaSnapshot {
                id: "sample".into(),
                label: "Sample".into(),
                model: None,
                primary: Some(QuotaWindow {
                    used_percent: 1,
                    duration_minutes: None,
                    resets_at: None,
                }),
                secondary: None,
                credits_available: None,
                credits_unlimited: None,
                spend_control_reached: None,
                reached: None,
            }),
        ),
        AgentEvent::global(AgentUpdate::SessionsLoaded {
            sessions: vec![
                SessionSummary {
                    id: "main".into(),
                    title: "현재 작업".into(),
                    status: "idle".into(),
                    updated_at: None,
                },
                SessionSummary {
                    id: "older".into(),
                    title: "이전 작업".into(),
                    status: "active".into(),
                    updated_at: Some(1),
                },
            ],
            cursor: None,
            append: false,
        }),
    ]);
    app.panel_open = true;
    let overview = app.render(140, 36);
    let screen = screen_text(&overview);
    report.check(
        S,
        "활동 패널에서 컨텍스트 게이지 표시",
        screen.contains("Context 71%") && screen.contains("━"),
        "",
    );
    report.check(
        S,
        "활동 패널에서 측정된 할당량과 작업 수 표시",
        screen.contains("Quota 1%") && screen.contains("Queue 0 · Agent 0"),
        "",
    );
    report.check(
        S,
        "활동 패널에서 상세 사용량은 숨김",
        !screen.contains("Total tokens:"),
        "",
    );
    app.apply_event(AgentEvent::scoped(
        "main",
        AgentUpdate::EntryUpdated(Entry {
            id: "tool-running-1".into(),
            kind: Kind::Tool,
            title: "sh · cargo test".into(),
            body: "running tests...\ntest result: ok".into(),
            status: "inProgress".into(),
            expanded: false,
        }),
    ));
    let running_screen = screen_text(&app.render(140, 36));
    report.check(
        S,
        "현황은 실행 기록을 복제하지 않고 상세로 연결",
        running_screen.matches("sh · cargo test").count() == 1
            && running_screen.contains("실행 기록 보기")
            && !running_screen.contains("실시간 출력"),
        "",
    );
    let nav = overview.plain_line(0);
    report.check(
        S,
        "상위 메뉴는 대화 작업 세션 환경 설정의 다섯 계층",
        ["대화", "작업", "세션", "환경", "설정"]
            .iter()
            .all(|label| nav.contains(label))
            && !nav.contains("활동")
            && !nav.contains("모델")
            && !nav.contains("권한")
            && !nav.contains("스킬"),
        "",
    );

    report.check(
        S,
        "현황의 실행 상세 연결과 대화의 실행 표시",
        running_screen.contains("실행 기록 보기")
            && running_screen.matches("sh · cargo test").count() == 1
            && !running_screen.contains("running tests"),
        "",
    );
    app.apply_event(AgentEvent::scoped(
        "main",
        AgentUpdate::EntryUpdated(Entry {
            id: "tool-running-1".into(),
            kind: Kind::Tool,
            title: "sh · cargo test".into(),
            body: "running tests...\ntest result: ok".into(),
            status: "completed".into(),
            expanded: false,
        }),
    ));
    let finished_screen = screen_text(&app.render(140, 36));
    report.check(
        S,
        "완료하면 현황의 실행 연결은 정리하고 대화에는 기록 유지",
        !finished_screen.contains("실행 기록 보기") && finished_screen.contains("sh · cargo test"),
        "",
    );
    app.activate(Action::Command("/doctor".into()));
    let doctor_screen = screen_text(&app.render(140, 36));
    report.check(
        S,
        "/doctor 명령으로 환경 및 도구체인 진단 보고서 표시",
        doctor_screen.contains("Loom Doctor") && doctor_screen.contains("Loom Core V5"),
        "",
    );
    app.activate(Action::Command("/cost".into()));
    let cost_screen = screen_text(&app.render(140, 36));
    report.check(
        S,
        "/cost 명령으로 토큰 및 경제성 분석 표시",
        cost_screen.contains("비용 및 절감액"),
        "",
    );
    app.activate(Action::Agents);
    let active = app.render(140, 36);
    let activity = screen_text(&active);
    report.check(
        S,
        "활성 탭은 현재 상태 강조",
        tone_of(&active, "● 작업") == Some(Tone::Accent),
        "",
    );
    app.focus = Some(Action::Agents);
    let focused = app.render(140, 36);
    report.check(
        S,
        "키보드 선택은 활성 상태와 구분",
        tone_of(&focused, "● 작업") == Some(Tone::Selected),
        "",
    );
    app.focus = None;
    for width in [48, 80, 140] {
        app.editor.insert("보존할 초안");
        app.activate(Action::Command("/environment".into()));
        let env = app.render(width, 36);
        report.check(
            S,
            format!("환경 하위 메뉴와 현재 상위 표시 {width}"),
            env.plain_line(0).contains("● 환경")
                && ["/model", "/skills", "/permissions"].iter().all(|command| {
                    env.hits
                        .iter()
                        .any(|h| h.action == Action::Command((*command).into()))
                })
                && app.editor.text.ends_with("보존할 초안"),
            "",
        );
        report.dump(S, &env);
        app.activate(Action::Agents);
        let work = app.render(width, 36);
        report.check(
            S,
            format!("작업 하위 메뉴와 현재 상위 표시 {width}"),
            work.plain_line(0).contains("● 작업")
                && [
                    Action::Agents,
                    Action::Diff,
                    Action::Approvals,
                    Action::Command("/usage".into()),
                ]
                .iter()
                .all(|action| work.hits.iter().any(|h| &h.action == action)),
            "",
        );
        report.dump(S, &work);
    }
    report.check(
        S,
        "작업 목록은 사용량 요약을 반복하지 않음",
        activity.contains("● 목록")
            && activity.contains("○ 사용량")
            && !activity.contains("Context 71%")
            && !activity.contains("Reasoning output:"),
        "",
    );
    app.model_ledger_path = None;
    app.model_ledger = ledger_fixture();
    app.activate(Action::Command("/usage".into()));
    let detail = screen_text(&app.render(140, 36));
    report.check(
        S,
        "상세 첫 화면은 컨텍스트 게이지와 누적 사용량 우선",
        detail.contains("사용 71%")
            && detail.contains("71.0k / 100.0k")
            && detail.contains("누적 120.0k"),
        "",
    );
    let detail_rows = |width| {
        app.meter_rows(width)
            .into_iter()
            .map(|r| r.0)
            .take_while(|r| !r.contains("모델별 사용량"))
            .collect::<Vec<_>>()
    };
    let compact = detail_rows(38);
    let wide = detail_rows(80);
    report.check(
        S,
        "토큰 세부 정보는 입력과 출력, 하위 항목을 구별하고 너비에 맞게 정렬",
        [compact.as_slice(), wide.as_slice()]
            .iter()
            .enumerate()
            .all(|(i, rows)| {
                rows.iter()
                    .any(|row| row.contains("입력 90.0k") && row.contains("출력 30.0k"))
                    && rows
                        .iter()
                        .any(|row| row.contains("캐시 60.0k") && row.contains("추론 18.0k"))
                    && rows.iter().all(|row| {
                        unicode_width::UnicodeWidthStr::width(row.as_str())
                            <= if i == 0 { 38 } else { 80 }
                    })
            }),
        "",
    );
    report.check(
        S,
        "넓은 화면에서도 연관 수치를 가까이 배치",
        wide.iter()
            .find(|row| row.contains("입력 90.0k") && row.contains("출력 30.0k"))
            .is_some_and(|row| row.find("출력").unwrap() - row.find("입력").unwrap() < 35),
        "",
    );
    let mut native = app_with([AgentEvent::scoped(
        "main",
        AgentUpdate::TokenUsageUpdated(TokenUsage {
            total: Some(4000),
            input: Some(3000),
            output: Some(1000),
            ..TokenUsage::default()
        }),
    )]);
    native.activate(Action::Command("/usage".into()));
    let native_rows = native.meter_rows(38);
    let native_text = native_rows
        .iter()
        .map(|r| r.0.as_str())
        .collect::<Vec<_>>()
        .join("\n");
    report.check(
        S,
        "네이티브 미보고 컨텍스트를 토큰 누계로 추정하지 않음",
        native_text.contains("사용률 미보고")
            && native_text.contains("누적 4.0k")
            && !native_text.contains("사용 100%"),
        "",
    );
    report.dump("usage-detail", &app.render(140, 36));
    report.dump("usage-detail", &app.render(80, 36));
    // Loom's router today: total first, one bar per model, a bar opens that model's details.
    let panel = |app: &mut App, width: u16| {
        let mut text = String::new();
        for _ in 0..12 {
            text.push_str(&screen_text(&app.render(width, 36)));
            app.scroll += 20;
        }
        app.scroll = 0;
        text
    };
    let models = panel(&mut app, 140);
    report.check(
        S,
        "모델별 사용량 섹션",
        models.contains("모델별 사용량 · 오늘"),
        "",
    );
    let total = models.find("총 5.2k · 4회 · 모델 3개");
    report.check(
        S,
        "총계가 모델 막대보다 먼저",
        total.is_some()
            && models
                .find("▸ gemini-3-flash")
                .is_some_and(|bar| total < Some(bar)),
        "",
    );
    report.check(
        S,
        "가격 모르는 호출이 있으면 총 비용은 미확인",
        models.contains("비용 미확인"),
        "",
    );
    report.check(
        S,
        "어제 호출은 오늘 집계에서 제외",
        !models.contains("old-model"),
        "",
    );
    report.check(
        S,
        "막대는 토큰이 많은 모델이 가장 길게",
        {
            // the bar is on the name's line (wide panel) or the line below it (narrow panel)
            let bar = |name: &str| {
                let lines: Vec<&str> = models.lines().collect();
                lines
                    .iter()
                    .position(|l| l.contains(&format!("▸ {name}")))
                    .map_or(0, |i| {
                        lines[i..(i + 2).min(lines.len())]
                            .iter()
                            .map(|l| l.matches('━').count())
                            .max()
                            .unwrap_or(0)
                    })
            };
            bar("gemini-3-flash") > bar("gemma-4b") && bar("gemma-4b") > 0
        },
        "",
    );
    report.check(
        S,
        "상세는 누르기 전에는 숨김",
        !models.contains("경로 openrouter→gemini"),
        "",
    );
    app.activate(Action::Command(
        "/usage model openrouter→gemini|gemini-3-flash".into(),
    ));
    let opened = panel(&mut app, 140);
    report.check(
        S,
        "막대를 누르면 경로·호출·실패·토큰 상세",
        opened.contains("▾ gemini-3-flash")
            && opened.contains("경로 openrouter→gemini")
            && opened.contains("호출 2회 · 실패 1")
            && opened.contains("in 2.4k · cached 1.6k · out 180"),
        "",
    );
    // the limit itself comes from defaults or ~/.loom/free-tiers.toml
    report.check(
        S,
        "무료 등급 사용률",
        opened.contains("gemini · 요청 1/"),
        "",
    );
    let narrow = {
        app.panel_open = true;
        panel(&mut app, 120)
    };
    report.check(
        S,
        "좁은 패널에서도 막대와 상세가 줄 중간에서 끊기지 않음",
        narrow.contains("━") && narrow.contains("in 2.4k · cached 1.6k · out 180"),
        "",
    );
    app.activate(Action::Command(
        "/usage model openrouter→gemini|gemini-3-flash".into(),
    ));
    report.check(
        S,
        "같은 막대를 다시 누르면 상세 닫힘",
        !panel(&mut app, 140).contains("경로 openrouter→gemini"),
        "",
    );
    // OmniRoute is no longer a Loom dependency: /usage shows Loom's own router only.
    let usage = panel(&mut app, 140);
    report.check(
        S,
        "사용량 화면에 OmniRoute 계정 할당량 없음",
        !usage.to_lowercase().contains("omniroute"),
        &usage,
    );
    app.activate(Action::Sessions);
    app.panel_open = false;
    let wide = app.render(80, 34);
    report.check(
        S,
        "넓은 세션 목록은 상태와 시간을 한 줄에",
        screen_text(&wide)
            .lines()
            .any(|row| row.contains("○ 이전 작업") && row.contains("실행 중")),
        "",
    );
    report.check(
        S,
        "세션 실행 상태는 강조색 유지",
        tone_of(&wide, "● 실행 중") == Some(Tone::Accent),
        "",
    );
    let narrow = app.render(48, 34);
    let narrow_text = screen_text(&narrow);
    report.check(
        S,
        "좁은 세션 목록은 메타정보를 아래에",
        narrow_text.contains("○ 이전 작업")
            && narrow_text.lines().any(|row| {
                row.trim_start().starts_with("오래")
                    || row.trim_start().starts_with("실행 중")
                    || row.contains("● 실행 중")
            }),
        "",
    );
    report.check(
        S,
        "현재 세션은 연결 표시로 구분",
        screen_text(&wide).contains("● self-check"),
        "",
    );
    report.dump(S, &overview);
}

/// Screens for UX review: every main view at a wide and a narrow size, no checks.
fn tour(report: &mut Report) {
    const S: &str = "tour";
    let entry = |id: &str, kind: Kind, title: &str, body: &str| {
        AgentEvent::scoped(
            "main",
            AgentUpdate::EntryUpdated(Entry {
                id: id.into(),
                kind,
                title: title.into(),
                body: body.into(),
                status: "completed".into(),
                expanded: false,
            }),
        )
    };
    let events = || {
        vec![
            AgentEvent::global(AgentUpdate::ModelsLoaded(vec![
                "model-a".into(),
                "model-b".into(),
            ])),
            entry(
                "u1",
                Kind::User,
                "› 나",
                "로그인 오류 고쳐줘. 세션이 만료되면 401이 나와.",
            ),
            entry(
                "t1",
                Kind::Tool,
                "rg -n refresh_session src",
                "src/auth.rs:42: fn refresh_session(token: &str)\nsrc/api.rs:10: refresh_session(&t)",
            ),
            entry(
                "c1",
                Kind::Change,
                "파일 변경",
                "+++ src/auth.rs\n@@ -42,3 +42,4 @@\n fn refresh_session(token: &str) {\n-    let s = load(token);\n+    let s = load(token)?;\n+    s.renew();\n }",
            ),
            message("md", MARKDOWN),
            AgentEvent::global(AgentUpdate::SessionsLoaded {
                sessions: vec![
                    SessionSummary {
                        id: "main".into(),
                        title: "self-check".into(),
                        status: "idle".into(),
                        updated_at: None,
                    },
                    SessionSummary {
                        id: "old".into(),
                        title: "지난주 리팩터링".into(),
                        status: "idle".into(),
                        updated_at: None,
                    },
                ],
                cursor: None,
                append: false,
            }),
        ]
    };
    for (w, h) in [(110u16, 36u16), (48, 24)] {
        for (label, action) in [
            ("대화", None),
            ("작업", Some(Action::Agents)),
            ("세션", Some(Action::Sessions)),
            ("모델", Some(Action::Command("/model".into()))),
            ("설정", Some(Action::Command("/settings".into()))),
            ("도움말", Some(Action::Command("/help".into()))),
        ] {
            let mut app = app_with(events());
            if let Some(action) = action {
                app.activate(action);
            }
            let canvas = app.render(w, h);
            report.dumps.push(Dump {
                scenario: S,
                label: format!("{label} {w}x{h}"),
                width: w,
                height: h,
                rows: styled_rows(&canvas),
            });
        }
    }
    let mut app = app_with(events());
    app.apply_event(AgentEvent::scoped(
        "main",
        AgentUpdate::ApprovalRequested(Approval {
            id: "a1".into(),
            title: "명령 실행 승인".into(),
            summary: "cargo test --all".into(),
            detail: "작업 폴더에서 테스트를 실행합니다.".into(),
            choices: vec![
                Choice {
                    label: "허용".into(),
                    result: "accept".into(),
                },
                Choice {
                    label: "거절".into(),
                    result: "decline".into(),
                },
            ],
            answering: false,
            expanded: false,
        }),
    ));
    let canvas = app.render(110, 36);
    report.dumps.push(Dump {
        scenario: S,
        label: "승인 요청 110x36".into(),
        width: 110,
        height: 36,
        rows: styled_rows(&canvas),
    });
}

fn composer_and_navigation(report: &mut Report) {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    const S: &str = "composer_and_navigation";
    let mut recalled = app_with([]);
    recalled.editor.insert("직전 입력");
    recalled.activate(Action::Send);
    let canvas = recalled.render(80, 24);
    recalled.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &canvas);
    report.check(
        S,
        "빈 입력창 ↑ 직전 메시지 복원",
        recalled.editor.text == "직전 입력",
        recalled.editor.text.clone(),
    );
    let canvas = recalled.render(80, 24);
    recalled.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &canvas);
    report.check(
        S,
        "기록 끝 ↓ 입력창 비움",
        recalled.editor.text.is_empty(),
        recalled.editor.text.clone(),
    );
    let mut queued = app_with([]);
    queued.busy = true;
    queued.turn = Some("running".into());
    queued.editor.insert("첫 메시지");
    queued.activate(Action::Send);
    queued.editor.insert("수정할 메시지");
    queued.activate(Action::Send);
    let canvas = queued.render(80, 24);
    queued.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &canvas);
    report.check(
        S,
        "빈 입력창 ↑ 최신 대기 메시지를 편집기로 회수",
        queued.editor.text == "수정할 메시지"
            && queued.queued.len() == 1
            && queued.queued.front().map(|item| item.text.as_str()) == Some("첫 메시지"),
        format!(
            "editor={:?}, queued={:?}",
            queued.editor.text, queued.queued
        ),
    );
    let mut scrolling_queue = app_with([]);
    scrolling_queue.busy = true;
    scrolling_queue.turn = Some("running".into());
    for index in 0..5 {
        scrolling_queue.editor.insert(&format!("queued {index}"));
        scrolling_queue.activate(Action::Send);
    }
    let before = scrolling_queue.render(80, 24);
    let queue_y = (0..before.height)
        .find(|y| before.plain_line(*y).contains("대기열"))
        .unwrap();
    scrolling_queue.mouse(
        MouseEvent {
            kind: MouseEventKind::ScrollDown,
            column: 4,
            row: queue_y + 1,
            modifiers: KeyModifiers::NONE,
        },
        &before,
    );
    let after = scrolling_queue.render(80, 24);
    let after_text = screen_text(&after);
    report.check(
        S,
        "대기열 위 휠로 숨은 다음 항목 스크롤",
        after_text.contains("queued 3") && !after_text.contains("queued 0"),
        after_text,
    );
    report.dump(S, &after);
    let mut slash = app_with([]);
    slash.editor.insert("/");
    let canvas = slash.render(80, 24);
    slash.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &canvas);
    report.check(
        S,
        "슬래시 메뉴 Tab은 실행하지 않고 입력창에 명령을 채움",
        slash.editor.text.starts_with('/') && slash.submitted.is_none() && slash.view == View::Chat,
        format!(
            "editor={:?}, submitted={:?}, view={:?}",
            slash.editor.text, slash.submitted, slash.view
        ),
    );
    for width in [48, 140] {
        let mut app = app_with([message("reply", "대화 내용")]);
        app.model = "model-a".into();
        app.busy = true;
        let canvas = app.render(width, 32);
        let input = canvas
            .hits
            .iter()
            .find(|h| h.action == Action::Input)
            .unwrap();
        let header = canvas.plain_line(input.y - 1);
        report.check(
            S,
            format!("{width}: 상태와 모델은 입력 위에 표시"),
            header.contains("실타래를 살피는 중")
                && header.contains("model-a")
                && canvas.plain_line(0).contains("대화"),
            header,
        );
        report.check(
            S,
            format!("{width}: 입력 위 모델 선택 버튼"),
            canvas
                .hits
                .iter()
                .any(|h| h.y == input.y - 1 && h.action == Action::Command("/model".into())),
            "",
        );
        app.selected_skills.push(Skill {
            name: "review".into(),
            path: "review".into(),
            description: String::new(),
        });
        let skills = app.render(width, 32);
        report.check(
            S,
            format!("{width}: 상태와 모델 이동 후 선택한 스킬 유지"),
            screen_text(&skills).contains("$review")
                && skills
                    .hits
                    .iter()
                    .any(|h| h.action == Action::RemoveSkill("review".into())),
            "",
        );
        app.selected_skills.clear();
        app.busy = false;
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &canvas);
        app.key(KeyEvent::new(KeyCode::Right, KeyModifiers::NONE), &canvas);
        report.check(
            S,
            format!("{width}: 상단 탭 오른쪽 이동"),
            app.focus == Some(Action::Agents),
            format!("{:?}", app.focus),
        );
        app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &canvas);
        let canvas = app.render(width, 32);
        app.key(KeyEvent::new(KeyCode::Left, KeyModifiers::NONE), &canvas);
        report.check(
            S,
            format!("{width}: 탭 실행 후에도 왼쪽 이동"),
            app.focus == Some(Action::Back),
            "",
        );
        app.panel_open = width >= 110;
        app.models = (0..20).map(|i| format!("model-{i}")).collect();
        app.activate(Action::Command("/model".into()));
        app.focus = Some(Action::Model("model-0".into()));
        for i in 1..20 {
            let canvas = app.render(width, 24);
            app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &canvas);
            let canvas = app.render(width, 24);
            let action = Action::Model(format!("model-{i}"));
            report.check(
                S,
                format!("{width}: 모델 목록 ↓ {i} 및 화면 밖 선택 자동 스크롤"),
                app.focus.as_ref() == Some(&action)
                    && canvas.hits.iter().any(|h| h.action == action),
                "",
            );
        }
        app.sessions_loaded = true;
        app.sessions = ["older", "oldest"]
            .map(|id| SessionSummary {
                id: id.into(),
                title: id.into(),
                status: "idle".into(),
                updated_at: None,
            })
            .to_vec();
        app.activate(Action::Sessions);
        app.focus = Some(Action::Resume("older".into()));
        let canvas = app.render(width, 24);
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &canvas);
        report.check(
            S,
            format!("{width}: 세션 목록 ↓ 이동"),
            app.focus == Some(Action::Resume("oldest".into())),
            "",
        );
        let canvas = app.render(width, 24);
        app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &canvas);
        report.check(
            S,
            format!("{width}: 세션 목록 ↑ 이동"),
            app.focus == Some(Action::Resume("older".into())),
            "",
        );
        report.dump(S, &app.render(width, 24));
    }
    let mut app = app_with([]);
    let item = serde_json::json!({"type":"agentMessage", "id":"async", "delivery":"async", "text":"어디인가요?\n• 상단 탭·버튼 사이 이동• 모델·세션 목록 선택• 메시지 입력창 커서 이동", "questions":[{"title":"어디인가요?", "options":["상단 탭·버튼 사이 이동", "모델·세션 목록 선택", "메시지 입력창 커서 이동"]}]});
    for update in crate::backend::codec::item_events(&item) {
        app.apply_event(AgentEvent::scoped("main", update));
    }
    let id = app.input_forms[0].request.id.clone();
    app.apply_event(AgentEvent::scoped("main", AgentUpdate::InputResolved(id)));
    let text = screen_text(&app.render(100, 32));
    report.check(
        S,
        "질문 답변 뒤 선택지 중복 표시 제거",
        text.contains("어디인가요?")
            && !text.contains("상단 탭·버튼 사이 이동")
            && !text.contains("모델·세션 목록 선택"),
        text,
    );
}

fn input_images_and_model_hierarchy(report: &mut Report) {
    const S: &str = "input_images_and_model_hierarchy";
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("screen shot.png");
    // Valid 1x1 RGBA PNG, including pixel data and checksums.
    let png: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137, 0, 0, 0, 11, 73, 68, 65, 84, 120, 156, 99, 96, 0, 2, 0, 0, 5, 0,
        1, 165, 246, 69, 64, 0, 0, 0, 0, 73, 69, 78, 68, 174, 66, 96, 130,
    ];
    std::fs::write(&path, png).unwrap();
    let mut app = app_with([]);
    app.paste(&format!("'{}'", path.display()));
    let canvas = app.render(80, 24);
    let text = screen_text(&canvas);
    report.check(
        S,
        "이미지 파일 붙여넣기는 첨부로 표시",
        text.contains("이미지") && text.contains("screen shot.png"),
        &text,
    );
    for (width, height) in [(40, 12), (80, 24), (140, 24)] {
        let canvas = app.render(width, height);
        report.check(
            S,
            format!("{width}: 첨부 제거와 입력창을 함께 표시"),
            canvas
                .hits
                .iter()
                .any(|hit| hit.action == Action::RemoveImage(0))
                && canvas.hits.iter().any(|hit| hit.action == Action::Input),
            screen_text(&canvas),
        );
        report.dump(S, &canvas);
    }
    let canvas = app.render(140, 24);
    report.check(
        S,
        // No usage is known yet, so the bottom line stays empty rather than showing dashes.
        "첨부가 있어도 하단은 사용량만 표시",
        !canvas.plain_line(canvas.height - 1).contains("Ctrl+V")
            && !canvas.plain_line(canvas.height - 1).contains("—"),
        screen_text(&canvas),
    );
    let intent = app.activate(Action::Send);
    report.check(
        S,
        "텍스트 없이 이미지 전송",
        matches!(intent, Some(crate::app::Intent::Submit(ref text)) if text.is_empty()),
        format!("{intent:?}"),
    );
    // Terminal.app can send a dragged file as ordinary keystrokes without paste markers.
    app = app_with([]);
    app.editor.insert("이 그림 설명해줘 ");
    let canvas = app.render(80, 24);
    for ch in format!("'{}' ", path.display()).chars() {
        app.key(
            crossterm::event::KeyEvent::new(
                crossterm::event::KeyCode::Char(ch),
                crossterm::event::KeyModifiers::NONE,
            ),
            &canvas,
        );
    }
    app.tick(std::time::Instant::now() + std::time::Duration::from_millis(400));
    report.check(
        S,
        "일반 키 입력으로 전달된 이미지 드롭 자동 첨부",
        app.images.len() == 1 && app.editor.text == "이 그림 설명해줘 ",
        screen_text(&app.render(80, 24)),
    );
    report.dump(S, &app.render(80, 24));
    app = app_with([]);
    app.model = "model-a".into();
    app.models = vec!["model-a".into(), "model-b".into()];
    app.model_profiles = vec![crate::agent::ModelProfile {
        id: "model-a".into(),
        default_effort: Some("medium".into()),
        efforts: ["low", "medium", "high", "xhigh"]
            .into_iter()
            .map(|value| crate::agent::EffortOption {
                value: value.into(),
                description: format!("{value} 설명이 반복되면 모델을 고르기 어렵습니다."),
            })
            .collect(),
    }];
    app.activate(Action::Command("/model".into()));
    for width in [48, 80, 140] {
        let canvas = app.render(width, 24);
        let text = screen_text(&canvas);
        // The result list precedes effort settings even when the latter is below
        // the viewport; the search field is now given the first visible rows.
        let model_rows = app.content_rows_for(&View::Models, usize::from(width - 4));
        let model_text = model_rows.iter().map(|row| row.0.as_str()).collect::<Vec<_>>().join("\n");
        report.check(
            S,
            format!("{width}: 모델 목록이 추론 설정보다 먼저"),
            text.contains("model-b") && model_text.find("model-b")
                .zip(model_text.find("Reasoning effort"))
                .is_some_and(|(m, e)| m < e),
            &text,
        );
        report.check(
            S,
            format!("{width}: 선택하지 않은 effort 설명은 숨김"),
            !text.contains("low 설명") && !text.contains("xhigh 설명"),
            &text,
        );
        report.dump(S, &canvas);
    }
    // Loom owns automatic routing; every connection is a closed group of its models.
    app = app_with([]);
    app.model = "auto".into();
    app.models = [
        "auto",
        "ollama/gemma",
        "openrouter/google/gemini-3-flash",
        "openrouter/google/gemini-2.5-pro",
        "openrouter/groq/llama-x",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    app.activate(Action::Command("/model".into()));
    let closed = screen_text(&app.render(140, 40));
    report.check(
        S,
        "자동 선택은 Loom 하나",
        closed.contains("자동 라우팅 (Loom)") && !closed.to_lowercase().contains("omniroute"),
        &closed,
    );
    report.check(
        S,
        "공급자 그룹은 접힌 채 개수만",
        closed.contains("▸ openrouter · 3")
            && closed.contains("▸ ollama · 1")
            && !closed.contains("gemini-2.5-pro"),
        &closed,
    );
    app.activate(Action::ToggleModelGroup("1로컬·직접 연결\u{1}openrouter".into()));
    app.activate(Action::Model("openrouter/google/gemini-2.5-pro".into()));
    let opened = screen_text(&app.render(140, 40));
    report.check(
        S,
        "그룹을 펼쳐 고른 모델은 짧은 이름으로",
        opened.contains("▾ openrouter · 3")
            && opened.contains("● google/gemini-2.5-pro")
            && opened.contains("선택한 모델 · google/gemini-2.5-pro"),
        &opened,
    );
    app.model = "auto".into();
    app.apply_event(AgentEvent::global(AgentUpdate::ModelResolved {
        requested: "auto".into(),
        provider: "openrouter".into(),
        model: "gemini-3-flash".into(),
    }));
    let status = screen_text(&app.render(140, 40));
    report.check(
        S,
        "상태줄에 실제 응답 모델",
        status.contains("auto → gemini-3-flash"),
        &status,
    );
    // A model chosen afterwards is shown alone: the last answer belongs to the old choice.
    app.models.extend(["work/model-a".into(), "work/model-b".into()]);
    app.model = "work/model-a".into();
    app.apply_event(AgentEvent::global(AgentUpdate::ModelResolved {
        requested: "work/model-a".into(),
        provider: "work".into(),
        model: "model-a-served".into(),
    }));
    app.activate(Action::Model("work/model-b".into()));
    let switched = screen_text(&app.render(140, 40));
    report.check(
        S,
        "모델을 바꾸면 이전 응답 모델을 거꾸로 표시하지 않음",
        switched.contains("model-b") && !switched.contains("→ model-a"),
        &switched,
    );
    report.dump(S, &app.render(140, 40));
}

fn response_bands(report: &mut Report) {
    use crate::engine::Background;
    const S: &str = "response_bands";
    let mut app = app_with([]);
    for turn in 0..4 {
        for (suffix, kind, body) in [
            ("user", Kind::User, format!("요청 {turn}")),
            (
                "reply",
                Kind::Assistant,
                format!("응답 {turn}\n\n**강조** `코드`\n\n> 인용"),
            ),
            ("tool", Kind::Tool, "tool output".into()),
            ("final", Kind::Assistant, format!("완료 {turn}")),
        ] {
            app.apply_event(AgentEvent::scoped(
                "main",
                AgentUpdate::EntryUpdated(Entry {
                    id: format!("{turn}-{suffix}"),
                    kind,
                    body,
                    title: "도구 실행".into(),
                    status: "completed".into(),
                    expanded: false,
                }),
            ));
        }
    }
    for width in [40, 80, 140] {
        app.panel_open = width == 140;
        let canvas = app.render(width, 80);
        for turn in 0..4 {
            let expected = if turn % 2 == 0 {
                Background::ResponseA
            } else {
                Background::ResponseB
            };
            for prefix in ["요청", "응답", "완료"] {
                let needle = format!("{prefix} {turn}");
                let y = (0..canvas.height).find(|&y| canvas.plain_line(y).contains(&needle));
                report.check(
                    S,
                    format!("{width}: {needle} 배경과 왼쪽 여백"),
                    y.is_some_and(|y| {
                        let row = &canvas.cells[usize::from(y) * usize::from(width)..];
                        row[0].background == expected && row[3].background == expected
                    }),
                    "",
                );
            }
        }
        report.check(
            S,
            format!("{width}: 코드·인용 배경 유지"),
            canvas
                .cells
                .iter()
                .any(|cell| cell.background == Background::Code)
                && canvas
                    .cells
                    .iter()
                    .any(|cell| cell.background == Background::Quote),
            "",
        );
        report.check(
            S,
            format!("{width}: 입력창 배경 유지"),
            canvas.cells[usize::from(width) * 77].background == Background::Default,
            "",
        );
        report.dump(S, &canvas);
    }
    app.panel_open = false;
    app.follow = false;
    app.scroll = 16;
    let canvas = app.render(80, 18);
    let needle = "완료 1";
    let y = (0..canvas.height).find(|&y| canvas.plain_line(y).contains(needle));
    report.check(
        S,
        "스크롤 중에도 두 번째 응답 배경 유지",
        y.is_some_and(|y| canvas.cells[usize::from(y) * 80].background == Background::ResponseB),
        screen_text(&canvas),
    );

    // Exercise the indexed band lookup well beyond the visible viewport.
    let mut deep = app_with([]);
    for turn in 0..130 {
        for (suffix, kind, body) in [
            ("user", Kind::User, format!("요청 {turn}")),
            ("reply", Kind::Assistant, format!("응답 {turn}")),
        ] {
            deep.apply_event(AgentEvent::scoped(
                "main",
                AgentUpdate::EntryUpdated(Entry {
                    id: format!("deep-{turn}-{suffix}"),
                    kind,
                    body,
                    title: String::new(),
                    status: "completed".into(),
                    expanded: false,
                }),
            ));
        }
    }
    let rows = deep.content_rows_for(&crate::app::View::Chat, 76);
    let target = rows
        .iter()
        .position(|row| row.0.contains("요청 100"))
        .expect("deep transcript contains turn 100");
    deep.follow = false;
    deep.scroll = target.saturating_sub(1);
    let canvas = deep.render(80, 24);
    for turn in [100, 101] {
        let expected = if turn % 2 == 0 {
            Background::ResponseA
        } else {
            Background::ResponseB
        };
        for prefix in ["요청", "응답"] {
            let needle = format!("{prefix} {turn}");
            let y = (0..canvas.height).find(|&y| canvas.plain_line(y).contains(&needle));
            report.check(
                S,
                format!("긴 대화 스크롤: {needle} 배경"),
                y.is_some_and(|y| canvas.cells[usize::from(y) * 80].background == expected),
                screen_text(&canvas),
            );
        }
    }
}

fn quiet_chat(report: &mut Report) {
    const S: &str = "quiet_chat";
    let mut app = app_with([message("reply", "답변")]);
    app.diff = "+changed".into();
    app.activate(Action::Diff);
    app.activate(Action::Back);
    let canvas = app.render(100, 30);
    report.check(
        S,
        "검토한 변경 알림 숨김",
        !canvas.plain_line(1).contains("파일 변경"),
        canvas.plain_line(1),
    );
    app.tokens.total = Some(12400);
    app.quotas.push(QuotaSnapshot {
        id: "test".into(),
        label: "session".into(),
        model: None,
        primary: Some(QuotaWindow {
            used_percent: 27,
            duration_minutes: Some(300),
            resets_at: None,
        }),
        secondary: None,
        credits_available: None,
        credits_unlimited: None,
        spend_control_reached: None,
        reached: None,
    });
    let canvas = app.render(100, 30);
    report.check(
        S,
        "하단 누적 토큰 숫자",
        canvas.plain_line(29).contains("tokens 12k"),
        canvas.plain_line(29),
    );
    report.check(
        S,
        "하단 잔여 한도",
        canvas.plain_line(29).contains("73%"),
        canvas.plain_line(29),
    );
    report.check(
        S,
        "입력 아래 계기판 제거",
        !canvas.plain_line(28).contains("tokens"),
        canvas.plain_line(28),
    );
    report.check(
        S,
        "전송 버튼 제거",
        !canvas.hits.iter().any(|hit| hit.action == Action::Send),
        "",
    );
    report.dump(S, &canvas);
    app.diff = "+changed again".into();
    report.check(
        S,
        "새 변경은 다시 알림",
        app.render(100, 30).plain_line(1).contains("파일 변경"),
        "",
    );
    let canvas = app.render(100, 30);
    app.key(
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ),
        &canvas,
    );
    app.key(
        crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Esc,
            crossterm::event::KeyModifiers::NONE,
        ),
        &canvas,
    );
    let canvas = app.render(100, 30);
    report.check(
        S,
        "Esc 두 번 복원 시점 선택",
        // Esc opens the picker; code rewind is offered only after a completed
        // turn is selected and its saved checkpoint has been previewed.
        screen_text(&canvas).contains("복원 시점 선택")
            && !screen_text(&canvas).contains("대화와 코드 되돌리기"),
        screen_text(&canvas),
    );
    report.dump(S, &canvas);
}

fn quiet_timestamps(report: &mut Report) {
    use crate::preferences::Language;
    const S: &str = "quiet_timestamps";
    for language in [Language::Korean, Language::English] {
        for width in [40, 80, 140] {
            let mut app = app_with([message("timed-reply", "답변 내용")]);
            app.preferences.timestamps = true;
            app.preferences.language = language;
            let screen = app.render(width, 30);
            let text = screen_text(&screen);
            report.check(
                S,
                format!("{language:?} {width}: 수신 라벨 생략"),
                !text.contains("수신") && !text.contains("Received"),
                &text,
            );
            report.check(
                S,
                format!("{language:?} {width}: 시간과 답변 유지"),
                text.contains("UTC") && text.contains("답변 내용"),
                &text,
            );
            report.dump(S, &screen);
        }
    }
}

fn accent_settings(report: &mut Report) {
    const S: &str = "accent_settings";
    use crate::preferences::{Accent, Language};
    for accent in Accent::ALL {
        let value = accent.key();
        let label = accent.label(Language::Korean);
        let mut app = App::new(false);
        let intent = app.activate(Action::Command(format!("/settings accent {value}")));
        report.check(
            S,
            format!("{value}: 색상 변경과 저장"),
            intent == Some(Intent::SaveSettings),
            format!("{intent:?}"),
        );
        for width in [40, 80, 140] {
            let canvas = app.render(width, 24);
            let text = screen_text(&canvas);
            report.check(
                S,
                format!("{value} {width}: 렌더러에 색상 적용"),
                canvas.accent == accent,
                "",
            );
            report.check(
                S,
                format!("{value} {width}: 현재 색상 표시"),
                text.contains(&format!("● {label}")),
                &text,
            );
            report.check(
                S,
                format!("{value} {width}: 현재 색상을 악센트로 강조"),
                tone_of(&canvas, &format!("● {label}")) == Some(Tone::Accent),
                "",
            );
            report.check(
                S,
                format!("{value} {width}: 색상 선택 클릭"),
                actions_at(&canvas, label).contains(&&Action::Setting(format!("accent:{value}"))),
                &text,
            );
            report.dump(S, &canvas);
        }
        for tone in [
            Tone::Accent,
            Tone::Success,
            Tone::Warning,
            Tone::Danger,
            Tone::User,
            Tone::Selected,
            Tone::Added,
            Tone::Removed,
        ] {
            if accent != Accent::Cyan {
                report.check(
                    S,
                    format!("{value}: {tone:?} 색상도 변경"),
                    tone.color(accent) != tone.color(Accent::Cyan),
                    "",
                );
            }
        }
        let colors: Vec<_> = [Tone::Accent, Tone::Success, Tone::Warning, Tone::Danger]
            .map(|tone| tone.color(accent))
            .into();
        report.check(
            S,
            format!("{value}: 진행·완료·경고·실패 색상 구분"),
            colors
                .iter()
                .enumerate()
                .all(|(i, color)| !colors[..i].contains(color)),
            "",
        );
        let alternative = Accent::ALL
            .into_iter()
            .find(|other| *other != accent)
            .unwrap();
        app.focus = Some(Action::Setting(format!("accent:{}", alternative.key())));
        let canvas = app.render(80, 24);
        report.check(
            S,
            format!("{value}: 키보드 선택과 현재 색상 구분"),
            tone_of(&canvas, &format!("● {label}")) == Some(Tone::Accent)
                && tone_of(
                    &canvas,
                    &format!("○ {}", alternative.label(Language::Korean)),
                ) == Some(Tone::Selected),
            "",
        );
        app.focus = None;
        app.activate(Action::Setting("english".into()));
        let canvas = app.render(80, 36);
        report.check(
            S,
            format!("{value}: 영어 색상 이름"),
            screen_text(&canvas).contains(&format!("● {}", accent.label(Language::English))),
            "",
        );
        let previous = app.preferences.accent;
        let invalid = app.activate(Action::Command("/settings accent unknown".into()));
        report.check(
            S,
            format!("{value}: 잘못된 색상은 기존 설정 유지"),
            invalid.is_none() && app.preferences.accent == previous,
            "",
        );
    }
}

fn tool_targets_are_whole(report: &mut Report) {
    const S: &str = "tool_targets_are_whole";
    let event = |kind, column, row| MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE };
    let titles = ["open · README.md", "find · MoE", "expand · S12"];
    for width in [60u16, 100, 140] {
        let mut app = app_with([AgentEvent::scoped("main", AgentUpdate::TurnStarted { id: Some("t".into()) })]);
        for (i, title) in titles.iter().enumerate() {
            for status in ["inProgress", "completed"] {
                app.apply_event(AgentEvent::scoped(
                    "main",
                    AgentUpdate::EntryUpdated(Entry {
                        id: format!("tool-{i}"),
                        kind: Kind::Tool,
                        title: (*title).into(),
                        body: format!("대상 · {}", title.split(" · ").nth(1).unwrap_or("")),
                        status: status.into(),
                        expanded: false,
                    }),
                ));
            }
        }
        let canvas = app.render(width, 30);
        let text = screen_text(&canvas);
        report.check(
            S,
            format!("{width}: 도구 대상 마지막 글자까지 표시"),
            titles.iter().all(|t| text.contains(t)),
            "",
        );
        // Copying a row (triple click selects the line) keeps every character.
        for title in titles {
            let Some((x, y)) = (0..canvas.height).find_map(|y| {
                let line = canvas.plain_line(y);
                line.find(title).map(|byte| (unicode_width::UnicodeWidthStr::width(&line[..byte]) as u16, y))
            }) else {
                report.check(S, format!("{width}: {title} 행 찾기"), false, &text);
                continue;
            };
            // A fresh window per row, so earlier clicks do not count toward this one.
            let mut app = app_with([AgentEvent::scoped("main", AgentUpdate::TurnStarted { id: Some("t".into()) })]);
            for (i, title) in titles.iter().enumerate() {
                app.apply_event(AgentEvent::scoped(
                    "main",
                    AgentUpdate::EntryUpdated(Entry {
                        id: format!("tool-{i}"),
                        kind: Kind::Tool,
                        title: (*title).into(),
                        body: String::new(),
                        status: "completed".into(),
                        expanded: false,
                    }),
                ));
            }
            let mut copied = None;
            let mut frame = app.render(width, 30);
            for _ in 0..3 {
                app.mouse(event(MouseEventKind::Down(MouseButton::Left), x + 1, y), &frame);
                frame = app.render(width, 30);
                copied = app.mouse(event(MouseEventKind::Up(MouseButton::Left), x + 1, y), &frame);
            }
            report.check(
                S,
                format!("{width}: 줄 복사에 '{title}' 전체 포함"),
                matches!(&copied, Some(Intent::Copy(t)) if t.contains(title)),
                format!("{copied:?}"),
            );
        }
        if width == 140 {
            report.check(S, "할당량 정보가 없으면 Quota 줄 숨김", !text.contains("Quota"), &text);
        }
        if width == 100 {
            report.dump(S, &canvas);
        }
    }
}

fn token_footer(report: &mut Report) {
    const S: &str = "token_footer";
    let mut app = app_with([AgentEvent::scoped("main", AgentUpdate::TurnStarted { id: Some("t".into()) })]);
    app.preferences.show_tokens = true;
    let canvas = app.render(100, 24);
    let text = screen_text(&canvas);
    report.check(
        S,
        "토큰 정보가 없으면 하단 줄과 클릭 영역 없음",
        !text.contains("제목 없는 대화")
            && !canvas.hits.iter().any(|h| h.action == Action::Command("/usage".into())),
        &text,
    );
    app.apply_event(AgentEvent::scoped(
        "main",
        AgentUpdate::TokenUsageUpdated(crate::agent::TokenUsage {
            input: Some(1200),
            output: Some(300),
            total: Some(1500),
            ..Default::default()
        }),
    ));
    let canvas = app.render(100, 24);
    let text = screen_text(&canvas);
    report.check(S, "토큰 사용량이 있으면 하단에 표시", text.contains("tokens 1.5k"), &text);
    report.dump(S, &canvas);
}

fn auto_compaction_status(report: &mut Report) {
    const S: &str = "auto_compaction_status";
    let mut app = app_with([AgentEvent::scoped("main", AgentUpdate::TurnStarted { id: Some("t".into()) })]);
    app.apply_event(AgentEvent::global(AgentUpdate::CompressionState { running: true, error: None }));
    for width in [48u16, 100] {
        let canvas = app.render(width, 24);
        let text = screen_text(&canvas);
        report.check(S, format!("{width}: 요청 전 자동 압축 중 표시"), text.contains("압축 중"), &text);
        report.dump(S, &canvas);
    }
    app.apply_event(AgentEvent::global(AgentUpdate::CompressionState { running: false, error: None }));
    app.apply_event(AgentEvent::global(AgentUpdate::Notice(
        "이전 대화가 길어져 3턴을 자동으로 요약했습니다.".into(),
    )));
    let text = screen_text(&app.render(100, 24));
    report.check(S, "압축이 끝나면 압축 표시 해제", !text.contains("압축 중"), &text);
}

fn remote_control(report: &mut Report) {
    const S: &str = "remote_control";
    let url = "http://192.168.0.2:8787/t/ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut app = app_with([AgentEvent::scoped("main", AgentUpdate::TurnStarted { id: None })]);
    app.remote_started(url.into(), crate::remote::qr_modules(url), false);
    let canvas = app.render(100, 40);
    let text = screen_text(&canvas);
    let qr_cells = canvas.cells.iter().filter(|c| c.tone == Tone::QrDark || c.tone == Tone::QrLight).count();
    report.check(S, "100x40: QR 코드와 주소 표시", qr_cells > 200 && text.contains("원격 조작") && text.contains("8787"), &text);
    report.check(
        S,
        "QR은 터미널 테마와 무관한 흑백",
        canvas.cells.iter().any(|c| c.background == crate::engine::Background::QrLight)
            && canvas.cells.iter().any(|c| c.tone == Tone::QrDark),
        "",
    );
    report.check(
        S,
        "닫기 버튼과 주소 복사",
        canvas.hits.iter().any(|h| h.action == Action::Command("/remote hide".into()))
            && canvas.hits.iter().any(|h| h.action == Action::Command("/remote copy".into())),
        "",
    );
    report.dump(S, &canvas);
    for (width, height) in [(60u16, 24u16), (40, 20)] {
        let canvas = app.render(width, height);
        let qr = canvas.cells.iter().filter(|c| c.tone == Tone::QrDark || c.tone == Tone::QrLight).count();
        let text = screen_text(&canvas);
        report.check(S, format!("{width}x{height}: 좁은 터미널에서도 QR과 주소"), qr > 200 && text.contains("8787"), &text);
        report.dump(S, &canvas);
    }
    let tiny = screen_text(&app.render(40, 12));
    report.check(S, "40x12: QR이 안 들어가면 키우라는 안내와 주소", tiny.contains("키우면") && tiny.contains("8787"), &tiny);
    app.key(crossterm::event::KeyEvent::new(crossterm::event::KeyCode::Esc, KeyModifiers::NONE), &canvas);
    report.check(S, "Esc로 창만 닫고 원격은 유지", app.remote.as_ref().is_some_and(|v| !v.shown), "");
    report.check(S, "/remote로 다시 열기", { app.activate(Action::Command("/remote".into())); app.remote.as_ref().is_some_and(|v| v.shown) }, "");
    report.check(S, "/remote off는 끄기 요청", app.activate(Action::Command("/remote off".into())) == Some(Intent::Remote(None)), "");

    // A message from the browser is sent without touching the draft typed here.
    let mut app = app_with([]);
    app.editor.insert("my draft");
    app.selected_skills.push(crate::agent::Skill { name: "local-only".into(), path: "local/SKILL.md".into(), description: String::new() });
    let intent = app.remote_command(crate::remote::Command::Send("run the tests".into()));
    report.check(
        S,
        "원격 메시지 전송은 입력 중인 초안을 보존",
        matches!(&intent, Some(Intent::Submit(text)) if text == "run the tests") && app.editor.text == "my draft",
        format!("{intent:?} draft={:?}", app.editor.text),
    );
    report.check(S, "원격 메시지에 로컬 초안의 스킬을 붙이지 않음",
        app.selected_skills.len() == 1 && app.submitted_skills.is_empty(), "");
    app.apply_event(AgentEvent::scoped("main", AgentUpdate::InputRequested(crate::agent::InputRequest {
        id: "remote-question".into(), blocking: true,
        questions: vec![crate::agent::Question { id: "q".into(), header: "확인".into(), prompt: "대상을 골라주세요".into(),
            options: vec![], allow_custom: true, secret: true }],
    })));
    let snapshot = app.remote_snapshot();
    report.check(S, "브라우저용 상태에 대화와 상태 포함", snapshot["entries"].is_array() && snapshot["status"].is_string(), &snapshot.to_string());
    report.check(S, "원격 질문에 요청 식별자와 비밀 입력 표시",
        snapshot["questions"][0]["id"] == "remote-question" && snapshot["questions"][0]["questions"][0]["secret"] == true, &snapshot.to_string());
    let mut recovery = app_with([]);
    recovery.apply_event(AgentEvent::scoped("main", AgentUpdate::TurnCompleted {
        id: None, status: "failed".into(), error: Some("request failed".into()),
    }));
    let canvas = recovery.render(80, 24);
    report.check(S, "실패 후 이어서 시도 버튼", canvas.hits.iter().any(|h| h.action == Action::Command("/retry".into())), screen_text(&canvas));
    report.dump(S, &canvas);
    let narrow = recovery.render(40, 20);
    report.check(S, "좁은 터미널에서도 이어서 시도 가능", narrow.hits.iter().any(|h| h.action == Action::Command("/retry".into())), screen_text(&narrow));
    report.dump(S, &narrow);
    report.check(S, "모바일에도 복구 상태 제공", recovery.remote_snapshot()["recovery"] == true, "");
    let mut queued = app_with([]);
    queued.busy = true;
    queued.remote_command(crate::remote::Command::Send("queued from phone".into()));
    report.check(S, "모바일 대기 메시지와 옵션 제공", queued.remote_snapshot()["queue"][0]["text"] == "queued from phone" && queued.remote_snapshot()["queue_revision"].is_string(), queued.remote_snapshot().to_string());
    let reason = crate::backend::v5_projection::failure_summary("BadRequest: context window exceeded").unwrap();
    report.check(S, "문맥 초과는 이유와 복구 방법 안내", reason.contains("문맥") && reason.contains("압축"), reason);

}

fn settings_categories(report: &mut Report) {
    const S: &str = "settings_categories";
    let mut app = App::new(false);
    let workspace = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(workspace.path().join(".loom")).unwrap();
    std::fs::write(
        workspace.path().join(".loom/config.toml"),
        "[web_search]\nprovider = \"brave\"\n",
    )
    .unwrap();
    app.workspace_root = Some(workspace.path().to_path_buf());
    app.activate(Action::Command("/settings".into()));
    let canvas = app.render(80, 60);
    let text = screen_text(&canvas);
    for label in [
        "표시",
        "언어",
        "입력",
        "알림",
        "시작",
        "고급",
        "인터페이스 밀도",
        "도구 실행 상세",
        "토큰 사용량",
        "타임스탬프",
        "Enter 동작",
        "붙여넣기 확인",
        "전송 후 입력 기록",
        "터미널 벨",
        "이전 세션 복원",
        "마지막 작업 폴더",
        "에이전트 실행",
        "모델 요청 한도",
        "작업 시간 한도",
        "웹 도구",
        "web_fetch",
        "web_search · 설정됨: brave",
        "로그 및 진단",
        "설정 초기화",
    ] {
        report.check(S, label, text.contains(label), "");
    }
    report.check(
        S,
        "작업 한도 기본값은 제한 없음, 버전 안내 문구 없음",
        app.preferences.core_steps == 0
            && app.preferences.core_seconds == 0
            && canvas.hits.iter().any(|hit| hit.action == Action::Setting("set:core_steps:0".into()))
            && canvas.hits.iter().any(|hit| hit.action == Action::Setting("set:core_seconds:0".into()))
            && !text.contains("V4/V5"),
        "",
    );
    report.check(
        S,
        "악센트 별도 선택 화면",
        app.activate(Action::Setting("page:accent".into()))
            .is_none()
            && screen_text(&app.render(80, 24)).contains("← 설정"),
        "",
    );
    report.dump(S, &canvas);
    for width in [40, 80, 140] {
        app.activate(Action::Setting("page:main".into()));
        app.composer_active = true;
        app.activate(Action::Setting("page:diagnostics".into()));
        app.activate(Action::Setting("page:main".into()));
        report.check(S, format!("{width}: 설정 하위 화면에서 돌아오면 방향키 탐색 복원"),
            !app.composer_active, "");
        app.panel_open = width >= 110;
        let canvas = app.render(width, 24);
        report.check(
            S,
            format!("{width}: 선택지별 클릭 영역"),
            canvas
                .hits
                .iter()
                .any(|hit| hit.action == Action::Setting("set:density:compact".into())),
            "",
        );
        report.dump(S, &canvas);
        for _ in 0..70 {
            let canvas = app.render(width, 24);
            app.key(
                crossterm::event::KeyEvent::new(
                    crossterm::event::KeyCode::Down,
                    crossterm::event::KeyModifiers::NONE,
                ),
                &canvas,
            );
            if app.focus == Some(Action::Setting("page:reset".into())) {
                break;
            }
        }
        let canvas = app.render(width, 24);
        report.check(
            S,
            format!("{width}: 화면 밖 고급 항목까지 이동"),
            canvas
                .hits
                .iter()
                .any(|hit| hit.action == Action::Setting("page:reset".into()))
                && app.focus == Some(Action::Setting("page:reset".into())),
            "",
        );
    }
    app.activate(Action::Setting("page:main".into()));
    app.panel_open = false;
    let narrow = screen_text(&app.render(40, 70));
    let compact: String = narrow.chars().filter(|c| !c.is_whitespace()).collect();
    report.check(
        S,
        "좁은 화면에서도 즉시 전송 설명 끝까지 표시",
        compact.contains("즉시전송:현재응답중단후우선전송"),
        narrow,
    );
    app.activate(Action::Setting("set:activity_panel:true".into()));
    app.activate(Action::Setting("page:reset".into()));
    app.activate(Action::Setting("reset:confirm".into()));
    let reset = app.render(80, 60);
    report.check(
        S,
        "초기화는 현재 패널도 기본값으로 복원",
        !app.panel_open,
        screen_text(&reset),
    );
    app.editor.insert("보존할 초안");
    let hidden = app.render(140, 60);
    report.check(
        S,
        "활동이 숨김이어도 설정은 오른쪽에 유지",
        app.wide_layout && !app.preferences.activity_panel,
        screen_text(&hidden),
    );
    for shown in [true, false, true, false] {
        app.activate(Action::Setting(format!("set:activity_panel:{shown}")));
        app.render(140, 60);
        report.check(
            S,
            format!("활동 {shown}: 설정 위치와 초안 유지"),
            app.wide_layout && app.editor.text == "보존할 초안",
            "",
        );
    }
    app.activate(Action::Panel);
    app.render(140, 60);
    report.check(
        S,
        "설정 접기는 숨김 활동을 열지 않음",
        app.view == crate::app::View::Chat && !app.panel_open && !app.wide_layout,
        "",
    );
}

fn native_projection(report: &mut Report) {
    let mut app = App::new(false);
    app.apply_event(loaded("native", "Native Core"));
    app.apply_event(AgentEvent::scoped(
        "native",
        AgentUpdate::Notice(crate::backend::native::STARTUP_NOTICE.into()),
    ));
    let startup = screen_text(&app.render(80, 30));
    report.check(
        "native",
        "시작 시 편집·조회·대화 입력 방식 안내",
        startup.contains(":inspect") && startup.contains(":chat"),
        &startup,
    );
    report.check(
        "native",
        "일반 입력은 자동 처리, 명시적 편집 방식 안내",
        startup.contains("자동")
            && startup.contains(":edit")
            && !startup.contains("기본 입력=코드 편집"),
        &startup,
    );
    // The submitted message must render while Core is still indexing and has
    // not assigned a task id. This is the optimistic Native submit state.
    app.apply_event(AgentEvent::scoped(
        "native",
        AgentUpdate::TurnSubmitted {
            id: None,
            status: "inProgress".into(),
        },
    ));
    app.apply_event(AgentEvent::scoped(
        "native",
        AgentUpdate::EntryUpdated(Entry {
            id: "native-echo-1".into(),
            kind: Kind::User,
            title: String::new(),
            body: "Native 수신 즉시 표시".into(),
            status: "completed".into(),
            expanded: false,
        }),
    ));
    let pending_screen = app.render(80, 30);
    let pending_text = (0..pending_screen.height)
        .map(|y| pending_screen.plain_line(y))
        .collect::<Vec<_>>()
        .join("\n");
    report.check(
        "native",
        "Core 준비 중 즉시 입력 표시",
        pending_text.contains("Native 수신 즉시 표시"),
        &pending_text,
    );
    let status_before = screen_text(&pending_screen);
    app.tick(std::time::Instant::now() + std::time::Duration::from_secs(4));
    let status_after = screen_text(&app.render(80, 30));
    report.check(
        "native",
        "작업 단계가 같으면 경과 시간에도 상태 문구 유지",
        status_before.contains("실타래를 살피는 중") && status_after.contains("실타래를 살피는 중"),
        "stage label missing or changed",
    );
    app.preferences.progress_labels = crate::preferences::ProgressLabels::Minimal;
    let minimal_status = screen_text(&app.render(80, 30));
    report.check(
        "native",
        "설정으로 진행 문구를 간단히 표시",
        minimal_status.contains("작업 중") && !minimal_status.contains("실타래"),
        "minimal label missing",
    );
    app.preferences.progress_labels = crate::preferences::ProgressLabels::Stages;
    app.apply_event(AgentEvent::scoped(
        "native",
        AgentUpdate::TurnStarted {
            id: Some("native-task".into()),
        },
    ));
    app.apply_event(AgentEvent::scoped(
        "native",
        AgentUpdate::TaskUpdated(ActivityTask {
            id: "native-task".into(),
            title: "Native 검증과 승인".into(),
            worker: Some(WorkerKind::Small),
            status: TaskStatus::NeedsReview,
            reason: Some("AwaitingApproval · 독립 검증 통과".into()),
            critical: true,
            session: Some("native".into()),
            ..Default::default()
        }),
    ));
    app.apply_event(AgentEvent::scoped(
        "native",
        AgentUpdate::ApprovalRequested(Approval {
            id: "native-task".into(),
            title: "검증된 Native 변경 승인".into(),
            summary: "Core 검증 통과 · 원본은 승인 후 적용".into(),
            detail: "diff --git a/file.txt b/file.txt\n-old\n+new".into(),
            choices: vec![
                Choice {
                    label: "승인하고 적용".into(),
                    result: "accept".into(),
                },
                Choice {
                    label: "거절하고 보존".into(),
                    result: "reject".into(),
                },
            ],
            answering: false,
            expanded: false,
        }),
    ));
    for width in [40, 80, 140] {
        let screen = app.render(width, 30);
        let text = (0..screen.height)
            .map(|y| screen.plain_line(y))
            .collect::<Vec<_>>()
            .join("\n");
        report.check(
            "native",
            format!("{width}: Native 변경 승인 표시"),
            text.contains("승인"),
            &text,
        );
        report.dump("native", &screen);
    }
    app.apply_event(AgentEvent::scoped(
        "native",
        AgentUpdate::ApprovalResolved("native-task".into()),
    ));
    app.apply_event(AgentEvent::scoped(
        "native",
        AgentUpdate::TurnCompleted {
            id: Some("native-task".into()),
            status: "completed".into(),
            error: None,
        },
    ));
    let screen = app.render(80, 30);
    report.dump("native", &screen);

    // A task that failed before the window closed must still look failed
    // when Native reloads its session and task history.
    let mut restored = App::new(false);
    restored.apply_event(loaded("native", "Native Core"));
    restored.apply_event(AgentEvent::scoped(
        "native",
        AgentUpdate::TaskUpdated(ActivityTask {
            id: "failed-native-task".into(),
            title: "Native 복원 실패 기록".into(),
            status: TaskStatus::Failed,
            session: Some("native".into()),
            reason: Some("Unresolved · 검증을 완료하지 못함".into()),
            ..Default::default()
        }),
    ));
    restored.activate(Action::Task("failed-native-task".into()));
    let failed_screen = restored.render(140, 30);
    let failed_text = screen_text(&failed_screen);
    report.check(
        "native",
        "복원된 Native 실패 작업은 실패 상태로 표시",
        failed_text.contains("Native 복원 실패 기록") && failed_text.contains("실패"),
        &failed_text,
    );
    report.dump("native", &failed_screen);
}

fn skill_mentions(report: &mut Report) {
    const S: &str = "multiple inline skills";
    let mut app = App::new(false);
    app.thread = Some("main".into());
    app.skills = ["review", "test"].into_iter().map(|name| Skill {
        name: name.into(), path: format!("/tmp/{name}/SKILL.md"), description: String::new(),
    }).collect();
    app.editor.insert("변경 검토 $rev");
    let canvas = app.render(80, 30);
    report.check(S, "문장 중간 스킬 자동완성", canvas.hits.iter().any(|h| h.action == Action::Skill("/tmp/review/SKILL.md".into())), screen_text(&canvas));
    report.check(S, "문장 중간 자동완성도 스킬 메뉴로 표시", screen_text(&canvas).contains("스킬 ·"), screen_text(&canvas));
    report.dump(S, &canvas);
    app.activate(Action::Skill("/tmp/review/SKILL.md".into()));
    report.check(S, "스킬 선택은 주변 문장을 보존", app.editor.text == "변경 검토 ", &app.editor.text);
    app.editor.take_text();
    app.editor.insert("$test $REVIEW $test 함께 적용해줘");
    app.activate(Action::Send);
    let names: Vec<_> = app.submitted_skills.iter().map(|skill| skill.name.as_str()).collect();
    report.check(S, "직접 입력한 여러 스킬을 중복 없이 연결", names == ["review", "test"], format!("{names:?}"));
    report.dump(S, &app.render(80, 30));
    let mut slash = App::new(false);
    slash.thread = Some("main".into());
    slash.skills = app.skills.clone();
    slash.editor.insert("/review 변경 검토");
    let intent = slash.activate(Action::Send);
    report.check(S, "슬래시 스킬 호출과 인자 전달", intent == Some(Intent::Submit("$review 변경 검토".into())), format!("{intent:?}"));
    let mut picker = App::new(false);
    picker.skills = app.skills.clone();
    picker.view = View::Skills;
    picker.editor.insert("유지할 지시");
    picker.activate(Action::Skill("/tmp/review/SKILL.md".into()));
    picker.activate(Action::Skill("/tmp/test/SKILL.md".into()));
    let canvas = picker.render(80, 35);
    report.check(S, "선택창을 유지하며 여러 스킬 추가", picker.view == View::Skills && picker.selected_skills.len() == 2 && picker.editor.text == "유지할 지시", screen_text(&canvas));
    report.dump(S, &canvas);
}

pub fn run() -> Report {
    let mut report = Report::default();
    skill_mentions(&mut report);
    searchable_commands(&mut report);
    session_exit_notice(&mut report);
    v5_plan_command(&mut report);
    v5_disclosure(&mut report);
    v5_parallel_question(&mut report);
    v5_command_lifecycle(&mut report);
    v5_visible_progress(&mut report);
    v5_model_failure(&mut report);
    loom_ast_identity(&mut report);
    loom_diff_projection(&mut report);
    escape_stops_active_turn(&mut report);
    shift_enter_newline(&mut report);
    rewind_choices(&mut report);
    response_model_details(&mut report);
    native_projection(&mut report);
    version_approval_restore(&mut report);
    native_work_report(&mut report);
    native_tool_evidence(&mut report);
    native_readable_results(&mut report);
    native_model_recovery(&mut report);
    response_bands(&mut report);
    quiet_chat(&mut report);
    quiet_timestamps(&mut report);
    markdown(&mut report);
    streaming(&mut report);
    copy(&mut report);
    drag_copy(&mut report);
    activity(&mut report);
    quota(&mut report);
    layouts(&mut report);
    ui_hierarchy(&mut report);
    tool_timeline(&mut report);
    provider_connections(&mut report);
    App::activity_label_selfcheck(&mut report);
    App::activity_model_retention_selfcheck(&mut report);
    App::question_hover_selfcheck(&mut report);
    App::question_cancel_selfcheck(&mut report);
    searchable_model_catalog(&mut report);
    local_model_discovery_status(&mut report);
    lmx_models_are_shown_as_loom_engine(&mut report);
    model_management_and_commentary(&mut report);
    compact_diff_boundaries(&mut report);
    accent_settings(&mut report);
    settings_categories(&mut report);
    tool_targets_are_whole(&mut report);
    token_footer(&mut report);
    auto_compaction_status(&mut report);
    remote_control(&mut report);
    composer_and_navigation(&mut report);
    input_images_and_model_hierarchy(&mut report);
    tour(&mut report);
    report
}

fn searchable_commands(report: &mut Report) {
    use crossterm::event::{KeyCode, KeyEvent};
    const S: &str = "searchable_commands";
    for width in [48, 100, 180] {
        let mut app = App::new(false);
        app.editor.insert("작성 중인 요청");
        let canvas = app.render(width, 30);
        app.key(KeyEvent::new(KeyCode::Char('p'), KeyModifiers::CONTROL), &canvas);
        for character in "컨텍스트".chars() {
            let canvas = app.render(width, 30);
            app.key(KeyEvent::new(KeyCode::Char(character), KeyModifiers::NONE), &canvas);
        }
        let canvas = app.render(width, 30);
        let text = screen_text(&canvas);
        report.check(S, format!("{width}: 설명으로 명령 검색, 초안 보존"),
            text.contains("/compact") && !text.contains("/quit")
                && app.editor.text == "작성 중인 요청", &text);
        report.check(S, format!("{width}: 커서는 검색 입력에 표시"),
            canvas.cursor.is_some_and(|(_, y)| text.lines().nth(usize::from(y)).is_some_and(|line| line.contains("컨텍스트"))), &text);
        report.dump(S, &canvas);
        app.key(crossterm::event::KeyEvent::new(crossterm::event::KeyCode::Esc, KeyModifiers::NONE), &canvas);
        let canvas = app.render(width, 30);
        report.check(S, format!("{width}: 검색 취소 후 초안 복원"),
            app.editor.text == "작성 중인 요청" && !screen_text(&canvas).contains("/compact"),
            screen_text(&canvas));
        app.activate(Action::Menu);
        app.paste("존재하지않는명령");
        let canvas = app.render(width, 30);
        let intent = app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &canvas);
        report.check(S, format!("{width}: 빈 결과를 전송하지 않음"),
            screen_text(&canvas).contains("일치하는 명령이 없습니다") && intent.is_none()
                && app.editor.text == "작성 중인 요청", screen_text(&canvas));
        report.dump(S, &canvas);
    }
}

fn loom_diff_projection(report: &mut Report) {
    const S: &str = "loom_diff_projection";
    let mut app = App::new(false);
    app.apply_event(loaded("loom", "Loom"));
    app.apply_event(AgentEvent::scoped(
        "loom",
        AgentUpdate::EntryUpdated(Entry {
            id: "loom-turn-change".into(),
            kind: Kind::Change,
            title: "파일 변경 · src/lib.rs".into(),
            body: "diff --git a/src/lib.rs b/src/lib.rs\n--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-old\n+new".into(),
            status: "completed".into(),
            expanded: false,
        }),
    ));
    let chat = app.render(80, 30);
    let text = screen_text(&chat);
    report.check(
        S,
        "Loom 변경이 Codex 스타일 요약과 인라인 diff로 표시됨",
        text.contains("• Edited src/lib.rs (+1 -1)")
            && text.contains("1 -old")
            && text.contains("1 +new")
            && !text.contains("· 완료")
            && text.contains("변경 내용 전체 보기")
            && chat
                .hits
                .iter()
                .any(|hit| hit.action == Action::DiffEntry("loom-turn-change".into())),
        &text,
    );
    report.dump(S, &chat);
    app.activate(Action::DiffEntry("loom-turn-change".into()));
    let diff = screen_text(&app.render(80, 30));
    report.check(
        S,
        "Loom 변경 전체 보기에서 실제 +/- 라인을 확인 가능",
        diff.contains("- │ old") && diff.contains("+ │ new"),
        &diff,
    );
    let wide = app.render(120, 30);
    let wide_text = screen_text(&wide);
    report.check(
        S,
        "넓은 화면에서는 대화를 남기고 변경을 확장 사이드 패널로 표시",
        wide_text.contains("• Edited src/lib.rs (+1 -1)")
            && wide_text.contains("에이전트가 보고한 파일 변경")
            && (0..wide.height).any(|y| wide.plain_line(y).contains('│')),
        &wide_text,
    );
    report.dump(S, &wide);
}

fn loom_ast_identity(report: &mut Report) {
    const S: &str = "loom_ast_identity";
    let label = crate::backend::version_core::Version::Loom.label();
    report.check(S, "단일 코어가 Loom으로 식별됨", label == "Loom", label);
    let finish_detail = serde_json::json!({
        "operation_id": "finish-1",
        "tool": "finish",
        "args": {"answer": true, "summary": "최종 답변 원문"},
        "result_summary": "Result: delivered",
        "result": "delivered"
    })
    .to_string();
    let finish_body =
        crate::backend::versions::progress_entry_body("finish", "작업 결과 전달", &finish_detail);
    report.check(
        S,
        "Loom finish payload는 최종 답변과 중복 표시하지 않음",
        finish_body.is_empty(),
        &finish_body,
    );
    let mut app = app_with([
        AgentEvent::scoped(
            "main",
            AgentUpdate::Notice("Loom · /plan 목표로 작업 분할 · /ask 병렬 질문".into()),
        ),
        AgentEvent::scoped(
            "main",
            AgentUpdate::EntryUpdated(Entry {
                id: "loom-ast-context".into(),
                kind: Kind::Tool,
                title: "symbol_context · AST 문맥 · src/lib.rs::value".into(),
                body: "target: src/lib.rs::value · Fn · src/lib.rs:1-1".into(),
                status: "completed".into(),
                expanded: false,
            }),
        ),
        AgentEvent::scoped(
            "main",
            AgentUpdate::EntryUpdated(Entry {
                id: "loom-map-log".into(),
                kind: Kind::Tool,
                title: "map · 저장소 구조 확인 · 4 files · 12 symbols".into(),
                body: "입력\npath: .\n\n결과 · 4 files · 12 symbols\nmap of the repository".into(),
                status: "completed".into(),
                expanded: true,
            }),
        ),
    ]);
    let canvas = app.render(80, 24);
    let text = screen_text(&canvas);
    report.check(
        S,
        "Loom AST 준비 성공은 대화에 출력하지 않음",
        !text.contains("AST 인덱스 준비") && !text.contains("코드 탐색 준비"),
        &text,
    );
    report.check(
        S,
        "Loom 코어 이름이 화면에 보임",
        text.contains("Loom"),
        &text,
    );
    report.check(
        S,
        "Loom AST 도구가 일반 tool 대신 작업 의미로 보임",
        text.contains("AST 문맥") && text.contains("src/lib.rs::value"),
        &text,
    );
    report.check(
        S,
        "Loom 도구 로그에 입력·결과가 보이고 내부 이벤트 JSON은 숨김",
        text.contains("저장소 구조 확인")
            && text.contains("4 files · 12 symbols")
            && !text.contains("tool_started")
            && !text.contains("operation_id"),
        &text,
    );
    report.dump(S, &canvas);
}

fn shift_enter_newline(report: &mut Report) {
    use crossterm::event::{KeyCode, KeyEvent};
    let mut app = App::new(false);
    app.apply_event(loaded("main", "Shift+Enter"));
    app.apply_event(AgentEvent::scoped(
        "main",
        AgentUpdate::InputRequested(InputRequest {
            id: "newline-question".into(),
            blocking: true,
            questions: vec![Question {
                id: "q".into(),
                header: "직접 입력".into(),
                prompt: "두 줄로 답변하세요".into(),
                options: vec![],
                allow_custom: true,
                secret: false,
            }],
        }),
    ));
    app.paste("첫째 줄");
    let canvas = app.render(100, 24);
    let intent = app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT), &canvas);
    report.check(
        "shift_enter",
        "질문에서 Shift+Enter가 제출하지 않음",
        intent.is_none(),
        format!("{intent:?}"),
    );
    app.paste("둘째 줄");
    let canvas = app.render(100, 24);
    report.dump("shift_enter", &canvas);
    let intent = app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &canvas);
    report.check("shift_enter","Enter로 두 줄 답변 제출",matches!(intent,Some(Intent::Answer(_,ref answers)) if answers[0].value==AnswerValue::Text("첫째 줄\n둘째 줄".into())),format!("{intent:?}"));
}

fn native_work_report(report: &mut Report) {
    const S: &str = "native_work_report";
    let mut app = app_with([message(
        "progress",
        "파일을 확인하고 수정 결과를 검증합니다.",
    )]);
    for (id, path, result) in [("1", "a.rs", "결과: old"), ("2", "b.rs", "결과: new")] {
        app.apply_event(AgentEvent::scoped(
            "main",
            AgentUpdate::EntryUpdated(Entry {
                id: format!("native-tool-{id}"),
                kind: Kind::Tool,
                title: format!("read · {path} · {result}"),
                body: format!("{path}\n도구의 전체 출력"),
                status: "completed".into(),
                expanded: false,
            }),
        ));
    }
    app.apply_event(message("stop", "작업을 완료하지 못했습니다.\n\nCore 실행 보고\n격리 사본 수정 제안: a.rs\nCore 검증: 아직 수행하지 않음\n원본 반영: 없음\n남은 작업: 변경 검증\n중단 사유: 모델 호출 한도\n실행 근거: .loom/artifacts/trace"));
    for width in [40, 80, 140] {
        let screen = app.render(width, 40);
        let text = screen_text(&screen);
        for expected in [
            "파일을 확인",
            "a.rs",
            "b.rs",
            "결과: old",
            "결과: new",
            "Core 실행 보고",
            "원본 반영",
            "남은 작업",
            "중단 사유",
        ] {
            report.check(
                S,
                format!("{width}: {expected} 표시"),
                text.contains(expected),
                &text,
            );
        }
        report.check(
            S,
            format!("{width}: 접힌 도구도 결과 요약 표시"),
            !text.contains("도구의 전체 출력"),
            &text,
        );
        report.dump(S, &screen);
    }
}

fn native_tool_evidence(report: &mut Report) {
    use crate::backend::activity_events::{ContextSymbol, Event, EventKind, GraphEvidence};
    use crate::backend::version_conversation::Conversation;
    const S: &str = "native_tool_evidence";
    let mut conversation = Conversation::default();
    conversation.observe(&Event::graph(
        "worker-1",
        GraphEvidence::Context {
            bytes: 70,
            symbol_count: 1,
            truncated: false,
            symbols: vec![ContextSymbol {
                name: "alpha".into(),
                path: "alpha.rs".into(),
                start: 1,
                end: 1,
                role: "body".into(),
                bytes: 30,
                partial: false,
            }],
        },
    ));
    let mut tool = Event::new("worker-1", EventKind::ToolStarted, "started");
    tool.tool = Some("search".into());
    tool.detail = serde_json::json!({"query":"alpha"}).to_string();
    conversation.observe(&tool);
    for width in [40, 80, 140] {
        let mut app = app_with([]);
        for entry in [
            conversation.assistant("turn"),
            conversation.log("turn", false).unwrap(),
            conversation.graph_log("turn").unwrap(),
        ] {
            app.apply_event(AgentEvent::scoped("main", AgentUpdate::EntryUpdated(entry)));
        }
        let screen = app.render(width, 40);
        let text = screen_text(&screen);
        for expected in ["search", "alpha", "AST", "문맥 1 심볼"] {
            report.check(
                S,
                format!("{width}: 접힌 상태 {expected}"),
                text.contains(expected),
                &text,
            );
        }
        report.check(
            S,
            format!("{width}: 실행은 Accent"),
            tone_of(&screen, "◉") == Some(Tone::Accent),
            &text,
        );
        report.dump(S, &screen);
        tool.kind = EventKind::ToolFailed;
        tool.text = "query failed".into();
        let mut finished = Conversation::default();
        finished.observe(&tool);
        app.apply_event(AgentEvent::scoped(
            "main",
            AgentUpdate::EntryUpdated(finished.log("turn", true).unwrap()),
        ));
        let failed = app.render(width, 40);
        report.check(
            S,
            format!("{width}: 실패 기록은 Danger"),
            tone_of(&failed, "!") == Some(Tone::Danger),
            screen_text(&failed),
        );
    }
    let mut artifact = Conversation::default();
    let mut event = Event::new("worker-1", EventKind::ToolStarted, "read receipt");
    event.tool = Some("read_artifact".into());
    event.detail = serde_json::json!({"id":"a".repeat(64),"offset":3000}).to_string();
    artifact.observe(&event);
    for width in [40, 80, 140] {
        let mut app = app_with([]);
        for entry in [
            artifact.assistant("receipt"),
            artifact.log("receipt", false).unwrap(),
        ] {
            app.apply_event(AgentEvent::scoped("main", AgentUpdate::EntryUpdated(entry)));
        }
        let screen = app.render(width, 40);
        let text = screen_text(&screen);
        for expected in ["read_artifact", "offset 3000"] {
            report.check(
                S,
                format!("{width}: 원문 구간 {expected}"),
                text.contains(expected),
                &text,
            );
        }
        report.check(
            S,
            format!("{width}: 원문 읽기 실행 상태"),
            tone_of(&screen, "◉") == Some(Tone::Accent),
            &text,
        );
        report.dump("native_receipt", &screen);
    }
    let mut recovery = Conversation::default();
    for attempt in 1..=3 {
        recovery.observe(&Event::correction(
            "검토 응답",
            attempt,
            "tool assertion missing name",
        ));
    }
    for width in [40, 80, 140] {
        let mut app = app_with([]);
        for entry in [
            recovery.assistant("recovery"),
            recovery.log("recovery", true).unwrap(),
        ] {
            app.apply_event(AgentEvent::scoped("main", AgentUpdate::EntryUpdated(entry)));
        }
        let screen = app.render(width, 40);
        let text = screen_text(&screen);
        report.check(
            S,
            format!("{width}: 형식 교정은 접힌 기록"),
            !text.contains("tool assertion") && !text.contains("검증에 실패"),
            &text,
        );
        report.check(
            S,
            format!("{width}: 교정을 작업 실패로 표시하지 않음"),
            !text.contains("실패 3") && tone_of(&screen, "✓") == Some(Tone::Success),
            &text,
        );
        report.dump("native_recovery", &screen);
        let mut detail = recovery.log("recovery", true).unwrap();
        detail.expanded = true;
        app.apply_event(AgentEvent::scoped(
            "main",
            AgentUpdate::EntryUpdated(detail),
        ));
        let expanded = screen_text(&app.render(width, 40));
        report.check(
            S,
            format!("{width}: 펼치면 교정 근거 보존"),
            expanded.contains("tool assertion missing name"),
            &expanded,
        );
    }
    let mut waiting = Conversation::default();
    let mut read = Event::new("worker-1", EventKind::ToolSucceeded, "observed read");
    read.tool = Some("read_file".into());
    read.path = Some("README.md".into());
    waiting.observe(&read);
    waiting.observe(&Event::new(
        "worker-1",
        EventKind::ModelRequest,
        "next response",
    ));
    for width in [40, 80, 140] {
        let mut app = app_with([]);
        app.apply_event(AgentEvent::scoped(
            "main",
            AgentUpdate::EntryUpdated(waiting.assistant("waiting")),
        ));
        let screen = app.render(width, 40);
        let text = screen_text(&screen);
        report.check(
            S,
            format!("{width}: 응답 대기 중 직전 작업 보존"),
            text.contains("README.md") && text.contains("응답"),
            &text,
        );
        report.dump("native_waiting", &screen);
    }
}

fn native_readable_results(report: &mut Report) {
    use crate::backend::activity_events::{Event, EventKind};
    use crate::backend::{version_core::Outcome, versions::VersionBackend};
    const S: &str = "native_readable_results";
    let mut outcome: Outcome = serde_json::from_value(serde_json::json!({
        "version":"Loom","id":"readable","goal":"코어 차이를 설명해줘","status":"Accepted",
        "summary":"세 코어의 문서를 읽었습니다. V3는 독립 작업을 여러 워커에게 나눕니다.",
        "blocker":null,"remaining":"","diff":"","files":[],"sources":["README.md"],
        "records":".loom-v3","elapsed_ms":41000,"calls":7,"input":13180,"output":5290,"cached":null,
        "attempts":2,"validation":[],"plan_mode":"single","plan_reason":"Compare cores","groups":[],"model_calls":[],"events":[]
    })).unwrap();
    for status in ["Accepted", "Unresolved", "AwaitingApproval", "Cancelled"] {
        outcome.status = status.into();
        outcome.blocker = if status == "Unresolved" {
            Some("tool assertion missing name".into())
        } else {
            None
        };
        outcome.summary = if status == "Unresolved" {
            "Plan coverage review failed: tool assertion missing name".into()
        } else {
            "세 코어의 문서를 읽었습니다. V3는 독립 작업을 여러 워커에게 나눕니다.".into()
        };
        for width in [40, 80, 140] {
            let mut app = app_with([]);
            let body = VersionBackend::report(&outcome);
            app.apply_event(message("readable-assistant", &body));
            let screen = app.render(width, 50);
            let text = screen_text(&screen);
            report.check(
                S,
                format!("{status} {width}: 통계는 본문에서 숨김"),
                !text.contains("모델 요청") && !text.contains("기록:") && !body.contains("검증 0"),
                &text,
            );
            report.check(
                S,
                format!("{status} {width}: 사람에게 결과 설명"),
                match status {
                    "Unresolved" => {
                        body.contains("계획을 준비하다") && !text.contains("tool assertion")
                    }
                    "AwaitingApproval" => body.contains("승인하면"),
                    "Cancelled" => {
                        body.contains("작업을 중지했습니다") && !text.contains("cancelled")
                    }
                    _ => body == outcome.summary,
                },
                &text,
            );
            report.check(
                S,
                format!("{status} {width}: 별도 실행 상세 카드를 만들지 않음"),
                actions_at(&screen, "실행 상세").is_empty()
                    && !text.contains(".loom-v3")
                    && !text.contains("13180")
                    && !text.contains("5290"),
                &text,
            );
            report.dump(S, &screen);
            report.check(
                S,
                format!("{status} {width}: 진단 데이터는 결과 객체에 보존"),
                outcome.input == Some(13180)
                    && outcome.output == Some(5290)
                    && outcome.records.to_string_lossy().contains(".loom-v3")
                    && (status != "Unresolved"
                        || outcome.blocker.as_deref() == Some("tool assertion missing name")),
                &text,
            );
        }
    }
    outcome.status = "Unresolved".into();
    outcome.summary = "repeated identical operations on unchanged revision".into();
    outcome.blocker = Some(outcome.summary.clone());
    outcome.calls = 4;
    outcome.events.clear(); // Live V5 outcomes have domain events in activity_report.
    for width in [40, 80, 140] {
        let body = VersionBackend::report(&outcome);
        let mut app = app_with([message("repeated-tools", &body)]);
        let screen = app.render(width, 40);
        let text = screen_text(&screen);
        report.check(S, format!("{width}: 도구 반복 중단의 실제 원인 표시"),
            body.contains("같은 도구 요청") && body.contains("반복") && !body.contains("계획을 준비하다"), &body);
        report.check(S, format!("{width}: 반복 중단 사유가 화면에 보임"), text.contains("반복"), &text);
        report.dump("native_repeated_tools", &screen);
    }

    let mut conversation = crate::backend::version_conversation::Conversation::default();
    let mut read = Event::new("worker-1", EventKind::ToolSucceeded, "1: contents");
    read.tool = Some("read_file".into());
    read.path = Some("README.md".into());
    conversation.observe(&read);
    let body = conversation.assistant("progress").body;
    report.check(
        S,
        "파일 읽기를 문장으로 설명",
        body.contains("읽었습니다") && body.contains("README.md") && body.contains("read_file"),
        &body,
    );
}

fn native_model_recovery(report: &mut Report) {
    use crate::backend::activity_events::{Event, EventKind, ModelErrorKind, ModelIssue};
    use crate::backend::{
        version_conversation::Conversation, version_core::Outcome, versions::VersionBackend,
    };
    const S: &str = "native_model_recovery";
    let mut conversation = Conversation::default();
    let mut read = Event::new("worker", EventKind::ToolSucceeded, "source contents");
    read.tool = Some("read_file".into());
    read.path = Some("README.md".into());
    conversation.observe(&read);
    let mut events = vec![read];
    for attempt in 1..=2 {
        let event = ModelIssue::event(
            "worker",
            Some(ModelErrorKind::Transient),
            true,
            attempt,
            std::time::Duration::from_millis(500),
            "HTTP 503: UNAVAILABLE",
        );
        conversation.observe(&event);
        events.push(event);
    }
    let mut outcome:Outcome=serde_json::from_value(serde_json::json!({
        "version":"Loom","id":"recovery","goal":"Explain architectures","status":"Unresolved","summary":"Model call error: HTTP 503: UNAVAILABLE",
        "blocker":"HTTP 503: UNAVAILABLE","remaining":"Explain architectures","diff":"","files":[],"sources":["README.md"],
        "records":".loom-v3","elapsed_ms":1500,"calls":3,"input":null,"output":null,"cached":null,
        "attempts":1,"validation":[],"plan_mode":"single","plan_reason":"Inspect sources","groups":[],"model_calls":[],"events":[]
    })).unwrap();
    events.push(ModelIssue::event(
        "worker",
        Some(ModelErrorKind::Transient),
        false,
        3,
        std::time::Duration::ZERO,
        "HTTP 503: UNAVAILABLE",
    ));
    outcome.events = events;
    for width in [40, 80, 140] {
        let mut app = app_with([]);
        app.apply_event(AgentEvent::scoped(
            "main",
            AgentUpdate::EntryUpdated(conversation.assistant("recovery")),
        ));
        let log = conversation.log("recovery", false).unwrap();
        app.apply_event(AgentEvent::scoped(
            "main",
            AgentUpdate::EntryUpdated(log.clone()),
        ));
        let screen = app.render(width, 45);
        let text = screen_text(&screen);
        report.check(
            S,
            format!("{width}: 다시 연결할 때 직전 파일 보존"),
            text.contains("README.md") && text.chars().filter(|c| !c.is_whitespace()).collect::<String>().contains("다시연결"),
            &text,
        );
        report.check(
            S,
            format!("{width}: 재연결은 도구 실패와 구별"),
            log.title.contains("재연결 2") && log.title.contains("실패 0"),
            &text,
        );
        report.check(
            S,
            format!("{width}: 원시 통신 오류는 본문에 없음"),
            !text.contains("HTTP 503"),
            &text,
        );
        report.dump(S, &screen);
        let mut app = app_with([]);
        let body = VersionBackend::report(&outcome);
        app.apply_event(message("failure", &body));
        let screen = app.render(width, 45);
        let text = screen_text(&screen);
        report.check(
            S,
            format!("{width}: 마지막 실패는 모델 연결 문제로 설명"),
            body.contains("모델 연결") && !text.contains("HTTP 503"),
            &text,
        );
        report.dump("native_model_unavailable", &screen);
        let original = outcome
            .events
            .iter()
            .rev()
            .filter_map(ModelIssue::decode)
            .next();
        report.check(
            S,
            format!("{width}: 원시 오류는 내부 이벤트에만 보존"),
            original.is_some_and(|issue| issue.error.contains("HTTP 503"))
                && outcome.input.is_none()
                && outcome.output.is_none()
                && !text.contains("UNAVAILABLE"),
            &text,
        );
    }
}

fn escape_stops_active_turn(report: &mut Report) {
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    let esc = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
    {
        // On another screen Escape returns to the conversation; the running work goes on.
        let mut app = App::new(false);
        app.apply_event(loaded("main", "Escape"));
        app.turn = Some("running".into());
        app.busy = true;
        app.activate(Action::Command("/model".into()));
        let opened = app.view != View::Chat;
        let canvas = app.render(80, 24);
        let intent = app.key(esc, &canvas);
        report.check("escape", "다른 화면의 Esc는 대화 화면으로 돌아가고 작업은 계속",
            opened && intent != Some(Intent::Interrupt) && app.view == View::Chat && app.turn.is_some(),
            format!("{intent:?} {:?}", app.view));
        let canvas = app.render(80, 24);
        report.check("escape", "대화 화면에서 누르는 Esc는 작업 중단",
            app.key(esc, &canvas) == Some(Intent::Interrupt), "");
    }
    for obstruction in ["menu", "selection", "approval", "pending submission"] {
        let mut app = App::new(false);
        app.apply_event(loaded("main", "Escape"));
        if obstruction == "pending submission" {
            app.editor.insert("pending request");
            let intent = app.activate(Action::Send);
            report.check(
                "escape",
                "pending submit fixture",
                matches!(intent, Some(Intent::Submit(_))),
                format!("{intent:?}"),
            );
        } else {
            app.turn = Some("running".into());
            app.busy = true;
            app.editor.insert(if obstruction == "menu" {
                "/"
            } else {
                "keep draft"
            });
            if obstruction == "selection" {
                app.editor.select_all();
            } else if obstruction == "approval" {
                app.apply_event(AgentEvent::scoped(
                    "main",
                    AgentUpdate::ApprovalRequested(Approval {
                        id: "pending-approval".into(),
                        title: "Tool needs approval".into(),
                        summary: "Test approval".into(),
                        detail: String::new(),
                        choices: vec![Choice {
                            label: "Allow".into(),
                            result: "accept".into(),
                        }],
                        answering: false,
                        expanded: false,
                    }),
                ));
            }
        }
        let canvas = app.render(80, 24);
        let intent = app.key(esc, &canvas);
        report.check(
            "escape",
            format!("Esc stops with {obstruction} visible"),
            intent == Some(Intent::Interrupt),
            format!("{intent:?}"),
        );
        if obstruction == "pending submission" {
            app.apply_event(AgentEvent::scoped(
                "main",
                AgentUpdate::TurnStarted {
                    id: Some("assigned-later".into()),
                },
            ));
            report.check(
                "escape",
                "late start keeps the stopped display",
                app.status == "중단됨",
                format!("status={}", app.status),
            );
            let screen = app.render(80, 24);
            report.check(
                "escape",
                "Escape immediately replaces the working phrase",
                screen_text(&screen).contains("중단됨"),
                screen_text(&screen),
            );
        }
        report.dump("escape", &app.render(80, 24));
    }
}

fn response_model_details(report: &mut Report) {
    let mut app = App::new(false);
    app.apply_event(loaded("model-session", "Models"));
    app.apply_event(AgentEvent::global(AgentUpdate::ModelsLoaded(vec![
        "auto".into(),
        "gateway/route-model".into(),
    ])));
    app.apply_event(AgentEvent::scoped(
        "model-session",
        AgentUpdate::ModelResolved {
            requested: "gateway/route-model".into(),
            provider: "provider-from-response".into(),
            model: "actual-model".into(),
        },
    ));
    app.activate(Action::Command("/model".into()));
    for width in [40, 80, 140] {
        let screen = app.render(width, 35);
        let text = screen_text(&screen);
        report.check(
            "models",
            format!("{width}: 실제 응답 모델 표시 및 상세 접힘"),
            text.contains("actual-model") && !text.contains("provider-from-response"),
            &text,
        );
        report.dump("models", &screen);
    }
    app.activate(Action::ToggleModelDetails);
    let screen = app.render(80, 35);
    report.check(
        "models",
        "모델 상세 펼치면 요청과 실제 provider 구분",
        screen_text(&screen).contains("provider-from-response")
            && screen_text(&screen).contains("gateway/route-model"),
        screen_text(&screen),
    );
    report.dump("models", &screen);
}

fn rewind_choices(report: &mut Report) {
    let mut app = App::new(false);
    app.apply_event(loaded("rewind-session", "Rewind"));
    app.apply_event(AgentEvent::scoped(
        "rewind-session",
        AgentUpdate::TurnStarted {
            id: Some("first".into()),
        },
    ));
    app.apply_event(AgentEvent::scoped(
        "rewind-session",
        AgentUpdate::EntryUpdated(Entry {
            id: "answer".into(),
            kind: Kind::Assistant,
            title: String::new(),
            body: "완료된 답변".into(),
            status: "completed".into(),
            expanded: false,
        }),
    ));
    app.apply_event(AgentEvent::scoped(
        "rewind-session",
        AgentUpdate::TurnCompleted {
            id: Some("first".into()),
            status: "completed".into(),
            error: None,
        },
    ));
    app.view = crate::app::View::Rewind;
    app.activate(Action::Rewind("first".into()));
    for width in [40, 80, 140] {
        let screen = app.render(width, 30);
        let text = screen_text(&screen);
        report.check(
            "rewind",
            format!("{width}: 대화만 되돌리기 선택"),
            text.contains("대화만 되돌리기"),
            &text,
        );
        report.dump("rewind", &screen);
    }
    app.apply_event(AgentEvent::scoped(
        "rewind-session",
        AgentUpdate::RewindPreview {
            turn_id: "first".into(),
            files: 2,
            added: 12,
            removed: 3,
            blocked: None,
        },
    ));
    for width in [40, 80, 140] {
        let screen = app.render(width, 30);
        let text = screen_text(&screen);
        report.check(
            "rewind",
            format!("{width}: 코드 복원 변경량과 선택"),
            text.contains("+12 −3")
                && screen
                    .hits
                    .iter()
                    .any(|hit| hit.action == Action::ConfirmRewind),
            &text,
        );
        report.check(
            "rewind",
            format!("{width}: 추가 초록 삭제 빨강"),
            screen
                .cells
                .iter()
                .any(|cell| cell.tone == Tone::Success && cell.text == "+")
                && screen
                    .cells
                    .iter()
                    .any(|cell| cell.tone == Tone::Danger && cell.text == "−"),
            "missing semantic colors",
        );
        report.dump("rewind", &screen);
    }
    app.apply_event(AgentEvent::scoped(
        "rewind-session",
        AgentUpdate::RewindPreview {
            turn_id: "first".into(),
            files: 0,
            added: 0,
            removed: 0,
            blocked: Some("사용자 수정 파일을 보존합니다".into()),
        },
    ));
    let screen = app.render(80, 30);
    report.check(
        "rewind",
        "코드 복원이 막혀도 대화 복원 가능",
        screen
            .hits
            .iter()
            .any(|h| h.action == Action::ConfirmConversationRewind)
            && !screen
                .hits
                .iter()
                .any(|h| h.action == Action::ConfirmRewind),
        screen_text(&screen),
    );
    report.dump("rewind", &screen);
}

#[cfg(test)]
mod tests {
    #[test]
    fn self_check_runs_every_scenario() {
        let report = super::run();
        assert!(report.checks.len() > 30);
        for scenario in [
            "markdown",
            "streaming",
            "copy",
            "activity",
            "quota",
            "layout",
            "ui_hierarchy",
            "accent_settings",
        ] {
            assert!(
                report.checks.iter().any(|c| c.scenario == scenario),
                "{scenario}"
            );
        }
    }
}

fn version_approval_restore(report: &mut Report) {
    const S: &str = "version_approval_restore";
    let scenario = || -> anyhow::Result<Vec<AgentEvent>> {
        use loom_core::models::{Part, Response, ScriptedModel, Stop};
        let root = tempfile::tempdir()?;
        anyhow::ensure!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .arg(root.path())
                .status()?
                .success(),
            "git init failed"
        );
        std::fs::write(
            root.path().join("service.py"),
            "def status(): return 'down'\n",
        )?;
        let model = ScriptedModel::new(vec![Response {
            model: "scripted".into(),
            usage: Default::default(),
            stop: Stop::ToolUse,
            parts: vec![
                Part::ToolCall {
                    id: "read".into(),
                    name: "open".into(),
                    arguments: serde_json::json!({"target":"service.py"}),
                },
                Part::ToolCall {
                    id: "edit".into(),
                    name: "change".into(),
                    arguments: serde_json::json!({"op":"edit","target":"service.py","old":"'down'","new":"'up'"}),
                },
                Part::ToolCall {
                    id: "finish".into(),
                    name: "finish".into(),
                    arguments: serde_json::json!({"summary":"status corrected"}),
                },
            ],
            latency: std::time::Duration::ZERO,
            route: None,
            gateway_connection: None,
            gateway_upstream_ms: None,
        }]);
        loom_core::agent::Core::open(root.path())?.execute(
            "fix service.py",
            loom_core::agent::Intent::Edit,
            loom_core::agent::Options {
                checks: vec![if cfg!(windows) {
                    "findstr /c:up service.py >nul"
                } else {
                    "grep -q up service.py"
                }
                .into()],
                validation_reserve: 0,
                ..Default::default()
            },
            &model,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            &|_| {},
        )?;
        let mut backend = crate::backend::versions::VersionBackend::with_model(
            crate::backend::version_core::Version::Loom,
            root.path(),
            std::sync::Arc::new(ScriptedModel::new([])),
            crate::backend::version_core::Settings::default(),
        )?;
        let mut events = vec![];
        while let Some(event) = backend.poll()? {
            events.push(event);
        }
        Ok(events)
    };
    match scenario() {
        Ok(events) => {
            report.check(
                S,
                "V5 재시작 시 검증된 제안에 실제 승인 버튼 복원",
                events
                    .iter()
                    .any(|e| matches!(&e.update, AgentUpdate::ApprovalRequested(_))),
                "",
            );
            let mut app = app_with(events);
            let screen = app.render(100, 32);
            let text = screen_text(&screen);
            report.check(
                S,
                "V5 연결과 승인 선택지가 실제 화면에 보임",
                text.contains("승인하고 적용") && text.contains("거절하고 보존"),
                text.clone(),
            );
            report.dump(S, &screen);
        }
        Err(error) => report.check(S, "V5 승인 복원 시나리오", false, error.to_string()),
    }
}

fn v5_visible_progress(report: &mut Report) {
    use crate::backend::v5_projection::Projection;
    use loom_core::agent::events::{Event, Stage};
    use serde_json::json;
    const S: &str = "v5_visible_progress";
    let mut p = Projection::default();
    p.observe(&Event::new("main", Stage::Direct, "안녕", json!({})));
    report.check(
        S,
        "내부 역할 대신 실제 준비 상태 표시",
        p.summary().contains("요청 확인 중") && !p.summary().contains("직접 작업"),
        p.summary(),
    );
    p.observe(&Event::new(
        "main",
        Stage::Run,
        "model",
        json!({"request":"PRIVATE_PROMPT","model":"local"}),
    ));
    report.check(
        S,
        "모델 대기를 본문에 표시하고 요청 원문 숨김",
        p.summary().contains("모델 응답")
            && !format!("{:?}", p.entries("progress", false)).contains("PRIVATE_PROMPT"),
        p.summary(),
    );
    for width in [48, 80, 120] {
        let mut app = App::new(false);
        app.apply_event(loaded("v5", "Loom"));
        app.apply_event(AgentEvent::scoped(
            "v5",
            AgentUpdate::TurnStarted {
                id: Some("progress".into()),
            },
        ));
        for entry in p.entries("progress", false) {
            app.apply_event(AgentEvent::scoped("v5", AgentUpdate::EntryUpdated(entry)));
        }
        let before = app.render(width, 28);
        let row = (0..before.height).find(|&y| before.plain_line(y).contains("모델 응답"));
        app.tick(std::time::Instant::now() + std::time::Duration::from_millis(240));
        let after = app.render(width, 28);
        let animated = row.is_some_and(|y| {
            let start = usize::from(y) * usize::from(width);
            before.cells[start..start + usize::from(width)]
                != after.cells[start..start + usize::from(width)]
                && after
                    .plain_line(y)
                    .chars()
                    .any(|c| "⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏".contains(c))
        });
        report.check(
            S,
            format!("{width}: 본문 진행 문구도 움직임"),
            animated,
            screen_text(&after),
        );
        report.dump(S, &before);
        report.dump(S, &after);
        app.apply_event(AgentEvent::scoped(
            "v5",
            AgentUpdate::TurnCompleted {
                id: Some("progress".into()),
                status: "failed".into(),
                error: None,
            },
        ));
        let stopped = app.render(width, 28);
        app.tick(std::time::Instant::now() + std::time::Duration::from_secs(2));
        let settled = app.render(width, 28);
        report.check(
            S,
            format!("{width}: 실행 종료 후 움직임 멈춤"),
            stopped.cells == settled.cells,
            screen_text(&settled),
        );
    }
}

fn v5_model_failure(report: &mut Report) {
    use crate::backend::{version_core::Outcome, versions::VersionBackend};
    const S: &str = "v5_model_failure";
    let mut outcome: Outcome=serde_json::from_value(serde_json::json!({
        "version":"Loom","id":"timeout","goal":"안녕","status":"Unresolved",
        "summary":"Transient: unreadable stream: timed out reading response", "blocker":null,
        "remaining":"안녕","diff":"","files":[],"records":".loom/v5/tasks/timeout.json",
        "elapsed_ms":93802,"calls":1,"input":null,"output":null,"cached":null,
        "attempts":1,"validation":[],"plan_mode":"single","plan_reason":"","groups":[],"model_calls":[]
    })).unwrap();
    for (failure, label) in [
        ("TimedOut: execution deadline reached", "시간 초과"),
        (
            "Transient: unreadable stream: timed out reading response",
            "시간 초과",
        ),
        ("Transient: connection reset", "연결 오류"),
        ("Auth: invalid token", "인증"),
        ("RateLimited: requests exceeded", "요청 제한"),
    ] {
        outcome.summary = failure.into();
        let body = VersionBackend::report(&outcome);
        report.check(
            S,
            format!("{label}: 숨긴 상세 없이도 모델 실패 원인을 설명"),
            (body.contains(label) || (label == "시간 초과" && body.contains("초과")))
                && !body.contains(failure),
            &body,
        );
        let mut app = app_with([]);
        app.apply_event(message("timeout-assistant", &body));
        let screen = app.render(80, 28);
        report.check(
            S,
            format!("{label}: 원문은 UI에 노출하지 않고 결과에 보존"),
            !screen_text(&screen).contains(failure) && outcome.summary == failure,
            screen_text(&screen),
        );
        report.dump(S, &screen);
    }
}

fn v5_plan_command(report: &mut Report) {
    const S: &str = "v5_plan_command";
    let mut app = App::new(false);
    app.apply_event(loaded("v5", "Loom"));
    app.editor.insert("/plan 모델 계층 정리");
    let intent = app.activate(Action::Send);
    report.check(
        S,
        "Plan 목표가 일반 전송 경로로 전달됨",
        matches!(intent, Some(Intent::Submit(ref text)) if text == "/plan 모델 계층 정리"),
        format!("{intent:?}"),
    );
    report.check(
        S,
        "전송 후 입력이 비워짐",
        app.editor.text.is_empty(),
        &app.editor.text,
    );
    for width in [48, 80, 120] {
        let canvas = app.render(width, 24);
        report.dump(S, &canvas);
    }
}

fn v5_disclosure(report: &mut Report) {
    use crate::backend::v5_projection::Projection;
    use loom_core::agent::events::{Event, Stage};
    use serde_json::json;
    const S: &str = "v5_disclosure";
    let mut p = Projection::default();
    p.observe(&Event::new(
        "main",
        Stage::Orchestrating,
        "DAG",
        json!({"units":[{"id":"a","goal":"모델 계층 정리","depends_on":[]}]}),
    ));
    p.observe(&Event::new(
        "a",
        Stage::AgentStarted,
        "모델 계층 정리",
        json!({}),
    ));
    p.observe(&Event::new(
        "a",
        Stage::Source,
        "src/model.rs",
        json!({"completed":true,"source":"RAW_SOURCE_DETAIL"}),
    ));
    let mut app = App::new(false);
    app.apply_event(loaded("v5", "Loom"));
    // Progress text lives in the status line while the turn runs; the transcript keeps a spinner.
    app.apply_event(AgentEvent::scoped("v5", AgentUpdate::StatusUpdated { status: "작업 중".into(), busy: true }));
    for entry in p.entries("v5-turn", false) {
        app.apply_event(AgentEvent::scoped("v5", AgentUpdate::EntryUpdated(entry)));
    }
    for width in [48, 80, 120] {
        let canvas = app.render(width, 28);
        let text = screen_text(&canvas);
        report.check(
            S,
            "기본 화면은 현재 동작과 진행 수치만 표시",
            text.contains("0/1 완료")
                && text.contains("코드 확인")
                && !text.contains("오케스트레이션")
                && !text.contains("에이전트 모드")
                && !text.contains("RAW_SOURCE_DETAIL"),
            &text,
        );
        report.dump(S, &canvas);
    }
    let entries = p.entries("v5-turn", false);
    report.check(
        S,
        "도구 사용은 대화의 독립 항목으로 남기고 내부 payload는 숨김",
        entries.len() == 2
            && entries.iter().any(|e| {
                e.kind == Kind::Tool
                    && !e.expanded
                    && e.title.contains("src/model.rs")
                    && !e.body.contains("RAW_SOURCE_DETAIL")
                    && !e.body.contains("\"source\"")
            }),
        format!("{entries:?}"),
    );
    let mut failure = Event::new(
        "a",
        Stage::AgentFinished,
        "모델 계층 정리",
        json!({"reason":"scope conflict"}),
    );
    failure.failed = true;
    p.observe(&failure);
    report.check(
        S,
        "실패한 에이전트를 완료 수에 넣지 않음",
        p.summary().contains("0/1 완료") && p.summary().contains("확인 필요"),
        p.summary(),
    );
}

fn v5_parallel_question(report: &mut Report) {
    const S: &str = "v5_parallel_question";
    let mut app = App::new(false);
    app.apply_event(loaded("v5", "Loom"));
    app.apply_event(AgentEvent::scoped(
        "v5",
        AgentUpdate::TurnStarted {
            id: Some("main-turn".into()),
        },
    ));
    app.editor.insert("/ask 왜 이 방식으로 구현하고 있어?");
    let result = app.activate(Action::Send);
    report.check(
        S,
        "작업 중 질문은 대기열에 넣지 않고 별도 조회로 전달",
        format!("{result:?}").contains("Inquiry"),
        format!("{result:?}"),
    );
    report.check(
        S,
        "본 작업과 중단 상태를 보존",
        app.busy && app.queued.is_empty(),
        format!("busy={} queued={}", app.busy, app.queued.len()),
    );
    report.check(
        S,
        "질문을 보낸 뒤 입력창을 비움",
        app.editor.text.is_empty(),
        &app.editor.text,
    );
}

fn v5_command_lifecycle(report: &mut Report) {
    use crate::backend::v5_projection::Projection;
    use loom_core::agent::events::{Event, Stage};
    use serde_json::json;
    const S: &str = "v5_command_lifecycle";
    let mut p = Projection::default();
    p.observe(&Event::new(
        "main",
        Stage::Run,
        "cargo test",
        json!({"tool":"run","args":{"kind":"command","command":"cargo test"},"started":true}),
    ));
    report.check(
        S,
        "툴 반환 전에도 명령 실행 중 표시",
        p.summary().contains("실행 중")
            && p.entries("run", false)
                .iter()
                .any(|e| e.kind == Kind::Tool && e.status == "inProgress"),
        p.summary(),
    );
    for width in [48, 80, 120] {
        let mut app = App::new(false);
        app.apply_event(loaded("v5", "Loom"));
        app.apply_event(AgentEvent::scoped(
            "v5",
            AgentUpdate::TurnStarted {
                id: Some("run".into()),
            },
        ));
        for e in p.entries("run", false) {
            app.apply_event(AgentEvent::scoped("v5", AgentUpdate::EntryUpdated(e)));
        }
        let screen = app.render(width, 24);
        let lines: Vec<_> = (0..screen.height)
            .map(|y| screen.plain_line(y))
            .filter(|line| line.contains("cargo test"))
            .collect();
        report.check(
            S,
            format!("{width}: 실행 중 명령은 한 줄에 한 번만 표시"),
            lines.len() == 1 && lines[0].matches("실행 중").count() == 1,
            screen_text(&screen),
        );
        report.dump(S, &screen);
    }
    p.observe(&Event::new(
        "main",
        Stage::Run,
        "cargo test",
        json!({"tool":"run","completed":true,"result":[{"status":"Passed"}]}),
    ));
    report.check(
        S,
        "명령 종료를 완료 상태로 구분",
        p.summary().contains("완료")
            && p.entries("run", false)
                .iter()
                .any(|e| e.kind == Kind::Tool && e.status == "completed"),
        p.summary(),
    );
    let entries = p.entries("run", false);
    report.check(
        S,
        "명령 완료도 같은 한 줄에서 표시",
        entries
            .iter()
            .filter(|e| e.kind == Kind::Assistant)
            .all(|e| e.body.is_empty())
            && entries.iter().any(|e| {
                e.kind == Kind::Tool && e.title == "run · cargo test" && e.status == "completed"
            }),
        format!("{entries:?}"),
    );
    for width in [48, 80, 120] {
        let mut app = App::new(false);
        app.apply_event(loaded("v5", "Loom"));
        for e in p.entries("run", false) {
            app.apply_event(AgentEvent::scoped("v5", AgentUpdate::EntryUpdated(e)));
        }
        report.dump(S, &app.render(width, 24));
    }
    let mut failed = Event::new(
        "main",
        Stage::Run,
        "cargo test",
        json!({"tool":"run","completed":true,"result":[{"status":"Failed"}]}),
    );
    failed.failed = true;
    p.observe(&failed);
    report.check(
        S,
        "실패한 명령 기록은 성공 체크로 표시하지 않음",
        p.entries("run", false)
            .iter()
            .any(|e| e.kind == Kind::Tool && e.status == "failed"),
        p.summary(),
    );
}

fn tool_timeline(report: &mut Report) {
    const S: &str = "tool_timeline";
    let mut app = App::new(false);
    app.thread = Some("trace-session".into());
    app.apply_event(AgentEvent::scoped(
        "trace-session",
        AgentUpdate::TurnStarted {
            id: Some("trace".into()),
        },
    ));
    app.apply_event(AgentEvent::scoped(
        "trace-session",
        AgentUpdate::EntryUpdated(Entry {
            id: "trace-assistant".into(),
            kind: Kind::Assistant,
            title: crate::agent::PROGRESS_TITLE.into(),
            body: "모델 응답 기다리는 중 · 재시도 1".into(),
            status: "inProgress".into(),
            expanded: false,
        }),
    ));
    app.apply_event(AgentEvent::scoped(
        "trace-session",
        AgentUpdate::EntryUpdated(Entry {
            id: "trace-tool-git".into(),
            kind: Kind::Tool,
            title: "run · git status".into(),
            body: "결과 · 변경 없음".into(),
            status: "completed".into(),
            expanded: false,
        }),
    ));
    app.apply_event(AgentEvent::scoped(
        "trace-session",
        AgentUpdate::EntryUpdated(Entry {
            id: "trace-tool-open".into(),
            kind: Kind::Tool,
            title: "open · custom-tui/src/agent.rs".into(),
            body: "대상 · custom-tui/src/agent.rs".into(),
            status: "inProgress".into(),
            expanded: false,
        }),
    ));
    for width in [48, 80, 140] {
        let screen = app.render(width, 36);
        let text = screen_text(&screen);
        report.check(
            S,
            format!("{width}: 모델 진행과 도구 기록을 분리"),
            // The status line carries the model's progress; narrow screens may cut its tail.
            (text.contains("모델 응답 기다리는 중 · 재시도 1")
                || (width == 48 && text.contains("모델 응답 기다리는 중")))
                && text.matches("모델 응답 기다리는 중").count() == 1
                && text.contains("git status")
                && (text.contains("custom-tui/src/agent.rs")
                    || (width == 48 && text.contains("custom-tui/src/")))
                && !text.contains("진행 기록")
                && !text.contains("AGENTS.md")
                && !text.contains("Spark-X2.5"),
            &text,
        );
        report.check(
            S,
            format!("{width}: 각 도구 항목을 독립적으로 펼칠 수 있음"),
            screen
                .hits
                .iter()
                .any(|hit| hit.action == Action::Toggle("trace-tool-git".into()))
                && screen
                    .hits
                    .iter()
                    .any(|hit| hit.action == Action::Toggle("trace-tool-open".into())),
            &text,
        );
    }
    app.activate(Action::Command("/tool 1".into()));
    let screen = app.render(80, 36);
    let text = screen_text(&screen);
    report.check(
        S,
        "/tool은 최근 도구 기록만 펼침",
        text.contains("대상 · custom-tui/src/agent.rs")
            && !text.contains("AGENTS.md")
            && !text.contains("Spark-X2.5")
            && app.busy,
        text,
    );
    report.dump(S, &screen);
    let mut projection = crate::backend::v5_projection::Projection::default();
    for completed in [false, true] {
        projection.observe(&loom_core::agent::events::Event::new(
            "find_v4_v5_diff", loom_core::agent::events::Stage::Navigation, "\"v4 Core\"",
            serde_json::json!({"tool":"find","args":{"kind":"text","query":"\"v4 Core\"","scope":"custom-tui"},
                "started":!completed,"completed":completed}),
        ));
        for entry in projection.entries("compact", false) {
            app.apply_event(AgentEvent::scoped("trace-session", AgentUpdate::EntryUpdated(entry)));
        }
        for width in [48, 80, 140] {
            let screen = app.render(width, 36);
            let text = screen_text(&screen);
            report.check(S, format!("{width}: 도구와 입력이 먼저 보임 · completed={completed}"),
                text.contains("find · \"v4 Core\"") && !text.contains("find_v4_v5_diff")
                    && !text.contains("✓ 모델"), &text);
            report.dump(S, &screen);
        }
    }
}

#[cfg(test)]
mod provider_checks {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent};
    #[test]
    fn command_help_and_usage_errors_do_not_expose_retired_core_versions() {
        for command in ["/ask", "/plan"] {
            let mut app = App::new(false);
            app.editor.insert(command);
            let canvas = app.render(80, 40);
            assert!(!screen_text(&canvas).contains("V5:"));
            app.activate(Action::Command(command.into()));
            assert!(!app.notice.contains("V5"));
        }
    }
    #[test]
    fn escape_stops_the_display_before_the_backend_acknowledges() {
        let mut app = App::new(false);
        app.thread = Some("main".into());
        app.turn = Some("running".into());
        app.busy = true;
        app.editor.insert("keep draft");
        app.apply_event(AgentEvent::scoped("main", AgentUpdate::EntryUpdated(Entry {
            id: "progress".into(), kind: Kind::Assistant, title: PROGRESS_TITLE.into(),
            body: "partial output".into(), status: "inProgress".into(), expanded: false,
        })));
        let canvas = app.render(80, 24);
        assert_eq!(app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &canvas), Some(Intent::Interrupt));
        assert_eq!(app.status, "중단됨");
        app.apply_event(AgentEvent::scoped("main", AgentUpdate::EntryDelta { id: "progress".into(), kind: Kind::Assistant, text: " late output".into() }));
        app.apply_event(AgentEvent::scoped("main", AgentUpdate::StatusUpdated { status: "working again".into(), busy: true }));
        assert_eq!(app.entries.iter().find(|e| e.id == "progress").unwrap().body, "partial output");
        assert_eq!(app.status, "중단됨");
        assert_eq!(app.editor.text, "keep draft");
        assert!(!screen_text(&app.render(80, 24)).contains("중단 요청 중"));
    }
    #[test]
    fn effort_command_reveals_its_controls_below_the_model_catalog() {
        for width in [40, 80, 140] {
            let mut app = App::new(false);
            app.model = "fixture".into();
            app.models = (0..420).map(|i| format!("work/model-{i:03}")).collect();
            app.model_profiles = vec![ModelProfile {
                id: "fixture".into(),
                default_effort: Some("low".into()),
                efforts: ["low", "high"].into_iter().map(|value| EffortOption {
                    value: value.into(), description: String::new(),
                }).collect(),
            }];
            app.editor.insert("/effort high");
            app.activate(Action::Send);
            let canvas = app.render(width, 24);
            assert!(screen_text(&canvas).contains("● effort high"), "{}", screen_text(&canvas));
            assert!(canvas.hits.iter().any(|h| h.action == Action::Effort("high".into())));
            app.activate(Action::Command("/model".into()));
            assert!(screen_text(&app.render(width, 24)).contains("┌─ 검색"));
        }
    }
    #[test]
    fn model_selection_reveals_effort_controls_in_a_small_window() {
        let mut app = App::new(false);
        app.models = vec!["first".into(), "second".into()];
        app.model = "first".into();
        app.model_profiles = vec![ModelProfile {
            id: "second".into(), default_effort: None,
            efforts: vec![EffortOption { value: "high".into(), description: String::new() }],
        }];
        app.activate(Action::Command("/model".into()));
        app.activate(Action::Model("second".into()));
        let canvas = app.render(80, 24);
        assert!(canvas.hits.iter().any(|h| h.action == Action::Effort("high".into())), "{}", screen_text(&canvas));
    }
    #[test]
    fn model_search_is_a_visible_editable_field_with_a_cursor() {
        let mut app = App::new(false);
        app.models = vec!["auto".into(), "work/model-419".into()];
        app.activate(Action::Command("/model".into()));
        for width in [48, 80, 140] {
            let canvas = app.render(width, 40);
            let text = screen_text(&canvas);
            assert!(text.contains("┌─ 검색"), "{text}");
            let hit = canvas.hits.iter().find(|h| h.action == Action::ModelSearch).unwrap();
            let (x, y) = canvas.cursor.expect("search field needs its own cursor");
            assert!(x >= hit.x && x < hit.x + hit.width && y == hit.y);
            assert!(text.contains("검색 결과"));
        }
    }
    #[test]
    fn up_in_model_picker_does_not_recall_chat_history_or_steal_focus() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new(false);
        app.models = vec!["auto".into(), "work/model".into()];
        app.editor.insert("previous chat command");
        app.activate(Action::Send);
        app.editor.insert("unsent draft");
        app.activate(Action::Command("/model".into()));
        let canvas = app.render(80, 40);
        app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &canvas);
        assert_eq!(app.view, View::Models);
        assert!(!app.composer_active, "model picker must retain keyboard focus");
        assert!(app.focus.is_some(), "Up should navigate inside model picker");
        assert_eq!(app.editor.text, "unsent draft");
    }
    #[test]
    fn a_saved_conversation_is_deleted_only_after_a_confirming_second_key() {
        use crate::app::SessionSummary;
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let mut app = App::new(false);
        app.set_thread(&serde_json::json!({"id":"current","preview":"Now"}));
        app.sessions = vec![
            SessionSummary::from_thread(&serde_json::json!({"id":"current","preview":"Now"})).unwrap(),
            SessionSummary::from_thread(&serde_json::json!({"id":"old","preview":"Old talk","updatedAt":1})).unwrap(),
        ];
        app.sessions_loaded = true;
        app.activate(Action::Sessions);
        let screen = screen_text(&app.render(80, 30));
        assert!(screen.contains("Delete"), "the list must say how to delete: {screen}");
        app.focus = Some(Action::Resume("old".into()));
        let canvas = app.render(80, 30);
        let delete = KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(app.key(delete, &canvas), None, "one key must not delete");
        let armed = screen_text(&app.render(80, 30));
        assert!(armed.contains("Old talk") && armed.contains("한 번 더"), "{armed}");
        // Moving away disarms; the next Delete only asks again.
        app.focus = Some(Action::NewSession);
        let canvas = app.render(80, 30);
        app.key(delete, &canvas);
        app.focus = Some(Action::Resume("old".into()));
        let canvas = app.render(80, 30);
        assert_eq!(app.key(delete, &canvas), None);
        let canvas = app.render(80, 30);
        assert_eq!(app.key(delete, &canvas), Some(Intent::DeleteSession("old".into())));
        // The command form names the same numbered rows as the list.
        assert_eq!(app.activate(Action::Command("/sessions delete 1".into())), Some(Intent::DeleteSession("old".into())));
        assert!(app.activate(Action::Command("/sessions delete 9".into())).is_none());
    }
    #[test]
    fn network_access_can_be_allowed_from_the_permissions_screen() {
        use crate::agent::{AgentUpdate, Permissions};
        let mut app = App::new(false);
        app.apply_event(AgentEvent::global(AgentUpdate::PermissionsUpdated {
            permissions: Permissions { approval: Some("on-request".into()), sandbox: Some("workspaceWrite".into()), network: Some("deny".into()) },
            defaults_only: false,
        }));
        app.activate(Action::Command("/permissions".into()));
        let rows = app.content_rows_for(&app.view.clone(), 80);
        let allow = rows.iter().find(|row| row.2 == Some(Action::Permission("network".into(), "allow".into())));
        assert!(allow.is_some_and(|row| row.0.starts_with('○')), "{rows:?}");
        assert!(rows.iter().any(|row| row.0.starts_with('●') && row.0.contains("차단")
            && row.2 == Some(Action::Permission("network".into(), "deny".into()))), "{rows:?}");
        assert!(!rows.iter().any(|row| row.0.contains("네트워크 차단") && row.0.contains("작업 폴더")),
            "the sandbox label must not claim the network state: {rows:?}");
        app.apply_event(AgentEvent::global(AgentUpdate::PermissionsUpdated {
            permissions: Permissions { approval: None, sandbox: None, network: Some("allow".into()) },
            defaults_only: false,
        }));
        let rows = app.content_rows_for(&app.view.clone(), 80);
        assert!(rows.iter().any(|row| row.0.starts_with('●') && row.0.contains("허용")), "{rows:?}");
        // Clicking the row asks the backend to change it.
        app.thread = Some("session".into());
        assert_eq!(
            app.activate(Action::Permission("network".into(), "deny".into())),
            Some(Intent::UpdatePermission("network".into(), "deny".into()))
        );
    }
    #[test]
    fn local_model_speed_is_shown_next_to_the_model() {
        use crate::agent::AgentUpdate;
        let mut app = App::new(false);
        let local = "mlx/mlx-community/Qwen3-Coder-30B-A3B-Instruct-3bit";
        app.models = vec!["auto".into(), local.into(), "chatgpt/g".into()];
        app.model = local.into();
        app.apply_event(AgentEvent::global(AgentUpdate::ModelSpeed { tokens_per_second: 48.4 }));
        for width in [80, 120] {
            let status = screen_text(&app.render(width, 30));
            assert!(status.contains("48 tok/s"), "{width}: {status}");
        }
        // Another model's speed is not this model's.
        app.activate(Action::Model("chatgpt/g".into()));
        assert!(!screen_text(&app.render(120, 30)).contains("tok/s"));
    }
    #[test]
    fn live_progress_and_streamed_text_stay_below_the_tool_records() {
        use crate::agent::{AgentUpdate, Entry, Kind};
        let mut app = App::new(false);
        let entry = |id: &str, kind: Kind, title: &str, body: &str, status: &str| Entry {
            id: id.into(), kind, title: title.into(), body: body.into(), status: status.into(), expanded: false,
        };
        app.apply_event(AgentEvent::global(AgentUpdate::TurnStarted { id: Some("t".into()) }));
        for update in [
            entry("t-user", Kind::User, "", "질문", "completed"),
            entry("t-assistant", Kind::Assistant, crate::agent::PROGRESS_TITLE, "모델 응답 기다리는 중", "inProgress"),
            entry("t-tool-0", Kind::Tool, "read src/lib.rs", "", "completed"),
            entry("t-assistant", Kind::Assistant, crate::agent::PROGRESS_TITLE, "스트리밍 중인 답", "inProgress"),
        ] {
            app.apply_event(AgentEvent::global(AgentUpdate::EntryUpdated(update)));
        }
        let order: Vec<_> = app.entries.iter().map(|entry| entry.id.as_str()).collect();
        assert_eq!(order, ["t-user", "t-tool-0", "t-assistant"], "live text must stack below tools");
        app.apply_event(AgentEvent::global(AgentUpdate::EntryUpdated(entry("t-assistant", Kind::Assistant, "Native", "최종 답", "completed"))));
        assert_eq!(app.entries.last().map(|entry| entry.body.as_str()), Some("최종 답"));
    }
    #[test]
    fn streamed_text_is_not_repeated_in_the_status_line() {
        use crate::agent::{AgentUpdate, Entry, Kind, WorkPhase};
        let mut app = App::new(false);
        let entry = |id: &str, kind: Kind, title: &str, body: &str, status: &str| Entry {
            id: id.into(), kind, title: title.into(), body: body.into(), status: status.into(), expanded: false,
        };
        app.apply_event(AgentEvent::global(AgentUpdate::TurnStarted { id: Some("t".into()) }));
        app.apply_event(AgentEvent::global(AgentUpdate::EntryUpdated(entry("t-user", Kind::User, "", "질문", "completed"))));
        // A phase status reaches the status line once; the transcript shows only a spinner.
        app.apply_event(AgentEvent::global(AgentUpdate::EntryUpdated(entry(
            "t-assistant", Kind::Assistant, crate::agent::PROGRESS_TITLE, "코드 인덱스 확인 중", "inProgress"))));
        let screen = screen_text(&app.render(100, 24));
        assert_eq!(screen.matches("코드 인덱스 확인 중").count(), 1, "{screen}");
        // Streamed text is a plain reply entry and is never copied into the status line.
        app.apply_event(AgentEvent::global(AgentUpdate::EntryUpdated(entry(
            "t-assistant", Kind::Assistant, "", "첫 줄입니다.\n둘째 줄은 한 번만 보여야 합니다.", "inProgress"))));
        app.apply_event(AgentEvent::global(AgentUpdate::WorkPhaseUpdated(WorkPhase::Writing)));
        let screen = screen_text(&app.render(100, 24));
        assert_eq!(screen.matches("둘째 줄은 한 번만").count(), 1, "{screen}");
        // Streaming text looks like the final reply: the reply marker, no spinner.
        app.apply_event(AgentEvent::global(AgentUpdate::EntryUpdated(entry(
            "t-assistant", Kind::Assistant, "", "스트리밍 본문", "inProgress"))));
        let screen = screen_text(&app.render(100, 24));
        let line = screen.lines().find(|line| line.contains("스트리밍 본문")).unwrap_or_default();
        assert!(line.trim_start().starts_with("● 스트리밍 본문"), "{screen}");
    }
    #[test]
    fn command_menu_tab_completes_and_enter_runs() {
        use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
        let press = |typed: &str, code| {
            let mut app = App::new(false);
            app.thread = Some("session".into());
            app.editor.insert(typed);
            let canvas = app.render(100, 30);
            let intent = app.key(KeyEvent::new(code, KeyModifiers::NONE), &canvas);
            (intent, app.editor.text.clone())
        };
        assert_eq!(press("/compact", KeyCode::Tab), (None, "/compact".into()), "Tab completes without running");
        assert_eq!(press("/compact", KeyCode::Enter).0, Some(Intent::Compact), "Enter runs");
        assert_eq!(press("/plan", KeyCode::Enter), (None, "/plan ".into()), "a command that needs a goal waits for it");
    }
    #[test]
    fn rewind_picker_lists_requests_oldest_first_without_internal_ids() {
        let mut app = App::new(false);
        app.set_thread(&serde_json::json!({"id":"session","turns":[
            {"id":"turn-a","status":"completed","items":[
                {"id":"ua","type":"userMessage","content":[{"text":"첫 번째 요청"}]},
                {"id":"aa","type":"agentMessage","text":"첫 번째 답"}]},
            {"id":"turn-b","status":"completed","items":[
                {"id":"ub","type":"userMessage","content":[{"text":"두 번째 요청"}]},
                {"id":"ab","type":"agentMessage","text":"두 번째 답"}]}
        ]}));
        let rows = app.content_rows_for(&View::Rewind, 80);
        let text: Vec<_> = rows.iter().map(|row| row.0.as_str()).collect();
        let first = text.iter().position(|row| row.contains("첫 번째 요청")).expect("request shown");
        let second = text.iter().position(|row| row.contains("두 번째 요청")).expect("request shown");
        assert!(first < second, "oldest first: {text:?}");
        assert!(rows.iter().any(|row| row.0.contains("첫 번째 답")), "the reply helps recognize the point");
        assert!(rows[first].2 == Some(Action::Rewind("turn-a".into())));
        assert!(!text.iter().any(|row| row.contains("turn-a")), "no internal ids: {text:?}");
        app.activate(Action::Rewind("turn-a".into()));
        let confirm: Vec<_> = app.content_rows_for(&View::Rewind, 80).into_iter().map(|row| row.0).collect();
        assert!(confirm.iter().any(|row| row.contains("첫 번째 요청")), "{confirm:?}");
        assert!(!confirm.iter().any(|row| row.contains("turn-a")), "{confirm:?}");
    }
    #[test]
    fn finished_tools_are_a_compact_list_without_repeating_done() {
        use crate::agent::{AgentUpdate, Entry, Kind};
        let mut app = App::new(false);
        let tool = |id: &str, title: &str, status: &str| Entry {
            id: id.into(), kind: Kind::Tool, title: title.into(), body: String::new(), status: status.into(), expanded: false,
        };
        app.apply_event(AgentEvent::global(AgentUpdate::TurnStarted { id: Some("t".into()) }));
        for entry in [tool("t-1", "read · a.rs", "completed"), tool("t-2", "read · b.rs", "completed"), tool("t-3", "run · cargo test", "failed")] {
            app.apply_event(AgentEvent::global(AgentUpdate::EntryUpdated(entry)));
        }
        let screen = screen_text(&app.render(80, 24));
        let lines: Vec<_> = screen.lines().collect();
        let a = lines.iter().position(|l| l.contains("read · a.rs")).unwrap();
        assert!(lines[a + 1].contains("read · b.rs"), "consecutive tools have no blank line between: {screen}");
        assert!(!lines[a].contains("완료"), "✓ already says done: {}", lines[a]);
        assert!(screen.contains("cargo test · 실패"), "other states keep their word: {screen}");
    }
    #[test]
    fn completed_reply_uses_the_width_without_a_branch_gutter() {
        let mut app = App::new(false);
        let body = "x".repeat(70);
        app.set_thread(&serde_json::json!({"id":"source","turns":[{"id":"one","status":"completed","items":[{"id":"reply","type":"agentMessage","text":body}]}]}));
        let canvas = app.render(80, 24);
        assert!(canvas.hits.iter().all(|h| !matches!(h.action, Action::Branch(_))));
        assert!(screen_text(&canvas).contains(&body), "reply should fit on one line");
        assert!(matches!(app.activate(Action::Command("/branch 1".into())), Some(Intent::Fork(_))));
    }
    #[test]
    fn routing_and_checkpoint_management_are_reachable_from_commands() {
        let mut app = App::new(false);
        app.activate(Action::Command("/routing".into()));
        assert!(screen_text(&app.render(80, 48)).contains("자동 라우팅 설정"));
        app.activate(Action::Command("/runtime".into()));
        assert!(screen_text(&app.render(80, 48)).contains("로컬 추론 엔진"));
    }
    #[test]
    fn large_model_catalog_can_be_searched_without_touching_the_draft() {
        let mut app = App::new(false);
        app.models = (0..420).map(|i| format!("work/model-{i:03}")).collect();
        app.editor.insert("unsent draft");
        app.activate(Action::Command("/model".into()));
        for c in "model-419".chars() {
            let canvas = app.render(80, 40);
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &canvas);
        }
        let canvas = app.render(80, 40);
        assert_eq!(app.view, View::Models);
        assert_eq!(app.editor.text, "unsent draft");
        assert!(screen_text(&canvas).contains("model-419"));
        assert!(!canvas.hits.iter().any(|h| h.action == Action::Model("work/model-001".into())));
        assert!(canvas.hits.iter().any(|h| h.action == Action::Model("work/model-419".into())));
    }
    #[test]
    fn slash_commands_leave_filtered_model_picker_without_losing_draft() {
        let mut app = App::new(false);
        app.editor.insert("unsent draft");
        app.open_provider_models("work");
        for c in "/runtime".chars() {
            let canvas = app.render(80, 40);
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &canvas);
        }
        assert_eq!(app.editor.text, "/runtime");
        // The first Enter accepts the slash suggestion, the next executes it.
        for _ in 0..2 {
            let canvas = app.render(80, 40);
            app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &canvas);
        }
        assert_eq!(app.view, View::Runtime);
        assert_eq!(app.editor.text, "unsent draft");
    }
    #[test]
    fn slash_picker_command_can_be_cancelled_or_pasted_without_losing_draft() {
        let mut app = App::new(false);
        app.editor.insert("unsent draft");
        app.open_provider_models("work");
        let canvas = app.render(80, 40);
        app.key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE), &canvas);
        let canvas = app.render(80, 40);
        app.key(crossterm::event::KeyEvent::new(crossterm::event::KeyCode::Esc, KeyModifiers::NONE), &canvas);
        assert_eq!(app.editor.text, "unsent draft");
        app.paste("/runtime");
        assert_eq!(app.editor.text, "/runtime");
        app.activate(Action::Send);
        assert_eq!(app.view, View::Runtime);
        assert_eq!(app.editor.text, "unsent draft");
    }
    #[test]
    fn provider_menu_opens_a_local_registration_form() {
        let mut app = App::new(false);
        app.editor.insert("draft stays");
        app.activate(Action::Command("/provider add".into()));
        assert_eq!(app.input_forms.len(), 1, "registration must stay outside chat");
        assert!(app.input_forms[0].request.questions.iter().any(|q| q.secret));
        assert_eq!(app.editor.text, "draft stays");
        assert!(app.entries.is_empty());
    }
}

fn provider_connections(report: &mut Report) {
    const S: &str = "provider connections";
    let mut app = App::new(false);
    app.apply_event(AgentEvent::global(AgentUpdate::EntryUpdated(Entry {
        id: "provider-chat".into(), kind: Kind::Assistant, title: "".into(),
        body: "대화는 그대로 유지됩니다".into(), status: "completed".into(), expanded: false,
    })));
    app.editor.insert("작성 중인 질문");
    app.view = View::Providers;
    app.panel_open = true;
    let mut account = loom_core::models::registry::Account::template("openai", "work").unwrap();
    account.models = vec!["test-model".into()];
    app.provider_accounts.push(loom_core::models::registry::Summary { account, authenticated: true });
    for width in [48, 80, 140] {
        let screen = app.render(width, 40);
        let text = screen_text(&screen);
        report.check(S, format!("{width}: 연결·인증 상태·관리 동작을 필요한 공간에 표시"),
            text.contains("work") && text.contains("인증 설정됨")
                && screen.hits.iter().any(|hit| hit.action == Action::Command("/provider test work".into())), &text);
        report.check(S, format!("{width}: 초안 보존·넓은 화면 대화 유지"),
            app.editor.text == "작성 중인 질문" && (width < 110 || text.contains("대화는 그대로 유지됩니다")), &text);
        report.dump(S, &screen);
    }
    let account = loom_core::models::registry::Account::template("chatgpt", "personal").unwrap();
    app.provider_accounts.push(loom_core::models::registry::Summary { account, authenticated: false });
    let screen = app.render(140, 64);
    let text = screen_text(&screen);
    report.check(S, "ChatGPT 로그인·로그아웃·사용량 관리에 독립적으로 접근",
        screen.hits.iter().any(|hit| hit.action == Action::Command("/provider login chatgpt personal".into()))
            && screen.hits.iter().any(|hit| hit.action == Action::Command("/provider logout personal".into()))
            && screen.hits.iter().any(|hit| hit.action == Action::OpenLink("https://chatgpt.com/settings/usage".into()))
            && text.contains("다시 로그인하여 요금제 사용 승인"), &text);
    report.dump(S, &screen);
    app.activate(Action::Command("/provider add".into()));
    // Step through the actual form to the secret field using its public actions.
    for _ in 0..3 { app.activate(Action::QuestionMove(true)); }
    app.paste("secret-selfcheck-fixture");
    let screen = app.render(140, 40);
    let text = screen_text(&screen);
    report.check(S, "API 키는 입력 폼에서 가리고 대화·진단에는 남기지 않음",
        !text.contains("secret-selfcheck-fixture")
            && !app.diagnostics().to_string().contains("secret-selfcheck-fixture")
            && app.entries.iter().all(|entry| !entry.body.contains("secret-selfcheck-fixture")), &text);
    report.dump(S, &screen);
}

fn compact_diff_boundaries(report: &mut Report) {
    const S: &str = "compact diff boundaries";
    let mut app = App::new(true);
    app.diff = "+compact regression".into();
    app.activate(Action::Diff);
    for width in 40..=48 {
        let screen = app.render(width, 24);
        let text = screen_text(&screen);
        report.check(S, format!("{width}: 변경 검토가 좁은 터미널에서도 정상 표시"),
            text.contains("compact regression") && screen.hits.iter().all(|hit|
                u32::from(hit.x) + u32::from(hit.width) <= u32::from(width)), &text);
    }
}

fn local_model_discovery_status(report: &mut Report) {
    use crate::agent::ModelCatalogSource;
    const S: &str = "local model discovery";
    let mut app = App::new(false);
    app.models = vec!["auto".into()];
    app.apply_event(AgentEvent::global(AgentUpdate::ModelCatalogSources(vec![ModelCatalogSource {
        name: "ollama".into(), count: None, error: Some("not running (connection refused)".into()),
    }, ModelCatalogSource {
        name: "lmstudio".into(), count: None, error: Some("not running (connection refused)".into()),
    }])));
    app.activate(Action::Command("/model".into()));
    for width in [48, 80, 140] {
        let canvas = app.render(width, 40);
        let text = screen_text(&canvas);
        // Ollama is optional: when it is not running the catalog says nothing about it.
        report.check(S, format!("{width}: 로컬 서버가 꺼져 있으면 Ollama 안내를 표시하지 않음"),
            !text.contains("Ollama") && !text.contains("lmstudio") && !text.contains("연결 안 됨")
                && !text.contains("connection refused"), &text);
        report.dump(S, &canvas);
    }
    app.models.push("ollama/Spark-X2.5-4B".into());
    app.apply_event(AgentEvent::global(AgentUpdate::ModelCatalogSources(vec![ModelCatalogSource {
        name: "ollama".into(), count: Some(1), error: None,
    }])));
    app.paste("Spark");
    let canvas = app.render(80, 40);
    let text = screen_text(&canvas);
    report.check(S, "재조회로 돌아온 로컬 모델을 검색하고 선택 가능",
        canvas.hits.iter().any(|h| h.action == Action::Model("ollama/Spark-X2.5-4B".into()))
            && !text.contains("연결 안 됨"), &text);
    report.dump(S, &canvas);
}

/// LMX models run on Loom's own engine: they get their own first section and are
/// named by model, never by the identity hash in their id.
fn lmx_models_are_shown_as_loom_engine(report: &mut Report) {
    use crate::agent::ModelCatalogEntry;
    const S: &str = "LMX models in the model list";
    let id = "lmx/lmx/2c154b1b0e4fd3762851f45d621341d3e1b0bb47d6c58e3f60e18bdae21e9b64";
    let mut app = App::new(false);
    app.models = vec!["auto".into(), "openrouter/qwen/qwen3-coder".into(), id.into()];
    app.model_catalog.push(ModelCatalogEntry {
        id: id.into(), cost: "local".into(), price: Some((0.0, 0.0)), tools: true,
        context: Some(32_768), name: Some("Qwen3.8-35B-A3B-IQ3_M".into()),
    });
    app.activate(Action::Command("/model".into()));
    for width in [48, 80, 140] {
        let canvas = app.render(width, 40);
        let text = screen_text(&canvas);
        let own = text.find("◆ LMX");
        let direct = text.find("로컬·직접 연결");
        report.check(S, format!("{width}: LMX 섹션이 맨 위에 모델 이름으로 표시"),
            own.is_some_and(|own| direct.is_none_or(|direct| own < direct))
                && text.contains("Qwen3.8-35B-A3B")
                && !text.contains("2c154b1b")
                && canvas.hits.iter().any(|h| h.action == Action::Model(id.into())), &text);
        report.dump(S, &canvas);
    }
    app.activate(Action::Model(id.into()));
    let canvas = app.render(100, 30);
    let text = screen_text(&canvas);
    report.check(S, "선택한 LMX 모델도 이름으로 표시",
        text.contains("Qwen3.8-35B-A3B") && !text.contains("2c154b1b"), &text);
    report.dump(S, &canvas);
    // After an answer the router reports the LMX under its source path; the header keeps
    // the readable name instead of switching to that path.
    app.apply_event(AgentEvent::global(AgentUpdate::ModelResolved {
        requested: id.into(),
        provider: "lmx".into(),
        model: "empero-ai/Qwen3.8-35B-A3B-Distill-GGUF/Qwen3.8-35B-A3B-IQ3_M.gguf".into(),
    }));
    let canvas = app.render(100, 30);
    let text = screen_text(&canvas);
    report.check(S, "응답 후에도 LMX 모델 이름 유지 (원본 경로로 바뀌지 않음)",
        text.contains("Qwen3.8-35B-A3B") && !text.contains("empero-ai/") && !text.contains("Distill-GGUF"), &text);
    report.dump(S, &canvas);
    // A narrow window still names the model; LMX is marked by colour, not a prefix.
    app.apply_event(AgentEvent::global(AgentUpdate::ModelSpeed { tokens_per_second: 15.0 }));
    app.view = View::Chat;
    app.panel_open = false;
    for width in [48u16, 70] {
        let canvas = app.render(width, 30);
        let text = screen_text(&canvas);
        report.check(S, format!("{width}: 좁은 화면에서도 LMX 모델 이름이 색으로 구분되어 보임"),
            text.contains("Qwen3.8") && !text.contains("LMX ·") && tone_of(&canvas, "Qwen3.8") == Some(Tone::Success), &text);
        report.dump(S, &canvas);
    }
}

fn searchable_model_catalog(report: &mut Report) {
    use crate::agent::ModelCatalogEntry;
    const S: &str = "model catalog search and prices";
    let mut app = App::new(false);
    app.models = vec!["auto".into()];
    for (name, cost, price, tools) in [
        ("gemma-free", "free", Some((0.0, 0.0)), true),
        ("low", "paid", Some((0.01, 0.4)), true),
        ("medium", "paid", Some((1.0, 4.0)), true),
        ("high", "paid", Some((10.0, 30.0)), true),
        ("unpriced", "unknown", None, true),
        ("text-only", "free", Some((0.0, 0.0)), false),
    ] {
        let id = format!("work/{name}");
        app.models.push(id.clone());
        app.model_catalog.push(ModelCatalogEntry { id, cost: cost.into(), price, tools, context: Some(128_000), name: None });
    }
    app.model = "auto".into();
    app.editor.insert("작성 중인 대화 초안");
    app.activate(Action::Command("/model".into()));
    for width in [48, 80, 140] {
        let canvas = app.render(width, 48);
        let text = screen_text(&canvas);
        report.check(S, format!("{width}: 실제 가격에 따라 무료·저가·중간·고가·미확인 구분"),
            ["무료 API", "저가", "중간", "고가", "가격 미확인"].iter().all(|label| text.contains(label)), &text);
        report.check(S, format!("{width}: 검색과 무료 필터를 마우스로 선택 가능"),
            canvas.hits.iter().any(|h| h.action == Action::ModelSearch)
                && canvas.hits.iter().any(|h| h.action == Action::ModelFreeOnly)
                && canvas.hits.iter().any(|h| h.action == Action::ModelGrouping), &text);
        report.dump(S, &canvas);
    }
    app.activate(Action::ModelSearch);
    app.paste("WORK medium");
    let canvas = app.render(80, 48);
    let text = screen_text(&canvas);
    report.check(S, "공급자·이름 여러 검색어 및 입력·출력 실제 단가 표시",
        text.contains("work/medium") && text.contains("입력 $1 · 출력 $4")
            && !canvas.hits.iter().any(|h| h.action == Action::Model("work/high".into()))
            && app.editor.text == "작성 중인 대화 초안", &text);
    report.dump(S, &canvas);
    app.key(crossterm::event::KeyEvent::new(crossterm::event::KeyCode::Esc, KeyModifiers::NONE), &canvas);
    app.activate(Action::ModelFreeOnly);
    app.activate(Action::ToggleModelGroup("가격\u{1}1무료 API".into()));
    let canvas = app.render(80, 48);
    let text = screen_text(&canvas);
    report.check(S, "무료만 필터는 가격 미확인·유료 모델을 제외하고 도구 지원을 구분",
        text.contains("gemma-free") && text.contains("도구 미지원")
            && !canvas.hits.iter().any(|h| h.action == Action::Model("work/unpriced".into()))
            && !canvas.hits.iter().any(|h| h.action == Action::Model("work/high".into())), &text);
    report.dump(S, &canvas);
    report.check(S, "무료 자동 라우팅은 auto 선택과 비용 제한을 함께 요청",
        app.activate(Action::Command("/provider policy free".into())) == Some(Intent::Provider("policy free".into()))
            && app.model == "auto" && app.view == View::Models, "auto + free policy");
    app.activate(Action::ClearModelSearch);
    app.paste("없는모델");
    let canvas = app.render(80, 40);
    report.check(S, "검색 결과 없음은 명확히 알리고 대화 초안은 보존",
        screen_text(&canvas).contains("일치하는 모델이 없습니다") && app.editor.text == "작성 중인 대화 초안", screen_text(&canvas));
    report.dump(S, &canvas);
}

fn model_management_and_commentary(report: &mut Report) {
    const S: &str = "routing runtime and commentary";
    let mut app = App::new(false);
    app.models = vec!["auto".into(), "loom/owner/spark/model.gguf".into()];
    app.model = "auto".into();
    app.editor.insert("유지할 대화 초안");
    app.panel_open = true;
    app.routing_max_cost = loom_core::models::CostClass::Free;
    let account = loom_core::models::registry::Account::template("openrouter", "personal").unwrap();
    app.provider_accounts.push(loom_core::models::registry::Summary { account, authenticated: true });
    app.view = View::Routing;
    for width in [48, 80, 140] {
        let canvas = app.render(width, 52);
        let text = screen_text(&canvas);
        report.check(S, format!("{width}: 라우팅 설정에서 비용 상한과 연결별 자동 선택 관리"),
            text.contains("자동 라우팅 설정") && text.contains("비용 상한") && text.contains("personal")
                && canvas.hits.iter().any(|h| h.action == Action::Command("/provider policy free".into()))
                && canvas.hits.iter().any(|h| h.action == Action::Command("/provider route personal manual".into())), &text);
        // Korean wrapping may split a word across rows; verify all content survives.
        let unwrapped: String = text.chars().filter(|c| !c.is_whitespace() && *c != '│').collect();
        report.check(S, format!("{width}: 라우팅 설명을 잘라내지 않고 표시"),
            unwrapped.contains("Loom이허용한비용·도구지원·연결상태로선택합니다."), &text);
        report.dump(S, &canvas);
    }
    app.view = View::Runtime;
    app.runtime_checkpoints.push(loom_core::models::checkpoints::Checkpoint {id: "owner/spark/model.gguf".into(), path: "/models/owner/spark/model.gguf".into(), bytes: 2_000_000_000});
    for width in [48, 80, 140] {
        let canvas = app.render(width, 60);
        let text = screen_text(&canvas);
        report.check(S, format!("{width}: 내장 엔진·체크포인트 크기·다운로드·삭제·선택 표시"),
            text.contains("로컬 추론 엔진") && text.contains("2.00 GB")
                && canvas.hits.iter().any(|h| h.action == Action::Command("/runtime pull".into()))
                && canvas.hits.iter().any(|h| h.action == Action::Command("/runtime remove owner/spark/model.gguf".into()))
                && canvas.hits.iter().any(|h| h.action == Action::Command("/model loom/owner/spark/model.gguf".into())), &text);
        report.dump(S, &canvas);
    }
    {
        // Downloads convert to LMX by default; both choices are visible and toggle; the panel
        // carries no self-description.
        let canvas = app.render(80, 60);
        let text = screen_text(&canvas);
        report.check(S, "다운로드 후 LMX 변환·원본 보관 선택을 표시하고 설명 문구는 없음",
            text.contains("● 받은 GGUF를 LMX로 변환") && text.contains("● 변환 후 원본 GGUF 보관")
                && !text.contains("별도 서버") && !text.contains("프로세스 내장")
                && canvas.hits.iter().any(|h| h.action == Action::Command("/runtime keep-original off".into())), &text);
        report.dump(S, &canvas);
        let saved = app.activate(Action::Command("/runtime keep-original off".into()));
        let canvas = app.render(80, 60);
        report.check(S, "원본 보관을 끄면 저장하고 표시가 바뀜",
            saved == Some(Intent::SaveSettings) && !app.preferences.lmx_keep_original
                && screen_text(&canvas).contains("○ 변환 후 원본 GGUF 보관"), screen_text(&canvas));
        app.activate(Action::Command("/runtime keep-original on".into()));
    }
    app.local_models.transfer = Some(crate::local_models::Transfer {spec:"hf:owner/spark/model.gguf".into(), done:500_000_000, total:2_000_000_000, active:true, ..Default::default()});
    let canvas = app.render(80, 60);
    report.check(S, "다운로드 바이트·진행률·취소 동작을 표시", screen_text(&canvas).contains("25.0%")
        && canvas.hits.iter().any(|h| h.action == Action::Command("/runtime cancel".into())), screen_text(&canvas));
    report.dump(S, &canvas);
    app.activate(Action::Command("/runtime cancel".into()));
    let canvas = app.render(80, 60);
    report.check(S, "다운로드 취소가 대화 초안을 건드리지 않음", screen_text(&canvas).contains("취소 요청 중") && app.editor.text == "유지할 대화 초안", screen_text(&canvas));
    let mut projection = crate::backend::v5_projection::Projection::default();
    use loom_core::agent::events::{Event, Stage};
    projection.observe(&Event::new("main", Stage::Answer, "commentary", serde_json::json!({"text":"핵심 함수를 확인하고 있습니다."})));
    projection.observe(&Event::new("main", Stage::Source, "src/core.rs", serde_json::json!({"tool":"open","completed":true,"args":{"target":"src/core.rs"}})));
    app.view = View::Chat;
    for entry in projection.entries("turn", false) { app.upsert_entry(entry); }
    app.upsert_entry(Entry {id:"turn-assistant".into(), kind:Kind::Assistant, title:"Loom".into(), body:"확인을 마쳤습니다.".into(), status:"completed".into(), expanded:false});
    let order: Vec<_> = app.entries.iter().map(|e| e.id.as_str()).collect();
    report.check(S, "중간 설명·도구·최종 응답이 대화 순서대로 남음", order == ["turn-commentary-0", "turn-tool-0", "turn-assistant"], format!("{order:?}"));
    report.dump(S, &app.render(80, 40));
}
