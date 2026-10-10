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
    cancelling: bool,
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
            cancelling: false,
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
            form.cancelling = false;
            form.dismissed = false;
            form.error = reason.into();
        }
    }
    pub(super) fn remote_answers(&mut self, id: &str, answers: Option<Vec<QuestionAnswer>>) -> Result<Option<Intent>, String> {
        let form = self.input_forms.iter_mut().find(|f| f.request.id == id)
            .ok_or_else(|| "이미 처리된 질문입니다. 최신 질문을 확인해주세요.".to_owned())?;
        if form.answering { return Err("이 질문의 답변을 처리 중입니다.".into()); }
        if let Some(answers) = answers {
            form.request.validate_answers(&answers).map_err(|e| e.to_string())?;
            form.answering = true;
            form.error.clear();
            Ok(Some(Intent::Answer(id.to_owned(), answers)))
        } else {
            form.answering = true;
            form.cancelling = true;
            form.error.clear();
            Ok(Some(Intent::CancelQuestions(id.to_owned())))
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
        self.focus = None;
        match action {
            Action::QuestionOption(index) => form.choose(*index),
            Action::QuestionMove(next) => form.move_question(*next),
            Action::DismissQuestion => form.dismissed = true,
            Action::CancelQuestions => {
                form.answering = true;
                form.cancelling = true;
                form.error.clear();
                return Some(Intent::CancelQuestions(form.request.id.clone()));
            }
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
            self.focus = None;
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
                KeyCode::Char('c') => self.activate(Action::Interrupt),
                KeyCode::Char('v') => Some(Intent::PasteClipboard),
                KeyCode::Char('x' | 'X') => self.question_action(&Action::CancelQuestions),
                _ => None,
            };
        }
        if key.code == KeyCode::Esc {
            self.input_forms[0].dismissed = true;
            self.focus = None;
            return None;
        }
        if self.input_forms[0].answering {
            return None;
        }
        let actions = [
            Action::QuestionMove(false),
            Action::QuestionMove(true),
            Action::SubmitAnswers,
            Action::DismissQuestion,
            Action::CancelQuestions,
        ];
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            let selected = self
                .focus
                .as_ref()
                .and_then(|f| actions.iter().position(|a| a == f));
            let next = if key.code == KeyCode::BackTab {
                selected.map_or(actions.len() - 1, |i| {
                    (i + actions.len() - 1) % actions.len()
                })
            } else {
                selected.map_or(0, |i| (i + 1) % actions.len())
            };
            self.focus = Some(actions[next].clone());
            return None;
        }
        if key.code == KeyCode::Enter
            && let Some(action) = self.focus.clone().filter(|f| actions.contains(f))
        {
            return self.question_action(&action);
        }
        // Typing and option navigation return keyboard focus to the question.
        self.focus = None;
        let form = &mut self.input_forms[0];
        let question = &form.request.questions[form.current];
        let count = question.options.len() + usize::from(question.allow_custom);
        match key.code {
            KeyCode::Up => form.focused_option = (form.focused_option + count - 1) % count,
            KeyCode::Down => form.focused_option = (form.focused_option + 1) % count,
            KeyCode::Enter
                if key
                    .modifiers
                    .intersects(KeyModifiers::ALT | KeyModifiers::SHIFT)
                    && form.custom() =>
            {
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
    pub(super) fn question_input_rows(&self, width: u16, height: u16) -> Option<u16> {
        if self.trust_prompt.is_some() || !self.approvals.is_empty() {
            return None;
        }
        let form = self.visible_form()?;
        let question = &form.request.questions[form.current];
        let prompt_rows = wrap(&question.prompt, usize::from(width.saturating_sub(4)))
            .len()
            .clamp(1, 3) as u16;
        let options =
            (question.options.len() + usize::from(question.allow_custom)).clamp(1, 6) as u16;
        // Use the same bottom composer allocation as approval decisions, keeping
        // room for the transcript even at the minimum terminal size.
        Some(
            (prompt_rows + options + 2 + u16::from(question.allow_custom))
                .min(height.saturating_sub(9)),
        )
    }
    pub(super) fn paint_questions(&self, canvas: &mut Canvas, top: u16, bottom: u16) {
        let Some(form) = self.visible_form() else {
            return;
        };
        let question = &form.request.questions[form.current];
        let width = canvas.width;
        let height = bottom - top + 2;
        let mut composer = Canvas::new(width, height);
        canvas.cursor = None;
        canvas.hits.retain(|hit| hit.y < top);
        composer.rule(
            0,
            &format!(
                "? 질문 · {} · {}/{}",
                question.header,
                form.current + 1,
                form.values.len()
            ),
        );
        let body_rows = usize::from(height.saturating_sub(4));
        let custom_rows = usize::from(form.custom());
        let prompt = wrap(&question.prompt, usize::from(width - 4));
        let prompt_rows = prompt
            .len()
            .min(3)
            .min(body_rows.saturating_sub(custom_rows + 1).max(1));
        for (index, line) in prompt.iter().take(prompt_rows).enumerate() {
            composer.text(2, index as u16 + 1, line, Tone::Normal);
        }
        let description_rows = usize::from(body_rows > prompt_rows + custom_rows + 1);
        let slots = body_rows.saturating_sub(prompt_rows + custom_rows + description_rows);
        let start = form.focused_option.saturating_sub(slots.saturating_sub(1));
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
            composer.button(
                2,
                1 + prompt_rows as u16 + offset as u16,
                &label,
                Action::QuestionOption(index),
                form.focused_option == index,
            );
        }
        if description_rows > 0 {
            let description = question
                .options
                .get(form.focused_option)
                .map(|o| o.description.as_str())
                .unwrap_or("문자를 입력하거나 붙여넣으세요.");
            composer.text(
                2,
                height - 4 - custom_rows as u16,
                &one_line(description, usize::from(width - 4)),
                Tone::Muted,
            );
        }
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
            let end = composer.text(2, height - 4, &format!("> {tail}"), Tone::User);
            if !form.answering && self.focus.is_none() {
                canvas.cursor = Some((end.min(width - 2), top + height - 4));
            }
        }
        if !form.answering {
            let mut x = composer.button(2, height - 3, "이전", Action::QuestionMove(false), false);
            x = composer.button(x, height - 3, "다음", Action::QuestionMove(true), false);
            x = composer.button(
                x,
                height - 3,
                if width < 60 {
                    "제출"
                } else {
                    "답변 제출"
                },
                Action::SubmitAnswers,
                false,
            );
            x = composer.button(x, height - 3, "나중에", Action::DismissQuestion, false);
            composer.button(x, height - 3, "취소", Action::CancelQuestions, false);
        }
        composer.rule(height - 2, "");
        composer.text(
            2,
            height - 1,
            &one_line(
                if form.cancelling {
                    "질문 취소 중…"
                } else if form.answering {
                    "답변 전송 중…"
                } else if !form.error.is_empty() {
                    &form.error
                } else if width < 60 {
                    "Tab 버튼 · Ctrl+X 취소 · Esc 중단/닫기"
                } else if width < 80 {
                    "↑↓ ↵ 선택 · Tab 버튼 · Ctrl+X 취소 · Esc 중단/나중에"
                } else {
                    "↑↓ 선택 · Enter 확인 · Tab/Shift+Tab 버튼 · Ctrl+X 질문 취소 · Esc 중단/나중에"
                },
                usize::from(width - 4),
            ),
            Tone::Warning,
        );
        canvas.blit(&composer, 0, top);
    }
    pub(super) fn question_visible(&self) -> bool {
        self.visible_form().is_some()
    }

    /// Keep question cancellation in the headless UI checks as well as unit tests.
    pub(crate) fn question_cancel_selfcheck(report: &mut crate::selfcheck::Report) {
        use crate::agent::{Question, QuestionOption};
        use crate::selfcheck::{Check, Dump};
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        const SCENARIO: &str = "question_cancel";
        for (width, height) in [(40, 12), (80, 24), (140, 40)] {
            let mut app = App::new(false);
            app.thread = Some("question-session".into());
            app.turn = Some("active-turn".into());
            app.busy = true;
            app.editor.insert("대화 초안");
            app.input_requested(InputRequest {
                id: "cancel-question".into(),
                blocking: true,
                questions: vec![Question {
                    id: "secret".into(),
                    header: "연결 정보".into(),
                    prompt: "연결 정보를 입력하세요.".into(),
                    options: vec![QuestionOption {
                        label: "기본값".into(),
                        description: "기본 연결".into(),
                    }],
                    allow_custom: true,
                    secret: true,
                }],
            });
            app.question_paste("private-cancel-fixture");
            let canvas = app.render(width, height);
            let hit = canvas
                .hits
                .iter()
                .find(|h| h.action == Action::CancelQuestions);
            report.checks.push(Check {
                scenario: SCENARIO,
                name: format!("{width}x{height} 취소 버튼 표시"),
                pass: hit.is_some_and(|h| {
                    h.x + h.width <= width && canvas.plain_line(h.y).contains(" 취소 ")
                }),
                detail: "최소 너비에서도 취소 글자와 클릭 영역을 모두 표시합니다.".into(),
            });
            if let Some(hit) = hit {
                app.mouse(
                    MouseEvent {
                        kind: MouseEventKind::Moved,
                        column: hit.x,
                        row: hit.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    &canvas,
                );
                let hovered = app.render(width, height);
                report.checks.push(Check {
                    scenario: SCENARIO,
                    name: format!("{width} 취소 버튼 호버"),
                    pass: (hit.x..hit.x + hit.width).all(|x| {
                        hovered.cells[usize::from(hit.y) * usize::from(width) + usize::from(x)].tone
                            == Tone::Hover
                    }),
                    detail: "취소 버튼의 최종 composer 셀에 Hover가 남습니다.".into(),
                });
                // A release outside the button must not cancel the question.
                app.mouse(
                    MouseEvent {
                        kind: MouseEventKind::Down(MouseButton::Left),
                        column: hit.x,
                        row: hit.y,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hovered,
                );
                let escaped = app.mouse(
                    MouseEvent {
                        kind: MouseEventKind::Up(MouseButton::Left),
                        column: 0,
                        row: 0,
                        modifiers: KeyModifiers::NONE,
                    },
                    &hovered,
                );
                report.checks.push(Check {
                    scenario: SCENARIO,
                    name: format!("{width} 취소 버튼 밖에서 놓기"),
                    pass: escaped.is_none() && !app.input_forms[0].answering,
                    detail: "버튼을 누른 뒤 밖에서 놓으면 취소하지 않습니다.".into(),
                });
            }
            app.key(
                KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
                &canvas,
            );
            let focused = app.render(width, height);
            report.checks.push(Check {
                scenario: SCENARIO,
                name: format!("{width} 취소 키보드 포커스"),
                pass: app.focus == Some(Action::CancelQuestions)
                    && focused.cursor.is_none()
                    && focused
                        .hits
                        .iter()
                        .find(|h| h.action == Action::CancelQuestions)
                        .is_some_and(|h| {
                            (h.x..h.x + h.width).all(|x| {
                                focused.cells
                                    [usize::from(h.y) * usize::from(width) + usize::from(x)]
                                .tone
                                    == Tone::Selected
                            })
                        }),
                detail: "Shift+Tab으로 취소에 접근하고 Selected로 표시합니다.".into(),
            });
            report.dumps.push(Dump {
                scenario: SCENARIO,
                label: format!("질문 취소 {width}x{height}"),
                width,
                height,
                rows: (0..height).map(|y| focused.plain_line(y)).collect(),
            });
            let intent = app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &focused);
            report.checks.push(Check {
                scenario: SCENARIO,
                name: format!("{width} 질문만 취소하고 작업과 대화 초안 유지"),
                pass: intent == Some(Intent::CancelQuestions("cancel-question".into()))
                    && app.turn.as_deref() == Some("active-turn")
                    && app.busy
                    && app.editor.text == "대화 초안"
                    && app.question_action(&Action::CancelQuestions).is_none(),
                detail: "취소 요청은 질문 ID만 전달하며 중복 요청을 보내지 않습니다.".into(),
            });
            app.input_resolved("cancel-question");
            report.checks.push(Check {
                scenario: SCENARIO,
                name: format!("{width} 취소 완료 후 비밀 폼 제거"),
                pass: app.input_forms.is_empty() && app.busy && app.editor.text == "대화 초안",
                detail: "InputResolved가 비밀 입력 폼을 제거하고 작업은 계속됩니다.".into(),
            });
        }
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
    fn question_cancel_selfcheck_covers_controls_and_resolution() {
        let mut report = crate::selfcheck::Report::default();
        App::question_cancel_selfcheck(&mut report);
        println!("{}", report.text(true));
        assert_eq!(report.failures(), 0);
        assert_eq!(report.checks.len(), 18);
    }
    #[test]
    fn cancel_targets_only_the_question_and_resolves_secret_drafts() {
        for blocking in [false, true] {
            let mut app = app(blocking, true);
            app.turn = Some("running-turn".into());
            app.paste("private-answer");
            let mut next = app.input_forms[0].request.clone();
            next.id = "next-question".into();
            app.input_requested(next);
            assert_eq!(
                app.activate(Action::CancelQuestions),
                Some(Intent::CancelQuestions("question".into()))
            );
            assert!(app.busy);
            assert_eq!(app.turn.as_deref(), Some("running-turn"));
            assert!(app.input_forms[0].answering);
            assert!(app.activate(Action::CancelQuestions).is_none());
            assert!(app.activate(Action::SubmitAnswers).is_none());
            app.apply_event(AgentEvent::scoped(
                "main",
                AgentUpdate::InputResolved("question".into()),
            ));
            assert_eq!(app.input_forms.len(), 1);
            assert_eq!(app.input_forms[0].request.id, "next-question");
            assert!(app.input_forms[0].editors[0].text.is_empty());
            assert_eq!(app.editor.text, "대화 초안");
            assert!(app.busy);
        }
    }
    #[test]
    fn mouse_cancel_activates_once_on_release_without_interrupting_the_turn() {
        use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
        let mut app = app(false, false);
        app.turn = Some("running-turn".into());
        let canvas = app.render(40, 12);
        let hit = canvas
            .hits
            .iter()
            .find(|h| h.action == Action::CancelQuestions)
            .unwrap();
        let event = |kind| MouseEvent {
            kind,
            column: hit.x,
            row: hit.y,
            modifiers: KeyModifiers::NONE,
        };
        assert!(
            app.mouse(event(MouseEventKind::Down(MouseButton::Left)), &canvas)
                .is_none()
        );
        assert!(!app.input_forms[0].answering);
        assert_eq!(
            app.mouse(event(MouseEventKind::Up(MouseButton::Left)), &canvas),
            Some(Intent::CancelQuestions("question".into()))
        );
        assert!(
            app.mouse(event(MouseEventKind::Up(MouseButton::Left)), &canvas)
                .is_none()
        );
        assert!(app.busy);
        assert_eq!(app.turn.as_deref(), Some("running-turn"));
    }
    #[test]
    fn cancel_failure_allows_retry_and_preserves_the_question_draft() {
        let mut app = app(true, true);
        app.paste("private-answer");
        app.activate(Action::CancelQuestions);
        let pending = app.render(40, 12);
        assert!(pending.plain_line(11).contains("질문 취소 중"));
        app.input_reply_failed("question", "cancel failed");
        assert_eq!(app.input_forms[0].editors[0].text, "private-answer");
        assert!(!app.input_forms[0].answering);
        let canvas = app.render(40, 12);
        assert!(canvas.plain_line(11).contains("cancel failed"));
        assert_eq!(
            app.activate(Action::CancelQuestions),
            Some(Intent::CancelQuestions("question".into()))
        );
    }
    #[test]
    fn narrow_cancel_is_fully_visible_and_keyboard_reachable() {
        for (width, height) in [(40, 12), (60, 20), (80, 24), (140, 40)] {
            let mut app = app(false, false);
            app.paste("질문 초안");
            let canvas = app.render(width, height);
            let hit = canvas
                .hits
                .iter()
                .find(|hit| hit.action == Action::CancelQuestions)
                .expect("cancel must remain visible at minimum terminal size");
            assert!(hit.x + hit.width <= width);
            assert!(canvas.plain_line(hit.y).contains("취소"));
            app.key(
                KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
                &canvas,
            );
            assert_eq!(app.focus, Some(Action::CancelQuestions));
            let focused = app.render(width, height);
            assert!(
                focused.cursor.is_none(),
                "button focus hides the input caret"
            );
            let hit = focused
                .hits
                .iter()
                .find(|hit| hit.action == Action::CancelQuestions)
                .unwrap();
            assert!((hit.x..hit.x + hit.width).all(|x| {
                focused.cells[usize::from(hit.y) * usize::from(width) + usize::from(x)].tone
                    == Tone::Selected
            }));
            assert_eq!(
                app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &focused),
                Some(Intent::CancelQuestions("question".into()))
            );
        }
        let mut app = app(false, true);
        app.turn = Some("running-turn".into());
        let canvas = app.render(40, 12);
        assert_eq!(
            app.key(
                KeyEvent::new(KeyCode::Char('x'), KeyModifiers::CONTROL),
                &canvas
            ),
            Some(Intent::CancelQuestions("question".into()))
        );
        assert_eq!(app.turn.as_deref(), Some("running-turn"));
        assert_eq!(app.editor.text, "대화 초안");
    }
    #[test]
    fn pasting_from_a_footer_button_returns_focus_to_the_question_editor() {
        let mut app = app(false, true);
        let canvas = app.render(80, 24);
        app.key(
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            &canvas,
        );
        assert_eq!(app.focus, Some(Action::CancelQuestions));
        app.paste("private-answer");
        assert!(app.focus.is_none());
        let canvas = app.render(80, 24);
        assert!(canvas.cursor.is_some());
        assert!(matches!(
            app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &canvas),
            Some(Intent::Answer(_, _))
        ));
    }
    #[test]
    fn tab_reaches_each_question_control_without_changing_answers() {
        let mut app = app(false, false);
        for expected in [
            Action::QuestionMove(false),
            Action::QuestionMove(true),
            Action::SubmitAnswers,
            Action::DismissQuestion,
            Action::CancelQuestions,
        ] {
            let canvas = app.render(40, 12);
            assert!(
                app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &canvas)
                    .is_none()
            );
            assert_eq!(app.focus, Some(expected));
            assert_eq!(app.input_forms[0].current, 0);
            assert!(app.input_forms[0].values[0].is_none());
        }
        let canvas = app.render(40, 12);
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &canvas);
        assert!(app.focus.is_none());
        assert_eq!(app.input_forms[0].focused_option, 1);
    }
    #[test]
    fn unanswered_questions_offer_cancel_at_narrow_widths() {
        let mut app = app(false, false);
        for width in [40, 48, 80, 120] {
            let canvas = app.render(width, 24);
            assert!(
                (0..canvas.height).any(|y| canvas.plain_line(y).contains(" 취소 ")),
                "{width}-column questions need an explicit Cancel button"
            );
        }
    }
    #[test]
    fn questions_replace_the_bottom_composer_and_preserve_transcript_controls() {
        let mut app = app(false, false);
        for (width, height) in [(40, 12), (60, 20), (80, 24), (140, 40)] {
            let canvas = app.render(width, height);
            let submit = canvas
                .hits
                .iter()
                .find(|hit| hit.action == Action::SubmitAnswers)
                .expect("question controls must remain visible at the minimum size");
            assert_eq!(submit.y, height - 3, "question replaces the docked input");
            assert!(!canvas.hits.iter().any(|hit| hit.action == Action::Input));
            assert!(canvas.hits.iter().any(|hit| hit.y < 3));
            assert!(canvas.plain_line(height - 1).contains("Esc"));
        }
        app.activate(Action::DismissQuestion);
        let canvas = app.render(80, 24);
        assert!(canvas.hits.iter().any(|hit| hit.action == Action::Input));
        assert_eq!(app.editor.text, "대화 초안");
    }
    #[test]
    fn escape_interrupts_with_question_visible_and_preserves_both_drafts() {
        let mut app = app(false, false);
        app.turn = Some("turn".into());
        app.paste("질문 초안");
        let canvas = app.render(80, 24);
        assert_eq!(
            app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &canvas),
            Some(Intent::Interrupt)
        );
        // Keep the question state until the runtime confirms interruption;
        // answering later remains possible if the interrupt fails.
        assert!(app.question_visible());
        assert_eq!(app.status, "중단됨");
        let canvas = app.render(80, 24);
        assert_eq!(
            app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &canvas),
            None,
            "repeated Escape must not send duplicate interrupts"
        );
        app.activate(Action::Questions);
        assert_eq!(app.input_forms[0].editors[0].text, "질문 초안");
        assert_eq!(app.editor.text, "대화 초안");
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
