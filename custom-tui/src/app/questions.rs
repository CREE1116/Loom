//! Question forms own their drafts independently from the conversation composer.
use super::*;
use crate::agent::{AnswerValue, InputRequest, QuestionAnswer};

pub struct InputForm {
    pub request: InputRequest,
    current: usize,
    focused_option: usize,
    values: Vec<Option<AnswerValue>>,
    editors: Vec<Editor>,
    pub answering: bool,
    dismissed: bool,
    error: String,
}
impl InputForm {
    fn new(request: InputRequest) -> Self {
        let count = request.questions.len();
        Self {
            request,
            current: 0,
            focused_option: 0,
            values: vec![None; count],
            editors: (0..count).map(|_| Editor::default()).collect(),
            answering: false,
            dismissed: false,
            error: String::new(),
        }
    }
    fn answers(&self) -> Option<Vec<QuestionAnswer>> {
        self.request
            .questions
            .iter()
            .zip(&self.values)
            .map(|(question, value)| {
                Some(QuestionAnswer {
                    question_id: question.id.clone(),
                    value: value.clone()?,
                })
            })
            .collect()
    }
    fn custom(&self) -> bool {
        self.focused_option == self.request.questions[self.current].options.len()
            && self.request.questions[self.current].allow_custom
    }
    fn choose(&mut self, index: usize) {
        let question = &self.request.questions[self.current];
        if index < question.options.len() {
            self.focused_option = index;
            self.values[self.current] = Some(AnswerValue::Option(index));
        } else if index == question.options.len() && question.allow_custom {
            self.focused_option = index;
            self.values[self.current] = (!self.editors[self.current].text.trim().is_empty())
                .then(|| AnswerValue::Text(self.editors[self.current].text.clone()));
        }
        self.error.clear();
    }
    fn move_question(&mut self, next: bool) {
        self.current = if next {
            (self.current + 1) % self.values.len()
        } else {
            (self.current + self.values.len() - 1) % self.values.len()
        };
        self.focused_option = match &self.values[self.current] {
            Some(AnswerValue::Option(index)) => *index,
            Some(AnswerValue::Text(_)) => self.request.questions[self.current].options.len(),
            None => 0,
        };
    }
}

impl App {
    pub(super) fn input_requested(&mut self, request: InputRequest) {
        if request
            .questions
            .iter()
            .any(|q| q.options.is_empty() && !q.allow_custom)
        {
            self.error("질문에 선택지 또는 직접 입력이 필요합니다.");
            return;
        }
        if self.readonly
            || request.questions.is_empty()
            || self.input_forms.iter().any(|f| f.request.id == request.id)
        {
            return;
        }
        self.input_forms.push(InputForm::new(request));
        self.focus = None;
        self.menu_open = false;
    }
    pub(super) fn input_resolved(&mut self, id: &str) {
        self.input_forms.retain(|form| form.request.id != id);
    }
    pub fn input_reply_failed(&mut self, id: &str, reason: &str) {
        if let Some(form) = self.input_forms.iter_mut().find(|f| f.request.id == id) {
            form.answering = false;
            form.dismissed = false;
            form.error = reason.into();
        }
    }
    fn visible_form(&self) -> Option<&InputForm> {
        self.input_forms.first().filter(|f| !f.dismissed)
    }
    pub(super) fn question_action(&mut self, action: &Action) -> Option<Intent> {
        if matches!(action, Action::Questions) {
            if let Some(form) = self.input_forms.first_mut() {
                form.dismissed = false;
            }
            return None;
        }
        let form = self.input_forms.first_mut()?;
        if form.answering {
            return None;
        }
        match action {
            Action::QuestionOption(index) => form.choose(*index),
            Action::QuestionMove(next) => form.move_question(*next),
            Action::DismissQuestion => form.dismissed = true,
            Action::SubmitAnswers => {
                let Some(answers) = form.answers() else {
                    form.error =
                        "모든 질문에 답해주세요. 기본 선택은 자동 제출되지 않습니다.".into();
                    return None;
                };
                if let Err(error) = form.request.validate_answers(&answers) {
                    form.error = error.to_string();
                    return None;
                }
                form.answering = true;
                return Some(Intent::Answer(form.request.id.clone(), answers));
            }
            _ => {}
        }
        None
    }
    pub(super) fn question_paste(&mut self, text: &str) -> bool {
        if self.visible_form().is_none() {
            return false;
        }
        let form = &mut self.input_forms[0];
        if !form.answering && form.request.questions[form.current].allow_custom {
            if form.editors[form.current].text.len() + text.len() <= 16_384 {
                form.editors[form.current].paste(text);
                form.choose(form.request.questions[form.current].options.len());
            } else {
                form.error = "입력은 16384바이트까지 가능합니다.".into();
            }
        }
        true
    }
    pub(super) fn question_key(&mut self, key: KeyEvent) -> Option<Intent> {
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return match key.code {
                KeyCode::Char('q') => Some(Intent::Quit),
                KeyCode::Char('c') => Some(Intent::Interrupt),
                KeyCode::Char('v') => Some(Intent::PasteClipboard),
                _ => None,
            };
        }
        let form = &mut self.input_forms[0];
        if key.code == KeyCode::Esc {
            form.dismissed = true;
            return None;
        }
        if form.answering {
            return None;
        }
        let question = &form.request.questions[form.current];
        let count = question.options.len() + usize::from(question.allow_custom);
        match key.code {
            KeyCode::Up => form.focused_option = (form.focused_option + count - 1) % count,
            KeyCode::Down => form.focused_option = (form.focused_option + 1) % count,
            KeyCode::Tab => form.move_question(true),
            KeyCode::BackTab => form.move_question(false),
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) && form.custom() => {
                form.editors[form.current].insert("\n");
                form.choose(form.focused_option);
            }
            KeyCode::Enter => {
                form.choose(form.focused_option);
                if form.values[form.current].is_none() {
                    form.error = "답변을 선택하거나 직접 입력해주세요.".into();
                } else if form.current + 1 < form.values.len() {
                    form.move_question(true);
                } else {
                    return self.question_action(&Action::SubmitAnswers);
                }
            }
            KeyCode::Char(ch) if question.allow_custom => {
                if form.editors[form.current].text.len() + ch.len_utf8() <= 16_384 {
                    form.editors[form.current].insert(&ch.to_string());
                    form.choose(question.options.len());
                }
            }
            KeyCode::Backspace
            | KeyCode::Delete
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Home
            | KeyCode::End
                if form.custom() =>
            {
                let editor = &mut form.editors[form.current];
                match key.code {
                    KeyCode::Backspace => editor.backspace(),
                    KeyCode::Delete => editor.delete(),
                    KeyCode::Left => editor.left(),
                    KeyCode::Right => editor.right(),
                    KeyCode::Home => editor.cursor = 0,
                    KeyCode::End => editor.cursor = editor.text.len(),
                    _ => {}
                }
                form.choose(form.focused_option);
            }
            _ => {}
        }
        None
    }
    pub(super) fn paint_questions(&self, canvas: &mut Canvas) {
        let Some(form) = self.visible_form() else {
            return;
        };
        let question = &form.request.questions[form.current];
        let width = canvas.width.saturating_sub(4).min(88);
        let height = canvas.height.saturating_sub(4).min(20);
        if width < 18 || height < 8 {
            return;
        }
        let x = (canvas.width - width) / 2;
        let y = (canvas.height - height) / 2;
        let mut popup = Canvas::new(width, height);
        for cell in &mut popup.cells {
            cell.background = crate::engine::Background::Code;
        }
        popup.rule(
            0,
            &format!(
                "? 질문 · {} · {}/{}",
                question.header,
                form.current + 1,
                form.values.len()
            ),
        );
        for (index, line) in wrap(&question.prompt, usize::from(width - 4))
            .iter()
            .take(3)
            .enumerate()
        {
            popup.text(2, index as u16 + 1, line, Tone::Normal);
        }
        let slots = usize::from(height.saturating_sub(9)).max(1);
        let start = form.focused_option.saturating_sub(slots - 1);
        let count = question.options.len() + usize::from(question.allow_custom);
        for (offset, index) in (start..count).take(slots).enumerate() {
            let label = if let Some(option) = question.options.get(index) {
                format!(
                    "{} {}",
                    if form.values[form.current] == Some(AnswerValue::Option(index)) {
                        "✓"
                    } else {
                        "○"
                    },
                    option.label
                )
            } else {
                "직접 입력".into()
            };
            popup.button(
                2,
                4 + offset as u16,
                &label,
                Action::QuestionOption(index),
                form.focused_option == index,
            );
        }
        let description = question
            .options
            .get(form.focused_option)
            .map(|o| o.description.as_str())
            .unwrap_or("문자를 입력하거나 붙여넣으세요.");
        popup.text(
            2,
            height - 5,
            &one_line(description, usize::from(width - 4)),
            Tone::Muted,
        );
        if form.custom() {
            let editor = &form.editors[form.current];
            let text = if question.secret {
                "•".repeat(editor.text.graphemes(true).count())
            } else {
                editor.text.replace('\n', " ↵ ")
            };
            let tail: String = text
                .graphemes(true)
                .rev()
                .take(usize::from(width - 6) / 2)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let end = popup.text(2, height - 4, &format!("> {tail}"), Tone::User);
            if !form.answering {
                canvas.cursor = Some((x + end.min(width - 2), y + height - 4));
            }
        } else {
            canvas.cursor = None;
        }
        popup.text(
            2,
            height - 3,
            if form.answering {
                "답변 전송 중…"
            } else if !form.error.is_empty() {
                &form.error
            } else {
                "↑↓ 선택 · Enter 확인 · Tab 다음 질문 · Esc 나중에"
            },
            Tone::Warning,
        );
        if !form.answering {
            popup.button(2, height - 2, "이전", Action::QuestionMove(false), false);
            popup.button(12, height - 2, "다음", Action::QuestionMove(true), false);
            popup.button(22, height - 2, "답변 제출", Action::SubmitAnswers, false);
            popup.button(38, height - 2, "나중에", Action::DismissQuestion, false);
        }
        // The card blocks underlying clicks, including blank areas.
        canvas.hits.clear();
        canvas.hits.push(crate::engine::Hit {
            x,
            y,
            width,
            height,
            action: Action::Questions,
        });
        canvas.blit(&popup, x, y);
    }
    pub(super) fn question_visible(&self) -> bool {
        self.visible_form().is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentEvent, AgentUpdate, Question, QuestionOption};
    fn app(blocking: bool, secret: bool) -> App {
        let mut app = App::new(false);
        app.thread = Some("main".into());
        app.busy = true;
        app.editor.insert("대화 초안");
        app.apply_event(AgentEvent::scoped(
            "main",
            AgentUpdate::InputRequested(InputRequest {
                id: "question".into(),
                blocking,
                questions: vec![Question {
                    id: "q".into(),
                    header: "테스트".into(),
                    prompt: "기능을 선택하세요".into(),
                    options: vec![QuestionOption {
                        label: "TUI".into(),
                        description: "화면을 구현합니다".into(),
                    }],
                    allow_custom: true,
                    secret,
                }],
            }),
        ));
        app
    }
    #[test]
    fn explicit_answers_preserve_drafts_and_retry_without_double_submission() {
        let mut app = app(true, false);
        assert!(!app.is_working());
        assert!(app.activate(Action::SubmitAnswers).is_none());
        assert!(!app.input_forms[0].error.is_empty());
        app.activate(Action::QuestionOption(0));
        let intent = app.activate(Action::SubmitAnswers).unwrap();
        assert!(
            matches!(intent, Intent::Answer(_, ref answers) if answers[0].value == AnswerValue::Option(0))
        );
        assert!(app.activate(Action::SubmitAnswers).is_none());
        app.input_reply_failed("question", "connection failed");
        assert!(app.activate(Action::SubmitAnswers).is_some());
        app.input_resolved("question");
        assert_eq!(app.editor.text, "대화 초안");
        assert!(app.is_working());
    }
    #[test]
    fn secret_input_masking_hide_reopen_and_unicode_editing() {
        let mut app = app(false, true);
        assert!(
            app.is_working(),
            "nonblocking question keeps the animation running"
        );
        let canvas = app.render(80, 24);
        for ch in "비밀🙂".chars() {
            app.key(
                KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                &canvas,
            );
        }
        let masked = app.render(80, 24);
        let screen = (0..masked.height)
            .map(|y| masked.plain_line(y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!screen.contains("비밀") && !screen.contains('🙂'));
        assert!(screen.contains('•'));
        app.key(
            KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE),
            &masked,
        );
        app.activate(Action::DismissQuestion);
        assert!(!app.question_visible());
        app.activate(Action::Questions);
        assert!(app.question_visible());
        let Intent::Answer(_, answers) = app.activate(Action::SubmitAnswers).unwrap() else {
            panic!()
        };
        assert_eq!(answers[0].value, AnswerValue::Text("비밀".into()));
        assert_eq!(app.editor.text, "대화 초안");
    }
    #[test]
    fn stale_session_input_is_ignored_and_custom_paste_is_preserved() {
        let mut app = app(true, false);
        app.apply_event(AgentEvent::scoped(
            "other",
            AgentUpdate::InputResolved("question".into()),
        ));
        assert_eq!(app.input_forms.len(), 1);
        app.paste("한글\nsecond line");
        let Intent::Answer(_, answers) = app.activate(Action::SubmitAnswers).unwrap() else {
            panic!()
        };
        assert_eq!(
            answers[0].value,
            AnswerValue::Text("한글\nsecond line".into())
        );
        for (width, height) in [(40, 12), (60, 18), (120, 32)] {
            app.render(width, height);
        }
        assert_eq!(app.editor.text, "대화 초안");
    }
}
