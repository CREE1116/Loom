//! Code and unified diff presentation, shared by messages, tools and review.
use crate::engine::{Background, Canvas, Tone, wrap, wrap_each};
use unicode_width::UnicodeWidthStr;

pub type Row = (String, Tone);

pub fn is_diff(text: &str) -> bool {
    text.lines().any(|line| line.starts_with("@@ ")) || text.starts_with("diff --git ")
}

fn range_start(range: &str) -> Option<usize> {
    range.get(1..)?.split(',').next()?.parse().ok()
}

fn code_line(
    emit: &mut dyn FnMut(Row) -> bool,
    gutter: &str,
    code: &str,
    tone: Tone,
    width: usize,
) -> bool {
    let gutter_width = UnicodeWidthStr::width(gutter);
    let available = width.saturating_sub(gutter_width).max(1);
    let mut index = 0;
    wrap_each(code, available, &mut |line| {
        let prefix = if index == 0 {
            gutter.to_owned()
        } else {
            // Continuation never advances the source line number.
            format!("{}│ ", " ".repeat(gutter_width.saturating_sub(2)))
        };
        index += 1;
        emit((format!("{prefix}{line}"), tone))
    })
}

fn wrapped(emit: &mut dyn FnMut(Row) -> bool, text: &str, tone: Tone, width: usize) -> bool {
    wrap_each(text, width, &mut |line| emit((line, tone)))
}

fn render_diff(text: &str, width: usize, emit: &mut dyn FnMut(Row) -> bool) {
    let (mut old, mut new) = (0, 0);
    let mut in_hunk = false;
    let mut current_file = String::new();
    for line in text.lines() {
        if let Some(file) = line.strip_prefix("diff --git ") {
            let path = file.rsplit(" b/").next().unwrap_or(file);
            if !wrapped(emit, &format!("▤ {path}"), Tone::Accent, width) {
                return;
            }
            current_file = path.into();
            in_hunk = false;
        } else if let Some(file) = line.strip_prefix("+++ ") {
            let path = file.trim_start_matches("b/");
            if file != "/dev/null" && path != current_file {
                if !wrapped(emit, &format!("▤ {path}"), Tone::Accent, width) {
                    return;
                }
                current_file = path.into();
            }
            in_hunk = false;
        } else if line.starts_with("--- ") || line.starts_with("index ") {
            in_hunk = false;
        } else if line.starts_with("@@ ") {
            let mut parts = line.split_whitespace();
            parts.next();
            match (
                parts.next().and_then(range_start),
                parts.next().and_then(range_start),
            ) {
                (Some(a), Some(b)) => {
                    old = a;
                    new = b;
                    in_hunk = true;
                    if !wrapped(emit, line, Tone::Muted, width) {
                        return;
                    }
                }
                _ => {
                    in_hunk = false;
                    if !wrapped(emit, line, Tone::Muted, width) {
                        return;
                    }
                }
            }
        } else if in_hunk {
            let (gutter, code, tone) = match line.as_bytes().first() {
                Some(b'+') => {
                    let gutter = format!("{new:>4} + │ ");
                    new += 1;
                    (gutter, &line[1..], Tone::CodeAdded)
                }
                Some(b'-') => {
                    let gutter = format!("{old:>4} - │ ");
                    old += 1;
                    (gutter, &line[1..], Tone::CodeRemoved)
                }
                Some(b' ') => {
                    let gutter = format!("{new:>4}   │ ");
                    old += 1;
                    new += 1;
                    (gutter, &line[1..], Tone::Code)
                }
                _ => {
                    if !wrapped(emit, line, Tone::Muted, width) {
                        return;
                    }
                    continue;
                }
            };
            if !code_line(emit, &gutter, code, tone, width) {
                return;
            }
        } else {
            if !wrapped(emit, line, Tone::Muted, width) {
                return;
            }
        }
    }
}

pub fn diff_rows(text: &str, width: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    render_diff(text, width, &mut |row| {
        rows.push(row);
        true
    });
    rows
}

fn render_code(text: &str, width: usize, emit: &mut dyn FnMut(Row) -> bool) {
    for (index, line) in text.lines().enumerate() {
        if !code_line(
            emit,
            &format!("{:>4} │ ", index + 1),
            line,
            Tone::Code,
            width,
        ) {
            return;
        }
    }
}

pub fn code_rows(text: &str, width: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    render_code(text, width, &mut |row| {
        rows.push(row);
        true
    });
    rows
}

/// Tool entries render a single bounded page. Full output stays in the entry,
/// while the renderer stops after finding one row beyond the requested page.
pub fn output_page(
    text: &str,
    width: usize,
    page: usize,
    page_size: usize,
    diff: bool,
) -> (Vec<Row>, bool) {
    let start = page.saturating_mul(page_size);
    let mut skipped = 0usize;
    let mut rows = Vec::new();
    let mut has_next = false;
    let mut emit = |row| {
        if skipped < start {
            skipped += 1;
            return true;
        }
        if rows.len() >= page_size {
            has_next = true;
            return false;
        }
        rows.push(row);
        true
    };
    if diff {
        render_diff(text, width, &mut emit);
    } else {
        render_code(text, width, &mut emit);
    }
    (rows, has_next)
}

/// Render fenced snippets incrementally; an unfinished streamed fence stays code.
pub fn message_rows(text: &str, width: usize) -> Vec<Row> {
    let mut rows = Vec::new();
    let mut fence: Option<(char, usize, String)> = None;
    let mut block = String::new();
    for line in text.lines() {
        let trimmed = line.trim_start();
        let marker = trimmed.chars().next().unwrap_or(' ');
        let length = trimmed.chars().take_while(|c| *c == marker).count();
        if matches!(marker, '`' | '~') && length >= 3 {
            if let Some((active, count, language)) = &fence {
                if marker == *active && length >= *count && trimmed[length..].trim().is_empty() {
                    append_block(&mut rows, &block, language, width);
                    block.clear();
                    fence = None;
                    continue;
                }
            } else {
                fence = Some((marker, length, trimmed[length..].trim().into()));
                continue;
            }
        }
        if fence.is_some() {
            block.push_str(line);
            block.push('\n');
        } else {
            rows.extend(
                wrap(line, width)
                    .into_iter()
                    .map(|line| (line, Tone::Normal)),
            );
        }
    }
    if let Some((_, _, language)) = fence {
        append_block(&mut rows, &block, &language, width);
    }
    rows
}

fn append_block(rows: &mut Vec<Row>, block: &str, language: &str, width: usize) {
    let label = if language.is_empty() {
        "code"
    } else {
        language
    };
    rows.extend(
        wrap(&format!("┌ {label}"), width)
            .into_iter()
            .map(|line| (line, Tone::Muted)),
    );
    rows.extend(if language == "diff" || is_diff(block) {
        diff_rows(block, width)
    } else {
        code_rows(block, width)
    });
}

/// Small lexical highlighter for common source syntax; never interprets escapes.
fn tokens(code: &str) -> Vec<(&str, Tone)> {
    let mut output = Vec::new();
    let mut offset = 0;
    while offset < code.len() {
        let rest = &code[offset..];
        let ch = rest.chars().next().unwrap();
        if rest.starts_with("//") || rest.starts_with("/*") || ch == '#' {
            output.push((rest, Tone::Comment));
            break;
        }
        let (length, tone) = if matches!(ch, '\'' | '"' | '`') {
            let mut escaped = false;
            let mut length = ch.len_utf8();
            for next in rest[ch.len_utf8()..].chars() {
                length += next.len_utf8();
                if !escaped && next == ch {
                    break;
                }
                escaped = !escaped && next == '\\';
            }
            (length, Tone::String)
        } else if ch.is_alphanumeric() || ch == '_' {
            let length = rest
                .char_indices()
                .find(|(_, c)| !c.is_alphanumeric() && *c != '_')
                .map_or(rest.len(), |(i, _)| i);
            let word = &rest[..length];
            let tone = if ch.is_ascii_digit() {
                Tone::Number
            } else if matches!(
                word,
                "fn" | "pub"
                    | "use"
                    | "let"
                    | "mut"
                    | "impl"
                    | "struct"
                    | "enum"
                    | "match"
                    | "if"
                    | "else"
                    | "for"
                    | "while"
                    | "loop"
                    | "return"
                    | "async"
                    | "await"
                    | "const"
                    | "new"
                    | "function"
                    | "class"
                    | "import"
                    | "from"
                    | "export"
                    | "def"
                    | "in"
                    | "try"
                    | "catch"
                    | "throw"
                    | "true"
                    | "false"
                    | "null"
                    | "None"
                    | "self"
                    | "break"
                    | "continue"
            ) {
                Tone::Keyword
            } else if rest[length..].trim_start().starts_with('(') {
                Tone::Function
            } else {
                Tone::Normal
            };
            (length, tone)
        } else {
            (ch.len_utf8(), Tone::Normal)
        };
        output.push((&rest[..length], tone));
        offset += length;
    }
    output
}

pub fn paint(canvas: &mut Canvas, text: &str, tone: Tone) {
    if !matches!(tone, Tone::Code | Tone::CodeAdded | Tone::CodeRemoved) {
        canvas.text(0, 0, text, tone);
        return;
    }
    let background = match tone {
        Tone::CodeAdded => Background::Added,
        Tone::CodeRemoved => Background::Removed,
        _ => Background::Code,
    };
    for cell in &mut canvas.cells {
        cell.background = background;
    }
    let (gutter, code) = text.split_once("│ ").unwrap_or(("", text));
    let gutter_tone = match tone {
        Tone::CodeAdded => Tone::Added,
        Tone::CodeRemoved => Tone::Removed,
        _ => Tone::Muted,
    };
    let mut x = if gutter.is_empty() {
        0
    } else {
        canvas.text(0, 0, &format!("{gutter}│ "), gutter_tone)
    };
    for (token, tone) in tokens(code) {
        x = canvas.text(x, 0, token, tone);
    }
}

/// The full command remains available on expansion; the summary fits one row.
pub fn command_summary(command: &str, width: usize) -> String {
    let command = command
        .split_once(" -lc ")
        .or_else(|| command.split_once(" -c "))
        .map_or(command, |(_, script)| {
            script.trim().trim_matches(['\'', '"'])
        });
    if UnicodeWidthStr::width(command) <= width {
        return command.into();
    }
    let mut prefix = wrap(command, width.saturating_sub(1))
        .into_iter()
        .next()
        .unwrap_or_default();
    prefix.push('…');
    prefix
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn long_tool_output_is_paged_without_losing_line_numbers() {
        let output = (1..=5000)
            .map(|n| format!("line-{n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (first, more) = output_page(&output, 40, 0, 24, false);
        assert_eq!(first.len(), 24);
        assert!(first[0].0.contains("line-1"));
        assert!(more);
        let (second, more) = output_page(&output, 40, 1, 24, false);
        assert_eq!(second.len(), 24);
        assert!(second[0].0.contains("25 │ line-25"));
        assert!(more);
        let (last, more) = output_page(&output, 40, 208, 24, false);
        assert_eq!(last.len(), 8);
        assert!(!more);
    }
    #[test]
    fn diff_paging_preserves_hunk_line_numbers_and_wrapped_rows() {
        let diff = format!(
            "--- a/file.rs\n+++ b/file.rs\n@@ -1,60 +1,60 @@\n{}",
            (1..=60)
                .map(|n| format!("+updated_{n}_with_a_long_name"))
                .collect::<Vec<_>>()
                .join("\n")
        );
        let all = diff_rows(&diff, 24);
        let pages = (0..20)
            .map(|page| output_page(&diff, 24, page, 24, true))
            .take_while(|(rows, _)| !rows.is_empty())
            .collect::<Vec<_>>();
        assert_eq!(
            pages
                .iter()
                .flat_map(|(rows, _)| rows.iter().cloned())
                .collect::<Vec<_>>(),
            all
        );
        assert!(pages.last().is_some_and(|(_, more)| !more));
    }
    #[test]
    fn unified_diff_counts_old_and_new_lines_across_wraps_and_hunks() {
        let rows = diff_rows(
            "--- a/한글.rs\n+++ b/한글.rs\n@@ -1250,2 +1250,2 @@\n const names = new Set();\n-old 한글한글한글\n+new 한글한글한글\n@@ -1297 +1298 @@\n-old\n+new",
            24,
        );
        assert_eq!(rows[0], ("▤ 한글.rs".into(), Tone::Accent));
        assert!(
            rows.iter()
                .any(|(line, tone)| line.starts_with("1251 -") && *tone == Tone::CodeRemoved)
        );
        assert!(
            rows.iter()
                .any(|(line, tone)| line.starts_with("1251 +") && *tone == Tone::CodeAdded)
        );
        assert!(rows.iter().any(|(line, _)| line.starts_with("1298 +")));
        assert!(
            rows.iter()
                .all(|(line, _)| UnicodeWidthStr::width(line.as_str()) <= 24)
        );
        assert!(
            rows.iter()
                .filter(|(line, _)| line.starts_with("1251"))
                .count()
                == 2
        );
    }
    #[test]
    fn streamed_code_and_diff_fences_use_the_same_renderer() {
        let rows = message_rows("응답\n```rust\nlet 값 = 42; // 설명", 60);
        assert!(
            rows.iter()
                .any(|(line, tone)| line.contains("1 │ let 값") && *tone == Tone::Code)
        );
        let rows = message_rows("```diff\n@@ -1 +1 @@\n-old\n+new\n```\n끝", 60);
        assert!(rows.iter().any(|(_, tone)| *tone == Tone::CodeAdded));
        assert_eq!(rows.last().unwrap().0, "끝");
    }
    #[test]
    fn syntax_colors_keep_diff_background_on_every_cell() {
        let mut canvas = Canvas::new(50, 1);
        paint(
            &mut canvas,
            "        1 + │ const value = \"가\"; // 설명",
            Tone::CodeAdded,
        );
        assert!(
            canvas
                .cells
                .iter()
                .all(|cell| cell.background == Background::Added)
        );
        assert!(canvas.cells.iter().any(|cell| cell.tone == Tone::Keyword));
        assert!(canvas.cells.iter().any(|cell| cell.tone == Tone::String));
        assert!(canvas.cells.iter().any(|cell| cell.tone == Tone::Comment));
    }
    #[test]
    fn command_summary_does_not_hide_the_shell_script() {
        let command = "/bin/zsh -lc 'git status --short && git diff -- src/한글.rs'";
        assert_eq!(
            command_summary(command, 100),
            "git status --short && git diff -- src/한글.rs"
        );
        assert!(command_summary(command, 20).ends_with('…'));
    }
}
