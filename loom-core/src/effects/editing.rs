/// Byte ranges of `needle` in `text`: exact first, then line-trimmed, then
/// whitespace-collapsed matching (as opencode, Aider and Cline do).
pub fn find_matches(text: &str, needle: &str) -> Option<(Vec<(usize, usize)>, &'static str)> {
    let exact: Vec<(usize, usize)> = text
        .match_indices(needle)
        .map(|(i, m)| (i, i + m.len()))
        .collect();
    if !exact.is_empty() {
        return Some((exact, "exactly"));
    }
    // Line-based fuzzy: compare trimmed lines, then whitespace-collapsed lines.
    let starts: Vec<usize> = std::iter::once(0)
        .chain(text.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let lines: Vec<&str> = text.split('\n').collect();
    let want: Vec<&str> = needle.trim_matches('\n').split('\n').collect();
    let collapse = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
    for (how, norm) in [
        (
            "ignoring indentation",
            &(|s: &str| s.trim().to_owned()) as &dyn Fn(&str) -> String,
        ),
        ("ignoring whitespace", &collapse as &dyn Fn(&str) -> String),
    ] {
        let want: Vec<String> = want.iter().map(|l| norm(l)).collect();
        let mut found = Vec::new();
        for i in 0..lines.len().saturating_sub(want.len() - 1) {
            if (0..want.len()).all(|k| norm(lines[i + k]) == want[k]) {
                let end_line = i + want.len() - 1;
                let end = starts[end_line] + lines[end_line].len();
                found.push((starts[i], end));
            }
        }
        if !found.is_empty() {
            return Some((found, how));
        }
    }
    None
}

/// For Rust files: an error the edit introduces (a file that was already broken may stay broken).
pub(crate) fn new_syntax_error(rel: &str, before: &str, after: &str) -> Option<String> {
    if !rel.ends_with(".rs") || syn::parse_file(before).is_err() {
        return None;
    }
    syn::parse_file(after)
        .err()
        .map(|e| format!("line {}: {e}", e.span().start().line))
}

/// Replace identifier tokens only; strings, comments and whitespace keep their bytes.
pub(crate) fn rename_identifiers(source: &str, old: &str, new: &str) -> anyhow::Result<String> {
    use proc_macro2::{TokenStream, TokenTree};
    fn visit(tokens: TokenStream, old: &str, starts: &[usize], ranges: &mut Vec<(usize, usize)>) {
        for token in tokens {
            match token {
                TokenTree::Group(group) => visit(group.stream(), old, starts, ranges),
                TokenTree::Ident(ident) if ident == old => {
                    let start = ident.span().start();
                    let end = ident.span().end();
                    ranges.push((
                        starts[start.line - 1] + start.column,
                        starts[end.line - 1] + end.column,
                    ));
                }
                _ => {}
            }
        }
    }
    let tokens: TokenStream = source
        .parse()
        .map_err(|e: proc_macro2::LexError| anyhow::anyhow!("{e}"))?;
    let starts: Vec<_> = std::iter::once(0)
        .chain(source.match_indices('\n').map(|(i, _)| i + 1))
        .collect();
    let mut ranges = Vec::new();
    visit(tokens, old, &starts, &mut ranges);
    let mut updated = source.to_owned();
    ranges.sort_unstable();
    for (start, end) in ranges.into_iter().rev() {
        anyhow::ensure!(
            source.get(start..end) == Some(old),
            "identifier span does not match source bytes"
        );
        updated.replace_range(start..end, new);
    }
    Ok(updated)
}
