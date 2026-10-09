//! Structural code search behind a Loom-owned API. The ast-grep engine is replaceable;
//! results use Loom's revision, paths, and symbol ids.
use super::{graph::CodeGraph, lang::Language, repository::RepositorySnapshot};
use ast_grep_core::Pattern;
use ast_grep_language::{LanguageExt, SupportLang};

fn grammar(lang: Language) -> SupportLang {
    match lang {
        Language::Rust => SupportLang::Rust,
        Language::Python => SupportLang::Python,
        Language::JavaScript => SupportLang::JavaScript,
        Language::TypeScript => SupportLang::TypeScript,
        Language::Tsx => SupportLang::Tsx,
        Language::Go => SupportLang::Go,
        Language::Java => SupportLang::Java,
    }
}

/// Exact AST pattern matches; refs/symbol ownership, when shown, are approximate.
/// A mixed-language query tries each grammar separately. `language` restricts it.
pub fn search(
    snapshot: &RepositorySnapshot,
    graph: &CodeGraph,
    pattern: &str,
    scope: &str,
    language: Option<Language>,
    max_results: usize,
) -> Result<String, String> {
    if pattern.trim().is_empty() {
        return Err("AST pattern must not be empty".into());
    }
    let mut output = String::new();
    let mut matched = 0;
    let mut tried = 0;
    let mut invalid = Vec::new();
    let mut cached = std::collections::BTreeMap::new();
    let mut inspected_files = 0;
    for source in snapshot.sources() {
        let (path, _, source) = source.map_err(|e| e.to_string())?;
        let file = path.to_string_lossy().replace('\\', "/");
        if !scope.is_empty() && file != scope && !file.starts_with(&format!("{scope}/")) {
            continue;
        }
        let Some(lang) = Language::of(&file) else {
            continue;
        };
        if language.is_some_and(|wanted| lang != wanted) {
            continue;
        }
        let ast_language = grammar(lang);
        let query = cached
            .entry(lang)
            .or_insert_with(|| Pattern::try_new(pattern, ast_language).map_err(|e| e.to_string()));
        let query = match query {
            Ok(query) => query,
            Err(error) => {
                if !invalid.iter().any(|(l, _)| *l == lang) {
                    invalid.push((lang, error.clone()));
                }
                continue;
            }
        };
        tried += 1;
        inspected_files += 1;
        let root = ast_language.ast_grep(source.as_ref());
        for item in root.root().find_all(&*query) {
            let line = item.start_pos().line() + 1;
            let owner = graph
                .symbol_at(&file, line)
                .map(|s| format!(" [{}]", s.id))
                .unwrap_or_default();
            let preview = item
                .text()
                .lines()
                .next()
                .unwrap_or_default()
                .trim()
                .chars()
                .take(120)
                .collect::<String>();
            output.push_str(&format!("{file}:{line}{owner}: {preview}\n"));
            matched += 1;
            if matched >= max_results {
                output.push_str("… result limit reached; narrow path or pattern\n");
                return Ok(output);
            }
        }
    }
    if tried == 0 && !invalid.is_empty() {
        return Err(format!(
            "Invalid AST pattern for {}: {}",
            invalid[0].0.name(),
            invalid[0].1
        ));
    }
    if matched == 0 {
        output.push_str("No structural matches found.\n");
    }
    if !invalid.is_empty() {
        output.push_str("Pattern could not be parsed in: ");
        output.push_str(
            &invalid
                .iter()
                .map(|(l, _)| l.name())
                .collect::<Vec<_>>()
                .join(", "),
        );
        output.push('\n');
    }
    output.push_str(&format!("{matched} matches in {inspected_files} searched files (AST syntax exact; symbol ownership approximate)"));
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{graph::GraphIndex, repository::RepositoryExplorer};

    #[test]
    fn structural_search_is_language_aware_and_returns_loom_ids() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.rs"),
            "fn main() { save(1); save(2); }\nfn save(x: u8) {}\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("b.py"), "def go():\n    save(1)\n").unwrap();
        let repo = RepositoryExplorer::new(dir.path()).unwrap();
        let snapshot = repo.snapshot().unwrap();
        let graph = GraphIndex::default().graph(&snapshot).unwrap();
        let hits = search(&snapshot, &graph, "save($X)", "", Some(Language::Rust), 20).unwrap();
        assert!(hits.contains("a.rs:1 [a.rs::main]"), "{hits}");
        assert!(hits.contains("2 matches"), "{hits}");
        assert!(!hits.contains("b.py:"), "{hits}");
        assert!(
            search(
                &snapshot,
                &graph,
                "save($X)",
                "",
                Some(Language::Python),
                20
            )
            .unwrap()
            .contains("b.py:2 [b.py::go]")
        );
    }
}
