//! Search and price grouping stay in the picker, independently of the conversation draft.
use super::*;
use crate::agent::ModelCatalogEntry;

fn band(info: Option<&ModelCatalogEntry>) -> &'static str {
    match info {
        Some(p) if p.cost == "local" => "0로컬",
        Some(p) if p.cost == "free" => "1무료 API",
        Some(p) if p.cost == "subscription" => "2구독",
        Some(p) if p.cost == "unknown" => "6가격 미확인",
        Some(p) => match p.price {
            Some((_, output)) if output < 1.0 => "3저가 · 출력 $1 미만",
            Some((_, output)) if output < 10.0 => "4중간 · 출력 $1–10 미만",
            Some(_) => "5고가 · 출력 $10 이상",
            None => "6가격 미확인",
        },
        None => "6가격 미확인",
    }
}

fn dollars(value: f64) -> String {
    if value > 0.0 && value < 0.000_001 { return format!("{value:.2e}"); }
    format!("{value:.6}").trim_end_matches('0').trim_end_matches('.').to_owned()
}

impl App {
    pub fn open_provider_models(&mut self, id: &str) {
        self.model_search = Editor::default();
        self.model_search.insert(&format!("{id}/"));
        self.switch_view(View::Models);
        self.panel_open = true;
        self.model_catalog_loading = true;
        self.reset_model_search();
    }
    fn catalog_info(&self, id: &str) -> Option<&ModelCatalogEntry> {
        self.model_catalog.iter().find(|p| p.id == id)
    }
    /// The model's readable name: the catalog's when the id is a content hash (LMX).
    pub(super) fn model_display(&self, id: &str) -> String {
        self.catalog_info(id)
            .and_then(|info| info.name.clone())
            .unwrap_or_else(|| super::model_name_in_group(id).to_owned())
    }
    /// The readable name of a model a router reports as having answered: the catalog
    /// entry it names (by id, or by id without its endpoint), else the name as given.
    pub(super) fn served_display(&self, requested: &str, served: &str) -> String {
        // An LMX answers under another of its names; the selection names it.
        if requested.starts_with("lmx/") {
            return self.model_display(requested);
        }
        self.model_catalog
            .iter()
            .find(|p| p.id == served || p.id.split_once('/').is_some_and(|(_, rest)| rest == served))
            .map(|p| self.model_display(&p.id))
            .unwrap_or_else(|| served.to_owned())
    }
    fn model_matches(&self, model: &str) -> bool {
        let name = format!("{model} {}", self.model_display(model)).to_lowercase();
        self.model_search.text.split_whitespace().all(|word| name.contains(&word.to_lowercase()))
            && (!self.model_free_only || self.catalog_info(model).is_some_and(|p| matches!(p.cost.as_str(), "free" | "local")))
    }
    pub(super) fn reset_model_search(&mut self) {
        self.scroll = 0;
        self.follow = false;
        self.composer_active = false;
        self.focus = Some(Action::ModelSearch);
    }
    pub(super) fn paste_model_search(&mut self, text: &str) {
        self.model_search.insert(&text.replace(['\n', '\r'], " "));
        self.reset_model_search();
    }
    pub(super) fn model_search_key(&mut self, key: KeyEvent) -> bool {
        if self.view != View::Models || self.composer_active { return false; }
        let editing = self.focus == Some(Action::ModelSearch) || self.focus.is_none();
        if ctrl_char(key, 'a') && editing {
            self.model_search.select_all();
            self.focus = Some(Action::ModelSearch);
            return true;
        }
        if key.modifiers.intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) { return false; }
        match key.code {
            KeyCode::Char('/') => {
                self.model_command_draft = Some(std::mem::take(&mut self.editor));
                self.editor.insert("/");
                self.composer_active = true;
                self.focus = None;
                self.menu_dismissed = false;
                self.menu_index = 0;
                true
            }
            KeyCode::Char(c) => {
                self.model_search.insert(&c.to_string()); self.reset_model_search(); true
            }
            KeyCode::Backspace if editing => {
                self.model_search.backspace(); self.reset_model_search(); true
            }
            KeyCode::Delete if editing => {
                self.model_search.delete(); self.reset_model_search(); true
            }
            KeyCode::Left if editing => { self.model_search.left(); true }
            KeyCode::Right if editing => { self.model_search.right(); true }
            KeyCode::Home if editing => { self.model_search.home(); true }
            KeyCode::End if editing => { self.model_search.end(); true }
            KeyCode::Esc if !self.model_search.text.is_empty() => {
                self.model_search = Editor::default(); self.reset_model_search(); true
            }
            KeyCode::Enter if self.focus == Some(Action::ModelSearch) => {
                // Enter moves to results; choosing a model is still a deliberate action.
                self.focus = self.model_catalog_rows(80).into_iter().filter_map(|(_, _, action)| action)
                    .find(|action| matches!(action, Action::Model(_) | Action::ToggleModelGroup(_)));
                true
            }
            _ => false,
        }
    }
    pub(super) fn model_catalog_rows(&self, width: usize) -> Vec<(String, Tone, Option<Action>)> {
        let mut rows = vec![];
        let matched = self.models.iter().filter(|m| self.model_matches(m)).count();
        let (_, query, _) = self.search_field(width);
        rows.push((format!("┌─ 검색 {}┐", "─".repeat(width.saturating_sub(9))), Tone::Accent, None));
        rows.push((format!("│ > {query}{}│", " ".repeat(width.saturating_sub(5 + UnicodeWidthStr::width(query.as_str())))), Tone::Normal, Some(Action::ModelSearch)));
        rows.push((format!("└{}┘", "─".repeat(width.saturating_sub(2))), Tone::Faint, None));
        let price_groups = self.model_price_groups && !self.model_catalog.is_empty();
        rows.push((format!("묶음 · [{} ▾]", if price_groups {"가격별"} else {"공급자별"}), Tone::Normal, Some(Action::ModelGrouping)));
        rows.push((format!("{} 무료만", if self.model_free_only {"☑"} else {"☐"}), Tone::Normal, Some(Action::ModelFreeOnly)));
        if !self.model_search.text.is_empty() { rows.push(("검색 지우기".into(), Tone::Muted, Some(Action::ClearModelSearch))); }
        if self.model_free_only {
            rows.push(("자동 라우팅도 무료로 제한".into(), Tone::Accent, (!self.readonly).then(|| Action::Command("/provider policy free".into()))));
        }
        if price_groups {
            rows.push(("USD / 100만 토큰 · 출력 단가로 묶음".into(), Tone::Muted, None));
        }
        rows.push((format!("─ 검색 결과 {matched}개 / 전체 {}개", self.models.len()), Tone::Accent, None));
        if self.model_catalog_loading { rows.push(("사용 가능한 모델 조회 중…".into(), Tone::Muted, None)); }
        for source in &self.model_catalog_sources {
            if let Some(error) = &source.error {
                // Ollama is optional; when it is not running, its models are simply absent.
                if source.name == "ollama" { continue; }
                if !self.provider_accounts.iter().any(|a| a.account.id == source.name && a.account.enabled) && source.name != "connections" { continue; }
                rows.extend(wrap(&format!("{} · 조회 실패: {error}", source.name), width).into_iter().map(|s| (s, Tone::Warning, None)));
                rows.push(("다시 조회".into(), Tone::Accent, Some(Action::Command("/provider refresh".into()))));
            }
        }
        if matched == 0 && !self.models.is_empty() && !self.model_catalog_loading {
            rows.push(("일치하는 모델이 없습니다. 검색·필터를 지우세요.".into(), Tone::Muted, None));
        }
        let mut groups: BTreeMap<String, Vec<&String>> = BTreeMap::new();
        for model in self.models.iter().filter(|m| self.model_matches(m)) {
            if let Some(provider_group) = model_group(model) {
                // LMX keeps its own section in every grouping.
                let group = if price_groups && !model.starts_with("lmx/") { format!("가격\u{1}{}", band(self.catalog_info(model))) } else {provider_group};
                groups.entry(group).or_default().push(model);
            } else {
                rows.push((format!("{}{}", if self.model == *model {"● "} else {"○ "}, if model == "auto" {"자동 라우팅 (Loom)"} else {model}), Tone::Normal, Some(Action::Model(model.clone()))));
            }
        }
        let mut section = String::new();
        for (group, mut models) in groups {
            models.sort_by(|a, b| {
                let cost = |id: &str| self.catalog_info(id).and_then(|p| p.price).map(|(_, output)| output);
                cost(a).unwrap_or(f64::INFINITY).total_cmp(&cost(b).unwrap_or(f64::INFINITY)).then_with(|| a.cmp(b))
            });
            let (this_section, label) = group.split_once('\u{1}').unwrap_or(("", &group));
            let own_engine = |s: &str| s.starts_with('0');
            if this_section != section && (!price_groups || own_engine(this_section) || own_engine(&section)) {
                section = this_section.to_owned();
                rows.push((String::new(), Tone::Normal, None));
                let title = section.trim_start_matches(|c: char| c.is_ascii_digit());
                rows.push(if own_engine(&section) {
                    (format!("◆ {title}"), Tone::Accent, None)
                } else {
                    ((if title == "가격" {"가격별"} else {title}).into(), Tone::Muted, None)
                });
            }
            let label = if price_groups {label.trim_start_matches(|c: char| c.is_ascii_digit())} else {label};
            let selected = models.iter().any(|m| **m == self.model);
            // Loom's own engine is listed open: there are few such models and they come first.
            // A toggle flips the default.
            let open = selected || (!self.model_search.text.is_empty() && matched <= 30)
                || self.open_model_groups.contains(&group) != own_engine(this_section);
            rows.push((format!("{} {label} · {}", if open {"▾"} else {"▸"}, models.len()), if selected {Tone::Accent} else {Tone::Normal}, Some(Action::ToggleModelGroup(group.clone()))));
            if !open { continue; }
            for model in models {
                let name = if model.starts_with("lmx/") || !price_groups {self.model_display(model)} else {model.to_string()};
                rows.push((format!("  {}{}", if self.model == *model {"● "} else {"○ "}, one_line(&name, width.saturating_sub(4))), Tone::Normal, Some(Action::Model(model.clone()))));
                if let Some(info) = self.catalog_info(model) {
                    let price = match info.cost.as_str() {
                        "local" => "로컬".into(), "free" => "무료".into(), "subscription" => "구독".into(),
                        "unknown" => "전체 요금 미확인".into(),
                        _ => info.price.map(|(input, output)| format!("입력 ${} · 출력 ${}{}", dollars(input), dollars(output), if input == 0.0 && output == 0.0 {" · 토큰 외 요금 있음"} else {""})).unwrap_or_else(|| "가격 미확인".into()),
                    };
                    let engine = if model.starts_with("lmx/") {"LMX · "} else {""};
                    rows.extend(wrap(&format!("    {engine}{price}{}", if info.tools {""} else {" · 도구 미지원"}), width).into_iter().map(|s| (s, Tone::Muted, None)));
                }
            }
        }
        rows
    }
    /// Keep the insertion point in view for long model IDs and Unicode searches.
    fn search_field(&self, width: usize) -> (usize, String, usize) {
        let available = width.saturating_sub(6).max(1);
        if self.model_search.text.is_empty() {
            return (0, one_line("모델 이름·공급자 입력…", available), 0);
        }
        let text = &self.model_search.text;
        let cursor = self.model_search.cursor.min(text.len());
        let mut start = 0;
        while UnicodeWidthStr::width(&text[start..cursor]) >= available {
            let Some(next) = text[start..cursor].graphemes(true).next() else { break; };
            start += next.len();
        }
        (start, one_line(&text[start..], available), UnicodeWidthStr::width(&text[start..cursor]))
    }
    pub(super) fn paint_model_search(&self, canvas: &mut Canvas) {
        if self.view != View::Models || self.composer_active { return; }
        let Some(hit) = canvas.hits.iter().find(|h| h.action == Action::ModelSearch).cloned() else { return; };
        if self.focus != Some(Action::ModelSearch) { return; }
        let (start, visible, column) = self.search_field(usize::from(hit.width));
        canvas.cursor = Some((hit.x + 4 + column as u16, hit.y));
        if let Some(selection) = self.model_search.selection() {
            let mut x = hit.x + 4;
            for (offset, grapheme) in visible.grapheme_indices(true) {
                let width = UnicodeWidthStr::width(grapheme) as u16;
                if start + offset < selection.end && start + offset + grapheme.len() > selection.start {
                    for column in x..(x + width).min(canvas.width) {
                        canvas.cells[usize::from(hit.y) * usize::from(canvas.width) + usize::from(column)].background = crate::engine::Background::Selected;
                    }
                }
                x += width;
            }
        }
    }
}
