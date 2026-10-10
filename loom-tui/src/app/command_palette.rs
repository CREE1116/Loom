//! Search commands independently of the message draft, including its selection.
use super::*;

impl App {
    pub(super) fn command_search_field(&self, width: usize) -> (String, usize) {
        let available = width.saturating_sub(6).max(1);
        let text = &self.command_search.text;
        if text.is_empty() {
            let placeholder = if self.preferences.language == Language::English {
                "Name or description…"
            } else {
                "이름이나 설명으로 검색…"
            };
            return (one_line(placeholder, available), 0);
        }
        let cursor = self.command_search.cursor.min(text.len());
        let mut start = 0;
        while UnicodeWidthStr::width(&text[start..cursor]) >= available {
            let Some(next) = text[start..cursor].graphemes(true).next() else {
                break;
            };
            start += next.len();
        }
        (
            one_line(&text[start..], available),
            UnicodeWidthStr::width(&text[start..cursor]),
        )
    }

    pub(super) fn command_palette_key(&mut self, key: KeyEvent) -> Option<Intent> {
        if ctrl_char(key, 'c') {
            return self.activate(Action::Interrupt);
        }
        if ctrl_char(key, 'v') {
            return Some(Intent::PasteClipboard);
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            if ctrl_char(key, 'a') {
                self.command_search.select_all();
            }
            return None;
        }
        let menu = self.menu_items();
        match key.code {
            KeyCode::Esc => {
                self.menu_open = false;
                self.menu_dismissed = true;
                self.command_search = Editor::default();
            }
            KeyCode::Up | KeyCode::BackTab if !menu.is_empty() => {
                self.menu_index =
                    (self.menu_index.min(menu.len() - 1) + menu.len() - 1) % menu.len();
            }
            KeyCode::Down if !menu.is_empty() => {
                self.menu_index = (self.menu_index + 1) % menu.len();
            }
            KeyCode::Tab | KeyCode::Enter if !menu.is_empty() => {
                if let Action::Command(command) =
                    menu[self.menu_index.min(menu.len() - 1)].2.clone()
                {
                    return self.select_palette_command(command, key.code == KeyCode::Tab);
                }
            }
            KeyCode::Char(character) => {
                self.command_search.insert(&character.to_string());
                self.menu_index = 0;
            }
            KeyCode::Backspace => {
                self.command_search.backspace();
                self.menu_index = 0;
            }
            KeyCode::Delete => {
                self.command_search.delete();
                self.menu_index = 0;
            }
            KeyCode::Left => self.command_search.left(),
            KeyCode::Right => self.command_search.right(),
            KeyCode::Home => self.command_search.home(),
            KeyCode::End => self.command_search.end(),
            _ => {}
        }
        None
    }

    pub(super) fn select_palette_command(
        &mut self,
        command: String,
        complete: bool,
    ) -> Option<Intent> {
        self.menu_open = false;
        self.menu_dismissed = true;
        self.menu_index = 0;
        self.command_search = Editor::default();
        if complete || COMMANDS_WITH_ARGUMENT.contains(&command.as_str()) {
            // Existing command-composer lifecycle restores this exact draft on Esc
            // or after the command, including cursor and selection.
            if self.model_command_draft.is_none() {
                self.model_command_draft = Some(std::mem::take(&mut self.editor));
            } else {
                self.editor = Editor::default();
            }
            self.editor.insert(&format!("{command} "));
            self.composer_active = true;
            self.focus = None;
            return None;
        }
        // Preserve drafts beginning with '/' too: finish_command consumes commands.
        let saved_draft = self.model_command_draft.take();
        let draft = std::mem::take(&mut self.editor);
        let intent = self.command(&command);
        self.editor = saved_draft.unwrap_or(draft);
        intent
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(app: &mut App, code: KeyCode) -> Option<Intent> {
        let canvas = app.render(80, 30);
        app.key(KeyEvent::new(code, KeyModifiers::NONE), &canvas)
    }

    #[test]
    fn palette_handles_empty_results_paste_and_explicit_execution_without_losing_draft() {
        let mut app = App::new(false);
        app.editor.insert("/draft 그대로");
        app.editor.cursor = 1;
        app.editor.anchor = Some(4);
        app.activate(Action::Menu);
        app.paste("unmatched-command");
        assert!(app.menu_items().is_empty());
        assert_eq!(key(&mut app, KeyCode::Enter), None);
        assert_eq!(app.editor.text, "/draft 그대로");
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.editor.cursor, 1);
        assert_eq!(app.editor.anchor, Some(4));
        app.activate(Action::Menu);
        app.paste("sessions");
        assert_eq!(key(&mut app, KeyCode::Enter), Some(Intent::RefreshSessions));
        assert_eq!(app.view, View::Sessions);
        assert_eq!(app.editor.text, "/draft 그대로");
        assert_eq!(app.editor.cursor, 1);
        assert_eq!(app.editor.anchor, Some(4));
    }

    #[test]
    fn argument_commands_complete_and_cancel_restores_exact_draft() {
        let mut app = App::new(false);
        app.editor.insert("계속 작성");
        app.editor.cursor = 0;
        app.activate(Action::Menu);
        assert_eq!(app.activate(Action::Command("/plan".into())), None);
        assert_eq!(app.editor.text, "/plan ");
        key(&mut app, KeyCode::Esc);
        assert_eq!(app.editor.text, "계속 작성");
        assert_eq!(app.editor.cursor, 0);
        assert!(app.model_command_draft.is_none());
    }

    #[test]
    fn escape_still_stops_an_active_turn_while_palette_is_open() {
        let mut app = App::new(false);
        app.thread = Some("main".into());
        app.turn = Some("running".into());
        app.busy = true;
        app.editor.insert("保留할 초안");
        app.activate(Action::Menu);
        assert_eq!(key(&mut app, KeyCode::Esc), Some(Intent::Interrupt));
        assert_eq!(app.editor.text, "保留할 초안");
    }

    #[test]
    fn reopening_palette_during_argument_input_restores_the_original_draft() {
        let mut app = App::new(false);
        app.editor.insert("원래 요청");
        app.editor.cursor = 0;
        app.activate(Action::Menu);
        app.activate(Action::Command("/plan".into()));
        app.activate(Action::Menu);
        app.activate(Action::Command("/help".into()));
        assert_eq!(app.editor.text, "원래 요청");
        assert_eq!(app.editor.cursor, 0);
        assert!(app.model_command_draft.is_none());
    }
}
