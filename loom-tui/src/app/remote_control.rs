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
    /// The address is a public HTTPS tunnel rather than the local network.
    pub public: bool,
    /// A tunnel was requested and its address is not known yet.
    pub tunnel_pending: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AnswerValue, InputRequest, Question, QuestionAnswer};

    #[test]
    fn remote_send_preserves_the_entire_local_composer_and_attachments() {
        let directory = tempfile::tempdir().unwrap();
        let file = directory.path().join("local.png");
        image::RgbaImage::new(1, 1).save(&file).unwrap();
        let mut app = App::new(false);
        app.editor.insert("local draft");
        app.editor.cursor = 1;
        app.editor.anchor = Some(4);
        app.images
            .push(crate::attachment::ImageAttachment::from_path(&file).unwrap());
        app.selected_skills.push(crate::agent::Skill {
            name: "local".into(),
            path: "local/SKILL.md".into(),
            description: String::new(),
        });
        app.image_drop = Some((0, 0, Instant::now()));
        app.paste_pending = true;
        app.focus = Some(Action::Menu);
        app.checked_remote_command(Command::Send("phone message".into()))
            .unwrap();
        assert_eq!(app.editor.text, "local draft");
        assert_eq!((app.editor.cursor, app.editor.anchor), (1, Some(4)));
        assert_eq!(app.images.len(), 1);
        assert_eq!(app.selected_skills.len(), 1);
        assert!(app.image_drop.is_some() && app.paste_pending);
        assert_eq!(app.focus, Some(Action::Menu));
        assert!(app.submitted_images.is_empty() && app.submitted_skills.is_empty());
        assert!(
            app.queued
                .iter()
                .all(|q| q.images.is_empty() && q.skills.is_empty())
        );
    }

    #[test]
    fn remote_questions_validate_answers_and_recover_from_runtime_failure() {
        let mut app = App::new(false);
        app.input_requested(InputRequest {
            id: "request".into(),
            blocking: true,
            questions: vec![Question {
                id: "q".into(),
                header: "".into(),
                prompt: "Target?".into(),
                options: vec![],
                allow_custom: true,
                secret: true,
            }],
        });
        assert!(
            app.checked_remote_command(Command::Answer {
                id: "request".into(),
                answers: vec![]
            })
            .is_err()
        );
        assert!(!app.input_forms[0].answering);
        let command = Command::Answer {
            id: "request".into(),
            answers: vec![QuestionAnswer {
                question_id: "q".into(),
                value: AnswerValue::Text("private answer".into()),
            }],
        };
        assert!(matches!(
            app.checked_remote_command(command.clone()).unwrap(),
            Some(Intent::Answer(_, _))
        ));
        assert!(
            app.checked_remote_command(command).is_err(),
            "double reply rejected"
        );
        assert!(!app.remote_snapshot().to_string().contains("private answer"));
        app.input_reply_failed("request", "runtime unavailable");
        assert!(matches!(
            app.checked_remote_command(Command::CancelQuestions("request".into()))
                .unwrap(),
            Some(Intent::CancelQuestions(_))
        ));
        app.input_resolved("request");
        assert!(
            app.checked_remote_command(Command::CancelQuestions("request".into()))
                .is_err()
        );
    }

    #[test]
    fn stale_and_unsupported_actions_are_reported() {
        let mut app = App::new(false);
        assert!(
            app.checked_remote_command(Command::Model("unknown".into()))
                .is_err()
        );
        assert!(
            app.checked_remote_command(Command::Effort("unknown".into()))
                .is_err()
        );
        assert!(
            app.checked_remote_command(Command::Reply {
                id: "stale".into(),
                choice: "accept".into()
            })
            .is_err()
        );
        assert!(
            app.checked_remote_command(Command::Send("/remote off".into()))
                .is_err()
        );
        assert!(app.checked_remote_command(Command::Send("/plan ".into())).is_err());
        app.readonly = true;
        assert!(
            app.checked_remote_command(Command::Send("message".into()))
                .is_err()
        );
    }
}

impl App {
    pub fn remote_started(&mut self, url: String, qr: Vec<Vec<bool>>, tunnel_pending: bool) {
        self.remote = Some(RemoteView {
            url,
            qr,
            clients: 0,
            shown: true,
            public: false,
            tunnel_pending,
        });
    }

    /// The tunnel's HTTPS address arrived: show it (and its QR code) instead.
    pub fn remote_public(&mut self, url: String, qr: Vec<Vec<bool>>) {
        if let Some(view) = self.remote.as_mut() {
            view.url = url;
            view.qr = qr;
            view.public = true;
            view.tunnel_pending = false;
        }
    }

    pub fn remote_stopped(&mut self) {
        self.remote = None;
        self.notify("원격 조작을 껐습니다.");
    }

    pub fn remote_tunnel_failed(&mut self, url: String, qr: Vec<Vec<bool>>, reason: &str) {
        if let Some(view) = self.remote.as_mut() {
            view.url = url;
            view.qr = qr;
            view.public = false;
            view.tunnel_pending = false;
        }
        self.notify(&format!(
            "{reason} 같은 네트워크 주소는 계속 사용할 수 있습니다."
        ));
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
        let questions: Vec<_> = self.input_forms.iter().map(|form| json!({
            "id": form.request.id, "blocking": form.request.blocking, "answering": form.answering,
            "questions": form.request.questions.iter().map(|question| json!({
                "id": question.id, "header": question.header, "prompt": question.prompt,
                "options": question.options.iter().map(|option| json!({"label":option.label,"description":option.description})).collect::<Vec<_>>(),
                "allow_custom": question.allow_custom, "secret": question.secret,
            })).collect::<Vec<_>>(),
        })).collect();
        let label = |id: &str| -> String {
            if id == "auto" {
                return "자동 (Loom)".into();
            }
            let name = self.model_display(id);
            // LMX names are source paths; the file stem reads best.
            let tail = name.rsplit('/').next().unwrap_or(&name);
            tail.strip_suffix(".gguf").unwrap_or(tail).to_owned()
        };
        let group = |id: &str| -> String {
            match super::model_group(id) {
                None => "자동".into(),
                Some(group) => {
                    let group = group.split('\u{1}').collect::<Vec<_>>();
                    let section = group[0].trim_start_matches(|c: char| c.is_ascii_digit());
                    match group.get(1) {
                        Some(endpoint) if section != "LMX" => format!("{section} · {endpoint}"),
                        _ => section.to_owned(),
                    }
                }
            }
        };
        let models: Vec<_> = self
            .models
            .iter()
            .map(|id| json!({"id": id, "label": label(id), "group": group(id)}))
            .collect();
        let efforts: Vec<_> = self
            .model_profile()
            .map(|p| {
                p.efforts
                    .iter()
                    .map(|e| json!({"value": e.value, "description": e.description}))
                    .collect()
            })
            .unwrap_or_default();
        json!({
            "title": self.session_title,
            "status": self.status_phrase(),
            "busy": self.is_working(),
            "model": self.model,
            "model_label": label(&self.model),
            "effort": self.effort,
            "efforts": efforts,
            "models": models,
            "queued": self.queued.len(),
            "public": self.remote.as_ref().is_some_and(|v| v.public),
            "entries": entries,
            "approvals": approvals,
            "questions": questions,
        })
    }

    /// Validate against live state before acknowledging a browser action.
    pub fn checked_remote_command(&mut self, command: Command) -> Result<Option<Intent>, String> {
        if self.readonly {
            return Err("열람 전용 창에서는 조작할 수 없습니다.".into());
        }
        match &command {
            Command::Send(text) => {
                if text.trim().is_empty() {
                    return Err("메시지를 입력해주세요.".into());
                }
                if self.permission_pending.is_some() {
                    return Err("권한 설정 확인이 끝난 뒤 다시 전송해주세요.".into());
                }
                if text.trim_start().starts_with('/') && !text.trim_start().strip_prefix("/plan ").is_some_and(|goal| !goal.trim().is_empty()) {
                    return Err("원격 메뉴로 조작하거나 컴퓨터 창에서 명령을 실행해주세요.".into());
                }
            }
            Command::Model(model) if !self.models.contains(model) || self.submitted.is_some() => {
                return Err("지금 선택할 수 없는 모델입니다. 목록을 새로 확인해주세요.".into());
            }
            Command::Effort(effort)
                if self.submitted.is_some()
                    || (!effort.is_empty()
                        && !self
                            .model_profile()
                            .is_some_and(|p| p.efforts.iter().any(|e| e.value == *effort))) =>
            {
                return Err("선택한 모델에서 지원하지 않는 추론 수준입니다.".into());
            }
            Command::Reply { id, choice }
                if !self.approvals.iter().any(|a| {
                    a.id == *id && !a.answering && a.choices.iter().any(|c| c.result == *choice)
                }) =>
            {
                return Err("이미 처리됐거나 유효하지 않은 승인 요청입니다.".into());
            }
            Command::Answer { id, answers } => {
                return self.remote_answers(id, Some(answers.clone()));
            }
            Command::CancelQuestions(id) => return self.remote_answers(id, None),
            Command::NewConversation => {
                if let Some(reason) = self.session_switch_block_reason() {
                    return Err(reason.into());
                }
            }
            Command::Compact
                if self.is_working() || self.input_forms.iter().any(|f| f.request.blocking) =>
            {
                return Err("현재 작업과 질문이 끝난 뒤 대화를 압축해주세요.".into());
            }
            _ => {}
        }
        Ok(self.remote_command(command))
    }

    /// Turns a browser's command into what the same action in the window would do. A
    /// message sent while the user is typing here leaves their draft alone.
    pub fn remote_command(&mut self, command: Command) -> Option<Intent> {
        match command {
            Command::Send(text) => {
                let draft = std::mem::take(&mut self.editor);
                let confirmed = std::mem::replace(&mut self.paste_confirmed, true);
                let images = std::mem::take(&mut self.images);
                let skills = std::mem::take(&mut self.selected_skills);
                let image_drop = self.image_drop.take();
                let command_draft = self.model_command_draft.take();
                let paste_pending = std::mem::replace(&mut self.paste_pending, false);
                let immediate_pending = std::mem::replace(&mut self.immediate_pending, false);
                let focus = self.focus.clone();
                let composer_active = self.composer_active;
                let history_position = self.history_position;
                self.editor.insert(&text);
                let intent = self.activate(Action::Send);
                self.editor = draft;
                self.paste_confirmed = confirmed;
                self.images = images;
                self.selected_skills = skills;
                self.image_drop = image_drop;
                self.model_command_draft = command_draft;
                self.paste_pending = paste_pending;
                self.immediate_pending = immediate_pending;
                self.focus = focus;
                self.composer_active = composer_active;
                self.history_position = history_position;
                intent
            }
            Command::Interrupt => self.activate(Action::Interrupt),
            Command::Model(model) => self.activate(Action::Model(model)),
            Command::Effort(effort) => self.activate(Action::Effort(effort)),
            Command::NewConversation | Command::Compact => {
                let draft = std::mem::take(&mut self.editor);
                let saved = self.model_command_draft.take();
                let intent = self.activate(Action::Command(
                    if command == Command::Compact {
                        "/compact"
                    } else {
                        "/clear"
                    }
                    .into(),
                ));
                self.editor = draft;
                self.model_command_draft = saved;
                intent
            }
            Command::Answer { id, answers } => {
                self.remote_answers(&id, Some(answers)).ok().flatten()
            }
            Command::CancelQuestions(id) => self.remote_answers(&id, None).ok().flatten(),
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

    /// The window with the address and QR code, over the conversation. It shrinks to
    /// fit: first the notes go, then the side padding, so a narrow terminal still shows
    /// a scannable code.
    pub(super) fn paint_remote(&self, c: &mut Canvas) {
        let Some(view) = self.remote.as_ref().filter(|v| v.shown) else {
            return;
        };
        let english = self.preferences.language == Language::English;
        let qr_width = view.qr.first().map_or(0, Vec::len) as u16;
        let qr_height = (view.qr.len() as u16).div_ceil(2);
        let url_width = view.url.chars().count() as u16;
        // Full: title, code, blank, hint, address, blank, connections, warning.
        // Compact: title, code, address.
        let full = qr_height + 8;
        let compact = qr_height + 2;
        let show_qr = qr_width > 0 && qr_width <= c.width && compact <= c.height;
        let detailed = !show_qr || full <= c.height;
        let height = match (show_qr, detailed) {
            (true, true) => full,
            (true, false) => compact,
            (false, _) => 8u16.min(c.height),
        };
        let width = (if show_qr { qr_width + 4 } else { 0 })
            .max(if detailed { url_width + 4 } else { 0 })
            .max(if detailed { 48 } else { 0 })
            .max(qr_width)
            .min(c.width);
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
        c.hits.push(crate::engine::Hit {
            x,
            y,
            width,
            height,
            action: Action::TranscriptText,
        });
        let pad = if width >= qr_width + 4 { 2 } else { 0 };
        c.text(
            x + pad,
            y,
            if english {
                "Remote control"
            } else {
                "원격 조작"
            },
            Tone::Accent,
        );
        let close = if english { "Close" } else { "닫기" };
        c.button(
            x + width.saturating_sub(3 + close.chars().count() as u16 * 2),
            y,
            close,
            Action::Command("/remote hide".into()),
            false,
        );
        let mut line = y + 1;
        if show_qr {
            c.qr(x + (width - qr_width) / 2, line, &view.qr);
            line += qr_height;
        }
        let address = |c: &mut Canvas, row: u16| {
            let room = width.saturating_sub(2 * pad);
            c.text(
                x + pad,
                row,
                &one_line(&view.url, usize::from(room)),
                Tone::Muted,
            );
            c.hits.push(crate::engine::Hit {
                x: x + pad,
                y: row,
                width: room,
                height: 1,
                action: Action::Command("/remote copy".into()),
            });
        };
        if !detailed {
            address(c, line);
            return;
        }
        let scan = if !show_qr {
            if english {
                "Enlarge the terminal to show the QR code."
            } else {
                "터미널을 키우면 QR 코드가 보입니다."
            }
        } else if view.public {
            if english {
                "Scan with a phone camera · works from anywhere (HTTPS)."
            } else {
                "폰 카메라로 스캔하세요 · 어디서나 접속 가능 (HTTPS)"
            }
        } else if view.tunnel_pending {
            if english {
                "Same network for now · preparing an internet address…"
            } else {
                "같은 네트워크용 주소 · 인터넷 주소 준비 중…"
            }
        } else if english {
            "Scan with a phone on the same network."
        } else {
            "같은 네트워크의 폰 카메라로 스캔하세요."
        };
        c.text(x + pad, line + 1, scan, Tone::Normal);
        address(c, line + 2);
        let clients = if english {
            format!("Connected browsers: {}", view.clients)
        } else {
            format!("접속 중인 브라우저 {}개", view.clients)
        };
        c.text(x + pad, line + 4, &clients, Tone::Muted);
        c.text(
            x + pad,
            line + 5,
            if english {
                "Anyone with this address can control Loom · /remote off"
            } else {
                "이 주소를 아는 사람은 Loom을 조작할 수 있습니다 · /remote off로 끄기"
            },
            Tone::Warning,
        );
    }
}
