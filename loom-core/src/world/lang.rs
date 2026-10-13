//! Symbols for every supported language behind one Loom interface.
//!
//! Rust uses Loom's own extractor (`graph::extract`, item paths from syn). Other languages
//! use tree-sitter tags queries for now; this module is the only place that knows, so an
//! extractor can be replaced by Loom's own without touching the graph or its users.
//! Symbols carry the same id scheme (`<file>::<container>::<name>`) and name-resolved
//! references as Rust symbols.
use super::graph::{Symbol, SymbolKind};
use std::{collections::BTreeSet, sync::OnceLock};
use tree_sitter_tags::{TagsConfiguration, TagsContext};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Language {
    Rust,
    Python,
    JavaScript,
    TypeScript,
    Tsx,
    Go,
    Java,
}

impl Language {
    pub fn of(path: &str) -> Option<Self> {
        let extension = path.rsplit_once('.')?.1;
        Some(match extension {
            "rs" => Self::Rust,
            "py" | "pyi" => Self::Python,
            "js" | "mjs" | "cjs" | "jsx" => Self::JavaScript,
            "ts" | "mts" | "cts" => Self::TypeScript,
            "tsx" => Self::Tsx,
            "go" => Self::Go,
            "java" => Self::Java,
            _ => return None,
        })
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Rust => "rust",
            Self::Python => "python",
            Self::JavaScript => "javascript",
            Self::TypeScript | Self::Tsx => "typescript",
            Self::Go => "go",
            Self::Java => "java",
        }
    }
}

/// Symbols of one file, or `None` when the language is not supported.
pub fn extract(file: &str, source: &str) -> Option<Result<Vec<Symbol>, String>> {
    let language = Language::of(file)?;
    Some(match language {
        Language::Rust => super::graph::extract(file, source),
        other => tags(other, file, source),
    })
}

fn configuration(language: Language) -> Option<&'static TagsConfiguration> {
    static CONFIGS: OnceLock<Vec<(Language, Option<TagsConfiguration>)>> = OnceLock::new();
    let configs = CONFIGS.get_or_init(|| {
        let javascript = tree_sitter_javascript::TAGS_QUERY;
        // The TypeScript tags query only covers TypeScript-specific nodes and is meant to
        // be combined with the JavaScript one.
        let typescript = format!("{javascript}\n{}", tree_sitter_typescript::TAGS_QUERY);
        let build = |language: tree_sitter::Language, query: &str| {
            TagsConfiguration::new(language, query, "").ok()
        };
        vec![
            (
                Language::Python,
                build(
                    tree_sitter_python::LANGUAGE.into(),
                    tree_sitter_python::TAGS_QUERY,
                ),
            ),
            (
                Language::JavaScript,
                build(tree_sitter_javascript::LANGUAGE.into(), javascript),
            ),
            (
                Language::TypeScript,
                build(
                    tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
                    &typescript,
                ),
            ),
            (
                Language::Tsx,
                build(tree_sitter_typescript::LANGUAGE_TSX.into(), &typescript),
            ),
            (
                Language::Go,
                build(tree_sitter_go::LANGUAGE.into(), tree_sitter_go::TAGS_QUERY),
            ),
            (
                Language::Java,
                build(
                    tree_sitter_java::LANGUAGE.into(),
                    tree_sitter_java::TAGS_QUERY,
                ),
            ),
        ]
    });
    configs.iter().find(|(l, _)| *l == language)?.1.as_ref()
}

fn kind(syntax: &str) -> SymbolKind {
    match syntax {
        "function" => SymbolKind::Fn,
        "method" => SymbolKind::Method,
        "class" | "struct" => SymbolKind::Struct,
        "interface" => SymbolKind::Trait,
        "module" | "namespace" => SymbolKind::Mod,
        "constant" => SymbolKind::Const,
        "macro" => SymbolKind::Macro,
        "enum" => SymbolKind::Enum,
        "implementation" => SymbolKind::Impl,
        _ => SymbolKind::Type,
    }
}

fn is_test_path(file: &str) -> bool {
    let name = file.rsplit('/').next().unwrap_or(file);
    file.starts_with("tests/")
        || file.starts_with("test/")
        || file.contains("/tests/")
        || file.contains("/test/")
        || file.contains("__tests__/")
        || name.starts_with("test_")
        || name.ends_with("_test.py")
        || name.ends_with("_test.go")
        || name.contains(".test.")
        || name.contains(".spec.")
        || name.ends_with("Test.java")
}

struct Definition {
    name: String,
    kind: SymbolKind,
    bytes: std::ops::Range<usize>,
    lines: (usize, usize),
    refs: BTreeSet<String>,
}

/// 1-based line of a byte offset. Tag offsets can fall inside a multibyte char
/// (e.g. Korean comments), so count newline bytes instead of slicing the str.
fn line_at(source: &str, byte: usize) -> usize {
    source.as_bytes()[..byte.min(source.len())]
        .iter()
        .filter(|&&b| b == b'\n')
        .count()
        + 1
}

fn tags(language: Language, file: &str, source: &str) -> Result<Vec<Symbol>, String> {
    let config = configuration(language)
        .ok_or_else(|| format!("no {} symbol extractor", language.name()))?;
    let mut context = TagsContext::new();
    let (tags, _) = context
        .generate_tags(config, source.as_bytes(), None)
        .map_err(|e| format!("{e:?}"))?;
    let line_of = |byte: usize| line_at(source, byte);
    let mut definitions: Vec<Definition> = Vec::new();
    let mut references: Vec<(String, usize)> = Vec::new();
    for tag in tags {
        let tag = tag.map_err(|e| format!("{e:?}"))?;
        let name = source
            .get(tag.name_range.clone())
            .unwrap_or_default()
            .to_owned();
        if name.is_empty() {
            continue;
        }
        if tag.is_definition {
            definitions.push(Definition {
                name,
                kind: kind(config.syntax_type_name(tag.syntax_type_id)),
                lines: (
                    line_of(tag.range.start),
                    line_of(tag.range.end.saturating_sub(1)),
                ),
                bytes: tag.range,
                refs: BTreeSet::new(),
            });
        } else {
            references.push((name, tag.name_range.start));
        }
    }
    // Outer definitions first, so the innermost container of a position is the last match.
    definitions.sort_by_key(|d| (d.bytes.start, std::cmp::Reverse(d.bytes.end)));
    let innermost = |definitions: &[Definition], at: usize, skip: Option<usize>| {
        definitions
            .iter()
            .enumerate()
            .filter(|(i, d)| Some(*i) != skip && d.bytes.start <= at && at < d.bytes.end)
            .map(|(i, _)| i)
            .next_back()
    };
    for (name, at) in references {
        if let Some(i) = innermost(&definitions, at, None)
            && definitions[i].name != name
        {
            definitions[i].refs.insert(name);
        }
    }
    let test = is_test_path(file);
    let mut out = Vec::with_capacity(definitions.len());
    for (index, definition) in definitions.iter().enumerate() {
        let mut path = vec![definition.name.clone()];
        let mut kind = definition.kind;
        let mut at = index;
        while let Some(parent) = definitions[..at].iter().rposition(|d| {
            d.bytes.start <= definitions[at].bytes.start && definitions[at].bytes.end <= d.bytes.end
        }) {
            // Grammars such as Python's tag methods as functions; a function whose
            // nearest container is a type is a method.
            if at == index
                && kind == SymbolKind::Fn
                && matches!(
                    definitions[parent].kind,
                    SymbolKind::Struct | SymbolKind::Trait | SymbolKind::Impl
                )
            {
                kind = SymbolKind::Method;
            }
            path.push(definitions[parent].name.clone());
            at = parent;
        }
        path.reverse();
        out.push(Symbol {
            id: format!("{file}::{}", path.join("::")),
            name: definition.name.clone(),
            kind,
            file: file.to_owned(),
            start: definition.lines.0,
            end: definition.lines.1,
            refs: definition.refs.clone(),
            test,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids(file: &str, source: &str) -> Vec<String> {
        extract(file, source)
            .unwrap()
            .unwrap()
            .into_iter()
            .map(|s| s.id)
            .collect()
    }

    #[test]
    fn line_numbers_accept_offsets_inside_multibyte_chars() {
        let source = "# 한글 주석\ndef f():\n    return 1\n";
        // Byte 3 is inside '한' (bytes 2..5); slicing the str there used to panic.
        assert_eq!(line_at(source, 3), 1);
        assert_eq!(line_at(source, source.find("def").unwrap()), 2);
        assert_eq!(line_at(source, usize::MAX), 4);
    }

    #[test]
    fn languages_are_chosen_by_extension() {
        assert_eq!(Language::of("a/b.py"), Some(Language::Python));
        assert_eq!(Language::of("x.tsx"), Some(Language::Tsx));
        assert_eq!(Language::of("main.go"), Some(Language::Go));
        assert_eq!(Language::of("README.md"), None);
        assert!(extract("notes.txt", "hello").is_none());
    }

    #[test]
    fn python_methods_nest_under_classes_and_calls_become_references() {
        let source = "class Cache:\n    def get(self, key):\n        return lookup(key)\n\ndef lookup(key):\n    return key\n";
        let symbols = extract("pkg/cache.py", source).unwrap().unwrap();
        let get = symbols
            .iter()
            .find(|s| s.id == "pkg/cache.py::Cache::get")
            .unwrap();
        assert_eq!(get.kind, SymbolKind::Method);
        assert_eq!((get.start, get.end), (2, 3));
        assert!(get.refs.contains("lookup"));
        assert!(
            symbols
                .iter()
                .any(|s| s.id == "pkg/cache.py::lookup" && s.kind == SymbolKind::Fn)
        );
        assert!(!get.test);
        assert!(extract("tests/test_cache.py", source).unwrap().unwrap()[0].test);
    }

    #[test]
    fn typescript_go_java_and_javascript_yield_symbols() {
        let ts = ids(
            "src/a.ts",
            "interface Shape { area(): number }\nclass Box implements Shape { area() { return size(); } }\nfunction size() { return 1 }\n",
        );
        assert!(ts.contains(&"src/a.ts::Shape".to_owned()), "{ts:?}");
        assert!(ts.contains(&"src/a.ts::Box::area".to_owned()), "{ts:?}");
        assert!(ts.contains(&"src/a.ts::size".to_owned()), "{ts:?}");
        let go = ids(
            "main.go",
            "package main\ntype Server struct{}\nfunc (s *Server) Run() { start() }\nfunc start() {}\n",
        );
        assert!(go.iter().any(|i| i.ends_with("::Run")), "{go:?}");
        assert!(go.contains(&"main.go::start".to_owned()), "{go:?}");
        let java = ids(
            "App.java",
            "class App { void run() { helper(); } void helper() {} }\n",
        );
        assert!(java.contains(&"App.java::App::run".to_owned()), "{java:?}");
        let js = ids(
            "web/app.js",
            "function render() { return view() }\nconst view = () => 1\n",
        );
        assert!(js.contains(&"web/app.js::render".to_owned()), "{js:?}");
    }

    #[test]
    fn rust_keeps_loom_item_paths() {
        assert_eq!(
            ids("src/lib.rs", "struct A;\nimpl A { fn go(&self) {} }\n"),
            extract("src/lib.rs", "struct A;\nimpl A { fn go(&self) {} }\n")
                .unwrap()
                .unwrap()
                .into_iter()
                .map(|s| s.id)
                .collect::<Vec<_>>()
        );
        assert!(ids("src/lib.rs", "fn go() {}\n").contains(&"src/lib.rs::go".to_owned()));
    }
}
