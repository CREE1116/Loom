//! Revision-pinned code graph: item-level symbols and name-resolved references, for every
//! language `lang` supports (Rust by the extractor below, others through `lang`).
//!
//! Symbol ids are `<file>::<item path>` (`src/walk.rs::WorkerState::spawn_senders`), the
//! scheme validated in experiments/effect-contract. References are resolved by name and
//! are deliberately over-approximate; Core uses them for boundaries and context slices,
//! never as proof that two edits are independent.
use super::repository::RepositorySnapshot;
use anyhow::Result;
use proc_macro2::{TokenStream, TokenTree};
use quote::ToTokens;
use std::{
    collections::{BTreeSet, HashMap, HashSet},
    path::Path,
    sync::{Arc, Mutex, OnceLock},
};
use syn::spanned::Spanned;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SymbolKind {
    Fn,
    Method,
    Struct,
    Enum,
    Union,
    Trait,
    Impl,
    Const,
    Static,
    Type,
    Macro,
    Mod,
    Imports,
    Field,
    Variant,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct Symbol {
    pub id: String,
    pub name: String,
    pub kind: SymbolKind,
    pub file: String,
    pub start: usize,
    pub end: usize,
    pub refs: BTreeSet<String>,
    pub test: bool,
}

impl Symbol {
    pub fn lines(&self) -> usize {
        self.end + 1 - self.start
    }
    /// Pseudo-symbols merge textually; they never signal a semantic conflict.
    pub fn is_pseudo(&self) -> bool {
        self.kind == SymbolKind::Imports
    }
}

// ------------------------------------------------------------------ extraction

fn idents(tokens: TokenStream, out: &mut BTreeSet<String>) {
    for tree in tokens {
        match tree {
            TokenTree::Ident(ident) => {
                let text = ident.to_string();
                out.insert(text.strip_prefix("r#").unwrap_or(&text).to_owned());
            }
            TokenTree::Group(group) => idents(group.stream(), out),
            _ => {}
        }
    }
}

fn has_test_attr(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|attr| {
        let text = attr.to_token_stream().to_string().replace(' ', "");
        text.contains("#[test]")
            || text.contains("::test]")
            || text.contains("cfg(test)")
            || text.contains("#[bench]")
    })
}

fn span_lines(node: &impl Spanned, attrs: &[syn::Attribute]) -> (usize, usize) {
    let span = node.span();
    let start = attrs
        .iter()
        .map(|a| a.span().start().line)
        .fold(span.start().line, usize::min);
    (start, span.end().line)
}

fn type_name(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(path) => path
            .path
            .segments
            .last()
            .map(|s| s.ident.to_string())
            .unwrap_or_else(|| "_".into()),
        syn::Type::Reference(reference) => type_name(&reference.elem),
        other => other
            .to_token_stream()
            .to_string()
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '_')
            .collect(),
    }
}

struct Extractor<'a> {
    file: &'a str,
    out: Vec<Symbol>,
}

impl Extractor<'_> {
    fn push(
        &mut self,
        prefix: &[String],
        name: String,
        kind: SymbolKind,
        span: (usize, usize),
        tokens: TokenStream,
        test: bool,
    ) {
        let mut refs = BTreeSet::new();
        idents(tokens, &mut refs);
        let mut path = prefix.to_vec();
        path.push(name.clone());
        self.out.push(Symbol {
            id: format!("{}::{}", self.file, path.join("::")),
            name,
            kind,
            file: self.file.to_owned(),
            start: span.0,
            end: span.1,
            refs,
            test,
        });
    }

    fn fields(&mut self, prefix: &[String], owner: &str, fields: &syn::Fields, test: bool) {
        for (index, field) in fields.iter().enumerate() {
            let name = field
                .ident
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_else(|| index.to_string());
            self.push(
                prefix,
                format!("{owner}.{name}"),
                SymbolKind::Field,
                span_lines(field, &field.attrs),
                field.to_token_stream(),
                test,
            );
            self.out.last_mut().unwrap().name = name;
        }
    }

    fn items(&mut self, items: &[syn::Item], prefix: &[String], test: bool) {
        for item in items {
            self.item(item, prefix, test);
        }
    }

    fn item(&mut self, item: &syn::Item, prefix: &[String], test: bool) {
        use syn::Item;
        let tokens = item.to_token_stream();
        match item {
            Item::Fn(f) => {
                let test = test || has_test_attr(&f.attrs);
                self.push(
                    prefix,
                    f.sig.ident.to_string(),
                    SymbolKind::Fn,
                    span_lines(f, &f.attrs),
                    tokens,
                    test,
                )
            }
            Item::Struct(s) => {
                self.push(
                    prefix,
                    s.ident.to_string(),
                    SymbolKind::Struct,
                    span_lines(s, &s.attrs),
                    tokens,
                    test,
                );
                self.fields(prefix, &s.ident.to_string(), &s.fields, test);
            }
            Item::Enum(e) => {
                self.push(
                    prefix,
                    e.ident.to_string(),
                    SymbolKind::Enum,
                    span_lines(e, &e.attrs),
                    tokens,
                    test,
                );
                for variant in &e.variants {
                    let name = format!("{}.{}", e.ident, variant.ident);
                    self.push(
                        prefix,
                        name.clone(),
                        SymbolKind::Variant,
                        span_lines(variant, &variant.attrs),
                        variant.to_token_stream(),
                        test,
                    );
                    self.out.last_mut().unwrap().name = variant.ident.to_string();
                    self.fields(prefix, &name, &variant.fields, test);
                }
            }
            Item::Union(u) => self.push(
                prefix,
                u.ident.to_string(),
                SymbolKind::Union,
                span_lines(u, &u.attrs),
                tokens,
                test,
            ),
            Item::Const(c) => self.push(
                prefix,
                c.ident.to_string(),
                SymbolKind::Const,
                span_lines(c, &c.attrs),
                tokens,
                test,
            ),
            Item::Static(s) => self.push(
                prefix,
                s.ident.to_string(),
                SymbolKind::Static,
                span_lines(s, &s.attrs),
                tokens,
                test,
            ),
            Item::Type(t) => self.push(
                prefix,
                t.ident.to_string(),
                SymbolKind::Type,
                span_lines(t, &t.attrs),
                tokens,
                test,
            ),
            Item::Macro(m) => {
                let name = m.ident.as_ref().map(|i| i.to_string()).unwrap_or_else(|| {
                    // `rgtest!(name, ...)` → `rgtest!name`: stable across line shifts.
                    let mut first = BTreeSet::new();
                    idents(
                        m.mac.tokens.clone().into_iter().take(1).collect(),
                        &mut first,
                    );
                    let path = m.mac.path.to_token_stream().to_string().replace(' ', "");
                    format!("{path}!{}", first.into_iter().next().unwrap_or_default())
                });
                self.push(
                    prefix,
                    name,
                    SymbolKind::Macro,
                    span_lines(m, &m.attrs),
                    tokens,
                    test,
                )
            }
            Item::Trait(t) => {
                let name = t.ident.to_string();
                let mut inner = prefix.to_vec();
                inner.push(name.clone());
                let mut header = TokenStream::new();
                t.ident.to_tokens(&mut header);
                t.generics.to_tokens(&mut header);
                t.supertraits.to_tokens(&mut header);
                for trait_item in &t.items {
                    if let syn::TraitItem::Fn(f) = trait_item {
                        self.push(
                            &inner,
                            f.sig.ident.to_string(),
                            SymbolKind::Method,
                            span_lines(f, &f.attrs),
                            f.to_token_stream(),
                            test,
                        );
                    } else {
                        trait_item.to_tokens(&mut header);
                    }
                }
                self.push(
                    prefix,
                    name,
                    SymbolKind::Trait,
                    span_lines(t, &t.attrs),
                    header,
                    test,
                )
            }
            Item::Impl(i) => {
                let ty = type_name(&i.self_ty);
                let label = match &i.trait_ {
                    Some((_, path, _)) => format!(
                        "impl {} for {ty}",
                        path.segments
                            .last()
                            .map(|s| s.ident.to_string())
                            .unwrap_or_default()
                    ),
                    None => format!("impl {ty}"),
                };
                let test = test || has_test_attr(&i.attrs);
                let mut header = TokenStream::new();
                i.generics.to_tokens(&mut header);
                if let Some((_, path, _)) = &i.trait_ {
                    path.to_tokens(&mut header);
                }
                i.self_ty.to_tokens(&mut header);
                let mut inner = prefix.to_vec();
                inner.push(ty);
                for impl_item in &i.items {
                    match impl_item {
                        syn::ImplItem::Fn(f) => {
                            let mut tokens = f.to_token_stream();
                            // A method implicitly reads its self type.
                            header.clone().to_tokens(&mut tokens);
                            let test = test || has_test_attr(&f.attrs);
                            self.push(
                                &inner,
                                f.sig.ident.to_string(),
                                SymbolKind::Method,
                                span_lines(f, &f.attrs),
                                tokens,
                                test,
                            )
                        }
                        other => other.to_tokens(&mut header),
                    }
                }
                self.push(
                    prefix,
                    label,
                    SymbolKind::Impl,
                    span_lines(i, &i.attrs),
                    header,
                    test,
                )
            }
            Item::Mod(m) => {
                let name = m.ident.to_string();
                let test = test || has_test_attr(&m.attrs) || name == "tests";
                match &m.content {
                    Some((_, inner_items)) => {
                        let mut inner = prefix.to_vec();
                        inner.push(name);
                        self.items(inner_items, &inner, test)
                    }
                    None => self.push(
                        prefix,
                        format!("mod {name}"),
                        SymbolKind::Mod,
                        span_lines(m, &m.attrs),
                        tokens,
                        test,
                    ),
                }
            }
            Item::Use(u) => self.push(
                prefix,
                "<imports>".into(),
                SymbolKind::Imports,
                span_lines(u, &u.attrs),
                tokens,
                test,
            ),
            Item::ExternCrate(e) => self.push(
                prefix,
                "<imports>".into(),
                SymbolKind::Imports,
                span_lines(e, &e.attrs),
                tokens,
                test,
            ),
            _ => {}
        }
    }
}

/// Parse one Rust file. Unparseable files yield no symbols (the caller records them).
pub fn extract(file: &str, source: &str) -> std::result::Result<Vec<Symbol>, String> {
    let parsed = syn::parse_file(source).map_err(|e| format!("{}:{e}", e.span().start().line))?;
    let test =
        file.starts_with("tests/") || file.contains("/tests/") || file.ends_with("/tests.rs");
    let mut extractor = Extractor {
        file,
        out: Vec::new(),
    };
    extractor.items(&parsed.items, &[], test);
    Ok(extractor.out)
}

// ------------------------------------------------------------------ graph

pub struct CodeGraph {
    pub revision: String,
    symbols: Vec<Symbol>,
    by_id: HashMap<String, usize>,
    by_name: HashMap<String, Vec<usize>>,
    by_file: HashMap<String, Vec<usize>>,
    adjacency: OnceLock<Adjacency>,
    pub parse_errors: Vec<(String, String)>,
}

struct Adjacency {
    resolved: HashMap<String, Vec<usize>>,
    incoming: HashMap<String, Vec<usize>>,
}

fn crate_of(path: &str) -> &str {
    let mut parts = path.splitn(3, '/');
    match (parts.next(), parts.next()) {
        (Some("crates"), Some(name)) => &path[..7 + name.len()],
        _ => "",
    }
}

impl CodeGraph {
    fn new(
        revision: String,
        files: Vec<Arc<Vec<Symbol>>>,
        parse_errors: Vec<(String, String)>,
    ) -> Self {
        let mut symbols = Vec::new();
        let mut by_id = HashMap::new();
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_file: HashMap<String, Vec<usize>> = HashMap::new();
        for file in files {
            for symbol in file.iter() {
                let index = symbols.len();
                by_file.entry(symbol.file.clone()).or_default().push(index);
                // Repeated ids (`use` groups, cfg-gated twins) share one identity: the
                // first entry carries the union of references; every span stays in by_file.
                match by_id.get(&symbol.id) {
                    Some(&first) => {
                        let refs = symbol.refs.clone();
                        let first: &mut Symbol = &mut symbols[first];
                        first.refs.extend(refs);
                    }
                    None => {
                        by_id.insert(symbol.id.clone(), index);
                        by_name.entry(symbol.name.clone()).or_default().push(index);
                    }
                }
                symbols.push(symbol.clone());
            }
        }
        Self {
            revision,
            symbols,
            by_id,
            by_name,
            by_file,
            adjacency: OnceLock::new(),
            parse_errors,
        }
    }

    fn adjacency(&self) -> &Adjacency {
        self.adjacency.get_or_init(|| {
            let mut resolved = HashMap::new();
            for id in self.by_id.keys() {
                let indices = self
                    .resolve_uncached(id)
                    .into_iter()
                    .filter_map(|symbol| self.by_id.get(&symbol.id).copied())
                    .collect::<Vec<_>>();
                resolved.insert(id.clone(), indices);
            }
            let mut incoming: HashMap<String, Vec<usize>> = HashMap::new();
            for (index, symbol) in self.symbols.iter().enumerate() {
                let Some(targets) = resolved.get(&symbol.id) else {
                    continue;
                };
                for &target in targets {
                    let target = &self.symbols[target];
                    if symbol.refs.contains(&target.name) {
                        incoming.entry(target.id.clone()).or_default().push(index);
                    }
                }
            }
            Adjacency { resolved, incoming }
        })
    }

    /// Every symbol, in file order.
    pub fn symbols(&self) -> &[Symbol] {
        &self.symbols
    }

    pub fn len(&self) -> usize {
        self.symbols.len()
    }

    pub fn is_empty(&self) -> bool {
        self.symbols.is_empty()
    }

    pub fn get(&self, id: &str) -> Option<&Symbol> {
        self.by_id.get(id).map(|&i| &self.symbols[i])
    }

    pub fn in_file(&self, file: &str) -> impl Iterator<Item = &Symbol> {
        self.by_file
            .get(file)
            .into_iter()
            .flatten()
            .map(|&i| &self.symbols[i])
    }

    pub fn named(&self, name: &str) -> impl Iterator<Item = &Symbol> {
        self.by_name
            .get(name)
            .into_iter()
            .flatten()
            .map(|&i| &self.symbols[i])
    }

    /// Innermost item containing `line`; `None` means file toplevel.
    pub fn symbol_at(&self, file: &str, line: usize) -> Option<&Symbol> {
        self.in_file(file)
            .filter(|s| s.start <= line && line <= s.end)
            .min_by_key(|s| s.end - s.start)
    }

    /// Repository symbols that `id` references, by name.
    ///
    /// A method `T::m` is reached through `m` only when `T` is also referenced or the
    /// owner lives in `T`; free items prefer candidates in the owner's crate.
    pub fn resolve(&self, id: &str) -> Vec<&Symbol> {
        self.adjacency()
            .resolved
            .get(id)
            .into_iter()
            .flatten()
            .map(|&index| &self.symbols[index])
            .collect()
    }

    fn resolve_uncached(&self, id: &str) -> Vec<&Symbol> {
        let Some(owner) = self.get(id) else {
            return Vec::new();
        };
        let owner_path: HashSet<&str> = owner.id.split("::").collect();
        let mut out: Vec<&Symbol> = Vec::new();
        let mut seen = HashSet::new();
        for name in &owner.refs {
            if *name == owner.name {
                continue;
            }
            let (methods, free): (Vec<&Symbol>, Vec<&Symbol>) =
                self.named(name).partition(|c| c.kind == SymbolKind::Method);
            for candidate in methods {
                let ty = candidate.id.rsplit("::").nth(1).unwrap_or("");
                if (owner.refs.contains(ty) || owner_path.contains(ty))
                    && seen.insert(candidate.id.as_str())
                {
                    out.push(candidate);
                }
            }
            let same: Vec<&Symbol> = free
                .iter()
                .copied()
                .filter(|c| crate_of(&c.file) == crate_of(&owner.file))
                .collect();
            for candidate in if same.is_empty() { free } else { same } {
                if candidate.id != owner.id && seen.insert(candidate.id.as_str()) {
                    out.push(candidate);
                }
            }
        }
        out
    }

    /// Direct dependencies and callers, with stable ordering and no duplicates.
    pub fn neighbors(&self, id: &str) -> Vec<&Symbol> {
        let mut neighbors = self.resolve(id);
        neighbors.extend(self.referrers(id));
        neighbors.sort_by(|a, b| a.id.cmp(&b.id));
        neighbors.dedup_by(|a, b| a.id == b.id);
        neighbors
    }

    /// Symbols whose references resolve to `id` (direct callers/users).
    pub fn referrers(&self, id: &str) -> Vec<&Symbol> {
        self.adjacency()
            .incoming
            .get(id)
            .into_iter()
            .flatten()
            .map(|&index| &self.symbols[index])
            .filter(|symbol| symbol.id != id)
            .collect()
    }
}

/// One file's parse at one content digest: symbols, or the parse error.
type Parsed = Arc<std::result::Result<Vec<Symbol>, String>>;

/// Builds graphs for repository snapshots, reparsing only files whose digest changed.
#[derive(Default)]
pub struct GraphIndex {
    parsed: Mutex<HashMap<(String, String), Parsed>>,
    current: Mutex<Option<Arc<CodeGraph>>>,
    parses: std::sync::atomic::AtomicUsize,
}

impl GraphIndex {
    pub fn graph(&self, snapshot: &RepositorySnapshot) -> Result<Arc<CodeGraph>> {
        self.graph_cancellable(snapshot, &std::sync::atomic::AtomicBool::new(false))
    }

    pub fn graph_cancellable(&self, snapshot: &RepositorySnapshot, cancel: &std::sync::atomic::AtomicBool) -> Result<Arc<CodeGraph>> {
        let check = || -> Result<()> { anyhow::ensure!(!cancel.load(std::sync::atomic::Ordering::Acquire), "AST indexing cancelled"); Ok(()) };
        check()?;
        if let Some(graph) = self
            .current
            .lock()
            .map_err(|_| anyhow::anyhow!("graph lock"))?
            .as_ref()
            && graph.revision == snapshot.revision
        {
            return Ok(Arc::clone(graph));
        }
        let mut live = HashSet::new();
        // Parsed files come from memory, then the shared disk cache; the rest are parsed
        // in parallel. A file's symbols depend only on its path and content.
        let mut parsed: Vec<Option<Parsed>> = Vec::new();
        let mut misses = Vec::new();
        let entries: Vec<(String, String)> = snapshot
            .entries()
            .map(|(path, digest)| (path_str(path), digest.to_owned()))
            .collect();
        for (index, (file, digest)) in entries.iter().enumerate() {
            check()?;
            if super::lang::Language::of(file).is_none() {
                parsed.push(None);
                continue;
            }
            let key = (file.clone(), digest.clone());
            live.insert(key.clone());
            let cached = self
                .parsed
                .lock()
                .map_err(|_| anyhow::anyhow!("graph lock"))?
                .get(&key)
                .cloned()
                .or_else(|| disk_cache::load(file, digest).map(Arc::new));
            if cached.is_none() {
                misses.push(index);
            }
            parsed.push(cached);
        }
        let workers = std::thread::available_parallelism()
            .map_or(4, |n| n.get())
            .min(16);
        let chunk = misses.len().div_ceil(workers).max(1);
        let results: Vec<(usize, Parsed)> = std::thread::scope(|scope| {
            let handles: Vec<_> = misses
                .chunks(chunk)
                .map(|indices| {
                    let entries = &entries;
                    scope.spawn(move || -> Result<Vec<(usize, Parsed)>> {
                        let mut out = Vec::with_capacity(indices.len());
                        for &index in indices {
                            check()?;
                            let (file, digest) = &entries[index];
                            let text = snapshot.text(index)?;
                            let result =
                                super::lang::extract(file, &text).expect("language checked above");
                            check()?;
                            disk_cache::store(file, digest, &result);
                            out.push((index, Arc::new(result)));
                        }
                        Ok(out)
                    })
                })
                .collect();
            let mut all = Vec::new();
            for handle in handles {
                all.extend(
                    handle
                        .join()
                        .map_err(|_| anyhow::anyhow!("graph worker panicked"))??,
                );
            }
            Ok::<_, anyhow::Error>(all)
        })?;
        check()?;
        self.parses
            .fetch_add(results.len(), std::sync::atomic::Ordering::Relaxed);
        {
            let mut memory = self
                .parsed
                .lock()
                .map_err(|_| anyhow::anyhow!("graph lock"))?;
            for (index, result) in results {
                let (file, digest) = &entries[index];
                memory.insert((file.clone(), digest.clone()), Arc::clone(&result));
                parsed[index] = Some(result);
            }
            for (index, cached) in parsed.iter().enumerate() {
                if let Some(cached) = cached {
                    let (file, digest) = &entries[index];
                    memory
                        .entry((file.clone(), digest.clone()))
                        .or_insert_with(|| Arc::clone(cached));
                }
            }
        }
        let mut files = Vec::new();
        let mut errors = Vec::new();
        for (index, parsed) in parsed.into_iter().enumerate() {
            match parsed.as_deref() {
                Some(Ok(symbols)) => files.push(Arc::new(symbols.clone())),
                Some(Err(error)) => errors.push((entries[index].0.clone(), error.clone())),
                None => {}
            }
        }
        // Old revisions' parses are released once no snapshot lists them.
        self.parsed
            .lock()
            .map_err(|_| anyhow::anyhow!("graph lock"))?
            .retain(|key, _| live.contains(key));
        check()?;
        let graph = Arc::new(CodeGraph::new(snapshot.revision.clone(), files, errors));
        *self
            .current
            .lock()
            .map_err(|_| anyhow::anyhow!("graph lock"))? = Some(Arc::clone(&graph));
        Ok(graph)
    }

    /// Files parsed so far (cache misses).
    pub fn parses(&self) -> usize {
        self.parses.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Parsed symbols shared across processes, overlays and repositories, keyed by the
/// extractor version, the file path (symbol ids contain it) and the content digest.
mod disk_cache {
    use super::Symbol;
    use sha2::{Digest, Sha256};
    use std::path::PathBuf;

    /// Bump when an extractor's output changes for the same input.
    const VERSION: &str = "symbols-v1";

    fn path(file: &str, digest: &str) -> Option<PathBuf> {
        // Unit tests count parses; a cache shared with earlier runs would hide them.
        if cfg!(test) || std::env::var_os("LOOM_NO_GRAPH_CACHE").is_some() {
            return None;
        }
        let home = std::env::var_os("HOME")?;
        let key = format!(
            "{:x}",
            Sha256::digest(format!("{VERSION}\0{file}\0{digest}"))
        );
        Some(
            PathBuf::from(home)
                .join(".loom/cache/symbols")
                .join(&key[..2])
                .join(&key[2..]),
        )
    }

    pub fn load(file: &str, digest: &str) -> Option<Result<Vec<Symbol>, String>> {
        serde_json::from_slice(&std::fs::read(path(file, digest)?).ok()?).ok()
    }

    pub fn store(file: &str, digest: &str, result: &Result<Vec<Symbol>, String>) {
        let Some(path) = path(file, digest) else {
            return;
        };
        let Ok(bytes) = serde_json::to_vec(result) else {
            return;
        };
        if let Some(dir) = path.parent()
            && std::fs::create_dir_all(dir).is_ok()
        {
            // write then rename: a concurrent reader never sees a partial entry
            let tmp = path.with_extension(format!("tmp{}", std::process::id()));
            if std::fs::write(&tmp, bytes).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }
    }
}

fn path_str(path: &Path) -> String {
    path.components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOURCE: &str = r#"
use std::fmt;

pub struct Walk { depth: usize }

impl Walk {
    pub fn next(&mut self) -> usize { helper(self.depth) }
}

fn helper(n: usize) -> usize { n + 1 }

macro_rules! rgtest { ($name:ident, $body:expr) => {}; }
rgtest!(f1534_ebuild_type, |dir| {});

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn walks() { let mut w = Walk { depth: 0 }; assert_eq!(w.next(), 1); }
}
"#;

    fn graph() -> CodeGraph {
        CodeGraph::new(
            "r".into(),
            vec![Arc::new(extract("src/walk.rs", SOURCE).unwrap())],
            vec![],
        )
    }

    #[test]
    fn extracts_stable_item_ids() {
        let graph = graph();
        for id in [
            "src/walk.rs::Walk",
            "src/walk.rs::Walk::next",
            "src/walk.rs::impl Walk",
            "src/walk.rs::helper",
            "src/walk.rs::rgtest",
            "src/walk.rs::rgtest!f1534_ebuild_type",
            "src/walk.rs::tests::walks",
            "src/walk.rs::<imports>",
        ] {
            assert!(graph.get(id).is_some(), "missing {id}");
        }
        assert!(graph.get("src/walk.rs::tests::walks").unwrap().test);
        assert!(!graph.get("src/walk.rs::helper").unwrap().test);
    }

    #[test]
    fn innermost_symbol_wins() {
        let graph = graph();
        let line = SOURCE
            .lines()
            .position(|l| l.contains("helper(self.depth)"))
            .unwrap()
            + 1;
        assert_eq!(
            graph.symbol_at("src/walk.rs", line).unwrap().id,
            "src/walk.rs::Walk::next"
        );
        assert!(graph.symbol_at("src/walk.rs", 1).is_none());
    }

    #[test]
    fn adjacency_is_built_only_when_edge_queries_need_it() {
        let graph = graph();
        assert!(graph.adjacency.get().is_none());

        assert!(graph.get("src/walk.rs::helper").is_some());
        assert_eq!(graph.in_file("src/walk.rs").count(), graph.len());
        assert!(graph.adjacency.get().is_none());

        assert!(!graph.resolve("src/walk.rs::Walk::next").is_empty());
        assert!(graph.adjacency.get().is_some());
    }

    #[test]
    fn resolves_calls_and_methods_through_their_type() {
        let graph = graph();
        let ids = |v: Vec<&Symbol>| v.into_iter().map(|s| s.id.clone()).collect::<BTreeSet<_>>();
        let next = ids(graph.resolve("src/walk.rs::Walk::next"));
        assert!(next.contains("src/walk.rs::helper"));
        assert!(next.contains("src/walk.rs::Walk"));
        let test = ids(graph.resolve("src/walk.rs::tests::walks"));
        assert!(
            test.contains("src/walk.rs::Walk::next"),
            "method reached via its type: {test:?}"
        );
        let callers = ids(graph.referrers("src/walk.rs::helper"));
        assert_eq!(
            callers,
            BTreeSet::from(["src/walk.rs::Walk::next".to_owned()])
        );
    }

    #[test]
    fn index_reparses_only_changed_files() {
        use crate::world::repository::RepositoryExplorer;
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir(directory.path().join("src")).unwrap();
        std::fs::write(directory.path().join("src/a.rs"), "fn a() {}\n").unwrap();
        std::fs::write(directory.path().join("src/b.rs"), "fn b() { a(); }\n").unwrap();
        let explorer = RepositoryExplorer::new(directory.path()).unwrap();
        let index = GraphIndex::default();
        let first = index.graph(&explorer.snapshot().unwrap()).unwrap();
        assert_eq!(index.parses(), 2);
        assert!(first.get("src/b.rs::b").is_some());
        assert!(Arc::ptr_eq(
            &first,
            &index.graph(&explorer.snapshot().unwrap()).unwrap()
        ));
        std::fs::write(directory.path().join("src/b.rs"), "fn b2() { a(); }\n").unwrap();
        let second = index.graph(&explorer.snapshot().unwrap()).unwrap();
        assert_eq!(index.parses(), 3, "only the changed file is reparsed");
        assert!(second.get("src/b.rs::b2").is_some() && second.get("src/b.rs::b").is_none());
        assert!(
            first.get("src/b.rs::b").is_some(),
            "older graphs stay valid for running tasks"
        );
    }
}
