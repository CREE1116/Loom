//! A budgeted map of the repository read from the code graph: directories and files with
//! their language, the symbols other code uses most, and which files each file depends
//! on. It replaces a model's crawl of directory listings, outlines and searches.
//!
//! Ranking is Loom's own and deliberately simple: a symbol scores by how many symbols in
//! other files reference its name (name-resolved, over-approximate like the graph).
use super::{graph::CodeGraph, lang::Language};
use std::collections::{BTreeMap, BTreeSet, HashMap};

const SYMBOLS_PER_FILE: usize = 4;
/// A name defined in more places than this is too ambiguous to draw a file dependency.
const AMBIGUOUS: usize = 3;

struct FileEntry {
    language: Option<Language>,
    symbols: Vec<(usize, String)>,
    total: usize,
    uses: BTreeSet<String>,
    test: bool,
}

/// Map of the files under `scope` ("" for the whole repository), at most `budget` bytes.
/// `paths` lists every file, including ones the graph does not parse.
pub fn render(graph: &CodeGraph, paths: &[String], scope: &str, budget: usize) -> String {
    let scope = scope.trim().trim_start_matches("./").trim_end_matches('/');
    let in_scope =
        |file: &str| scope.is_empty() || file == scope || file.starts_with(&format!("{scope}/"));
    // How often each name is used from another file.
    let mut used: HashMap<&str, usize> = HashMap::new();
    for symbol in graph.symbols() {
        for name in &symbol.refs {
            if graph.named(name).any(|d| d.file != symbol.file) {
                *used.entry(name.as_str()).or_default() += 1;
            }
        }
    }
    let mut files: BTreeMap<&str, FileEntry> = BTreeMap::new();
    for path in paths.iter().filter(|p| in_scope(p)) {
        files.insert(
            path,
            FileEntry {
                language: Language::of(path),
                symbols: Vec::new(),
                total: 0,
                uses: BTreeSet::new(),
                test: false,
            },
        );
    }
    for symbol in graph
        .symbols()
        .iter()
        .filter(|s| !s.is_pseudo() && in_scope(&s.file))
    {
        let Some(entry) = files.get_mut(symbol.file.as_str()) else {
            continue;
        };
        entry.total += 1;
        entry.test |= symbol.test;
        let score = used.get(symbol.name.as_str()).copied().unwrap_or(0);
        let label = format!("{} {}", kind_label(symbol.kind), symbol.name);
        if !entry.symbols.iter().any(|(_, l)| *l == label) {
            entry.symbols.push((score, label));
        }
        for name in &symbol.refs {
            let defined: Vec<_> = graph
                .named(name)
                .map(|d| d.file.as_str())
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect();
            if defined.len() <= AMBIGUOUS {
                for file in defined.into_iter().filter(|f| *f != symbol.file) {
                    entry.uses.insert(file.to_owned());
                }
            }
        }
    }
    for entry in files.values_mut() {
        entry
            .symbols
            .sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    }
    let code = files.values().filter(|f| f.language.is_some()).count();
    let mut languages: BTreeMap<&str, usize> = BTreeMap::new();
    for entry in files.values() {
        if let Some(language) = entry.language {
            *languages.entry(language.name()).or_default() += 1;
        }
    }
    let header = format!(
        "map of {} · {} files ({}) · {} symbols · symbols ranked by use from other files (×n)\n",
        if scope.is_empty() {
            "the repository"
        } else {
            scope
        },
        files.len(),
        if languages.is_empty() {
            "no supported source".to_owned()
        } else {
            languages
                .iter()
                .map(|(l, n)| format!("{l} {n}"))
                .collect::<Vec<_>>()
                .join(", ")
        },
        files.values().map(|f| f.total).sum::<usize>(),
    );
    // Try full detail first; when over budget, collapse directories from the deepest up.
    let max_depth = files
        .keys()
        .map(|f| f.matches('/').count())
        .max()
        .unwrap_or(0);
    for collapse_below in (0..=max_depth + 1).rev() {
        let text = format!("{header}{}", body(&files, collapse_below, code > 0));
        if text.len() <= budget || collapse_below == 0 {
            return truncate(text, budget);
        }
    }
    unreachable!()
}

fn body(files: &BTreeMap<&str, FileEntry>, collapse_below: usize, any_code: bool) -> String {
    let mut out = String::new();
    let mut collapsed: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    let mut other: BTreeMap<String, Vec<&str>> = BTreeMap::new();
    let mut current_dir = None;
    for (path, entry) in files {
        let (dir, name) = path.rsplit_once('/').unwrap_or(("", path));
        if dir.matches('/').count() + usize::from(!dir.is_empty()) > collapse_below {
            let top: String = dir
                .split('/')
                .take(collapse_below.max(1))
                .collect::<Vec<_>>()
                .join("/");
            let slot = collapsed.entry(top).or_default();
            slot.0 += 1;
            slot.1 += entry.total;
            continue;
        }
        if entry.language.is_none() {
            other.entry(dir.to_owned()).or_default().push(name);
            continue;
        }
        if current_dir != Some(dir) {
            out.push_str(&format!("{}/\n", if dir.is_empty() { "." } else { dir }));
            current_dir = Some(dir);
        }
        let uses: Vec<_> = entry.uses.iter().map(|u| relative(dir, u)).collect();
        out.push_str(&format!(
            "  {name} ({}{}){}\n",
            entry.language.map_or("", |l| l.name()),
            if entry.test { ", tests" } else { "" },
            if uses.is_empty() {
                String::new()
            } else {
                format!(" · uses {}", uses.join(", "))
            },
        ));
        if !entry.symbols.is_empty() {
            let shown: Vec<_> = entry
                .symbols
                .iter()
                .take(SYMBOLS_PER_FILE)
                .map(|(score, label)| {
                    if *score > 0 {
                        format!("{label} ×{score}")
                    } else {
                        label.clone()
                    }
                })
                .collect();
            let more = entry.symbols.len().saturating_sub(SYMBOLS_PER_FILE);
            out.push_str(&format!(
                "    {}{}\n",
                shown.join(", "),
                if more > 0 {
                    format!(", +{more}")
                } else {
                    String::new()
                }
            ));
        }
    }
    for (dir, (count, symbols)) in collapsed {
        out.push_str(&format!(
            "{dir}/… {count} files, {symbols} symbols (map {dir} for detail)\n"
        ));
    }
    if !other.is_empty() {
        let list: Vec<_> = other
            .iter()
            .map(|(dir, names)| {
                if dir.is_empty() {
                    names.join(", ")
                } else {
                    format!("{dir}/{{{}}}", names.join(", "))
                }
            })
            .collect();
        out.push_str(&format!("other files: {}\n", list.join("; ")));
    }
    if !any_code {
        out.push_str("(no source in a supported language here; use find and read)\n");
    }
    out
}

fn kind_label(kind: super::graph::SymbolKind) -> &'static str {
    use super::graph::SymbolKind::*;
    match kind {
        Fn => "fn",
        Method => "method",
        Struct => "type",
        Enum => "enum",
        Union => "union",
        Trait => "trait",
        Impl => "impl",
        Const | Static => "const",
        Type => "type",
        Macro => "macro",
        Mod => "mod",
        Imports => "use",
        Field => "field",
        Variant => "variant",
    }
}

fn relative(dir: &str, path: &str) -> String {
    match path.strip_prefix(&format!("{dir}/")) {
        Some(rest) if !dir.is_empty() => rest.to_owned(),
        _ => path.to_owned(),
    }
}

fn truncate(mut text: String, budget: usize) -> String {
    if text.len() > budget {
        let mut cut = budget.saturating_sub(40);
        while !text.is_char_boundary(cut) {
            cut -= 1;
        }
        text.truncate(cut);
        text.push_str("\n… (map truncated; pass a path)\n");
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::World;

    fn repo(files: &[(&str, &str)]) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success()
        );
        for (path, text) in files {
            let path = dir.path().join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        std::process::Command::new("git").args(["init", "-q"]).current_dir(dir.path()).status().unwrap();
        dir
    }

    fn map_of(dir: &tempfile::TempDir, scope: &str, budget: usize) -> String {
        let world = World::open(dir.path()).unwrap();
        let paths: Vec<String> = world
            .snapshot
            .files
            .paths()
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .collect();
        render(&world.graph, &paths, scope, budget)
    }

    #[test]
    fn map_ranks_used_symbols_and_shows_file_dependencies_across_languages() {
        let dir = repo(&[
            (
                "src/lib.rs",
                "pub fn core_value() -> u32 { 1 }\npub fn unused() {}\n",
            ),
            (
                "src/report.rs",
                "fn report() -> u32 { crate::core_value() + core_value() }\n",
            ),
            (
                "tools/gen.py",
                "def generate():\n    return helper()\n\ndef helper():\n    return 1\n",
            ),
            (
                "web/app.ts",
                "export function render() { return view() }\nfunction view() { return 1 }\n",
            ),
            ("README.md", "# demo\n"),
        ]);
        let map = map_of(&dir, "", 6000);
        assert!(
            map.contains("rust 2") && map.contains("python 1") && map.contains("typescript 1"),
            "{map}"
        );
        assert!(map.contains("report.rs (rust) · uses lib.rs"), "{map}");
        let lib = map
            .lines()
            .skip_while(|l| !l.contains("lib.rs (rust)"))
            .nth(1)
            .unwrap();
        assert!(
            lib.trim_start().starts_with("fn core_value ×1"),
            "most used first: {lib}"
        );
        assert!(
            map.contains("gen.py (python)") && map.contains("fn generate"),
            "{map}"
        );
        assert!(map.contains("other files: README.md"), "{map}");
    }

    #[test]
    fn map_respects_scope_and_budget() {
        let mut files = vec![];
        for i in 0..60 {
            files.push((
                format!("deep/a/b/m{i}.py"),
                format!("def f{i}():\n    return {i}\n"),
            ));
        }
        files.push((
            "src/main.go".into(),
            "package main\nfunc main() {}\n".into(),
        ));
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        let dir = repo(&refs);
        let small = map_of(&dir, "", 600);
        assert!(small.len() <= 600, "{}", small.len());
        assert!(
            small.contains("deep/") && small.contains("60 files, 60 symbols"),
            "{small}"
        );
        assert!(small.contains("for detail"), "{small}");
        let scoped = map_of(&dir, "src", 6000);
        assert!(
            scoped.contains("main.go") && !scoped.contains("m1.py"),
            "{scoped}"
        );
        let detailed = map_of(&dir, "deep/a", 6000);
        assert!(
            detailed.contains("m0.py") && !detailed.contains("main.go"),
            "{detailed}"
        );
    }
}
