//! Conversation state and actions. The renderer never calls Codex directly.
pub use crate::agent::{Agent, Approval, Choice, Entry, Kind, SessionSummary, Skill};
use crate::agent::{AgentCommand, Operation};
use crate::document;
use crate::engine::{Action, Canvas, Tone, wrap};
use crate::preferences::{self, Accent, Language, Preferences};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

mod activity;
mod commands;
mod command_palette;
mod doctor;
mod events;
mod images;
mod meter;
mod models;
mod model_catalog;
pub mod model_management;
mod mouse;
mod navigation;
mod progress;
pub mod providers;
mod questions;
pub mod remote_control;
mod settings;
#[cfg(test)]
mod test_protocol;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::rc::Rc;
use std::time::{Duration, Instant};
use std::time::{SystemTime, UNIX_EPOCH};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

fn ctrl_char(key: KeyEvent, expected: char) -> bool {
    key.modifiers.contains(KeyModifiers::CONTROL)
        && matches!(key.code, KeyCode::Char(c) if c.eq_ignore_ascii_case(&expected))
}

#[derive(Default, Debug)]
pub struct Editor {
    pub text: String,
    pub cursor: usize,
    // Byte ranges in the original text; the composer paints them as atomic chips.
    pasted: Vec<std::ops::Range<usize>>,
    anchor: Option<usize>,
}
impl Editor {
    pub fn selection(&self) -> Option<std::ops::Range<usize>> {
        self.anchor
            .filter(|at| *at != self.cursor)
            .map(|at| at.min(self.cursor)..at.max(self.cursor))
    }
    fn delete_selection(&mut self) -> bool {
        let selection = self.selection();
        self.anchor = None;
        if let Some(range) = selection {
            self.remove_range(range.start, range.end);
            true
        } else {
            false
        }
    }
    pub fn select_all(&mut self) {
        self.anchor = Some(0);
        self.cursor = self.text.len();
    }
    pub fn expand_pastes(&mut self) {
        self.pasted.clear();
    }
    pub fn insert(&mut self, text: &str) {
        let normalized = text.replace('\r', "");
        if normalized.is_empty() {
            return;
        }
        self.delete_selection();
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
        self.delete_selection();
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
        self.anchor = None;
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
    fn raw_at_visible(&self, visible: usize) -> usize {
        let mut raw_start = 0;
        let mut shown_start = 0;
        for range in &self.pasted {
            let plain_len = range.start - raw_start;
            if visible < shown_start + plain_len {
                return raw_start + visible - shown_start;
            }
            shown_start += plain_len;
            let chip_len = format!(
                "[붙여넣은 내용 · {}자]",
                self.text[range.clone()].chars().count()
            )
            .len();
            if visible < shown_start + chip_len {
                return if visible <= shown_start + chip_len / 2 {
                    range.start
                } else {
                    range.end
                };
            }
            shown_start += chip_len;
            raw_start = range.end;
        }
        (raw_start + visible.saturating_sub(shown_start)).min(self.text.len())
    }
    fn move_to_visible(&mut self, visible: usize) {
        self.cursor = self.raw_at_visible(visible);
    }
    fn visible_at_raw(&self, raw: usize) -> usize {
        let mut hidden = 0;
        let mut shown = 0;
        for range in &self.pasted {
            if raw <= range.start {
                break;
            }
            hidden += range.end - range.start;
            shown += format!(
                "[붙여넣은 내용 · {}자]",
                self.text[range.clone()].chars().count()
            )
            .len();
        }
        raw - hidden + shown
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
        if self.delete_selection() {
            return;
        }
        let end = self.cursor;
        self.left();
        self.remove_range(self.cursor, end);
    }
    pub fn delete(&mut self) {
        if self.delete_selection() {
            return;
        }
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

// Isometric ASCII wordmark; the O faces follow the user's woven, 3-D sketch.
// Keep all rows anchored to the same left edge when centering the logo.
const LOOM_WORDMARK: [&str; 11] = [
    r"       ___             ___            ___            ___",
    r"      /  /\           /  /\          /  /\          /__/\",
    r"     /  /:/          /  /::\        /  /::\        |  |::\",
    r"    /  /:/          /  /:/\:\      /  /:/\:\       |  |:|:\",
    r"   /  /:/          /  /:/  \:\    /  /:/  \:\    __|__|:|\:\",
    r"  /__/:/____      /__/:/ \__\:\  /__/:/ \__\:\  /__/::::| \:\",
    r"  \  \:\    \     \  \:\ /  /:/  \  \:\ /  /:/  \  \:\~~\__\/",
    r"   \  \:\    \     \  \:\  /:/    \  \:\  /:/    \  \:\",
    r"    \  \:\    \     \  \:\/:/      \  \:\/:/      \  \:\",
    r"     \  \:\____\     \  \::/        \  \::/        \  \:\",
    r"      \__\:::::/      \__\/          \__\/          \__\/",
];
const LOOM_COMPACT: [&str; 4] = [
    r"|      .----. .----. |\    /|",
    r"|     / /\  V  /\ \ | \  / |",
    r"|     \ \/  ^  \/ / |  \/  |",
    r"|_____ '----' '----' |      |",
];

fn welcome_rows(width: usize, height: usize, language: Language) -> Vec<ContentRow> {
    let lines: &[&str] = if height >= LOOM_WORDMARK.len() + 2
        && LOOM_WORDMARK.iter().all(|line| line.len() <= width)
    {
        &LOOM_WORDMARK
    } else if height >= LOOM_COMPACT.len() + 3
        && LOOM_COMPACT.iter().all(|line| line.len() <= width)
    {
        &LOOM_COMPACT
    } else {
        return vec![(
            if language == Language::English {
                "Type below to begin."
            } else {
                "아래에 지시를 입력하세요."
            }
            .into(),
            Tone::Muted,
            None,
        )];
    };
    let mut rows: Vec<ContentRow> = vec![];
    rows.resize(
        (height - lines.len()) / 2,
        (String::new(), Tone::Normal, None),
    );
    let logo_width = lines.iter().map(|line| line.len()).max().unwrap_or(0);
    let padding = (width - logo_width) / 2;
    rows.extend(lines.iter().map(|line| {
        (
            format!("{}{}", " ".repeat(padding), line),
            Tone::Accent,
            None,
        )
    }));
    rows
}
struct CachedChatRows {
    width: usize,
    count: usize,
    tail_len: usize,
    language: Language,
    rows: Rc<Vec<ContentRow>>,
    /// Start offsets of alternating conversation backgrounds. Calculated
    /// once with the transcript layout, not every time the composer repaints.
    bands: Rc<Vec<usize>>,
}
#[derive(Clone, Debug)]
pub struct QueuedMessage {
    pub effort: Option<String>,
    pub text: String,
    pub skills: Vec<Skill>,
    pub images: Vec<crate::attachment::ImageAttachment>,
    pub model: String,
}
/// Connection group of a model id in the model list, or `None` for Loom automatic routing:
/// `ollama/x` → local · ollama, `openrouter/google/x` → openrouter.
fn model_group(model: &str) -> Option<String> {
    if model == "auto" {
        return None;
    }
    // "<order><section>\u{1}<group>"; the order prefix keeps the section sortable.
    // LMX models (Loom's own format) come first.
    if model.starts_with("lmx/") {
        return Some("0LMX\u{1}로컬".into());
    }
    let (endpoint, _) = model.split_once('/')?;
    Some(format!("1로컬·직접 연결\u{1}{endpoint}"))
}
fn model_name_in_group(model: &str) -> &str {
    model.split_once('/').map_or(model, |(_, rest)| rest)
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
    result
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
    Providers,
    Routing,
    Runtime,
    Sessions,
    Settings,
    Permissions,
    Rewind,
}
/// Commands that do nothing useful without an argument; Enter in the menu completes them.
const COMMANDS_WITH_ARGUMENT: &[&str] = &["/ask", "/plan", "/explore", "/task", "/agent", "/approve", "/tool", "/image"];

const COMMANDS: &[(&str, &str)] = &[
    ("/help", "조작과 명령 안내"),
    ("/ask", "작업을 멈추지 않고 병렬 질문"),
    ("/plan", "목표를 계획하고 에이전트로 분할 실행"),
    ("/goal", "목표: /goal [set TEXT|pause|resume|clear]"),
    ("/clear", "기록을 보존하고 새 대화로 초기화"),
    ("/compression", "현재 대화의 실제 컨텍스트 압축"),
    ("/compact", "/compression과 동일: 컨텍스트 압축"),
    ("/remote", "폰·브라우저에서 원격 조작: /remote [local|off]"),
    ("/stop", "현재 응답 중단 (/cancel도 가능)"),
    ("/status", "현재 실행·목표·대기열·작업 상태"),
    ("/doctor", "환경·도구체인·로컬모델 진단: /doctor"),
    ("/cost", "토큰 사용량 및 로컬 모델 비용 절감액: /cost"),
    ("/context", "현재 세션 참조 파일 및 컨텍스트 현황: /context"),
    ("/settings", "UI 언어·악센트 색상·작업 패널"),
    ("/skills", "설치된 스킬 선택"),
    (
        "/permissions",
        "권한 확인/변경: /permissions sandbox MODE | approval MODE | confirm | cancel",
    ),
    ("/model", "모델 선택 · 다음 실행에도 기억"),
    ("/provider", "공급자 연결: /provider [add|login chatgpt|test NAME|enable NAME|disable NAME|remove NAME|refresh]"),
    ("/environment", "환경: 모델·스킬·권한"),
    ("/routing", "자동 라우팅·비용 상한·연결별 자동 선택"),
    ("/runtime", "로컬 추론 엔진·모델 다운로드·관리"),
    ("/work", "작업: 목록·변경·승인·사용량"),
    ("/image", "이미지 첨부: /image PATH · remove N · clear"),
    ("/diff", "보고된 변경: /diff [N] (N은 최근 이벤트 순서)"),
    (
        "/agents",
        "에이전트 목록과 직접 제어: /agent N open|send TEXT|stop",
    ),
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
        "대기열 확인 · /queue pause|resume|drop N|force N|clear",
    ),
    ("/quit", "화면 종료"),
    ("/chat", "대화와 입력으로 돌아가기"),
    ("/panel", "대화 옆 현황 열기/닫기"),
    ("/copy", "최근 턴 전체 로그 복사: /copy [N], 최신이 1"),
    ("/branch", "완료된 모델 메시지에서 분기: /branch [N]"),
    ("/tool", "최근 도구 로그: /tool N [next|prev]"),
    ("/agent", "에이전트: /agent [list|N [open|send TEXT|stop]]"),
    ("/approve", "승인: /approve N M | detail N | later"),
    (
        "/skill",
        "스킬: /skill [list|NAME|info NAME|remove NAME|clear]",
    ),
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
    Goal(crate::agent::GoalAction),
    Compact,
    DirectAgent(String, Option<String>),
    Inquiry(String),
    Explore(String),
    Submit(String),
    Interrupt,
    PasteClipboard,
    Reply(String, String),
    Answer(String, Vec<crate::agent::QuestionAnswer>),
    CancelQuestions(String),
    OpenAgent(String),
    OpenLink(String),
    ReadAgent(String),
    Copy(String),
    Quit,
    RefreshSkills,
    RefreshModels,
    Provider(String),
    Session(Option<String>),
    RefreshSessions,
    LoadMoreSessions(String),
    DeleteSession(String),
    Fork(String),
    Rewind(String),
    PreviewRewind(String),
    SaveSettings,
    ExportDiagnostics,
    /// Turn remote control on (Some: whether to open an internet tunnel too) or off.
    Remote(Option<bool>),
    UpdatePermission(String, String),
    TrustWorkspace(bool),
    RefreshUsage,
}

pub struct App {
    /// Remote control address and QR code while it is on.
    pub remote: Option<remote_control::RemoteView>,
    /// Last model handed to the native engine's background preload.
    preloaded_model: String,
    pub goal: Option<crate::agent::GoalState>,
    pub compacting: bool,
    pub recovery_available: bool,
    pub thread: Option<String>,
    pub turn: Option<String>,
    pub model: String,
    pub effort: Option<String>,
    pub runtime_effort: Option<String>,
    pub model_profiles: Vec<crate::agent::ModelProfile>,
    pub status: String,
    pub readonly: bool,
    pub entries: Vec<Entry>,
    // The startup mark is shown only until the session's first accepted task.
    welcome_dismissed: bool,
    // Finished messages dominate chat redraw costs. Reformat only entries whose
    // text or available width has changed, including during streaming.
    message_cache: MessageCache,
    // Reuse the laid-out transcript while only the composer or status changes.
    chat_rows_cache: RefCell<Option<CachedChatRows>>,
    pub agents: Vec<Agent>,
    pub tasks: Vec<crate::agent::ActivityTask>,
    pub selected_task: Option<String>,
    /// Sessions entered from a task, newest last: (session id, title) to return to.
    pub session_trail: Vec<(String, String)>,
    /// (target, origin id, origin title) until the target session loads.
    pub pending_enter: Option<(String, String, String)>,
    pub approvals: Vec<Approval>,
    pub input_forms: Vec<questions::InputForm>,
    pub approval_popup_dismissed: bool,
    pub trust_prompt: Option<String>,
    pub trust_pending: bool,
    pub workspace_trust: Option<bool>,
    pub restricted_workspace: bool,
    pub diff: String,
    selected_diff_entry: Option<String>,
    reviewed_diff: String,
    reviewed_changes: HashMap<String, String>,
    branch_pending: Option<String>,
    escape_pressed: Option<Instant>,
    rewind_confirm: Option<String>,
    rewind_preview: Option<(usize, usize, usize, Option<String>)>,
    resolved_model: Option<(String, String, String)>,
    /// Last measured generation speed and the model it belongs to.
    model_speed: Option<(String, f32)>,
    model_details_open: bool,
    /// Provider groups opened in the model list (all closed by default).
    open_model_groups: HashSet<String>,
    recent_change_ids: HashSet<String>,
    pub usage: String,
    pub tokens: crate::agent::TokenUsage,
    usage_expanded: bool,
    /// Loom router call ledger (~/.loom/model-usage.jsonl), read when /usage opens.
    /// `None` path: never read from disk (self-check fixtures).
    pub(crate) model_ledger_path: Option<std::path::PathBuf>,
    pub(crate) model_ledger: Vec<crate::core::model::router::CallRecord>,
    /// Model whose usage details are open (`endpoint|model`).
    pub(crate) usage_model: Option<String>,
    pub quotas: Vec<crate::agent::QuotaSnapshot>,
    pub execution_limit: Option<crate::agent::LimitKind>,
    /// Model selected when the limit was reported; choosing another model lifts it.
    pub execution_limit_model: Option<String>,
    pub work_phase: crate::agent::WorkPhase,
    pub editor: Editor,
    // Submitted text, oldest first. The cursor is set only while browsing history.
    input_history: Vec<String>,
    history_position: Option<usize>,
    mouse_state: mouse::MouseState,
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
    interrupt_requested: bool,
    pub pending: HashMap<u64, Operation>,
    pub submitted: Option<String>,
    pub submitted_model: Option<String>,
    pub submitted_effort: Option<String>,
    pub queued: VecDeque<QueuedMessage>,
    pub queue_paused: bool,
    queue_scroll: usize,
    queue_scroll_limit: usize,
    // The current turn must finish before the promoted message may start.
    force_after_interrupt: Option<String>,
    expanded: HashSet<String>,
    tool_pages: HashMap<String, usize>,
    positions: HashMap<View, (usize, bool)>,
    pub preferences: Preferences,
    model_engine: Option<String>,
    model_memory_dirty: bool,
    model_default_allowed: bool,
    pub(crate) workspace_root: Option<std::path::PathBuf>,
    settings_page: settings::Page,
    tool_overrides: HashMap<String, bool>,
    entry_times: HashMap<String, u64>,
    notice_log: VecDeque<(bool, String)>,
    pub bell_pending: bool,
    paste_pending: bool,
    paste_confirmed: bool,
    immediate_pending: bool,
    // Confirmed by the runtime only; unknown until explicitly set or reported.
    pub sandbox_mode: Option<String>,
    /// `allow` or `deny` as the backend reported it.
    pub network_mode: Option<String>,
    pub approval_mode: Option<String>,
    pub permission_pending: Option<(String, String)>,
    pub permission_confirm: Option<(String, String)>,
    pub panel_open: bool,
    pub wide_layout: bool,
    pub sessions: Vec<SessionSummary>,
    pub sessions_loaded: bool,
    /// A saved conversation awaiting the confirming second Delete.
    pub session_delete_armed: Option<String>,
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
    pub images: Vec<crate::attachment::ImageAttachment>,
    pub submitted_images: Vec<crate::attachment::ImageAttachment>,
    pub active_images: Vec<crate::attachment::ImageAttachment>,
    image_drop: Option<(usize, usize, Instant)>,
    pub selected_skills: Vec<Skill>,
    pub submitted_skills: Vec<Skill>,
    pub skills_loaded: bool,
    pub skill_errors: Vec<String>,
    pub models: Vec<String>,
    pub model_catalog: Vec<crate::agent::ModelCatalogEntry>,
    pub model_catalog_sources: Vec<crate::agent::ModelCatalogSource>,
    pub model_catalog_loading: bool,
    model_search: Editor,
    model_command_draft: Option<Editor>,
    model_price_groups: bool,
    model_free_only: bool,
    pub provider_accounts: Vec<loom_core::models::registry::Summary>,
    pub routing_max_cost: loom_core::models::CostClass,
    pub local_models: crate::local_models::Manager,
    pub runtime_checkpoints: Vec<loom_core::models::checkpoints::Checkpoint>,
    runtime_backend: String,
    runtime_remove: Option<String>,
    runtime_reload: bool,
    menu_open: bool,
    command_search: Editor,
    menu_dismissed: bool,
    menu_index: usize,
}
impl App {
    pub fn new(readonly: bool) -> Self {
        Self {
            remote: None,
            preloaded_model: String::new(),
            goal: None,
            compacting: false,
            recovery_available: false,
            thread: None,
            turn: None,
            model: String::new(),
            effort: None,
            runtime_effort: None,
            model_profiles: Vec::new(),
            status: "연결 중".into(),
            readonly,
            entries: Vec::new(),
            welcome_dismissed: false,
            message_cache: RefCell::new(HashMap::new()),
            chat_rows_cache: RefCell::new(None),
            agents: Vec::new(),
            tasks: Vec::new(),
            selected_task: None,
            session_trail: Vec::new(),
            pending_enter: None,
            approvals: Vec::new(),
            input_forms: Vec::new(),
            approval_popup_dismissed: false,
            trust_prompt: None,
            trust_pending: false,
            workspace_trust: None,
            restricted_workspace: false,
            diff: String::new(),
            selected_diff_entry: None,
            reviewed_diff: String::new(),
            reviewed_changes: HashMap::new(),
            branch_pending: None,
            escape_pressed: None,
            rewind_confirm: None,
            rewind_preview: None,
            resolved_model: None,
            model_speed: None,
            model_details_open: false,
            open_model_groups: HashSet::new(),
            recent_change_ids: HashSet::new(),
            usage: String::new(),
            tokens: crate::agent::TokenUsage::default(),
            usage_expanded: false,
            model_ledger_path: crate::core::model::cli::ledger_path(),
            model_ledger: Vec::new(),
            usage_model: None,
            quotas: Vec::new(),
            execution_limit: None,
            execution_limit_model: None,
            work_phase: crate::agent::WorkPhase::default(),
            editor: Editor::default(),
            input_history: Vec::new(),
            history_position: None,
            mouse_state: mouse::MouseState::default(),
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
            interrupt_requested: false,
            pending: HashMap::new(),
            submitted: None,
            submitted_model: None,
            submitted_effort: None,
            queued: VecDeque::new(),
            queue_paused: false,
            queue_scroll: 0,
            queue_scroll_limit: 0,
            force_after_interrupt: None,
            expanded: HashSet::new(),
            tool_pages: HashMap::new(),
            positions: HashMap::new(),
            preferences: Preferences::default(),
            model_engine: None,
            model_memory_dirty: false,
            model_default_allowed: false,
            workspace_root: None,
            settings_page: settings::Page::Main,
            tool_overrides: HashMap::new(),
            entry_times: HashMap::new(),
            notice_log: VecDeque::new(),
            bell_pending: false,
            paste_pending: false,
            paste_confirmed: false,
            immediate_pending: false,
            sandbox_mode: None,
            network_mode: None,
            approval_mode: None,
            permission_pending: None,
            permission_confirm: None,
            panel_open: false,
            wide_layout: false,
            sessions: Vec::new(),
            sessions_loaded: false,
            session_delete_armed: None,
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
            images: Vec::new(),
            submitted_images: Vec::new(),
            active_images: Vec::new(),
            image_drop: None,
            selected_skills: Vec::new(),
            submitted_skills: Vec::new(),
            skills_loaded: false,
            skill_errors: Vec::new(),
            models: Vec::new(),
            model_catalog: Vec::new(),
            model_catalog_sources: Vec::new(),
            model_catalog_loading: false,
            model_search: Editor::default(),
            model_command_draft: None,
            model_price_groups: true,
            model_free_only: false,
            provider_accounts: Vec::new(),
            routing_max_cost: loom_core::models::COST_CEILING,
            local_models: Default::default(),
            runtime_checkpoints: Vec::new(),
            runtime_backend: "auto".into(),
            runtime_remove: None,
            runtime_reload: false,
            menu_open: false,
            command_search: Editor::default(),
            menu_dismissed: false,
            menu_index: 0,
        }
    }

    pub fn set_workspace_root(&mut self, root: std::path::PathBuf) {
        self.workspace_root = Some(root);
    }
    pub fn permission_applied(&mut self) {
        if let Some((kind, value)) = self.permission_pending.take() {
            match kind.as_str() {
                "sandbox" => self.sandbox_mode = Some(value),
                "network" => self.network_mode = Some(value),
                "approval" => self.approval_mode = Some(value),
                _ => {}
            }
            self.notify("권한 설정을 코어에서 확인했습니다 · 다음 작업부터 적용");
        }
    }
    pub fn branch_failed(&mut self, reason: &str) {
        self.branch_pending = None;
        self.error(reason);
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
        self.branch_pending = None;
        let message = message.into();
        self.log_notice(true, message.clone());
        if !self.preferences.notify_errors {
            return;
        }
        self.bell_pending |= self.preferences.terminal_bell;
        self.notice = message;
        self.notice_deadline = Some(Instant::now() + Duration::from_secs(7));
        self.notice_error = true;
    }
    pub fn notify(&mut self, message: impl Into<String>) {
        self.notice = message.into();
        self.log_notice(false, self.notice.clone());
        self.notice_deadline = Some(Instant::now() + Duration::from_secs(4));
        self.notice_error = false;
    }
    pub fn tick(&mut self, now: Instant) -> bool {
        // A selected native LMX model loads and warms up while the user types, so the
        // first answer does not wait for gigabytes of experts.
        if self.model != self.preloaded_model {
            self.preloaded_model = self.model.clone();
            loom_core::models::lmx_native_runtime::preload(&self.model);
        }
        let (download_changed, completed) = self.local_models.poll();
        if completed { self.refresh_runtime(); self.runtime_reload = true; }
        let mut changed = self
            .image_drop
            .is_some_and(|(_, _, deadline)| now >= deadline)
            && self.flush_image_drop();
        changed |= download_changed;
        changed |= self.tick_drag_scroll(now);
        if self.notice_deadline.is_some_and(|deadline| now >= deadline) {
            self.notice.clear();
            self.notice_deadline = None;
            self.notice_error = false;
            changed = true;
        }
        if self.is_working() {
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
        }
        changed
    }

    fn is_working(&self) -> bool {
        !self.interrupt_requested && (self.compacting
            || self.busy
            || self.turn.is_some()
            || self.submitted.is_some()
            || !self.active_tasks.is_empty())
            && (self.execution_limit.is_none()
                || self.active_tasks.iter().any(|id| {
                    self.tasks.iter().any(|task| {
                        &task.id == id && task.worker == Some(crate::agent::WorkerKind::Local)
                    })
                }))
            && !self.approvals.iter().any(|approval| !approval.answering)
            && !self.input_forms.iter().any(|f| f.request.blocking)
            && self.trust_prompt.is_none()
            && !matches!(
                self.status.as_str(),
                "연결 끊김" | "failed" | "실패" | "오류"
            )
    }
    pub fn clear_text_selection(&mut self) {
        self.reset_mouse();
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
        self.paste_pending = false;
        self.paste_confirmed = false;
        self.image_drop = None;
        if self.readonly
            || text.is_empty()
            || self.trust_prompt.is_some()
            || self.approval_visible()
        {
            return;
        }
        if self.question_paste(text) {
            return;
        }
        if self.menu_open {
            self.command_search.insert(&text.replace(['\n', '\r'], " "));
            self.menu_index = 0;
            return;
        }
        if self.view == View::Models && !self.composer_active {
            if text.starts_with('/') {
                self.model_command_draft = Some(std::mem::take(&mut self.editor));
            } else {
                self.paste_model_search(text);
                return;
            }
        }
        self.reset_mouse();
        self.focus = None;
        self.branch_entry = None;
        self.menu_open = false;
        self.menu_dismissed = true;
        if self.view != View::Chat && !self.wide_layout {
            self.switch_view(View::Chat);
        }
        self.composer_active = true;
        if !self.paste_image_paths(text) {
            self.history_position = None;
            self.editor.paste(text);
        }
    }
    fn recall_input(&mut self, down: bool) {
        let next = match (self.history_position, down) {
            (Some(position), false) => Some(position.saturating_sub(1)),
            (Some(position), true) if position + 1 < self.input_history.len() => Some(position + 1),
            (Some(_), true) => None,
            (None, false) => self.input_history.len().checked_sub(1),
            (None, true) => return,
        };
        self.history_position = next;
        self.editor = Editor::default();
        if let Some(position) = next {
            self.editor.insert(&self.input_history[position]);
        }
    }
    fn recall_queued_for_edit(&mut self) -> bool {
        if !self.editor.text.is_empty()
            || !self.images.is_empty()
            || !self.selected_skills.is_empty()
        {
            return false;
        }
        let was_front = self.queued.len() == 1;
        let Some(item) = self.queued.pop_back() else {
            return false;
        };
        if was_front && self.force_after_interrupt.is_some() {
            self.force_after_interrupt = None;
            self.interrupt_requested = false;
        }
        if self.queued.is_empty() {
            self.queue_paused = false;
            self.immediate_pending = false;
        }
        self.history_position = None;
        self.editor = Editor::default();
        self.editor.insert(&item.text);
        self.selected_skills = item.skills;
        self.images = item.images;
        self.model = item.model;
        self.effort = item.effort;
        self.notice.clear();
        self.notice_deadline = None;
        self.notice_error = false;
        true
    }
    pub fn invalidate_chat_rows(&self) {
        self.chat_rows_cache.borrow_mut().take();
    }
    fn has_reported_changes(&self) -> bool {
        (!self.diff.is_empty() && self.diff != self.reviewed_diff)
            || self.entries.iter().any(|entry| {
                entry.kind == Kind::Change
                    && !entry.body.is_empty()
                    && self.recent_change_ids.contains(&entry.id)
                    && self.reviewed_changes.get(&entry.id) != Some(&entry.body)
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
    pub(crate) fn active_tool_entry(&self) -> Option<&Entry> {
        self.entries.iter().rev().find(|entry| {
            matches!(entry.kind, Kind::Tool | Kind::Change) && entry.status == "inProgress"
        })
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
        let bands = Rc::new(transcript_band_starts(&rows));
        *self.chat_rows_cache.borrow_mut() = Some(CachedChatRows {
            width,
            count,
            tail_len,
            language: self.preferences.language,
            rows: Rc::clone(&rows),
            bands: Rc::clone(&bands),
        });
        rows
    }
    fn cached_chat_bands(&self) -> Rc<Vec<usize>> {
        Rc::clone(
            &self
                .chat_rows_cache
                .borrow()
                .as_ref()
                .expect("chat rows cached")
                .bands,
        )
    }
    /// Finished turns in conversation order with their request and final reply text.
    fn rewind_points(&self) -> Vec<(String, String, String)> {
        let mut points: Vec<(String, String, String)> = Vec::new();
        for entry in &self.entries {
            let Some(turn) = self.entry_turns.get(&entry.id).filter(|turn| self.finished_turns.contains(*turn)) else {
                continue;
            };
            let index = match points.iter().position(|point| &point.0 == turn) {
                Some(index) => index,
                None => {
                    points.push((turn.clone(), String::new(), String::new()));
                    points.len() - 1
                }
            };
            let body = entry.body.lines().find(|line| !line.trim().is_empty()).unwrap_or_default().trim();
            match entry.kind {
                Kind::User if points[index].1.is_empty() => points[index].1 = body.to_owned(),
                Kind::Assistant if entry.title != crate::agent::PROGRESS_TITLE && !body.is_empty() => {
                    points[index].2 = body.to_owned();
                }
                _ => {}
            }
        }
        for point in &mut points {
            if point.1.is_empty() {
                point.1 = std::mem::take(&mut point.2);
            }
        }
        points
    }
    pub fn activate(&mut self, action: Action) -> Option<Intent> {
        match action {
            Action::Rewind(turn) if !self.readonly && self.preferences.experimental_rewind => {
                if self.finished_turns.contains(&turn) {
                    self.rewind_confirm = Some(turn);
                    self.rewind_preview = None;
                    self.focus = Some(Action::ConfirmConversationRewind);
                    return self.rewind_confirm.clone().map(Intent::PreviewRewind);
                }
            }
            Action::ConfirmRewind if !self.readonly && self.preferences.experimental_rewind => {
                if !self
                    .rewind_preview
                    .as_ref()
                    .is_some_and(|(_, _, _, blocked)| blocked.is_none())
                {
                    self.error("코드 복원 미리보기를 확인한 뒤 선택하세요.");
                    return None;
                }
                if self.session_switch_block_reason().is_none() && self.branch_pending.is_none() {
                    if let Some(turn) = self.rewind_confirm.take() {
                        self.branch_pending = self.thread.clone();
                        self.notify("대화와 코드를 되돌리는 중…");
                        return Some(Intent::Rewind(turn));
                    }
                } else {
                    self.error("현재 작업을 마친 뒤 되돌리세요.");
                }
            }
            Action::ConfirmConversationRewind
                if !self.readonly && self.preferences.experimental_rewind =>
            {
                if self.session_switch_block_reason().is_none() && self.branch_pending.is_none() {
                    if let Some(turn) = self.rewind_confirm.take() {
                        self.branch_pending = self.thread.clone();
                        self.notify("대화만 되돌리는 중…");
                        return Some(Intent::Fork(turn));
                    }
                } else {
                    self.error("현재 작업을 마친 뒤 되돌리세요.");
                }
            }
            Action::Rewind(_) | Action::ConfirmRewind | Action::ConfirmConversationRewind => {}
            Action::RewindDelta(_, _) => {}
            Action::ToggleModelDetails => self.model_details_open = !self.model_details_open,
            Action::ModelSearch => {
                self.composer_active = false;
                self.focus = Some(Action::ModelSearch);
            }
            Action::ClearModelSearch => { self.model_search = Editor::default(); self.reset_model_search(); }
            Action::ModelGrouping => { self.model_price_groups = !self.model_price_groups; self.reset_model_search(); }
            Action::ModelFreeOnly => { self.model_free_only = !self.model_free_only; self.reset_model_search(); }
            Action::ToggleModelGroup(group) => {
                if !self.open_model_groups.remove(&group) {
                    self.open_model_groups.insert(group);
                }
            }
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
            | Action::CancelQuestions
            | Action::DismissQuestion => return self.question_action(&action),
            Action::ToggleApproval(id) => {
                for approval in &mut self.approvals {
                    if approval.id == id {
                        approval.expanded = !approval.expanded;
                    }
                }
            }
            Action::Toggle(id) => {
                let current = self.tool_is_expanded(&id);
                self.tool_overrides.insert(id.clone(), !current);
                if !current {
                    self.expanded.insert(id.clone());
                } else {
                    self.expanded.remove(&id);
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
                self.usage_expanded = false;
                self.panel_open = true;
                self.switch_view(View::Agents);
            }
            Action::Diff => {
                self.reviewed_diff.clone_from(&self.diff);
                for entry in self
                    .entries
                    .iter()
                    .filter(|e| e.kind == Kind::Change && self.recent_change_ids.contains(&e.id))
                {
                    self.reviewed_changes
                        .insert(entry.id.clone(), entry.body.clone());
                }
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
                    if let Some(entry) = self.entries.iter().find(|entry| entry.id == id) {
                        self.reviewed_changes.insert(id.clone(), entry.body.clone());
                    }
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
                let activity_panel = self.preferences.activity_panel;
                self.switch_view(View::Chat);
                // Chat is the reading surface. A side panel is opt-in via /panel.
                self.panel_open = activity_panel;
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
            Action::OpenThread(id) => return Some(Intent::OpenAgent(id)),
            Action::OpenLink(url) => return Some(Intent::OpenLink(url)),
            Action::EnterSession(id) => {
                if self.readonly {
                    self.error("열람 전용 창입니다. 메인 창에서 실행하세요.");
                } else if let Some(reason) = self.session_switch_block_reason() {
                    self.error(format!("{reason} 지금 보려면 '새 창에서 열기 ↗'를 쓰세요."));
                } else if let Some(origin) = self.thread.clone() {
                    self.pending_enter = Some((id.clone(), origin, self.session_title.clone()));
                    self.selected_task = None;
                    return Some(Intent::Session(Some(id)));
                }
            }
            Action::ReturnSession => {
                if let Some((id, _)) = self.session_trail.last().cloned() {
                    if let Some(reason) = self.session_switch_block_reason() {
                        self.error(reason);
                    } else {
                        self.selected_task = None;
                        return Some(Intent::Session(Some(id)));
                    }
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
                return self.setting_action(&setting);
            }
            Action::SettingChoices(_) => {}
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
                        | ("network", "allow" | "deny")
                ) {
                    return None;
                }
                let current = match kind.as_str() {
                    "sandbox" => &self.sandbox_mode,
                    "network" => &self.network_mode,
                    _ => &self.approval_mode,
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
            Action::SelectEntry(_) | Action::MessageRow(_, _, _) => {
                self.clear_text_selection();
            }
            Action::Branch(id) if !self.readonly => {
                self.restore_input_after_message_action();
                if self.branch_pending.is_some() {
                    return None;
                }
                if self.compacting
                    || self.busy
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
                    let turn = turn.clone();
                    self.branch_pending = self.thread.clone();
                    self.notify(if self.preferences.language == Language::English {
                        "Creating branch…"
                    } else {
                        "새 분기를 만드는 중…"
                    });
                    return Some(Intent::Fork(turn));
                } else {
                    self.error("분기할 완료된 턴을 찾을 수 없습니다.");
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
                if self.view == View::Settings {
                    return self.activate(Action::Back);
                }
                self.panel_open = !self.panel_open;
                self.preferences.activity_panel = self.panel_open;
                if !self.panel_open {
                    self.switch_view(View::Chat);
                }
                return Some(Intent::SaveSettings);
            }
            Action::Menu => {
                self.menu_open = !self.menu_open;
                self.command_search = Editor::default();
                self.composer_active = true;
                self.menu_dismissed = false;
                self.menu_index = 0;
                self.focus = None;
            }
            Action::Command(command) if self.menu_open => {
                return self.select_palette_command(command, false);
            }
            Action::Command(command) => return self.command(&command),
            Action::Skill(path) if !self.readonly => {
                if let Some(skill) = self.skills.iter().find(|skill| skill.path == path).cloned() {
                    let keep_picker = self.view == View::Skills;
                    if !self.selected_skills.iter().any(|s| s.path == path) {
                        self.selected_skills.push(skill);
                    }
                    if !self.menu_open && !keep_picker {
                        if let Some(range) = loom_core::instructions::skill_mention_at(&self.editor.text, self.editor.cursor) {
                            self.editor.anchor = None;
                            self.editor.remove_range(range.start, range.end);
                        }
                    }
                    self.menu_open = false;
                    self.menu_dismissed = true;
                    if keep_picker {
                        self.focus = self.skills.iter()
                            .find(|skill| !self.selected_skills.iter().any(|selected| selected.path == skill.path))
                            .map(|skill| Action::Skill(skill.path.clone()));
                        self.notify("스킬 추가됨 · 더 선택하거나 Esc로 돌아가세요");
                    } else {
                        self.switch_view(View::Chat);
                        self.focus = None;
                        self.notify("스킬 추가됨 · $로 더 추가하거나 지시를 전송하세요");
                    }
                }
            }
            Action::RemoveSkill(path) => self.selected_skills.retain(|skill| skill.path != path),
            Action::RemoveImage(index) => {
                if index < self.images.len() {
                    self.images.remove(index);
                }
            }
            Action::RemoveQueued(index) => {
                self.queued.remove(index);
                if index < self.queue_scroll {
                    self.queue_scroll = self.queue_scroll.saturating_sub(1);
                }
                if index == 0 && self.force_after_interrupt.is_some() {
                    self.force_after_interrupt = None;
                    self.notify("우선 전송이 취소되었습니다.");
                }
                self.focus = None;
                if self.queued.is_empty() {
                    self.queue_paused = false;
                    self.queue_scroll = 0;
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
                self.queue_scroll = 0;
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
                    self.status = "중단됨".into();
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
            Action::QueueViewport => {}
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
                    let lifted = self.execution_limit.is_some()
                        && self.execution_limit_model.as_deref() != Some(model.as_str());
                    self.remember_model_choice();
                    self.model = model;
                    let saved_effort = self
                        .model_engine
                        .as_ref()
                        .and_then(|engine| self.preferences.model_selections.get(engine))
                        .and_then(|saved| saved.efforts.get(&self.model))
                        .cloned();
                    let had_saved_effort = saved_effort.is_some();
                    self.effort = saved_effort.flatten();
                    if lifted {
                        self.lift_limit_for_model_change();
                    }
                    self.reconcile_effort();
                    let fast_local_default = !had_saved_effort && self.apply_fast_local_default();
                    self.remember_model_choice();
                    if fast_local_default {
                        self.notify("로컬 모델은 추론 끄기로 시작합니다. 환경 → 모델에서 바꾸면 기억합니다.");
                    } else if !(had_effort && self.effort.is_none()) {
                        self.notify("다음 메시지부터 선택한 모델을 사용합니다");
                    }
                    if self.view == View::Models {
                        self.scroll = 0;
                        self.follow = false;
                        self.focus = if self.model_profile().is_some_and(|p| p.efforts.is_empty()) {
                            Some(Action::Model(self.model.clone()))
                        } else {
                            Some(Action::Effort(self.effort.clone().unwrap_or_default()))
                        };
                        if matches!(self.focus, Some(Action::Effort(_))) {
                            self.scroll = usize::MAX;
                        }
                        self.composer_active = false;
                    }
                }
            }
            Action::Skill(_) | Action::Model(_) => {}
            Action::Send => {
                self.flush_image_drop();
                if !self.editor.text.starts_with('/')
                    && !self.editor.pasted.is_empty()
                    && !self.paste_confirmed
                {
                    let confirm = match self.preferences.paste_confirm {
                        preferences::PasteConfirm::Always => true,
                        preferences::PasteConfirm::Never => false,
                        preferences::PasteConfirm::Multiline => self
                            .editor
                            .pasted
                            .iter()
                            .any(|range| self.editor.text[range.clone()].contains('\n')),
                    };
                    if confirm {
                        self.paste_pending = true;
                        self.focus = Some(Action::Setting("paste:confirm".into()));
                        return None;
                    }
                }
                self.paste_pending = false;
                self.paste_confirmed = false;
                if self.permission_pending.is_some() {
                    self.error("권한 설정을 코어에서 확인한 뒤 전송할 수 있습니다.");
                    return None;
                }
                if self.editor.text.trim_start().starts_with('/')
                    && !self
                        .editor
                        .text
                        .trim_start()
                        .strip_prefix("/plan ")
                        .is_some_and(|goal| !goal.trim().is_empty())
                {
                    let command = self.editor.text.trim().to_owned();
                    self.history_position = None;
                    if self.preferences.input_history == preferences::InputHistory::Keep
                        && self.input_history.last() != Some(&command)
                    {
                        self.input_history.push(command.clone());
                    }
                    let intent = self.command(&command);
                    if self.view != View::Models || self.focus != Some(Action::ModelSearch) {
                        self.composer_active = true;
                        self.focus = None;
                    }
                    return intent;
                }
                if !self.readonly
                    && (!self.editor.text.trim().is_empty() || !self.images.is_empty())
                {
                    self.branch_entry = None;
                    self.notice.clear();
                    self.notice_deadline = None;
                    self.notice_error = false;
                    let text = self.editor.take_text();
                    let catalog: Vec<_> = self.skills.iter().map(|skill| loom_core::instructions::Skill {
                        name: skill.name.clone(), path: skill.path.clone().into(), description: String::new(),
                    }).collect();
                    for mention in loom_core::instructions::mentioned_skills(&text, &catalog) {
                        if let Some(skill) = self.skills.iter().find(|skill| skill.name == mention.name) {
                            if !self.selected_skills.iter().any(|selected| selected.path == skill.path) {
                                self.selected_skills.push(skill.clone());
                            }
                        }
                    }
                    self.history_position = None;
                    if self.preferences.input_history == preferences::InputHistory::Keep
                        && !text.trim().is_empty()
                        && self.input_history.last() != Some(&text)
                    {
                        self.input_history.push(text.clone());
                    }
                    let item = QueuedMessage {
                        effort: self.effort.clone(),
                        text,
                        skills: std::mem::take(&mut self.selected_skills),
                        images: std::mem::take(&mut self.images),
                        model: self.model.clone(),
                    };
                    self.welcome_dismissed = true;
                    self.follow = true;
                    if self.compacting
                        || self.busy
                        || self.turn.is_some()
                        || self.submitted.is_some()
                        || !self.approvals.is_empty()
                        || !self.input_forms.is_empty()
                        || !self.queued.is_empty()
                        || self.thread.is_none()
                        || self.queue_paused
                        || self.execution_limit.is_some()
                    {
                        if self.queued.is_empty() {
                            self.queue_scroll = 0;
                        }
                        self.queued.push_back(item);
                        if self.preferences.enter_mode == preferences::EnterMode::Immediate
                            && self.turn.is_some()
                            && !self.compacting
                            && self.approvals.is_empty()
                            && self.input_forms.is_empty()
                        {
                            return self.activate(Action::ForceQueued(self.queued.len() - 1));
                        }
                        if self.preferences.enter_mode == preferences::EnterMode::Immediate
                            && (self.busy || self.submitted.is_some())
                            && self.turn.is_none()
                        {
                            let item = self.queued.pop_back().unwrap();
                            self.queued.push_front(item);
                            self.immediate_pending = true;
                        }
                        self.notify(format!("전송 대기열에 추가됨 · {}개", self.queued.len()));
                    } else {
                        return Some(self.begin_submission(item));
                    }
                }
            }
            Action::Interrupt => {
                if !self.readonly
                    && (self.turn.is_some()
                        || self.submitted.is_some()
                        || (self.busy && self.status == "inProgress"))
                    && !self.interrupt_requested
                    && self.force_after_interrupt.is_none()
                    && !self
                        .pending
                        .values()
                        .any(|method| *method == Operation::Interrupt)
                {
                    self.interrupt_requested = true;
                    self.status = "중단됨".into();
                    for entry in &mut self.entries {
                        if entry.status == "inProgress" {
                            entry.status = "interrupted".into();
                        }
                    }
                    self.invalidate_chat_rows();
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
            Action::TranscriptText => {}
            Action::Quit => return Some(Intent::Quit),
        }
        None
    }
    fn copy_turn_text(&self, entry_id: &str) -> Option<String> {
        let turn = self.entry_turns.get(entry_id)?;
        let sections = self
            .entries
            .iter()
            .filter(|entry| self.entry_turns.get(&entry.id) == Some(turn))
            .filter(|entry| !entry.body.is_empty())
            .map(|entry| {
                let kind = match entry.kind {
                    Kind::User => "User",
                    Kind::Assistant => "Assistant",
                    Kind::Tool => "Tool",
                    Kind::Change => "Change",
                    Kind::System => "System",
                };
                let heading = if matches!(entry.kind, Kind::User | Kind::Assistant)
                    || entry.title.trim().is_empty()
                {
                    kind.to_owned()
                } else {
                    format!("{kind} · {}", entry.title.trim())
                };
                format!("[{heading}]\n{}", entry.body)
            })
            .collect::<Vec<_>>();
        (!sections.is_empty()).then(|| sections.join("\n\n"))
    }
    fn begin_submission(&mut self, item: QueuedMessage) -> Intent {
        self.recovery_available = false;
        self.submitted = Some(item.text.clone());
        self.submitted_skills = item.skills;
        self.submitted_images = item.images;
        self.submitted_model = Some(item.model);
        self.submitted_effort = item.effort;
        self.status = "전송 중".into();
        self.follow = true;
        Intent::Submit(item.text)
    }
    pub fn next_queued(&mut self) -> Option<Intent> {
        if self.immediate_pending
            && self.turn.is_some()
            && !self.compacting
            && self.approvals.is_empty()
            && self.input_forms.is_empty()
        {
            self.immediate_pending = false;
            return self.activate(Action::ForceQueued(0));
        }
        if self.readonly
            || self.busy
            || self.compacting
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
        self.queue_scroll = self.queue_scroll.saturating_sub(1);
        Some(self.begin_submission(item))
    }
    pub fn interrupt_failed(&mut self, message: &str) {
        self.interrupt_requested = false;
        self.status = if self.turn.is_some() || self.busy {
            "실행 중"
        } else {
            "대기"
        }
        .into();
        if self.force_after_interrupt.take().is_some() {
            self.queue_paused = true;
            self.error(format!("중단 실패 · 대기열 보존됨 · {message}"));
        } else {
            self.error(format!("중단 실패 · {message}"));
        }
    }
    pub fn fail_submission(&mut self, message: impl Into<String>) {
        self.interrupt_requested = false;
        if let Some(text) = self.submitted.take() {
            self.queued.push_front(QueuedMessage {
                effort: self.submitted_effort.take(),
                text,
                skills: std::mem::take(&mut self.submitted_skills),
                images: std::mem::take(&mut self.submitted_images),
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
        self.images
            .extend(std::mem::take(&mut self.submitted_images));
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
        if self.compacting {
            Some("컨텍스트 압축이 완료된 후 대화를 전환하세요.")
        } else if !self.input_forms.is_empty() {
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
        if let Some(result) = self.management_command(name, argument) {
            return self.finish_command(result);
        }
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
            "/ask" => {
                if argument.trim().is_empty() {
                    self.error("사용법: /ask 질문 · 진행 중인 작업에 병렬로 질문합니다.");
                    None
                } else if self.thread.is_none() {
                    self.error("세션 연결이 준비되지 않았습니다.");
                    None
                } else {
                    self.editor.take_text();
                    Some(Intent::Inquiry(argument.trim().into()))
                }
            }
            "/plan" => {
                self.error("사용법: /plan 목표 · 계획과 에이전트 실행을 시작합니다.");
                None
            }
            "/explore" => {
                if !self.preferences.experimental_explore {
                    self.error("로컬 코드 탐색은 설정 > 고급 > 실험 기능에서 켜세요.");
                    return None;
                }
                if argument.is_empty() {
                    self.error("사용법: /explore SYMBOL 또는 검색어");
                    None
                } else {
                    Some(Intent::Explore(argument.into()))
                }
            }
            "/settings" => {
                if let Some(value) = argument.strip_prefix("accent ") {
                    if Accent::parse(value).is_some() {
                        let intent = self.activate(Action::Setting(format!("accent:{value}")));
                        self.settings_page = settings::Page::Accent;
                        self.switch_view(View::Settings);
                        return self.finish_command(intent);
                    }
                    self.error("사용법: /settings accent cyan|white|hacker|purple|amber");
                    return None;
                }
                if matches!(argument, "korean" | "english" | "panel") {
                    let intent = self.activate(Action::Setting(argument.into()));
                    // The panel setting changes the actual panel state.
                    // Opening settings unconditionally here used to undo it.
                    if argument == "panel" && !self.panel_open {
                        self.switch_view(View::Chat);
                    } else {
                        self.panel_open = true;
                        self.settings_page = settings::Page::Main;
                        self.switch_view(View::Settings);
                        if argument == "panel" {
                            let rows = self.settings_rows(32);
                            self.scroll = rows.iter().position(|(_, title_tone, action)| {
                                *title_tone == Tone::Normal && matches!(action, Some(Action::SettingChoices(choices)) if choices.iter().any(|(key,_,_)| key.starts_with("set:panel_open:")))
                            }).unwrap_or(0).saturating_sub(2);
                        }
                    }
                    return self.finish_command(intent);
                }
                if let Some((key, value)) = argument.split_once(' ') {
                    let intent = self.setting_action(&format!("set:{key}:{value}"));
                    if intent.is_some() {
                        self.settings_page = settings::Page::Main;
                        self.switch_view(View::Settings);
                        return self.finish_command(intent);
                    }
                }
                if !argument.is_empty() {
                    self.error("사용법: /settings [korean|english|panel|accent cyan|white|hacker|purple|amber]");
                }
                self.settings_page = settings::Page::Main;
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
            "/provider" | "/providers" => self.provider_command(argument),
            "/routing" => {
                self.refresh_providers(); self.switch_view(View::Routing); self.panel_open = true;
                None
            }
            "/runtime" => self.runtime_command(argument),
            "/environment" | "/model" => {
                self.panel_open = true;
                self.switch_view(View::Models);
                if let Some(query) = argument.strip_prefix("search ") {
                    self.model_search = Editor::default();
                    self.paste_model_search(query);
                } else if !argument.is_empty() {
                    self.activate(Action::Model(argument.into()));
                } else {
                    self.reset_model_search();
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
            "/panel" => self.activate(Action::Panel),
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
                            let intent = self
                                .copy_turn_text(&id)
                                .map(Intent::Copy)
                                .or_else(|| self.activate(Action::Copy(id)));
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
                                if !self.tool_is_expanded(&id) {
                                    self.activate(Action::Toggle(id.clone()));
                                }
                                self.activate(Action::OutputPage(id, true));
                            }
                            Some("prev") => {
                                if !self.tool_is_expanded(&id) {
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
                // This command targets the controls below the searchable catalogue.
                self.scroll = usize::MAX;
                self.follow = false;
                None
            }
            "/usage" if argument.starts_with("model ") => {
                // a model bar was clicked: open its details, or close them when already open;
                // the panel keeps its scroll position so the bar stays under the pointer
                let key = argument.trim_start_matches("model ");
                self.usage_model =
                    (self.usage_model.as_deref() != Some(key)).then(|| key.to_owned());
                None
            }
            "/usage" => {
                self.activate(Action::Agents);
                self.usage_expanded = true;
                self.reload_model_ledger();
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
            "/work" | "/agents" => {
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
            "/sessions" if argument.starts_with("delete") => {
                let number = argument["delete".len()..].trim();
                let id = command_index(number).and_then(|index| {
                    self.sessions
                        .iter()
                        .filter(|session| self.thread.as_deref() != Some(session.id.as_str()))
                        .nth(index)
                        .map(|session| session.id.clone())
                });
                return match id {
                    Some(id) if !self.readonly => self.finish_command(Some(Intent::DeleteSession(id))),
                    _ => {
                        self.error("삭제할 대화 번호가 없습니다. /sessions 목록의 번호를 쓰세요 (현재 대화는 삭제할 수 없음).");
                        self.finish_command(None)
                    }
                };
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
                    "pause" => {
                        self.queue_paused = true;
                        self.notify("대기열을 일시정지했습니다. 현재 응답은 계속됩니다.");
                    }
                    "resume" => {
                        let intent = self.activate(Action::ResumeQueue);
                        return self.finish_command(intent);
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
                    _ => self.error("사용법: /queue [pause|resume|drop N|force N|clear]"),
                }
                None
            }
            "/quit" => Some(Intent::Quit),
            _ => {
                if let Some(skill) = self.skills.iter().find(|skill| skill.name.eq_ignore_ascii_case(name.trim_start_matches('/'))) {
                    if self.readonly {
                        self.error("열람 전용 창입니다. 메인 창에서 실행하세요.");
                        return None;
                    }
                    let prompt = format!("${}{}{}", skill.name, if argument.is_empty() { "" } else { " " }, argument);
                    self.editor = Editor::default();
                    self.editor.insert(&prompt);
                    return self.activate(Action::Send);
                }
                self.error("알 수 없는 명령입니다. /help 또는 /를 입력하세요.");
                return None;
            }
        };
        self.finish_command(intent)
    }
    fn finish_command(&mut self, intent: Option<Intent>) -> Option<Intent> {
        if matches!(intent, Some(Intent::Explore(_))) {
            self.welcome_dismissed = true;
        }
        if self.editor.text.starts_with('/') {
            self.editor = Editor::default();
        }
        if let Some(draft) = self.model_command_draft.take() {
            self.editor = draft;
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
        if let Some(range) = (!self.menu_open).then(|| loom_core::instructions::skill_mention_at(&self.editor.text, self.editor.cursor)).flatten() {
            let filter = self.editor.text[range.start + 1..self.editor.cursor].to_lowercase();
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
                .filter(|(name, description)| {
                    if self.menu_open {
                        let searchable = format!("{name} {description} {}", self.label(description)).to_lowercase();
                        self.command_search.text.split_whitespace().all(|word| searchable.contains(&word.to_lowercase()))
                    } else {
                        name.starts_with(text)
                    }
                })
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
    fn scroll_queue(&mut self, down: bool) {
        self.queue_scroll = if down {
            self.queue_scroll
                .saturating_add(1)
                .min(self.queue_scroll_limit)
        } else {
            self.queue_scroll.saturating_sub(1)
        };
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
    pub(super) fn scroll_pane(&mut self, x: u16, width: u16, down: bool, step: usize) {
        // The conversation and task panes keep independent positions.
        let chat_pane = self.wide_layout && x < width - self.side_width(width) - 1;
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
    fn side_width(&self, width: u16) -> u16 {
        if self.view == View::Diff {
            (width / 2).clamp(48, 72)
        } else {
            (width / 3).clamp(36, 46)
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
    fn arrow_focus(&mut self, code: KeyCode, canvas: &Canvas) -> bool {
        let down = code == KeyCode::Down;
        if matches!(code, KeyCode::Up | KeyCode::Down) && self.view != View::Chat {
            let width = if self.wide_layout {
                self.side_width(canvas.width)
            } else {
                canvas.width
            };
            let rows = self.content_rows_for(&self.view, usize::from(width - 4));
            let mut seen = Vec::new();
            let mut actions = Vec::new();
            for (row, (_, _, action)) in rows.iter().enumerate() {
                let Some(action) = action else { continue };
                let candidates = match action {
                    Action::SettingChoices(choices) => choices
                        .iter()
                        .map(|(key, _, _)| Action::Setting(key.clone()))
                        .collect(),
                    _ => vec![action.clone()],
                };
                for candidate in candidates {
                    if !matches!(
                        candidate,
                        Action::TranscriptText
                            | Action::SelectEntry(_)
                            | Action::MessageRow(_, _, _)
                    ) && !seen.contains(&candidate)
                    {
                        seen.push(candidate.clone());
                        actions.push((row, candidate));
                    }
                }
            }
            if !actions.is_empty() {
                let current = actions
                    .iter()
                    .position(|(_, action)| self.focus.as_ref() == Some(action));
                let index = match (current, down) {
                    (Some(i), true) => (i + 1) % actions.len(),
                    (Some(i), false) => (i + actions.len() - 1) % actions.len(),
                    (None, true) => 0,
                    (None, false) => actions.len() - 1,
                };
                let (row, action) = &actions[index];
                let visible_rows = rows.len().saturating_sub(self.scroll_limit).max(1);
                if *row < self.scroll {
                    self.scroll = *row;
                } else if *row >= self.scroll + visible_rows {
                    self.scroll = *row + 1 - visible_rows;
                }
                self.follow = false;
                self.composer_active = false;
                self.focus = Some(action.clone());
                return true;
            }
        }
        let Some(current) = self
            .focus
            .as_ref()
            .and_then(|action| canvas.hits.iter().find(|hit| &hit.action == action))
        else {
            return false;
        };
        let candidates: Vec<_> = canvas
            .hits
            .iter()
            .filter(|hit| {
                hit.action != current.action
                    && !matches!(
                        hit.action,
                        Action::TranscriptText
                            | Action::SelectEntry(_)
                            | Action::MessageRow(_, _, _)
                    )
            })
            .collect();
        let target = if matches!(code, KeyCode::Left | KeyCode::Right) {
            let mut row: Vec<_> = candidates
                .iter()
                .copied()
                .filter(|hit| hit.y == current.y)
                .collect();
            row.sort_by_key(|hit| hit.x);
            if code == KeyCode::Right {
                row.iter()
                    .copied()
                    .find(|hit| hit.x > current.x)
                    .or_else(|| row.first().copied())
            } else {
                row.iter()
                    .rev()
                    .copied()
                    .find(|hit| hit.x < current.x)
                    .or_else(|| row.last().copied())
            }
        } else {
            candidates
                .iter()
                .copied()
                .filter(|hit| {
                    if down {
                        hit.y > current.y
                    } else {
                        hit.y < current.y
                    }
                })
                .min_by_key(|hit| (hit.y.abs_diff(current.y), hit.x.abs_diff(current.x)))
        };
        if let Some(hit) = target {
            self.focus = Some(hit.action.clone());
            self.composer_active = false;
            true
        } else {
            false
        }
    }
    pub fn key(&mut self, key: KeyEvent, canvas: &Canvas) -> Option<Intent> {
        if !matches!(key.code, KeyCode::Char(_) | KeyCode::Enter) {
            self.image_drop = None;
        }
        // Delete on a focused saved conversation arms; a second Delete on the same row
        // deletes. Any other key or row disarms, so one stray key never loses history.
        let delete_key = matches!(key.code, KeyCode::Delete | KeyCode::Backspace)
            && key.modifiers.is_empty();
        let armed = self.session_delete_armed.take();
        if delete_key && !self.readonly {
            if let Some(Action::Resume(id)) = self.focus.clone() {
                if armed.as_deref() == Some(id.as_str()) {
                    return Some(Intent::DeleteSession(id));
                }
                self.session_delete_armed = Some(id);
                return None;
            }
        }
        if key.modifiers.contains(KeyModifiers::CONTROL)
            && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        {
            if let Some(text) = self.selected_mouse_text() {
                return Some(Intent::Copy(text));
            }
            if let Some(range) = self.editor.selection() {
                return Some(Intent::Copy(self.editor.text[range].into()));
            }
        }
        // Enhanced keyboard protocols may preserve Shift/Caps state and report
        // Ctrl+Q as `Char('Q')` instead of `Char('q')`. Treat the shortcut as a
        // physical control chord, not as an exact lowercase character. Keep it
        // global so blocking approval/question UI cannot accidentally swallow exit.
        if ctrl_char(key, 'q') {
            return Some(Intent::Quit);
        }
        if key.code == KeyCode::Esc
            && let Some(view) = self.remote.as_mut().filter(|v| v.shown)
        {
            view.shown = false;
            return None;
        }
        // Escape takes priority over menus, selections and blocking UI while
        // a turn is active. Submit can precede assignment of the turn ID. On another
        // screen (models, settings, ...) Escape first returns to the conversation.
        if key.code == KeyCode::Esc
            && !self.readonly
            && self.view == View::Chat
            && (self.turn.is_some()
                || self.submitted.is_some()
                || (self.busy && self.status == "inProgress"))
        {
            self.escape_pressed = None;
            return self.activate(Action::Interrupt);
        }
        self.reset_mouse();
        if self.trust_prompt.is_some() || self.approval_visible() || self.paste_pending {
            let actions = self.decision_actions();
            let selected = self
                .focus
                .as_ref()
                .and_then(|f| actions.iter().position(|a| a == f))
                .unwrap_or(0);
            match key.code {
                KeyCode::Esc if self.trust_prompt.is_some() => return Some(Intent::Quit),
                KeyCode::Esc if self.paste_pending && !self.approval_visible() => {
                    return self.activate(Action::Setting("paste:cancel".into()));
                }
                KeyCode::Esc => return self.activate(Action::DismissApproval),
                KeyCode::Up | KeyCode::BackTab => {
                    self.focus = actions
                        .get((selected + actions.len() - 1) % actions.len())
                        .cloned()
                }
                KeyCode::Down | KeyCode::Tab => {
                    self.focus = actions.get((selected + 1) % actions.len()).cloned()
                }
                KeyCode::Enter => {
                    return actions
                        .get(selected)
                        .cloned()
                        .and_then(|a| self.activate(a));
                }
                KeyCode::Char(c)
                    if c.is_ascii_digit()
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    if let Some(action) = c
                        .to_digit(10)
                        .and_then(|n| n.checked_sub(1))
                        .and_then(|n| actions.get(n as usize))
                        .cloned()
                    {
                        return self.activate(action);
                    }
                }
                _ => {}
            }
            return None;
        }
        if self.question_visible() && self.approvals.is_empty() {
            return self.question_key(key);
        }
        if ctrl_char(key, 'p') && !self.readonly {
            return self.activate(Action::Menu);
        }
        if self.menu_open {
            return self.command_palette_key(key);
        }
        if key.code == KeyCode::Esc && self.model_command_draft.is_some() {
            self.editor = self.model_command_draft.take().unwrap();
            self.composer_active = false;
            self.menu_open = false;
            self.menu_dismissed = true;
            self.focus = None;
            return None;
        }
        if self.model_search_key(key) { return None; }
        if self.preferences.experimental_rewind
            && key.code == KeyCode::Esc
            && key.modifiers.is_empty()
            && !self.readonly
            && self.view == View::Chat
            && self.session_switch_block_reason().is_none()
        {
            let now = Instant::now();
            if self
                .escape_pressed
                .take()
                .is_some_and(|last| now.duration_since(last) < Duration::from_millis(600))
            {
                self.rewind_confirm = None;
                self.switch_view(View::Rewind);
                self.composer_active = false;
                self.focus = None;
                return None;
            }
            self.escape_pressed = Some(now);
        } else if key.code != KeyCode::Esc {
            self.escape_pressed = None;
        }
        if key.code == KeyCode::Esc && self.view == View::Rewind {
            self.rewind_confirm = None;
            self.switch_view(View::Chat);
            self.focus = None;
            return None;
        }
        if ctrl_char(key, 'n') && !self.queued.is_empty() {
            return self.activate(Action::ForceQueued(0));
        }
        let menu = self.menu_items();
        if !menu.is_empty() && self.focus.is_none() && self.history_position.is_none() {
            match key.code {
                KeyCode::Up | KeyCode::BackTab => {
                    self.menu_index =
                        (self.menu_index.min(menu.len() - 1) + menu.len() - 1) % menu.len();
                    return None;
                }
                KeyCode::Down => {
                    self.menu_index = (self.menu_index + 1) % menu.len();
                    return None;
                }
                // Tab completes the command into the composer; Enter runs it, except for
                // commands that need an argument, which Enter completes the same way.
                KeyCode::Tab | KeyCode::Enter => {
                    let action = menu[self.menu_index.min(menu.len() - 1)].2.clone();
                    if let Action::Command(command) = action {
                        self.menu_open = false;
                        self.menu_dismissed = true;
                        self.menu_index = 0;
                        self.editor = Editor::default();
                        let needs_argument = COMMANDS_WITH_ARGUMENT.contains(&command.as_str());
                        if key.code == KeyCode::Enter && !needs_argument {
                            return self.activate(Action::Command(command));
                        }
                        self.editor.insert(&if needs_argument { format!("{command} ") } else { command });
                        self.composer_active = true;
                        return None;
                    }
                    return self.activate(action);
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
                    if !self.readonly {
                        self.activate(Action::Input);
                        self.editor.select_all();
                    }
                    return None;
                }
                KeyCode::Char('o') if !self.readonly => {
                    self.activate(Action::Input);
                    self.editor.expand_pastes();
                    return None;
                }
                KeyCode::Char('e') => {
                    self.editor.anchor = None;
                    self.editor.cursor = self.editor.text.len();
                    return None;
                }
                _ => {}
            }
        }
        // Typing resumes the composer after keyboard navigation, including on
        // compact screens where the selected tab occupies the main pane.
        let compact_detail_needs_chat = self.view != View::Chat
            && !self.wide_layout
            && !(self.view == View::Diff && self.composer_active);
        if !self.readonly
            && menu.is_empty()
            && matches!(key.code, KeyCode::Char(_))
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && (self.focus.is_some() || compact_detail_needs_chat)
        {
            if compact_detail_needs_chat {
                self.switch_view(View::Chat);
            }
            self.focus = None;
            self.composer_active = true;
        }
        if matches!(
            key.code,
            KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down
        ) && !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT)
            && (self.focus.is_some() || (self.view != View::Chat && !self.composer_active))
            && self.arrow_focus(key.code, canvas)
        {
            return None;
        }
        match key.code {
            KeyCode::Esc => {
                if self.view == View::Settings && self.settings_page != settings::Page::Main {
                    return self.setting_action("page:main");
                }
                if self.editor.anchor.take().is_some() {
                    return None;
                }
                if !self.readonly && self.turn.is_some() && self.view == View::Chat {
                    return self.activate(Action::Interrupt);
                }
                if self.branch_entry.take().is_some() {
                    return None;
                }
                if self.composer_active && self.view != View::Chat {
                    self.composer_active = false;
                    return None;
                }
                if self.view != View::Chat {
                    self.activate(Action::Back);
                    self.focus = None;
                } else if self.focus.is_some() {
                    self.focus = None;
                } else {
                    self.focus = canvas
                        .hits
                        .iter()
                        .find(|h| {
                            !matches!(
                                h.action,
                                Action::TranscriptText
                                    | Action::SelectEntry(_)
                                    | Action::MessageRow(_, _, _)
                            )
                        })
                        .map(|h| h.action.clone());
                }
            }
            KeyCode::Tab | KeyCode::BackTab => {
                self.composer_active = false;
                let hits: Vec<_> = canvas
                    .hits
                    .iter()
                    .filter(|h| {
                        !matches!(
                            h.action,
                            Action::TranscriptText
                                | Action::SelectEntry(_)
                                | Action::MessageRow(_, _, _)
                        )
                    })
                    .collect();
                let count = hits.len();
                if count > 0 {
                    let current = self
                        .focus
                        .as_ref()
                        .and_then(|f| hits.iter().position(|h| &h.action == f));
                    let index = if key.code == KeyCode::BackTab {
                        current.map_or(count - 1, |i| (i + count - 1) % count)
                    } else {
                        current.map_or(0, |i| (i + 1) % count)
                    };
                    self.focus = Some(hits[index].action.clone());
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
                let action = self.focus.clone().unwrap();
                let nav = canvas
                    .hits
                    .iter()
                    .any(|hit| hit.y == 0 && hit.action == action);
                let intent = self.activate(action.clone());
                if nav && self.focus.is_none() {
                    self.focus = Some(action);
                }
                return intent;
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
                if matches!(
                    key.code,
                    KeyCode::Left
                        | KeyCode::Right
                        | KeyCode::Up
                        | KeyCode::Down
                        | KeyCode::Home
                        | KeyCode::End
                ) {
                    if !key.modifiers.contains(KeyModifiers::SHIFT)
                        && matches!(key.code, KeyCode::Left | KeyCode::Right)
                    {
                        if let Some(range) = self.editor.selection() {
                            self.editor.cursor = if key.code == KeyCode::Left {
                                range.start
                            } else {
                                range.end
                            };
                            self.editor.anchor = None;
                            return None;
                        }
                    }
                    if key.modifiers.contains(KeyModifiers::SHIFT) {
                        self.editor.anchor.get_or_insert(self.editor.cursor);
                    } else {
                        self.editor.anchor = None;
                    }
                }
                match key.code {
                    KeyCode::Enter
                        if key
                            .modifiers
                            .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT) =>
                    {
                        self.history_position = None;
                        self.editor.insert("\n")
                    }
                    KeyCode::Enter => return self.activate(Action::Send),
                    KeyCode::Char(c)
                        if !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        self.history_position = None;
                        self.insert_with_image_drop(c)
                    }
                    KeyCode::Backspace => {
                        self.history_position = None;
                        self.editor.backspace();
                    }
                    KeyCode::Delete => {
                        self.history_position = None;
                        self.editor.delete();
                    }
                    KeyCode::Left => self.editor.left(),
                    KeyCode::Right => self.editor.right(),
                    KeyCode::Up | KeyCode::Down
                        if !key.modifiers.intersects(
                            KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SHIFT,
                        ) && (self.editor.text.is_empty()
                            || self.history_position.is_some()) =>
                    {
                        if key.code != KeyCode::Up || !self.recall_queued_for_edit() {
                            self.recall_input(key.code == KeyCode::Down)
                        }
                    }
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
        let mut palette_cursor = None;
        c.accent = self.preferences.accent;
        // Settings owns its side pane independently of the activity panel preference.
        self.wide_layout = width >= 110
            && (self.panel_open || self.view == View::Settings);
        if width < 40 || height < 12 {
            c.text(0, 0, "터미널을 넓혀주세요 (40 × 12 이상)", Tone::Muted);
            return c;
        }
        let wide = self.wide_layout;
        let side_width = self.side_width(width);
        let split = if wide { width - side_width - 1 } else { 0 };
        let left_width = if wide { split } else { width };
        let left_view = if wide { &View::Chat } else { &self.view };
        let mut chat_bands: Option<Rc<Vec<usize>>> = None;
        let rows = if *left_view == View::Chat {
            let rows = self.cached_chat_rows(usize::from(left_width - 4));
            chat_bands = Some(self.cached_chat_bands());
            rows
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
        let input_layout = mouse::composer_rows(&display, input_width);
        let decision_rows = self.decision_actions().len() as u16;
        let input_rows = if decision_rows > 0 {
            (decision_rows + 3).min(height.saturating_sub(9)).max(1)
        } else if let Some(rows) = self.question_input_rows(width, height) {
            rows
        } else if self.readonly {
            1
        } else {
            (input_layout.len() as u16).clamp(1, (height - 9).min(5))
        };
        let notice_rows = u16::from(!self.notice.is_empty());
        let skill_rows = u16::from(!self.selected_skills.is_empty() && !self.readonly);
        let image_rows = u16::from(!self.images.is_empty() && !self.readonly);
        let max_body =
            height.saturating_sub(5 + input_rows + notice_rows + skill_rows + image_rows);
        // Reserve a fixed strip above the composer. Queue entries must remain
        // clickable and visible while the transcript scrolls or a tab is open.
        let queue_rows = if self.readonly || self.queued.is_empty() {
            0
        } else {
            (1 + self.queued.len().min(3) as u16).min(max_body.saturating_sub(3))
        };
        if queue_rows == 0 {
            self.queue_scroll = 0;
            self.queue_scroll_limit = 0;
        }
        let body_height = max_body - queue_rows;
        // The startup logo occupies only the empty, untouched chat viewport.
        let rows = if *left_view == View::Chat
            && self.entries.is_empty()
            && !self.welcome_dismissed
            && !self.readonly
            && !self.history_pending
            && !self.busy
            && self.turn.is_none()
            && self.active_tasks.is_empty()
        {
            Rc::new(welcome_rows(
                usize::from(left_width - 4),
                usize::from(body_height),
                self.preferences.language,
            ))
        } else {
            rows
        };
        let body_top = 2;
        let body_end = body_top + body_height;
        let queue_top = body_end + notice_rows;
        let composer_top = queue_top + queue_rows;
        let composer_bottom = composer_top + input_rows + skill_rows + image_rows + 1;
        let footer = composer_bottom + 1;
        // Keep execution state and the model next to the composer.
        let mut header = Canvas::new(width - 4, 1);
        header.rule(0, "");
        let (status_icon, status_tone) = if self.is_working() {
            (progress::SPINNER[self.animation_frame], Tone::Accent)
        } else {
            status_style(&self.status)
        };
        // Automatic routing names the model that actually answered last.
        // A resolution made for an earlier selection is stale once the model changes.
        let shown = match &self.resolved_model {
            // An LMX model answers under another of its names (source path, hash); the
            // selection already names it.
            // An LMX model is marked by colour (below), leaving the narrow header to its name.
            _ if self.model.starts_with("lmx/") => self.model_display(&self.model),
            Some((requested, _, served))
                if requested == &self.model
                    && !served.is_empty()
                    && !self.model.ends_with(served.as_str())
                    && !self.model.is_empty() =>
            {
                format!("{} → {}", model_name_in_group(&self.model), self.served_display(requested, served))
            }
            _ => self.model.clone(),
        };
        // The speed is kept whole; a long model name is shortened instead.
        let speed = match &self.model_speed {
            Some((model, speed)) if model == &self.model && !shown.is_empty() => {
                format!(" · {speed:.0} tok/s")
            }
            _ => String::new(),
        };
        // A narrow header keeps the model's name and drops the speed first.
        let name_room = usize::from(width / 3).max(14);
        let speed = if width < 72 && UnicodeWidthStr::width(shown.as_str()) + UnicodeWidthStr::width(speed.as_str()) > name_room {
            String::new()
        } else {
            speed
        };
        let model_label = if shown.is_empty() {
            self.label("모델 선택").to_string()
        } else {
            format!("{}{speed}", one_line(&shown, name_room.saturating_sub(UnicodeWidthStr::width(speed.as_str())).max(8)))
        };
        let model_width = UnicodeWidthStr::width(model_label.as_str()) + 4;
        let status_phrase = self.status_phrase();
        let state_x = header.text(
            0,
            0,
            &one_line(
                &format!(
                    "{} {}",
                    status_icon,
                    if self.readonly {
                        self.label("열람 전용")
                    } else if self.approvals.iter().any(|approval| !approval.answering) {
                        self.label("승인 대기")
                    } else if self.input_forms.iter().any(|f| f.request.blocking) {
                        self.label("질문 답변 대기")
                    } else if !self.active_tasks.is_empty() && !self.busy {
                        if self.active_tasks.iter().all(|id| {
                            self.tasks.iter().any(|t| {
                                &t.id == id && t.worker == Some(crate::agent::WorkerKind::Local)
                            })
                        }) {
                            &*status_phrase
                        } else {
                            "작업 실행 중"
                        }
                    } else if self.is_working() {
                        &*status_phrase
                    } else {
                        self.label(status_label(&self.status))
                    }
                ),
                usize::from(header.width).saturating_sub(model_width),
            ),
            status_tone,
        );
        if self.is_working() {
            if let Some(log) = self
                .entries
                .iter()
                .rev()
                .find(|entry| entry.id.ends_with("-v5-log"))
            {
                header.hits.push(crate::engine::Hit {
                    x: 0,
                    y: 0,
                    width: state_x,
                    height: 1,
                    action: Action::Toggle(log.id.clone()),
                });
            }
        }
        // The model button pads itself; one separator space is enough.
        let model_x = header.text(state_x, 0, " ·", Tone::Muted);
        let model_end = if self.model.starts_with("lmx/") {
            let end = header.text(model_x, 0, &format!(" {model_label} "), Tone::Success);
            header.hits.push(crate::engine::Hit {
                x: model_x,
                y: 0,
                width: end.saturating_sub(model_x),
                height: 1,
                action: Action::Command("/model".into()),
            });
            end
        } else {
            header.button(model_x, 0, &model_label, Action::Command("/model".into()), false)
        };
        header.text(
            model_end,
            0,
            &format!(
                "{}",
                self.effort
                    .as_ref()
                    .map(|effort| format!(" · {effort}"))
                    .unwrap_or_default()
            ),
            Tone::Muted,
        );
        let mut nav = Canvas::new(width - 4, 1);
        let mut x: u16 = 0;
        let back_label = self.session_trail.last().map(|(_, title)| {
            format!(
                "← {}",
                one_line(
                    if title.is_empty() {
                        "이전 세션"
                    } else {
                        title
                    },
                    18
                )
            )
        });
        let mut tabs = self.root_tabs();
        if let Some(label) = &back_label {
            tabs.insert(0, (label.clone(), Action::ReturnSession, false));
        }
        let active_width =
            tabs.iter()
                .find(|tab| tab.2)
                .map_or(0, |tab| UnicodeWidthStr::width(tab.0.as_str()) + 5) as u16;
        let mut active_pending = true;
        for (label, action, active) in tabs {
            let label = label.as_str();
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
            if matches!(action, Action::Agents) && !self.approvals.is_empty() {
                nav.text(button_x + 1, 0, &label, Tone::Warning);
            }
            if active {
                // Active tab is a current-state indicator; only keyboard focus
                // gets the selection tone.
                nav.highlight(&action, Tone::Accent);
            }
            if self.focus.as_ref() == Some(&action) {
                nav.highlight(&action, Tone::Selected);
            }
        }
        c.blit(&nav, 2, 0);
        c.rule(1, "");
        if let (Some(sandbox), Some(approval)) = (&self.sandbox_mode, &self.approval_mode) {
            let english = self.preferences.language == Language::English;
            let sandbox_label = match sandbox.as_str() {
                "readOnly" => {
                    if english {
                        "Read-only"
                    } else {
                        "읽기 전용"
                    }
                }
                "workspaceWrite" => {
                    if english {
                        "Workspace write"
                    } else {
                        "작업 폴더 쓰기"
                    }
                }
                "dangerFullAccess" => {
                    if english {
                        "Full access"
                    } else {
                        "전체 접근"
                    }
                }
                other => other,
            };
            let approval_label = match approval.as_str() {
                "never" => {
                    if english {
                        "Never ask"
                    } else {
                        "승인 없음"
                    }
                }
                "on-request" => {
                    if english {
                        "Ask when needed"
                    } else {
                        "필요시 승인"
                    }
                }
                "untrusted" => {
                    if english {
                        "Conservative approval"
                    } else {
                        "보수적 승인"
                    }
                }
                "granular" => {
                    if english {
                        "Granular approval rules"
                    } else {
                        "세부 승인 규칙"
                    }
                }
                other => other,
            };
            let dangerous = sandbox == "dangerFullAccess" && approval == "never";
            c.text(
                2,
                1,
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
        if (!self.diff.is_empty()
            || self.entries.iter().any(|entry| {
                entry.kind == Kind::Change
                    && !entry.body.is_empty()
                    && self.recent_change_ids.contains(&entry.id)
            }))
            && self.view == View::Chat
            && width >= 72
        {
            let label = if self.has_reported_changes() {
                format!("파일 변경 {} ▸", self.reported_change_label())
            } else {
                "▤".into()
            };
            // Reserve space for the separate panel toggle on full-width chat.
            // Don't overwrite the permission status on the left of this row.
            if !wide {
                let panel_space = if width >= 110 { 16 } else { 0 };
                let label_width = unicode_width::UnicodeWidthStr::width(label.as_str()) as u16 + 2;
                let x = width.saturating_sub(panel_space + label_width + 2);
                let row_start = usize::from(width);
                let status_end = c.cells[row_start..row_start + usize::from(width)]
                    .iter()
                    .rposition(|cell| cell.tone != Tone::Faint)
                    .unwrap_or(0) as u16;
                if x > status_end.saturating_add(2) {
                    c.button(x, 1, &label, Action::Diff, false);
                }
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
                    chat_bands.as_deref().map(Vec::as_slice),
                    self.is_working()
                        .then_some((&self.entries, self.animation_frame)),
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
                    chat_bands.as_deref().map(Vec::as_slice),
                    self.is_working()
                        .then_some((&self.entries, self.animation_frame)),
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
                    None,
                    None,
                ),
                split + 1,
                body_top,
            );
            for y in 1..body_end {
                c.text(split, y, "│", Tone::Faint);
            }
            c.text(split, 1, "┬", Tone::Faint);
            c.text(
                split + 2,
                1,
                &format!(
                    " {} ",
                    if self.view == View::Chat {
                        self.label("현황")
                    } else {
                        self.view_title()
                    }
                ),
                Tone::Accent,
            );
            c.button(width - 9, 1, self.label("접기"), Action::Panel, false);
        } else if width >= 110 {
            c.button(width - 14, 1, self.label("패널 열기"), Action::Panel, false);
        }
        if !menu.is_empty() || self.menu_open {
            let header_rows = if self.menu_open { 2 } else { 1 };
            let count = menu
                .len()
                .min(usize::from(body_height.saturating_sub(header_rows)))
                .min(5);
            let index = self.menu_index.min(menu.len().saturating_sub(1));
            let start = index.saturating_sub(count.saturating_sub(1));
            let mut popup = Canvas::new(left_width, (count.max(1) as u16 + header_rows).min(body_height));
            popup.rule(
                0,
                if self.menu_open {
                    if self.view == View::Chat && (self.turn.is_some() || self.submitted.is_some() || (self.busy && self.status == "inProgress")) {
                        if self.preferences.language == Language::English { "Commands · type to search · Esc stop" } else { "명령 검색 · 입력해서 찾기 · Esc 중단" }
                    } else if self.preferences.language == Language::English { "Commands · type to search · Esc close" } else { "명령 검색 · 입력해서 찾기 · Esc 닫기" }
                } else if loom_core::instructions::skill_mention_at(&self.editor.text, self.editor.cursor).is_some() {
                    "스킬 · ↑↓ 선택 · Enter 추가"
                } else {
                    "명령 · ↑↓ 선택 · Enter 입력"
                },
            );
            if self.menu_open {
                let (query, column) = self.command_search_field(usize::from(left_width));
                popup.text(2, 1, &format!("> {query}"), Tone::Accent);
                palette_cursor = Some((4 + column as u16, body_end - popup.height + 1));
                if menu.is_empty() {
                    popup.text(2, 2, if self.preferences.language == Language::English { "No matching commands" } else { "일치하는 명령이 없습니다" }, Tone::Muted);
                }
            }
            for (row, (label, desc, action)) in menu.iter().skip(start).take(count).enumerate() {
                let x = popup.button(2, row as u16 + header_rows, label, action.clone(), false);
                popup.text(x, row as u16 + header_rows, self.label(desc), Tone::Muted);
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
            self.queue_scroll_limit = self.queued.len().saturating_sub(visible);
            self.queue_scroll = self.queue_scroll.min(self.queue_scroll_limit);
            let queue_start = self.queue_scroll;
            let queue_end = (queue_start + visible).min(self.queued.len());
            let compact_queue = width < 65;
            let header = format!(
                "≡ 대기열 · {}개{}{}",
                self.queued.len(),
                if self.queue_scroll_limit > 0 {
                    if compact_queue {
                        format!(" · {}-{}/{}", queue_start + 1, queue_end, self.queued.len())
                    } else {
                        format!(
                            " · {}-{}/{} · 휠로 이동",
                            queue_start + 1,
                            queue_end,
                            self.queued.len()
                        )
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
            c.hits.push(crate::engine::Hit {
                x: 0,
                y: queue_top,
                width,
                height: queue_rows,
                action: Action::QueueViewport,
            });
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
            for (row, (index, item)) in self
                .queued
                .iter()
                .enumerate()
                .skip(queue_start)
                .take(visible)
                .enumerate()
            {
                let y = queue_top + 1 + row as u16;
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
                            &format!(
                                "{}{}",
                                item.text.lines().next().unwrap_or(""),
                                if item.images.is_empty() {
                                    String::new()
                                } else {
                                    format!(" · 이미지 {}개", item.images.len())
                                }
                            ),
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
        c.rule(composer_top, "");
        c.blit(&header, 2, composer_top);
        if self.busy && !self.readonly {
            c.button(
                width.saturating_sub(8),
                composer_top,
                "■",
                Action::Interrupt,
                false,
            );
        }
        if self.recovery_available && !self.is_working() && !self.readonly {
            let (space, label) = if width < 65 { (14, "재시도") } else { (28, "이어서 시도") };
            c.button(width.saturating_sub(space), composer_top, label, Action::Command("/retry".into()), false);
        }
        let mut chip_x = 2;
        for skill in &self.selected_skills {
            chip_x = c.button(
                chip_x,
                composer_top + 1,
                &format!("${} ×", skill.name),
                Action::RemoveSkill(skill.path.clone()),
                false,
            );
        }
        self.paint_images(&mut c, composer_top + skill_rows + 1);
        let input_y = composer_top + skill_rows + image_rows + 1;
        if self.readonly {
            c.text(
                2,
                input_y,
                self.label("실행과 입력은 메인 창에서"),
                Tone::Muted,
            );
        } else {
            let (row, col) = input_layout
                .iter()
                .enumerate()
                .rev()
                .find_map(|(r, line)| {
                    line.offsets
                        .iter()
                        .rposition(|at| *at == visible_cursor)
                        .map(|col| (r, col))
                })
                .unwrap_or((0, 0));
            let start = row.saturating_sub(usize::from(input_rows) - 1);
            let selection = self
                .editor
                .selection()
                .map(|r| self.editor.visible_at_raw(r.start)..self.editor.visible_at_raw(r.end));
            for offset in 0..input_rows {
                let y = input_y + offset;
                let line_index = start + usize::from(offset);
                let Some(line) = input_layout.get(line_index) else {
                    continue;
                };
                c.text(2, y, if offset == 0 { "›" } else { " " }, Tone::Accent);
                if self.editor.text.is_empty() && offset == 0 {
                    c.text(4, y, self.label("메시지 입력…"), Tone::Muted);
                } else {
                    c.text(4, y, &line.text, Tone::Normal);
                }
                let end = *line.offsets.last().unwrap();
                for x in 1..width - 1 {
                    let column = usize::from(x.saturating_sub(4));
                    let visible = line.offsets.get(column).copied().unwrap_or(end);
                    let i = usize::from(y) * usize::from(width) + usize::from(x);
                    c.input_offsets[i] = Some(self.editor.raw_at_visible(visible));
                    if x >= 4 && column < line.offsets.len().saturating_sub(1) {
                        if let Some(range) = &selection {
                            let next = line.offsets[column + 1];
                            if visible < range.end && next > range.start {
                                c.cells[i].background = crate::engine::Background::Selected;
                                if x + 1 < width && c.cells[i + 1].continuation {
                                    c.cells[i + 1].background = crate::engine::Background::Selected;
                                }
                            }
                        }
                    }
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
        self.paint_meter(&mut c, footer);
        if !self.input_forms.is_empty() {
            c.button(
                2,
                composer_top,
                &format!("? 질문 {} · 열기", self.input_forms.len()),
                Action::Questions,
                false,
            );
        }
        self.paint_model_search(&mut c);
        if self.trust_prompt.is_none() && self.approvals.is_empty() {
            self.paint_questions(&mut c, composer_top, composer_bottom);
        }
        self.paint_decision_composer(&mut c, composer_top, composer_bottom);
        // Composer overlays replace cells and hit regions; highlight their final controls.
        if let Some(action) = &self.hovered
            && !matches!(
                action,
                Action::TranscriptText
                    | Action::SelectEntry(_)
                    | Action::MessageRow(_, _, _)
                    | Action::Input
            )
        {
            c.highlight(action, Tone::Hover);
        }
        if let Some(action) = &self.focus
            && !matches!(
                action,
                Action::TranscriptText
                    | Action::SelectEntry(_)
                    | Action::MessageRow(_, _, _)
                    | Action::Input
            )
        {
            c.highlight(action, Tone::Selected);
        }
        self.paint_mouse_selection(&mut c);
        if self.menu_open && self.trust_prompt.is_none() && !self.approval_visible() && !self.question_visible() {
            c.cursor = palette_cursor;
        }
        self.paint_remote(&mut c);
        c
    }
    fn approval_visible(&self) -> bool {
        !self.readonly
            && !self.approval_popup_dismissed
            && self.approvals.iter().any(|a| !a.answering)
    }
    fn decision_actions(&self) -> Vec<Action> {
        if self.trust_prompt.is_some() {
            vec![Action::TrustWorkspace(true), Action::TrustWorkspace(false)]
        } else if self.approval_visible() {
            let approval = self.approvals.iter().find(|a| !a.answering).unwrap();
            (0..approval.choices.len())
                .map(|i| Action::Decide(approval.id.clone(), i))
                .chain([Action::Approvals, Action::DismissApproval])
                .collect()
        } else if self.paste_pending {
            vec![
                Action::Setting("paste:confirm".into()),
                Action::Setting("paste:cancel".into()),
            ]
        } else {
            Vec::new()
        }
    }
    fn paint_decision_composer(&self, canvas: &mut Canvas, top: u16, bottom: u16) {
        let actions = self.decision_actions();
        if actions.is_empty() {
            return;
        }
        canvas.cursor = None;
        canvas.hits.retain(|hit| hit.y < top);
        for y in top..canvas.height {
            for x in 0..canvas.width {
                canvas.text(x, y, " ", Tone::Normal);
            }
        }
        let (title, summary, labels) = if let Some(path) = &self.trust_prompt {
            (
                "! 작업 폴더 신뢰 확인".to_owned(),
                path.clone(),
                vec!["신뢰하고 시작".to_owned(), "제한 모드로 시작".to_owned()],
            )
        } else if self.paste_pending && !self.approval_visible() {
            let english = self.preferences.language == Language::English;
            (
                if english {
                    "Confirm pasted input"
                } else {
                    "붙여넣은 내용을 전송할까요?"
                }
                .into(),
                self.editor.text.clone(),
                vec![
                    if english { "Send" } else { "전송" }.into(),
                    if english { "Cancel" } else { "취소" }.into(),
                ],
            )
        } else {
            let approval = self.approvals.iter().find(|a| !a.answering).unwrap();
            (
                format!("! {} · 승인 필요", approval.title),
                approval.summary.clone(),
                approval
                    .choices
                    .iter()
                    .map(|c| c.label.clone())
                    .chain(["상세 보기".into(), "나중에".into()])
                    .collect(),
            )
        };
        canvas.rule(top, &title);
        canvas.text(
            2,
            top + 1,
            &one_line(&summary, usize::from(canvas.width.saturating_sub(4))),
            Tone::Normal,
        );
        let selected = self
            .focus
            .as_ref()
            .and_then(|f| actions.iter().position(|a| a == f))
            .unwrap_or(0);
        let slots = usize::from(bottom.saturating_sub(top + 2)).max(1);
        let start = selected.saturating_sub(slots - 1);
        for (offset, index) in (start..actions.len()).take(slots).enumerate() {
            canvas.button(
                2,
                top + 2 + offset as u16,
                &format!("{} {}", index + 1, labels[index]),
                actions[index].clone(),
                index == selected,
            );
        }
        canvas.rule(bottom, "");
        canvas.text(
            2,
            bottom + 1,
            "↑↓ / Tab 선택 · Enter 실행 · 숫자 선택 · Esc 닫기",
            Tone::Muted,
        );
    }
    fn view_title(&self) -> &'static str {
        match self.view {
            View::Chat => "대화",
            View::Agents if self.usage_expanded => "사용량",
            View::Agents => "작업 목록",
            View::Diff => "변경 검토",
            View::Approvals => "승인 요청",
            View::Permissions => "권한 관리",
            View::Rewind => "되돌리기",
            View::Help => "도움말",
            View::Skills => "스킬 선택",
            View::Models => "모델 선택",
            View::Providers => "공급자 연결",
            View::Routing => "자동 라우팅 설정",
            View::Runtime => "로컬 추론 엔진",
            View::Sessions => "세션 관리",
            View::Settings => "설정",
        }
    }
    fn overview_rows(&self, width: usize) -> Vec<(String, Tone, Option<Action>)> {
        let mut rows = Vec::new();
        panel_section(&mut rows, "현재 상태", width);
        let (symbol, tone) = if self.compacting {
            ("◉ 압축 중", Tone::Accent)
        } else if self.is_working() {
            ("◉ 작업 중", Tone::Accent)
        } else if self.queue_paused {
            ("Ⅱ 대기열 일시정지", Tone::Warning)
        } else {
            ("✓ 대기 중", Tone::Success)
        };
        rows.push((format!("  {symbol}"), tone, None));
        rows.extend(self.meter_summary_rows(width));
        if !self.queued.is_empty() {
            rows.push((
                format!("  다음 지시 {}개 대기", self.queued.len()),
                Tone::Muted,
                None,
            ));
        }
        if let Some(tool) = self.active_tool_entry() {
            rows.push((
                "실행 기록 보기 ▸".into(),
                Tone::Accent,
                Some(Action::Toggle(tool.id.clone())),
            ));
        }
        if !self.approvals.is_empty() || self.has_reported_changes() {
            panel_section(&mut rows, "확인할 항목", width);
        }
        if !self.approvals.is_empty() {
            rows.push((
                format!("! 승인 요청 {}건 ▸", self.approvals.len()),
                Tone::Warning,
                Some(Action::Approvals),
            ));
        }
        if self.has_reported_changes() {
            rows.push((
                format!("▤ 파일 변경 {} ▸", self.reported_change_label()),
                Tone::Accent,
                Some(Action::Diff),
            ));
        }
        let running: Vec<_> = self
            .tasks
            .iter()
            .filter(|task| task.status == crate::agent::TaskStatus::Running)
            .collect();
        if !running.is_empty() {
            panel_section(&mut rows, &format!("진행 중 · {}", running.len()), width);
            for task in running.iter().take(3) {
                for line in wrap(&format!("● {} ▸", task.title), width) {
                    rows.push((line, Tone::Accent, Some(Action::Task(task.id.clone()))));
                }
            }
            if running.len() > 3 {
                rows.push((
                    format!("그 외 {}개 · 작업 목록 ▸", running.len() - 3),
                    Tone::Muted,
                    Some(Action::Agents),
                ));
            }
        }
        if self.tasks.is_empty() && !self.agents.is_empty() {
            panel_section(
                &mut rows,
                &format!("하위 에이전트 · {}", self.agents.len()),
                width,
            );
        }
        for (index, agent) in self
            .agents
            .iter()
            .take(if self.tasks.is_empty() { 3 } else { 0 })
            .enumerate()
        {
            rows.push((
                format!("{} · {} ▸", agent.name, status_label(&agent.status)),
                Tone::Normal,
                Some(Action::Agents),
            ));
            rows.push((
                "  새 터미널에서 열기 ↗".into(),
                Tone::Muted,
                Some(Action::OpenAgent(index)),
            ));
        }
        if self.tasks.is_empty() && self.agents.len() > 3 {
            rows.push((
                format!("에이전트 {}명 모두 보기 ▸", self.agents.len()),
                Tone::Muted,
                Some(Action::Agents),
            ));
        }
        panel_section(&mut rows, "바로가기", width);
        rows.push((
            "작업 목록 보기 ▸".into(),
            Tone::Accent,
            Some(Action::Agents),
        ));
        rows
    }
    pub(crate) fn content_rows_for(
        &self,
        view: &View,
        width: usize,
    ) -> Vec<(String, Tone, Option<Action>)> {
        let mut rows = if *view == View::Chat {
            Vec::new()
        } else {
            self.navigation_rows()
        };
        match view {
            View::Providers => rows.extend(self.provider_rows(width)),
            View::Rewind => {
                let english = self.preferences.language == Language::English;
                rows.push((
                    if english {
                        "Choose a restore point"
                    } else {
                        "복원 시점 선택"
                    }
                    .into(),
                    Tone::Accent,
                    None,
                ));
                rows.push((
                    if english {
                        "Restore after the selected reply. Code requires a saved checkpoint."
                    } else {
                        "선택한 답변 직후로 복원합니다. 코드는 저장된 복원 기록이 있어야 합니다."
                    }
                    .into(),
                    Tone::Muted,
                    None,
                ));
                if let Some(turn) = &self.rewind_confirm {
                    let points = self.rewind_points();
                    if let Some((number, (_, request, answer))) =
                        points.iter().enumerate().find(|(_, point)| &point.0 == turn)
                    {
                        rows.push((
                            one_line(&format!("{}. {request}", number + 1), width),
                            Tone::Normal,
                            None,
                        ));
                        if !answer.is_empty() {
                            rows.push((one_line(&format!("   ↳ {answer}"), width), Tone::Muted, None));
                        }
                        rows.push((String::new(), Tone::Normal, None));
                    }
                    rows.push((
                        if english {
                            "Rewind conversation only"
                        } else {
                            "대화만 되돌리기"
                        }
                        .into(),
                        Tone::Accent,
                        Some(Action::ConfirmConversationRewind),
                    ));
                    match &self.rewind_preview {
                        Some((files, added, removed, None)) => {
                            rows.push((
                                format!(
                                    "{}: {files}",
                                    if english {
                                        "Files changed"
                                    } else {
                                        "바뀔 파일"
                                    }
                                ),
                                Tone::Muted,
                                None,
                            ));
                            rows.push((
                                format!("+{added} −{removed}"),
                                Tone::Normal,
                                Some(Action::RewindDelta(*added, *removed)),
                            ));
                            rows.push((
                                if english {
                                    "Rewind conversation and code"
                                } else {
                                    "대화와 코드 되돌리기"
                                }
                                .into(),
                                Tone::Warning,
                                Some(Action::ConfirmRewind),
                            ));
                        }
                        Some((_, _, _, Some(reason))) => {
                            rows.push((
                                if english {
                                    "Code rewind unavailable"
                                } else {
                                    "코드 복원 불가"
                                }
                                .into(),
                                Tone::Warning,
                                None,
                            ));
                            rows.extend(
                                wrap(reason, width)
                                    .into_iter()
                                    .map(|line| (line, Tone::Muted, None)),
                            );
                        }
                        None => rows.push((
                            if english {
                                "Loading code preview…"
                            } else {
                                "코드 변경 미리보기 확인 중…"
                            }
                            .into(),
                            Tone::Muted,
                            None,
                        )),
                    }
                    rows.push((
                        if english { "Cancel" } else { "취소" }.into(),
                        Tone::Normal,
                        Some(Action::Back),
                    ));
                } else {
                    // Oldest first, like the conversation: the request identifies the point,
                    // its reply below helps recognize it. Both lines select it.
                    let points = self.rewind_points();
                    for (number, (turn, request, answer)) in points.iter().enumerate() {
                        let action = Some(Action::Rewind(turn.clone()));
                        rows.push((
                            one_line(&format!("{}. {request}", number + 1), width),
                            Tone::Normal,
                            action.clone(),
                        ));
                        if !answer.is_empty() {
                            rows.push((one_line(&format!("   ↳ {answer}"), width), Tone::Muted, action));
                        }
                    }
                    if points.is_empty() {
                        rows.push((
                            if english {
                                "No completed replies"
                            } else {
                                "완료된 답변이 없습니다"
                            }
                            .into(),
                            Tone::Muted,
                            None,
                        ));
                    }
                }
            }
            View::Chat => {
                append_entries(
                    &mut rows,
                    &self.entries,
                    width,
                    &self.tool_pages,
                    &self.message_cache,
                    &self.preferences,
                    &self.tool_overrides,
                    &self.entry_times,
                );
            }
            View::Diff => {
                let content_start = rows.len();
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
                    if rows.len() == content_start + 4 {
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
                if self.usage_expanded {
                    rows.extend(self.meter_rows(width));
                    return rows;
                }
                if self.selected_task.is_none() && self.goal.is_some() {
                    rows.extend(self.management_rows(width));
                }
                rows.extend(self.task_rows(width, true));
                if self.selected_task.is_none() && self.tasks.is_empty() {
                    panel_section(&mut rows, "하위 에이전트", width);
                }
                if self.agents.is_empty() && self.selected_task.is_none() && self.tasks.is_empty() {
                    rows.push(("현재 연결된 하위 에이전트 없음".into(), Tone::Muted, None));
                }
                for (index, agent) in self
                    .agents
                    .iter()
                    .enumerate()
                    .filter(|_| self.selected_task.is_none() && self.tasks.is_empty())
                {
                    rows.push((
                        format!("{} · {} ▸", agent.name, status_label(&agent.status)),
                        Tone::Normal,
                        Some(Action::Agent(index)),
                    ));
                    rows.push((
                        "  새 터미널에서 열기 ↗".into(),
                        Tone::Muted,
                        Some(Action::OpenAgent(index)),
                    ));
                }
                if let Some(agent) = self
                    .agents
                    .get(self.selected_agent)
                    .filter(|_| self.selected_task.is_none() && !self.agent_entries.is_empty())
                {
                    panel_section(&mut rows, "선택한 에이전트", width);
                    rows.push((agent.name.clone(), Tone::Accent, None));
                    for line in wrap(&agent.detail, width) {
                        rows.push((line, Tone::Normal, None));
                    }
                }
                if !self.agent_entries.is_empty() && self.selected_task.is_none() {
                    panel_section(&mut rows, "에이전트 대화", width);
                    append_entries(
                        &mut rows,
                        &self.agent_entries,
                        width,
                        &self.tool_pages,
                        &self.message_cache,
                        &self.preferences,
                        &self.tool_overrides,
                        &self.entry_times,
                    );
                }
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
                    "Ctrl+P 명령 검색 · / 자동완성 · $ 스킬 검색\nTab/Shift+Tab 버튼 이동 · Enter 선택\nEsc 응답 중단 · 메뉴/질문에서는 닫기\nAlt+Enter 줄바꿈 · Ctrl+C 중단\nPageUp/PageDown 기록 · Ctrl+Q 종료\n마우스 오버 강조 · 클릭 실행 · 휠 스크롤",
                    width,
                ) {
                    rows.push((line, Tone::Normal, None));
                }
            }
            View::Skills => {
                rows.extend(skill_rows(
                    &self.skills,
                    &self.selected_skills,
                    &self.skill_errors,
                    self.skills_loaded,
                    self.preferences.language,
                    width,
                ));
            }
            View::Settings => rows.extend(self.settings_rows(width)),
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
                                    "Trusted earlier"
                                } else {
                                    "이전에 신뢰됨"
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
                            "Current: default (not queried)"
                        } else {
                            "현재: 기본 설정 · 미확인"
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
                        "작업 폴더 쓰기",
                        "Workspace write",
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
                if let Some(network) = self.network_mode.as_deref() {
                    rows.push((String::new(), Tone::Normal, None));
                    rows.push((
                        if english { "Network access" } else { "네트워크 접근" }.into(),
                        Tone::Muted,
                        None,
                    ));
                    for (value, korean, en) in [
                        ("deny", "차단 · 명령과 도구가 인터넷에 연결하지 않음", "Blocked · commands and tools stay offline"),
                        ("allow", "허용 · 웹 검색·웹 페이지 읽기·패키지 설치 가능", "Allowed · web search, web pages, package installs"),
                    ] {
                        let selected = network == value;
                        rows.push((
                            format!("{} {}", if selected { "●" } else { "○" }, if english { en } else { korean }),
                            if selected { Tone::Success } else { Tone::Normal },
                            (!self.readonly && self.permission_pending.is_none())
                                .then(|| Action::Permission("network".into(), value.into())),
                        ));
                    }
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
                            "Current: default (not queried)"
                        } else {
                            "현재: 기본 설정 · 미확인"
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
                if let Some(armed) = self
                    .session_delete_armed
                    .as_ref()
                    .and_then(|id| self.sessions.iter().find(|s| &s.id == id))
                {
                    let title = one_line(&armed.title, width.saturating_sub(30));
                    rows.push((
                        if english {
                            format!("! Press Delete again to delete “{title}”")
                        } else {
                            format!("! 「{title}」 삭제: Delete를 한 번 더 누르세요")
                        },
                        Tone::Warning,
                        None,
                    ));
                } else if !self.readonly
                    && self.sessions.iter().any(|s| self.thread.as_deref() != Some(s.id.as_str()))
                {
                    rows.push((
                        if english {
                            "Delete twice on a conversation removes it"
                        } else {
                            "대화에서 Delete 두 번: 삭제"
                        }
                        .into(),
                        Tone::Faint,
                        None,
                    ));
                }
                for (index, session) in self
                    .sessions
                    .iter()
                    .filter(|s| self.thread.as_deref() != Some(s.id.as_str()))
                    .enumerate()
                {
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
                    let meta = format!("{activity}  {status}").trim().to_owned();
                    let inline = width >= 58 && !meta.is_empty();
                    let prefix = format!("{}. ○ ", index + 1);
                    let title = one_line(
                        &session.title,
                        width.saturating_sub(
                            UnicodeWidthStr::width(prefix.as_str())
                                + if inline {
                                    UnicodeWidthStr::width(meta.as_str()) + 2
                                } else {
                                    4
                                },
                        ),
                    );
                    let line = if inline {
                        let padding = width.saturating_sub(
                            UnicodeWidthStr::width(prefix.as_str())
                                + UnicodeWidthStr::width(title.as_str())
                                + UnicodeWidthStr::width(meta.as_str()),
                        );
                        format!("{prefix}{title}{}{meta}", " ".repeat(padding))
                    } else {
                        format!("{prefix}{title}")
                    };
                    rows.push((
                        line,
                        if locked { Tone::Muted } else { Tone::Normal },
                        (!self.readonly).then(|| Action::Resume(session.id.clone())),
                    ));
                    if !inline && !meta.is_empty() {
                        rows.push((
                            format!("  {meta}"),
                            if session.status == "systemError" {
                                Tone::Danger
                            } else {
                                Tone::Muted
                            },
                            None,
                        ));
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
            View::Routing => rows.extend(self.routing_rows(width)),
            View::Runtime => rows.extend(self.runtime_rows(width)),
            View::Models => {
                rows.extend(self.model_catalog_rows(width));
                rows.push((String::new(), Tone::Normal, None));
                if let Some((requested, provider, model)) = &self.resolved_model {
                    let model = self.served_display(requested, model);
                    rows.push((
                        format!(
                            "{} · {model}",
                            if self.preferences.language == Language::English {
                                "Last response"
                            } else {
                                "최근 응답"
                            }
                        ),
                        Tone::Muted,
                        None,
                    ));
                    rows.push((
                        if self.model_details_open {
                            "▾ 모델 상세"
                        } else {
                            "▸ 모델 상세"
                        }
                        .into(),
                        Tone::Muted,
                        Some(Action::ToggleModelDetails),
                    ));
                    if self.model_details_open {
                        for detail in [
                            format!("요청 경로: {requested}"),
                            format!("Provider: {provider}"),
                            format!("실제 모델: {model}"),
                        ] {
                            rows.extend(
                                wrap(&detail, width)
                                    .into_iter()
                                    .map(|line| (line, Tone::Muted, None)),
                            );
                        }
                    }
                }
                rows.push((
                    format!(
                        "{} · {}",
                        self.label("선택한 모델"),
                        if self.model.is_empty() {
                            "runtime".to_owned()
                        } else {
                            match model_group(&self.model) {
                                Some(group) => format!(
                                    "{} · {}",
                                    self.model_display(&self.model),
                                    group.split_once('\u{1}').map_or(group.as_str(), |(_, g)| g)
                                ),
                                None => self.model.clone(),
                            }
                        }
                    ),
                    Tone::Accent,
                    None,
                ));
                rows.push((
                    self.label("다음 메시지에 사용할 모델").into(),
                    Tone::Muted,
                    None,
                ));
                rows.push((String::new(), Tone::Normal, None));
                if self.models.is_empty() && !self.model_catalog_loading {
                    rows.push(("모델 목록을 불러오는 중…".into(), Tone::Muted, None));
                }
                rows.push((String::new(), Tone::Normal, None));
                if !self.model_profile().is_some_and(|p| p.efforts.is_empty()) {
                    rows.extend(self.effort_rows(width));
                }
                rows.push((String::new(), Tone::Normal, None));
                rows.push((
                    if self.preferences.language == Language::English {
                        "↑↓ / Tab choose · Enter apply · Esc return"
                    } else {
                        "입력 검색 · ↑↓ / Tab 선택 · Enter 적용 · Esc 대화"
                    }
                    .into(),
                    Tone::Muted,
                    None,
                ));
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
        if rows.is_empty() && *view != View::Chat {
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
// Subpanes share a visual hierarchy: section divider, short summary, optional detail.
fn panel_section(rows: &mut Vec<ContentRow>, title: &str, width: usize) {
    if rows.last().is_some_and(|row| !row.0.is_empty()) {
        rows.push((String::new(), Tone::Normal, None));
    }
    let label = format!("─ {title} ");
    let used = unicode_width::UnicodeWidthStr::width(label.as_str());
    for line in wrap(
        &format!("{label}{}", "─".repeat(width.saturating_sub(used))),
        width,
    ) {
        rows.push((line, Tone::Accent, None));
    }
}

// Group by the skill's actual location. The API only supplies name, path and
// description, so the UI avoids guessing a category from description text.
fn skill_group(skill: &Skill, language: Language) -> (u8, String) {
    let path = skill.path.replace('\\', "/");
    let parts: Vec<_> = path.split('/').collect();
    let english = language == Language::English;
    if let Some(plugin) = parts
        .windows(3)
        .find_map(|parts| (parts[0] == "plugins" && parts[1] == "cache").then_some(parts[2]))
    {
        let name = skill
            .name
            .split_once(':')
            .map(|(prefix, _)| prefix)
            .filter(|prefix| !prefix.starts_with("app-") && prefix.len() <= 24)
            .unwrap_or(plugin);
        let name = if name.starts_with("app-") {
            if english { "Apps" } else { "연결 앱" }
        } else if name.starts_with("openai-") {
            "OpenAI"
        } else {
            name
        };
        return (
            2,
            format!("{} · {name}", if english { "Plugins" } else { "확장" }),
        );
    }
    if parts
        .windows(3)
        .any(|parts| parts[0] == ".codex" && parts[1] == "skills" && parts[2] == ".system")
    {
        return (0, if english { "Built-in" } else { "기본 제공" }.into());
    }
    if parts
        .windows(2)
        .any(|parts| (parts[0] == ".codex" || parts[0] == ".agents") && parts[1] == "skills")
    {
        return (1, if english { "Personal" } else { "직접 추가" }.into());
    }
    (3, if english { "Other" } else { "기타" }.into())
}

fn skill_rows(
    skills: &[Skill],
    selected: &[Skill],
    errors: &[String],
    loaded: bool,
    language: Language,
    width: usize,
) -> Vec<ContentRow> {
    let english = language == Language::English;
    let mut rows = vec![(
        if english {
            format!("{} installed skills", skills.len())
        } else {
            format!("설치된 스킬 {}개", skills.len())
        },
        Tone::Accent,
        None,
    )];
    rows.push((
        if english && width < 58 {
            "Tab/Enter · Search: $name"
        } else if english {
            "Choose with Tab + Enter · Search with $name"
        } else if width < 58 {
            "Tab/Enter 선택 · $이름 검색"
        } else {
            "Tab + Enter 선택 · 입력창에서 $이름 검색"
        }
        .into(),
        Tone::Muted,
        None,
    ));
    if !loaded {
        rows.push((
            if english {
                "Loading skills…"
            } else {
                "스킬 목록을 불러오는 중…"
            }
            .into(),
            Tone::Muted,
            None,
        ));
        return rows;
    }
    if skills.is_empty() {
        rows.push((
            if english {
                "No skills available here."
            } else {
                "이 작업 폴더에 사용 가능한 스킬이 없습니다."
            }
            .into(),
            Tone::Muted,
            None,
        ));
    }

    if !selected.is_empty() {
        panel_section(
            &mut rows,
            &format!(
                "{} ({})",
                if english { "Selected" } else { "선택됨" },
                selected.len()
            ),
            width,
        );
        for skill in selected {
            rows.push((
                format!("✓ ${} ×", one_line(&skill.name, width.saturating_sub(7))),
                Tone::Success,
                Some(Action::RemoveSkill(skill.path.clone())),
            ));
        }
    }

    let selected_paths: HashSet<_> = selected.iter().map(|skill| skill.path.as_str()).collect();
    let mut groups: BTreeMap<(u8, String), Vec<&Skill>> = BTreeMap::new();
    for skill in skills {
        if !selected_paths.contains(skill.path.as_str()) {
            groups
                .entry(skill_group(skill, language))
                .or_default()
                .push(skill);
        }
    }
    for ((_, title), group) in groups {
        let count = format!(" ({})", group.len());
        let title = one_line(&title, width.saturating_sub(count.len() + 3));
        panel_section(&mut rows, &format!("{title}{count}"), width);
        for skill in group {
            rows.push((
                one_line(&format!("${}", skill.name), width.saturating_sub(2)),
                Tone::Accent,
                Some(Action::Skill(skill.path.clone())),
            ));
            if !skill.description.trim().is_empty() {
                rows.push((
                    format!(
                        "  {}",
                        one_line(&skill.description, width.saturating_sub(4))
                    ),
                    Tone::Muted,
                    None,
                ));
            }
            rows.push((String::new(), Tone::Normal, None));
        }
    }
    if !errors.is_empty() {
        panel_section(
            &mut rows,
            if english {
                "Load errors"
            } else {
                "불러오기 오류"
            },
            width,
        );
        for error in errors {
            for line in wrap(error, width) {
                rows.push((line, Tone::Warning, None));
            }
        }
    }
    rows
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
        "active" | "running" | "inProgress" | "working" | "실행 중" | "전송 중" => ("◉", Tone::Accent),
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
fn transcript_band_starts(rows: &[ContentRow]) -> Vec<usize> {
    if !rows.iter().any(|row| {
        matches!(
            row.2,
            Some(Action::SelectEntry(_) | Action::MessageRow(_, _, _))
        )
    }) {
        return Vec::new();
    }
    let mut starts = Vec::new();
    for (index, (text, tone, action)) in rows.iter().enumerate() {
        if action.is_none()
            && !text.is_empty()
            && text.chars().all(|ch| ch == '─')
            && (*tone == Tone::User || (*tone == Tone::Faint && starts.is_empty()))
        {
            starts.push(index);
        }
    }
    starts
}

fn paint_rows(
    rows: &[(String, Tone, Option<Action>)],
    scroll: usize,
    width: u16,
    height: u16,
    focus: Option<&Action>,
    message_cache: &MessageCache,
    band_starts: Option<&[usize]>,
    animation: Option<(&[Entry], usize)>,
) -> Canvas {
    let mut c = Canvas::new(width, height);
    let transcript = band_starts.is_some()
        || rows.iter().any(|row| {
            matches!(
                row.2,
                Some(Action::SelectEntry(_) | Action::MessageRow(_, _, _))
            )
        });
    // For cached chat rows, jump straight to the viewport. Only an uncached
    // pane needs to scan offscreen dividers to determine the first band.
    let mut next_band = band_starts.map_or(0, |starts| starts.partition_point(|&i| i < scroll));
    let mut band = match next_band {
        0 => crate::engine::Background::Default,
        count if count % 2 == 1 => crate::engine::Background::ResponseA,
        _ => crate::engine::Background::ResponseB,
    };
    let start = if band_starts.is_some() { scroll } else { 0 };
    for (index, (text, tone, action)) in rows.iter().enumerate().skip(start).take(
        scroll
            .saturating_add(usize::from(height))
            .saturating_sub(start),
    ) {
        let starts_here = if let Some(starts) = band_starts {
            starts.get(next_band).copied() == Some(index)
        } else {
            transcript
                && action.is_none()
                && !text.is_empty()
                && text.chars().all(|ch| ch == '─')
                && (*tone == Tone::User || (*tone == Tone::Faint && next_band == 0))
        };
        if starts_here {
            band = if next_band % 2 == 0 {
                crate::engine::Background::ResponseA
            } else {
                crate::engine::Background::ResponseB
            };
            next_band += 1;
        }
        if index < scroll {
            continue;
        }
        let y = index - scroll;
        for cell in &mut c.cells[y * usize::from(width)..(y + 1) * usize::from(width)] {
            cell.background = band;
        }
        let mut line = Canvas::new(width.saturating_sub(4), 1);
        for cell in &mut line.cells {
            cell.background = band;
        }
        // Every pane's text can be dragged to copy (a click still runs the row's action);
        // divider lines are not text.
        if !text.trim().is_empty() && !(action.is_none() && text.chars().all(|ch| ch == '─'))
        {
            let inset = if matches!(
                action,
                Some(Action::SelectEntry(_) | Action::MessageRow(_, _, _))
            ) {
                2
            } else {
                0
            };
            // Rows drawn as buttons are padded by a leading space, so the selectable span
            // reaches to the row's end; copying trims the trailing blanks.
            let text_width = if text.is_empty() { 0 } else { line.width };
            if text_width > inset {
                line.hits.push(crate::engine::Hit {
                    x: inset,
                    y: 0,
                    width: text_width - inset,
                    height: 1,
                    action: Action::TranscriptText,
                });
            }
        }
        if let Some(action) = action {
            let target = match action {
                Action::MessageRow(id, _, _) => Action::SelectEntry(id.clone()),
                _ => action.clone(),
            };
            if let Action::RewindDelta(added, removed) = action {
                let x = line.text(0, 0, &format!("+{added}"), Tone::Success);
                line.text(x, 0, &format!(" −{removed}"), Tone::Danger);
            } else if matches!(action, Action::ModelSearch) {
                // A real field spans its frame; it is not a padded list button.
                document::paint(&mut line, text, *tone);
                line.hits.push(crate::engine::Hit {
                    x: 0, y: 0,
                    width: UnicodeWidthStr::width(text.as_str()).min(usize::from(line.width)) as u16,
                    height: 1, action: target.clone(),
                });
            } else if let Action::SettingChoices(choices) = action {
                settings::paint_choices(&mut line, text, choices);
            } else if matches!(action, Action::Resume(_)) {
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
            } else if matches!(action, Action::Toggle(_))
                && (text.starts_with("● ") || text.starts_with("  "))
            {
                document::paint(&mut line, text, *tone);
                line.hits.push(crate::engine::Hit {
                    x: 0,
                    y: 0,
                    width: line.width,
                    height: 1,
                    action: action.clone(),
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
                let link_hits = std::mem::take(&mut line.hits);
                line.hits.push(crate::engine::Hit {
                    x: 0,
                    y: 0,
                    width: unicode_width::UnicodeWidthStr::width(text.as_str())
                        .min(usize::from(line.width)) as u16,
                    height: 1,
                    action: target.clone(),
                });
                line.hits.extend(link_hits);
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
                    Action::Permission(_, _) | Action::ConfirmPermission(_) | Action::Setting(_)
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
            if focus == Some(&target) && !matches!(target, Action::SelectEntry(_)) {
                line.highlight(&target, Tone::Selected);
            }
            if matches!(action, Action::Resume(_)) {
                // Execution state keeps its semantic color when the row has
                // keyboard focus. The rest of the row remains the selection.
                for (marker, tone) in [
                    ("● 실행 중", Tone::Accent),
                    ("● Running", Tone::Accent),
                    ("! 오류", Tone::Danger),
                    ("! Error", Tone::Danger),
                ] {
                    if let Some(byte) = text.find(marker) {
                        let x = UnicodeWidthStr::width(&text[..byte]) as u16;
                        line.text(x, 0, marker, tone);
                        break;
                    }
                }
            }
        } else {
            document::paint(&mut line, text, *tone);
            if text.starts_with("● ") {
                line.text(0, 0, "●", Tone::Accent);
            }
        }
        if let Some((entries, frame)) = animation {
            match action {
                Some(Action::MessageRow(id, row, _))
                    if entries.iter().any(|entry| {
                        &entry.id == id
                            && entry.kind == Kind::Assistant
                            && entry.title == crate::agent::PROGRESS_TITLE
                            && entry.status == "inProgress"
                    }) =>
                {
                    progress::paint(&mut line, *row == 0, frame)
                }
                Some(Action::Toggle(id))
                    if entries.iter().any(|entry| {
                        entry.title == crate::agent::PROGRESS_TITLE
                            && entry.status == "inProgress"
                            && (entry
                                .id
                                .strip_suffix("-assistant")
                                .is_some_and(|turn| id == &format!("{turn}-v5-log"))
                                || &entry.id == id)
                    }) && !text.starts_with("▸")
                        && !text.starts_with("▾") =>
                {
                    progress::paint(&mut line, text.starts_with("● "), frame);
                }
                Some(Action::Toggle(id))
                    if entries.iter().any(|entry| {
                        &entry.id == id && entry.kind == Kind::Tool && entry.status == "inProgress"
                    }) && (text.starts_with("▸ ◉") || text.starts_with("▾ ◉")) =>
                {
                    line.text(
                        3,
                        0,
                        progress::SPINNER[frame % progress::SPINNER.len()],
                        Tone::Accent,
                    );
                }
                _ => {}
            }
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
    preferences: &Preferences,
    tool_overrides: &HashMap<String, bool>,
    entry_times: &HashMap<String, u64>,
) {
    // An assistant's commentary, tools and final answer form one visual group.
    // Start another group only on a speaker change, including consecutive user turns.
    let mut loom_group = false;
    for (index, entry) in entries.iter().enumerate() {
        if entry.title == crate::agent::PROGRESS_TITLE && entry.body.is_empty() {
            continue;
        }
        let is_loom = matches!(entry.kind, Kind::Assistant | Kind::Tool | Kind::Change);
        if entry.kind == Kind::User || (is_loom && !loom_group) {
            rows.push((
                "─".repeat(width),
                if entry.kind == Kind::User {
                    Tone::User
                } else {
                    Tone::Faint
                },
                None,
            ));
        }
        loom_group = is_loom;

        let expanded = tool_overrides
            .get(&entry.id)
            .copied()
            .unwrap_or_else(|| match preferences.tool_details {
                preferences::ToolDetails::Auto => entry.expanded,
                preferences::ToolDetails::Expanded => true,
                preferences::ToolDetails::Collapsed => false,
            });
        if preferences.timestamps {
            let time = entry_times
                .get(&entry.id)
                .map(|seconds| settings::timestamp(*seconds))
                .unwrap_or_else(|| "—".into());
            rows.push((time, Tone::Muted, None));
        }
        match entry.kind {
            Kind::Tool | Kind::Change => {
                if entry.kind == Kind::Change
                    && let Some(preview) = document::codex_diff_preview_rows(&entry.body, width, 3)
                {
                    let action = Action::DiffEntry(entry.id.clone());
                    for (line, tone) in preview {
                        rows.push((line, tone, None));
                    }
                    rows.push(("변경 내용 전체 보기 ▸".into(), Tone::Muted, Some(action)));
                    continue;
                }
                let (symbol, state_tone) = item_style(&entry.status);
                let completed_tool = entry.kind == Kind::Tool
                    && matches!(entry.status.as_str(), "completed" | "complete" | "success");
                // Core attaches the observed outcome after the action/target.
                // Keep that outcome visible when the folded header is too narrow.
                let separate_result = entry.kind == Kind::Tool
                    && entry.title.matches(" · ").count() >= 2
                    && unicode_width::UnicodeWidthStr::width(entry.title.as_str())
                        > width.saturating_sub(14);
                let (title, result) = if separate_result {
                    let (title, result) = entry.title.rsplit_once(" · ").unwrap();
                    (title, Some(result))
                } else {
                    (entry.title.as_str(), None)
                };
                // ✓ already says a tool finished; other states keep their word.
                let state = if completed_tool {
                    String::new()
                } else {
                    format!(" · {}", status_label(&entry.status))
                };
                rows.push((
                    format!(
                        "{} {} {}{state}",
                        if expanded { "▾" } else { "▸" },
                        symbol,
                        document::command_summary(title, width.saturating_sub(14)),
                    ),
                    if completed_tool {
                        Tone::Muted
                    } else {
                        state_tone
                    },
                    Some(Action::Toggle(entry.id.clone())),
                ));
                if let Some(result) = result {
                    rows.extend(
                        wrap(result, width.saturating_sub(4))
                            .into_iter()
                            .take(2)
                            .map(|line| {
                                (
                                    format!("    {line}"),
                                    if completed_tool {
                                        Tone::Muted
                                    } else {
                                        state_tone
                                    },
                                    Some(Action::Toggle(entry.id.clone())),
                                )
                            }),
                    );
                }
                let has_diff = entry.kind == Kind::Change || document::is_diff(&entry.body);
                if expanded || has_diff {
                    if expanded && entry.kind == Kind::Tool {
                        rows.push((
                            format!(
                                "$ {}",
                                document::command_summary(entry.title.strip_prefix("run · ").unwrap_or(&entry.title), width.saturating_sub(3))
                            ),
                            Tone::Muted,
                            None,
                        ));
                    }
                    const PAGE_SIZE: usize = 24;
                    let page = if expanded {
                        tool_pages.get(&entry.id).copied().unwrap_or(0)
                    } else {
                        0
                    };
                    let limit = if expanded { PAGE_SIZE } else { 12 };
                    let (output, has_next) =
                        document::output_page(&entry.body, width, page, limit, has_diff);
                    if expanded && page > 0 {
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
                            if expanded {
                                format!("다음 {PAGE_SIZE}줄 ▸")
                            } else {
                                "… 펼쳐서 계속 보기".into()
                            },
                            Tone::Muted,
                            Some(if expanded {
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
                if entry.title.starts_with("모델 · ") {
                    rows.push((one_line(&entry.title, width), Tone::Muted, None));
                }
                // Account for the speaker marker and clickable progress padding.
                let width = width.saturating_sub(if entry.title == crate::agent::PROGRESS_TITLE { 4 } else { 2 });
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
                // The status line already says what is happening; the transcript keeps
                // only the spinner (still clickable for the activity log).
                if entry.title == crate::agent::PROGRESS_TITLE && entry.status == "inProgress" {
                    append_message(rows, entry, &[document::StyledRow::plain(String::new(), Tone::Muted)], Some(width));
                    continue;
                }
                append_message(rows, entry, &cached.rows, Some(width));
            }
            _ => {
                let available = width.saturating_sub(2);
                let rendered: Vec<_> = wrap(&entry.body, available)
                    .into_iter()
                    .map(|line| document::StyledRow::plain(line, Tone::Normal))
                    .collect();
                append_message(rows, entry, &rendered, None);
            }
        }
        // Consecutive tool rows read as one list; space only around other entries.
        let next_is_tool = entries[index + 1..]
            .iter()
            .find(|next| !(next.title == crate::agent::PROGRESS_TITLE && next.body.is_empty()))
            .is_some_and(|next| next.kind == Kind::Tool);
        if preferences.density == preferences::Density::Normal
            && !(entry.kind == Kind::Tool && next_is_tool && !expanded)
        {
            rows.push((String::new(), Tone::Normal, None));
        }
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
            if entry.title == crate::agent::PROGRESS_TITLE && entry.status == "inProgress" {
                Some(Action::Toggle(
                    entry
                        .id
                        .strip_suffix("-assistant")
                        .map(|turn| format!("{turn}-v5-log"))
                        .unwrap_or_else(|| entry.id.clone()),
                ))
            } else if let Some(width) = cache_width {
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
    use crate::agent::{AgentEvent, AgentUpdate};
    use serde_json::json;

    #[test]
    fn typed_slash_commands_are_recalled_without_sending_them_to_model() {
        let mut app = App::new(false);
        app.editor.insert("/model");
        assert_eq!(app.activate(Action::Send), Some(Intent::RefreshModels));
        // Up inside the opened model picker moves through models, so leave it first.
        let canvas = app.render(80, 30);
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &canvas);
        let canvas = app.render(80, 30);
        app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &canvas);
        assert_eq!(app.editor.text, "/model");
        assert!(app.submitted.is_none() && app.queued.is_empty());
    }
    #[test]
    fn typing_after_a_slash_command_returns_from_compact_details() {
        let mut app = App::new(false);
        app.editor.insert("/approvals");
        app.activate(Action::Send);
        let canvas = app.render(80, 24);
        app.key(
            KeyEvent::new(KeyCode::Char('안'), KeyModifiers::NONE),
            &canvas,
        );
        assert_eq!(app.view, View::Chat);
        assert_eq!(app.editor.text, "안");
    }
    #[test]
    fn welcome_logo_adapts_to_terminal_size_and_disappears_after_first_message() {
        let mut app = App::new(false);
        let full = app.render(80, 24);
        let full_lines: Vec<_> = (0..full.height).map(|y| full.plain_line(y)).collect();
        assert!(full_lines.iter().any(|line| line.contains("/__/:/____")));
        assert!(full_lines.iter().any(|line| line.contains("/__/::::|")));

        let compact = app.render(40, 24);
        let compact_lines: Vec<_> = (0..compact.height).map(|y| compact.plain_line(y)).collect();
        assert!(
            compact_lines
                .iter()
                .any(|line| line.contains("|_____ '----'"))
        );
        assert!(!compact_lines.iter().any(|line| line.contains("/__/:/____")));

        let short = app.render(40, 12);
        let short_text = (0..short.height)
            .map(|y| short.plain_line(y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!short_text.contains("'----'"));
        assert!(short_text.contains("아래에 지시를 입력하세요."));

        app.thread = Some("main".into());
        app.editor.insert("hello");
        assert_eq!(
            app.activate(Action::Send),
            Some(Intent::Submit("hello".into()))
        );
        let sending = app.render(80, 24);
        assert!(
            !(0..sending.height)
                .map(|y| sending.plain_line(y))
                .any(|line| line.contains("/__/:/____"))
        );

        app.entries.push(Entry {
            id: "first".into(),
            kind: Kind::User,
            title: String::new(),
            body: "hello".into(),
            status: String::new(),
            expanded: false,
        });
        let started = app.render(80, 24);
        let started_text = (0..started.height)
            .map(|y| started.plain_line(y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(started_text.contains("hello"));
        assert!(!started_text.contains("/__/:/____"));
    }
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
    fn focused_and_hovered_markdown_preserves_styles_and_math() {
        let mut app = App::new(false);
        app.entries.push(Entry {
            id: "formatted".into(),
            kind: Kind::Assistant,
            title: "Loom".into(),
            body: "**굵게** *기울임* ~~취소~~ <underline>밑줄</underline> `code()` \\(x^2\\)\n\n```rust\nlet x = 1;\n```".into(),
            status: "completed".into(),
            expanded: false,
        });
        for selection in [Tone::Selected, Tone::Hover] {
            if selection == Tone::Selected {
                app.focus = Some(Action::SelectEntry("formatted".into()));
            } else {
                app.focus = None;
                app.hovered = Some(Action::SelectEntry("formatted".into()));
            }
            let canvas = app.render(110, 35);
            let text = (0..canvas.height)
                .map(|y| canvas.plain_line(y))
                .collect::<Vec<_>>()
                .join("\n");
            assert!(text.contains("x²") && text.contains("let x = 1;"), "{text}");
            for tone in [
                Tone::Strong,
                Tone::Emphasis,
                Tone::Strike,
                Tone::Underline,
                Tone::InlineCode,
                Tone::Math,
            ] {
                assert!(
                    canvas.cells.iter().any(|c| c.tone == tone
                        && !matches!(
                            c.background,
                            crate::engine::Background::Selected | crate::engine::Background::Hover
                        )),
                    "missing {tone:?}"
                );
            }
        }
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
        let bars: Vec<_> = rows
            .iter()
            .filter(|row| row.0.chars().all(|ch| ch == '─') && !row.0.is_empty())
            .collect();
        assert_eq!(
            bars.len(),
            4,
            "tools and assistant continuations share a group"
        );
        assert_eq!(bars[0].1, Tone::User);
        assert_eq!(bars[1].1, Tone::Faint);
        assert!(bars.iter().all(|r| r.0.chars().all(|ch| ch == '─')));
        assert!(bars.iter().all(
            |row| unicode_width::UnicodeWidthStr::width(row.0.as_str()) == 36 && row.2.is_none()
        ));
        app.activate(Action::Setting("english".into()));
        let english = app.cached_chat_rows(36);
        assert_eq!(english.iter().filter(|r| r.0 == "─".repeat(36)).count(), 4);
        assert!(!english.iter().any(|row| row.0.contains("── 사용자")
            || row.0.contains("── You")
            || row.0.contains("── Loom")));
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
        assert_eq!(app.activate(Action::Send), None);
        assert!(
            app.paste_pending,
            "The default multiline paste policy asks before sending"
        );
        assert_eq!(
            app.activate(Action::Setting("paste:confirm".into())),
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
    fn empty_composer_arrows_recall_sent_inputs_and_editing_exits_history() {
        let mut app = App::new(false);
        app.thread = Some("main".into());
        app.editor.insert("첫 번째 요청");
        assert_eq!(
            app.activate(Action::Send),
            Some(Intent::Submit("첫 번째 요청".into()))
        );
        app.submitted = None; // Simulate the first request being accepted before the next send.
        app.editor.insert("두 번째\n요청");
        app.activate(Action::Send);
        assert!(app.editor.text.is_empty());

        let arrow = |app: &mut App, key| {
            let canvas = app.render(80, 24);
            app.key(KeyEvent::new(key, KeyModifiers::NONE), &canvas);
        };
        arrow(&mut app, KeyCode::Up);
        assert_eq!(app.editor.text, "두 번째\n요청");
        arrow(&mut app, KeyCode::Up);
        assert_eq!(app.editor.text, "첫 번째 요청");
        arrow(&mut app, KeyCode::Up);
        assert_eq!(app.editor.text, "첫 번째 요청");
        arrow(&mut app, KeyCode::Down);
        assert_eq!(app.editor.text, "두 번째\n요청");
        arrow(&mut app, KeyCode::Down);
        assert_eq!(app.editor.text, "");
        arrow(&mut app, KeyCode::Down);
        assert_eq!(app.editor.text, "");
        arrow(&mut app, KeyCode::Up);
        arrow(&mut app, KeyCode::Char('!'));
        assert_eq!(app.editor.text, "두 번째\n요청!");
        arrow(&mut app, KeyCode::Up);
        assert_eq!(
            app.editor.text, "두 번째\n요청!",
            "editing resumes multiline cursor movement"
        );
    }
    #[test]
    fn recalled_inputs_follow_session_history_and_do_not_cross_sessions() {
        let mut app = App::new(false);
        app.apply_event(AgentEvent::global(AgentUpdate::SessionLoaded {
            snapshot: crate::agent::SessionSnapshot {
                id: "old".into(),
                title: None,
                status: None,
                turns: Some(vec![crate::agent::TurnSnapshot {
                    id: Some("turn".into()),
                    status: "completed".into(),
                    entries: vec![Entry {
                        id: "user".into(),
                        kind: Kind::User,
                        title: "You".into(),
                        body: "이전 세션 입력".into(),
                        status: "completed".into(),
                        expanded: false,
                    }],
                    activities: Vec::new(),
                }]),
            },
            switching: true,
            model: None,
            permissions: crate::agent::Permissions::default(),
        }));
        let canvas = app.render(80, 24);
        app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &canvas);
        assert_eq!(app.editor.text, "이전 세션 입력");

        app.editor = Editor::default();
        app.apply_event(AgentEvent::global(AgentUpdate::SessionLoaded {
            snapshot: crate::agent::SessionSnapshot {
                id: "new".into(),
                title: None,
                status: None,
                turns: Some(Vec::new()),
            },
            switching: true,
            model: None,
            permissions: crate::agent::Permissions::default(),
        }));
        let canvas = app.render(80, 24);
        app.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &canvas);
        assert!(app.editor.text.is_empty());
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
    fn escape_interrupts_an_active_turn_without_clearing_draft_or_repeating_pending_request() {
        let mut app = App::new(false);
        app.thread = Some("main".into());
        app.turn = Some("turn".into());
        app.busy = true;
        app.editor.insert("보존할 초안");
        let canvas = app.render(80, 24);
        assert_eq!(
            app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &canvas),
            Some(Intent::Interrupt)
        );
        assert_eq!(app.editor.text, "보존할 초안");
        assert_eq!(app.status, "중단됨");
        app.track(99, "turn/interrupt");
        assert_eq!(
            app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &canvas),
            None
        );
        assert_eq!(app.editor.text, "보존할 초안");
        app.readonly = true;
        assert_eq!(
            app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &canvas),
            None
        );
    }
    #[test]
    fn approval_uses_composer_keyboard_and_preserves_draft() {
        let mut app = App::new(false);
        app.thread = Some("main".into());
        app.editor.insert("보존할 초안");
        app.server_request(json!(10), "item/commandExecution/requestApproval", &json!({"threadId":"main","command":"git status","availableDecisions":["accept","decline"]}));
        for (width, height) in [(40, 12), (80, 24), (140, 40)] {
            let canvas = app.render(width, height);
            assert!(canvas.cursor.is_none());
            assert!(!canvas.hits.iter().any(|h| h.action == Action::Input));
            assert!(
                canvas
                    .hits
                    .iter()
                    .any(|h| matches!(h.action, Action::Decide(_, _)))
            );
        }
        let canvas = app.render(80, 24);
        app.paste("붙여넣기");
        app.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &canvas);
        assert_eq!(
            app.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE), &canvas),
            Some(Intent::Reply("10".into(), "1".into()))
        );
        assert_eq!(app.editor.text, "보존할 초안");
    }
    #[test]
    fn queue_shortcut_uses_existing_interrupt_confirmation_without_losing_draft() {
        let mut app = App::new(false);
        app.thread = Some("main".into());
        app.busy = true;
        app.turn = Some("turn".into());
        app.editor.insert("우선 작업");
        app.activate(Action::Send);
        app.editor.insert("보존할 초안");
        let canvas = app.render(80, 24);
        assert_eq!(
            app.key(
                KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
                &canvas
            ),
            Some(Intent::Interrupt)
        );
        assert_eq!(app.force_after_interrupt.as_deref(), Some("turn"));
        assert_eq!(app.queued.front().unwrap().text, "우선 작업");
        assert_eq!(app.editor.text, "보존할 초안");
        assert!(app.next_queued().is_none());
        app.apply_event(crate::agent::AgentEvent::global(
            crate::agent::AgentUpdate::TurnCompleted {
                id: Some("turn".into()),
                status: "interrupted".into(),
                error: None,
            },
        ));
        assert!(app.next_queued().is_some());
    }
    #[test]
    fn control_shortcuts_accept_uppercase_enhanced_key_events() {
        let mut app = App::new(false);
        app.thread = Some("main".into());
        app.busy = true;
        app.turn = Some("turn".into());
        app.editor.insert("우선 작업");
        app.activate(Action::Send);
        let canvas = app.render(80, 24);

        assert_eq!(
            app.key(
                KeyEvent::new(
                    KeyCode::Char('N'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT
                ),
                &canvas
            ),
            Some(Intent::Interrupt)
        );
        assert_eq!(app.force_after_interrupt.as_deref(), Some("turn"));

        let mut blocked = App::new(false);
        blocked.thread = Some("main".into());
        assert!(blocked.server_request(
            json!(10),
            "item/commandExecution/requestApproval",
            &json!({"threadId":"main","command":"git status","availableDecisions":["accept","decline"]})
        ));
        let blocked_canvas = blocked.render(80, 24);
        assert_eq!(
            blocked.key(
                KeyEvent::new(
                    KeyCode::Char('Q'),
                    KeyModifiers::CONTROL | KeyModifiers::SHIFT
                ),
                &blocked_canvas
            ),
            Some(Intent::Quit)
        );
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
    fn branch_reports_progress_and_new_session_with_preserved_draft() {
        let mut app = App::new(false);
        app.set_thread(&json!({"id":"source","turns":[{"id":"one","status":"completed","items":[{"id":"a","type":"agentMessage","text":"first"}]}]}));
        app.editor.insert("draft");
        assert_eq!(
            app.activate(Action::Branch("a".into())),
            Some(Intent::Fork("one".into()))
        );
        assert!(app.notice.contains("분기"));
        assert_eq!(app.activate(Action::Branch("a".into())), None);
        app.apply_event(AgentEvent::global(AgentUpdate::SessionLoaded {
            snapshot: crate::agent::SessionSnapshot {
                id: "branch".into(),
                title: Some("first".into()),
                status: Some("idle".into()),
                turns: Some(Vec::new()),
            },
            switching: true,
            model: None,
            permissions: Default::default(),
        }));
        assert_eq!(app.thread.as_deref(), Some("branch"));
        assert!(app.sessions.iter().any(|session| session.id == "source"));
        assert!(app.sessions.iter().any(|session| session.id == "branch"));
        assert!(app.notice.contains("branch"));
        assert_eq!(app.editor.text, "draft");
    }
    #[test]
    fn double_escape_opens_rewind_picker_and_selection_requires_confirmation() {
        let mut app = App::new(false);
        app.set_thread(&json!({"id":"source","turns":[{"id":"one","status":"completed","items":[{"id":"a","type":"agentMessage","text":"first"}]}]}));
        let canvas = app.render(80, 30);
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &canvas);
        app.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE), &canvas);
        assert_eq!(app.view, View::Rewind);
        assert_eq!(
            app.activate(Action::Rewind("one".into())),
            Some(Intent::PreviewRewind("one".into()))
        );
        assert_eq!(
            app.activate(Action::ConfirmRewind),
            None,
            "code requires a successful preview"
        );
        app.apply_event(AgentEvent::scoped(
            "source",
            AgentUpdate::RewindPreview {
                turn_id: "one".into(),
                files: 1,
                added: 12,
                removed: 3,
                blocked: None,
            },
        ));
        assert_eq!(
            app.activate(Action::ConfirmRewind),
            Some(Intent::Rewind("one".into()))
        );
    }
    #[test]
    fn blocked_code_preview_still_allows_conversation_only_and_ignores_old_preview() {
        let mut app = App::new(false);
        app.set_thread(&json!({"id":"source","turns":[{"id":"one","status":"completed","items":[{"id":"a","type":"agentMessage","text":"first"}]}]}));
        app.activate(Action::Rewind("one".into()));
        app.apply_event(AgentEvent::scoped(
            "source",
            AgentUpdate::RewindPreview {
                turn_id: "old".into(),
                files: 1,
                added: 1,
                removed: 1,
                blocked: None,
            },
        ));
        assert!(app.rewind_preview.is_none());
        app.apply_event(AgentEvent::scoped(
            "source",
            AgentUpdate::RewindPreview {
                turn_id: "one".into(),
                files: 0,
                added: 0,
                removed: 0,
                blocked: Some("manual change".into()),
            },
        ));
        assert_eq!(app.activate(Action::ConfirmRewind), None);
        assert_eq!(
            app.activate(Action::ConfirmConversationRewind),
            Some(Intent::Fork("one".into()))
        );
    }
    #[test]
    fn current_codex_permissions_and_completed_tool_output_are_visually_distinct() {
        let mut app = App::new(false);
        app.workspace_trust = Some(true);
        app.read_config_settings(
            &json!({"approval_policy":"never","sandbox_mode":"danger-full-access"}),
        );
        let canvas = app.render(80, 24);
        assert!(canvas.plain_line(1).contains("전체 접근 · 승인 없음"));
        app.preferences.language = Language::English;
        let english = app.render(80, 24);
        assert!(english.plain_line(1).contains("Full access · Never ask"));
        assert!(!english.plain_line(1).contains("전체 접근"));
        app.preferences.language = Language::Korean;
        app.activate(Action::Command("/permissions".into()));
        let rows = app.content_rows_for(&View::Permissions, 80);
        assert!(rows.iter().any(|r| r.0.contains("이전에 신뢰됨")));
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
            images: vec![],
            skills: vec![],
        });
        let canvas = app.render(80, 24);
        let input = canvas
            .hits
            .iter()
            .find(|hit| hit.action == Action::Input)
            .unwrap();
        let status = usize::from(input.y - 1) * usize::from(canvas.width) + 2;
        assert_eq!(canvas.cells[status].text, "⠋");
        assert_eq!(canvas.cells[status].tone, Tone::Accent);
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
    #[ignore = "manual deep-scroll paint profiling"]
    fn profile_deep_scrollback_paint() {
        let mut rows: Vec<ContentRow> = Vec::new();
        for i in 0..4000 {
            rows.push(("─".repeat(76), Tone::User, None));
            rows.push((
                format!("› message {i}"),
                Tone::Normal,
                Some(Action::SelectEntry(format!("entry-{i}"))),
            ));
            rows.push((String::new(), Tone::Normal, None));
        }
        let cache = RefCell::new(HashMap::new());
        let bands = transcript_band_starts(&rows);
        let start = Instant::now();
        for _ in 0..200 {
            std::hint::black_box(paint_rows(
                &rows,
                rows.len() - 24,
                80,
                24,
                None,
                &cache,
                Some(&bands),
                None,
            ));
        }
        eprintln!("200 cached deep-scroll paints: {:?}", start.elapsed());
    }
    #[test]
    fn indexed_backgrounds_match_legacy_at_every_group_boundary() {
        let mut rows: Vec<ContentRow> = vec![("opening".into(), Tone::Muted, None)];
        for i in 0..75 {
            rows.push((
                "─".repeat(76),
                if i == 0 { Tone::Faint } else { Tone::User },
                None,
            ));
            rows.push((
                format!("● response {i}"),
                Tone::Normal,
                Some(Action::SelectEntry(format!("entry-{i}"))),
            ));
            rows.push(("─".repeat(76), Tone::Faint, None));
            rows.push((String::new(), Tone::Normal, None));
        }
        let cache = RefCell::new(HashMap::new());
        let starts = transcript_band_starts(&rows);
        assert_eq!(starts.len(), 75);
        for scroll in 0..rows.len() {
            let legacy = paint_rows(&rows, scroll, 80, 12, None, &cache, None, None);
            let indexed = paint_rows(&rows, scroll, 80, 12, None, &cache, Some(&starts), None);
            assert_eq!(
                legacy.cells, indexed.cells,
                "background or content changed at row {scroll}"
            );
            let hit_data = |canvas: &Canvas| {
                canvas
                    .hits
                    .iter()
                    .map(|hit| {
                        (
                            hit.x,
                            hit.y,
                            hit.width,
                            hit.height,
                            format!("{:?}", hit.action),
                        )
                    })
                    .collect::<Vec<_>>()
            };
            assert_eq!(
                hit_data(&legacy),
                hit_data(&indexed),
                "click target changed at row {scroll}"
            );
        }
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
        let key = ("response".into(), 74);
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
                .contains_key(&("response".into(), 114))
        );
        a.activate(Action::Panel);
        a.render(120, 24);
        assert!(
            a.message_cache
                .borrow()
                .contains_key(&("response".into(), 73))
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
        a.activate(Action::Panel);
        a.agents = (0..30)
            .map(|i| Agent {
                id: format!("agent-{i}"),
                name: format!("Agent {i}"),
                status: "working".into(),
                detail: String::new(),
            })
            .collect();
        a.render(120, 16);
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
    fn compact_diff_renders_at_minimum_terminal_widths() {
        let mut a = App::new(true);
        a.diff = "+compact regression".into();
        a.activate(Action::Diff);
        for width in 40..=48 {
            let canvas = a.render(width, 24);
            assert_eq!(a.view, View::Diff);
            assert!(!a.wide_layout);
            assert!((0..24).any(|y| canvas.plain_line(y).contains("compact regression")));
            assert_eq!(canvas.cells.len(), usize::from(width) * 24);
            assert!(canvas.hits.iter().all(|hit| {
                usize::from(hit.x) + usize::from(hit.width) <= usize::from(width)
                    && usize::from(hit.y) + usize::from(hit.height) <= 24
            }));
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
            assert!(c.plain_line(1).starts_with("──"));
            let input_y = c
                .hits
                .iter()
                .find(|hit| hit.action == Action::Input)
                .unwrap()
                .y;
            assert!(c.plain_line(input_y - 1).starts_with("──"));
            for (action, tone) in [(Action::Back, Tone::Accent), (Action::Agents, Tone::Hover)] {
                let hit = c
                    .hits
                    .iter()
                    .find(|hit| hit.y == 0 && hit.action == action)
                    .unwrap();
                assert_eq!(
                    c.cells[usize::from(hit.y) * usize::from(w) + usize::from(hit.x)].tone,
                    tone
                );
            }
            assert!(
                !c.hits
                    .iter()
                    .any(|hit| hit.y == 0 && hit.action == Action::Diff)
            );
        }
        a.activate(Action::Diff);
        let c = a.render(120, 32);
        assert!(a.wide_layout, "diff review stays beside the conversation");
        assert_eq!(a.side_width(120), 60, "diff review expands the side pane");
        assert!(
            (0..32).any(|y| c.plain_line(y).contains("에이전트가 보고한 파일 변경")),
            "expanded side pane renders the diff review"
        );
        assert!(
            !c.hits
                .iter()
                .any(|hit| hit.y == 0 && hit.action == Action::Diff)
        );
    }
    #[test]
    fn numbered_commands_reach_mouse_actions_without_discarding_drafts() {
        let mut a = App::new(false);
        a.set_thread(&json!({"id":"main","turns":[
            {"id":"one","status":"completed","items":[{"id":"first-user","type":"userMessage","content":[{"type":"text","text":"first question"}]},{"id":"first","type":"agentMessage","text":"first response"}]},
            {"id":"two","status":"completed","items":[{"id":"second-user","type":"userMessage","content":[{"type":"text","text":"second question"}]},{"id":"second","type":"agentMessage","text":"second response"}]}
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

        a.entries.push(Entry {
            id: "one-work-log".into(),
            kind: Kind::Tool,
            title: "작업 기록".into(),
            body: "read src/main.rs\nedit src/main.rs".into(),
            status: "completed".into(),
            expanded: false,
        });
        a.entry_turns.insert("one-work-log".into(), "one".into());
        assert_eq!(
            a.command("/copy 2"),
            Some(Intent::Copy(
                "[User]\nfirst question\n\n[Assistant]\nfirst response\n\n[Tool · 작업 기록]\nread src/main.rs\nedit src/main.rs".into()
            ))
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
                images: vec![],
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
        assert!(screen.contains("대화 기록 유지") && screen.contains("하위 에이전트"));
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
        for _ in 0..COMMANDS
            .iter()
            .position(|(name, _)| *name == "/skills")
            .unwrap()
        {
            a.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE), &c);
        }
        // Tab completes the selected command; Enter would run it.
        assert_eq!(
            a.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &c),
            None
        );
        assert_eq!(a.view, View::Chat);
        assert_eq!(a.editor.text, "/skills");
        assert!(a.submitted.is_none());

        let c = a.render(80, 24);
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
    fn skill_completion_preserves_cursor_suffix_and_allows_a_second_skill() {
        let mut app = App::new(false);
        app.thread = Some("main".into());
        app.skills = ["review", "test"].into_iter().map(|name| Skill {
            name: name.into(), path: format!("/tmp/{name}/SKILL.md"), description: String::new(),
        }).collect();
        app.editor.insert("앞 $rev 뒤");
        app.editor.cursor = "앞 $rev".len();
        let canvas = app.render(80, 24);
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &canvas);
        assert_eq!(app.editor.text, "앞  뒤");
        assert_eq!(app.editor.cursor, "앞 ".len());
        for c in "$test".chars() {
            let canvas = app.render(80, 24);
            app.key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE), &canvas);
        }
        let canvas = app.render(80, 24);
        app.key(KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE), &canvas);
        assert_eq!(app.selected_skills.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(), ["review", "test"]);
        assert_eq!(app.editor.text, "앞  뒤");
        app.editor.take_text();
        app.editor.insert("$reviewer `$review` \\$test $HOME");
        app.selected_skills.clear();
        app.activate(Action::Send);
        assert!(app.submitted_skills.is_empty());
    }

    #[test]
    fn slash_skill_alias_preserves_builtin_priority_and_readonly_drafts() {
        let mut app = App::new(false);
        app.thread = Some("main".into());
        app.skills = ["review", "help"].into_iter().map(|name| Skill {
            name: name.into(), path: format!("/tmp/{name}/SKILL.md"), description: String::new(),
        }).collect();
        app.editor.insert("/help");
        app.activate(Action::Send);
        assert_eq!(app.view, View::Help);
        assert!(app.submitted.is_none());
        app.readonly = true;
        app.editor.insert("/review 변경 검토");
        let draft = app.editor.text.clone();
        assert!(app.activate(Action::Send).is_none());
        assert_eq!(app.editor.text, draft);
        assert!(app.submitted_skills.is_empty());
    }

    #[test]
    fn skills_view_groups_by_origin_and_keeps_selection_easy_to_find() {
        let skills = vec![
            Skill {
                name: "cloudflare:wrangler".into(),
                path: "/home/me/.codex/plugins/cache/cloudflare/0.1/skills/wrangler/SKILL.md"
                    .into(),
                description:
                    "A very long description that should be shortened on a narrow terminal screen"
                        .into(),
            },
            Skill {
                name: "my-skill".into(),
                path: "/home/me/.agents/skills/my-skill/SKILL.md".into(),
                description: "Personal tool".into(),
            },
            Skill {
                name: "system".into(),
                path: "/home/me/.codex/skills/.system/system/SKILL.md".into(),
                description: "Built in".into(),
            },
            Skill {
                name: "selected".into(),
                path: "/demo/selected/SKILL.md".into(),
                description: "Already chosen".into(),
            },
        ];
        let rows = skill_rows(&skills, &skills[3..], &[], true, Language::Korean, 36);
        let text = rows.iter().map(|row| row.0.as_str()).collect::<Vec<_>>();
        let at = |part: &str| text.iter().position(|line| line.contains(part)).unwrap();
        assert!(at("선택됨 (1)") < at("기본 제공 (1)"));
        assert!(at("기본 제공 (1)") < at("직접 추가 (1)"));
        assert!(at("직접 추가 (1)") < at("확장 · cloudflare (1)"));
        assert!(text[at("selected")].contains('×'));
        assert_eq!(
            text.iter()
                .filter(|line| line.contains("$selected"))
                .count(),
            1
        );
        assert!(text.iter().any(|line| line.contains("…")));
        assert!(text.iter().all(|line| UnicodeWidthStr::width(*line) <= 36));

        let english = skill_rows(&skills, &[], &[], true, Language::English, 36);
        assert!(english.iter().any(|row| row.0.contains("Built-in (1)")));
        assert!(
            english
                .iter()
                .any(|row| row.0.contains("Plugins · cloudflare (1)"))
        );

        let mut app = App::new(false);
        app.skills = skills;
        app.skills_loaded = true;
        app.panel_open = true;
        app.switch_view(View::Skills);
        let canvas = app.render(120, 40);
        assert!(
            canvas
                .hits
                .iter()
                .any(|hit| { hit.action == Action::Skill(app.skills[0].path.clone()) })
        );
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
    fn up_arrow_recalls_latest_queued_message_for_editing() {
        let mut a = App::new(false);
        a.thread = Some("main".into());
        a.turn = Some("running".into());
        a.busy = true;

        a.model = "model-a".into();
        a.effort = Some("medium".into());
        a.selected_skills.push(Skill {
            name: "review".into(),
            path: "/review/SKILL.md".into(),
            description: String::new(),
        });
        a.editor.insert("first");
        a.activate(Action::Send);

        a.model = "model-b".into();
        a.effort = Some("high".into());
        a.selected_skills.push(Skill {
            name: "edit".into(),
            path: "/edit/SKILL.md".into(),
            description: String::new(),
        });
        a.editor.insert("second");
        a.activate(Action::Send);

        let canvas = a.render(80, 24);
        a.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &canvas);

        assert_eq!(a.editor.text, "second");
        assert_eq!(a.queued.len(), 1);
        assert_eq!(a.queued[0].text, "first");
        assert_eq!(a.model, "model-b");
        assert_eq!(a.effort.as_deref(), Some("high"));
        assert_eq!(a.selected_skills.len(), 1);
        assert_eq!(a.selected_skills[0].name, "edit");

        let canvas = a.render(80, 24);
        a.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE), &canvas);
        assert_eq!(a.editor.text, "second");
        assert_eq!(a.queued.len(), 1);
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
            Some(Intent::Copy("줄 하나\n줄 둘".into()))
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
    fn submitting_while_approval_pending_queues_without_backend_attempt() {
        let mut a = App::new(false);
        a.thread = Some("main".into());
        assert!(a.server_request(
            json!(1),
            "item/commandExecution/requestApproval",
            &json!({"threadId":"main","command":"echo test","availableDecisions":["decline"]})
        ));
        a.editor.insert("after approval");
        assert!(a.activate(Action::Send).is_none());
        assert_eq!(a.queued.len(), 1);
        assert_eq!(a.queued[0].text, "after approval");
        assert!(a.submitted.is_none());
        assert!(!a.queue_paused);
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
        let c = a.render(80, 60);
        assert!(c.plain_line(0).contains("● Settings"));
        let screen = (0..c.height)
            .map(|y| c.plain_line(y))
            .collect::<Vec<_>>()
            .join("\n");
        assert!(screen.contains("Display") && screen.contains("Overview panel"));
        let settings = a
            .content_rows_for(&View::Settings, 76)
            .iter()
            .map(|row| row.0.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(settings.contains("Language") && settings.contains("Overview panel"));
    }
    #[test]
    fn settings_panel_command_applies_preference_and_actual_layout() {
        let mut app = App::new(false);
        app.editor.insert("unsent draft");
        assert!(!app.panel_open && !app.preferences.panel_open);
        assert_eq!(app.command("/settings panel"), Some(Intent::SaveSettings));
        assert!(app.panel_open && app.preferences.panel_open);
        assert_eq!(app.view, View::Settings);
        app.render(120, 36);
        assert!(app.wide_layout);
        assert_eq!(app.editor.text, "unsent draft");
        assert_eq!(app.command("/settings panel"), Some(Intent::SaveSettings));
        assert!(!app.panel_open && !app.preferences.panel_open);
        assert_eq!(app.view, View::Chat);
        app.render(120, 36);
        assert!(!app.wide_layout);
        assert_eq!(app.editor.text, "unsent draft");
    }
    #[test]
    fn chat_uses_full_width_by_default_and_manual_sidebar_has_structured_overview() {
        let mut app = App::new(false);
        app.editor.insert("초안");
        app.entries.push(Entry {
            id: "last-tool".into(),
            kind: Kind::Tool,
            title: "혼란스러운 내부 명령 줄".into(),
            body: "details".into(),
            status: "completed".into(),
            expanded: false,
        });
        let initial = app.render(120, 32);
        assert!(!app.wide_layout);
        assert!(!initial.plain_line(1).contains("접기"));
        app.activate(Action::Panel);
        let open = app.render(120, 32);
        assert!(app.wide_layout);
        let content = app
            .overview_rows(40)
            .iter()
            .map(|r| r.0.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(content.contains("현재 상태") && content.contains("바로가기"));
        assert!(!content.contains("혼란스러운 내부 명령 줄"));
        assert!(open.plain_line(1).contains("접기"));
        app.activate(Action::Agents);
        app.activate(Action::Back);
        app.render(120, 32);
        // Returning from another tab preserves the explicitly shown activity panel.
        assert!(app.wide_layout && app.panel_open && app.preferences.activity_panel);
        app.activate(Action::Panel);
        app.render(120, 32);
        assert!(!app.wide_layout && !app.panel_open && !app.preferences.activity_panel);
        assert_eq!(app.editor.text, "초안");
    }
    #[test]
    fn full_width_chat_keeps_change_shortcut_clear_of_panel_toggle() {
        let mut app = App::new(false);
        app.diff = "+modified".into();
        let screen = app.render(120, 32);
        let changes = screen
            .hits
            .iter()
            .find(|hit| hit.action == Action::Diff)
            .unwrap();
        let panel = screen
            .hits
            .iter()
            .find(|hit| hit.action == Action::Panel)
            .unwrap();
        assert_eq!(changes.y, 1);
        assert_eq!(panel.y, 1);
        assert!(changes.x + changes.width < panel.x);
        let header = screen.plain_line(1);
        assert!(header.contains("파일 변경") && header.contains("패널 열기"));
    }
    #[test]
    fn work_list_and_usage_have_separate_content() {
        let mut app = App::new(false);
        app.activate(Action::Agents);
        let summary = app
            .content_rows_for(&View::Agents, 38)
            .iter()
            .map(|r| r.0.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            summary.contains("● 목록")
                && summary.contains("○ 사용량")
                && summary.contains("하위 에이전트")
        );
        assert!(!summary.contains("토큰·할당량 상세") && !summary.contains("Context"));
        assert!(!summary.contains("Total tokens:"));
        app.command("/usage");
        assert!(app.usage_expanded);
        let details = app
            .content_rows_for(&View::Agents, 38)
            .iter()
            .map(|r| r.0.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(details.contains("아직 집계된 토큰 없음"));
        assert!(!details.contains("하위 에이전트") && !details.contains("현재 연결된"));
        app.activate(Action::Agents);
        assert!(!app.usage_expanded);
    }
    #[test]
    fn completed_messages_do_not_reserve_or_display_branch_controls() {
        let mut a = App::new(false);
        a.set_thread(&json!({"id":"source","turns":[{"id":"turn","status":"completed","items":[{"id":"message","type":"agentMessage","text":"분기 대상"}]}]}));
        let before = a.render(80, 24);
        assert!(before.hits.iter().all(|hit| !matches!(hit.action, Action::Branch(_))));
        let message_y = before.hits.iter().find(|hit| hit.action == Action::SelectEntry("message".into())).unwrap().y;
        assert!(before.plain_line(message_y).contains("분기 대상"));
        a.activate(Action::SelectEntry("message".into()));
        let after = a.render(80, 24);
        assert_eq!(before.cursor, after.cursor);
        assert_eq!(
            after.hits.iter().find(|hit| hit.action == Action::SelectEntry("message".into())).unwrap().y,
            message_y
        );
        assert!(after.hits.iter().all(|hit| !matches!(hit.action, Action::Branch(_))));
        assert!(!(0..after.height).any(|y| after.plain_line(y).contains("이 대화까지 새 분기")));
    }
    #[test]
    fn streaming_and_readonly_messages_offer_drag_copy_without_copy_buttons() {
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
                .any(|h| h.action == Action::SelectEntry("stream".into()))
        );
        assert!(
            !live
                .hits
                .iter()
                .any(|h| h.action == Action::Branch("stream".into()))
        );
        assert!(
            live.hits
                .iter()
                .all(|h| !matches!(h.action, Action::Copy(_)))
        );
        a.readonly = true;
        let readonly = a.render(80, 24);
        assert!(
            readonly
                .hits
                .iter()
                .any(|h| h.action == Action::SelectEntry("stream".into()))
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
        assert!(canvas.plain_line(0).contains("세션"));
        assert!(!canvas.plain_line(0).contains("기록"));

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
        app.set_thread(&json!({"id":"saved","preview":"Older thread"}));
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
            let (meta_x, meta_y) = if width == 80 {
                (width - 9, row_y)
            } else {
                (row_x.saturating_add(1), row_y + 1)
            };
            assert_eq!(
                canvas.hit(meta_x, meta_y),
                Some(action.clone()),
                "the metadata must switch the same session at either layout width"
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

    #[test]
    fn request_pasted_after_a_blank_line_still_names_the_conversation() {
        let mut app = App::new(false);
        app.set_thread(&json!({"id":"new"}));
        app.upsert(&json!({"id":"user","type":"userMessage","content":[{"text":"\n  \n  Fix   the parser\nmore"}]}));
        assert_eq!(app.session_title, "Fix the parser");
    }
}
