//! Terminal cells, incremental painting and hit regions, independent of agent semantics.
use std::io::{self, Write};

use crate::preferences::Accent;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::style::{
    Attribute, Attributes, Color, Print, ResetColor, SetAttribute, SetAttributes,
    SetBackgroundColor, SetForegroundColor,
};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute, queue};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

struct Palette {
    accent: Color,
    user: Color,
    success: Color,
    danger: Color,
    warning: Color,
}

impl Accent {
    fn palette(self) -> Palette {
        let colors = match self {
            Self::Cyan => [
                (117, 200, 230),
                (172, 190, 245),
                (125, 214, 161),
                (242, 137, 137),
                (244, 201, 123),
            ],
            Self::White => [
                (240, 244, 248),
                (209, 218, 231),
                (190, 235, 216),
                (255, 173, 181),
                (242, 222, 170),
            ],
            Self::Hacker => [
                (57, 255, 20),
                (108, 245, 194),
                (179, 255, 117),
                (255, 87, 117),
                (220, 255, 0),
            ],
            Self::Purple => [
                (192, 156, 255),
                (212, 184, 255),
                (145, 230, 207),
                (255, 143, 184),
                (255, 211, 153),
            ],
            Self::Amber => [
                (255, 193, 87),
                (244, 203, 156),
                (185, 220, 124),
                (255, 128, 101),
                (255, 222, 132),
            ],
        }
        .map(|(r, g, b)| Color::Rgb { r, g, b });
        Palette {
            accent: colors[0],
            user: colors[1],
            success: colors[2],
            danger: colors[3],
            warning: colors[4],
        }
    }

    fn selection_background(self, hover: bool) -> Color {
        if self == Self::Cyan {
            return if hover {
                Background::Hover.color()
            } else {
                Background::Selected.color()
            };
        }
        let Color::Rgb { r, g, b } = self.palette().accent else {
            unreachable!()
        };
        let tint = |value: u8| 24 + value / if hover { 10 } else { 7 };
        Color::Rgb {
            r: tint(r),
            g: tint(g),
            b: tint(b),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tone {
    #[default]
    Normal,
    Strong,
    Emphasis,
    StrongEmphasis,
    Strike,
    Underline,
    Math,
    InlineCode,
    Muted,
    Faint,
    Accent,
    User,
    Success,
    Danger,
    Added,
    Removed,
    Warning,
    Selected,
    Hover,
    Code,
    CodeAdded,
    CodeRemoved,
    Keyword,
    String,
    Number,
    Comment,
    Function,
    /// QR code modules: fixed black and white whatever the terminal theme.
    QrLight,
    QrDark,
}
impl Tone {
    pub fn color(self, accent: Accent) -> Color {
        let palette = accent.palette();
        match self {
            Self::Normal | Self::Code | Self::CodeAdded | Self::CodeRemoved => Color::Reset,
            Self::Muted | Self::Strike => Color::Rgb {
                r: 165,
                g: 173,
                b: 182,
            },
            Self::Faint => Color::Rgb {
                r: 106,
                g: 117,
                b: 130,
            },
            Self::Accent | Self::Strong | Self::StrongEmphasis | Self::Underline | Self::Math => {
                palette.accent
            }
            Self::User | Self::Emphasis => palette.user,
            Self::Success => palette.success,
            Self::Danger => palette.danger,
            Self::Added => {
                if accent == Accent::Cyan {
                    Color::Green
                } else {
                    palette.success
                }
            }
            Self::Removed => {
                if accent == Accent::Cyan {
                    Color::Red
                } else {
                    palette.danger
                }
            }
            Self::Warning => palette.warning,
            Self::Selected | Self::Hover => {
                if accent == Accent::Cyan {
                    Color::White
                } else {
                    palette.accent
                }
            }
            Self::Keyword => Color::Cyan,
            Self::String => Color::Green,
            Self::InlineCode => Color::Rgb {
                r: 186,
                g: 232,
                b: 175,
            },
            Self::Number => Color::Magenta,
            Self::Comment => Color::DarkGrey,
            Self::Function => Color::Yellow,
            Self::QrLight => Color::Rgb { r: 255, g: 255, b: 255 },
            Self::QrDark => Color::Rgb { r: 0, g: 0, b: 0 },
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Background {
    #[default]
    Default,
    ResponseA,
    ResponseB,
    Code,
    Quote,
    Added,
    Removed,
    Selected,
    Hover,
    QrLight,
    QrDark,
}
impl Background {
    fn color(self) -> Color {
        match self {
            Self::Default => Color::Reset,
            Self::QrLight => Color::Rgb { r: 255, g: 255, b: 255 },
            Self::QrDark => Color::Rgb { r: 0, g: 0, b: 0 },
            Self::ResponseA => Color::Rgb {
                r: 18,
                g: 24,
                b: 30,
            },
            Self::ResponseB => Color::Rgb {
                r: 20,
                g: 26,
                b: 32,
            },
            Self::Code => Color::Rgb {
                r: 49,
                g: 56,
                b: 65,
            },
            Self::Quote => Color::Rgb {
                r: 31,
                g: 40,
                b: 50,
            },
            Self::Added => Color::Rgb {
                r: 15,
                g: 48,
                b: 24,
            },
            Self::Removed => Color::Rgb {
                r: 57,
                g: 22,
                b: 26,
            },
            Self::Selected => Color::Rgb {
                r: 66,
                g: 69,
                b: 73,
            },
            Self::Hover => Color::Rgb {
                r: 55,
                g: 58,
                b: 62,
            },
        }
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Cell {
    pub text: String,
    pub tone: Tone,
    pub background: Background,
    pub continuation: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Effort(String),
    Task(String),
    Questions,
    QuestionOption(usize),
    QuestionMove(bool),
    SubmitAnswers,
    DismissQuestion,
    CancelQuestions,
    Toggle(String),
    OutputPage(String, bool),
    Agents,
    Diff,
    DiffEntry(String),
    Back,
    Agent(usize),
    OpenAgent(usize),
    /// Switch into the session that carries out a task; the way back is remembered.
    EnterSession(String),
    /// Open a session read-only in a new window (works while the current turn runs).
    OpenThread(String),
    /// Return to the session the user entered from.
    ReturnSession,
    Approvals,
    Decide(String, usize),
    ToggleApproval(String),
    Send,
    Interrupt,
    Input,
    Copy(String),
    Quit,
    Panel,
    Menu,
    Command(String),
    Skill(String),
    RemoveSkill(String),
    RemoveImage(usize),
    RemoveQueued(usize),
    ForceQueued(usize),
    QueueViewport,
    ResumeQueue,
    Model(String),
    ModelSearch,
    ClearModelSearch,
    ModelGrouping,
    ModelFreeOnly,
    ToggleModelDetails,
    /// Open or close one provider group in the model list.
    ToggleModelGroup(String),
    /// Invisible hit area marking text that belongs to the transcript and can be dragged/copied.
    TranscriptText,
    SelectEntry(String),
    OpenLink(String),
    MessageRow(String, usize, usize),
    Branch(String),
    Rewind(String),
    ConfirmRewind,
    ConfirmConversationRewind,
    RewindDelta(usize, usize),
    Resume(String),
    Sessions,
    NewSession,
    RefreshSessions,
    MoreSessions,
    Setting(String),
    SettingChoices(Vec<(String, String, bool)>),
    Permission(String, String),
    ConfirmPermission(bool),
    DismissApproval,
    TrustWorkspace(bool),
}
#[derive(Clone, Debug)]
pub struct Hit {
    pub x: u16,
    pub y: u16,
    pub width: u16,
    pub height: u16,
    pub action: Action,
}
#[derive(Clone, Debug)]
pub struct Canvas {
    pub width: u16,
    pub height: u16,
    pub accent: Accent,
    pub cells: Vec<Cell>,
    pub hits: Vec<Hit>,
    pub cursor: Option<(u16, u16)>,
    /// Composer cursor byte offsets, indexed by terminal cell.
    pub input_offsets: Vec<Option<usize>>,
}
impl Canvas {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            accent: Accent::default(),
            cells: vec![
                Cell {
                    text: " ".into(),
                    ..Cell::default()
                };
                usize::from(width) * usize::from(height)
            ],
            hits: Vec::new(),
            cursor: None,
            input_offsets: vec![None; usize::from(width) * usize::from(height)],
        }
    }
    pub fn text(&mut self, x: u16, y: u16, text: &str, tone: Tone) -> u16 {
        if y >= self.height {
            return x;
        }
        let mut col = x;
        for grapheme in text.graphemes(true) {
            if grapheme.chars().any(char::is_control) {
                continue;
            }
            let w = UnicodeWidthStr::width(grapheme).min(u16::MAX as usize) as u16;
            if w == 0 {
                continue;
            }
            if col.saturating_add(w) > self.width {
                break;
            }
            let idx = usize::from(y) * usize::from(self.width) + usize::from(col);
            self.cells[idx] = Cell {
                text: grapheme.into(),
                tone,
                continuation: false,
                background: self.cells[idx].background,
            };
            for offset in 1..w {
                self.cells[idx + usize::from(offset)] = Cell {
                    text: String::new(),
                    tone,
                    continuation: true,
                    background: self.cells[idx].background,
                };
            }
            col += w;
        }
        col
    }
    pub fn button(&mut self, x: u16, y: u16, label: &str, action: Action, selected: bool) -> u16 {
        let text = if selected {
            format!(" › {label} ")
        } else {
            format!(" {label} ")
        };
        let tone = if selected {
            Tone::Accent
        } else {
            match action {
                Action::Send | Action::ResumeQueue => Tone::Success,
                Action::Interrupt | Action::RemoveQueued(_) => Tone::Danger,
                Action::ForceQueued(_) => Tone::Warning,
                _ => Tone::Muted,
            }
        };
        let end = self.text(x, y, &text, tone);
        if end > x {
            self.hits.push(Hit {
                x,
                y,
                width: end - x,
                height: 1,
                action,
            });
        }
        end.saturating_add(1)
    }
    pub fn blit(&mut self, child: &Canvas, x: u16, y: u16) {
        for row in 0..child.height.min(self.height.saturating_sub(y)) {
            for col in 0..child.width.min(self.width.saturating_sub(x)) {
                self.cells[usize::from(y + row) * usize::from(self.width) + usize::from(x + col)] =
                    child.cells[usize::from(row) * usize::from(child.width) + usize::from(col)]
                        .clone();
                self.input_offsets
                    [usize::from(y + row) * usize::from(self.width) + usize::from(x + col)] = child
                    .input_offsets[usize::from(row) * usize::from(child.width) + usize::from(col)];
            }
        }
        for hit in &child.hits {
            if x + hit.x + hit.width <= self.width && y + hit.y + hit.height <= self.height {
                let mut hit = hit.clone();
                hit.x += x;
                hit.y += y;
                self.hits.push(hit);
            }
        }
    }
    /// Paints a QR code (`dark[y][x]`), two modules per cell with upper-half blocks, in
    /// fixed black and white so phones read it on light and dark terminals alike.
    pub fn qr(&mut self, x: u16, y: u16, dark: &[Vec<bool>]) {
        for (row, pair) in dark.chunks(2).enumerate() {
            let cy = y + row as u16;
            if cy >= self.height {
                break;
            }
            for (col, &top) in pair[0].iter().enumerate() {
                let cx = x + col as u16;
                if cx >= self.width {
                    break;
                }
                let bottom = pair.get(1).is_some_and(|r| r[col]);
                let cell = &mut self.cells[usize::from(cy) * usize::from(self.width) + usize::from(cx)];
                cell.text = "▀".into();
                cell.continuation = false;
                cell.tone = if top { Tone::QrDark } else { Tone::QrLight };
                cell.background = if bottom { Background::QrDark } else { Background::QrLight };
            }
        }
    }
    pub fn rule(&mut self, y: u16, title: &str) {
        self.text(0, y, &"─".repeat(usize::from(self.width)), Tone::Faint);
        if !title.is_empty() {
            self.text(2, y, &format!(" {title} "), Tone::Accent);
        }
    }
    pub fn highlight(&mut self, action: &Action, tone: Tone) {
        for hit in self.hits.iter().filter(|hit| &hit.action == action) {
            for y in hit.y..(hit.y + hit.height).min(self.height) {
                for x in hit.x..(hit.x + hit.width).min(self.width) {
                    let cell =
                        &mut self.cells[usize::from(y) * usize::from(self.width) + usize::from(x)];
                    // A selected/hovered message keeps its Markdown foreground
                    // and terminal attributes; selection paints the backdrop.
                    if matches!(action, Action::SelectEntry(_)) {
                        cell.background = if tone == Tone::Hover {
                            Background::Hover
                        } else {
                            Background::Selected
                        };
                    } else {
                        cell.tone = tone;
                    }
                }
            }
        }
    }
    /// The action under a cell. Selectable text only answers where nothing acts.
    pub fn hit(&self, x: u16, y: u16) -> Option<Action> {
        let over = |h: &&Hit| {
            x >= h.x && x < h.x.saturating_add(h.width) && y >= h.y && y < h.y.saturating_add(h.height)
        };
        let mut covering = self.hits.iter().rev().filter(over);
        let first = covering.clone().next()?;
        covering
            .find(|h| h.action != Action::TranscriptText)
            .or(Some(first))
            .map(|h| h.action.clone())
    }
    pub fn plain_line(&self, y: u16) -> String {
        if y >= self.height {
            return String::new();
        }
        self.cells
            [usize::from(y) * usize::from(self.width)..usize::from(y + 1) * usize::from(self.width)]
            .iter()
            .filter(|c| !c.continuation)
            .map(|c| c.text.as_str())
            .collect()
    }
}
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    wrap_each(text, width, &mut |line| {
        lines.push(line);
        true
    });
    lines
}

/// Emit one wrapped row at a time so paged tool output can stop immediately.
pub fn wrap_each(text: &str, width: usize, emit: &mut dyn FnMut(String) -> bool) -> bool {
    if width == 0 {
        return true;
    }
    for source in text.split('\n') {
        let mut line = String::new();
        let mut used = 0;
        for g in source.graphemes(true) {
            if g == "\t" {
                for _ in 0..4 {
                    if used == width {
                        if !emit(std::mem::take(&mut line)) {
                            return false;
                        }
                        used = 0;
                    }
                    line.push(' ');
                    used += 1;
                }
                continue;
            }
            if g.chars().any(char::is_control) {
                continue;
            }
            let w = UnicodeWidthStr::width(g);
            if w > width {
                continue;
            }
            if used + w > width {
                if !emit(std::mem::take(&mut line)) {
                    return false;
                }
                used = 0;
            }
            line.push_str(g);
            used += w;
        }
        if !emit(line) {
            return false;
        }
    }
    true
}

pub struct Terminal {
    previous: Option<(u16, u16, Accent, Vec<Cell>)>,
}
struct PaintRun {
    x: u16,
    end: u16,
    tone: Tone,
    foreground: Color,
    background: Color,
    text: String,
}

fn background(cell: &Cell, accent: Accent) -> Color {
    match cell.tone {
        Tone::Selected => accent.selection_background(false),
        Tone::Hover => accent.selection_background(true),
        _ => match cell.background {
            Background::Selected => accent.selection_background(false),
            Background::Hover => accent.selection_background(true),
            _ => cell.background.color(),
        },
    }
}

fn flush_run(output: &mut impl Write, run: &mut Option<PaintRun>, y: u16) -> io::Result<()> {
    if let Some(run) = run.take() {
        let mut attributes = Attributes::default();
        if matches!(run.tone, Tone::Strong | Tone::StrongEmphasis) {
            attributes.set(Attribute::Bold);
        }
        if matches!(run.tone, Tone::Emphasis | Tone::StrongEmphasis) {
            attributes.set(Attribute::Italic);
            // Underline stays visible on terminals that ignore italic.
            attributes.set(Attribute::Underlined);
        }
        if run.tone == Tone::Strike {
            attributes.set(Attribute::CrossedOut);
        }
        if run.tone == Tone::Underline {
            attributes.set(Attribute::Underlined);
        }
        queue!(
            output,
            cursor::MoveTo(run.x, y),
            SetAttribute(Attribute::Reset),
            SetAttributes(attributes),
            SetForegroundColor(run.foreground),
            SetBackgroundColor(run.background),
            Print(run.text)
        )?;
    }
    Ok(())
}
impl Terminal {
    pub fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        if let Err(error) = execute!(
            io::stdout(),
            EnterAlternateScreen,
            EnableMouseCapture,
            EnableBracketedPaste,
            PushKeyboardEnhancementFlags(
                KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                    | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS,
            ),
            cursor::Hide
        ) {
            restore();
            return Err(error);
        }
        let ui_thread = std::thread::current().id();
        let prior = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            // A caught worker panic must not leave the UI's alternate screen.
            if should_restore_on_panic(ui_thread) {
                restore();
            }
            prior(info);
        }));
        Ok(Self { previous: None })
    }
    pub fn paint(&mut self, canvas: &Canvas) -> io::Result<()> {
        let mut output = io::stdout().lock();
        self.paint_to(canvas, &mut output)
    }
    fn paint_to(&mut self, canvas: &Canvas, output: &mut impl Write) -> io::Result<()> {
        let old = self.previous.as_ref().filter(|(w, h, accent, _)| {
            *w == canvas.width && *h == canvas.height && *accent == canvas.accent
        });
        queue!(output, cursor::Hide)?;
        for y in 0..canvas.height {
            let mut run: Option<PaintRun> = None;
            for x in 0..canvas.width {
                let idx = usize::from(y) * usize::from(canvas.width) + usize::from(x);
                let cell = &canvas.cells[idx];
                if cell.continuation {
                    continue;
                }
                if old.is_some_and(|(_, _, _, cells)| cells[idx] == *cell) {
                    flush_run(output, &mut run, y)?;
                    continue;
                }
                let foreground = cell.tone.color(canvas.accent);
                let background = background(cell, canvas.accent);
                let end = x.saturating_add(UnicodeWidthStr::width(cell.text.as_str()) as u16);
                if let Some(active) = run.as_mut()
                    && active.end == x
                    && active.tone == cell.tone
                    && active.foreground == foreground
                    && active.background == background
                {
                    active.end = end;
                    active.text.push_str(&cell.text);
                } else {
                    flush_run(output, &mut run, y)?;
                    run = Some(PaintRun {
                        x,
                        end,
                        tone: cell.tone,
                        foreground,
                        background,
                        text: cell.text.clone(),
                    });
                }
            }
            flush_run(output, &mut run, y)?;
        }
        queue!(output, SetAttribute(Attribute::Reset), ResetColor)?;
        if let Some((x, y)) = canvas.cursor {
            queue!(output, cursor::MoveTo(x, y), cursor::Show)?;
        }
        output.flush()?;
        self.previous = Some((
            canvas.width,
            canvas.height,
            canvas.accent,
            canvas.cells.clone(),
        ));
        Ok(())
    }
    pub fn invalidate(&mut self) {
        self.previous = None;
    }
}
fn should_restore_on_panic(ui_thread: std::thread::ThreadId) -> bool {
    std::thread::current().id() == ui_thread
}

fn restore() {
    let _ = execute!(
        io::stdout(),
        SetAttribute(Attribute::Reset),
        ResetColor,
        cursor::Show,
        DisableBracketedPaste,
        PopKeyboardEnhancementFlags,
        DisableMouseCapture,
        LeaveAlternateScreen
    );
    let _ = terminal::disable_raw_mode();
}
impl Drop for Terminal {
    fn drop(&mut self) {
        restore();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn panic_restoration_is_owned_by_ui_thread() {
        let ui_thread = std::thread::current().id();
        assert!(should_restore_on_panic(ui_thread));
        std::thread::spawn(move || {
            let mut restore_requested = false;
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                restore_requested = should_restore_on_panic(ui_thread);
                panic!("caught worker panic");
            }));
            assert!(result.is_err());
            assert!(!restore_requested, "a worker panic must not restore the UI");
        })
        .join()
        .unwrap();
        assert!(should_restore_on_panic(ui_thread));
    }
    #[test]
    fn palette_change_repaints_unchanged_cells_and_status_colors() {
        let mut terminal = Terminal { previous: None };
        let mut canvas = Canvas::new(7, 1);
        let tones = [
            Tone::Accent,
            Tone::Success,
            Tone::Warning,
            Tone::Danger,
            Tone::User,
            Tone::Selected,
            Tone::Hover,
        ];
        for (x, tone) in tones.into_iter().enumerate() {
            canvas.text(x as u16, 0, "X", tone);
        }
        terminal.paint_to(&canvas, &mut Vec::new()).unwrap();
        for accent in [
            Accent::White,
            Accent::Hacker,
            Accent::Purple,
            Accent::Amber,
            Accent::Cyan,
        ] {
            canvas.accent = accent;
            let mut output = Vec::new();
            terminal.paint_to(&canvas, &mut output).unwrap();
            let paint = String::from_utf8(output).unwrap();
            assert_eq!(
                paint.matches('X').count(),
                7,
                "{accent:?}: all unchanged cells must repaint"
            );
            for tone in tones {
                let mut sequence = Vec::new();
                queue!(sequence, SetForegroundColor(tone.color(accent))).unwrap();
                assert!(
                    paint.contains(&String::from_utf8(sequence).unwrap()),
                    "{accent:?} {tone:?}"
                );
            }
            let mut unchanged = Vec::new();
            terminal.paint_to(&canvas, &mut unchanged).unwrap();
            assert!(!String::from_utf8(unchanged).unwrap().contains('X'));
        }
    }
    #[test]
    fn wide_text_wraps_on_cells_without_splitting_graphemes() {
        assert_eq!(wrap("가나e\u{301}다", 4), vec!["가나", "e\u{301}다"]);
    }
    #[test]
    fn wrapping_a_very_long_line_can_stop_after_one_page() {
        let mut count = 0;
        let finished = wrap_each(&"x".repeat(1_000_000), 10, &mut |_| {
            count += 1;
            count < 25
        });
        assert!(!finished);
        assert_eq!(count, 25);
    }
    #[test]
    fn hit_regions_match_visible_cells_after_resize() {
        let mut c = Canvas::new(9, 2);
        c.button(1, 0, "도구", Action::Agents, false);
        assert_eq!(c.hit(6, 0), Some(Action::Agents));
        assert_eq!(c.hit(7, 0), None);
        let narrow = Canvas::new(3, 2);
        assert_eq!(narrow.hit(1, 0), None);
    }
    #[test]
    fn wide_cells_and_escape_controls_are_safe() {
        let mut c = Canvas::new(8, 1);
        c.text(0, 0, "가\u{1b}X", Tone::Normal);
        assert!(c.cells[1].continuation);
        assert_eq!(c.cells[2].text, "X");
        assert!(!c.plain_line(0).contains('\u{1b}'));
    }
    #[test]
    fn markdown_attributes_are_emitted_and_reset_between_runs() {
        let mut terminal = Terminal { previous: None };
        let mut canvas = Canvas::new(8, 1);
        canvas.text(0, 0, "B", Tone::Strong);
        canvas.text(1, 0, "I", Tone::Emphasis);
        canvas.text(2, 0, "S", Tone::Strike);
        canvas.text(3, 0, "X", Tone::StrongEmphasis);
        let mut output = Vec::new();
        terminal.paint_to(&canvas, &mut output).unwrap();
        let paint = String::from_utf8(output).unwrap();
        for attribute in ["\u{1b}[1m", "\u{1b}[3m", "\u{1b}[4m", "\u{1b}[9m"] {
            assert!(paint.contains(attribute));
        }
        assert!(paint.matches("\u{1b}[1m").count() >= 2);
        assert!(paint.matches("\u{1b}[0m").count() >= 4);
    }
    #[test]
    fn painting_batches_adjacent_changes_but_respects_wide_cells_and_styles() {
        let mut terminal = Terminal { previous: None };
        let mut canvas = Canvas::new(8, 1);
        canvas.text(0, 0, "가ab", Tone::Normal);
        canvas.text(4, 0, "C", Tone::Warning);
        let mut output = Vec::new();
        terminal.paint_to(&canvas, &mut output).unwrap();
        let paint = String::from_utf8(output).unwrap();
        assert!(paint.contains("가ab"));
        assert_eq!(paint.matches("\u{1b}[1;1H").count(), 1);
        assert_eq!(paint.matches("\u{1b}[1;5H").count(), 1);
        let mut output = Vec::new();
        terminal.paint_to(&canvas, &mut output).unwrap();
        assert!(!String::from_utf8(output).unwrap().contains("\u{1b}[1;1H"));
    }
}
