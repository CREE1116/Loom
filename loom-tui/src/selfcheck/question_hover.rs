//! Question controls retain hover feedback after their composer overlay is painted.
use crate::{
    agent::{AgentEvent, AgentUpdate, InputRequest, Question, QuestionOption},
    app::App,
    engine::{Action, Canvas, Tone},
    selfcheck::{Check, Dump, Report},
};
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};

const SCENARIO: &str = "question_controls_hover";

fn event(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }
}

fn check(report: &mut Report, name: String, pass: bool, detail: String) {
    report.checks.push(Check {
        scenario: SCENARIO,
        name,
        pass,
        detail,
    });
}

fn has_tone(canvas: &Canvas, action: &Action, tone: Tone) -> bool {
    let Some(hit) = canvas.hits.iter().find(|hit| &hit.action == action) else {
        return false;
    };
    (hit.x..hit.x + hit.width).all(|x| {
        canvas.cells[usize::from(hit.y) * usize::from(canvas.width) + usize::from(x)].tone == tone
    })
}

fn dump(report: &mut Report, label: String, canvas: &Canvas) {
    let rows = canvas
        .cells
        .chunks(usize::from(canvas.width))
        .map(|cells| {
            let mut line = String::new();
            let mut start = 0;
            while start < cells.len() {
                let tone = cells[start].tone;
                let end = start
                    + cells[start..]
                        .iter()
                        .take_while(|cell| cell.tone == tone)
                        .count();
                let text: String = cells[start..end]
                    .iter()
                    .map(|cell| cell.text.as_str())
                    .collect();
                if tone == Tone::Normal || text.trim().is_empty() {
                    line.push_str(&text);
                } else {
                    line.push_str(&format!("[{tone:?}:{text}]"));
                }
                start = end;
            }
            line.trim_end().to_owned()
        })
        .collect();
    report.dumps.push(Dump {
        scenario: SCENARIO,
        label,
        width: canvas.width,
        height: canvas.height,
        rows,
    });
}

pub(crate) fn run(report: &mut Report) {
    for width in [40, 80, 120] {
        let mut app = App::new(false);
        let question = |id: &str, header: &str| Question {
            id: id.into(),
            header: header.into(),
            prompt: "진행 방향을 선택하세요.".into(),
            options: vec![
                QuestionOption {
                    label: "진행".into(),
                    description: "계속합니다.".into(),
                },
                QuestionOption {
                    label: "보류".into(),
                    description: "기다립니다.".into(),
                },
            ],
            allow_custom: true,
            secret: false,
        };
        app.apply_event(AgentEvent::global(AgentUpdate::InputRequested(
            InputRequest {
                id: "hover-question".into(),
                blocking: false,
                questions: vec![
                    question("first", "첫 질문"),
                    question("second", "둘째 질문"),
                ],
            },
        )));
        let initial = app.render(width, 24);
        dump(report, format!("질문 버튼 기본 {width}x24"), &initial);
        for action in [
            Action::QuestionOption(0),
            Action::QuestionOption(1),
            Action::QuestionOption(2),
            Action::QuestionMove(false),
            Action::QuestionMove(true),
            Action::SubmitAnswers,
            Action::DismissQuestion,
        ] {
            let canvas = app.render(width, 24);
            let hit = canvas.hits.iter().find(|hit| hit.action == action);
            let Some(hit) = hit else {
                check(
                    report,
                    format!("{width} {action:?} 클릭 영역"),
                    false,
                    "버튼이 보이지 않습니다.".into(),
                );
                continue;
            };
            let intent = app.mouse(event(MouseEventKind::Moved, hit.x, hit.y), &canvas);
            let hovered = app.render(width, 24);
            check(
                report,
                format!("{width} {action:?} 호버"),
                intent.is_none()
                    && app.hovered.as_ref() == Some(&action)
                    && app.focus.is_none()
                    && has_tone(&hovered, &action, Tone::Hover),
                "마우스 이동은 실행 없이 해당 버튼에 Hover를 표시해야 합니다.".into(),
            );
            if action == Action::QuestionMove(true) {
                dump(report, format!("다음 버튼 호버 {width}x24"), &hovered);
                app.focus = Some(action.clone());
                let focused = app.render(width, 24);
                check(
                    report,
                    format!("{width} 질문 포커스 우선"),
                    has_tone(&focused, &action, Tone::Selected),
                    "호버가 Selected를 덮어쓰면 안 됩니다.".into(),
                );
                app.focus = None;
            }
            app.mouse(event(MouseEventKind::Moved, 0, 0), &hovered);
            let cleared = app.render(width, 24);
            check(
                report,
                format!("{width} {action:?} 호버 해제"),
                app.hovered.is_none() && !has_tone(&cleared, &action, Tone::Hover),
                "버튼 밖으로 이동하면 Hover가 사라져야 합니다.".into(),
            );
        }
        let canvas = app.render(width, 24);
        if let Some(hit) = canvas
            .hits
            .iter()
            .find(|hit| hit.action == Action::QuestionMove(true))
        {
            app.mouse(
                event(MouseEventKind::Down(MouseButton::Left), hit.x, hit.y),
                &canvas,
            );
            let pressed = app.render(width, 24);
            let first_visible =
                (0..pressed.height).any(|y| pressed.plain_line(y).contains("첫 질문"));
            app.mouse(
                event(MouseEventKind::Up(MouseButton::Left), hit.x, hit.y),
                &pressed,
            );
            let next = app.render(width, 24);
            check(
                report,
                format!("{width} 다음 버튼 클릭"),
                first_visible && (0..next.height).any(|y| next.plain_line(y).contains("둘째 질문")),
                "다음 질문은 버튼을 놓을 때 열려야 합니다.".into(),
            );
        }
        app.activate(Action::DismissQuestion);
        let canvas = app.render(width, 24);
        if let Some(hit) = canvas
            .hits
            .iter()
            .find(|hit| hit.action == Action::Questions)
        {
            app.mouse(event(MouseEventKind::Moved, hit.x, hit.y), &canvas);
            let hovered = app.render(width, 24);
            check(
                report,
                format!("{width} 숨긴 질문 열기 호버"),
                has_tone(&hovered, &Action::Questions, Tone::Hover),
                "질문 열기 버튼도 Hover를 표시해야 합니다.".into(),
            );
        } else {
            check(
                report,
                format!("{width} 숨긴 질문 열기 클릭 영역"),
                false,
                "질문 열기 버튼이 없습니다.".into(),
            );
        }
    }
}
