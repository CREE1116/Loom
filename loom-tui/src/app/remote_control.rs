//! Remote control on the app side: the state a browser sees, the commands it sends,
//! and the window that shows the address and QR code.
use super::*;
use crate::remote::Command;
use serde_json::json;

/// What the window shows while remote control is on.
#[derive(Default)]
pub struct RemoteView {
    pub url: String,
    pub qr: Vec<Vec<bool>>,
    pub clients: usize,
    /// The address/QR window is open.
    pub shown: bool,
}

impl App {
    pub fn remote_started(&mut self, url: String, qr: Vec<Vec<bool>>) {
        self.remote = Some(RemoteView { url, qr, clients: 0, shown: true });
    }

    pub fn remote_stopped(&mut self) {
        self.remote = None;
        self.notify("원격 조작을 껐습니다.");
    }

    pub fn remote_clients(&mut self, clients: usize) -> bool {
        let joined = match &mut self.remote {
            Some(view) if view.clients != clients => {
                let first = view.clients == 0 && clients > 0;
                view.clients = clients;
                // Someone scanned the code: the window has done its job.
                if first {
                    view.shown = false;
                }
                first
            }
            Some(_) => return false,
            None => return false,
        };
        if joined {
            self.notify("브라우저가 원격 조작에 연결됐습니다.");
        }
        true
    }

    /// The conversation as a browser sees it.
    pub fn remote_snapshot(&self) -> serde_json::Value {
        let kind = |kind: &Kind| match kind {
            Kind::User => "user",
            Kind::Assistant => "assistant",
            Kind::Tool => "tool",
            Kind::Change => "change",
            Kind::System => "system",
        };
        let entries: Vec<_> = self
            .entries
            .iter()
            .rev()
            .take(200)
            .rev()
            .map(|e| {
                json!({
                    "id": e.id, "kind": kind(&e.kind), "title": e.title,
                    // Tool output can be large; the page shows tools as one line.
                    "body": if e.kind == Kind::Tool { String::new() } else { e.body.clone() },
                    "status": e.status,
                })
            })
            .collect();
        let approvals: Vec<_> = self
            .approvals
            .iter()
            .map(|a| {
                json!({
                    "id": a.id, "title": a.title, "summary": a.summary, "detail": a.detail,
                    "answering": a.answering,
                    "choices": a.choices.iter().map(|c| json!({"label": c.label, "result": c.result})).collect::<Vec<_>>(),
                })
            })
            .collect();
        json!({
            "title": self.session_title,
            "status": self.status_phrase(),
            "busy": self.is_working(),
            "model": self.model,
            "entries": entries,
            "approvals": approvals,
        })
    }

    /// Turns a browser's command into what the same action in the window would do. A
    /// message sent while the user is typing here leaves their draft alone.
    pub fn remote_command(&mut self, command: Command) -> Option<Intent> {
        match command {
            Command::Send(text) => {
                let draft = std::mem::take(&mut self.editor);
                let confirmed = std::mem::replace(&mut self.paste_confirmed, true);
                self.editor.insert(&text);
                let intent = self.activate(Action::Send);
                self.editor = draft;
                self.paste_confirmed = confirmed;
                intent
            }
            Command::Interrupt => self.activate(Action::Interrupt),
            Command::Reply { id, choice } => {
                let index = self
                    .approvals
                    .iter()
                    .find(|a| a.id == id)?
                    .choices
                    .iter()
                    .position(|c| c.result == choice)?;
                self.activate(Action::Decide(id, index))
            }
        }
    }

    /// The window with the address and QR code, over the conversation.
    pub(super) fn paint_remote(&self, c: &mut Canvas) {
        let Some(view) = self.remote.as_ref().filter(|v| v.shown) else {
            return;
        };
        let english = self.preferences.language == Language::English;
        let qr_width = view.qr.first().map_or(0, Vec::len) as u16;
        let qr_height = (view.qr.len() as u16).div_ceil(2);
        let url_width = view.url.chars().count() as u16;
        let width = (qr_width + 4).max(url_width + 4).max(48).min(c.width.saturating_sub(2));
        let fits = qr_width + 4 <= c.width && qr_height + 9 <= c.height;
        let height = if fits { qr_height + 9 } else { 9 }.min(c.height);
        let x = (c.width - width) / 2;
        let y = (c.height - height) / 2;
        for row in y..y + height {
            for col in x..x + width {
                let cell = &mut c.cells[usize::from(row) * usize::from(c.width) + usize::from(col)];
                *cell = Default::default();
                cell.text = " ".into();
            }
        }
        // Clicks inside the window must not reach the conversation underneath.
        c.hits.push(crate::engine::Hit { x, y, width, height, action: Action::TranscriptText });
        c.text(x + 2, y, if english { "Remote control" } else { "원격 조작" }, Tone::Accent);
        let close = if english { "Close" } else { "닫기" };
        c.button(x + width - 3 - close.chars().count() as u16 * 2, y, close, Action::Command("/remote hide".into()), false);
        let mut line = y + 1;
        if fits {
            c.qr(x + (width - qr_width) / 2, line, &view.qr);
            line += qr_height;
        }
        let scan = if !fits {
            if english { "Widen the terminal to show the QR code." } else { "터미널을 넓히면 QR 코드가 보입니다." }
        } else if english {
            "Scan with a phone on the same network."
        } else {
            "같은 네트워크의 폰 카메라로 스캔하세요."
        };
        c.text(x + 2, line + 1, scan, Tone::Normal);
        c.text(x + 2, line + 2, &one_line(&view.url, usize::from(width - 4)), Tone::Muted);
        c.hits.push(crate::engine::Hit {
            x: x + 2,
            y: line + 2,
            width: width - 4,
            height: 1,
            action: Action::Command("/remote copy".into()),
        });
        let clients = if english {
            format!("Connected browsers: {}", view.clients)
        } else {
            format!("접속 중인 브라우저 {}개", view.clients)
        };
        c.text(x + 2, line + 4, &clients, Tone::Muted);
        c.text(
            x + 2,
            line + 5,
            if english {
                "Anyone with this address can control Loom · /remote off"
            } else {
                "이 주소를 아는 사람은 Loom을 조작할 수 있습니다 · /remote off"
            },
            Tone::Warning,
        );
    }
}
