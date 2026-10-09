use super::*;
use crate::attachment::{self, ImageAttachment};

impl App {
    pub fn attach_images(&mut self, images: Vec<ImageAttachment>) {
        if self.readonly || self.trust_prompt.is_some() || self.approval_visible() {
            return;
        }
        if !self.input_forms.is_empty() {
            self.error(
                "질문 답변은 텍스트로 입력하세요. 이미지는 메시지 입력창에 첨부할 수 있습니다.",
            );
            return;
        }
        self.activate(Action::Input);
        if self.view != View::Chat && !self.wide_layout {
            self.switch_view(View::Chat);
        }
        self.menu_open = false;
        self.menu_dismissed = true;
        self.images.extend(images);
    }

    // Some terminals deliver file dragging as a burst of normal key events.
    // Convert only the newly inserted path, so the surrounding draft is untouched.
    pub(super) fn insert_with_image_drop(&mut self, ch: char) {
        let at = self
            .editor
            .selection()
            .map_or(self.editor.cursor, |range| range.start);
        let continuing = self.image_drop.filter(|(_, end, _)| *end == at);
        self.editor.insert(&ch.to_string());
        let start = continuing
            .map(|(start, _, _)| start)
            .or_else(|| matches!(ch, '/' | '~' | '\'' | '"').then_some(at));
        self.image_drop = start.map(|start| {
            (
                start,
                self.editor.cursor,
                Instant::now() + Duration::from_millis(200),
            )
        });
    }

    pub(super) fn flush_image_drop(&mut self) -> bool {
        let Some((start, end, _)) = self.image_drop.take() else {
            return false;
        };
        if self.readonly
            || self.editor.cursor != end
            || !self.input_forms.is_empty()
            || self.trust_prompt.is_some()
            || self.approval_visible()
        {
            return false;
        }
        let Some(text) = self.editor.text.get(start..end) else {
            return false;
        };
        let Some(images) = attachment::pasted_paths(text) else {
            return false;
        };
        self.editor.remove_range(start, end);
        self.attach_images(images);
        true
    }

    pub(super) fn paste_image_paths(&mut self, text: &str) -> bool {
        if let Some(images) = attachment::pasted_paths(text) {
            self.attach_images(images);
            true
        } else {
            false
        }
    }

    pub(super) fn image_command(&mut self, arg: &str) {
        if self.readonly {
            return;
        }
        match arg {
            "clear" => self.images.clear(),
            "" | "list" => {
                let names = self
                    .images
                    .iter()
                    .enumerate()
                    .map(|(i, image)| format!("{}. {}", i + 1, image.name()))
                    .collect::<Vec<_>>()
                    .join(" · ");
                self.notify(if names.is_empty() {
                    "이미지 첨부: Ctrl+V 또는 /image PATH".into()
                } else {
                    names
                });
            }
            arg if arg.starts_with("remove ") => {
                if let Some(index) = command_index(arg.trim_start_matches("remove ").trim())
                    && index < self.images.len()
                {
                    self.images.remove(index);
                } else {
                    self.error("사용법: /image remove N (1부터 시작)");
                }
            }
            path => {
                if !self.paste_image_paths(path) {
                    self.error(
                        "이미지 파일을 읽을 수 없습니다. PNG, JPEG, GIF, WebP 경로를 입력하세요.",
                    );
                }
            }
        }
    }

    pub(super) fn paint_images(&self, canvas: &mut Canvas, y: u16) {
        if self.images.is_empty() || self.readonly {
            return;
        }
        let mut x = 2;
        for (index, image) in self.images.iter().enumerate() {
            let remaining = canvas.width.saturating_sub(x + 2);
            let reserve = if index + 1 < self.images.len() { 10 } else { 0 };
            if remaining < 18 + reserve {
                canvas.button(
                    x,
                    y,
                    &format!("+{}개", self.images.len() - index),
                    Action::Command("/image list".into()),
                    false,
                );
                break;
            }
            let name = one_line(
                &image.name(),
                usize::from(remaining.saturating_sub(13 + reserve).min(26)),
            );
            x = canvas.button(
                x,
                y,
                &format!("이미지 · {name} ×"),
                Action::RemoveImage(index),
                false,
            );
        }
    }

    #[cfg(test)]
    pub(crate) fn fixture_image() -> ImageAttachment {
        ImageAttachment::from_clipboard(arboard::ImageData {
            width: 1,
            height: 1,
            bytes: vec![0, 0, 0, 255].into(),
        })
        .unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn images_stay_with_queued_message_and_retry_without_stealing_next_draft() {
        let mut app = App::new(false);
        app.thread = Some("main".into());
        app.busy = true;
        app.attach_images(vec![App::fixture_image()]);
        app.activate(Action::Send);
        assert_eq!(app.queued[0].images.len(), 1);
        assert!(app.images.is_empty());
        app.busy = false;
        let intent = app.next_queued().unwrap();
        assert!(
            matches!(app.agent_command(&intent), Some(AgentCommand::Submit { images, .. }) if images.len() == 1)
        );
        app.attach_images(vec![App::fixture_image()]);
        app.fail_submission("offline");
        assert_eq!(app.queued[0].images.len(), 1);
        assert_eq!(app.images.len(), 1);
        app.activate(Action::RemoveImage(0));
        assert!(app.images.is_empty());
        assert_eq!(app.queued[0].images.len(), 1);
    }
    #[test]
    fn accepted_clipboard_image_remains_readable_until_turn_finishes() {
        use crate::agent::{AgentEvent, AgentUpdate};
        let mut app = App::new(false);
        app.thread = Some("main".into());
        let image = App::fixture_image();
        let path = image.path().to_owned();
        app.attach_images(vec![image]);
        app.activate(Action::Send);
        app.apply_event(AgentEvent::scoped(
            "main",
            AgentUpdate::TurnSubmitted {
                id: Some("image-turn".into()),
                status: "inProgress".into(),
            },
        ));
        assert!(
            path.exists(),
            "runtime may load localImage after acknowledging turn/start"
        );
        app.apply_event(AgentEvent::scoped(
            "main",
            AgentUpdate::TurnCompleted {
                id: Some("image-turn".into()),
                status: "completed".into(),
                error: None,
            },
        ));
        assert!(!path.exists());
    }
}
