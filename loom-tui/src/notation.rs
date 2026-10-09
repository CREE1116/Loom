//! Terminal projections for formulas and basic Mermaid flowcharts.
//! Unsupported syntax stays visible rather than being discarded.
use std::collections::HashMap;

// Distinguish generated inline math from user-authored inline code after parsing.
pub(super) const INLINE_MATH_MARKER: char = '\u{e000}';

pub(super) fn formula(source: &str) -> String {
    fn group(chars: &[char], at: &mut usize, depth: usize) -> String {
        while chars.get(*at).is_some_and(|c| c.is_whitespace()) {
            *at += 1;
        }
        if chars.get(*at) == Some(&'{') {
            *at += 1;
            parse(chars, at, depth + 1, true)
        } else if let Some(c) = chars.get(*at) {
            *at += 1;
            c.to_string()
        } else {
            String::new()
        }
    }
    fn parse(chars: &[char], at: &mut usize, depth: usize, grouped: bool) -> String {
        let mut out = String::new();
        if depth > 32 {
            return chars[*at..].iter().collect();
        }
        while let Some(&c) = chars.get(*at) {
            *at += 1;
            match c {
                '}' if grouped => break,
                '{' => out.push_str(&parse(chars, at, depth + 1, true)),
                '^' | '_' => {
                    let value = group(chars, at, depth);
                    let alphabet = if c == '^' {
                        "⁰¹²³⁴⁵⁶⁷⁸⁹⁺⁻⁼⁽⁾ⁿⁱ"
                    } else {
                        "₀₁₂₃₄₅₆₇₈₉₊₋₌₍₎"
                    };
                    let keys = if c == '^' {
                        "0123456789+-=()ni"
                    } else {
                        "0123456789+-=()"
                    };
                    let converted: Option<String> = value
                        .chars()
                        .map(|c| {
                            keys.chars()
                                .position(|k| k == c)
                                .and_then(|i| alphabet.chars().nth(i))
                        })
                        .collect();
                    out.push_str(&converted.unwrap_or_else(|| format!("{c}({value})")));
                }
                '\\' => {
                    // Some API responses arrive with an extra escape layer.
                    // Accept it for TeX commands, while retaining TeX's \\\\ line breaks.
                    if chars.get(*at) == Some(&'\\')
                        && chars
                            .get(*at + 1)
                            .is_some_and(|c| c.is_ascii_alphabetic() || *c == ',')
                    {
                        *at += 1;
                    }
                    let start = *at;
                    while chars.get(*at).is_some_and(|c| c.is_ascii_alphabetic()) {
                        *at += 1;
                    }
                    let command: String = chars[start..*at].iter().collect();
                    match command.as_str() {
                        "frac" | "dfrac" | "tfrac" => {
                            let numerator = group(chars, at, depth);
                            let denominator = group(chars, at, depth);
                            out.push_str(&format!("({numerator})/({denominator})"));
                        }
                        "text" | "mathrm" | "operatorname" => {
                            out.push_str(&group(chars, at, depth))
                        }
                        "sqrt" => out.push_str(&format!("√({})", group(chars, at, depth))),
                        "left" | "right" => {}
                        "times" => out.push('×'),
                        "cdot" => out.push('·'),
                        "le" | "leq" => out.push('≤'),
                        "ge" | "geq" => out.push('≥'),
                        "neq" | "ne" => out.push('≠'),
                        "approx" => out.push('≈'),
                        "to" | "rightarrow" => out.push('→'),
                        "infty" => out.push('∞'),
                        "alpha" => out.push('α'),
                        "beta" => out.push('β'),
                        "gamma" => out.push('γ'),
                        "delta" => out.push('δ'),
                        "theta" => out.push('θ'),
                        "lambda" => out.push('λ'),
                        "pi" => out.push('π'),
                        "sigma" => out.push('σ'),
                        "sum" => out.push('Σ'),
                        "int" => out.push('∫'),
                        "iint" => out.push('∬'),
                        "oint" => out.push('∮'),
                        "quad" => out.push(' '),
                        "qquad" => out.push_str("  "),
                        "pm" => out.push('±'),
                        "div" => out.push('÷'),
                        "" => {
                            if let Some(c) = chars.get(*at) {
                                out.push(if *c == ',' || *c == ';' || *c == ':' {
                                    ' '
                                } else {
                                    *c
                                });
                                *at += 1;
                            }
                        }
                        _ => {
                            out.push('\\');
                            out.push_str(&command);
                            if chars.get(*at) == Some(&'{') {
                                out.push('{');
                                out.push_str(&group(chars, at, depth));
                                out.push('}');
                            }
                        }
                    }
                }
                _ => out.push(c),
            }
        }
        out
    }
    parse(&source.chars().collect::<Vec<_>>(), &mut 0, 0, false)
}

/// Normalize math delimiters before Markdown consumes backslash escapes.
/// Code fences and inline code are passed through unchanged.
pub(super) fn math_markdown(source: &str) -> String {
    let mut out = String::new();
    let mut at = 0;
    while at < source.len() {
        let tail = &source[at..];
        if tail.starts_with('`') || tail.starts_with('~') {
            let marker = tail.chars().next().unwrap();
            let count = tail.chars().take_while(|c| *c == marker).count();
            if marker == '`' || count >= 3 {
                let delimiter = marker.to_string().repeat(count);
                let end = tail[count..]
                    .find(&delimiter)
                    .map(|i| count + i + count)
                    .unwrap_or(tail.len());
                out.push_str(&tail[..end]);
                at += end;
                continue;
            }
        }
        let delimiter = [
            ("$$", "$$", true),
            ("\\[", "\\]", true),
            ("\\(", "\\)", false),
            ("$", "$", false),
        ]
        .into_iter()
        .find(|(open, _, _)| tail.starts_with(open));
        if let Some((open, close, block)) = delimiter
            && let Some(end) = tail[open.len()..].find(close)
        {
            let body = &tail[open.len()..open.len() + end];
            // A currency amount is not an inline equation.
            if block
                || (!body.trim().is_empty()
                    && !body.contains('\n')
                    && !body.starts_with(char::is_whitespace)
                    && !body.ends_with(char::is_whitespace))
            {
                let rendered = formula(body.trim());
                if !rendered.contains('`') {
                    if block {
                        out.push_str("\n\n```math\n");
                        // Keep the original TeX until the block renderer sees it.
                        out.push_str(body.trim());
                        out.push_str("\n```\n\n");
                    } else {
                        out.push('`');
                        out.push(INLINE_MATH_MARKER);
                        out.push_str(&rendered);
                        out.push('`');
                    }
                    at += open.len() + end + close.len();
                    continue;
                }
            }
        }
        let c = tail.chars().next().unwrap();
        out.push(c);
        at += c.len_utf8();
        if c == '\\' && at < source.len() {
            let escaped = source[at..].chars().next().unwrap();
            out.push(escaped);
            at += escaped.len_utf8();
        }
    }
    out
}

pub(super) fn flowchart(source: &str) -> Option<Vec<String>> {
    let mut statements = source
        .lines()
        .flat_map(|line| line.split(';'))
        .map(str::trim)
        .filter(|s| !s.is_empty() && !s.starts_with("%%"));
    let header = statements.next()?;
    if !(header.starts_with("graph ") || header.starts_with("flowchart ")) {
        return None;
    }
    let mut labels = HashMap::new();
    let mut edges = Vec::new();
    fn node(value: &str, labels: &mut HashMap<String, String>) -> Option<String> {
        let value = value.trim();
        let end = value
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
            .unwrap_or(value.len());
        let id = &value[..end];
        if id.is_empty() {
            return None;
        }
        let suffix = value[end..].trim();
        if !suffix.is_empty() {
            let valid = [('[', ']'), ('(', ')'), ('{', '}')]
                .iter()
                .any(|(a, b)| suffix.starts_with(*a) && suffix.ends_with(*b));
            if !valid {
                return None;
            }
            let label = suffix[1..suffix.len() - 1]
                .trim_matches('"')
                .replace("<br/>", " / ")
                .replace("<br>", " / ");
            labels.insert(id.into(), label);
        }
        Some(id.to_owned())
    }
    for statement in statements {
        if statement.contains("-->") {
            let mut parts = statement.split("-->");
            let mut from = node(parts.next()?, &mut labels)?;
            for part in parts {
                let part = part.trim();
                let (label, rest) = if let Some(tail) = part.strip_prefix('|') {
                    let (label, rest) = tail.split_once('|')?;
                    (Some(label.to_owned()), rest)
                } else {
                    (None, part)
                };
                let to = node(rest, &mut labels)?;
                edges.push((from, to.clone(), label));
                from = to;
            }
        } else {
            node(statement, &mut labels)?;
        }
    }
    if edges.is_empty() {
        return None;
    }
    Some(
        edges
            .into_iter()
            .map(|(from, to, label)| {
                format!(
                    "{} {} {}",
                    labels.get(&from).unwrap_or(&from),
                    label.map(|s| format!("─[{s}]→")).unwrap_or("→".into()),
                    labels.get(&to).unwrap_or(&to)
                )
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn formulas_keep_fractions_scripts_and_unknown_commands() {
        assert_eq!(
            formula(r"x^2 + \frac{a_1}{b} \times 100\%"),
            "x² + (a₁)/(b) × 100%"
        );
        assert_eq!(formula(r"\unknown{x}"), r"\unknown{x}");
        assert_eq!(
            math_markdown(r"`$x^2$` and $x^2$"),
            format!("`$x^2$` and `{}x²`", INLINE_MATH_MARKER)
        );
        assert!(math_markdown("```rust\nlet x = \"$x^2$\";\n```").contains("$x^2$"));
    }

    #[test]
    fn integration_and_thin_spaces_accept_tex_and_double_escaped_tex() {
        assert_eq!(formula(r"\int_0^1 x\,dx"), "∫₀¹ x dx");
        assert_eq!(formula(r"\\int_0^1 x\\,dx"), "∫₀¹ x dx");
    }
    #[test]
    fn branching_flowcharts_keep_labels_and_unsupported_syntax_falls_back() {
        assert_eq!(
            flowchart("flowchart TD\nA[Core] --> B[T0]\nA --> C[T1]\nB -->|失敗| C").unwrap(),
            ["Core → T0", "Core → T1", "T0 ─[失敗]→ T1"]
        );
        assert!(flowchart("sequenceDiagram\nA->>B: hello").is_none());
    }
}
