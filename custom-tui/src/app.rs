//! Conversation state and actions. The renderer never calls Codex directly.
pub use crate::agent::{Agent, Approval, Choice, Entry, Kind, SessionSummary, Skill};
use crate::agent::{AgentCommand, Operation};
use crate::document;
use crate::engine::{Action, Canvas, Tone, wrap};
use crate::preferences::{self, Language, Preferences};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod activity;
mod events;
mod meter;
mod models;
mod questions;
#[cfg(test)]
mod test_protocol;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Default, Debug)]
pub struct Editor {
    pub text: String,
    pub cursor: usize,
    // Byte ranges in the original text; the composer paints them as atomic chips.
    pasted: Vec<std::ops::Range<usize>>,
}
impl Editor {
    pub fn insert(&mut self, text: &str) {
        let normalized = text.replace('\r', "");
        if normalized.is_empty() {
            return;
        }
        self.snap_cursor_forward();
        let at = self.cursor;
        self.shift_pastes(at, normalized.len());
        self.text.insert_str(at, &normalized);
        self.cursor += normalized.len();
    }
    pub fn paste(&mut self, text: &str) {
        let normalized = text.replace('\r', "");
        if normalized.is_empty() {
            return;
        }
        self.snap_cursor_forward();
        let at = self.cursor;
        self.shift_pastes(at, normalized.len());
        self.text.insert_str(at, &normalized);
        self.cursor += normalized.len();
        self.pasted.push(at..self.cursor);
        self.pasted.sort_by_key(|range| range.start);
    }
    fn shift_pastes(&mut self, at: usize, len: usize) {
        for range in &mut self.pasted {
            if range.start >= at {
                range.start += len;
                range.end += len;
            }
        }
    }
    fn snap_cursor_forward(&mut self) {
        if let Some(range) = self
            .pasted
            .iter()
            .find(|r| r.start < self.cursor && self.cursor < r.end)
        {
            self.cursor = range.end;
        }
    }
    fn remove_range(&mut self, start: usize, end: usize) {
        self.text.replace_range(start..end, "");
        self.pasted.retain(|r| r.end <= start || r.start >= end);
        for range in &mut self.pasted {
            if range.start >= end {
                range.start -= end - start;
                range.end -= end - start;
            }
        }
        self.cursor = start;
    }
    pub fn take_text(&mut self) -> String {
        self.cursor = 0;
        self.pasted.clear();
        std::mem::take(&mut self.text)
    }
    /// Display text and the byte offset of the visible cursor. Original text
    /// remains untouched for submission and copying.
    pub fn display(&self) -> (String, usize) {
        let mut output = String::new();
        let mut raw_start = 0;
        let mut visible_cursor = None;
        for range in &self.pasted {
            if range.start > self.text.len() || range.end > self.text.len() {
                continue;
            }
            if self.cursor <= range.start && visible_cursor.is_none() {
                visible_cursor = Some(output.len() + self.text[raw_start..self.cursor].len());
            }
            output.push_str(&self.text[raw_start..range.start]);
            let placeholder = format!(
                "[붙여넣은 내용 · {}자]",
                self.text[range.clone()].chars().count()
            );
            if self.cursor > range.start && self.cursor <= range.end && visible_cursor.is_none() {
                visible_cursor = Some(output.len() + placeholder.len());
            }
            output.push_str(&placeholder);
            raw_start = range.end;
        }
        if visible_cursor.is_none() {
            visible_cursor = Some(output.len() + self.cursor.saturating_sub(raw_start));
        }
        output.push_str(&self.text[raw_start..]);
        (output, visible_cursor.unwrap_or(0))
    }
    fn move_to_visible(&mut self, visible: usize) {
        let mut raw_start = 0;
        let mut shown_start = 0;
        for range in &self.pasted {
            let plain_len = range.start - raw_start;
            if visible < shown_start + plain_len {
                self.cursor = raw_start + visible - shown_start;
                return;
            }
            shown_start += plain_len;
            let chip_len = format!(
                "[붙여넣은 내용 · {}자]",
                self.text[range.clone()].chars().count()
            )
            .len();
            if visible < shown_start + chip_len {
                self.cursor = if visible <= shown_start + chip_len / 2 {
                    range.start
                } else {
                    range.end
                };
                return;
            }
            shown_start += chip_len;
            raw_start = range.end;
        }
        self.cursor = (raw_start + visible.saturating_sub(shown_start)).min(self.text.len());
    }
    pub fn home(&mut self) {
        let (display, cursor) = self.display();
        self.move_to_visible(display[..cursor].rfind('\n').map_or(0, |p| p + 1));
    }
    pub fn end(&mut self) {
        let (display, cursor) = self.display();
        self.move_to_visible(
            display[cursor..]
                .find('\n')
                .map_or(display.len(), |p| cursor + p),
        );
    }
    pub fn left(&mut self) {
        if let Some(range) = self
            .pasted
            .iter()
            .find(|r| r.start < self.cursor && self.cursor <= r.end)
        {
            self.cursor = range.start;
            return;
        }
        if let Some((index, _)) = self.text[..self.cursor].grapheme_indices(true).next_back() {
            self.cursor = index;
        }
    }
    pub fn right(&mut self) {
        if let Some(range) = self
            .pasted
            .iter()
            .find(|r| r.start <= self.cursor && self.cursor < r.end)
        {
            self.cursor = range.end;
            return;
        }
        if let Some(g) = self.text[self.cursor..].graphemes(true).next() {
            self.cursor += g.len();
        }
    }
    pub fn backspace(&mut self) {
        let end = self.cursor;
        self.left();
        self.remove_range(self.cursor, end);
    }
    pub fn delete(&mut self) {
        let start = self.cursor;
        self.right();
        let end = self.cursor;
        self.remove_range(start, end);
    }
    pub fn vertical(&mut self, down: bool) {
        let (display, cursor) = self.display();
        let start = display[..cursor].rfind('\n').map_or(0, |p| p + 1);
        let col = display[start..cursor].graphemes(true).count();
        let end = display[cursor..]
            .find('\n')
            .map_or(display.len(), |p| cursor + p);
        let (target_start, target_end) = if down {
            if end == display.len() {
                return;
            }
            let next = end + 1;
            (
                next,
                display[next..]
                    .find('\n')
                    .map_or(display.len(), |p| next + p),
            )
        } else {
            if start == 0 {
                return;
            }
            let prev_end = start - 1;
            (
                display[..prev_end].rfind('\n').map_or(0, |p| p + 1),
                prev_end,
            )
        };
        let target = target_start
            + display[target_start..target_end]
                .grapheme_indices(true)
                .nth(col)
                .map_or(target_end - target_start, |(i, _)| i);
        self.move_to_visible(target);
    }
}
struct CachedMessage {
    body: String,
    rows: Vec<document::StyledRow>,
}
type MessageCache = RefCell<HashMap<(String, usize), CachedMessage>>;
type ContentRow = (String, Tone, Option<Action>);
struct CachedChatRows {
    width: usize,
    count: usize,
    tail_len: usize,
    language: Language,
    rows: Rc<Vec<ContentRow>>,
}
#[derive(Clone, Debug)]
pub struct QueuedMessage {
    pub effort: Option<String>,
    pub text: String,
    pub skills: Vec<Skill>,
    pub model: String,
}
fn one_line(text: &str, width: usize) -> String {
    let mut result = String::new();
    let mut used = 0;
    for part in text.split_whitespace() {
        let needed = UnicodeWidthStr::width(part) + usize::from(!result.is_empty());
        if used + needed > width {
            if result.is_empty() {
                for grapheme in part.graphemes(true) {
                    let g_width = UnicodeWidthStr::width(grapheme);
                    if used + g_width + 1 > width {
                        break;
                    }
                    result.push_str(grapheme);
                    used += g_width;
                }
            }
            result.push('…');
            break;
        }
        if !result.is_empty() {
            result.push(' ');
        }
        result.push_str(part);
        used += needed;
    }
    if result.is_empty() {
        "제목 없는 대화".into()
    } else {
        result
    }
}

// Numbered commands always use 1 for the first item shown to the user.
fn command_index(text: &str) -> Option<usize> {
    text.parse::<usize>().ok()?.checked_sub(1)
}

fn last_activity(timestamp: Option<u64>) -> String {
    let Some(timestamp) = timestamp else {
        return "".into();
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let elapsed = now.saturating_sub(timestamp);
    match elapsed {
        0..=59 => "방금 전".into(),
        60..=3599 => format!("{}분 전", elapsed / 60),
        3600..=86399 => format!("{}시간 전", elapsed / 3600),
        _ => format!("{}일 전", elapsed / 86400),
    }
}

/// Count only paths explicitly included in Codex's reported diff. This does
/// not inspect Git status or imply a complete working-tree inventory.
fn reported_file_count(diff: &str) -> usize {
    let mut files = HashSet::new();
    for line in diff.lines() {
        if let Some(file) = line.strip_prefix("diff --git ") {
            if let Some(path) = file.rsplit_once(" b/").map(|(_, path)| path) {
                files.insert(path.to_owned());
            }
        } else if let Some(path) = line.strip_prefix("+++ ")
            && path != "/dev/null"
        {
            files.insert(path.trim_start_matches("b/").to_owned());
        }
    }
    files.len().max(1)
}
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum View {
    Chat,
    Agents,
    Diff,
    Approvals,
    Help,
    Skills,
    Models,
    Sessions,
    Settings,
    Permissions,
}
const COMMANDS: &[(&str, &str)] = &[
    ("/help", "조작과 명령 안내"),
    ("/settings", "UI 언어와 작업 패널"),
    ("/skills", "설치된 스킬 선택"),
    (
        "/permissions",
        "권한 확인/변경: /permissions sandbox MODE | approval MODE | confirm | cancel",
    ),
    ("/model", "이 세션의 모델 변경"),
    ("/diff", "보고된 변경: /diff [N] (N은 최근 이벤트 순서)"),
    ("/agents", "에이전트와 새 터미널: /agent N [open]"),
    ("/approvals", "승인 요청 검토: /approve N M"),
    ("/new", "새 대화 시작"),
    (
        "/sessions",
        "세션 관리 · /sessions N|more|refresh (번호로 전환)",
    ),
    ("/resume", "세션 목록 열기 (기존 명령 호환)"),
    ("/resume last", "최근 대화 재개"),
    (
        "/queue",
        "대기열 확인 · /queue drop N · force N · resume · clear",
    ),
    ("/quit", "화면 종료"),
    ("/chat", "대화와 입력으로 돌아가기"),
    ("/panel", "오른쪽 활동 패널 열기/닫기"),
    ("/copy", "최근 모델 메시지 복사: /copy [N], 최신이 1"),
    ("/branch", "완료된 모델 메시지에서 분기: /branch [N]"),
    ("/tool", "최근 도구 로그: /tool N [next|prev]"),
    ("/agent", "에이전트 상세/터미널: /agent N [open]"),
    ("/approve", "승인: /approve N M | detail N | later"),
    ("/skill", "스킬 선택/해제: /skill NAME | remove NAME"),
    ("/explore", "로컬 공유 코드 탐색: /explore SYMBOL"),
    ("/questions", "대기 중인 질문 선택·직접 입력"),
    ("/task", "작업 상세 보기: /task N"),
    ("/usage", "토큰·할당량 보기: /usage [refresh]"),
    (
        "/effort",
        "모델 추론 effort 설정: /effort VALUE|default|runtime",
    ),
];
#[derive(Clone, Debug, PartialEq)]
pub enum Intent {
    Explore(String),
    Submit(String),
    Interrupt,
    PasteClipboard,
    Reply(String, String),
    Answer(String, Vec<crate::agent::QuestionAnswer>),
    OpenAgent(String),
    ReadAgent(String),
    Copy(String),
    Quit,
    RefreshSkills,
    RefreshModels,
    Session(Option<String>),
    RefreshSessions,
    LoadMoreSessions(String),
    Fork(String),
    SaveSettings,
    UpdatePermission(String, String),
    TrustWorkspace(bool),
    RefreshUsage,
}
pub struct App {
    pub thread: Option<String>,
    pub turn: Option<String>,
    pub model: String,
    pub effort: Option<String>,
    pub runtime_effort: Option<String>,
    pub model_profiles: Vec<crate::agent::ModelProfile>,
    pub status: String,
    pub readonly: bool,
    pub entries: Vec<Entry>,
    // Finished messages dominate chat redraw costs. Reformat only entries whose
    // text or available width has changed, including during streaming.
    message_cache: MessageCache,
    // Reuse the laid-out transcript while only the composer or status changes.
    chat_rows_cache: RefCell<Option<CachedChatRows>>,
    pub agents: Vec<Agent>,
    pub tasks: Vec<crate::agent::ActivityTask>,
    pub selected_task: Option<String>,
    pub approvals: Vec<Approval>,
    pub input_forms: Vec<questions::InputForm>,
    pub approval_popup_dismissed: bool,
    pub trust_prompt: Option<String>,
    pub trust_pending: bool,
    pub workspace_trust: Option<bool>,
    pub restricted_workspace: bool,
    pub diff: String,
    selected_diff_entry: Option<String>,
    recent_change_ids: HashSet<String>,
    pub usage: String,
    pub tokens: crate::agent::TokenUsage,
    pub quotas: Vec<crate::agent::QuotaSnapshot>,
    pub execution_limit: Option<crate::agent::LimitKind>,
    pub work_phase: crate::agent::WorkPhase,
    work_phrase_index: usize,
    work_phrase_tick: Instant,
    pub editor: Editor,
    pub view: View,
    pub focus: Option<Action>,
    pub composer_active: bool,
    pub follow: bool,
    pub scroll: usize,
    scroll_limit: usize,
    chat_scroll_limit: usize,
    overview_scroll: usize,
    overview_scroll_limit: usize,
    overview_follow: bool,
    pub notice: String,
    pub selected_agent: usize,
    pub agent_entries: Vec<Entry>,
    pub history_pending: bool,
    pub busy: bool,
    pub pending: HashMap<u64, Operation>,
    pub submitted: Option<String>,
    pub submitted_model: Option<String>,
    pub submitted_effort: Option<String>,
    pub queued: VecDeque<QueuedMessage>,
    pub queue_paused: bool,
    // The current turn must finish before the promoted message may start.
    force_after_interrupt: Option<String>,
    expanded: HashSet<String>,
    tool_pages: HashMap<String, usize>,
    positions: HashMap<View, (usize, bool)>,
    pub preferences: Preferences,
    // Confirmed by the runtime only; unknown until explicitly set or reported.
    pub sandbox_mode: Option<String>,
    pub approval_mode: Option<String>,
    pub permission_pending: Option<(String, String)>,
    pub permission_confirm: Option<(String, String)>,
    pub panel_open: bool,
    pub wide_layout: bool,
    pub sessions: Vec<SessionSummary>,
    pub sessions_loaded: bool,
    pub sessions_next_cursor: Option<String>,
    pub session_title: String,
    pub branch_entry: Option<String>,
    pub entry_turns: HashMap<String, String>,
    pub finished_turns: HashSet<String>,
    notice_deadline: Option<Instant>,
    notice_error: bool,
    animation_frame: usize,
    animation_tick: Instant,
    active_tasks: HashSet<String>,
    pub hovered: Option<Action>,
    pub skills: Vec<Skill>,
    pub selected_skills: Vec<Skill>,
    pub submitted_skills: Vec<Skill>,
    pub skills_loaded: bool,
    pub skill_errors: Vec<String>,
    pub models: Vec<String>,
    menu_open: bool,
    menu_dismissed: bool,
    menu_index: usize,
}
impl App {
    pub fn new(readonly: bool) -> Self {
        Self {
            thread: None,
            turn: None,
            model: String::new(),
            effort: None,
            runtime_effort: None,
            model_profiles: Vec::new(),
            status: "연결 중".into(),
            readonly,
            entries: Vec::new(),
            message_cache: RefCell::new(HashMap::new()),
            chat_rows_cache: RefCell::new(None),
            agents: Vec::new(),
            tasks: Vec::new(),
            selected_task: None,
            approvals: Vec::new(),
            input_forms: Vec::new(),
            approval_popup_dismissed: false,
            trust_prompt: None,
            trust_pending: false,
            workspace_trust: None,
            restricted_workspace: false,
            diff: String::new(),
            selected_diff_entry: None,
            recent_change_ids: HashSet::new(),
            usage: String::new(),
            tokens: crate::agent::TokenUsage::default(),
            quotas: Vec::new(),
            execution_limit: None,
            work_phase: crate::agent::WorkPhase::default(),
            work_phrase_index: 0,
            work_phrase_tick: Instant::now(),
            editor: Editor::default(),
            view: View::Chat,
            focus: None,
            composer_active: false,
            follow: true,
            scroll: 0,
            scroll_limit: 0,
            chat_scroll_limit: 0,
            overview_scroll: 0,
            overview_scroll_limit: 0,
            overview_follow: true,
            notice: String::new(),
            selected_agent: 0,
            agent_entries: Vec::new(),
            history_pending: false,
            busy: false,
            pending: HashMap::new(),
            submitted: None,
            submitted_model: None,
            submitted_effort: None,
            queued: VecDeque::new(),
            queue_paused: false,
            force_after_interrupt: None,
            expanded: HashSet::new(),
            tool_pages: HashMap::new(),
            positions: HashMap::new(),
            preferences: Preferences::default(),
            sandbox_mode: None,
            approval_mode: None,
            permission_pending: None,
            permission_confirm: None,
            panel_open: true,
            wide_layout: false,
            sessions: Vec::new(),
            sessions_loaded: false,
            sessions_next_cursor: None,
            session_title: "새 대화".into(),
            branch_entry: None,
            entry_turns: HashMap::new(),
            finished_turns: HashSet::new(),
            notice_deadline: None,
            notice_error: false,
            animation_frame: 0,
            animation_tick: Instant::now(),
            active_tasks: HashSet::new(),
            hovered: None,
            skills: Vec::new(),
            selected_skills: Vec::new(),
            submitted_skills: Vec::new(),
            skills_loaded: false,
            skill_errors: Vec::new(),
            models: Vec::new(),
            menu_open: false,
            menu_dismissed: false,
            menu_index: 0,
        }
    }
    pub fn permission_applied(&mut self) {
        if let Some((kind, value)) = self.permission_pending.take() {
            match kind.as_str() {
                "sandbox" => self.sandbox_mode = Some(value),
                "approval" => self.approval_mode = Some(value),
                _ => {}
            }
            self.notify("권한 설정을 코어에서 확인했습니다 · 다음 작업부터 적용");
        }
    }
    pub fn permission_failed(&mut self, reason: &str) {
        self.permission_pending = None;
        self.error(format!("권한 변경 실패: {reason} · 이전 설정 유지"));
    }
    pub fn approval_reply_failed(&mut self, id: &str, reason: &str) {
        if let Some(approval) = self.approvals.iter_mut().find(|approval| approval.id == id) {
            approval.answering = false;
        }
        self.error(format!("승인 응답 실패: {reason} · 다시 선택하세요"));
    }
    fn switch_view(&mut self, view: View) {
        if self.view == view {
            return;
        }
        self.positions
            .insert(self.view.clone(), (self.scroll, self.follow));
        let (scroll, follow) = self
            .positions
            .get(&view)
            .copied()
            .unwrap_or((0, view == View::Chat));
        self.view = view;
        self.scroll = scroll;
        self.follow = follow;
        self.focus = None;
        self.composer_active = false;
    }
    pub fn error(&mut self, message: impl Into<String>) {
        self.notice = message.into();
        self.notice_deadline = Some(Instant::now() + Duration::from_secs(7));
        self.notice_error = true;
    }
    pub fn notify(&mut self, message: impl Into<String>) {
        self.notice = message.into();
        self.notice_deadline = Some(Instant::now() + Duration::from_secs(4));
        self.notice_error = false;
    }
    pub fn tick(&mut self, now: Instant) -> bool {
        let mut changed = false;
        if self.notice_deadline.is_some_and(|deadline| now >= deadline) {
            self.notice.clear();
            self.notice_deadline = None;
            self.notice_error = false;
            changed = true;
        }
        if self.is_working() {
            if now.saturating_duration_since(self.work_phrase_tick) >= Duration::from_millis(2200) {
                self.work_phrase_index = (self.work_phrase_index + 1) % 3;
                self.work_phrase_tick = now;
                changed = true;
            }
            let steps = now
                .saturating_duration_since(self.animation_tick)
                .as_millis()
                / 120;
            if steps > 0 {
                self.animation_frame = (self.animation_frame + (steps % 10) as usize) % 10;
                self.animation_tick = now;
                changed = true;
            }
        } else {
            changed |= self.animation_frame != 0;
            self.animation_frame = 0;
            self.animation_tick = now;
            self.work_phrase_index = 0;
            self.work_phrase_tick = now;
        }
        changed
    }

    fn is_working(&self) -> bool {
        (self.busy
            || self.turn.is_some()
            || self.submitted.is_some()
            || !self.active_tasks.is_empty())
            && (self.execution_limit.is_none()
                || self.active_tasks.iter().any(|id| {
                    self.tasks.iter().any(|task| {
                        &task.id == id && task.worker == Some(crate::agent::WorkerKind::Local)
                    })
                }))
            && self.approvals.is_empty()
            && !self.input_forms.iter().any(|f| f.request.blocking)
            && self.trust_prompt.is_none()
            && !matches!(
                self.status.as_str(),
                "연결 끊김" | "failed" | "실패" | "오류"
            )
    }
    pub fn clear_text_selection(&mut self) {
        self.branch_entry = None;
        self.focus = None;
        self.hovered = None;
    }
    fn restore_input_after_message_action(&mut self) {
        self.clear_text_selection();
        if !self.readonly {
            self.composer_active = true;
        }
    }
    pub fn paste(&mut self, text: &str) {
        if self.readonly || text.is_empty() {
            return;
        }
        if self.question_paste(text) {
            return;
        }
        self.focus = None;
        self.branch_entry = None;
        self.menu_open = false;
        self.menu_dismissed = true;
        if self.view != View::Chat && !self.wide_layout {
            self.switch_view(View::Chat);
        }
        self.composer_active = true;
        self.editor.paste(text);
    }
    pub fn invalidate_chat_rows(&self) {
        self.chat_rows_cache.borrow_mut().take();
    }
    fn has_reported_changes(&self) -> bool {
        !self.diff.is_empty()
            || self.entries.iter().any(|entry| {
                entry.kind == Kind::Change
                    && !entry.body.is_empty()
                    && self.recent_change_ids.contains(&entry.id)
            })
    }
    fn reported_change_label(&self) -> String {
        if self.diff.is_empty() {
            format!(
                "{}건",
                self.entries
                    .iter()
                    .filter(|entry| entry.kind == Kind::Change
                        && !entry.body.is_empty()
                        && self.recent_change_ids.contains(&entry.id))
                    .count()
            )
        } else {
            reported_file_count(&self.diff).to_string()
        }
    }
    fn cached_chat_rows(&self, width: usize) -> Rc<Vec<ContentRow>> {
        // The tail guard also handles callers appending to the public entries
        // collection directly. Normal runtime updates invalidate explicitly.
        let count = self.entries.len();
        let tail_len = self.entries.last().map_or(0, |entry| entry.body.len());
        if let Some(cached) = self.chat_rows_cache.borrow().as_ref()
            && cached.width == width
            && cached.count == count
            && cached.tail_len == tail_len
            && cached.language == self.preferences.language
        {
            return Rc::clone(&cached.rows);
        }
        let rows = Rc::new(self.content_rows_for(&View::Chat, width));
        *self.chat_rows_cache.borrow_mut() = Some(CachedChatRows {
            width,
            count,
            tail_len,
            language: self.preferences.language,
            rows: Rc::clone(&rows),
        });
        rows
    }
    pub fn activate(&mut self, action: Action) -> Option<Intent> {
        match action {
            Action::Effort(value) => self.select_effort(&value),
            Action::Task(id) => {
                if self.tasks.iter().any(|task| task.id == id) {
                    self.selected_task = Some(id);
                    self.panel_open = true;
                    self.switch_view(View::Agents);
                    self.follow = false;
                    self.scroll = 0;
                    self.focus = None;
                }
            }
            Action::Questions
            | Action::QuestionOption(_)
            | Action::QuestionMove(_)
            | Action::SubmitAnswers
            | Action::DismissQuestion => return self.question_action(&action),
            Action::ToggleApproval(id) => {
                for approval in &mut self.approvals {
                    if approval.id == id {
                        approval.expanded = !approval.expanded;
                    }
                }
            }
            Action::Toggle(id) => {
                if !self.expanded.remove(&id) {
                    self.expanded.insert(id.clone());
                }
                self.tool_pages.remove(&id);
                let expanded = self.expanded.contains(&id);
                for entry in self
                    .entries
                    .iter_mut()
                    .chain(self.agent_entries.iter_mut())
                    .filter(|e| e.id == id)
                {
                    entry.expanded = expanded;
                }
                self.invalidate_chat_rows();
            }
            Action::OutputPage(id, next) => {
                let page = self.tool_pages.entry(id).or_default();
                if next {
                    *page = page.saturating_add(1);
                } else {
                    *page = page.saturating_sub(1);
                }
                self.invalidate_chat_rows();
            }
            Action::Agents => {
                self.selected_task = None;
                self.panel_open = true;
                self.switch_view(View::Agents);
            }
            Action::Diff => {
                self.selected_diff_entry = None;
                self.panel_open = true;
                self.switch_view(View::Diff);
            }
            Action::DiffEntry(id) => {
                if self
                    .entries
                    .iter()
                    .any(|entry| entry.id == id && entry.kind == Kind::Change)
                {
                    self.selected_diff_entry = Some(id);
                    self.panel_open = true;
                    self.switch_view(View::Diff);
                }
            }
            Action::Approvals => {
                self.approval_popup_dismissed = true;
                self.panel_open = true;
                self.switch_view(View::Approvals);
            }
            Action::Back => {
                self.switch_view(View::Chat);
                self.focus = None;
            }
            Action::Agent(index) => {
                if let Some(agent) = self.agents.get(index) {
                    self.selected_agent = index;
                    self.agent_entries.clear();
                    return Some(Intent::ReadAgent(agent.id.clone()));
                }
            }
            Action::OpenAgent(index) => {
                if let Some(agent) = self.agents.get(index) {
                    return Some(Intent::OpenAgent(agent.id.clone()));
                }
            }
            Action::Decide(id, choice) => {
                if let Some(approval) = self.approvals.iter_mut().find(|a| a.id == id)
                    && !approval.answering
                    && let Some(choice) = approval.choices.get(choice)
                {
                    approval.answering = true;
                    return Some(Intent::Reply(approval.id.clone(), choice.result.clone()));
                }
            }
            Action::DismissApproval => {
                self.approval_popup_dismissed = true;
                self.focus = None;
            }
            Action::TrustWorkspace(trust) => {
                if self.trust_prompt.is_some() && !self.trust_pending {
                    self.trust_pending = true;
                    self.focus = None;
                    return Some(Intent::TrustWorkspace(trust));
                }
            }
            Action::Setting(setting) => {
                match setting.as_str() {
                    "korean" => self.preferences.language = Language::Korean,
                    "english" => self.preferences.language = Language::English,
                    "panel" => {
                        self.preferences.panel_open = !self.preferences.panel_open;
                        self.panel_open = self.preferences.panel_open;
                    }
                    _ => return None,
                }
                return Some(Intent::SaveSettings);
            }
            Action::Permission(kind, value) if !self.readonly => {
                if self.thread.is_none() || self.permission_pending.is_some() {
                    return None;
                }
                if !matches!(
                    (kind.as_str(), value.as_str()),
                    (
                        "sandbox",
                        "readOnly" | "workspaceWrite" | "dangerFullAccess"
                    ) | ("approval", "untrusted" | "on-request" | "never")
                ) {
                    return None;
                }
                let current = if kind == "sandbox" {
                    &self.sandbox_mode
                } else {
                    &self.approval_mode
                };
                if current.as_deref() == Some(value.as_str()) {
                    self.permission_confirm = None;
                    return None;
                }
                if value == "dangerFullAccess" || value == "never" {
                    self.permission_confirm = Some((kind, value));
                    self.panel_open = true;
                    self.switch_view(View::Permissions);
                    return None;
                }
                self.permission_confirm = None;
                self.permission_pending = Some((kind.clone(), value.clone()));
                return Some(Intent::UpdatePermission(kind, value));
            }
            Action::ConfirmPermission(true) if !self.readonly => {
                if self.permission_pending.is_none()
                    && self.thread.is_some()
                    && let Some((kind, value)) = self.permission_confirm.take()
                {
                    self.permission_pending = Some((kind.clone(), value.clone()));
                    return Some(Intent::UpdatePermission(kind, value));
                }
            }
            Action::ConfirmPermission(false) => {
                self.permission_confirm = None;
            }
            Action::SelectEntry(id) | Action::MessageRow(id, _, _) => {
                self.branch_entry = Some(id);
                self.focus = None;
            }
            Action::Branch(id) if !self.readonly => {
                self.restore_input_after_message_action();
                if self.busy
                    || self.turn.is_some()
                    || self.submitted.is_some()
                    || !self.approvals.is_empty()
                    || !self.input_forms.is_empty()
                    || !self.queued.is_empty()
                {
                    self.error("현재 작업과 승인을 마친 뒤 분기하세요.");
                } else if let Some(turn) = self
                    .entry_turns
                    .get(&id)
                    .filter(|turn| self.finished_turns.contains(*turn))
                {
                    return Some(Intent::Fork(turn.clone()));
                }
            }
            Action::Branch(_) => self.restore_input_after_message_action(),
            Action::Resume(id) => {
                self.focus = None;
                return self.command(&format!("/resume {id}"));
            }
            Action::Sessions => {
                self.focus = None;
                return self.command("/sessions");
            }
            Action::NewSession => {
                self.focus = None;
                return self.command("/new");
            }
            Action::RefreshSessions => {
                self.focus = None;
                return Some(Intent::RefreshSessions);
            }
            Action::MoreSessions => {
                if !self.sessions_loaded {
                    self.notify("목록 새로고침이 끝난 뒤 이전 대화를 불러올 수 있습니다.");
                    return None;
                }
                if self
                    .pending
                    .values()
                    .any(|method| *method == Operation::MoreSessions)
                {
                    return None;
                }
                if let Some(cursor) = &self.sessions_next_cursor {
                    self.focus = None;
                    return Some(Intent::LoadMoreSessions(cursor.clone()));
                }
            }
            Action::Panel => {
                self.panel_open = !self.panel_open;
                if !self.panel_open {
                    self.switch_view(View::Chat);
                }
            }
            Action::Menu => {
                self.menu_open = !self.menu_open;
                self.menu_dismissed = false;
                self.menu_index = 0;
                self.focus = None;
            }
            Action::Command(command) => return self.command(&command),
            Action::Skill(path) if !self.readonly => {
                if let Some(skill) = self.skills.iter().find(|skill| skill.path == path).cloned() {
                    if !self.selected_skills.iter().any(|s| s.path == path) {
                        self.selected_skills.push(skill);
                    }
                    if self.editor.text.starts_with('$')
                        && !self.editor.text.contains(char::is_whitespace)
                    {
                        self.editor = Editor::default();
                    }
                    self.menu_open = false;
                    self.menu_dismissed = true;
                    self.switch_view(View::Chat);
                    self.focus = None;
                    self.notify("스킬 선택됨 · 지시를 입력하고 전송하세요");
                }
            }
            Action::RemoveSkill(path) => self.selected_skills.retain(|skill| skill.path != path),
            Action::RemoveQueued(index) => {
                self.queued.remove(index);
                if index == 0 && self.force_after_interrupt.is_some() {
                    self.force_after_interrupt = None;
                    self.notify("우선 전송이 취소되었습니다.");
                }
                self.focus = None;
                if self.queued.is_empty() {
                    self.queue_paused = false;
                }
            }
            Action::ForceQueued(index) if !self.readonly => {
                if index >= self.queued.len() {
                    self.error("해당 번호의 대기 메시지가 없습니다.");
                    return None;
                }
                if let Some(item) = self.queued.remove(index) {
                    self.queued.push_front(item);
                }
                self.focus = None;
                if self.execution_limit.is_some() {
                    self.error("할당량 또는 호출 제한을 확인한 후 재개하세요. /usage refresh");
                    return None;
                }
                self.queue_paused = false;
                if let Some(turn) = self.turn.clone() {
                    if self.force_after_interrupt.as_deref() == Some(turn.as_str()) {
                        self.notify("우선 전송할 메시지를 변경했습니다.");
                        return None;
                    }
                    self.force_after_interrupt = Some(turn);
                    self.status = "중단 요청 중".into();
                    self.notify("현재 응답 중단 요청 · 완료 확인 후 우선 전송");
                    if !self
                        .pending
                        .values()
                        .any(|method| *method == Operation::Interrupt)
                    {
                        return Some(Intent::Interrupt);
                    }
                } else if self.submitted.is_some() || self.busy {
                    self.notify("현재 요청이 시작되면 우선 전송할 수 있습니다.");
                } else {
                    self.notify("선택한 메시지를 다음으로 전송합니다.");
                }
            }
            Action::ForceQueued(_) => {}
            Action::ResumeQueue => {
                if self.execution_limit.is_some() {
                    self.error("할당량 또는 호출 제한을 확인한 후 재개하세요. /usage refresh");
                    return None;
                }
                self.queue_paused = false;
                self.focus = None;
                self.notify("대기열 자동 전송을 재개합니다.");
            }
            Action::Model(model) if !self.readonly && self.submitted.is_none() => {
                if self.models.contains(&model) {
                    let had_effort = self.effort.is_some();
                    self.model = model;
                    self.reconcile_effort();
                    if !(had_effort && self.effort.is_none()) {
                        self.notify("다음 메시지부터 선택한 모델을 사용합니다");
                    }
                    self.switch_view(View::Chat);
                }
            }
            Action::Skill(_) | Action::Model(_) => {}
            Action::Send => {
                if self.permission_pending.is_some() {
                    self.error("권한 설정을 코어에서 확인한 뒤 전송할 수 있습니다.");
                    return None;
                }
                if self.editor.text.trim_start().starts_with('/') {
                    let command = self.editor.text.trim().to_owned();
                    return self.command(&command);
                }
                if !self.readonly && !self.editor.text.trim().is_empty() {
                    self.branch_entry = None;
                    self.notice.clear();
                    self.notice_deadline = None;
                    self.notice_error = false;
                    let text = self.editor.take_text();
                    let item = QueuedMessage {
                        effort: self.effort.clone(),
                        text,
                        skills: std::mem::take(&mut self.selected_skills),
                        model: self.model.clone(),
                    };
                    self.follow = true;
                    if self.busy
                        || self.turn.is_some()
                        || self.submitted.is_some()
                        || !self.queued.is_empty()
                        || self.thread.is_none()
                        || self.queue_paused
                        || self.execution_limit.is_some()
                    {
                        self.queued.push_back(item);
                        self.notify(format!("전송 대기열에 추가됨 · {}개", self.queued.len()));
                    } else {
                        return Some(self.begin_submission(item));
                    }
                }
            }
            Action::Interrupt => {
                if !self.readonly
                    && self.turn.is_some()
                    && self.force_after_interrupt.is_none()
                    && !self
                        .pending
                        .values()
                        .any(|method| *method == Operation::Interrupt)
                {
                    self.status = "중단 요청 중".into();
                    return Some(Intent::Interrupt);
                }
            }
            Action::Input => {
                self.branch_entry = None;
                self.composer_active = true;
                self.focus = None;
            }
            Action::Copy(id) => {
                self.restore_input_after_message_action();
                if let Some(entry) = self
                    .entries
                    .iter()
                    .chain(self.agent_entries.iter())
                    .find(|e| e.id == id)
                {
                    return Some(Intent::Copy(entry.body.clone()));
                }
            }
            Action::Permission(_, _) | Action::ConfirmPermission(true) => {}
            Action::Quit => return Some(Intent::Quit),
        }
        None
    }
    fn begin_submission(&mut self, item: QueuedMessage) -> Intent {
        self.submitted = Some(item.text.clone());
        self.submitted_skills = item.skills;
        self.submitted_model = Some(item.model);
        self.submitted_effort = item.effort;
        self.status = "전송 중".into();
        self.follow = true;
        Intent::Submit(item.text)
    }
    pub fn next_queued(&mut self) -> Option<Intent> {
        if self.readonly
            || self.busy
            || self.turn.is_some()
            || self.submitted.is_some()
            || self.permission_pending.is_some()
            || self.force_after_interrupt.is_some()
            || self.queue_paused
            || self.execution_limit.is_some()
            || self.thread.is_none()
            || !self.approvals.is_empty()
            || !self.input_forms.is_empty()
            || self
                .pending
                .values()
                .any(|name| matches!(name, Operation::SwitchSession | Operation::Interrupt))
        {
            return None;
        }
        let item = self.queued.pop_front()?;
        Some(self.begin_submission(item))
    }
    pub fn interrupt_failed(&mut self, message: &str) {
        if self.force_after_interrupt.take().is_some() {
            self.queue_paused = true;
            self.error(format!("중단 실패 · 대기열 보존됨 · {message}"));
        } else {
            self.error(format!("중단 실패 · {message}"));
        }
    }
    pub fn fail_submission(&mut self, message: impl Into<String>) {
        if let Some(text) = self.submitted.take() {
            self.queued.push_front(QueuedMessage {
                effort: self.submitted_effort.take(),
                text,
                skills: std::mem::take(&mut self.submitted_skills),
                model: self
                    .submitted_model
                    .take()
                    .unwrap_or_else(|| self.model.clone()),
            });
        }
        self.queue_paused = true;
        self.busy = false;
        self.error(format!("전송 실패 · 대기열 보존됨 · {}", message.into()));
    }
    pub fn restore_skills(&mut self) {
        for skill in std::mem::take(&mut self.submitted_skills) {
            if !self.selected_skills.iter().any(|s| s.path == skill.path) {
                self.selected_skills.push(skill);
            }
        }
    }
    fn label<'a>(&self, text: &'a str) -> &'a str {
        preferences::label(self.preferences.language, text)
    }
    fn session_switch_block_reason(&self) -> Option<&'static str> {
        if !self.input_forms.is_empty() {
            Some("질문에 답한 후 대화를 전환할 수 있습니다.")
        } else if !self.approvals.is_empty() {
            Some("승인 요청을 처리한 후 대화를 전환할 수 있습니다.")
        } else if self.permission_pending.is_some() {
            Some("권한 변경이 완료된 후 대화를 전환할 수 있습니다.")
        } else if !self.queued.is_empty() && self.thread.is_some() {
            Some("대기열을 전송하거나 /queue clear로 비운 후 대화를 전환하세요.")
        } else if self.busy && self.thread.is_none() {
            Some("세션 연결이 끝나면 전환할 수 있습니다.")
        } else if self.busy || self.turn.is_some() || self.submitted.is_some() {
            Some("현재 응답이 완료되면 대화를 전환할 수 있습니다.")
        } else {
            None
        }
    }
    fn command(&mut self, command: &str) -> Option<Intent> {
        let (name, argument) = command
            .split_once(char::is_whitespace)
            .map_or((command, ""), |(a, b)| (a, b.trim()));
        if self.readonly
            && matches!(
                name,
                "/new" | "/resume" | "/sessions" | "/model" | "/skills" | "/permissions"
            )
        {
            self.error("열람 전용 창입니다. 메인 창에서 실행하세요.");
            return None;
        }
        if (name == "/new" || (name == "/resume" && !argument.is_empty()))
            && let Some(reason) = self.session_switch_block_reason()
        {
            self.error(reason);
            return None;
        }
        let intent = match name {
            "/explore" => {
                if argument.is_empty() {
                    self.error("사용법: /explore SYMBOL 또는 검색어");
                    None
                } else {
                    Some(Intent::Explore(argument.into()))
                }
            }
            "/settings" => {
                if matches!(argument, "korean" | "english" | "panel") {
                    let intent = self.activate(Action::Setting(argument.into()));
                    // The panel setting changes the actual panel state.
                    // Opening settings unconditionally here used to undo it.
                    if argument == "panel" && !self.panel_open {
                        self.switch_view(View::Chat);
                    } else {
                        self.panel_open = true;
                        self.switch_view(View::Settings);
                    }
                    return self.finish_command(intent);
                }
                if !argument.is_empty() {
                    self.error("사용법: /settings [korean|english|panel]");
                }
                self.panel_open = true;
                self.switch_view(View::Settings);
                None
            }
            "/permissions" => {
                if let Some((kind, value)) = argument.split_once(' ')
                    && matches!(kind, "sandbox" | "approval")
                {
                    self.panel_open = true;
                    self.switch_view(View::Permissions);
                    let intent =
                        self.activate(Action::Permission(kind.into(), value.trim().into()));
                    return self.finish_command(intent);
                }
                if matches!(argument, "confirm" | "cancel") {
                    self.panel_open = true;
                    self.switch_view(View::Permissions);
                    let intent = self.activate(Action::ConfirmPermission(argument == "confirm"));
                    return self.finish_command(intent);
                }
                if !argument.is_empty() {
                    self.error("사용법: /permissions [sandbox MODE|approval MODE|confirm|cancel]");
                }
                self.panel_open = true;
                self.switch_view(View::Permissions);
                None
            }
            "/help" => {
                self.panel_open = true;
                self.switch_view(View::Help);
                None
            }
            "/skills" => {
                self.panel_open = true;
                self.switch_view(View::Skills);
                Some(Intent::RefreshSkills)
            }
            "/model" => {
                self.panel_open = true;
                self.switch_view(View::Models);
                if !argument.is_empty() {
                    self.activate(Action::Model(argument.into()));
                }
                Some(Intent::RefreshModels)
            }
            "/diff" => {
                if argument.is_empty() {
                    self.activate(Action::Diff);
                } else if let Some(index) = command_index(argument) {
                    if let Some(id) = self
                        .entries
                        .iter()
                        .rev()
                        .filter(|entry| entry.kind == Kind::Change && !entry.body.is_empty())
                        .nth(index)
                        .map(|entry| entry.id.clone())
                    {
                        self.activate(Action::DiffEntry(id));
                    } else {
                        self.error("해당 번호의 파일 변경이 없습니다.");
                    }
                } else {
                    self.error("사용법: /diff [N] (최근 변경부터 1)");
                }
                None
            }
            "/chat" => {
                self.activate(Action::Back);
                None
            }
            "/panel" => {
                self.activate(Action::Panel);
                None
            }
            "/copy" | "/branch" | "/tool" => {
                let parts: Vec<_> = argument.split_whitespace().collect();
                let index_text = parts.first().copied().unwrap_or("1");
                if let Some(index) = command_index(index_text)
                    && (parts.len() <= 1 || (name == "/tool" && parts.len() == 2))
                {
                    let id = self
                        .entries
                        .iter()
                        .rev()
                        .filter(|entry| match name {
                            "/copy" => entry.kind == Kind::Assistant,
                            "/branch" => {
                                entry.kind == Kind::Assistant
                                    && self
                                        .entry_turns
                                        .get(&entry.id)
                                        .is_some_and(|turn| self.finished_turns.contains(turn))
                            }
                            _ => entry.kind == Kind::Tool,
                        })
                        .nth(index)
                        .map(|entry| entry.id.clone());
                    match (name, id) {
                        ("/copy", Some(id)) => {
                            let intent = self.activate(Action::Copy(id));
                            return self.finish_command(intent);
                        }
                        ("/branch", Some(id)) => {
                            let intent = self.activate(Action::Branch(id));
                            return self.finish_command(intent);
                        }
                        ("/tool", Some(id)) => match parts.get(1).copied() {
                            None => {
                                self.activate(Action::Toggle(id));
                            }
                            Some("next") => {
                                if !self.expanded.contains(&id) {
                                    self.activate(Action::Toggle(id.clone()));
                                }
                                self.activate(Action::OutputPage(id, true));
                            }
                            Some("prev") => {
                                if !self.expanded.contains(&id) {
                                    self.activate(Action::Toggle(id.clone()));
                                }
                                self.activate(Action::OutputPage(id, false));
                            }
                            _ => self.error("사용법: /tool N [next|prev]"),
                        },
                        _ => self.error("해당 번호의 기록이 없습니다."),
                    }
                } else {
                    self.error("사용법: /copy [N], /branch [N], /tool N [next|prev]");
                }
                None
            }
            "/effort" => {
                self.switch_view(View::Models);
                if !argument.is_empty() {
                    self.select_effort(if argument == "runtime" { "" } else { argument });
                }
                None
            }
            "/usage" => {
                self.activate(Action::Agents);
                self.follow = false;
                self.scroll = 0;
                if argument == "refresh" {
                    Some(Intent::RefreshUsage)
                } else if argument.is_empty() {
                    None
                } else {
                    self.error("사용법: /usage [refresh]");
                    None
                }
            }
            "/task" => {
                if let Some(index) = command_index(argument)
                    && let Some(task) = self.tasks.get(index)
                {
                    self.activate(Action::Task(task.id.clone()));
                } else {
                    self.error("사용법: /task N (1부터 시작)");
                }
                None
            }
            "/agent" => {
                let parts: Vec<_> = argument.split_whitespace().collect();
                if let Some(index) = command_index(parts.first().copied().unwrap_or("1"))
                    && index < self.agents.len()
                {
                    let intent = match parts.get(1).copied() {
                        None if parts.len() <= 1 => {
                            self.panel_open = true;
                            self.switch_view(View::Agents);
                            self.activate(Action::Agent(index))
                        }
                        Some("open") if parts.len() == 2 => self.activate(Action::OpenAgent(index)),
                        _ => {
                            self.error("사용법: /agent N [open]");
                            None
                        }
                    };
                    return self.finish_command(intent);
                }
                self.error("사용법: /agent N [open] (1부터 시작)");
                None
            }
            "/approve" => {
                if argument.is_empty() {
                    self.activate(Action::Approvals);
                    return self.finish_command(None);
                }
                if argument == "later" {
                    self.activate(Action::DismissApproval);
                    return self.finish_command(None);
                }
                if let Some(number) = argument.strip_prefix("detail ")
                    && let Some(index) = command_index(number.trim())
                    && let Some(approval) = self.approvals.get(index)
                {
                    let id = approval.id.to_string();
                    self.panel_open = true;
                    self.switch_view(View::Approvals);
                    self.activate(Action::ToggleApproval(id));
                    return self.finish_command(None);
                }
                let parts: Vec<_> = argument.split_whitespace().collect();
                if let [request, choice] = parts.as_slice()
                    && let (Some(request), Some(choice)) =
                        (command_index(request), command_index(choice))
                    && let Some(approval) = self.approvals.get(request)
                    && choice < approval.choices.len()
                {
                    let id = approval.id.to_string();
                    self.panel_open = true;
                    self.switch_view(View::Approvals);
                    let intent = self.activate(Action::Decide(id, choice));
                    return self.finish_command(intent);
                }
                self.error("사용법: /approve N M | detail N | later");
                None
            }
            "/skill" => {
                if argument.is_empty() {
                    self.panel_open = true;
                    self.switch_view(View::Skills);
                    return self.finish_command(Some(Intent::RefreshSkills));
                }
                if let Some(name) = argument.strip_prefix("remove ") {
                    if let Some(path) = self
                        .selected_skills
                        .iter()
                        .find(|skill| skill.name.eq_ignore_ascii_case(name.trim()))
                        .map(|skill| skill.path.clone())
                    {
                        self.activate(Action::RemoveSkill(path));
                    } else {
                        self.error("선택된 스킬을 찾지 못했습니다.");
                    }
                    return self.finish_command(None);
                }
                if let Some(path) = self
                    .skills
                    .iter()
                    .find(|skill| skill.name.eq_ignore_ascii_case(argument))
                    .map(|skill| skill.path.clone())
                {
                    self.activate(Action::Skill(path));
                } else {
                    self.error("스킬을 찾지 못했습니다. /skills로 확인하세요.");
                }
                None
            }
            "/agents" => {
                self.activate(Action::Agents);
                None
            }
            "/questions" => {
                self.activate(Action::Questions);
                None
            }
            "/approvals" => {
                self.activate(Action::Approvals);
                None
            }
            "/new" => Some(Intent::Session(None)),
            "/resume" if !argument.is_empty() => Some(Intent::Session(Some(argument.into()))),
            "/sessions" if let Some(index) = command_index(argument) => {
                if !self.sessions_loaded && self.sessions.is_empty() {
                    self.error("세션 목록을 불러온 후 선택하세요.");
                    return self.finish_command(None);
                }
                if let Some(id) = self
                    .sessions
                    .iter()
                    .filter(|session| self.thread.as_deref() != Some(session.id.as_str()))
                    .nth(index)
                    .map(|session| session.id.clone())
                {
                    let intent = self.activate(Action::Resume(id));
                    return self.finish_command(intent);
                }
                self.error("해당 번호의 세션이 없습니다. /sessions로 목록을 확인하세요.");
                return self.finish_command(None);
            }
            "/sessions" if argument == "more" => {
                self.panel_open = true;
                self.switch_view(View::Sessions);
                let intent = self.activate(Action::MoreSessions);
                return self.finish_command(intent);
            }
            "/sessions" if argument == "refresh" => {
                self.panel_open = true;
                self.switch_view(View::Sessions);
                Some(Intent::RefreshSessions)
            }
            "/resume" | "/sessions" if argument.is_empty() => {
                self.panel_open = true;
                self.switch_view(View::Sessions);
                Some(Intent::RefreshSessions)
            }
            "/queue" => {
                if let Some(number) = argument.strip_prefix("drop ") {
                    return match command_index(number.trim()) {
                        Some(index) if index < self.queued.len() => {
                            let intent = self.activate(Action::RemoveQueued(index));
                            self.finish_command(intent)
                        }
                        _ => {
                            self.error("사용법: /queue drop N (1부터 시작)");
                            None
                        }
                    };
                }
                if let Some(number) = argument.strip_prefix("force ") {
                    return match number.trim().parse::<usize>().ok().filter(|n| *n > 0) {
                        Some(n) => {
                            let intent = self.activate(Action::ForceQueued(n - 1));
                            self.finish_command(intent)
                        }
                        None => {
                            self.error("사용법: /queue force N (1부터 시작)");
                            None
                        }
                    };
                }
                match argument {
                    "resume" => {
                        self.queue_paused = false;
                        self.notify("대기열 자동 전송을 재개합니다.");
                    }
                    "clear" => {
                        self.queued.clear();
                        self.force_after_interrupt = None;
                        self.queue_paused = false;
                        self.notify("대기열을 비웠습니다.");
                    }
                    "" => self.notify(format!(
                        "전송 대기 {}개{}",
                        self.queued.len(),
                        if self.queue_paused {
                            " · 일시정지 중 (/queue resume)"
                        } else {
                            ""
                        }
                    )),
                    _ => self.error("사용법: /queue [drop N|force N|resume|clear]"),
                }
                None
            }
            "/quit" => Some(Intent::Quit),
            _ => {
                self.error("알 수 없는 명령입니다. /help 또는 /를 입력하세요.");
                return None;
            }
        };
        self.finish_command(intent)
    }
    fn finish_command(&mut self, intent: Option<Intent>) -> Option<Intent> {
        if self.editor.text.starts_with('/') {
            self.editor = Editor::default();
        }
        self.menu_open = false;
        self.menu_dismissed = true;
        self.menu_index = 0;
        intent
    }
    fn menu_items(&self) -> Vec<(String, String, Action)> {
        if self.readonly
            || self.menu_dismissed
            || (self.view != View::Chat && !self.wide_layout && !self.composer_active)
        {
            return Vec::new();
        }
        let text = self.editor.text.trim();
        if text.starts_with('$') && !text.contains(char::is_whitespace) {
            let filter = text.trim_start_matches('$').to_lowercase();
            return self
                .skills
                .iter()
                .filter(|skill| skill.name.to_lowercase().contains(&filter))
                .map(|skill| {
                    (
                        format!("${}", skill.name),
                        skill.description.clone(),
                        Action::Skill(skill.path.clone()),
                    )
                })
                .collect();
        }
        if self.menu_open || (text.starts_with('/') && !text.contains(char::is_whitespace)) {
            return COMMANDS
                .iter()
                .filter(|(name, _)| self.menu_open || name.starts_with(text))
                .map(|(name, desc)| {
                    (
                        name.to_string(),
                        desc.to_string(),
                        Action::Command(name.to_string()),
                    )
                })
                .collect();
        }
        Vec::new()
    }
    pub fn scroll_at(&mut self, x: u16, width: u16, down: bool) {
        self.scroll_pane(x, width, down, 3);
    }
    fn scroll_key(&mut self, canvas: &Canvas, down: bool, step: usize) {
        let x = self
            .focus
            .as_ref()
            .and_then(|action| canvas.hits.iter().find(|hit| &hit.action == action))
            .map(|hit| hit.x)
            .unwrap_or(if self.wide_layout && self.view != View::Chat {
                canvas.width.saturating_sub(1)
            } else {
                0
            });
        self.scroll_pane(x, canvas.width, down, step);
    }
    fn scroll_pane(&mut self, x: u16, width: u16, down: bool, step: usize) {
        // The conversation and task panes keep independent positions.
        let chat_pane = self.wide_layout && x < width - (width / 3).clamp(36, 46) - 1;
        if self.wide_layout && self.view == View::Chat && !chat_pane {
            Self::move_scroll(
                &mut self.overview_scroll,
                &mut self.overview_follow,
                self.overview_scroll_limit,
                down,
                step,
            );
            return;
        }
        if chat_pane && self.view != View::Chat {
            let (scroll, follow) = self.positions.entry(View::Chat).or_insert((0, true));
            Self::move_scroll(scroll, follow, self.chat_scroll_limit, down, step);
        } else {
            Self::move_scroll(
                &mut self.scroll,
                &mut self.follow,
                self.scroll_limit,
                down,
                step,
            );
        }
    }
    fn move_scroll(scroll: &mut usize, follow: &mut bool, max: usize, down: bool, step: usize) {
        *scroll = if down {
            scroll.saturating_add(step).min(max)
        } else {
            scroll.saturating_sub(step)
        };
        *follow = *scroll == max;
    }
    pub fn key(&mut self, key: KeyEvent, canvas: &Canvas) -> Option<Intent> {
        if self.trust_prompt.is_some() {
            match key.code {
                KeyCode::Esc => return Some(Intent::Quit),
                KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    return Some(Intent::Quit);
                }
                KeyCode::Char('1') => return self.activate(Action::TrustWorkspace(true)),
                KeyCode::Char('2') => return self.activate(Action::TrustWorkspace(false)),
                _ => return None,
            }
        }
        if self.question_visible() && self.approvals.is_empty() {
            return self.question_key(key);
        }
        if key.code == KeyCode::Esc && !self.approval_popup_dismissed && !self.approvals.is_empty()
        {
            self.approval_popup_dismissed = true;
            return None;
        }
        let menu = self.menu_items();
        if !menu.is_empty() && self.focus.is_none() {
            match key.code {
                KeyCode::Up | KeyCode::BackTab => {
                    self.menu_index =
                        (self.menu_index.min(menu.len() - 1) + menu.len() - 1) % menu.len();
                    return None;
                }
                KeyCode::Down | KeyCode::Tab => {
                    self.menu_index = (self.menu_index + 1) % menu.len();
                    return None;
                }
                KeyCode::Enter => {
                    return self.activate(menu[self.menu_index.min(menu.len() - 1)].2.clone());
                }
                KeyCode::Esc => {
                    self.menu_open = false;
                    self.menu_dismissed = true;
                    return None;
                }
                _ => {}
            }
        }
        if matches!(
            key.code,
            KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete
        ) {
            self.menu_dismissed = false;
            self.menu_index = 0;
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            match key.code {
                KeyCode::Char('q') => return Some(Intent::Quit),
                KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    if let Some(id) = self.branch_entry.clone() {
                        return self.activate(Action::Copy(id));
                    }
                    if !self.editor.text.is_empty() {
                        return Some(Intent::Copy(self.editor.text.clone()));
                    }
                    return None;
                }
                KeyCode::Char('v') => return (!self.readonly).then_some(Intent::PasteClipboard),
                KeyCode::Char('c') => return self.activate(Action::Interrupt),
                KeyCode::Char('a') => {
                    self.editor.cursor = 0;
                    return None;
                }
                KeyCode::Char('e') => {
                    self.editor.cursor = self.editor.text.len();
                    return None;
                }
                _ => {}
            }
        }
        // Typing resumes the composer after keyboard navigation, including on
        // compact screens where the selected tab occupies the main pane.
        if !self.readonly
            && menu.is_empty()
            && matches!(key.code, KeyCode::Char(_))
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && (self.focus.is_some()
                || (self.view != View::Chat && !self.wide_layout && !self.composer_active))
        {
            if self.view != View::Chat && !self.wide_layout {
                self.switch_view(View::Chat);
            }
            self.focus = None;
            self.composer_active = true;
        }
        match key.code {
            KeyCode::Esc => {
                if self.branch_entry.take().is_some() {
                    return None;
                }
                if self.composer_active && self.view != View::Chat {
                    self.composer_active = false;
                    return None;
                }
                if self.view != View::Chat {
                    self.switch_view(View::Chat);
                    self.focus = None;
                } else if self.focus.is_some() {
                    self.focus = None;
                } else {
                    self.focus = canvas.hits.first().map(|h| h.action.clone());
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.composer_active = false;
                let count = canvas.hits.len();
                if count > 0 {
                    let current = self
                        .focus
                        .as_ref()
                        .and_then(|f| canvas.hits.iter().position(|h| &h.action == f));
                    let index = if key.code == KeyCode::BackTab {
                        current.map_or(count - 1, |i| (i + count - 1) % count)
                    } else {
                        current.map_or(0, |i| (i + 1) % count)
                    };
                    self.focus = Some(canvas.hits[index].action.clone());
                }
            }
            KeyCode::PageUp => {
                self.scroll_key(canvas, false, 10);
            }
            KeyCode::PageDown => {
                self.scroll_key(canvas, true, 10);
            }
            KeyCode::End
                if self.focus.is_some()
                    || (self.view != View::Chat && !self.wide_layout && !self.composer_active)
                    || self.readonly =>
            {
                self.scroll_key(canvas, true, usize::MAX);
            }
            KeyCode::Enter if self.focus.is_some() => {
                return self.activate(self.focus.clone().unwrap());
            }
            KeyCode::Up | KeyCode::Down
                if self.focus.is_some()
                    || (self.view != View::Chat && !self.wide_layout && !self.composer_active)
                    || self.readonly =>
            {
                self.scroll_key(canvas, key.code == KeyCode::Down, 1);
            }
            _ if self.focus.is_none()
                && (self.view == View::Chat || self.wide_layout || self.composer_active)
                && !self.readonly =>
            {
                match key.code {
                    KeyCode::Enter
                        if key
                            .modifiers
                            .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
                    {
                        self.editor.insert("\n")
                    }
                    KeyCode::Enter => return self.activate(Action::Send),
                    KeyCode::Char(c)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        self.editor.insert(&c.to_string())
                    }
                    KeyCode::Backspace => self.editor.backspace(),
                    KeyCode::Delete => self.editor.delete(),
                    KeyCode::Left => self.editor.left(),
                    KeyCode::Right => self.editor.right(),
                    KeyCode::Up => self.editor.vertical(false),
                    KeyCode::Down => self.editor.vertical(true),
                    KeyCode::Home => self.editor.home(),
                    KeyCode::End => self.editor.end(),
                    _ => {}
                }
            }
            _ => {}
        }
        None
    }
    pub fn render(&mut self, width: u16, height: u16) -> Canvas {
        let mut c = Canvas::new(width, height);
        self.wide_layout = width >= 110 && self.panel_open && self.view != View::Diff;
        if width < 40 || height < 12 {
            c.text(0, 0, "터미널을 넓혀주세요 (40 × 12 이상)", Tone::Muted);
            return c;
        }
        let wide = self.wide_layout;
        let side_width = (width / 3).clamp(36, 46);
        let split = width - side_width - 1;
        let left_width = if wide { split } else { width };
        let left_view = if wide { &View::Chat } else { &self.view };
        let rows = if *left_view == View::Chat {
            self.cached_chat_rows(usize::from(left_width - 4))
        } else {
            Rc::new(self.content_rows_for(left_view, usize::from(left_width - 4)))
        };
        let side_rows = if wide {
            if self.view == View::Chat {
                self.overview_rows(usize::from(side_width - 4))
            } else {
                self.content_rows_for(&self.view, usize::from(side_width - 4))
            }
        } else {
            Vec::new()
        };
        let menu = self.menu_items();
        let input_width = usize::from(width - 6);
        let (display, visible_cursor) = self.editor.display();
        let input_rows = if self.readonly {
            1
        } else {
            (wrap(&display, input_width).len() as u16).clamp(1, (height - 9).min(5))
        };
        let notice_rows = u16::from(!self.notice.is_empty());
        let max_body = height - 6 - input_rows - notice_rows;
        // Reserve a fixed strip above the composer. Queue entries must remain
        // clickable and visible while the transcript scrolls or a tab is open.
        let queue_rows = if self.readonly || self.queued.is_empty() {
            0
        } else {
            (1 + self.queued.len().min(3) as u16).min(max_body.saturating_sub(3))
        };
        let body_height = max_body - queue_rows;
        let body_top = 3;
        let body_end = body_top + body_height;
        let queue_top = body_end + notice_rows;
        let composer_top = queue_top + queue_rows;
        let composer_bottom = composer_top + input_rows + 1;
        let footer = composer_bottom + 1;
        // One compact header, one navigation row, then content-sized partitions.
        c.text(
            2,
            0,
            if self.readonly {
                "Loom · 열람"
            } else {
                "Loom"
            },
            Tone::Normal,
        );
        let mut header = Canvas::new(width.saturating_sub(19), 1);
        let (status_icon, status_tone) = if self.is_working() {
            (
                ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"][self.animation_frame],
                Tone::Accent,
            )
        } else {
            status_style(&self.status)
        };
        let state_x = header.text(
            0,
            0,
            &format!(
                "{} {}",
                status_icon,
                if !self.approvals.is_empty() {
                    self.label("승인 대기")
                } else if self.input_forms.iter().any(|f| f.request.blocking) {
                    self.label("질문 답변 대기")
                } else if !self.active_tasks.is_empty() && !self.busy {
                    if self.active_tasks.iter().all(|id| {
                        self.tasks.iter().any(|t| {
                            &t.id == id && t.worker == Some(crate::agent::WorkerKind::Local)
                        })
                    }) {
                        self.working_phrase()
                    } else {
                        "작업 실행 중"
                    }
                } else if self.is_working() {
                    self.working_phrase()
                } else {
                    self.label(status_label(&self.status))
                }
            ),
            status_tone,
        );
        header.text(
            state_x,
            0,
            &format!(
                "  · {}{}",
                self.model,
                self.effort
                    .as_ref()
                    .map(|effort| format!(" · {effort}"))
                    .unwrap_or_default()
            ),
            Tone::Muted,
        );
        c.blit(&header, 17, 0);
        let mut nav = Canvas::new(width - 4, 1);
        let mut x: u16 = 0;
        let approvals_label = if self.approvals.is_empty() {
            self.label("승인").to_string()
        } else {
            format!("! {} {}", self.label("승인"), self.approvals.len())
        };
        let tabs = [
            ("대화", Action::Back, self.view == View::Chat),
            ("활동", Action::Agents, self.view == View::Agents),
            (
                approvals_label.as_str(),
                Action::Approvals,
                self.view == View::Approvals,
            ),
            (
                "권한",
                Action::Command("/permissions".into()),
                self.view == View::Permissions,
            ),
            (
                "스킬",
                Action::Command("/skills".into()),
                self.view == View::Skills,
            ),
            (
                "모델",
                Action::Command("/model".into()),
                self.view == View::Models,
            ),
            ("세션 ▾", Action::Sessions, self.view == View::Sessions),
            (
                "설정",
                Action::Command("/settings".into()),
                self.view == View::Settings,
            ),
        ];
        let active_width =
            tabs.iter()
                .find(|tab| tab.2)
                .map_or(0, |tab| UnicodeWidthStr::width(self.label(tab.0)) + 5) as u16;
        let mut active_pending = true;
        for (label, action, active) in tabs {
            let label = self.label(label);
            let needed = UnicodeWidthStr::width(label) as u16 + 3 + if active { 2 } else { 0 };
            let reserve = if active_pending && !active {
                active_width
            } else {
                0
            };
            if x.saturating_add(needed) > nav.width.saturating_sub(reserve) {
                continue;
            }
            if active {
                active_pending = false;
            }
            let label = if active {
                format!("● {label}")
            } else {
                label.into()
            };
            let button_x = x;
            x = nav.button(x, 0, &label, action.clone(), false);
            if matches!(action, Action::Approvals) && !self.approvals.is_empty() {
                nav.text(button_x + 1, 0, &label, Tone::Warning);
            }
            if active {
                nav.highlight(&action, Tone::Selected);
            }
        }
        c.blit(&nav, 2, 1);
        c.rule(2, "");
        if let (Some(sandbox), Some(approval)) = (&self.sandbox_mode, &self.approval_mode) {
            let sandbox_label = match sandbox.as_str() {
                "readOnly" => "읽기 전용",
                "workspaceWrite" => "작업 폴더 쓰기",
                "dangerFullAccess" => "전체 접근",
                other => other,
            };
            let approval_label = match approval.as_str() {
                "never" => "승인 없음",
                "on-request" => "필요시 승인",
                "untrusted" => "보수적 승인",
                "granular" => "세부 승인 규칙",
                other => other,
            };
            let dangerous = sandbox == "dangerFullAccess" && approval == "never";
            c.text(
                2,
                2,
                &format!(
                    "{} {sandbox_label} · {approval_label}",
                    if dangerous { "!" } else { "◈" }
                ),
                if dangerous {
                    Tone::Warning
                } else {
                    Tone::Muted
                },
            );
        }
        if self.has_reported_changes() && self.view == View::Chat && width >= 72 {
            let label = format!("파일 변경 {} ▸", self.reported_change_label());
            // A contextual shortcut appears only after Codex reports changes.
            // On wide layouts the activity panel offers the same shortcut.
            if !wide {
                c.button(width.saturating_sub(19), 2, &label, Action::Diff, false);
            }
        }
        if wide && self.view != View::Chat {
            let (scroll, follow) = self.positions.entry(View::Chat).or_insert((0, true));
            let max = rows.len().saturating_sub(usize::from(body_height));
            self.chat_scroll_limit = max;
            *scroll = if *follow { max } else { (*scroll).min(max) };
            c.blit(
                &paint_rows(
                    &rows,
                    *scroll,
                    left_width,
                    body_height,
                    self.focus.as_ref(),
                    &self.message_cache,
                ),
                0,
                body_top,
            );
        } else {
            let max = rows.len().saturating_sub(usize::from(body_height));
            self.scroll_limit = max;
            self.scroll = if self.follow {
                max
            } else {
                self.scroll.min(max)
            };
            c.blit(
                &paint_rows(
                    &rows,
                    self.scroll,
                    left_width,
                    body_height,
                    self.focus.as_ref(),
                    &self.message_cache,
                ),
                0,
                body_top,
            );
        }
        if wide {
            let scroll = if self.view == View::Chat {
                self.overview_scroll_limit =
                    side_rows.len().saturating_sub(usize::from(body_height));
                self.overview_scroll = if self.overview_follow {
                    self.overview_scroll_limit
                } else {
                    self.overview_scroll.min(self.overview_scroll_limit)
                };
                self.overview_scroll
            } else {
                let max = side_rows.len().saturating_sub(usize::from(body_height));
                self.scroll_limit = max;
                self.scroll = if self.follow {
                    max
                } else {
                    self.scroll.min(max)
                };
                self.scroll
            };
            c.blit(
                &paint_rows(
                    &side_rows,
                    scroll,
                    side_width,
                    body_height,
                    self.focus.as_ref(),
                    &self.message_cache,
                ),
                split + 1,
                body_top,
            );
            for y in 2..body_end {
                c.text(split, y, "│", Tone::Faint);
            }
            c.text(split, 2, "┬", Tone::Faint);
            c.text(
                split + 2,
                2,
                &format!(
                    " {} ",
                    if self.view == View::Chat {
                        "활동"
                    } else {
                        self.view_title()
                    }
                ),
                Tone::Accent,
            );
            c.button(width - 9, 2, self.label("접기"), Action::Panel, false);
        } else if width >= 110 {
            c.button(width - 14, 2, self.label("패널 열기"), Action::Panel, false);
        }
        // Message copying works during streaming and in read-only viewers;
        // branching is offered only for completed turns.
        if wide || self.view == View::Chat {
            let mut seen = HashSet::new();
            let icons: Vec<_> = c
                .hits
                .iter()
                .filter_map(|hit| {
                    if let Action::SelectEntry(id) = &hit.action
                        && hit.x < left_width
                        && seen.insert(id.clone())
                    {
                        Some((id.clone(), hit.y))
                    } else {
                        None
                    }
                })
                .collect();
            for (id, y) in icons {
                c.button(left_width - 11, y, "⧉", Action::Copy(id.clone()), false);
                if !self.readonly
                    && self
                        .entry_turns
                        .get(&id)
                        .is_some_and(|turn| self.finished_turns.contains(turn))
                {
                    c.button(left_width - 5, y, "⑂", Action::Branch(id.clone()), false);
                    if self.branch_entry.as_ref() == Some(&id) {
                        c.highlight(&Action::Branch(id), Tone::Selected);
                    }
                }
            }
        }
        if !menu.is_empty() {
            let count = menu
                .len()
                .min(usize::from(body_height.saturating_sub(1)))
                .min(5);
            let index = self.menu_index.min(menu.len() - 1);
            let start = index.saturating_sub(count.saturating_sub(1));
            let mut popup = Canvas::new(left_width, count as u16 + 1);
            popup.rule(
                0,
                if self.editor.text.starts_with('$') {
                    "스킬 · ↑↓ 선택 · Enter 추가"
                } else {
                    "명령 · ↑↓ 선택 · Enter 실행"
                },
            );
            for (row, (label, desc, action)) in menu.iter().skip(start).take(count).enumerate() {
                let x = popup.button(2, row as u16 + 1, label, action.clone(), false);
                popup.text(x, row as u16 + 1, self.label(desc), Tone::Muted);
                if start + row == index {
                    popup.highlight(action, Tone::Selected);
                }
            }
            let y = body_end - popup.height;
            c.hits
                .retain(|hit| hit.x >= left_width || hit.y < y || hit.y >= body_end);
            c.blit(&popup, 0, y);
            if wide {
                for y in body_top..body_end {
                    c.text(split, y, "│", Tone::Faint);
                }
            }
        }
        if !self.notice.is_empty() {
            c.text(
                2,
                body_end,
                &self.notice,
                if self.notice_error {
                    Tone::Warning
                } else {
                    Tone::Muted
                },
            );
        }
        if queue_rows > 0 {
            let visible = usize::from(queue_rows - 1);
            let overflow = self.queued.len().saturating_sub(visible);
            let compact_queue = width < 65;
            let header = format!(
                "≡ 대기열 · {}개{}{}",
                self.queued.len(),
                if overflow > 0 {
                    if compact_queue {
                        format!(" +{overflow}")
                    } else {
                        format!(" · +{overflow}개 더")
                    }
                } else {
                    String::new()
                },
                if self.thread.is_none() {
                    " · 세션 연결 대기 · /new"
                } else if self.queue_paused {
                    if compact_queue {
                        " · 재개"
                    } else {
                        " · 일시정지 · 재개"
                    }
                } else if self.force_after_interrupt.is_some() {
                    if compact_queue {
                        " · 중단 대기"
                    } else {
                        " · 중단 확인 대기"
                    }
                } else if self.busy || self.submitted.is_some() {
                    if compact_queue {
                        " · 자동"
                    } else {
                        " · 현재 응답 후 자동 전송"
                    }
                } else {
                    if compact_queue {
                        ""
                    } else {
                        " · 다음 전송 대기"
                    }
                },
            );
            if self.queue_paused {
                c.button(2, queue_top, &header, Action::ResumeQueue, false);
            } else {
                c.text(
                    2,
                    queue_top,
                    &header,
                    if self.force_after_interrupt.is_some() {
                        Tone::Warning
                    } else {
                        Tone::Accent
                    },
                );
            }
            for (index, item) in self.queued.iter().take(visible).enumerate() {
                let y = queue_top + 1 + index as u16;
                c.text(
                    4,
                    y,
                    &format!(
                        "{} {}  {}",
                        index + 1,
                        if index == 0 && self.force_after_interrupt.is_some() {
                            "↯"
                        } else if index == 0 && !self.queue_paused {
                            "▸"
                        } else {
                            "·"
                        },
                        document::command_summary(
                            item.text.lines().next().unwrap_or(""),
                            usize::from(width.saturating_sub(32))
                        )
                    ),
                    if index == 0 {
                        Tone::Normal
                    } else {
                        Tone::Muted
                    },
                );
                if self.turn.is_some() || (!self.busy && self.submitted.is_none()) {
                    c.button(width - 19, y, "즉시", Action::ForceQueued(index), false);
                }
                c.button(width - 8, y, "×", Action::RemoveQueued(index), false);
            }
        }
        c.rule(
            composer_top,
            if self.readonly {
                "열람 전용"
            } else if self.selected_skills.is_empty() {
                "입력"
            } else {
                ""
            },
        );
        let mut chip_x = 2;
        for skill in &self.selected_skills {
            chip_x = c.button(
                chip_x,
                composer_top,
                &format!("${} ×", skill.name),
                Action::RemoveSkill(skill.path.clone()),
                false,
            );
        }
        let input_y = composer_top + 1;
        if self.readonly {
            c.text(
                2,
                input_y,
                self.label("실행과 입력은 메인 창에서"),
                Tone::Muted,
            );
        } else {
            let lines = wrap(&display, input_width);
            let before = wrap(&display[..visible_cursor], input_width);
            let mut row = before.len().saturating_sub(1);
            let mut col = before
                .last()
                .map_or(0, |line| UnicodeWidthStr::width(line.as_str()));
            if col >= input_width {
                row += 1;
                col = 0;
            }
            let start = row.saturating_sub(usize::from(input_rows) - 1);
            for offset in 0..input_rows {
                let y = input_y + offset;
                c.text(2, y, if offset == 0 { "›" } else { " " }, Tone::Accent);
                if self.editor.text.is_empty() && offset == 0 {
                    c.text(4, y, self.label("메시지 입력…"), Tone::Muted);
                } else {
                    c.text(
                        4,
                        y,
                        lines
                            .get(start + usize::from(offset))
                            .map_or("", String::as_str),
                        Tone::Normal,
                    );
                }
                c.hits.push(crate::engine::Hit {
                    x: 1,
                    y,
                    width: width - 2,
                    height: 1,
                    action: Action::Input,
                });
            }
            if self.focus.is_none() && (self.view == View::Chat || wide || self.composer_active) {
                c.cursor = Some((4 + col as u16, input_y + (row - start) as u16));
            }
        }
        c.rule(composer_bottom, "");
        self.paint_meter(&mut c, composer_bottom);
        let compact_footer = width < 70;
        c.text(
            2,
            footer,
            self.label(
                if matches!(
                    self.hovered.as_ref().or(self.focus.as_ref()),
                    Some(Action::Branch(_))
                ) {
                    if compact_footer {
                        "분기 · 원본 유지"
                    } else {
                        "새 분기 · 이 대화까지 포함 · 원본 유지"
                    }
                } else if matches!(
                    self.hovered.as_ref().or(self.focus.as_ref()),
                    Some(Action::Copy(_))
                ) {
                    if compact_footer {
                        "메시지 복사"
                    } else {
                        "메시지 복사 · Ctrl+Shift+C 선택 메시지 복사"
                    }
                } else if !menu.is_empty() {
                    if compact_footer {
                        "↑↓ 선택  ↵ 실행"
                    } else {
                        "↑↓ 선택  Enter 실행  Esc 닫기"
                    }
                } else if self.focus.is_some()
                    || (self.view != View::Chat && !wide && !self.composer_active)
                {
                    if compact_footer {
                        "Tab 이동  ↵ 선택"
                    } else {
                        "Tab 이동  Enter 선택  Esc 입력"
                    }
                } else if self.busy {
                    if compact_footer {
                        "↵ 대기열  ^C"
                    } else {
                        "Enter 대기열 추가  Ctrl+C 중단"
                    }
                } else {
                    if compact_footer {
                        "^V 붙여넣기"
                    } else {
                        "Ctrl+V 붙여넣기  Ctrl+Q 종료"
                    }
                },
            ),
            Tone::Muted,
        );
        if !self.readonly {
            let (label, action) = if self.busy {
                ("■ 중단", Action::Interrupt)
            } else {
                ("↵ 전송", Action::Send)
            };
            let button_width = UnicodeWidthStr::width(label) as u16 + 2;
            c.button(
                width.saturating_sub(button_width + 2),
                footer,
                label,
                action,
                false,
            );
        }
        if let Some(id) = &self.branch_entry {
            c.highlight(&Action::SelectEntry(id.clone()), Tone::Selected);
        }
        if let Some(action) = &self.hovered {
            c.highlight(action, Tone::Hover);
        }
        if let Some(action) = &self.focus {
            c.highlight(action, Tone::Selected);
        }
        if !self.input_forms.is_empty() {
            c.button(
                2,
                composer_top,
                &format!("? 질문 {} · 열기", self.input_forms.len()),
                Action::Questions,
                false,
            );
        }
        if self.trust_prompt.is_none() && self.approvals.is_empty() {
            self.paint_questions(&mut c);
        }
        self.paint_decision_popup(&mut c, body_top, body_height);
        c
    }
    fn paint_decision_popup(&self, canvas: &mut Canvas, body_top: u16, body_height: u16) {
        if let Some(path) = &self.trust_prompt {
            let width = canvas.width.saturating_sub(6).min(74);
            let height = body_height.min(9);
            let mut popup = Canvas::new(width, height);
            for cell in &mut popup.cells {
                cell.background = crate::engine::Background::Code;
            }
            popup.rule(0, "! 작업 폴더 신뢰 확인");
            let compact = height < 8;
            for (i, line) in wrap(path, usize::from(width.saturating_sub(4)))
                .iter()
                .take(if compact { 1 } else { 2 })
                .enumerate()
            {
                popup.text(2, i as u16 + 1, line, Tone::Normal);
            }
            if compact {
                if height > 2 {
                    popup.text(2, 2, "신뢰: 기존 Codex 정책 적용", Tone::Warning);
                }
                if height > 3 {
                    popup.button(2, 3, "1 신뢰", Action::TrustWorkspace(true), false);
                }
                if height > 4 {
                    popup.button(16, 3, "2 제한", Action::TrustWorkspace(false), false);
                }
            } else {
                popup.text(
                    2,
                    4,
                    "신뢰하면 기존 Codex 권한 정책이 적용됩니다.",
                    Tone::Warning,
                );
                popup.button(2, 6, "1 신뢰하고 시작", Action::TrustWorkspace(true), false);
                popup.button(
                    23,
                    6,
                    "2 제한 모드로 시작",
                    Action::TrustWorkspace(false),
                    false,
                );
            }
            if self.trust_pending && height > 1 {
                popup.text(2, height - 1, "◉ 설정 확인 중…", Tone::Warning);
            }
            canvas.blit(
                &popup,
                (canvas.width - width) / 2,
                body_top + body_height.saturating_sub(height) / 2,
            );
        } else if !self.approval_popup_dismissed
            && let Some(approval) = self.approvals.iter().find(|a| !a.answering)
        {
            let width = canvas.width.saturating_sub(6).min(72);
            let height = body_height.min(11);
            let mut popup = Canvas::new(width, height);
            for cell in &mut popup.cells {
                cell.background = crate::engine::Background::Code;
            }
            popup.rule(0, &format!("! {} · 승인 필요", approval.title));
            let lines = wrap(&approval.summary, usize::from(width.saturating_sub(4)));
            let compact = height < 8;
            for (index, line) in lines
                .iter()
                .filter(|line| !line.is_empty())
                .take(if compact { 1 } else { 3 })
                .enumerate()
            {
                popup.text(2, 1 + index as u16, line, Tone::Normal);
            }
            for (index, choice) in approval.choices.iter().enumerate() {
                let y = (if compact { 2 } else { 4 }) + index as u16;
                if y >= height.saturating_sub(1) {
                    break;
                }
                popup.button(
                    2,
                    y,
                    &choice.label,
                    Action::Decide(approval.id.to_string(), index),
                    false,
                );
            }
            if height >= 6 {
                popup.button(2, height - 2, "상세 보기", Action::Approvals, false);
                popup.button(18, height - 2, "나중에", Action::DismissApproval, false);
            } else if height >= 2 {
                popup.text(2, height - 1, "Esc 닫기 · 승인 탭에서 자세히", Tone::Muted);
            }
            canvas.blit(
                &popup,
                (canvas.width - width) / 2,
                body_top + body_height.saturating_sub(height) / 2,
            );
        }
    }
    fn view_title(&self) -> &'static str {
        match self.view {
            View::Chat => "대화",
            View::Agents => "활동",
            View::Diff => "변경 검토",
            View::Approvals => "승인 요청",
            View::Permissions => "권한 관리",
            View::Help => "도움말",
            View::Skills => "스킬 선택",
            View::Models => "모델 선택",
            View::Sessions => "세션 관리",
            View::Settings => "설정",
        }
    }
    fn overview_rows(&self, width: usize) -> Vec<(String, Tone, Option<Action>)> {
        let mut rows = Vec::new();
        if !self.usage.is_empty() {
            rows.push((self.usage.clone(), Tone::Muted, None));
        }
        if !self.approvals.is_empty() {
            rows.push((
                format!("승인 대기 {}", self.approvals.len()),
                Tone::Normal,
                Some(Action::Approvals),
            ));
        }
        if self.has_reported_changes() {
            rows.push((
                format!("파일 변경 {} · 내용 보기 ▸", self.reported_change_label()),
                Tone::Accent,
                Some(Action::Diff),
            ));
            rows.push(("에이전트가 보고한 변경 내용".into(), Tone::Muted, None));
        }
        rows.extend(self.task_rows(width, false));
        if self.agents.is_empty() && self.tasks.is_empty() {
            rows.push(("아직 하위 에이전트가 없습니다".into(), Tone::Muted, None));
        }
        for (index, agent) in self.agents.iter().enumerate() {
            rows.push((
                format!("{} · {}", agent.name, status_label(&agent.status)),
                Tone::Normal,
                Some(Action::Agents),
            ));
            rows.push((
                "새 터미널 열기".into(),
                Tone::Muted,
                Some(Action::OpenAgent(index)),
            ));
        }
        if let Some(tool) = self
            .entries
            .iter()
            .rev()
            .find(|entry| matches!(entry.kind, Kind::Tool | Kind::Change))
        {
            for line in wrap(&tool.title, width) {
                rows.push((line, Tone::Muted, None));
            }
        }
        rows
    }
    fn content_rows_for(&self, view: &View, width: usize) -> Vec<(String, Tone, Option<Action>)> {
        let mut rows = Vec::new();
        match view {
            View::Chat => {
                append_entries(
                    &mut rows,
                    &self.entries,
                    width,
                    &self.tool_pages,
                    &self.message_cache,
                    self.preferences.language,
                );
            }
            View::Diff => {
                rows.push(("에이전트가 보고한 파일 변경".into(), Tone::Accent, None));
                rows.push((
                    "파일 수정 전후 비교 · 전체 작업 폴더의 Git diff와 다를 수 있습니다".into(),
                    Tone::Muted,
                    None,
                ));
                rows.push(("← 대화로 돌아가기".into(), Tone::Muted, Some(Action::Back)));
                rows.push((String::new(), Tone::Normal, None));
                if let Some(id) = &self.selected_diff_entry {
                    if let Some(entry) = self.entries.iter().find(|entry| &entry.id == id) {
                        rows.extend(
                            document::diff_rows(&entry.body, width)
                                .into_iter()
                                .map(|(line, tone)| (line, tone, None)),
                        );
                    }
                } else if self.diff.is_empty() {
                    for e in self.entries.iter().filter(|e| {
                        e.kind == Kind::Change && self.recent_change_ids.contains(&e.id)
                    }) {
                        rows.extend(
                            document::diff_rows(&e.body, width)
                                .into_iter()
                                .map(|(line, tone)| (line, tone, None)),
                        );
                    }
                    if rows.len() == 4 {
                        rows.push(("최근 작업에 보고된 변경이 없습니다. 이전 변경은 대화 기록에서 확인하세요.".into(), Tone::Muted, None));
                    }
                } else {
                    rows.extend(
                        document::diff_rows(&self.diff, width)
                            .into_iter()
                            .map(|(line, tone)| (line, tone, None)),
                    );
                }
            }
            View::Agents => {
                if self.selected_task.is_none() {
                    rows.extend(self.meter_rows(width));
                    rows.push((String::new(), Tone::Normal, None));
                }
                rows.extend(self.task_rows(width, true));
                rows.push(("연결된 runtime 활동".into(), Tone::Accent, None));
                if self.agents.is_empty() {
                    rows.push((
                        "아직 생성된 하위 에이전트가 없습니다.".into(),
                        Tone::Muted,
                        None,
                    ));
                }
                for (index, agent) in self.agents.iter().enumerate() {
                    rows.push((
                        format!("{} · {}", agent.name, agent.status),
                        Tone::Normal,
                        Some(Action::Agent(index)),
                    ));
                    rows.push((
                        "새 터미널 열기".into(),
                        Tone::Accent,
                        Some(Action::OpenAgent(index)),
                    ));
                }
                if let Some(agent) = self.agents.get(self.selected_agent) {
                    rows.push((format!("선택: {}", agent.id), Tone::Muted, None));
                    for line in wrap(&agent.detail, width) {
                        rows.push((line, Tone::Normal, None));
                    }
                }
                append_entries(
                    &mut rows,
                    &self.agent_entries,
                    width,
                    &self.tool_pages,
                    &self.message_cache,
                    self.preferences.language,
                );
            }
            View::Help => {
                rows.push(("명령 안내".into(), Tone::Accent, None));
                for (name, description) in COMMANDS {
                    rows.push((
                        name.to_string(),
                        Tone::Accent,
                        Some(Action::Command(name.to_string())),
                    ));
                    for line in wrap(description, width) {
                        rows.push((line, Tone::Muted, None));
                    }
                }
                for line in wrap(
                    "/ 명령 메뉴 · $ 스킬 검색\nTab/Shift+Tab 버튼 이동 · Enter 선택\nEsc 대화와 입력으로 복귀\nAlt+Enter 줄바꿈 · Ctrl+C 중단\nPageUp/PageDown 기록 · Ctrl+Q 종료\n마우스 오버 강조 · 클릭 실행 · 휠 스크롤",
                    width,
                ) {
                    rows.push((line, Tone::Normal, None));
                }
            }
            View::Skills => {
                rows.push((
                    if self.preferences.language == Language::English {
                        format!("{} installed skills", self.skills.len())
                    } else {
                        format!("설치된 스킬 {}개", self.skills.len())
                    },
                    Tone::Accent,
                    None,
                ));
                if !self.skills_loaded {
                    rows.push(("스킬 목록을 불러오는 중…".into(), Tone::Muted, None));
                } else if self.skills.is_empty() {
                    rows.push((
                        "이 작업 폴더에 사용 가능한 스킬이 없습니다.".into(),
                        Tone::Muted,
                        None,
                    ));
                }
                for skill in &self.skills {
                    rows.push((
                        format!("${}", skill.name),
                        Tone::Accent,
                        Some(Action::Skill(skill.path.clone())),
                    ));
                    for line in wrap(&skill.description, width) {
                        rows.push((line, Tone::Muted, None));
                    }
                    rows.push((String::new(), Tone::Normal, None));
                }
                for error in &self.skill_errors {
                    for line in wrap(error, width) {
                        rows.push((line, Tone::Warning, None));
                    }
                }
            }
            View::Settings => {
                let english = self.preferences.language == Language::English;
                rows.push((
                    if english {
                        "Manage permissions →"
                    } else {
                        "권한 및 승인 정책 →"
                    }
                    .into(),
                    Tone::Accent,
                    Some(Action::Command("/permissions".into())),
                ));
                rows.push((String::new(), Tone::Normal, None));
                rows.push((
                    if english { "Language" } else { "언어" }.into(),
                    Tone::Muted,
                    None,
                ));
                rows.push((
                    format!("{}한국어", if !english { "● " } else { "○ " }),
                    Tone::Normal,
                    Some(Action::Setting("korean".into())),
                ));
                rows.push((
                    format!("{}English", if english { "● " } else { "○ " }),
                    Tone::Normal,
                    Some(Action::Setting("english".into())),
                ));
                rows.push((String::new(), Tone::Normal, None));
                rows.push((
                    if english {
                        "Task panel on startup"
                    } else {
                        "시작 시 작업 패널"
                    }
                    .into(),
                    Tone::Muted,
                    None,
                ));
                rows.push((
                    if english {
                        if self.preferences.panel_open {
                            "● Show"
                        } else {
                            "○ Hide"
                        }
                    } else if self.preferences.panel_open {
                        "● 펼침"
                    } else {
                        "○ 접힘"
                    }
                    .into(),
                    Tone::Normal,
                    Some(Action::Setting("panel".into())),
                ));
                rows.push((String::new(), Tone::Normal, None));
                rows.push((
                    if english {
                        "Saved for this workspace"
                    } else {
                        "이 작업 폴더에 저장됩니다"
                    }
                    .into(),
                    Tone::Muted,
                    None,
                ));
            }
            View::Permissions => {
                let english = self.preferences.language == Language::English;
                rows.push((
                    format!(
                        "{}: {}",
                        if english {
                            "Folder trust"
                        } else {
                            "폴더 신뢰"
                        },
                        match self.workspace_trust {
                            Some(true) =>
                                if english {
                                    "Trusted by Codex"
                                } else {
                                    "Codex에서 신뢰됨"
                                },
                            Some(false) =>
                                if english {
                                    "Untrusted / restricted session"
                                } else {
                                    "미신뢰 · 제한된 세션"
                                },
                            None =>
                                if english {
                                    "Unknown"
                                } else {
                                    "확인되지 않음"
                                },
                        }
                    ),
                    Tone::Warning,
                    None,
                ));
                rows.push((String::new(), Tone::Normal, None));
                rows.push((
                    if english {
                        "Session permissions · future turns"
                    } else {
                        "세션 권한 · 다음 작업부터 적용"
                    }
                    .into(),
                    Tone::Accent,
                    None,
                ));
                rows.push((
                    if english {
                        "Sandbox / file access"
                    } else {
                        "샌드박스 · 파일 접근"
                    }
                    .into(),
                    Tone::Muted,
                    None,
                ));
                if self.sandbox_mode.is_none() {
                    rows.push((
                        if english {
                            "Current: Codex default (not queried)"
                        } else {
                            "현재: Codex 기본 설정 · 미확인"
                        }
                        .into(),
                        Tone::Warning,
                        None,
                    ));
                }
                if let Some(mode) = self.sandbox_mode.as_deref()
                    && !matches!(mode, "readOnly" | "workspaceWrite" | "dangerFullAccess")
                {
                    rows.push((
                        format!(
                            "{}: {mode}",
                            if english {
                                "Core mode"
                            } else {
                                "코어 지정 모드"
                            }
                        ),
                        Tone::Warning,
                        None,
                    ));
                }
                for (value, korean, en) in [
                    ("readOnly", "읽기 전용", "Read-only"),
                    (
                        "workspaceWrite",
                        "작업 폴더 쓰기 · 네트워크 차단",
                        "Workspace write · no network",
                    ),
                    (
                        "dangerFullAccess",
                        "전체 접근 · 샌드박스 해제 (위험)",
                        "Full access · no sandbox (danger)",
                    ),
                ] {
                    let selected = self.sandbox_mode.as_deref() == Some(value);
                    rows.push((
                        format!(
                            "{} {}",
                            if selected { "●" } else { "○" },
                            if english { en } else { korean }
                        ),
                        if value == "dangerFullAccess" {
                            Tone::Danger
                        } else if selected {
                            Tone::Success
                        } else {
                            Tone::Normal
                        },
                        (!self.readonly && self.permission_pending.is_none())
                            .then(|| Action::Permission("sandbox".into(), value.into())),
                    ));
                }
                rows.push((String::new(), Tone::Normal, None));
                rows.push((
                    if english {
                        "Approval requests"
                    } else {
                        "승인 요청 정책"
                    }
                    .into(),
                    Tone::Muted,
                    None,
                ));
                if self.approval_mode.is_none() {
                    rows.push((
                        if english {
                            "Current: Codex default (not queried)"
                        } else {
                            "현재: Codex 기본 설정 · 미확인"
                        }
                        .into(),
                        Tone::Warning,
                        None,
                    ));
                }
                if self.approval_mode.as_deref() == Some("granular") {
                    rows.push((
                        if english {
                            "Core mode: granular rules"
                        } else {
                            "코어 지정 모드: 세분화된 규칙"
                        }
                        .into(),
                        Tone::Warning,
                        None,
                    ));
                }
                for (value, korean, en) in [
                    (
                        "untrusted",
                        "보수적 · 신뢰하지 않는 작업 확인",
                        "Untrusted · conservative prompts",
                    ),
                    ("on-request", "필요시 사용자 승인 요청", "Ask when needed"),
                    (
                        "never",
                        "승인 요청 안 함 · 요청이 필요한 작업은 실패할 수 있음",
                        "Never ask · restricted actions may fail",
                    ),
                ] {
                    let selected = self.approval_mode.as_deref() == Some(value);
                    rows.push((
                        format!(
                            "{} {}",
                            if selected { "●" } else { "○" },
                            if english { en } else { korean }
                        ),
                        if value == "never" {
                            Tone::Warning
                        } else if selected {
                            Tone::Success
                        } else {
                            Tone::Normal
                        },
                        (!self.readonly && self.permission_pending.is_none())
                            .then(|| Action::Permission("approval".into(), value.into())),
                    ));
                }
                if let Some((kind, value)) = &self.permission_pending {
                    rows.push((String::new(), Tone::Normal, None));
                    rows.push((
                        format!(
                            "◉ {kind}: {value} · {}",
                            if english {
                                "waiting for runtime"
                            } else {
                                "코어 응답 대기"
                            }
                        ),
                        Tone::Warning,
                        None,
                    ));
                }
                if let Some((kind, value)) = &self.permission_confirm {
                    rows.push((String::new(), Tone::Normal, None));
                    rows.push((format!("! {kind}: {value}"), Tone::Danger, None));
                    rows.push((
                        if english {
                            "This may disable a safety boundary. Apply?"
                        } else {
                            "안전 경계를 약화할 수 있습니다. 적용할까요?"
                        }
                        .into(),
                        Tone::Warning,
                        None,
                    ));
                    rows.push((
                        if english {
                            "Confirm change"
                        } else {
                            "변경 확인"
                        }
                        .into(),
                        Tone::Danger,
                        Some(Action::ConfirmPermission(true)),
                    ));
                    rows.push((
                        if english { "Cancel" } else { "취소" }.into(),
                        Tone::Normal,
                        Some(Action::ConfirmPermission(false)),
                    ));
                }
                rows.push((String::new(), Tone::Normal, None));
                rows.push((if english { "Changes affect future turns, not the running tool or a pending approval." } else { "변경은 다음 작업부터 적용되며 현재 실행 중인 도구나 승인 요청을 바꾸지 않습니다." }.into(), Tone::Muted, None));
            }
            View::Sessions => {
                let english = self.preferences.language == Language::English;
                let lock_reason = self.session_switch_block_reason();
                let locked = lock_reason.is_some();
                rows.push((
                    if english {
                        "Current conversation"
                    } else {
                        "현재 대화"
                    }
                    .into(),
                    Tone::Accent,
                    None,
                ));
                rows.push((
                    format!(
                        "● {}",
                        one_line(&self.session_title, width.saturating_sub(5))
                    ),
                    Tone::Normal,
                    None,
                ));
                rows.push((
                    (if self.entries.is_empty() {
                        if english {
                            "First message not sent · no saved conversation yet"
                        } else {
                            "첫 메시지 전 · 아직 저장되지 않았을 수 있음"
                        }
                    } else if english {
                        "Closing stops owned work; saved history remains"
                    } else {
                        "종료 시 실행을 정리하고 저장된 기록은 유지됨"
                    })
                    .into(),
                    Tone::Muted,
                    None,
                ));
                rows.push((
                    if english {
                        "The next launch reopens your most recent conversation."
                    } else {
                        "다음 실행 시 새 연결로 최근 기록을 다시 엽니다."
                    }
                    .into(),
                    Tone::Muted,
                    None,
                ));
                if !self.readonly {
                    rows.push((String::new(), Tone::Normal, None));
                    rows.push((
                        if english {
                            "+ New conversation"
                        } else {
                            "+ 새 대화"
                        }
                        .into(),
                        if locked { Tone::Muted } else { Tone::Accent },
                        Some(Action::NewSession),
                    ));
                }
                rows.push((String::new(), Tone::Normal, None));
                rows.push((
                    if english {
                        "Recent conversations"
                    } else {
                        "최근 대화"
                    }
                    .into(),
                    Tone::Accent,
                    None,
                ));
                rows.push((
                    if english {
                        if self.sessions_loaded {
                            "↻ Refresh"
                        } else {
                            "◉ Refreshing…"
                        }
                    } else {
                        if self.sessions_loaded {
                            "↻ 새로고침"
                        } else {
                            "◉ 불러오는 중…"
                        }
                    }
                    .into(),
                    Tone::Muted,
                    self.sessions_loaded.then_some(Action::RefreshSessions),
                ));
                if !self.sessions_loaded && self.sessions.is_empty() {
                    rows.push((
                        if english {
                            "Loading…"
                        } else {
                            "불러오는 중…"
                        }
                        .into(),
                        Tone::Muted,
                        None,
                    ));
                } else if self.sessions_loaded
                    && self
                        .sessions
                        .iter()
                        .all(|session| self.thread.as_deref() == Some(session.id.as_str()))
                {
                    rows.push((
                        if english {
                            "No saved conversations in this workspace"
                        } else {
                            "이 폴더에 저장된 다른 대화가 없습니다"
                        }
                        .into(),
                        Tone::Muted,
                        None,
                    ));
                }
                if locked {
                    rows.push((
                        if english {
                            "Switch is currently unavailable; select a conversation to see why"
                                .into()
                        } else {
                            format!("! {}", lock_reason.unwrap_or_default())
                        },
                        Tone::Warning,
                        None,
                    ));
                }
                if self.readonly {
                    rows.push((
                        "열람 전용 창입니다. 메인 창에서 세션을 전환하세요.".into(),
                        Tone::Muted,
                        None,
                    ));
                }
                for (index, session) in self
                    .sessions
                    .iter()
                    .filter(|s| self.thread.as_deref() != Some(s.id.as_str()))
                    .enumerate()
                {
                    rows.push((
                        format!(
                            "{}. ○ {}",
                            index + 1,
                            one_line(&session.title, width.saturating_sub(9))
                        ),
                        if locked { Tone::Muted } else { Tone::Accent },
                        (!self.readonly).then(|| Action::Resume(session.id.clone())),
                    ));
                    let activity = last_activity(session.updated_at);
                    let status = match session.status.as_str() {
                        "active" => {
                            if english {
                                "● Running"
                            } else {
                                "● 실행 중"
                            }
                        }
                        "systemError" => {
                            if english {
                                "! Error"
                            } else {
                                "! 오류"
                            }
                        }
                        _ => "",
                    };
                    let meta = format!("  {activity}  {status}");
                    if !meta.trim().is_empty() {
                        rows.push((meta, Tone::Muted, None));
                    }
                }
                if self
                    .pending
                    .values()
                    .any(|method| *method == Operation::MoreSessions)
                {
                    rows.push((
                        if english {
                            "◉ Loading more…"
                        } else {
                            "◉ 이전 대화 불러오는 중…"
                        }
                        .into(),
                        Tone::Muted,
                        None,
                    ));
                } else if self.sessions_next_cursor.is_some() && self.sessions_loaded {
                    rows.push((
                        if english {
                            "More conversations ▸"
                        } else {
                            "이전 대화 더 보기 ▸"
                        }
                        .into(),
                        Tone::Muted,
                        Some(Action::MoreSessions),
                    ));
                }
            }
            View::Models => {
                rows.extend(self.effort_rows(width));
                rows.push((String::new(), Tone::Normal, None));
                rows.push(("다음 메시지에 사용할 모델".into(), Tone::Accent, None));
                if self.models.is_empty() {
                    rows.push(("모델 목록을 불러오는 중…".into(), Tone::Muted, None));
                }
                for model in &self.models {
                    rows.push((
                        format!(
                            "{}{}",
                            if self.model == *model { "● " } else { "○ " },
                            model
                        ),
                        Tone::Normal,
                        Some(Action::Model(model.clone())),
                    ));
                }
            }
            View::Approvals => {
                if self.approvals.is_empty() {
                    rows.push(("대기 중인 승인 요청이 없습니다.".into(), Tone::Muted, None));
                }
                for (request_index, approval) in self.approvals.iter().enumerate() {
                    rows.push((
                        format!("{}. {}", request_index + 1, approval.title),
                        Tone::Warning,
                        None,
                    ));
                    // Keep decisions reachable before lengthy tool details.
                    if approval.answering {
                        rows.push(("응답 전송됨 · 코어 확인 대기".into(), Tone::Muted, None));
                    } else {
                        for (choice_index, choice) in approval.choices.iter().enumerate() {
                            rows.push((
                                format!("  {}. {}", choice_index + 1, choice.label),
                                Tone::Warning,
                                Some(Action::Decide(approval.id.to_string(), choice_index)),
                            ));
                        }
                    }
                    for line in wrap(&approval.summary, width) {
                        if !line.is_empty() {
                            rows.push((line, Tone::Normal, None));
                        }
                    }
                    rows.push((
                        if approval.expanded {
                            "▾ 상세 내용 접기"
                        } else {
                            "▸ 전체 요청·도구 출력 보기"
                        }
                        .into(),
                        Tone::Accent,
                        Some(Action::ToggleApproval(approval.id.to_string())),
                    ));
                    if approval.expanded {
                        for line in wrap(&approval.detail, width) {
                            rows.push((line, Tone::Muted, None));
                        }
                    }
                    rows.push((String::new(), Tone::Normal, None));
                }
            }
        }
        if rows.is_empty() {
            rows.push((
                self.label("무엇을 만들까요? 아래에 지시를 입력하세요.")
                    .into(),
                Tone::Muted,
                None,
            ));
        }
        if *view != View::Chat {
            for row in &mut rows {
                row.0 = self.label(&row.0).to_string();
            }
        }
        rows
    }
}
fn status_label(status: &str) -> &str {
    match status {
        "idle" => "준비됨",
        "active" | "running" | "inProgress" | "working" => "실행 중",
        "completed" => "완료",
        "interrupted" => "중단됨",
        "failed" => "실패",
        _ => status,
    }
}
fn status_style(status: &str) -> (&'static str, Tone) {
    match status {
        "active" | "running" | "inProgress" | "working" | "실행 중" | "전송 중"
        | "중단 요청 중" => ("◉", Tone::Accent),
        "idle" | "completed" | "완료" | "대기" | "준비됨" => ("✓", Tone::Success),
        "failed" | "실패" | "연결 끊김" => ("!", Tone::Danger),
        "interrupted" | "중단됨" => ("■", Tone::Warning),
        _ => ("○", Tone::Muted),
    }
}
fn item_style(status: &str) -> (&'static str, Tone) {
    match status {
        "completed" | "complete" | "success" => ("✓", Tone::Success),
        "failed" | "error" => ("!", Tone::Danger),
        "interrupted" | "cancelled" => ("■", Tone::Warning),
        "inProgress" | "running" | "working" => ("◉", Tone::Accent),
        _ => ("○", Tone::Muted),
    }
}
fn paint_rows(
    rows: &[(String, Tone, Option<Action>)],
    scroll: usize,
    width: u16,
    height: u16,
    focus: Option<&Action>,
    message_cache: &MessageCache,
) -> Canvas {
    let mut c = Canvas::new(width, height);
    for (y, (text, tone, action)) in rows
        .iter()
        .skip(scroll)
        .take(usize::from(height))
        .enumerate()
    {
        let mut line = Canvas::new(width.saturating_sub(4), 1);
        if let Some(action) = action {
            let target = match action {
                Action::MessageRow(id, _, _) => Action::SelectEntry(id.clone()),
                _ => action.clone(),
            };
            if matches!(action, Action::Resume(_)) {
                // A session is a selectable list row, including its smaller
                // last-activity line. The full pane width is a click target.
                document::paint(&mut line, text, *tone);
                let has_metadata = y + 1 < usize::from(height)
                    && rows
                        .get(scroll + y + 1)
                        .is_some_and(|next| next.2.is_none() && next.0.starts_with("  "));
                line.hits.push(crate::engine::Hit {
                    x: 0,
                    y: 0,
                    width: line.width,
                    height: if has_metadata { 2 } else { 1 },
                    action: target.clone(),
                });
            } else if matches!(action, Action::SelectEntry(_) | Action::MessageRow(_, _, _)) {
                let cache = message_cache.borrow();
                let styled = match action {
                    Action::MessageRow(id, index, width) => cache
                        .get(&(id.clone(), *width))
                        .and_then(|entry| entry.rows.get(*index)),
                    _ => None,
                };
                if let Some(styled) = styled {
                    let prefix = if text.starts_with("● ") {
                        "● "
                    } else {
                        "  "
                    };
                    document::paint_message(&mut line, styled, prefix);
                } else {
                    document::paint(&mut line, text, *tone);
                }
                if text.starts_with("› ") {
                    line.text(0, 0, "›", Tone::User);
                } else if text.starts_with("● ") {
                    line.text(0, 0, "●", Tone::Accent);
                }
                line.hits.push(crate::engine::Hit {
                    x: 0,
                    y: 0,
                    width: unicode_width::UnicodeWidthStr::width(text.as_str())
                        .min(usize::from(line.width)) as u16,
                    height: 1,
                    action: target.clone(),
                });
            } else {
                line.button(0, 0, text, action.clone(), false);
                if matches!(action, Action::Toggle(_)) {
                    if text.starts_with("▸ ✓") || text.starts_with("▾ ✓") {
                        line.text(3, 0, "✓", Tone::Success);
                    } else if text.starts_with("▸ !") || text.starts_with("▾ !") {
                        line.text(3, 0, "!", Tone::Danger);
                    } else if text.starts_with("▸ ◉") || text.starts_with("▾ ◉") {
                        line.text(3, 0, "◉", Tone::Accent);
                    }
                }
                if matches!(
                    action,
                    Action::Permission(_, _) | Action::ConfirmPermission(_)
                ) {
                    line.text(1, 0, text, *tone);
                } else if matches!(action, Action::Decide(_, _)) {
                    let decision_tone = if text.contains("거절") || text.contains("취소") {
                        Tone::Danger
                    } else {
                        Tone::Success
                    };
                    line.text(1, 0, text, decision_tone);
                }
            }
            if focus == Some(&target) {
                line.highlight(&target, Tone::Selected);
            }
        } else {
            document::paint(&mut line, text, *tone);
        }
        c.blit(&line, 2, y as u16);
    }
    c
}
fn append_entries(
    rows: &mut Vec<(String, Tone, Option<Action>)>,
    entries: &[Entry],
    width: usize,
    tool_pages: &HashMap<String, usize>,
    message_cache: &MessageCache,
    language: Language,
) {
    // An assistant's commentary, tools and final answer form one visual group.
    // Start another group only on a speaker change, including consecutive user turns.
    let mut loom_group = false;
    for entry in entries {
        let is_loom = matches!(entry.kind, Kind::Assistant | Kind::Tool | Kind::Change);
        if entry.kind == Kind::User || (is_loom && !loom_group) {
            let label = if entry.kind == Kind::User {
                if language == Language::English {
                    "You"
                } else {
                    "사용자"
                }
            } else {
                "Loom"
            };
            let start = format!("── {label} ");
            let length = unicode_width::UnicodeWidthStr::width(start.as_str());
            rows.push((
                format!("{start}{}", "─".repeat(width.saturating_sub(length))),
                if entry.kind == Kind::User {
                    Tone::User
                } else {
                    Tone::Faint
                },
                None,
            ));
        }
        loom_group = is_loom;

        match entry.kind {
            Kind::Tool | Kind::Change => {
                let (symbol, state_tone) = item_style(&entry.status);
                let completed_tool = entry.kind == Kind::Tool
                    && matches!(entry.status.as_str(), "completed" | "complete" | "success");
                rows.push((
                    format!(
                        "{} {} {} · {}",
                        if entry.expanded { "▾" } else { "▸" },
                        symbol,
                        document::command_summary(&entry.title, width.saturating_sub(14)),
                        status_label(&entry.status)
                    ),
                    if completed_tool {
                        Tone::Muted
                    } else {
                        state_tone
                    },
                    Some(Action::Toggle(entry.id.clone())),
                ));
                let has_diff = entry.kind == Kind::Change || document::is_diff(&entry.body);
                if entry.expanded || has_diff {
                    if entry.expanded && entry.kind == Kind::Tool {
                        rows.push((
                            format!(
                                "$ {}",
                                document::command_summary(&entry.title, width.saturating_sub(3))
                            ),
                            Tone::Muted,
                            None,
                        ));
                    }
                    const PAGE_SIZE: usize = 24;
                    let page = if entry.expanded {
                        tool_pages.get(&entry.id).copied().unwrap_or(0)
                    } else {
                        0
                    };
                    let limit = if entry.expanded { PAGE_SIZE } else { 12 };
                    let (output, has_next) =
                        document::output_page(&entry.body, width, page, limit, has_diff);
                    if entry.expanded && page > 0 {
                        rows.push((
                            format!("◂ 이전 {PAGE_SIZE}줄"),
                            Tone::Muted,
                            Some(Action::OutputPage(entry.id.clone(), false)),
                        ));
                    }
                    rows.extend(output.into_iter().map(|(line, tone)| {
                        let tone = if completed_tool && !has_diff {
                            Tone::Muted
                        } else {
                            tone
                        };
                        (line, tone, None)
                    }));
                    if has_next {
                        rows.push((
                            if entry.expanded {
                                format!("다음 {PAGE_SIZE}줄 ▸")
                            } else {
                                "… 펼쳐서 계속 보기".into()
                            },
                            Tone::Muted,
                            Some(if entry.expanded {
                                Action::OutputPage(entry.id.clone(), true)
                            } else {
                                Action::Toggle(entry.id.clone())
                            }),
                        ));
                    }
                }
                if entry.kind == Kind::Change && !entry.body.is_empty() {
                    rows.push((
                        "변경 내용 전체 보기 ▸".into(),
                        Tone::Muted,
                        Some(Action::DiffEntry(entry.id.clone())),
                    ));
                }
            }
            Kind::Assistant => {
                // Inline copy / branch controls occupy the rightmost cells.
                let width = width.saturating_sub(12);
                let mut cache = message_cache.borrow_mut();
                let cached =
                    cache
                        .entry((entry.id.clone(), width))
                        .or_insert_with(|| CachedMessage {
                            body: entry.body.clone(),
                            rows: document::styled_message_rows(&entry.body, width),
                        });
                if cached.body != entry.body {
                    cached.body.clone_from(&entry.body);
                    cached.rows = document::styled_message_rows(&entry.body, width);
                }
                append_message(rows, entry, &cached.rows, Some(width));
            }
            _ => {
                let available = width.saturating_sub(if entry.kind == Kind::User { 12 } else { 2 });
                let rendered: Vec<_> = wrap(&entry.body, available)
                    .into_iter()
                    .map(|line| document::StyledRow::plain(line, Tone::Normal))
                    .collect();
                append_message(rows, entry, &rendered, None);
            }
        }
        rows.push((String::new(), Tone::Normal, None));
    }
}
fn append_message(
    rows: &mut Vec<(String, Tone, Option<Action>)>,
    entry: &Entry,
    rendered: &[document::StyledRow],
    cache_width: Option<usize>,
) {
    let marker = match entry.kind {
        Kind::User => "›",
        Kind::Assistant => "●",
        _ => "·",
    };
    for (index, row) in rendered.iter().enumerate() {
        let (line, tone) = (&row.text, row.tone);
        rows.push((
            format!("{} {}", if index == 0 { marker } else { " " }, line),
            tone,
            if let Some(width) = cache_width {
                Some(Action::MessageRow(entry.id.clone(), index, width))
            } else if matches!(entry.kind, Kind::User | Kind::Assistant) {
                Some(Action::SelectEntry(entry.id.clone()))
            } else {
                None
            },
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn markdown_display_keeps_copy_source_and_interactive_message_controls() {
        let mut app = App::new(false);
        let source = "## 제목\n\n**강조**와 `함수()`";
        app.entries.push(Entry {
            id: "markdown".into(),
            kind: Kind::Assistant,
            title: "Loom".into(),
            body: source.into(),
            status: "completed".into(),
            expanded: false,
        });
        let canvas = app.render(80, 24);
        let plain = (0..canvas.height)
            .map(|y| canvas.plain_line(y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            plain.contains("제목") && !plain.contains("## 제목") && !plain.contains("**강조**")
        );
        assert!(canvas.cells.iter().any(|cell| cell.tone == Tone::Strong));
        assert!(canvas.cells.iter().any(|cell| cell.tone == Tone::InlineCode
            && cell.background == crate::engine::Background::Code));
        assert!(
            canvas
                .hits
                .iter()
                .any(|hit| hit.action == Action::SelectEntry("markdown".into()))
        );
        assert_eq!(
            app.activate(Action::Copy("markdown".into())),
            Some(Intent::Copy(source.into()))
        );
    }
    #[test]
    fn conversation_dividers_group_tools_and_follow_language_and_width() {
        let mut app = App::new(false);
        for (id, kind, body) in [
            ("u1", Kind::User, "첫 요청"),
            ("a1", Kind::Assistant, "살펴보겠습니다."),
            ("t1", Kind::Tool, "output"),
            ("a2", Kind::Assistant, "완료했습니다."),
            ("u2", Kind::User, "다음 요청"),
            ("a3", Kind::Assistant, "다음 응답"),
        ] {
            app.entries.push(Entry {
                id: id.into(),
                kind,
                title: "rg example".into(),
                body: body.into(),
                status: "completed".into(),
                expanded: false,
            });
        }
        let rows = app.cached_chat_rows(36);
        let bars: Vec<_> = rows.iter().filter(|row| row.0.starts_with("── ")).collect();
        assert_eq!(
            bars.len(),
            4,
            "tools and assistant continuations share a group"
        );
        assert_eq!(bars[0].1, Tone::User);
        assert!(bars[1].0.contains("Loom"));
        assert!(bars.iter().all(
            |row| unicode_width::UnicodeWidthStr::width(row.0.as_str()) == 36 && row.2.is_none()
        ));
        app.activate(Action::Setting("english".into()));
        let english = app.cached_chat_rows(36);
        assert!(english.iter().any(|row| row.0.starts_with("── You ")));
        assert!(!english.iter().any(|row| row.0.contains("── 사용자")));
        assert!(
            english
                .iter()
                .any(|row| matches!(&row.2, Some(Action::MessageRow(id, _, _)) if id == "a2"))
        );
    }
    #[test]
    fn pasted_blocks_display_as_atomic_chips_but_send_the_full_original_text() {
        let mut app = App::new(false);
        app.thread = Some("main".into());
        app.editor.insert("앞 ");
        let pasted = "안녕하세요\n두 번째 줄\n🦀";
        app.paste(pasted);
        app.editor.insert(" 뒤");
        let (visible, cursor) = app.editor.display();
        assert_eq!(
            visible,
            format!("앞 [붙여넣은 내용 · {}자] 뒤", pasted.chars().count())
        );
        assert_eq!(cursor, visible.len());
        let canvas = app.render(80, 24);
        assert!((0..canvas.height).any(|y| canvas.plain_line(y).contains("[붙여넣은 내용")));
        assert!(!(0..canvas.height).any(|y| canvas.plain_line(y).contains("두 번째 줄")));
        assert_eq!(
            app.activate(Action::Send),
            Some(Intent::Submit(format!("앞 {pasted} 뒤")))
        );
        assert!(app.editor.pasted.is_empty());
        assert_eq!(app.editor.display().0, "");
    }
    #[test]
    fn pasted_blocks_support_multiple_pastes_and_whole_block_editing() {
        let mut editor = Editor::default();
        editor.paste("가\n나");
        editor.insert("+");
        editor.paste("🦀🐙");
        editor.insert(" 끝");
        assert_eq!(
            editor.display().0,
            "[붙여넣은 내용 · 3자]+[붙여넣은 내용 · 2자] 끝"
        );
        editor.home();
        assert_eq!(
            editor.cursor, 0,
            "Home must ignore newlines hidden inside a paste"
        );
        editor.right();
        assert_eq!(editor.cursor, "가\n나".len());
        editor.delete();
        assert_eq!(
            editor.display().0,
            "[붙여넣은 내용 · 3자][붙여넣은 내용 · 2자] 끝"
        );
        editor.delete();
        assert_eq!(editor.display().0, "[붙여넣은 내용 · 3자] 끝");
        editor.backspace();
        assert_eq!(editor.display().0, " 끝");
        assert_eq!(editor.text, " 끝");
        editor.paste("hello");
        editor.left();
        editor.insert("before ");
        assert_eq!(editor.display().0, "before [붙여넣은 내용 · 5자] 끝");
        editor.end();
        assert_eq!(editor.cursor, editor.text.len());
        assert_eq!(editor.take_text(), "before hello 끝");
        assert!(editor.pasted.is_empty());
    }
    #[test]
    fn vertical_navigation_ignores_hidden_paste_newlines() {
        let mut editor = Editor::default();
        editor.insert("첫 줄\n");
        editor.paste("숨김\n숨김\n숨김");
        editor.insert(" 다음");
        editor.vertical(false);
        assert_eq!(editor.cursor, "첫 줄".len());
        editor.vertical(true);
        assert_eq!(editor.cursor, "첫 줄\n".len());
        editor.right();
        assert_eq!(editor.cursor, "첫 줄\n숨김\n숨김\n숨김".len());
    }
    #[test]
    fn permission_changes_require_runtime_confirmation_and_risky_choices_require_user_confirmation()
    {
        let mut app = App::new(false);
        app.thread = Some("main".into());
        app.read_permission_settings(
            &json!({"sandbox":{"type":"workspaceWrite"},"approvalPolicy":"on-request"}),
        );
        app.activate(Action::Command("/permissions".into()));
        assert_eq!(app.view, View::Permissions);
        assert!(
            app.content_rows_for(&View::Permissions, 80)
                .iter()
                .any(|r| r.0.contains("● 작업 폴더"))
        );

        // Clicking a dangerous mode does not change the runtime or send an RPC.
        assert!(
            app.activate(Action::Permission(
                "sandbox".into(),
                "dangerFullAccess".into()
            ))
            .is_none()
        );
        assert!(app.permission_pending.is_none());
        assert_eq!(app.sandbox_mode.as_deref(), Some("workspaceWrite"));
        app.activate(Action::ConfirmPermission(false));
        assert!(app.permission_confirm.is_none());
        app.activate(Action::Permission(
            "sandbox".into(),
            "dangerFullAccess".into(),
        ));
        assert_eq!(
            app.activate(Action::ConfirmPermission(true)),
            Some(Intent::UpdatePermission(
                "sandbox".into(),
                "dangerFullAccess".into()
            ))
        );
        app.editor.insert("keep draft");
        assert!(app.activate(Action::Send).is_none());
        assert_eq!(app.editor.text, "keep draft");
        assert_eq!(app.sandbox_mode.as_deref(), Some("workspaceWrite"));
        app.permission_failed("rejected");
        assert_eq!(app.sandbox_mode.as_deref(), Some("workspaceWrite"));
        assert!(app.permission_pending.is_none());

        assert_eq!(
            app.activate(Action::Permission("sandbox".into(), "readOnly".into())),
            Some(Intent::UpdatePermission(
                "sandbox".into(),
                "readOnly".into()
            ))
        );
        app.permission_applied();
        assert_eq!(app.sandbox_mode.as_deref(), Some("readOnly"));
        app.notification("thread/settings/updated", &json!({"threadId":"other","threadSettings":{"sandboxPolicy":{"type":"dangerFullAccess"}}}));
        assert_eq!(app.sandbox_mode.as_deref(), Some("readOnly"));
        app.notification("thread/settings/updated", &json!({"threadId":"main","threadSettings":{"sandboxPolicy":{"type":"workspaceWrite"}}}));
        assert_eq!(app.sandbox_mode.as_deref(), Some("workspaceWrite"));
    }
    #[test]
    fn approval_choices_respect_advertised_session_scope_and_readonly_permissions() {
        let mut app = App::new(false);
        app.thread = Some("main".into());
        assert!(app.server_request(json!(1), "item/commandExecution/requestApproval", &json!({"threadId":"main","availableDecisions":["accept","acceptForSession","decline"],"command":"git status","reason":"check"})));
        assert!(
            app.approvals[0]
                .choices
                .iter()
                .any(|choice| choice.label == "이 세션에서 허용")
        );
        assert!(app.approvals[0].summary.contains("git status"));
        let mut readonly = App::new(true);
        readonly.thread = Some("main".into());
        assert!(
            readonly
                .activate(Action::Permission(
                    "sandbox".into(),
                    "dangerFullAccess".into()
                ))
                .is_none()
        );
        assert!(
            readonly
                .activate(Action::Command("/permissions".into()))
                .is_none()
        );
        assert!(readonly.permission_pending.is_none());
    }
    #[test]
    fn approval_popup_exposes_the_real_choices_and_can_be_dismissed_without_approval() {
        let mut app = App::new(false);
        app.thread = Some("main".into());
        assert!(app.server_request(
            json!(10),
            "item/commandExecution/requestApproval",
            &json!({"threadId":"main","command":"git status","availableDecisions":["accept","decline"]})
        ));
        let canvas = app.render(80, 24);
        assert!((0..canvas.height).any(|y| canvas.plain_line(y).contains("승인 필요")));
        assert!(
            canvas
                .hits
                .iter()
                .any(|hit| hit.action == Action::Decide("10".into(), 0))
        );
        assert!(
            !canvas
                .hits
                .iter()
                .any(|hit| hit.action == Action::Decide("10".into(), 2))
        );
        app.activate(Action::DismissApproval);
        assert!(!app.approvals.is_empty());
        assert!(
            !app.render(80, 24)
                .hits
                .iter()
                .any(|hit| hit.action == Action::DismissApproval)
        );
        app.activate(Action::Approvals);
        assert_eq!(app.view, View::Approvals);
        assert_eq!(
            app.activate(Action::Decide("10".into(), 1)),
            Some(Intent::Reply("10".into(), "1".into()))
        );
        app.notification("serverRequest/resolved", &json!({"requestId":10}));
        assert!(app.approvals.is_empty());
    }
    #[test]
    fn unknown_folder_prompt_blocks_typing_until_user_chooses_a_mode() {
        let mut app = App::new(false);
        app.trust_prompt = Some("/tmp/unseen".into());
        let canvas = app.render(80, 24);
        assert!((0..canvas.height).any(|y| canvas.plain_line(y).contains("작업 폴더 신뢰")));
        app.key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            &canvas,
        );
        assert!(app.editor.text.is_empty());
        assert_eq!(
            app.key(
                KeyEvent::new(KeyCode::Char('2'), KeyModifiers::NONE),
                &canvas
            ),
            Some(Intent::TrustWorkspace(false))
        );
        assert!(app.trust_pending);
    }
    #[test]
    fn current_codex_permissions_and_completed_tool_output_are_visually_distinct() {
        let mut app = App::new(false);
        app.workspace_trust = Some(true);
        app.read_config_settings(
            &json!({"approval_policy":"never","sandbox_mode":"danger-full-access"}),
        );
        let canvas = app.render(80, 24);
        assert!(canvas.plain_line(2).contains("전체 접근 · 승인 없음"));
        app.activate(Action::Command("/permissions".into()));
        let rows = app.content_rows_for(&View::Permissions, 80);
        assert!(rows.iter().any(|r| r.0.contains("Codex에서 신뢰됨")));
        app.upsert(&json!({"id":"finished","type":"commandExecution","command":"git status","status":"completed","aggregatedOutput":"done"}));
        let rows = app.content_rows_for(&View::Chat, 80);
        assert!(
            rows.iter()
                .any(|r| r.0.contains("git status") && r.1 == Tone::Muted)
        );
    }
    #[test]
    fn semantic_colors_and_single_primary_control_remain_clear_while_busy() {
        let mut app = App::new(false);
        app.busy = true;
        app.status = "실행 중".into();
        app.upsert(&json!({"id":"user","type":"userMessage","content":[{"text":"hello"}]}));
        app.upsert(&json!({"id":"assistant","type":"agentMessage","text":"world"}));
        app.queued.push_back(QueuedMessage {
            effort: None,
            text: "next".into(),
            model: "test".into(),
            skills: vec![],
        });
        let canvas = app.render(80, 24);
        assert_eq!(canvas.cells[17].text, "⠋");
        assert_eq!(canvas.cells[17].tone, Tone::Accent);
        assert!(!canvas.hits.iter().any(|hit| hit.action == Action::Send));
        assert_eq!(
            canvas
                .hits
                .iter()
                .filter(|hit| hit.action == Action::Interrupt)
                .count(),
            1
        );
        let y = (3..canvas.height)
            .find(|y| canvas.plain_line(*y).contains("› hello"))
            .unwrap();
        assert_eq!(canvas.cells[usize::from(y) * 80 + 2].tone, Tone::User);
        let y = (3..canvas.height)
            .find(|y| canvas.plain_line(*y).contains("● world"))
            .unwrap();
        assert_eq!(canvas.cells[usize::from(y) * 80 + 2].tone, Tone::Accent);
        let y = (3..canvas.height)
            .find(|y| canvas.plain_line(*y).contains("대기열"))
            .unwrap();
        assert_eq!(canvas.cells[usize::from(y) * 80 + 2].tone, Tone::Accent);
        assert_eq!(status_style("failed"), ("!", Tone::Danger));
        assert_eq!(item_style("completed"), ("✓", Tone::Success));
    }
    #[test]
    fn transcript_layout_reuses_rows_until_content_or_width_changes() {
        let mut app = App::new(false);
        app.upsert(&json!({"id":"one","type":"agentMessage","text":"first"}));
        let original = app.cached_chat_rows(72);
        assert!(Rc::ptr_eq(&original, &app.cached_chat_rows(72)));
        app.upsert(&json!({"id":"one","type":"agentMessage","text":"second"}));
        let updated = app.cached_chat_rows(72);
        assert!(!Rc::ptr_eq(&original, &updated));
        assert!(updated.iter().any(|row| row.0.contains("second")));
        assert!(!Rc::ptr_eq(&updated, &app.cached_chat_rows(60)));
    }
    #[test]
    fn typing_recovers_composer_from_focused_controls_and_compact_details() {
        let mut app = App::new(false);
        let canvas = app.render(80, 24);
        app.focus = Some(Action::Approvals);
        app.key(
            KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE),
            &canvas,
        );
        assert_eq!(app.editor.text, "a");
        assert!(app.focus.is_none());
        app.switch_view(View::Help);
        let canvas = app.render(80, 24);
        app.key(
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE),
            &canvas,
        );
        assert_eq!(app.editor.text, "ab");
        assert_eq!(app.view, View::Chat);
    }
    #[test]
    fn approval_choices_precede_lengthy_details() {
        let mut app = App::new(false);
        app.approvals.push(Approval {
            id: "1".into(),
            title: "Test approval".into(),
            summary: "A short summary".into(),
            detail: "A long explanation.\n".repeat(100),
            choices: vec![Choice {
                label: "Allow".into(),
                result: "0".into(),
            }],
            answering: false,
            expanded: false,
        });
        let rows = app.content_rows_for(&View::Approvals, 60);
        assert_eq!(rows[0].0, "1. Test approval");
        assert_eq!(rows[1].0, "  1. Allow");
        assert!(rows.iter().any(|r| r.0.contains("A short summary")));
        assert!(!rows.iter().any(|r| r.0.contains("A long explanation")));
        app.activate(Action::ToggleApproval("1".into()));
        assert!(
            app.content_rows_for(&View::Approvals, 60)
                .iter()
                .any(|r| r.0.contains("A long explanation"))
        );
    }
    #[test]
    #[ignore = "manual render profiling"]
    fn profile_long_conversation_typing() {
        let mut app = App::new(false);
        for index in 0..150 {
            app.upsert(&json!({"id":format!("profile-{index}"),"type":"agentMessage",
                "text":format!("### Answer {index}\n{}", "A reasonably long completed reply with code `hello` and prose.\n".repeat(10))}));
        }
        app.render(100, 34);
        let started = Instant::now();
        for _ in 0..40 {
            app.editor.insert("x");
            std::hint::black_box(app.render(100, 34));
        }
        eprintln!("40 long-history keystroke renders: {:?}", started.elapsed());
        let cold_started = Instant::now();
        for _ in 0..40 {
            app.invalidate_chat_rows();
            std::hint::black_box(app.render(100, 34));
        }
        eprintln!(
            "40 forced full transcript layouts: {:?}",
            cold_started.elapsed()
        );
    }
    #[test]
    fn expanding_tool_output_keeps_the_transcript_bounded_and_pages_forward() {
        let mut a = App::new(false);
        let output = (1..=4000)
            .map(|n| format!("line-{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        a.upsert(&json!({"id":"tool","type":"commandExecution","command":"make test","aggregatedOutput":output}));
        a.activate(Action::Toggle("tool".into()));
        let first = a.content_rows_for(&View::Chat, 70);
        assert!(first.len() <= 29);
        assert!(first.iter().any(|row| row.0.contains("다음 24줄")));
        a.activate(Action::OutputPage("tool".into(), true));
        let second = a.content_rows_for(&View::Chat, 70);
        assert!(second.len() <= 30);
        assert!(second.iter().any(|row| row.0.contains("25 │ line-25")));
        assert!(!second.iter().any(|row| row.0.contains("1 │ line-1")));
        a.activate(Action::Toggle("tool".into()));
        a.activate(Action::Toggle("tool".into()));
        assert_eq!(a.tool_pages.get("tool"), None);
    }
    #[test]
    fn copying_clears_previous_message_selection() {
        let mut a = App::new(false);
        a.activate(Action::SelectEntry("message".into()));
        a.focus = Some(Action::Toggle("tool".into()));
        a.hovered = Some(Action::SelectEntry("message".into()));
        a.clear_text_selection();
        assert!(a.branch_entry.is_none());
        assert!(a.focus.is_none());
        assert!(a.hovered.is_none());
    }
    #[test]
    fn copy_and_branch_buttons_return_focus_to_the_existing_draft() {
        let mut app = App::new(false);
        app.set_thread(&json!({"id":"source","turns":[{"id":"turn","status":"completed","items":[{"id":"message","type":"agentMessage","text":"reply"}]}]}));
        app.editor.insert("draft");

        app.activate(Action::SelectEntry("message".into()));
        app.focus = Some(Action::Copy("message".into()));
        app.hovered = app.focus.clone();
        assert_eq!(
            app.activate(Action::Copy("message".into())),
            Some(Intent::Copy("reply".into()))
        );
        assert!(app.focus.is_none() && app.hovered.is_none() && app.branch_entry.is_none());
        assert!(app.render(80, 24).cursor.is_some());

        app.activate(Action::SelectEntry("message".into()));
        app.focus = Some(Action::Branch("message".into()));
        app.hovered = app.focus.clone();
        assert_eq!(
            app.activate(Action::Branch("message".into())),
            Some(Intent::Fork("turn".into()))
        );
        assert!(app.focus.is_none() && app.hovered.is_none() && app.branch_entry.is_none());
        assert!(app.render(80, 24).cursor.is_some());

        app.busy = true;
        app.focus = Some(Action::Branch("message".into()));
        assert_eq!(app.activate(Action::Branch("message".into())), None);
        assert!(app.focus.is_none());
        let canvas = app.render(80, 24);
        app.key(
            KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE),
            &canvas,
        );
        assert_eq!(app.editor.text, "draft!");
    }
    #[test]
    fn typing_in_diff_composer_preserves_the_open_detail() {
        let mut a = App::new(false);
        a.activate(Action::Diff);
        let canvas = a.render(120, 32);
        a.activate(Action::Input);
        a.key(
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
            &canvas,
        );
        assert_eq!(a.view, View::Diff);
        assert_eq!(a.editor.text, "x");
        let canvas = a.render(120, 32);
        assert!(canvas.cursor.is_some());
        a.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &canvas);
        assert_eq!(a.view, View::Diff);
        a.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &canvas);
        assert_eq!(a.view, View::Chat);
    }
    #[test]
    fn editor_deletes_graphemes_and_preserves_cursor_boundaries() {
        let mut e = Editor::default();
        e.insert("가e\u{301}");
        e.backspace();
        assert_eq!(e.text, "가");
        e.left();
        e.insert("나");
        assert_eq!(e.text, "나가");
    }
    #[test]
    fn message_cache_reuses_unchanged_text_and_updates_for_streaming_and_resize() {
        let mut a = App::new(false);
        a.entries.push(Entry {
            id: "response".into(),
            kind: Kind::Assistant,
            title: "agent".into(),
            body: "first line".into(),
            status: "complete".into(),
            expanded: false,
        });
        a.render(80, 24);
        let key = ("response".into(), 64);
        assert_eq!(a.message_cache.borrow()[&key].body, "first line");
        a.render(80, 24);
        assert_eq!(a.message_cache.borrow().len(), 1);
        a.entries[0].body.push_str("\nsecond line");
        a.render(80, 24);
        assert_eq!(a.message_cache.borrow()[&key].rows.len(), 2);
        a.render(120, 24);
        assert!(
            a.message_cache
                .borrow()
                .contains_key(&("response".into(), 63))
        );
    }
    #[test]
    fn streamed_events_are_scoped_and_completion_is_authoritative() {
        let mut a = App::new(false);
        a.thread = Some("main".into());
        a.notification(
            "item/agentMessage/delta",
            &json!({"threadId":"other","itemId":"x","delta":"wrong"}),
        );
        assert!(a.entries.is_empty());
        a.notification(
            "item/agentMessage/delta",
            &json!({"threadId":"main","itemId":"x","delta":"partial"}),
        );
        a.notification(
            "item/completed",
            &json!({"threadId":"main","item":{"id":"x","type":"agentMessage","text":"final"}}),
        );
        assert_eq!(a.entries[0].body, "final");
    }
    #[test]
    fn approvals_do_not_capture_typed_enter_and_resolve_once() {
        let mut a = App::new(false);
        a.thread = Some("main".into());
        a.editor.insert("draft");
        assert!(a.server_request(
            json!(4),
            "item/commandExecution/requestApproval",
            &json!({"threadId":"main","command":"ls","availableDecisions":["decline"]})
        ));
        assert!(a.focus.is_none());
        assert_eq!(a.editor.text, "draft");
        assert_eq!(a.approvals[0].choices.len(), 1);
        let decision = a.activate(Action::Decide("4".into(), 0));
        assert_eq!(decision, Some(Intent::Reply("4".into(), "0".into())));
        assert!(a.activate(Action::Decide("4".into(), 0)).is_none());
    }
    #[test]
    fn panels_preserve_draft_and_readonly_windows_cannot_send() {
        let mut a = App::new(true);
        a.editor.insert("draft");
        a.activate(Action::Agents);
        a.activate(Action::Back);
        assert_eq!(a.editor.text, "draft");
        assert!(a.activate(Action::Send).is_none());
    }
    #[test]
    fn runtime_confirmation_owns_interrupt_and_approval_completion() {
        let mut a = App::new(false);
        a.thread = Some("main".into());
        a.turn = Some("turn".into());
        a.busy = true;
        a.activate(Action::Interrupt);
        assert!(a.busy);
        a.notification(
            "turn/completed",
            &json!({"threadId":"main","turn":{"status":"interrupted"}}),
        );
        assert!(!a.busy);
        a.server_request(
            json!(1),
            "item/fileChange/requestApproval",
            &json!({"threadId":"main"}),
        );
        a.notification(
            "serverRequest/resolved",
            &json!({"threadId":"main","requestId":1}),
        );
        assert!(a.approvals.is_empty());
    }
    #[test]
    fn returning_to_chat_restores_the_reading_position() {
        let mut a = App::new(false);
        a.scroll = 12;
        a.follow = false;
        a.editor.insert("초안");
        a.activate(Action::Diff);
        a.scroll = 7;
        a.activate(Action::Agents);
        a.activate(Action::Back);
        assert_eq!((a.scroll, a.follow), (12, false));
        assert_eq!(a.editor.text, "초안");
        a.activate(Action::Diff);
        assert_eq!(a.scroll, 7);
    }
    #[test]
    fn streaming_scroll_resumes_follow_at_bottom() {
        let mut a = App::new(false);
        a.thread = Some("main".into());
        let lines = (0..55)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        a.upsert(&json!({"id":"history","type":"agentMessage","text":lines}));
        a.render(80, 24);
        assert!(a.follow && a.scroll_limit > 3 && a.scroll == a.scroll_limit);
        a.scroll_at(2, 80, false);
        assert!(!a.follow);
        let parked = a.scroll;
        a.notification(
            "item/agentMessage/delta",
            &json!({"threadId":"main","itemId":"stream","delta":"new output\nnew line"}),
        );
        a.render(80, 24);
        assert_eq!(a.scroll, parked);
        while !a.follow {
            a.scroll_at(2, 80, true);
        }
        a.notification(
            "item/agentMessage/delta",
            &json!({"threadId":"main","itemId":"stream","delta":"\nlatest output"}),
        );
        a.render(80, 24);
        assert!(a.follow && a.scroll == a.scroll_limit);
    }
    #[test]
    fn overview_wheel_does_not_move_the_chat() {
        let mut a = App::new(false);
        a.agents = (0..30)
            .map(|i| Agent {
                id: format!("agent-{i}"),
                name: format!("Agent {i}"),
                status: "working".into(),
                detail: String::new(),
            })
            .collect();
        a.render(120, 24);
        assert!(a.overview_scroll_limit > 3);
        let chat_scroll = a.scroll;
        a.scroll_at(115, 120, false);
        assert!(!a.overview_follow);
        assert_eq!(a.scroll, chat_scroll);
        while !a.overview_follow {
            a.scroll_at(115, 120, true);
        }
        assert_eq!(a.overview_scroll, a.overview_scroll_limit);
    }
    #[test]
    fn resolved_request_cannot_approve_the_next_request() {
        let mut a = App::new(false);
        a.thread = Some("main".into());
        for id in [1, 2] {
            a.server_request(
                json!(id),
                "item/fileChange/requestApproval",
                &json!({"threadId":"main"}),
            );
        }
        a.notification(
            "serverRequest/resolved",
            &json!({"threadId":"main","requestId":1}),
        );
        assert!(a.activate(Action::Decide("1".into(), 0)).is_none());
        assert!(!a.approvals[0].answering);
    }
    #[test]
    fn layouts_fit_small_and_large_terminals() {
        let mut a = App::new(false);
        a.editor.insert("가나다 e\u{301}\n다음 줄");
        for (w, h) in [(80, 24), (120, 40), (20, 8)] {
            let canvas = a.render(w, h);
            assert_eq!(canvas.cells.len(), usize::from(w) * usize::from(h));
            assert!(
                canvas
                    .hits
                    .iter()
                    .all(|hit| hit.x + hit.width <= w && hit.y < h)
            );
            if let Some((x, y)) = canvas.cursor {
                assert!(x < w && y < h);
            }
        }
    }
    #[test]
    fn partitions_have_fixed_boundaries_and_active_tab_survives_hover() {
        let mut a = App::new(false);
        a.diff = "+검토할 변경".into();
        a.activate(Action::Back);
        a.hovered = Some(Action::Agents);
        for (w, h) in [(80, 24), (120, 32)] {
            let c = a.render(w, h);
            assert!(c.plain_line(2).starts_with("──"));
            let input_y = c
                .hits
                .iter()
                .find(|hit| hit.action == Action::Input)
                .unwrap()
                .y;
            assert!(c.plain_line(input_y - 1).starts_with("──"));
            for (action, tone) in [
                (Action::Back, Tone::Selected),
                (Action::Agents, Tone::Hover),
            ] {
                let hit = c
                    .hits
                    .iter()
                    .find(|hit| hit.y == 1 && hit.action == action)
                    .unwrap();
                assert_eq!(
                    c.cells[usize::from(hit.y) * usize::from(w) + usize::from(hit.x)].tone,
                    tone
                );
            }
            assert!(
                !c.hits
                    .iter()
                    .any(|hit| hit.y == 1 && hit.action == Action::Diff)
            );
        }
        a.activate(Action::Diff);
        let c = a.render(120, 32);
        assert!(!a.wide_layout, "diff review uses the full terminal width");
        assert!(
            !c.hits
                .iter()
                .any(|hit| hit.y == 1 && hit.action == Action::Diff)
        );
    }
    #[test]
    fn numbered_commands_reach_mouse_actions_without_discarding_drafts() {
        let mut a = App::new(false);
        a.set_thread(&json!({"id":"main","turns":[
            {"id":"one","status":"completed","items":[{"id":"first","type":"agentMessage","text":"first response"}]},
            {"id":"two","status":"completed","items":[{"id":"second","type":"agentMessage","text":"second response"}]}
        ]}));
        a.entries.push(Entry {
            id: "old-change".into(),
            kind: Kind::Change,
            title: "earlier".into(),
            body: "--- a/old\n+++ b/old\n+previous".into(),
            status: "completed".into(),
            expanded: false,
        });
        a.entries.push(Entry {
            id: "new-change".into(),
            kind: Kind::Change,
            title: "newer".into(),
            body: "--- a/new\n+++ b/new\n+latest".into(),
            status: "completed".into(),
            expanded: false,
        });
        a.entries.push(Entry {
            id: "tool".into(),
            kind: Kind::Tool,
            title: "check".into(),
            body: "stdout".into(),
            status: "completed".into(),
            expanded: false,
        });
        a.editor.insert("/diff 2");
        assert_eq!(a.activate(Action::Send), None);
        assert_eq!(a.view, View::Diff);
        assert_eq!(a.selected_diff_entry.as_deref(), Some("old-change"));
        assert!(
            a.editor.text.is_empty(),
            "slash commands clear the composer"
        );
        a.command("/chat");
        assert_eq!(a.view, View::Chat);
        a.command("/tool 1");
        assert!(a.expanded.contains("tool"));
        assert_eq!(
            a.command("/copy 2"),
            Some(Intent::Copy("first response".into()))
        );
        assert_eq!(a.command("/branch 1"), Some(Intent::Fork("two".into())));
        assert_eq!(a.command("/copy 3"), None);
        assert!(a.notice_error);
    }
    #[test]
    fn keyboard_only_queue_approval_and_permission_commands_keep_safety_checks() {
        let mut a = App::new(false);
        a.thread = Some("main".into());
        for text in ["first", "second", "third"] {
            a.queued.push_back(QueuedMessage {
                effort: None,
                text: text.into(),
                skills: vec![],
                model: "test".into(),
            });
        }
        a.command("/queue drop 2");
        assert_eq!(
            a.queued.iter().map(|q| q.text.as_str()).collect::<Vec<_>>(),
            ["first", "third"]
        );
        a.command("/queue force 2");
        assert_eq!(a.queued.front().unwrap().text, "third");
        a.command("/queue clear");
        assert!(a.queued.is_empty());

        assert!(a.server_request(
            json!(41),
            "item/fileChange/requestApproval",
            &json!({"threadId":"main"})
        ));
        assert_eq!(
            a.command("/approve 1 1"),
            Some(Intent::Reply("41".into(), "0".into()))
        );
        assert_eq!(
            a.command("/approve 1 1"),
            None,
            "an approval cannot be submitted twice"
        );

        a.read_permission_settings(
            &json!({"sandbox":{"type":"workspaceWrite"},"approvalPolicy":"on-request"}),
        );
        assert_eq!(a.command("/permissions sandbox dangerFullAccess"), None);
        assert!(a.permission_confirm.is_some());
        assert!(a.permission_pending.is_none());
        a.command("/permissions cancel");
        assert!(a.permission_confirm.is_none());
        a.command("/permissions sandbox dangerFullAccess");
        assert_eq!(
            a.command("/permissions confirm"),
            Some(Intent::UpdatePermission(
                "sandbox".into(),
                "dangerFullAccess".into()
            ))
        );
    }
    #[test]
    fn wide_detail_keeps_chat_visible_and_typing_does_not_close_it() {
        let mut a = App::new(false);
        a.upsert(&json!({"id":"answer","type":"agentMessage","text":format!("{}대화 기록 유지", "반복\n".repeat(45))}));
        a.diff = "+상세 변경".into();
        a.render(120, 32);
        a.activate(Action::Agents);
        let c = a.render(120, 32);
        let screen = (0..c.height)
            .map(|y| c.plain_line(y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("대화 기록 유지") && screen.contains("연결된 runtime 활동"));
        a.activate(Action::Input);
        a.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE), &c);
        assert_eq!(a.view, View::Agents);
        assert_eq!(a.editor.text, "a");
        assert!(a.chat_scroll_limit > 0);
        let detail_scroll = a.scroll;
        a.scroll_at(2, 120, false);
        assert_eq!(a.scroll, detail_scroll);
        assert!(!a.positions[&View::Chat].1);
    }
    #[test]
    fn slash_menu_is_local_and_never_submits_unknown_commands() {
        let mut a = App::new(false);
        a.editor.insert("/");
        let c = a.render(80, 24);
        a.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &c);
        a.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &c);
        assert_eq!(
            a.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &c),
            Some(Intent::RefreshSkills)
        );
        assert_eq!(a.view, View::Skills);
        assert!(a.editor.text.is_empty() && a.submitted.is_none());
        a.activate(Action::Back);
        a.editor.insert("/unknown");
        assert!(a.activate(Action::Send).is_none());
        assert_eq!(a.editor.text, "/unknown");
        assert!(a.submitted.is_none());
        a.busy = true;
        assert!(a.activate(Action::Command("/new".into())).is_none());
    }
    #[test]
    fn skills_are_discovered_selected_and_attached_to_the_real_input_shape() {
        let mut a = App::new(false);
        a.thread = Some("main".into());
        a.load_skills(&json!({"data":[{"skills":[
            {"name":"review","path":"/tmp/review/SKILL.md","description":"검토","enabled":true},
            {"name":"disabled","path":"/tmp/disabled/SKILL.md","enabled":false},
            {"name":"review","path":"/tmp/review/SKILL.md","enabled":true}
        ]}]}));
        assert_eq!(a.skills.len(), 1);
        a.editor.insert("$rev");
        let c = a.render(80, 24);
        a.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &c);
        assert!(a.editor.text.is_empty());
        a.editor.insert("검토해줘");
        assert_eq!(
            a.activate(Action::Send),
            Some(Intent::Submit("검토해줘".into()))
        );
        assert_eq!(
            a.turn_input("검토해줘"),
            json!([
                {"type":"text","text":"검토해줘"},
                {"type":"skill","name":"review","path":"/tmp/review/SKILL.md"}
            ])
        );
        a.restore_skills();
        assert_eq!(a.selected_skills.len(), 1);
        a.readonly = true;
        a.activate(Action::RemoveSkill("/tmp/review/SKILL.md".into()));
        a.activate(Action::Skill("/tmp/review/SKILL.md".into()));
        assert!(a.selected_skills.is_empty());
    }
    #[test]
    fn composer_is_docked_and_success_notices_expire_or_clear_on_submit() {
        let mut a = App::new(false);
        a.thread = Some("main".into());
        let c = a.render(80, 40);
        assert_eq!(
            c.cursor.unwrap().1,
            37,
            "composer must stay docked to the bottom"
        );
        a.models = vec!["model".into()];
        a.activate(Action::Model("model".into()));
        assert!(!a.notice.is_empty());
        assert!(a.tick(Instant::now() + Duration::from_secs(5)));
        assert!(a.notice.is_empty());
        a.activate(Action::Model("model".into()));
        a.editor.insert("hello");
        a.activate(Action::Send);
        assert!(a.notice.is_empty());
        a.error("failed");
        assert!(!a.tick(Instant::now() + Duration::from_secs(5)));
        assert_eq!(a.notice, "failed");
        assert!(a.tick(Instant::now() + Duration::from_secs(8)));
        assert!(a.notice.is_empty());
        assert!(!a.notice_error);
    }
    #[test]
    fn queued_messages_keep_order_and_capture_model_and_skills() {
        let mut a = App::new(false);
        a.thread = Some("main".into());
        a.turn = Some("running".into());
        a.busy = true;
        a.model = "model-a".into();
        a.selected_skills.push(Skill {
            name: "review".into(),
            path: "/review/SKILL.md".into(),
            description: String::new(),
        });
        a.editor.insert("first");
        assert_eq!(a.activate(Action::Send), None);
        a.model = "model-b".into();
        a.editor.insert("second");
        assert_eq!(a.activate(Action::Send), None);
        a.editor.insert("next draft");
        assert_eq!(a.queued.len(), 2);
        assert_eq!(a.queued[0].model, "model-a");
        assert_eq!(a.queued[1].model, "model-b");
        assert!(
            !a.content_rows_for(&View::Chat, 80)
                .iter()
                .any(|row| row.0.contains("전송 대기열"))
        );
        let canvas = a.render(80, 24);
        let input_y = canvas
            .hits
            .iter()
            .find(|h| h.action == Action::Input)
            .unwrap()
            .y;
        let queue_y = canvas
            .hits
            .iter()
            .find(|h| h.action == Action::RemoveQueued(0))
            .unwrap()
            .y;
        assert!(queue_y < input_y - 1);
        assert!(canvas.plain_line(queue_y).contains("first"));
        a.notification(
            "turn/completed",
            &json!({"threadId":"main","turn":{"id":"running","status":"completed"}}),
        );
        assert_eq!(a.next_queued(), Some(Intent::Submit("first".into())));
        assert_eq!(a.submitted_model.as_deref(), Some("model-a"));
        assert_eq!(a.submitted_skills[0].name, "review");
        assert!(a.next_queued().is_none());
        a.submitted = None; // Simulate a successful turn/start acknowledgement.
        a.submitted_skills.clear();
        a.submitted_model = None;
        a.turn = Some("first-turn".into());
        a.busy = true;
        a.notification(
            "turn/completed",
            &json!({"threadId":"main","turn":{"id":"first-turn","status":"completed"}}),
        );
        assert_eq!(a.next_queued(), Some(Intent::Submit("second".into())));
        assert_eq!(a.submitted_model.as_deref(), Some("model-b"));
        assert_eq!(a.editor.text, "next draft");
        assert!(a.queued.is_empty());
    }
    #[test]
    fn queued_strip_stays_above_input_and_supports_cancel_and_resume() {
        let mut a = App::new(false);
        a.thread = Some("main".into());
        a.busy = true;
        for i in 0..5 {
            a.editor.insert(&format!("queued {i}"));
            a.activate(Action::Send);
        }
        a.queue_paused = true;
        a.upsert(&json!({"id":"long","type":"agentMessage","text":"a line\n".repeat(70)}));
        for (w, h) in [(80, 24), (120, 40), (40, 12)] {
            let before = a.render(w, h);
            let input = before
                .hits
                .iter()
                .find(|h| h.action == Action::Input)
                .unwrap()
                .y;
            let header = before
                .hits
                .iter()
                .find(|h| h.action == Action::ResumeQueue)
                .unwrap();
            assert!(header.y < input);
            assert!(before.plain_line(header.y).contains("5개"));
            a.scroll_at(1, w, false);
            let after = a.render(w, h);
            assert_eq!(before.plain_line(header.y), after.plain_line(header.y));
            assert_eq!(
                after
                    .hits
                    .iter()
                    .find(|h| h.action == Action::Input)
                    .unwrap()
                    .y,
                input
            );
        }
        let canvas = a.render(80, 24);
        let remove = canvas
            .hits
            .iter()
            .find(|h| h.action == Action::RemoveQueued(0))
            .unwrap();
        a.activate(remove.action.clone());
        assert_eq!(a.queued.len(), 4);
        a.activate(Action::ResumeQueue);
        assert!(!a.queue_paused);
    }
    #[test]
    fn force_queued_waits_for_interrupt_completion_and_preserves_queue_order() {
        let mut a = App::new(false);
        a.thread = Some("main".into());
        a.turn = Some("running".into());
        a.busy = true;
        for msg in ["first", "second", "third"] {
            a.editor.insert(msg);
            a.activate(Action::Send);
        }
        assert_eq!(a.activate(Action::ForceQueued(2)), Some(Intent::Interrupt));
        assert_eq!(a.queued[0].text, "third");
        assert_eq!(a.queued[1].text, "first");
        assert_eq!(a.queued[2].text, "second");
        assert!(a.next_queued().is_none());
        // Changing the chosen message must not send a duplicate interrupt.
        assert_eq!(a.activate(Action::ForceQueued(2)), None);
        assert_eq!(a.queued[0].text, "second");
        a.track(20, "turn/interrupt");
        a.notification(
            "turn/completed",
            &json!({"threadId":"main","turn":{"id":"running","status":"interrupted"}}),
        );
        assert!(!a.queue_paused);
        assert!(a.next_queued().is_none(), "interrupt RPC still pending");
        a.pending.remove(&20);
        assert_eq!(a.next_queued(), Some(Intent::Submit("second".into())));
        assert_eq!(
            a.queued.iter().map(|m| m.text.as_str()).collect::<Vec<_>>(),
            ["third", "first"]
        );
    }
    #[test]
    fn failed_or_cancelled_force_preserves_queue_and_manual_interrupt_still_pauses() {
        let mut a = App::new(false);
        a.thread = Some("main".into());
        a.turn = Some("running".into());
        a.busy = true;
        a.editor.insert("keep me");
        a.activate(Action::Send);
        assert_eq!(a.activate(Action::ForceQueued(0)), Some(Intent::Interrupt));
        a.interrupt_failed("rejected");
        assert!(a.queue_paused);
        assert!(a.notice.contains("rejected"));
        assert_eq!(a.queued.front().unwrap().text, "keep me");
        assert!(a.next_queued().is_none());
        a.activate(Action::ResumeQueue);
        a.notification(
            "turn/completed",
            &json!({"threadId":"main","turn":{"id":"running","status":"interrupted"}}),
        );
        assert!(
            a.queue_paused,
            "manual interruption must not auto-run queue"
        );
        a.turn = Some("another".into());
        a.busy = true;
        assert_eq!(a.activate(Action::ForceQueued(0)), Some(Intent::Interrupt));
        a.activate(Action::RemoveQueued(0));
        assert!(a.queued.is_empty());
        assert!(a.force_after_interrupt.is_none());
    }
    #[test]
    fn queued_force_command_can_pick_an_item_hidden_below_the_fixed_strip() {
        let mut a = App::new(false);
        a.thread = Some("main".into());
        a.turn = Some("running".into());
        a.busy = true;
        for n in 1..=5 {
            a.editor.insert(&format!("item {n}"));
            a.activate(Action::Send);
        }
        assert_eq!(a.command("/queue force 5"), Some(Intent::Interrupt));
        assert_eq!(a.queued.front().unwrap().text, "item 5");
        assert!(a.command("/queue force 99").is_none());
        assert_eq!(a.queued.len(), 5);
    }
    #[test]
    fn paste_activates_composer_even_after_clicking_another_tab() {
        let mut a = App::new(false);
        a.activate(Action::Diff);
        let canvas = a.render(80, 24);
        a.focus = Some(Action::Diff);
        a.paste("줄 하나\n줄 둘");
        assert_eq!(a.view, View::Chat);
        assert_eq!(a.focus, None);
        assert_eq!(a.editor.text, "줄 하나\n줄 둘");
        assert!(a.render(80, 24).cursor.is_some());
        assert_eq!(
            a.key(
                KeyEvent::new(KeyCode::Char('v'), KeyModifiers::CONTROL),
                &canvas
            ),
            Some(Intent::PasteClipboard)
        );
        a.activate(Action::SelectEntry("copied".into()));
        a.entries.push(Entry {
            id: "copied".into(),
            kind: Kind::Assistant,
            title: String::new(),
            body: "copy me".into(),
            status: String::new(),
            expanded: false,
        });
        assert_eq!(
            a.key(
                KeyEvent::new(
                    KeyCode::Char('c'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT
                ),
                &canvas
            ),
            Some(Intent::Copy("copy me".into()))
        );
        a.readonly = true;
        a.paste("denied");
        assert_eq!(a.editor.text, "줄 하나\n줄 둘");
    }
    #[test]
    fn failed_submission_is_retained_until_queue_is_resumed() {
        let mut a = App::new(false);
        a.thread = Some("main".into());
        a.editor.insert("retry me");
        assert_eq!(
            a.activate(Action::Send),
            Some(Intent::Submit("retry me".into()))
        );
        a.fail_submission("network error");
        assert_eq!(a.queued.len(), 1);
        assert!(a.queue_paused && a.next_queued().is_none());
        assert!(a.notice.contains("network error"));
        a.activate(Action::ResumeQueue);
        assert_eq!(a.next_queued(), Some(Intent::Submit("retry me".into())));
    }
    #[test]
    fn pending_approval_holds_queued_messages_until_resolved() {
        let mut a = App::new(false);
        a.thread = Some("main".into());
        a.turn = Some("running".into());
        a.busy = true;
        a.editor.insert("later");
        a.activate(Action::Send);
        assert!(a.server_request(
            json!(7),
            "item/fileChange/requestApproval",
            &json!({"threadId":"main"})
        ));
        a.notification(
            "turn/completed",
            &json!({"threadId":"main","turn":{"status":"completed"}}),
        );
        assert!(a.next_queued().is_none());
        a.notification("serverRequest/resolved", &json!({"requestId":7}));
        assert_eq!(a.next_queued(), Some(Intent::Submit("later".into())));
    }
    #[test]
    fn branching_selects_a_completed_turn_without_rewriting_the_source() {
        let mut a = App::new(false);
        a.set_thread(&json!({"id":"original","turns":[
            {"id":"turn-one","status":"completed","items":[{"id":"message-one","type":"agentMessage","text":"one"}]},
            {"id":"turn-two","status":"completed","items":[{"id":"message-two","type":"agentMessage","text":"two"}]}
        ]}));
        a.activate(Action::SelectEntry("message-one".into()));
        assert_eq!(
            a.activate(Action::Branch("message-one".into())),
            Some(Intent::Fork("turn-one".into()))
        );
        assert_eq!(a.thread.as_deref(), Some("original"));
        assert_eq!(a.entries.len(), 2);
        a.busy = true;
        assert!(a.activate(Action::Branch("message-one".into())).is_none());
    }
    #[test]
    fn settings_switch_labels_and_keep_the_active_tab_visible() {
        let mut a = App::new(false);
        a.activate(Action::Command("/settings".into()));
        assert_eq!(
            a.activate(Action::Setting("english".into())),
            Some(Intent::SaveSettings)
        );
        let c = a.render(80, 24);
        assert!(c.plain_line(1).contains("● Settings"));
        let screen = (0..c.height)
            .map(|y| c.plain_line(y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("Language") && screen.contains("Task panel on startup"));
    }
    #[test]
    fn settings_panel_command_applies_preference_and_actual_layout() {
        let mut app = App::new(false);
        app.editor.insert("unsent draft");
        assert!(app.panel_open && app.preferences.panel_open);
        assert_eq!(app.command("/settings panel"), Some(Intent::SaveSettings));
        assert!(!app.panel_open && !app.preferences.panel_open);
        assert_eq!(app.view, View::Chat);
        app.render(120, 36);
        assert!(!app.wide_layout);
        assert_eq!(app.editor.text, "unsent draft");
        assert_eq!(app.command("/settings panel"), Some(Intent::SaveSettings));
        assert!(app.panel_open && app.preferences.panel_open);
        assert_eq!(app.view, View::Settings);
        app.render(120, 36);
        assert!(app.wide_layout);
        assert_eq!(app.editor.text, "unsent draft");
    }
    #[test]
    fn branch_icons_use_a_separate_gutter_without_adding_transcript_rows() {
        let mut a = App::new(false);
        a.set_thread(&json!({"id":"source","turns":[{"id":"turn","status":"completed","items":[{"id":"message","type":"agentMessage","text":"분기 대상"}]}]}));
        let before = a.render(80, 24);
        let hit = before
            .hits
            .iter()
            .find(|hit| hit.action == Action::Branch("message".into()))
            .unwrap();
        let icon_y = hit.y;
        assert!(
            before.plain_line(icon_y).contains("분기 대상")
                && before.plain_line(icon_y).contains("⑂")
        );
        a.activate(Action::SelectEntry("message".into()));
        let after = a.render(80, 24);
        assert_eq!(before.cursor, after.cursor);
        assert_eq!(
            after
                .hits
                .iter()
                .find(|hit| hit.action == Action::Branch("message".into()))
                .unwrap()
                .y,
            icon_y
        );
        assert!(!(0..after.height).any(|y| after.plain_line(y).contains("이 대화까지 새 분기")));
    }
    #[test]
    fn copy_stays_available_for_streaming_and_readonly_messages() {
        let mut a = App::new(false);
        a.thread = Some("main".into());
        a.notification(
            "item/agentMessage/delta",
            &json!({"threadId":"main","itemId":"stream","delta":"partial reply"}),
        );
        let live = a.render(80, 24);
        assert!(
            live.hits
                .iter()
                .any(|h| h.action == Action::Copy("stream".into()))
        );
        assert!(
            !live
                .hits
                .iter()
                .any(|h| h.action == Action::Branch("stream".into()))
        );
        a.readonly = true;
        let readonly = a.render(80, 24);
        assert!(
            readonly
                .hits
                .iter()
                .any(|h| h.action == Action::Copy("stream".into()))
        );
        assert!(
            readonly
                .hits
                .iter()
                .all(|h| !matches!(h.action, Action::Branch(_)))
        );
    }
    #[test]
    fn session_menu_keeps_current_conversation_distinct_and_does_not_show_raw_ids() {
        let mut app = App::new(false);
        app.set_thread(&json!({"id":"current-uuid","preview":"What changed?\nSecret second line"}));
        app.sessions = vec![
            SessionSummary::from_thread(&json!({"id":"current-uuid","preview":"What changed?"})).unwrap(),
            SessionSummary::from_thread(&json!({"id":"previous-uuid","preview":"Earlier conversation","status":{"type":"active"},"updatedAt":1})).unwrap(),
        ];
        app.sessions_loaded = true;
        app.sessions_next_cursor = Some("cursor".into());
        assert_eq!(
            app.activate(Action::Sessions),
            Some(Intent::RefreshSessions)
        );
        let rows = app.content_rows_for(&View::Sessions, 60);
        assert_eq!(
            rows.iter()
                .filter(|row| row.0.contains("What changed?"))
                .count(),
            1
        );
        assert!(rows.iter().any(|row| row.0.contains("Earlier conversation")
            && row.2 == Some(Action::Resume("previous-uuid".into()))));
        assert!(
            !rows
                .iter()
                .any(|row| row.0.contains("previous-uuid") || row.0.contains("Secret second line"))
        );
        assert_eq!(
            app.activate(Action::MoreSessions),
            Some(Intent::LoadMoreSessions("cursor".into()))
        );
        let canvas = app.render(80, 24);
        assert!(canvas.plain_line(1).contains("세션"));
        assert!(!canvas.plain_line(1).contains("기록"));

        app.busy = true;
        assert_eq!(
            app.activate(Action::Sessions),
            Some(Intent::RefreshSessions)
        );
        assert!(
            app.content_rows_for(&View::Sessions, 60)
                .iter()
                .any(|row| row.2 == Some(Action::Resume("previous-uuid".into())))
        );
        assert!(
            app.activate(Action::Resume("previous-uuid".into()))
                .is_none()
        );
        assert!(app.notice.contains("현재 응답"));
        assert!(app.notice_error);
        assert!(app.activate(Action::NewSession).is_none());
        assert_eq!(app.thread.as_deref(), Some("current-uuid"));
    }
    #[test]
    fn numbered_session_commands_follow_visible_order_and_preserve_list_while_refreshing() {
        let mut app = App::new(false);
        app.set_thread(&json!({"id":"current","preview":"Current"}));
        app.sessions = vec![
            SessionSummary::from_thread(&json!({"id":"current","preview":"Current"})).unwrap(),
            SessionSummary::from_thread(&json!({"id":"older","preview":"Older"})).unwrap(),
            SessionSummary::from_thread(&json!({"id":"oldest","preview":"Oldest"})).unwrap(),
        ];
        app.sessions_loaded = true;
        let rows = app.content_rows_for(&View::Sessions, 50);
        assert!(
            rows.iter().any(|row| row.0.starts_with("1. ○ Older")
                && row.2 == Some(Action::Resume("older".into())))
        );
        assert!(rows.iter().any(|row| row.0.starts_with("2. ○ Oldest")
            && row.2 == Some(Action::Resume("oldest".into()))));
        assert_eq!(
            app.command("/sessions 2"),
            Some(Intent::Session(Some("oldest".into())))
        );
        assert!(app.command("/sessions 3").is_none());
        assert!(app.notice.contains("번호"));

        app.sessions_loaded = false; // a refresh is in flight
        let rows = app.content_rows_for(&View::Sessions, 50);
        assert!(rows.iter().any(|row| row.0.contains("불러오는 중")));
        assert!(rows.iter().any(|row| row.0.starts_with("1. ○ Older")));
        app.sessions_next_cursor = Some("page2".into());
        assert!(app.activate(Action::MoreSessions).is_none());
        assert!(app.notice.contains("새로고침"));
        assert_eq!(
            app.command("/sessions 1"),
            Some(Intent::Session(Some("older".into())))
        );
        app.sessions_loaded = true;
        app.pending.insert(9, Operation::MoreSessions);
        assert!(app.activate(Action::MoreSessions).is_none());
        app.pending.clear();
        assert_eq!(
            app.activate(Action::MoreSessions),
            Some(Intent::LoadMoreSessions("page2".into()))
        );
        app.sessions_loaded = false;
        app.sessions.clear();
        assert!(app.command("/sessions 1").is_none());
        assert!(app.notice.contains("불러온 후"));
    }
    #[test]
    fn contextual_diff_only_shows_latest_turn_while_old_events_stay_accessible() {
        let mut app = App::new(false);
        app.thread = Some("main".into());
        app.notification(
            "turn/started",
            &json!({"threadId":"main","turn":{"id":"first"}}),
        );
        app.upsert(&json!({"id":"old-change","type":"fileChange","changes":[
            {"path":"old.txt","diff":"+historical change"}]}));
        app.notification(
            "turn/completed",
            &json!({"threadId":"main","turn":{"id":"first","status":"completed"}}),
        );
        assert!(app.has_reported_changes());
        assert_eq!(app.reported_change_label(), "1건");

        app.notification(
            "turn/started",
            &json!({"threadId":"main","turn":{"id":"second"}}),
        );
        assert!(!app.has_reported_changes());
        let canvas = app.render(80, 24);
        assert!(!canvas.hits.iter().any(|hit| hit.action == Action::Diff));
        assert!(app.entries.iter().any(|entry| entry.id == "old-change"));
        app.activate(Action::DiffEntry("old-change".into()));
        assert_eq!(app.view, View::Diff);
        assert!(
            app.content_rows_for(&View::Diff, 65)
                .iter()
                .any(|row| row.0.contains("historical change"))
        );

        app.activate(Action::Back);
        app.upsert(&json!({"id":"new-change","type":"fileChange","changes":[
            {"path":"new.txt","diff":"+latest change"}]}));
        assert!(app.has_reported_changes());
        app.activate(Action::Diff);
        let lines = app
            .content_rows_for(&View::Diff, 65)
            .into_iter()
            .map(|row| row.0)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(lines.contains("latest change"));
        assert!(!lines.contains("historical change"));
    }
    #[test]
    fn thread_reply_without_status_clears_temporary_busy_but_preserves_running_turns() {
        let mut app = App::new(false);
        app.busy = true; // set by request_thread before the server replies
        app.set_thread(&json!({"id":"saved","preview":"Older Codex thread"}));
        assert!(
            !app.busy,
            "a successful idle resume must unlock session switching"
        );
        assert_eq!(app.status, "대기");

        app.busy = true;
        app.set_thread(
            &json!({"id":"running","turns":[{"id":"turn", "status":"inProgress","items":[]}]}),
        );
        assert!(app.busy);
        assert_eq!(app.turn.as_deref(), Some("turn"));
    }
    #[test]
    fn session_row_and_activity_line_are_clickable_on_narrow_and_wide_terminals() {
        for (width, height) in [(80, 28), (120, 36)] {
            let mut app = App::new(false);
            app.set_thread(&json!({"id":"current","preview":"Current"}));
            app.sessions_loaded = true;
            app.sessions.push(SessionSummary {
                id: "previous".into(),
                title: "Earlier conversation".into(),
                status: "idle".into(),
                updated_at: Some(1),
            });
            app.activate(Action::Sessions);
            let canvas = app.render(width, height);
            let (row_x, row_y) = (0..height)
                .find_map(|y| {
                    canvas
                        .plain_line(y)
                        .find("Earlier conversation")
                        .map(|x| (x as u16, y))
                })
                .expect("session must be visible");
            let action = Action::Resume("previous".into());
            assert_eq!(canvas.hit(row_x, row_y), Some(action.clone()));
            assert_eq!(
                canvas.hit(row_x.saturating_add(1), row_y + 1),
                Some(action.clone()),
                "the metadata row must switch the same session"
            );
            assert_eq!(
                app.activate(action),
                Some(Intent::Session(Some("previous".into())))
            );
        }
    }
    #[test]
    fn first_user_message_provides_readable_session_title() {
        let mut app = App::new(false);
        app.set_thread(&json!({"id":"new"}));
        assert_eq!(app.session_title, "새 대화");
        app.upsert(&json!({"id":"user","type":"userMessage","content":[{"text":"Build an agent\nand keep the workspace"}]}));
        assert_eq!(app.session_title, "Build an agent");
    }
}
