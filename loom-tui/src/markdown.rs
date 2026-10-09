//! Markdown is a display projection. Original text remains in the message entry.
use super::{Row, append_block};
use crate::engine::Tone;
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

#[path = "notation.rs"]
mod notation;

#[derive(Clone, Debug)]
pub struct Span {
    pub text: String,
    pub tone: Tone,
    pub link: Option<String>,
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
                link: None,
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
    push_link(spans, text, tone, None);
}
fn push_link(spans: &mut Vec<Span>, text: &str, tone: Tone, link: Option<&str>) {
    if let Some(last) = spans
        .last_mut()
        .filter(|s| s.tone == tone && s.link.as_deref() == link)
    {
        last.text.push_str(text);
    } else if !text.is_empty() {
        spans.push(Span {
            text: text.into(),
            tone,
            link: link.map(str::to_owned),
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
        tone: if first.starts_with('│') {
            Tone::Accent
        } else {
            Tone::Muted
        },
        link: None,
    }];
    let mut used = UnicodeWidthStr::width(first.as_str());
    for span in spans {
        for grapheme in span.text.graphemes(true) {
            if grapheme == "\n" {
                rows.push(StyledRow::from_spans(std::mem::take(&mut line)));
                line = vec![Span {
                    text: continuation.clone(),
                    tone: if continuation.starts_with('│') {
                        Tone::Accent
                    } else {
                        Tone::Muted
                    },
                    link: None,
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
                    tone: if continuation.starts_with('│') {
                        Tone::Accent
                    } else {
                        Tone::Muted
                    },
                    link: None,
                }];
                used = UnicodeWidthStr::width(continuation.as_str());
            }
            push_link(&mut line, grapheme, span.tone, span.link.as_deref());
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
    heading_prefix: Option<String>,
    item_prefixes: Vec<String>,
    code: Option<(String, String)>,
    table: Option<Table>,
    links: Vec<(String, usize)>,
}
impl Renderer {
    fn tone(&self) -> Tone {
        let strong = self.styles.contains(&Tone::Strong);
        let emphasis = self.styles.contains(&Tone::Emphasis);
        if strong && emphasis {
            Tone::StrongEmphasis
        } else {
            self.styles.last().copied().unwrap_or(Tone::Normal)
        }
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
                Tag::Heading { level, .. } => {
                    self.paragraph();
                    self.heading_prefix = Some(std::mem::take(&mut self.prefix));
                    self.prefix = if level == pulldown_cmark::HeadingLevel::H1 {
                        "▌ ".into()
                    } else {
                        "▍ ".into()
                    };
                    self.styles.push(Tone::Strong);
                }
                Tag::Strong => self.styles.push(Tone::Strong),
                Tag::Emphasis => self.styles.push(Tone::Emphasis),
                Tag::Strikethrough => self.styles.push(Tone::Strike),
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
                        self.prefix = self.heading_prefix.take().unwrap_or_default();
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
                        if language == "mermaid"
                            && let Some(lines) = notation::flowchart(&body)
                        {
                            for line in lines {
                                self.rows.extend(wrap_spans(
                                    &[Span {
                                        text: line,
                                        tone: Tone::Accent,
                                        link: None,
                                    }],
                                    self.columns,
                                    "",
                                    "  ",
                                ));
                            }
                            self.gap();
                            return;
                        }
                        if matches!(language.split_whitespace().next(), Some("math" | "latex")) {
                            self.rows.extend(wrap_spans(
                                &[Span {
                                    text: notation::formula(body.trim()),
                                    tone: Tone::Math,
                                    link: None,
                                }],
                                self.columns,
                                "  ",
                                "  ",
                            ));
                            self.gap();
                            return;
                        }
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
                    push_link(
                        &mut self.spans,
                        &value,
                        tone,
                        self.links.last().map(|(url, _)| url.as_str()),
                    );
                }
            }
            // Padding gives inline code a distinct chip-like background.
            Event::Code(value) => {
                if let Some(math) = value.strip_prefix(notation::INLINE_MATH_MARKER) {
                    push(&mut self.spans, math, Tone::Math);
                } else {
                    push(&mut self.spans, &format!(" {value} "), Tone::InlineCode);
                }
            }
            Event::Html(value) | Event::InlineHtml(value) => match value.trim() {
                "<u>" | "<underline>" => self.styles.push(Tone::Underline),
                "</u>" | "</underline>" if self.styles.last() == Some(&Tone::Underline) => {
                    self.styles.pop();
                }
                _ => push(&mut self.spans, &value, Tone::Muted),
            },
            Event::SoftBreak | Event::HardBreak => {
                // Preserve LLM-authored line breaks without losing hanging indentation.
                if self.table.is_some() {
                    let tone = self.tone();
                    push(&mut self.spans, " ", tone);
                } else {
                    self.paragraph();
                    self.prefix = " ".repeat(UnicodeWidthStr::width(self.prefix.as_str()));
                }
            }
            Event::Rule => {
                self.paragraph();
                self.rows
                    .push(StyledRow::plain("─".repeat(self.columns), Tone::Faint));
            }
            Event::TaskListMarker(checked) => {
                // The checkbox replaces the ordinary unordered bullet.
                if self.spans.is_empty() && self.prefix.ends_with("• ") {
                    self.prefix.truncate(self.prefix.len() - "• ".len());
                }
                push(
                    &mut self.spans,
                    if checked { "☑ " } else { "☐ " },
                    if checked { Tone::Success } else { Tone::Muted },
                );
            }
            Event::FootnoteReference(label) => {
                push(&mut self.spans, &format!("[{label}]"), Tone::Muted)
            }
        }
    }
}
/// Repair line-broken pipe tables only when a complete separator establishes
/// the column count. Code fences and incomplete rows remain literal.
fn repair_split_tables(source: &str) -> String {
    let lines: Vec<_> = source.lines().collect();
    let mut output: Vec<String> = Vec::new();
    let mut index = 0;
    let mut fence: Option<char> = None;
    fn pipes(line: &str) -> usize {
        let mut escaped = false;
        line.chars()
            .filter(|c| {
                if escaped {
                    escaped = false;
                    return false;
                }
                if *c == '\\' {
                    escaped = true;
                    return false;
                }
                *c == '|'
            })
            .count()
    }
    fn row(lines: &[&str], columns: usize) -> Option<String> {
        if lines.iter().any(|line| {
            line.trim().starts_with("```")
                || line.trim().starts_with("~~~")
                || line.trim().starts_with('#')
        }) {
            return None;
        }
        let joined = lines.join(" ");
        let trimmed = joined.trim();
        if !trimmed.starts_with('|') || !trimmed.ends_with('|') || pipes(trimmed) != columns + 1 {
            return None;
        }
        Some(trimmed.split_whitespace().collect::<Vec<_>>().join(" "))
    }
    while index < lines.len() {
        let line = lines[index].trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            let marker = line.chars().next().unwrap();
            if fence == Some(marker) {
                fence = None;
            } else if fence.is_none() {
                fence = Some(marker);
            }
        }
        let columns =
            if fence.is_none() && line.len() > 1 && line.starts_with('|') && line.ends_with('|') {
                let cells: Vec<_> = line[1..line.len() - 1].split('|').map(str::trim).collect();
                if !cells.is_empty()
                    && cells.iter().all(|c| {
                        c.trim_matches(':').len() >= 3
                            && c.trim_matches(':').chars().all(|c| c == '-')
                    })
                {
                    cells.len()
                } else {
                    0
                }
            } else {
                0
            };
        if columns > 0 && index > 0 && row(&lines[index - 1..index], columns).is_none() {
            let start = (index.saturating_sub(128)..index).rev().find(|start| {
                lines[*start].trim().starts_with('|')
                    && row(&lines[*start..index], columns).is_some()
            });
            if let Some(start) = start {
                // Previously emitted lines correspond one-to-one until this repair.
                let remove = index - start;
                if remove <= output.len() {
                    output.truncate(output.len() - remove);
                    output.push(row(&lines[start..index], columns).unwrap());
                    output.push(lines[index].into());
                    index += 1;
                    while index < lines.len() {
                        let start = index;
                        while index < lines.len() && lines[index].trim().is_empty() {
                            index += 1;
                        }
                        if index == lines.len() || !lines[index].trim().starts_with('|') {
                            index = start;
                            break;
                        }
                        let begin = index;
                        let end = (begin..lines.len())
                            .find(|end| row(&lines[begin..=*end], columns).is_some());
                        let Some(end) = end else {
                            index = start;
                            break;
                        };
                        output.push(row(&lines[begin..=end], columns).unwrap());
                        index = end + 1;
                    }
                    continue;
                }
            }
        }
        output.push(lines[index].into());
        index += 1;
    }
    if source.ends_with('\n') {
        output.push(String::new());
    }
    output.join("\n")
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
        heading_prefix: None,
        item_prefixes: Vec::new(),
        code: None,
        table: None,
        links: Vec::new(),
    };
    let options = Options::ENABLE_TABLES
        | Options::ENABLE_TASKLISTS
        | Options::ENABLE_STRIKETHROUGH
        | Options::ENABLE_FOOTNOTES;
    let normalized = notation::math_markdown(&collapse_details(&repair_split_tables(source)));
    for event in Parser::new_ext(&normalized, options) {
        renderer.event(event);
    }
    renderer.paragraph();
    while renderer.rows.last().is_some_and(|r| r.text.is_empty()) {
        renderer.rows.pop();
    }
    renderer.rows
}

/// Show the summary of closed HTML details while keeping their body out of the
/// reading surface. Leave fenced code alone: its HTML is just example text.
fn collapse_details(source: &str) -> String {
    let mut result = String::new();
    let mut hidden = 0usize;
    let mut fence: Option<(char, usize)> = None;
    for line in source.split_inclusive('\n') {
        let trimmed = line.trim();
        let marker = trimmed.chars().next().unwrap_or(' ');
        let run = trimmed.chars().take_while(|c| *c == marker).count();
        if hidden == 0 && (marker == '`' || marker == '~') && run >= 3 {
            match fence {
                None => fence = Some((marker, run)),
                Some((kind, size)) if kind == marker && run >= size => fence = None,
                _ => {}
            }
        }
        if fence.is_some() {
            result.push_str(line);
            continue;
        }
        if hidden > 0 {
            if trimmed.contains("<details") {
                hidden += trimmed.matches("<details").count();
            }
            hidden = hidden.saturating_sub(trimmed.matches("</details>").count());
            continue;
        }
        if trimmed.starts_with("<details")
            && !trimmed.starts_with("<details open")
            && let Some(start) = trimmed.find("<summary>")
            && let Some(end) = trimmed.find("</summary>")
        {
            let summary = &trimmed[start + "<summary>".len()..end];
            result.push_str(&format!("접힘 · {summary}\n"));
            hidden = 1usize.saturating_sub(trimmed.matches("</details>").count());
        } else {
            result.push_str(line);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn line_broken_tables_from_chat_keep_cells_and_original_code() {
        let source = "| \n티어\n\n | \n설명\n\n |\n| --- | --- |\n|\nT0\n\n |\n간단한 수정\n\n |\n\n다음 문단";
        let rows = styled_message_rows(source, 60);
        let text = rows
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("티어") && text.contains("T0") && text.contains("간단한 수정"));
        assert!(text.contains('┼') && !text.contains("| --- |"));
        assert!(text.contains("다음 문단"));
        let code = format!("```text\n{source}\n```");
        assert_eq!(repair_split_tables(&code), code);
    }
    #[test]
    fn formulas_and_flowcharts_render_without_code_syntax() {
        let source = "$$\\frac{a_1}{b} \\times 100\\%$$\n\n```mermaid\nflowchart TD\nA[Core] --> B[T0]\nA --> C[T1]\n```";
        let rows = styled_message_rows(source, 60);
        let text = rows
            .iter()
            .map(|r| r.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("(a₁)/(b) × 100%"), "{text}");
        assert!(text.contains("Core → T0") && text.contains("Core → T1"));
        assert!(!text.contains("flowchart TD"));
    }

    #[test]
    fn collapsed_details_display_only_the_summary() {
        let rows = styled_message_rows(
            "일반 본문\n\n<details><summary>접기 요약</summary>\n\n숨은 내용\n\n</details>\n\n마지막 본문",
            60,
        );
        let plain = rows
            .iter()
            .map(|row| row.text.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            plain.contains("접기 요약") && plain.contains("마지막 본문"),
            "{plain}"
        );
        assert!(
            !plain.contains("숨은 내용") && !plain.contains("<details>"),
            "{plain}"
        );
    }
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
            plain.contains("3. 항목") && plain.contains("  ☑ 확인") && plain.contains("│ 인용")
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
    fn combined_emphasis_quote_and_inline_code_survive_wrapping() {
        let source = "> **굵은 *기울임***과 `code()` 뒤쪽 문장";
        let rows = styled_message_rows(source, 19);
        assert!(rows.len() >= 2);
        assert!(rows.iter().all(|r| r.text.starts_with("│ ")));
        assert!(rows.iter().any(|r| {
            r.spans
                .iter()
                .any(|s| s.tone == Tone::StrongEmphasis && s.text.contains("기울임"))
        }));
        let code = rows
            .iter()
            .flat_map(|r| r.spans.iter())
            .filter(|s| s.tone == Tone::InlineCode)
            .map(|s| s.text.as_str())
            .collect::<String>();
        assert_eq!(code, " code() ");
        let mut painted = false;
        for row in &rows {
            let mut canvas = crate::engine::Canvas::new(24, 1);
            super::super::paint_message(&mut canvas, row, "  ");
            if let Some(pos) = canvas.cells.iter().position(|c| c.tone == Tone::InlineCode) {
                assert_eq!(
                    canvas.cells[pos].background,
                    crate::engine::Background::Code
                );
                painted = true;
            }
            assert_eq!(canvas.cells[2].background, crate::engine::Background::Quote);
            assert_eq!(canvas.cells[2].tone, Tone::Accent);
        }
        assert!(painted);
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
        assert_eq!(inline, " 긴_한글_함수이름() ");
    }

    #[test]
    fn full_markdown_reply_displays_blocks_tasks_math_and_inline_styles() {
        // Reproduce the combined response format used for the reported failure.
        let source = concat!(
            "# 마크다운 렌더링 테스트\n\n",
            "## 텍스트\n\n**굵은 글씨**, *기울임*, ~~취소선~~, ",
            "<underline>밑줄</underline>, `인라인 코드`\n\n",
            "- 첫 번째 항목\n  - 중첩 항목\n\n",
            "- [x] 마크다운 작성\n- [ ] 렌더링 확인\n\n",
            "> **인용** 내용\n\n",
            "```python id=\"example\"\ndef hello():\n    print(\"Hello!\")\n```\n\n",
            "| 기능 | 상태 |\n| --- | --- |\n| Markdown | OK |\n\n",
            "\\[\nE = mc^2\n\\]\n\n",
            "인라인 수식: \\(x^2 + y^2\\).\n\n",
            "[GitHub](https://github.com)\n",
        );
        for columns in [24, 72] {
            let rows = styled_message_rows(source, columns);
            let flat = rows
                .iter()
                .map(|r| r.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");
            assert!(flat.contains("┌─ python") && flat.contains("└─"), "{flat}");
            assert!(
                flat.contains("def hello()") && flat.contains("Hello!"),
                "{flat}"
            );
            assert!(
                flat.contains("☑ 마크다운 작성") && flat.contains("☐ 렌더링 확인"),
                "{flat}"
            );
            assert!(
                flat.contains("E = mc²") && flat.contains("x² + y²"),
                "{flat}"
            );
            assert!(
                !flat.contains("<underline>") && !flat.contains("\\["),
                "{flat}"
            );
            assert!(flat.contains("Markdown") && flat.contains("OK"), "{flat}");
            let tones = rows
                .iter()
                .flat_map(|r| &r.spans)
                .map(|s| s.tone)
                .collect::<Vec<_>>();
            for expected in [
                Tone::Strong,
                Tone::Emphasis,
                Tone::Strike,
                Tone::Underline,
                Tone::InlineCode,
                Tone::Math,
                Tone::Success,
            ] {
                assert!(tones.contains(&expected), "missing {expected:?}");
            }
            assert!(
                rows.iter()
                    .any(|r| r.tone == Tone::Code && r.text.contains("print"))
            );
            assert!(
                rows.iter()
                    .all(|r| UnicodeWidthStr::width(r.text.as_str()) <= columns)
            );
        }
    }
}
