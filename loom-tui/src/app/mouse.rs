//! Mouse text selection and composer hit testing; buttons activate on release.
use super::*;
use crate::engine::Background;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};

const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(500);

#[derive(Default)]
pub(super) struct MouseState {
    selection: Option<TextSelection>,
    input_anchor: Option<usize>,
    input_word_anchor: Option<std::ops::Range<usize>>,
    input_line_anchor: Option<std::ops::Range<usize>>,
    pressed: Option<Action>,
    edge_scroll_tick: Option<Instant>,
    last_text_click: Option<TextClick>,
    last_input_click: Option<InputClick>,
}

impl MouseState {
    fn reset_gesture(&mut self) {
        self.selection = None;
        self.input_anchor = None;
        self.input_word_anchor = None;
        self.input_line_anchor = None;
        self.pressed = None;
        self.edge_scroll_tick = None;
    }
}

#[derive(Clone, Copy)]
struct TextClick {
    at: Instant,
    point: (u16, u16),
    width: u16,
    start: (u16, u16),
    end: (u16, u16),
    count: u8,
}

impl TextClick {
    fn matches(
        self,
        point: (u16, u16),
        width: u16,
        start: (u16, u16),
        end: (u16, u16),
        now: Instant,
    ) -> bool {
        self.start == start
            && self.end == end
            && self.width == width
            && self.point.1 == point.1
            && self.point.0.abs_diff(point.0) <= 1
            && now.saturating_duration_since(self.at) <= DOUBLE_CLICK_WINDOW
    }
}

#[derive(Clone)]
struct InputClick {
    at: Instant,
    point: (u16, u16),
    width: u16,
    range: std::ops::Range<usize>,
    count: u8,
}

impl InputClick {
    fn matches(
        &self,
        point: (u16, u16),
        width: u16,
        range: &std::ops::Range<usize>,
        now: Instant,
    ) -> bool {
        self.range == *range
            && self.width == width
            && self.point.1 == point.1
            && self.point.0.abs_diff(point.0) <= 1
            && now.saturating_duration_since(self.at) <= DOUBLE_CLICK_WINDOW
    }
}

#[derive(Clone, Copy)]
struct WordAnchor {
    start: (u16, u16),
    end: (u16, u16),
}

#[derive(Clone, Copy)]
struct LineAnchor {
    start: (u16, u16),
    end: (u16, u16),
}

struct TextSelection {
    canvas: Canvas,
    /// Exact content offset of the selected pane at the last painted frame.
    scroll: usize,
    from: (u16, u16),
    to: (u16, u16),
    pane_x: u16,
    dragging: bool,
    was_following: bool,
    /// Column of the anchor (`from`), kept while the anchor row is scrolled off screen.
    anchor_x: u16,
    word_anchor: Option<WordAnchor>,
    line_anchor: Option<LineAnchor>,
    /// Selected lines scrolled off above the pane (the anchor is the first) or below it
    /// (the anchor is the last), while auto-scrolling a drag past the pane edge.
    above: Vec<String>,
    below: Vec<String>,
}
impl TextSelection {
    fn is_word(text: &str) -> bool {
        text.chars().any(|ch| ch.is_alphanumeric() || ch == '_')
    }

    fn word_bounds(canvas: &Canvas, point: (u16, u16)) -> Option<((u16, u16), (u16, u16))> {
        let (left, right) = Self::row_bounds(canvas, point.1)?;
        if point.0 < left || point.0 >= right {
            return None;
        }
        let base = usize::from(point.1) * usize::from(canvas.width);
        let mut x = point.0;
        while x > left && canvas.cells[base + usize::from(x)].continuation {
            x -= 1;
        }
        let cell = &canvas.cells[base + usize::from(x)];
        if cell.continuation || !Self::is_word(&cell.text) {
            return None;
        }

        let mut start = x;
        while start > left {
            let mut previous = start - 1;
            while previous > left && canvas.cells[base + usize::from(previous)].continuation {
                previous -= 1;
            }
            let cell = &canvas.cells[base + usize::from(previous)];
            if cell.continuation || !Self::is_word(&cell.text) {
                break;
            }
            start = previous;
        }

        let width = |x: u16| {
            UnicodeWidthStr::width(canvas.cells[base + usize::from(x)].text.as_str()).max(1) as u16
        };
        let mut end = x.saturating_add(width(x) - 1).min(right - 1);
        let mut next = end.saturating_add(1);
        while next < right {
            while next < right && canvas.cells[base + usize::from(next)].continuation {
                next += 1;
            }
            if next >= right || !Self::is_word(&canvas.cells[base + usize::from(next)].text) {
                break;
            }
            end = next.saturating_add(width(next) - 1).min(right - 1);
            next = end.saturating_add(1);
        }
        Some(((start, point.1), (end, point.1)))
    }

    fn set_target(&mut self, canvas: &Canvas, point: (u16, u16)) {
        if let Some(anchor) = self.line_anchor {
            let Some((left, right)) = Self::row_bounds(canvas, point.1) else {
                self.to = point;
                return;
            };
            let target_start = (left, point.1);
            let target_end = (right.saturating_sub(1), point.1);
            if (target_start.1, target_start.0) < (anchor.start.1, anchor.start.0) {
                self.from = anchor.end;
                self.to = target_start;
            } else {
                self.from = anchor.start;
                self.to = target_end;
            }
            return;
        }
        let Some(anchor) = self.word_anchor else {
            self.to = point;
            return;
        };
        let (target_start, target_end) = Self::word_bounds(canvas, point).unwrap_or((point, point));
        if (target_start.1, target_start.0) < (anchor.start.1, anchor.start.0) {
            self.from = anchor.end;
            self.to = target_start;
        } else {
            self.from = anchor.start;
            self.to = target_end;
        }
    }

    /// First and last screen rows of the selectable pane, and its text columns.
    fn pane(&self, canvas: &Canvas) -> Option<(u16, u16, u16, u16)> {
        // Only the pane the selection started in: the chat and a side panel are separate.
        let rows: Vec<_> = canvas
            .hits
            .iter()
            .filter(|h| {
                matches!(h.action, Action::TranscriptText) && h.x.abs_diff(self.pane_x) <= 2
            })
            .collect();
        let top = rows.iter().map(|h| h.y).min()?;
        let bottom = rows.iter().map(|h| h.y).max()?;
        let left = rows.iter().map(|h| h.x).min()?;
        let right = rows.iter().map(|h| h.x + h.width).max()?.min(canvas.width);
        Some((top, bottom, left, right))
    }
    fn row_bounds(canvas: &Canvas, y: u16) -> Option<(u16, u16)> {
        canvas
            .hits
            .iter()
            .find(|h| h.y == y && matches!(h.action, Action::TranscriptText))
            .map(|h| (h.x, (h.x + h.width).min(canvas.width)))
    }
    fn row_text(canvas: &Canvas, y: u16, left: u16, right: u16) -> String {
        let base = usize::from(y) * usize::from(canvas.width);
        (left..right)
            .filter_map(|x| {
                let cell = canvas.cells.get(base + usize::from(x))?;
                (!cell.continuation).then_some(cell.text.as_str())
            })
            .collect()
    }
    /// The visible text of row `y` from column `from` on.
    fn line(&self, y: u16, from: u16) -> String {
        let Some((left, right)) = Self::row_bounds(&self.canvas, y) else {
            return String::new();
        };
        Self::row_text(&self.canvas, y, from.max(left), right)
            .trim_end()
            .to_owned()
    }
    /// The pane content moved since the selection's snapshot (auto-scroll): keep the
    /// selected rows that left the screen and move the anchor with the content.
    fn follow(&mut self, canvas: &Canvas, scroll: usize) {
        if (canvas.width, canvas.height) != (self.canvas.width, self.canvas.height) {
            return;
        }
        let Some((top, bottom, _, _)) = self.pane(&self.canvas) else {
            return;
        };
        let old: Vec<String> = (top..=bottom)
            .map(|y| {
                Self::row_bounds(&self.canvas, y).map_or_else(String::new, |(left, right)| {
                    Self::row_text(&self.canvas, y, left, right)
                })
            })
            .collect();
        let new: Vec<String> = (top..=bottom)
            .map(|y| {
                Self::row_bounds(canvas, y).map_or_else(String::new, |(left, right)| {
                    Self::row_text(canvas, y, left, right)
                })
            })
            .collect();
        if old == new && scroll == self.scroll {
            return;
        }
        let height = old.len() as i32;
        // shift > 0: content moved up (scrolled down)
        let inferred = (scroll == self.scroll)
            .then(|| {
                (1 - height..height)
                    .filter(|k| *k != 0)
                    .map(|k| {
                        let score = (0..height)
                            .filter(|i| {
                                let j = i - k;
                                j >= 0
                                    && j < height
                                    && !old[*i as usize].trim().is_empty()
                                    && old[*i as usize] == new[j as usize]
                            })
                            .count();
                        (score, -k.abs(), k)
                    })
                    .max()
                    .filter(|(score, _, _)| *score * 2 >= height as usize / 2 && *score > 0)
                    .map(|(_, _, k)| k)
            })
            .flatten();
        // Repeated or blank lines make visual matching ambiguous. The pane's
        // actual scroll offset is authoritative during an edge drag.
        let shift = if scroll != self.scroll {
            (scroll as i64 - self.scroll as i64).clamp(i64::from(1 - height), i64::from(height - 1))
                as i32
        } else if let Some(shift) = inferred {
            shift
        } else {
            self.scroll = scroll;
            self.canvas = canvas.clone();
            return;
        };
        if shift != 0 {
            self.word_anchor = None;
            self.line_anchor = None;
        }
        self.scroll = scroll;
        let k = shift.unsigned_abs() as u16;
        if shift > 0 {
            if !self.below.is_empty() {
                // Anchor is below the pane: rows come back in from the bottom.
                let back = usize::from(k).min(self.below.len());
                self.below.drain(..back);
                if self.below.is_empty() {
                    self.from = (self.anchor_x, bottom.saturating_sub(k - back as u16));
                }
            } else {
                // Selected rows from the anchor down leave at the top.
                for y in top..top.saturating_add(k).min(bottom + 1) {
                    if y >= self.from.1 {
                        let from = if y == self.from.1 { self.from.0 } else { 0 };
                        self.above.push(self.line(y, from));
                    }
                }
                if self.from.1 >= top + k {
                    self.from.1 -= k;
                } else {
                    self.from = (0, top);
                }
            }
        } else if !self.above.is_empty() {
            // Anchor is above the pane: scrolling back up returns rows from `above`.
            let back = usize::from(k).min(self.above.len());
            let keep = self.above.len() - back;
            self.above.truncate(keep);
            if self.above.is_empty() {
                self.from = (self.anchor_x, top + (k - back as u16));
            }
        } else {
            // Selected rows from the anchor up leave at the bottom.
            let mut leaving = Vec::new();
            for y in bottom.saturating_sub(k - 1).max(top)..=bottom {
                if y <= self.from.1 {
                    let Some((left, right)) = Self::row_bounds(&self.canvas, y) else {
                        continue;
                    };
                    let to = if y == self.from.1 {
                        self.from.0 + 1
                    } else {
                        right
                    };
                    leaving.push(
                        Self::row_text(&self.canvas, y, left, to.min(right))
                            .trim_end()
                            .to_owned(),
                    );
                }
            }
            leaving.extend(std::mem::take(&mut self.below));
            self.below = leaving;
            if self.from.1 + k <= bottom {
                self.from.1 += k;
            } else {
                self.from = (canvas.width, bottom);
            }
        }
        self.canvas = canvas.clone();
    }
    fn indices(&self) -> Vec<Vec<usize>> {
        let (start, end) = if (self.from.1, self.from.0) <= (self.to.1, self.to.0) {
            (self.from, self.to)
        } else {
            (self.to, self.from)
        };
        let mut lines = Vec::new();
        for y in start.1..=end.1.min(self.canvas.height.saturating_sub(1)) {
            let Some(hit) = self
                .canvas
                .hits
                .iter()
                .find(|h| h.y == y && matches!(h.action, Action::TranscriptText))
            else {
                continue;
            };
            // Each transcript hit starts at the first copyable cell for that row.
            let left = hit.x;
            let right = (hit.x + hit.width).min(self.canvas.width);
            let from = if y == start.1 {
                start.0.max(left)
            } else {
                left
            };
            let to = if y == end.1 {
                end.0.saturating_add(1).min(right)
            } else {
                right
            };
            let base = usize::from(y) * usize::from(self.canvas.width);
            let mut indices = Vec::new();
            for x in left..right {
                let i = base + usize::from(x);
                let cell = &self.canvas.cells[i];
                if cell.continuation {
                    continue;
                }
                let width = UnicodeWidthStr::width(cell.text.as_str()) as u16;
                if x < to && x.saturating_add(width) > from {
                    indices.extend(i..(i + usize::from(width)).min(base + usize::from(right)));
                }
            }
            lines.push(indices);
        }
        lines
    }
    fn text(&self) -> String {
        let visible = self.visible_text();
        self.above
            .iter()
            .cloned()
            .chain(
                (!visible.is_empty() || (self.above.is_empty() && self.below.is_empty()))
                    .then_some(visible),
            )
            .chain(self.below.iter().cloned())
            .collect::<Vec<_>>()
            .join("\n")
    }
    fn visible_text(&self) -> String {
        self.indices()
            .iter()
            .map(|line| {
                line.iter()
                    .filter_map(|i| {
                        let cell = &self.canvas.cells[*i];
                        (!cell.continuation).then_some(cell.text.as_str())
                    })
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

impl App {
    fn selection_pane_scroll(&self, pane_x: u16, width: u16) -> usize {
        let chat_pane = self.wide_layout && pane_x < width - (width / 3).clamp(36, 46) - 1;
        if self.wide_layout && self.view == View::Chat && !chat_pane {
            self.overview_scroll
        } else if chat_pane && self.view != View::Chat {
            self.positions
                .get(&View::Chat)
                .map_or(0, |position| position.0)
        } else {
            self.scroll
        }
    }
    fn selection_scroll_step(selection: &TextSelection) -> usize {
        selection
            .pane(&selection.canvas)
            .map_or(1, |(top, bottom, _, _)| {
                (usize::from(bottom - top + 1) / 3).clamp(1, 4)
            })
    }
    fn input_word_range(&self, offset: usize) -> Option<std::ops::Range<usize>> {
        let text = self.editor.text.as_str();
        if text.is_empty() {
            return None;
        }
        let graphemes = text
            .grapheme_indices(true)
            .map(|(start, g)| {
                let end = start + g.len();
                let word = g.chars().any(|ch| ch.is_alphanumeric() || ch == '_');
                (start, end, word)
            })
            .collect::<Vec<_>>();
        let index = graphemes
            .iter()
            .position(|(start, end, word)| *word && *start < offset && offset <= *end)
            .or_else(|| {
                graphemes
                    .iter()
                    .position(|(start, _, word)| *word && *start == offset)
            })?;
        let mut first = index;
        let mut last = index;
        while first > 0 && graphemes[first - 1].2 {
            first -= 1;
        }
        while last + 1 < graphemes.len() && graphemes[last + 1].2 {
            last += 1;
        }
        Some(graphemes[first].0..graphemes[last].1)
    }
    fn input_line_range(&self, offset: usize) -> std::ops::Range<usize> {
        let offset = offset.min(self.editor.text.len());
        let start = self.editor.text[..offset]
            .rfind('\n')
            .map_or(0, |at| at + 1);
        let end = self.editor.text[offset..]
            .find('\n')
            .map_or(self.editor.text.len(), |at| offset + at);
        start..end
    }
    fn set_input_unit_selection(
        &mut self,
        anchor: &std::ops::Range<usize>,
        target: &std::ops::Range<usize>,
    ) {
        if target.start < anchor.start {
            self.editor.anchor = Some(anchor.end);
            self.editor.cursor = target.start;
        } else {
            self.editor.anchor = Some(anchor.start);
            self.editor.cursor = target.end;
        }
    }
    pub fn reset_mouse(&mut self) {
        self.mouse_state = MouseState::default();
    }
    pub fn mouse(&mut self, event: MouseEvent, canvas: &Canvas) -> Option<Intent> {
        let point = (
            event.column.min(canvas.width.saturating_sub(1)),
            event.row.min(canvas.height.saturating_sub(1)),
        );
        let action = canvas.hit(point.0, point.1);
        let queue_viewport = canvas.hits.iter().any(|hit| {
            hit.action == Action::QueueViewport
                && point.0 >= hit.x
                && point.0 < hit.x.saturating_add(hit.width)
                && point.1 >= hit.y
                && point.1 < hit.y.saturating_add(hit.height)
        });
        match event.kind {
            MouseEventKind::Moved => {
                self.hovered = action.filter(|a| {
                    !matches!(
                        a,
                        Action::TranscriptText
                            | Action::SelectEntry(_)
                            | Action::MessageRow(_, _, _)
                            | Action::Input
                    )
                });
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                self.reset_mouse();
                if queue_viewport {
                    self.scroll_queue(event.kind == MouseEventKind::ScrollDown);
                } else {
                    self.scroll_at(
                        event.column,
                        canvas.width,
                        event.kind == MouseEventKind::ScrollDown,
                    );
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let now = Instant::now();
                self.hovered = None;
                let transcript_hit = canvas.hits.iter().find(|h| {
                    matches!(h.action, Action::TranscriptText)
                        && point.0 >= h.x
                        && point.0 < h.x + h.width
                        && point.1 >= h.y
                        && point.1 < h.y + h.height
                });
                if event.modifiers.contains(KeyModifiers::SHIFT)
                    && transcript_hit.is_some()
                    && !matches!(action, Some(Action::Branch(_)))
                    && self
                        .mouse_state
                        .selection
                        .as_ref()
                        .is_some_and(|selection| !selection.dragging)
                {
                    let mut selection = self.mouse_state.selection.take().unwrap();
                    let scroll = self.selection_pane_scroll(selection.pane_x, canvas.width);
                    selection.follow(canvas, scroll);
                    selection.canvas = canvas.clone();
                    selection.word_anchor = None;
                    selection.line_anchor = None;
                    selection.dragging = true;
                    selection.to = point;
                    self.mouse_state.reset_gesture();
                    self.mouse_state.last_text_click = None;
                    self.mouse_state.selection = Some(selection);
                    return None;
                }

                self.mouse_state.reset_gesture();
                if action == Some(Action::Input) && !self.readonly {
                    self.activate(Action::Input);
                    if let Some(offset) = self.input_offset_at(canvas, point) {
                        if event.modifiers.contains(KeyModifiers::SHIFT) {
                            let anchor = self.editor.anchor.unwrap_or(self.editor.cursor);
                            self.editor.anchor = (anchor != offset).then_some(anchor);
                            self.editor.cursor = offset;
                            self.mouse_state.input_anchor = Some(anchor);
                            self.mouse_state.last_input_click = None;
                        } else {
                            let word = self.input_word_range(offset);
                            let count = word.as_ref().map_or(1, |range| {
                                self.mouse_state
                                    .last_input_click
                                    .as_ref()
                                    .filter(|click| click.matches(point, canvas.width, range, now))
                                    .map_or(1, |click| click.count.saturating_add(1).min(3))
                            });
                            self.mouse_state.last_input_click =
                                word.clone().map(|range| InputClick {
                                    at: now,
                                    point,
                                    width: canvas.width,
                                    range,
                                    count,
                                });
                            if count >= 3 {
                                let range = self.input_line_range(offset);
                                self.editor.anchor = Some(range.start);
                                self.editor.cursor = range.end;
                                self.mouse_state.input_anchor = Some(range.start);
                                self.mouse_state.input_line_anchor = Some(range);
                            } else if count == 2 {
                                if let Some(range) = word {
                                    self.editor.anchor = Some(range.start);
                                    self.editor.cursor = range.end;
                                    self.mouse_state.input_anchor = Some(range.start);
                                    self.mouse_state.input_word_anchor = Some(range);
                                }
                            } else {
                                self.editor.cursor = offset;
                                self.editor.anchor = None;
                                self.mouse_state.input_anchor = Some(offset);
                            }
                        }
                    }
                } else if transcript_hit.is_some() && !matches!(action, Some(Action::Branch(_))) {
                    self.branch_entry = None;
                    self.focus = None;
                    self.mouse_state.pressed = action.clone().filter(|a| {
                        !matches!(
                            a,
                            Action::TranscriptText
                                | Action::SelectEntry(_)
                                | Action::MessageRow(_, _, _)
                        )
                    });
                    let mut was_following = self.follow;
                    if self.wide_layout && self.view != View::Chat {
                        if let Some(position) = self.positions.get_mut(&View::Chat) {
                            was_following = position.1;
                            position.1 = false;
                        }
                    } else {
                        self.follow = false;
                    }
                    let pane_x = transcript_hit.map_or(2, |h| h.x);
                    let word = TextSelection::word_bounds(canvas, point);
                    let count = word.map_or(1, |(start, end)| {
                        self.mouse_state
                            .last_text_click
                            .filter(|click| click.matches(point, canvas.width, start, end, now))
                            .map_or(1, |click| click.count.saturating_add(1).min(3))
                    });
                    self.mouse_state.last_text_click = word.map(|(start, end)| TextClick {
                        at: now,
                        point,
                        width: canvas.width,
                        start,
                        end,
                        count,
                    });
                    let line = (count >= 3)
                        .then(|| TextSelection::row_bounds(canvas, point.1))
                        .flatten()
                        .map(|(left, right)| {
                            let start = (left, point.1);
                            let end = (right.saturating_sub(1), point.1);
                            (start, end, Some(LineAnchor { start, end }))
                        });
                    let (from, to, word_anchor, line_anchor) =
                        if let Some((start, end, line)) = line {
                            (start, end, None, line)
                        } else if count == 2 {
                            word.map_or((point, point, None, None), |(start, end)| {
                                (start, end, Some(WordAnchor { start, end }), None)
                            })
                        } else {
                            (point, point, None, None)
                        };
                    self.mouse_state.selection = Some(TextSelection {
                        canvas: canvas.clone(),
                        scroll: self.selection_pane_scroll(pane_x, canvas.width),
                        from,
                        to,
                        pane_x,
                        dragging: true,
                        was_following,
                        anchor_x: from.0,
                        word_anchor,
                        line_anchor,
                        above: Vec::new(),
                        below: Vec::new(),
                    });
                } else {
                    self.mouse_state.last_text_click = None;
                    self.mouse_state.last_input_click = None;
                    self.mouse_state.pressed = action;
                }
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(anchor) = self.mouse_state.input_anchor {
                    self.mouse_state.last_input_click = None;
                    if let Some(offset) = self.input_offset_at(canvas, point) {
                        if let Some(line_anchor) = self.mouse_state.input_line_anchor.clone() {
                            let target = self.input_line_range(offset);
                            self.set_input_unit_selection(&line_anchor, &target);
                        } else if let Some(word_anchor) = self.mouse_state.input_word_anchor.clone()
                        {
                            let target = self.input_word_range(offset).unwrap_or(offset..offset);
                            self.set_input_unit_selection(&word_anchor, &target);
                        } else {
                            self.editor.anchor = Some(anchor);
                            self.editor.cursor = offset;
                        }
                    }
                } else if self
                    .mouse_state
                    .selection
                    .as_ref()
                    .is_some_and(|s| s.dragging)
                {
                    self.mouse_state.last_text_click = None;
                    let (pane_x, step, painted_scroll) = {
                        let selection = self.mouse_state.selection.as_ref().unwrap();
                        (
                            selection.pane_x,
                            Self::selection_scroll_step(selection),
                            selection.scroll,
                        )
                    };
                    let current_scroll = self.selection_pane_scroll(pane_x, canvas.width);
                    let selection = self.mouse_state.selection.as_mut().unwrap();
                    selection.follow(canvas, painted_scroll);
                    selection.set_target(canvas, point);
                    // Dragging onto the pane's first or last row scrolls it, so a
                    // selection can extend beyond what fits on screen.
                    if let Some((top, bottom, _, _)) = selection.pane(canvas) {
                        let edge = if point.1 >= bottom {
                            Some(true)
                        } else if point.1 <= top {
                            Some(false)
                        } else {
                            None
                        };
                        if let Some(down) = edge {
                            if painted_scroll == current_scroll {
                                self.scroll_pane(pane_x, canvas.width, down, step);
                            }
                            self.mouse_state.edge_scroll_tick = Some(Instant::now());
                        } else {
                            self.mouse_state.edge_scroll_tick = None;
                        }
                    }
                }
            }
            MouseEventKind::Up(MouseButton::Left) => {
                self.mouse_state.edge_scroll_tick = None;
                if let Some(anchor) = self.mouse_state.input_anchor.take() {
                    if let Some(offset) = self.input_offset_at(canvas, point) {
                        if let Some(line_anchor) = self.mouse_state.input_line_anchor.take() {
                            let target = self.input_line_range(offset);
                            self.set_input_unit_selection(&line_anchor, &target);
                        } else if let Some(word_anchor) = self.mouse_state.input_word_anchor.take()
                        {
                            let target = self.input_word_range(offset).unwrap_or(offset..offset);
                            self.set_input_unit_selection(&word_anchor, &target);
                        } else {
                            self.editor.anchor = (anchor != offset).then_some(anchor);
                            self.editor.cursor = offset;
                        }
                    }
                    return self
                        .editor
                        .selection()
                        .map(|range| Intent::Copy(self.editor.text[range].into()));
                }
                let scroll = self.mouse_state.selection.as_ref().map(|s| s.scroll);
                if let Some(selection) = self.mouse_state.selection.as_mut().filter(|s| s.dragging)
                {
                    selection.follow(canvas, scroll.unwrap());
                    selection.set_target(canvas, point);
                    selection.dragging = false;
                    if selection.from == selection.to
                        && selection.word_anchor.is_none()
                        && selection.line_anchor.is_none()
                        && selection.above.is_empty()
                        && selection.below.is_empty()
                    {
                        let was_following = selection.was_following;
                        self.mouse_state.selection = None;
                        if self.wide_layout && self.view != View::Chat {
                            if let Some(position) = self.positions.get_mut(&View::Chat) {
                                position.1 = was_following;
                            }
                        } else {
                            self.follow = was_following;
                        }
                    } else {
                        let text = selection.text();
                        return (!text.is_empty()).then_some(Intent::Copy(text));
                    }
                }
                if let Some(pressed) = self.mouse_state.pressed.take() {
                    if action.as_ref() == Some(&pressed) {
                        if !matches!(pressed, Action::Input | Action::OpenLink(_)) {
                            self.focus = Some(pressed.clone());
                        }
                        return self.activate(pressed);
                    }
                }
            }
            _ => {}
        }
        None
    }
    fn input_offset_at(&self, canvas: &Canvas, point: (u16, u16)) -> Option<usize> {
        // Clamp dragging outside the input to the closest visible input row.
        let y = canvas
            .hits
            .iter()
            .filter(|h| h.action == Action::Input)
            .min_by_key(|h| h.y.abs_diff(point.1))?
            .y;
        let x = point.0.clamp(1, canvas.width.saturating_sub(2));
        canvas.input_offsets[usize::from(y) * usize::from(canvas.width) + usize::from(x)]
    }
    pub(super) fn tick_drag_scroll(&mut self, now: Instant) -> bool {
        let Some(previous) = self.mouse_state.edge_scroll_tick else {
            return false;
        };
        if now.saturating_duration_since(previous) < Duration::from_millis(60) {
            return false;
        }
        self.mouse_state.edge_scroll_tick = Some(now);
        let edge = self
            .mouse_state
            .selection
            .as_ref()
            .filter(|s| s.dragging)
            .and_then(|s| {
                let (top, bottom, _, _) = s.pane(&s.canvas)?;
                let down = if s.to.1 >= bottom {
                    true
                } else if s.to.1 <= top {
                    false
                } else {
                    return None;
                };
                Some((
                    s.pane_x,
                    s.canvas.width,
                    down,
                    Self::selection_scroll_step(s),
                ))
            });
        if let Some((x, width, down, step)) = edge {
            let before = self.selection_pane_scroll(x, width);
            if self
                .mouse_state
                .selection
                .as_ref()
                .is_some_and(|s| s.scroll != before)
            {
                return false;
            }
            self.scroll_pane(x, width, down, step);
            self.selection_pane_scroll(x, width) != before
        } else {
            false
        }
    }
    pub(super) fn paint_mouse_selection(&mut self, canvas: &mut Canvas) {
        let scroll = self
            .mouse_state
            .selection
            .as_ref()
            .map(|s| self.selection_pane_scroll(s.pane_x, canvas.width));
        if let Some(selection) = self.mouse_state.selection.as_mut().filter(|s| s.dragging) {
            selection.follow(canvas, scroll.unwrap());
        }
        if let Some(s) = &self.mouse_state.selection {
            if (canvas.width, canvas.height) == (s.canvas.width, s.canvas.height)
                && (s.from != s.to || s.word_anchor.is_some() || s.line_anchor.is_some())
            {
                for i in s.indices().into_iter().flatten() {
                    // Don't paint an old selection over a replaced transcript.
                    if canvas.cells[i].text == s.canvas.cells[i].text {
                        canvas.cells[i].background = Background::Selected;
                    }
                }
            }
        }
    }
    pub(super) fn selected_mouse_text(&self) -> Option<String> {
        self.mouse_state
            .selection
            .as_ref()
            .filter(|s| s.from != s.to || s.word_anchor.is_some() || s.line_anchor.is_some())
            .map(TextSelection::text)
    }
}

pub(super) struct ComposerRow {
    pub text: String,
    pub offsets: Vec<usize>,
}
/// Each terminal cell maps to a UTF-8/grapheme boundary in the display text.
pub(super) fn composer_rows(text: &str, width: usize) -> Vec<ComposerRow> {
    let width = width.max(1);
    let mut rows = Vec::new();
    let mut row = ComposerRow {
        text: String::new(),
        offsets: vec![0],
    };
    let mut used = 0;
    for (at, g) in text.grapheme_indices(true) {
        let end = at + g.len();
        if g == "\n" {
            rows.push(row);
            row = ComposerRow {
                text: String::new(),
                offsets: vec![end],
            };
            used = 0;
            continue;
        }
        if g.chars().any(char::is_control) && g != "\t" {
            continue;
        }
        let shown = if g == "\t" { "    " } else { g };
        let size = UnicodeWidthStr::width(shown);
        if used + size > width {
            rows.push(row);
            row = ComposerRow {
                text: String::new(),
                offsets: vec![at],
            };
            used = 0;
        }
        row.text.push_str(shown);
        // The second half of a wide grapheme clicks to its trailing boundary.
        for n in 1..=size {
            row.offsets.push(if n * 2 >= size { end } else { at });
        }
        used += size;
    }
    let end = *row.offsets.last().unwrap();
    rows.push(row);
    if used == width {
        rows.push(ComposerRow {
            text: String::new(),
            offsets: vec![end],
        });
    }
    rows
}

#[path = "../selfcheck/question_hover.rs"]
mod question_hover_checks;

impl App {
    /// Called by the central self-check runner; uses the already-landed hover scenarios.
    pub(crate) fn question_hover_selfcheck(report: &mut crate::selfcheck::Report) {
        question_hover_checks::run(report);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn question_controls_show_hover_after_composer_overlays() {
        let mut report = crate::selfcheck::Report::default();
        App::question_hover_selfcheck(&mut report);
        for dump in &report.dumps {
            println!("{}\n{}", dump.label, dump.rows.join("\n"));
        }
        println!(
            "question hover: {} checks, {} failed",
            report.checks.len(),
            report.failures()
        );
        let failures: Vec<_> = report.checks.iter().filter(|check| !check.pass).collect();
        assert!(
            failures.is_empty(),
            "{}",
            failures
                .iter()
                .map(|check| { format!("{}: {}", check.name, check.detail) })
                .collect::<Vec<_>>()
                .join("\n")
        );
    }
    #[test]
    fn mouse_capture_enables_pointer_motion_and_sgr_coordinates() {
        use crossterm::Command;
        let mut ansi = String::new();
        crossterm::event::EnableMouseCapture
            .write_ansi(&mut ansi)
            .unwrap();
        assert!(
            ansi.contains("\x1b[?1003h"),
            "hover requires all mouse movement"
        );
        assert!(
            ansi.contains("\x1b[?1006h"),
            "hover requires SGR mouse coordinates"
        );
    }
    fn event(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
        event_with(kind, x, y, KeyModifiers::NONE)
    }
    fn event_with(kind: MouseEventKind, x: u16, y: u16, modifiers: KeyModifiers) -> MouseEvent {
        MouseEvent {
            kind,
            column: x,
            row: y,
            modifiers,
        }
    }
    fn point(canvas: &Canvas, text: &str) -> (u16, u16) {
        for y in 0..canvas.height {
            for x in 0..canvas.width {
                let start = usize::from(y) * usize::from(canvas.width) + usize::from(x);
                let tail = canvas.cells[start..usize::from(y + 1) * usize::from(canvas.width)]
                    .iter()
                    .filter(|c| !c.continuation)
                    .map(|c| c.text.as_str())
                    .collect::<String>();
                if tail.starts_with(text) {
                    return (x, y);
                }
            }
        }
        panic!("missing {text}");
    }
    fn click(app: &mut App, c: &Canvas, x: u16, y: u16) -> Option<Intent> {
        assert_eq!(
            app.mouse(event(MouseEventKind::Down(MouseButton::Left), x, y), c),
            None
        );
        app.mouse(event(MouseEventKind::Up(MouseButton::Left), x, y), c)
    }
    #[test]
    fn panel_text_can_be_dragged_to_copy_and_a_click_still_acts() {
        let mut app = App::new(false);
        app.set_thread(&json!({"id":"current","preview":"Now"}));
        app.activate(Action::Sessions);
        app.panel_open = true;
        let c = app.render(100, 30);
        let (x, y) = point(&c, "현재 대화");
        app.mouse(event(MouseEventKind::Down(MouseButton::Left), x, y), &c);
        // "현재 대화" is 9 cells wide (Hangul takes two).
        app.mouse(event(MouseEventKind::Drag(MouseButton::Left), x + 8, y), &c);
        let copied = app.mouse(event(MouseEventKind::Up(MouseButton::Left), x + 8, y), &c);
        assert_eq!(copied, Some(Intent::Copy("현재 대화".into())), "panel text must be selectable");
        // A click without dragging keeps the row's own action.
        let c = app.render(100, 30);
        let (x, y) = point(&c, "+ 새 대화");
        assert_eq!(click(&mut app, &c, x + 2, y), Some(Intent::Session(None)));
    }

    #[test]
    fn dragging_past_the_bottom_scrolls_and_copies_rows_beyond_the_first_screen() {
        let mut app = App::new(false);
        let text: String = (1..=60).map(|i| format!("줄 {i:02} 내용\n\n")).collect();
        app.set_thread(&json!({"id":"long","turns":[{"id":"turn","status":"completed","items":[{"id":"message","type":"agentMessage","text":text}]}]}));
        app.follow = false;
        app.scroll = 0;
        let mut c = app.render(80, 24);
        let (x, y) = point(&c, "줄 01");
        app.mouse(event(MouseEventKind::Down(MouseButton::Left), x, y), &c);
        let bottom = c
            .hits
            .iter()
            .filter(|h| matches!(h.action, Action::SelectEntry(_)))
            .map(|h| h.y)
            .max()
            .unwrap();
        // Hold the drag on the last pane row: every event scrolls one row.
        for _ in 0..30 {
            app.mouse(
                event(MouseEventKind::Drag(MouseButton::Left), x + 4, bottom),
                &c,
            );
            c = app.render(80, 24);
        }
        let Some(Intent::Copy(copied)) = app.mouse(
            event(MouseEventKind::Up(MouseButton::Left), x + 4, bottom),
            &c,
        ) else {
            panic!("expected a copy");
        };
        assert!(copied.starts_with("줄 01"), "{copied}");
        assert!(
            copied.contains("줄 05"),
            "rows that scrolled off are kept: {copied}"
        );
        let last = (1..=60)
            .rev()
            .find(|i| copied.contains(&format!("줄 {i:02}")))
            .unwrap();
        assert!(
            last > 12,
            "selection extends past the first screen: {copied}"
        );
        for i in 1..=last {
            assert_eq!(
                copied.matches(&format!("줄 {i:02}")).count(),
                1,
                "row {i} once: {copied}"
            );
        }
    }
    #[test]
    fn dragging_past_the_top_scrolls_up_and_keeps_rows_below() {
        let mut app = App::new(false);
        let text: String = (1..=60).map(|i| format!("줄 {i:02} 내용\n\n")).collect();
        app.set_thread(&json!({"id":"long","turns":[{"id":"turn","status":"completed","items":[{"id":"message","type":"agentMessage","text":text}]}]}));
        let mut c = app.render(80, 24);
        let (x, y) = point(&c, "줄 60");
        app.mouse(event(MouseEventKind::Down(MouseButton::Left), x + 8, y), &c);
        let top = c
            .hits
            .iter()
            .filter(|h| matches!(h.action, Action::SelectEntry(_)))
            .map(|h| h.y)
            .min()
            .unwrap();
        for _ in 0..20 {
            app.mouse(event(MouseEventKind::Drag(MouseButton::Left), x, top), &c);
            c = app.render(80, 24);
        }
        let Some(Intent::Copy(copied)) =
            app.mouse(event(MouseEventKind::Up(MouseButton::Left), x, top), &c)
        else {
            panic!("expected a copy");
        };
        assert!(copied.trim_end().ends_with("줄 60 내용"), "{copied}");
        assert!(
            copied.contains("줄 50"),
            "rows that scrolled off below are kept: {copied}"
        );
        let first = (1..=60)
            .find(|i| copied.contains(&format!("줄 {i:02}")))
            .unwrap();
        assert!(
            first < 48,
            "selection extends above the first screen: {copied}"
        );
        for i in first..=60 {
            assert_eq!(
                copied.matches(&format!("줄 {i:02}")).count(),
                1,
                "row {i} once: {copied}"
            );
        }
    }
    #[test]
    fn holding_a_drag_at_the_bottom_scrolls_without_more_mouse_events() {
        let mut app = App::new(false);
        let text: String = (1..=60).map(|i| format!("줄 {i:02} 내용\n\n")).collect();
        app.set_thread(&json!({"id":"long","turns":[{"id":"turn","status":"completed","items":[{"id":"message","type":"agentMessage","text":text}]}]}));
        app.follow = false;
        app.scroll = 0;
        let mut canvas = app.render(80, 24);
        let (x, y) = point(&canvas, "줄 01");
        let bottom = canvas
            .hits
            .iter()
            .filter(|h| matches!(h.action, Action::SelectEntry(_)))
            .map(|h| h.y)
            .max()
            .unwrap();
        app.mouse(
            event(MouseEventKind::Down(MouseButton::Left), x, y),
            &canvas,
        );
        app.mouse(
            event(MouseEventKind::Drag(MouseButton::Left), x + 4, bottom),
            &canvas,
        );
        let initial_scroll = app.scroll;
        let now = Instant::now();
        for step in 1..=15 {
            app.tick(now + Duration::from_millis(step * 150));
            canvas = app.render(80, 24);
        }
        assert!(
            app.scroll > initial_scroll + 5,
            "stationary edge drag did not scroll"
        );
        let Some(Intent::Copy(text)) = app.mouse(
            event(MouseEventKind::Up(MouseButton::Left), x + 4, bottom),
            &canvas,
        ) else {
            panic!("copy missing")
        };
        assert!(
            text.starts_with("줄 01") && text.contains("줄 10"),
            "{text}"
        );
        let stopped = app.scroll;
        app.tick(now + Duration::from_secs(5));
        assert_eq!(app.scroll, stopped, "mouse release must stop scrolling");
    }
    #[test]
    fn queued_drag_events_wait_for_a_painted_frame_before_scrolling_again() {
        let mut app = App::new(false);
        let text: String = (1..=80).map(|i| format!("줄 {i:02} 내용\n")).collect();
        app.set_thread(&json!({"id":"long","turns":[{"id":"turn","status":"completed","items":[{"id":"message","type":"agentMessage","text":text}]}]}));
        app.follow = false;
        app.scroll = 0;
        let c = app.render(80, 24);
        let (x, y) = point(&c, "줄 01");
        let bottom = c
            .hits
            .iter()
            .filter(|h| matches!(h.action, Action::SelectEntry(_)))
            .map(|h| h.y)
            .max()
            .unwrap();
        app.mouse(event(MouseEventKind::Down(MouseButton::Left), x, y), &c);
        app.mouse(
            event(MouseEventKind::Drag(MouseButton::Left), x, bottom),
            &c,
        );
        let first = app.scroll;
        assert!(first > 0);
        app.mouse(
            event(MouseEventKind::Drag(MouseButton::Left), x, bottom),
            &c,
        );
        let now = Instant::now();
        app.tick(now + Duration::from_millis(150));
        assert_eq!(app.scroll, first, "unpainted rows must not be skipped");
        let c = app.render(80, 24);
        app.mouse(
            event(MouseEventKind::Drag(MouseButton::Left), x, bottom),
            &c,
        );
        assert!(app.scroll > first);
        let c = app.render(80, 24);
        let Some(Intent::Copy(copied)) =
            app.mouse(event(MouseEventKind::Up(MouseButton::Left), x, bottom), &c)
        else {
            panic!("copy missing")
        };
        assert!(copied.starts_with("줄 01"), "{copied}");
        for i in 1..=80 {
            assert!(
                copied.matches(&format!("줄 {i:02}")).count() <= 1,
                "duplicate row {i}: {copied}"
            );
        }
    }
    #[test]
    fn body_hover_and_click_do_not_focus_and_drag_copies_unicode_without_controls() {
        for readonly in [false, true] {
            let mut app = App::new(readonly);
            app.set_thread(&json!({"id":"source","turns":[{"id":"turn","status":"completed","items":[{"id":"message","type":"agentMessage","text":"**한글** e\u{301} 🦀\n둘째 줄"}]}]}));
            let c = app.render(80, 30);
            let (x, y) = point(&c, "한글");
            app.mouse(event(MouseEventKind::Moved, x, y), &c);
            assert!(app.hovered.is_none());
            click(&mut app, &c, x, y);
            assert!(app.focus.is_none() && app.branch_entry.is_none());
            assert_eq!(
                app.render(80, 30).cells[usize::from(y) * 80 + usize::from(x)].background,
                c.cells[usize::from(y) * 80 + usize::from(x)].background
            );
            app.mouse(event(MouseEventKind::Down(MouseButton::Left), x + 3, y), &c);
            app.mouse(event(MouseEventKind::Drag(MouseButton::Left), x + 1, y), &c);
            let painted = app.render(80, 30);
            let index = usize::from(y) * 80 + usize::from(x);
            assert_eq!(painted.cells[index].background, Background::Selected);
            assert_eq!(painted.cells[index].tone, crate::engine::Tone::Strong);
            assert_eq!(
                app.mouse(
                    event(MouseEventKind::Up(MouseButton::Left), x + 1, y),
                    &painted
                ),
                Some(Intent::Copy("한글".into()))
            );
            // Selecting across rows excludes branch buttons and the message marker.
            let (end_x, end_y) = point(&c, "둘째 줄");
            app.mouse(event(MouseEventKind::Down(MouseButton::Left), x, y), &c);
            let copy = app.mouse(
                event(MouseEventKind::Up(MouseButton::Left), end_x + 6, end_y),
                &c,
            );
            assert_eq!(copy, Some(Intent::Copy("한글 e\u{301} 🦀\n둘째 줄".into())));
        }
    }
    #[test]
    fn double_click_selects_a_word_and_drag_expands_by_word_boundaries() {
        let mut app = App::new(false);
        app.set_thread(&json!({"id":"source","turns":[{"id":"turn","status":"completed","items":[{"id":"message","type":"agentMessage","text":"alpha beta 한글_test gamma omega"}]}]}));
        let c = app.render(80, 30);
        let (beta_x, y) = point(&c, "beta");
        let (gamma_x, _) = point(&c, "gamma");

        assert_eq!(click(&mut app, &c, beta_x + 1, y), None);
        assert_eq!(
            app.mouse(
                event(MouseEventKind::Down(MouseButton::Left), beta_x + 1, y),
                &c,
            ),
            None
        );
        let selected = app.render(80, 30);
        for x in beta_x..beta_x + 4 {
            assert_eq!(
                selected.cells[usize::from(y) * 80 + usize::from(x)].background,
                Background::Selected,
                "double click should select the complete word"
            );
        }

        app.mouse(
            event(MouseEventKind::Drag(MouseButton::Left), gamma_x + 1, y),
            &selected,
        );
        let selected = app.render(80, 30);
        assert_eq!(
            selected.cells[usize::from(y) * 80 + usize::from(gamma_x + 4)].background,
            Background::Selected,
            "word drag should extend through the target word"
        );
        assert_eq!(
            app.mouse(
                event(MouseEventKind::Up(MouseButton::Left), gamma_x + 1, y),
                &selected,
            ),
            Some(Intent::Copy("beta 한글_test gamma".into()))
        );
    }
    #[test]
    fn triple_click_selects_the_transcript_row_and_shift_click_extends_selection() {
        let mut app = App::new(false);
        app.set_thread(&json!({"id":"source","turns":[{"id":"turn","status":"completed","items":[{"id":"message","type":"agentMessage","text":"alpha beta gamma\nsecond row"}]}]}));
        let c = app.render(80, 30);
        let (beta_x, y) = point(&c, "beta");
        let (gamma_x, _) = point(&c, "gamma");

        assert_eq!(click(&mut app, &c, beta_x + 1, y), None);
        assert_eq!(
            click(&mut app, &c, beta_x + 1, y),
            Some(Intent::Copy("beta".into()))
        );
        assert_eq!(
            app.mouse(
                event(MouseEventKind::Down(MouseButton::Left), beta_x + 1, y),
                &c,
            ),
            None
        );
        let selected = app.render(80, 30);
        assert_eq!(
            app.mouse(
                event(MouseEventKind::Up(MouseButton::Left), beta_x + 1, y),
                &selected,
            ),
            Some(Intent::Copy("alpha beta gamma".into()))
        );

        let c = app.render(80, 30);
        let (alpha_x, y) = point(&c, "alpha");
        app.mouse(
            event(MouseEventKind::Down(MouseButton::Left), alpha_x, y),
            &c,
        );
        app.mouse(
            event(MouseEventKind::Drag(MouseButton::Left), beta_x + 1, y),
            &c,
        );
        let selected = app.render(80, 30);
        assert_eq!(
            app.mouse(
                event(MouseEventKind::Up(MouseButton::Left), beta_x + 1, y),
                &selected,
            ),
            Some(Intent::Copy("alpha be".into()))
        );
        let selected = app.render(80, 30);
        assert_eq!(
            app.mouse(
                event_with(
                    MouseEventKind::Down(MouseButton::Left),
                    gamma_x + 4,
                    y,
                    KeyModifiers::SHIFT,
                ),
                &selected,
            ),
            None
        );
        let extended = app.render(80, 30);
        assert_eq!(
            app.mouse(
                event_with(
                    MouseEventKind::Up(MouseButton::Left),
                    gamma_x + 4,
                    y,
                    KeyModifiers::SHIFT,
                ),
                &extended,
            ),
            Some(Intent::Copy("alpha beta gamma".into()))
        );
    }

    #[test]
    fn composer_double_triple_click_and_shift_click_match_native_text_selection() {
        let mut app = App::new(false);
        app.editor.insert("alpha beta\ngamma delta");
        let c = app.render(80, 30);
        let (beta_x, beta_y) = point(&c, "beta");
        let (gamma_x, gamma_y) = point(&c, "gamma");
        let (delta_x, delta_y) = point(&c, "delta");

        assert_eq!(click(&mut app, &c, beta_x + 1, beta_y), None);
        assert_eq!(
            click(&mut app, &c, beta_x + 1, beta_y),
            Some(Intent::Copy("beta".into()))
        );
        assert_eq!(
            click(&mut app, &c, beta_x + 1, beta_y),
            Some(Intent::Copy("alpha beta".into()))
        );

        let c = app.render(80, 30);
        assert_eq!(click(&mut app, &c, gamma_x, gamma_y), None);
        let start = app.editor.cursor;
        app.mouse(
            event_with(
                MouseEventKind::Down(MouseButton::Left),
                delta_x + 4,
                delta_y,
                KeyModifiers::SHIFT,
            ),
            &c,
        );
        let selected = app.render(80, 30);
        let copied = app.mouse(
            event_with(
                MouseEventKind::Up(MouseButton::Left),
                delta_x + 4,
                delta_y,
                KeyModifiers::SHIFT,
            ),
            &selected,
        );
        assert!(app.editor.selection().is_some());
        assert_eq!(app.editor.anchor, Some(start));
        assert_eq!(copied, Some(Intent::Copy("gamma delt".into())));
    }
    #[test]
    fn markdown_link_clicks_without_focus_and_drag_still_selects_link_text() {
        let mut app = App::new(false);
        app.set_thread(&json!({"id":"source","turns":[{"id":"turn","status":"completed","items":[{"id":"link","type":"agentMessage","text":"앞 [이동](https://example.com) 뒤"}]}]}));
        let c = app.render(80, 30);
        let (x, y) = point(&c, "이동");
        let url = Action::OpenLink("https://example.com".into());
        assert_eq!(c.hit(x, y), Some(url.clone()));
        assert_eq!(
            click(&mut app, &c, x, y),
            Some(Intent::OpenLink("https://example.com".into()))
        );
        assert!(app.focus.is_none());
        app.mouse(event(MouseEventKind::Down(MouseButton::Left), x, y), &c);
        app.mouse(event(MouseEventKind::Drag(MouseButton::Left), x + 2, y), &c);
        assert_eq!(
            app.mouse(event(MouseEventKind::Up(MouseButton::Left), x + 2, y), &c),
            Some(Intent::Copy("이동".into()))
        );
    }
    #[test]
    fn dragging_across_tool_rows_copies_command_and_output_text() {
        let mut app = App::new(false);
        app.entries.push(Entry {
            id: "tool-copy".into(),
            kind: Kind::Tool,
            title: "cargo test".into(),
            body: "line one\nline two".into(),
            status: "completed".into(),
            expanded: true,
        });
        let c = app.render(80, 30);
        let (start_x, start_y) = point(&c, "cargo test");
        let (end_x, end_y) = point(&c, "line two");

        app.mouse(
            event(MouseEventKind::Down(MouseButton::Left), start_x, start_y),
            &c,
        );
        app.mouse(
            event(MouseEventKind::Drag(MouseButton::Left), end_x + 8, end_y),
            &c,
        );
        let Some(Intent::Copy(copied)) = app.mouse(
            event(MouseEventKind::Up(MouseButton::Left), end_x + 8, end_y),
            &c,
        ) else {
            panic!("tool transcript drag did not copy");
        };
        assert!(copied.contains("cargo test"), "{copied}");
        assert!(copied.contains("line one"), "{copied}");
        assert!(copied.contains("line two"), "{copied}");
    }
    #[test]
    fn completed_text_can_be_selected_through_the_old_branch_gutter() {
        let mut app = App::new(false);
        app.set_thread(&json!({"id":"source","turns":[{"id":"one","status":"completed","items":[{"id":"a","type":"agentMessage","text":format!("first {}", "x".repeat(64))}]},{"id":"two","status":"completed","items":[{"id":"b","type":"agentMessage","text":"later"}]}]}));
        app.editor.insert("draft");
        for width in [40, 80, 120] {
            let c = app.render(width, 30);
            let h = c
                .hits
                .iter()
                .find(|h| h.action == Action::SelectEntry("a".into()))
                .unwrap();
            let end = h.x + h.width - 1;
            assert!(c.hits.iter().all(|h| !matches!(h.action, Action::Branch(_))));
            assert_eq!(
                app.mouse(
                    event(MouseEventKind::Down(MouseButton::Left), h.x + 2, h.y),
                    &c
                ),
                None
            );
            app.mouse(event(MouseEventKind::Drag(MouseButton::Left), end, h.y), &c);
            let Some(Intent::Copy(text)) = app.mouse(event(MouseEventKind::Up(MouseButton::Left), end, h.y), &c) else {
                panic!("full-width message selection must copy text");
            };
            assert!(text.contains("first") && text.contains('x'), "{text}");
            assert!(app.branch_pending.is_none());
            assert_eq!(app.editor.text, "draft");
            assert!(app.focus.is_none());
            assert!(c.hits.iter().all(|h| !matches!(h.action, Action::Copy(_))));
        }
    }
    #[test]
    fn input_click_drag_replace_and_paste_expansion_use_grapheme_boundaries() {
        let mut app = App::new(false);
        app.editor.insert("가e\u{301} 🦀\nsecond");
        let c = app.render(40, 24);
        let h = c.hits.iter().find(|h| h.action == Action::Input).unwrap();
        click(&mut app, &c, 6, h.y); // after 가, before the combining grapheme
        assert_eq!(app.editor.cursor, "가".len());
        app.key(KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE), &c);
        assert_eq!(app.editor.text, "가!e\u{301} 🦀\nsecond");
        let c = app.render(40, 24);
        let h = c.hits.iter().find(|h| h.action == Action::Input).unwrap();
        app.mouse(event(MouseEventKind::Down(MouseButton::Left), 4, h.y), &c);
        app.mouse(event(MouseEventKind::Drag(MouseButton::Left), 7, h.y), &c);
        let selection = app.render(40, 24);
        assert_eq!(
            selection.cells[usize::from(h.y) * 40 + 4].background,
            Background::Selected
        );
        app.mouse(event(MouseEventKind::Up(MouseButton::Left), 7, h.y), &c);
        app.editor.insert("替");
        assert_eq!(app.editor.text, "替e\u{301} 🦀\nsecond");
        app.key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::CONTROL), &c);
        app.paste("새\n입력");
        assert_eq!(app.editor.text, "새\n입력");
        app.key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL), &c);
        assert!(!app.editor.display().0.contains("붙여넣은 내용"));
        let c = app.render(40, 24);
        let h = c.hits.iter().find(|h| h.action == Action::Input).unwrap();
        click(&mut app, &c, 4, h.y);
        app.editor.insert("앞");
        assert_eq!(app.editor.text, "앞새\n입력");
    }
    #[test]
    fn wrapped_composer_clicks_track_raw_offsets_after_resize_and_scrolling() {
        let mut app = App::new(false);
        app.editor
            .insert(&format!("{}\n끝", "한글e\u{301}🦀".repeat(50)));
        for width in [40, 80, 120] {
            let c = app.render(width, 24);
            let last = c
                .hits
                .iter()
                .filter(|h| h.action == Action::Input)
                .last()
                .unwrap();
            click(&mut app, &c, 4, last.y);
            assert_eq!(app.editor.cursor, app.editor.text.len() - "끝".len());
            app.editor.cursor = app.editor.text.len();
        }
        assert_eq!(
            composer_rows("1234\nx", 4)
                .iter()
                .map(|r| r.text.as_str())
                .collect::<Vec<_>>(),
            vec!["1234", "x"]
        );
        assert_eq!(composer_rows("1234", 4).len(), 2);
    }
}
