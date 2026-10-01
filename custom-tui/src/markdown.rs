//! Markdown is a display projection. Original text remains in the message entry.
use super::{Row, append_block};
use crate::engine::Tone;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Debug)]
pub struct Span {
    pub text: String,
    pub tone: Tone,
}
#[derive(Clone, Debug)]
pub struct StyledRow {
    pub text: String,
    pub tone: Tone,
    pub spans: Vec<Span>,
}
impl StyledRow {
    pub fn plain(text: String, tone: Tone) -> Self {
        Self {
            spans: vec![Span {
                text: text.clone(),
                tone,
            }],
            text,
            tone,
        }
    }
    fn from_spans(spans: Vec<Span>) -> Self {
        Self {
            text: spans.iter().map(|s| s.text.as_str()).collect(),
            tone: Tone::Normal,
            spans,
        }
    }
}
fn push(spans: &mut Vec<Span>, text: &str, tone: Tone) {
    if let Some(last) = spans.last_mut().filter(|s| s.tone == tone) {
        last.text.push_str(text);
    } else if !text.is_empty() {
        spans.push(Span {
            text: text.into(),
            tone,
        });
    }
}
fn text(spans: &[Span]) -> String {
    spans.iter().map(|s| s.text.as_str()).collect()
}
fn width(spans: &[Span]) -> usize {
    UnicodeWidthStr::width(text(spans).as_str())
}

/// Wrap visible graphemes rather than Markdown bytes, preserving inline styles.
fn wrap_spans(spans: &[Span], columns: usize, first: &str, continuation: &str) -> Vec<StyledRow> {
    let columns = columns.max(2);
    let prefix = |s: &str| -> String {
        let mut used = 0;
        s.graphemes(true)
            .take_while(|g| {
                used += UnicodeWidthStr::width(*g);
                used <= columns.saturating_sub(2)
            })
            .collect()
    };
    let first = prefix(first);
    let continuation = prefix(continuation);
    let mut rows = Vec::new();
    let mut line = vec![Span {
        text: first.clone(),
        tone: Tone::Muted,
    }];
    let mut used = UnicodeWidthStr::width(first.as_str());
    for span in spans {
        for grapheme in span.text.graphemes(true) {
            if grapheme == "\n" {
                rows.push(StyledRow::from_spans(std::mem::take(&mut line)));
                line = vec![Span {
                    text: continuation.clone(),
                    tone: Tone::Muted,
                }];
                used = UnicodeWidthStr::width(continuation.as_str());
                continue;
            }
            if grapheme.chars().any(char::is_control) && grapheme != "\t" {
                continue;
            }
            let expanded;
            let grapheme = if grapheme == "\t" {
                expanded = " ".repeat(4 - used % 4);
                expanded.as_str()
            } else {
                grapheme
            };
            let size = UnicodeWidthStr::width(grapheme);
            if used + size > columns {
                rows.push(StyledRow::from_spans(std::mem::take(&mut line)));
                line = vec![Span {
                    text: continuation.clone(),
                    tone: Tone::Muted,
                }];
                used = UnicodeWidthStr::width(continuation.as_str());
            }
            push(&mut line, grapheme, span.tone);
            used += size;
        }
    }
    if !line.is_empty() {
        rows.push(StyledRow::from_spans(line));
    }
    rows
}

#[derive(Default)]
struct Table {
    rows: Vec<Vec<Vec<Span>>>,
    cells: Vec<Vec<Span>>,
}
impl Table {
    fn render(self, columns: usize) -> Vec<StyledRow> {
        let count = self.rows.first().map_or(0, Vec::len);
        if count == 0 {
            return Vec::new();
        }
        let mut rows = Vec::new();
        if columns < count.saturating_mul(8) + (count - 1) * 3 {
            // Stacked records keep every value readable in a narrow pane.
            for record in self.rows.iter().skip(1) {
                for (index, value) in record.iter().enumerate() {
                    let mut spans = Vec::new();
                    if let Some(header) = self.rows[0].get(index) {
                        push(&mut spans, &format!("{}: ", text(header)), Tone::Strong);
                    }
                    spans.extend(value.clone());
                    rows.extend(wrap_spans(&spans, columns, "", "  "));
                }
                rows.push(StyledRow::plain(String::new(), Tone::Normal));
            }
            if self.rows.len() == 1 {
                for cell in &self.rows[0] {
                    rows.extend(wrap_spans(cell, columns, "", ""));
                }
            }
            return rows;
        }
        let mut widths: Vec<_> = (0..count)
            .map(|i| {
                self.rows
                    .iter()
                    .filter_map(|r| r.get(i))
                    .map(|c| width(c).clamp(3, 40))
                    .max()
                    .unwrap_or(3)
            })
            .collect();
        let available = columns.saturating_sub((count - 1) * 3);
        while widths.iter().sum::<usize>() > available {
            let index = widths
                .iter()
                .enumerate()
                .max_by_key(|(_, w)| **w)
                .unwrap()
                .0;
            widths[index] -= 1;
        }
        for (row_index, record) in self.rows.iter().enumerate() {
            let cells: Vec<_> = widths
                .iter()
                .enumerate()
                .map(|(i, w)| wrap_spans(record.get(i).map_or(&[][..], Vec::as_slice), *w, "", ""))
                .collect();
            let height = cells.iter().map(Vec::len).max().unwrap_or(1);
            for line_index in 0..height {
                let mut spans = Vec::new();
                for (column, cell) in cells.iter().enumerate() {
                    if column > 0 {
                        push(&mut spans, " │ ", Tone::Faint);
                    }
                    let value = cell.get(line_index);
                    if let Some(value) = value {
                        for span in &value.spans {
                            push(
                                &mut spans,
                                &span.text,
                                if row_index == 0 {
                                    Tone::Strong
                                } else {
                                    span.tone
                                },
                            );
                        }
                    }
                    let used = value.map_or(0, |v| UnicodeWidthStr::width(v.text.as_str()));
                    push(
                        &mut spans,
                        &" ".repeat(widths[column].saturating_sub(used)),
                        Tone::Normal,
                    );
                }
                rows.push(StyledRow::from_spans(spans));
            }
            if row_index == 0 {
                rows.push(StyledRow::plain(
                    widths
                        .iter()
                        .map(|w| "─".repeat(*w))
                        .collect::<Vec<_>>()
                        .join("─┼─"),
                    Tone::Faint,
                ));
            }
        }
        rows
    }
}
struct Renderer {
    columns: usize,
    rows: Vec<StyledRow>,
    spans: Vec<Span>,
    styles: Vec<Tone>,
    quote: usize,
    lists: Vec<Option<u64>>,
    prefix: String,
    item_prefixes: Vec<String>,
    code: Option<(String, String)>,
    table: Option<Table>,
    links: Vec<(String, usize)>,
}
impl Renderer {
    fn tone(&self) -> Tone {
        self.styles.last().copied().unwrap_or(Tone::Normal)
    }
    fn paragraph(&mut self) {
        if self.spans.is_empty() {
            return;
        }
        let quote = "│ ".repeat(self.quote);
        let first = format!("{quote}{}", self.prefix);
        let continuation = format!(
            "{quote}{}",
            " ".repeat(UnicodeWidthStr::width(self.prefix.as_str()))
        );
        self.rows.extend(wrap_spans(
            &std::mem::take(&mut self.spans),
            self.columns,
            &first,
            &continuation,
        ));
    }
    fn gap(&mut self) {
        if self.rows.last().is_some_and(|r| !r.text.is_empty()) {
            self.rows
                .push(StyledRow::plain(String::new(), Tone::Normal));
        }
    }
    fn event(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => match tag {
                Tag::Heading { .. } => {
                    self.paragraph();
                    self.styles.push(Tone::Strong);
                }
                Tag::Strong => self.styles.push(Tone::Strong),
                Tag::Emphasis => self.styles.push(Tone::Emphasis),
                Tag::Strikethrough => self.styles.push(Tone::Muted),
                Tag::BlockQuote => {
                    self.paragraph();
                    self.quote += 1;
                }
                Tag::List(start) => {
                    self.paragraph();
                    self.lists.push(start);
                }
                Tag::Item => {
                    self.paragraph();
                    self.item_prefixes.push(self.prefix.clone());
                    let marker = match self.lists.last_mut() {
                        Some(Some(n)) => {
                            let label = format!("{n}. ");
                            *n = n.saturating_add(1);
                            label
                        }
                        _ => "• ".into(),
                    };
                    self.prefix = format!(
                        "{}{marker}",
                        "  ".repeat(self.lists.len().saturating_sub(1))
                    );
                }
                Tag::CodeBlock(kind) => {
                    self.paragraph();
                    self.code = Some((
                        match kind {
                            CodeBlockKind::Fenced(s) => s.into_string(),
                            _ => String::new(),
                        },
                        String::new(),
                    ));
                }
                Tag::Table(_) => {
                    self.paragraph();
                    self.table = Some(Table::default());
                }
                Tag::TableCell => self.spans.clear(),
                Tag::Link { dest_url, .. } | Tag::Image { dest_url, .. } => {
                    self.links
                        .push((dest_url.into_string(), text(&self.spans).len()));
                    self.styles.push(Tone::Accent);
                }
                Tag::FootnoteDefinition(label) => {
                    push(&mut self.spans, &format!("[{label}] "), Tone::Muted)
                }
                _ => {}
            },
            Event::End(tag) => match tag {
                TagEnd::Paragraph | TagEnd::Heading(_) => {
                    self.paragraph();
                    if matches!(tag, TagEnd::Heading(_)) {
                        self.styles.pop();
                    }
                    if self.lists.is_empty() {
                        self.gap();
                    } else {
                        self.prefix = " ".repeat(UnicodeWidthStr::width(self.prefix.as_str()));
                    }
                }
                TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough => {
                    self.styles.pop();
                }
                TagEnd::BlockQuote => {
                    self.paragraph();
                    self.quote = self.quote.saturating_sub(1);
                    self.gap();
                }
                TagEnd::Item => {
                    self.paragraph();
                    self.prefix = self.item_prefixes.pop().unwrap_or_default();
                }
                TagEnd::List(_) => {
                    self.paragraph();
                    self.lists.pop();
                    if self.lists.is_empty() {
                        self.gap();
                    }
                }
                TagEnd::CodeBlock => {
                    if let Some((language, body)) = self.code.take() {
                        let mut plain: Vec<Row> = Vec::new();
                        append_block(&mut plain, &body, &language, self.columns);
                        self.rows.extend(
                            plain
                                .into_iter()
                                .map(|(text, tone)| StyledRow::plain(text, tone)),
                        );
                    }
                }
                TagEnd::TableCell => {
                    if let Some(table) = self.table.as_mut() {
                        table.cells.push(std::mem::take(&mut self.spans));
                    }
                }
                TagEnd::TableHead | TagEnd::TableRow => {
                    if let Some(table) = self.table.as_mut() {
                        table.rows.push(std::mem::take(&mut table.cells));
                    }
                }
                TagEnd::Table => {
                    if let Some(table) = self.table.take() {
                        self.rows.extend(table.render(self.columns));
                        self.gap();
                    }
                }
                TagEnd::Link | TagEnd::Image => {
                    self.styles.pop();
                    if let Some((url, start)) = self.links.pop() {
                        let label = text(&self.spans);
                        if !url.is_empty() && label.get(start..) != Some(url.as_str()) {
                            push(&mut self.spans, &format!(" ({url})"), Tone::Muted);
                        }
                    }
                }
                _ => {}
            },
            Event::Text(value) => {
                if let Some((_, code)) = self.code.as_mut() {
                    code.push_str(&value);
                } else {
                    let tone = self.tone();
                    push(&mut self.spans, &value, tone);
                }
            }
            Event::Code(value) => push(&mut self.spans, &value, Tone::InlineCode),
            Event::Html(value) | Event::InlineHtml(value) => {
                push(&mut self.spans, &value, Tone::Muted)
            }
            Event::SoftBreak | Event::HardBreak => {
                // Preserve LLM-authored line breaks without losing hanging indentation.
                self.paragraph();
                self.prefix = " ".repeat(UnicodeWidthStr::width(self.prefix.as_str()));
            }
            Event::Rule => {
                self.paragraph();
                self.rows
                    .push(StyledRow::plain("─".repeat(self.columns), Tone::Faint));
            }
            Event::TaskListMarker(checked) => push(
                &mut self.spans,
                if checked { "☑ " } else { "☐ " },
                Tone::Accent,
            ),
            Event::FootnoteReference(label) => {
                push(&mut self.spans, &format!("[{label}]"), Tone::Muted)
            }
        }
    }
}
pub fn styled_message_rows(source: &str, columns: usize) -> Vec<StyledRow> {
    let mut renderer = Renderer {
        columns: columns.max(2),
        rows: Vec::new(),
        spans: Vec::new(),
        styles: Vec::new(),
        quote: 0,
        lists: Vec::new(),
        prefix: String::new(),
        item_prefixes: Vec::new(),
        code: None,
        table: None,
        links: Vec::new(),
    };
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_FOOTNOTES;
    for event in Parser::new_ext(source, options) {
        renderer.event(event);
    }
    renderer.paragraph();
    while renderer.rows.last().is_some_and(|r| r.text.is_empty()) {
        renderer.rows.pop();
    }
    renderer.rows
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn headings_inline_styles_escapes_links_and_lists_render_visible_text() {
        let source = "## 제목\n\n**강조**와 *설명*, `a_b()` 및 \\*문자\\*. [파일](src/app.rs)\n\n3. 항목\n   - [x] 확인\n   - [ ] 대기\n\n> 인용";
        let rows = styled_message_rows(source, 36);
        let plain = rows
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(plain.contains("제목") && !plain.contains("## 제목"));
        assert!(plain.contains("강조") && !plain.contains("**강조**"));
        assert!(plain.contains("a_b()") && !plain.contains('`'));
        let flat = plain.replace('\n', "");
        assert!(flat.contains("*문자*") && flat.contains("(src/app.rs)"));
        assert!(
            plain.contains("3. 항목") && plain.contains("  • ☑ 확인") && plain.contains("│ 인용")
        );
        assert!(
            rows.iter()
                .flat_map(|r| &r.spans)
                .any(|s| s.tone == Tone::InlineCode)
        );
        assert!(
            rows.iter()
                .flat_map(|r| &r.spans)
                .any(|s| s.tone == Tone::Strong)
        );
        assert!(
            rows.iter()
                .all(|r| UnicodeWidthStr::width(r.text.as_str()) <= 36)
        );
    }
    #[test]
    fn unicode_tables_wrap_cells_and_stack_without_losing_values() {
        let source = "| 이름 | 설명 |\n| :--- | ---: |\n| 한글 | 아주긴한글설명과 `a\\|b` |\n| 다른 값 | 두 번째 |";
        for columns in [12, 24, 48] {
            let rows = styled_message_rows(source, columns);
            assert!(
                rows.iter()
                    .all(|r| UnicodeWidthStr::width(r.text.as_str()) <= columns)
            );
            let compact: String = rows
                .iter()
                .map(|r| r.text.as_str())
                .collect::<String>()
                .chars()
                .filter(|c| !c.is_whitespace() && !matches!(c, '│' | '─' | '┼' | ':'))
                .collect();
            assert!(
                compact.contains("아주긴한글설명과a|b"),
                "{columns}: {compact}"
            );
            assert!(compact.contains("두번째"));
        }
    }
    #[test]
    fn streaming_unicode_fences_and_partial_markers_are_bounded_and_literal_code_is_preserved() {
        let source = "**진행**\n\n```rust\nlet 값 = \"**원문**\";\n```\n- 한글 e\u{301} 🦀\n";
        for end in source
            .char_indices()
            .map(|(i, _)| i)
            .chain(std::iter::once(source.len()))
        {
            let rows = styled_message_rows(&source[..end], 24);
            assert!(
                rows.iter()
                    .all(|r| UnicodeWidthStr::width(r.text.as_str()) <= 24)
            );
        }
        assert!(
            styled_message_rows(source, 60)
                .iter()
                .any(|r| r.tone == Tone::Code && r.text.contains("**원문**"))
        );
    }
    #[test]
    fn inline_code_style_survives_wrapping_and_painting() {
        let rows = styled_message_rows("앞 **강조** `긴_한글_함수이름()` 뒤", 18);
        let mut inline = String::new();
        for row in rows {
            let mut canvas = crate::engine::Canvas::new(20, 1);
            super::super::paint_message(&mut canvas, &row, "  ");
            assert_eq!(
                canvas.plain_line(0).trim_end(),
                format!("  {}", row.text).trim_end()
            );
            for span in row.spans.iter().filter(|s| s.tone == Tone::InlineCode) {
                inline.push_str(&span.text);
            }
        }
        assert_eq!(inline, "긴_한글_함수이름()");
    }
}
