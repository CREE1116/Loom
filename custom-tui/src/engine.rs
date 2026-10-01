//! Terminal cells, incremental painting and hit regions, independent of agent semantics.
use std::io::{self, Write};

use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use crossterm::style::{Color, Print, ResetColor, SetBackgroundColor, SetForegroundColor};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
use crossterm::{cursor, execute, queue};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Tone {
    #[default]
    Normal,
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
}
impl Tone {
    fn color(self) -> Color {
        match self {
            Self::Normal | Self::Code | Self::CodeAdded | Self::CodeRemoved => Color::Reset,
            Self::Muted => Color::Rgb {
                r: 165,
                g: 173,
                b: 182,
            },
            Self::Faint => Color::Rgb {
                r: 106,
                g: 117,
                b: 130,
            },
            Self::Accent => Color::Rgb {
                r: 117,
                g: 200,
                b: 230,
            },
            Self::User => Color::Rgb {
                r: 172,
                g: 190,
                b: 245,
            },
            Self::Success => Color::Rgb {
                r: 125,
                g: 214,
                b: 161,
            },
            Self::Danger => Color::Rgb {
                r: 242,
                g: 137,
                b: 137,
            },
            Self::Added => Color::Green,
            Self::Removed => Color::Red,
            Self::Warning => Color::Rgb {
                r: 244,
                g: 201,
                b: 123,
            },
            Self::Selected | Self::Hover => Color::White,
            Self::Keyword => Color::Cyan,
            Self::String => Color::Green,
            Self::Number => Color::Magenta,
            Self::Comment => Color::DarkGrey,
            Self::Function => Color::Yellow,
        }
    }
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Background {
    #[default]
    Default,
    Code,
    Added,
    Removed,
}
impl Background {
    fn color(self) -> Color {
        match self {
            Self::Default => Color::Reset,
            Self::Code => Color::Rgb {
                r: 24,
                g: 28,
                b: 33,
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
    Questions,
    QuestionOption(usize),
    QuestionMove(bool),
    SubmitAnswers,
    DismissQuestion,
    Toggle(String),
    OutputPage(String, bool),
    Agents,
    Diff,
    DiffEntry(String),
    Back,
    Agent(usize),
    OpenAgent(usize),
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
    RemoveQueued(usize),
    ForceQueued(usize),
    ResumeQueue,
    Model(String),
    SelectEntry(String),
    Branch(String),
    Resume(String),
    Sessions,
    NewSession,
    RefreshSessions,
    MoreSessions,
    Setting(String),
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
    pub cells: Vec<Cell>,
    pub hits: Vec<Hit>,
    pub cursor: Option<(u16, u16)>,
}
impl Canvas {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            cells: vec![
                Cell {
                    text: " ".into(),
                    ..Cell::default()
                };
                usize::from(width) * usize::from(height)
            ],
            hits: Vec::new(),
            cursor: None,
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
                    self.cells[usize::from(y) * usize::from(self.width) + usize::from(x)].tone =
                        tone;
                }
            }
        }
    }
    pub fn hit(&self, x: u16, y: u16) -> Option<Action> {
        self.hits
            .iter()
            .rev()
            .find(|h| {
                x >= h.x
                    && x < h.x.saturating_add(h.width)
                    && y >= h.y
                    && y < h.y.saturating_add(h.height)
            })
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
    previous: Option<(u16, u16, Vec<Cell>)>,
}
struct PaintRun {
    x: u16,
    end: u16,
    foreground: Color,
    background: Color,
    text: String,
}

fn background(cell: &Cell) -> Color {
    match cell.tone {
        Tone::Selected => Color::Rgb {
            r: 66,
            g: 69,
            b: 73,
        },
        Tone::Hover => Color::Rgb {
            r: 55,
            g: 58,
            b: 62,
        },
        _ => cell.background.color(),
    }
}

fn flush_run(output: &mut impl Write, run: &mut Option<PaintRun>, y: u16) -> io::Result<()> {
    if let Some(run) = run.take() {
        queue!(
            output,
            cursor::MoveTo(run.x, y),
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
            cursor::Hide
        ) {
            restore();
            return Err(error);
        }
        let prior = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore();
            prior(info);
        }));
        Ok(Self { previous: None })
    }
    pub fn paint(&mut self, canvas: &Canvas) -> io::Result<()> {
        let mut output = io::stdout().lock();
        self.paint_to(canvas, &mut output)
    }
    fn paint_to(&mut self, canvas: &Canvas, output: &mut impl Write) -> io::Result<()> {
        let old = self
            .previous
            .as_ref()
            .filter(|(w, h, _)| *w == canvas.width && *h == canvas.height);
        queue!(output, cursor::Hide)?;
        for y in 0..canvas.height {
            let mut run: Option<PaintRun> = None;
            for x in 0..canvas.width {
                let idx = usize::from(y) * usize::from(canvas.width) + usize::from(x);
                let cell = &canvas.cells[idx];
                if cell.continuation {
                    continue;
                }
                if old.is_some_and(|(_, _, cells)| cells[idx] == *cell) {
                    flush_run(output, &mut run, y)?;
                    continue;
                }
                let foreground = cell.tone.color();
                let background = background(cell);
                let end = x.saturating_add(UnicodeWidthStr::width(cell.text.as_str()) as u16);
                if let Some(active) = run.as_mut()
                    && active.end == x
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
                        foreground,
                        background,
                        text: cell.text.clone(),
                    });
                }
            }
            flush_run(output, &mut run, y)?;
        }
        queue!(output, ResetColor)?;
        if let Some((x, y)) = canvas.cursor {
            queue!(output, cursor::MoveTo(x, y), cursor::Show)?;
        }
        output.flush()?;
        self.previous = Some((canvas.width, canvas.height, canvas.cells.clone()));
        Ok(())
    }
    pub fn invalidate(&mut self) {
        self.previous = None;
    }
}
fn restore() {
    let _ = execute!(
        io::stdout(),
        ResetColor,
        cursor::Show,
        DisableBracketedPaste,
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
