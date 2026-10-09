//! L1: immutable, revision-pinned repository knowledge shared across workers.
pub mod graph;
pub mod lang;
pub mod map;
pub mod repository;
pub mod split;
pub mod structural;

use crate::base::Revision;
use anyhow::{Context, Result};
pub use graph::{CodeGraph, GraphIndex, Symbol, SymbolKind};
pub use repository::{
    Evidence, ExploreResult, RepositoryExplorer as RepoIndex, RepositorySnapshot as FileTable,
};
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

#[derive(Clone)]
pub struct Snapshot {
    pub revision: Revision,
    pub files: Arc<FileTable>,
}

#[derive(Clone)]
pub struct World {
    pub snapshot: Snapshot,
    pub index: Arc<RepoIndex>,
    pub graph: Arc<CodeGraph>,
    root: PathBuf,
    graph_index: Arc<GraphIndex>,
}

impl World {
    pub fn open(root: &Path) -> Result<Self> {
        Self::open_with_graph(root, Arc::new(GraphIndex::default()))
    }
    /// Branch snapshots share digest-keyed parses, while retaining their own revision.
    pub fn open_with_graph(root: &Path, graph_index: Arc<GraphIndex>) -> Result<Self> {
        let root = root.canonicalize()?;
        let index = Arc::new(RepoIndex::new(&root)?);
        Self::capture(root, index, graph_index)
    }
    /// Read an already captured Git revision, without recapturing working files.
    /// Callers pairing this World with live tools must first materialize that revision.
    pub fn open_at(root: &Path, revision: &Revision, graph_index: Arc<GraphIndex>) -> Result<Self> {
        Self::open_at_cancellable(root, revision, graph_index, &std::sync::atomic::AtomicBool::new(false))
    }
    pub fn open_at_cancellable(root: &Path, revision: &Revision, graph_index: Arc<GraphIndex>, cancel: &std::sync::atomic::AtomicBool) -> Result<Self> {
        anyhow::ensure!(!cancel.load(std::sync::atomic::Ordering::Acquire), "indexing cancelled");
        let root = root.canonicalize()?;
        let index = Arc::new(RepoIndex::new(&root)?);
        let files = index.snapshot_at_cancellable(revision, cancel)?;
        let graph = graph_index.graph_cancellable(&files, cancel)?;
        Ok(Self { snapshot: Snapshot { revision: revision.clone(), files }, index, graph, root, graph_index })
    }
    /// A checkout of `snapshot`'s revision at `root`: its files are reused, not re-read.
    pub fn open_with_snapshot(root: &Path, revision: &Revision, snapshot: &FileTable, graph_index: Arc<GraphIndex>, cancel: &std::sync::atomic::AtomicBool) -> Result<Self> {
        anyhow::ensure!(snapshot.revision == revision.as_str(), "snapshot of another revision");
        let root = root.canonicalize()?;
        let index = Arc::new(RepoIndex::new(&root)?);
        let files = snapshot.rebased(&root)?;
        let graph = graph_index.graph_cancellable(&files, cancel)?;
        Ok(Self { snapshot: Snapshot { revision: revision.clone(), files }, index, graph, root, graph_index })
    }
    fn capture(root: PathBuf, index: Arc<RepoIndex>, graph_index: Arc<GraphIndex>) -> Result<Self> {
        let revision = Revision::capture(&root)?;
        Self::from_revision(root, index, graph_index, revision)
    }
    fn from_revision(root: PathBuf, index: Arc<RepoIndex>, graph_index: Arc<GraphIndex>, revision: Revision) -> Result<Self> {
        let files = index.snapshot_at(&revision)?;
        let graph = graph_index.graph(&files)?;
        Ok(Self {
            snapshot: Snapshot { revision, files },
            index,
            graph,
            root,
            graph_index,
        })
    }
    /// Changes are hints; the captured tree is authoritative, including unreported edits.
    /// Digest-keyed parses ensure only changed Rust files are reparsed.
    pub fn advance(&self, _changed: &[PathBuf]) -> Result<Self> {
        Self::capture(
            self.root.clone(),
            Arc::clone(&self.index),
            Arc::clone(&self.graph_index),
        )
    }
    pub fn find(&self, query: &str, budget_chars: usize) -> Result<Arc<ExploreResult>> {
        self.index
            .explore_snapshot(&self.snapshot.files, query, budget_chars)
    }
    pub fn parses(&self) -> usize {
        self.graph_index.parses()
    }
    pub fn query_count(&self) -> usize {
        self.index.counts().1
    }
    pub fn signature(&self, id: &str) -> Result<String> {
        let symbol = self.graph.get(id).context("Unknown symbol")?;
        for source in self.snapshot.files.sources() {
            let (path, _, text) = source?;
            if path == Path::new(&symbol.file) {
                let source = text
                    .lines()
                    .skip(symbol.start.saturating_sub(1))
                    .take(symbol.lines())
                    .collect::<Vec<_>>()
                    .join("\n");
                return Ok(source
                    .split(['{', ';'])
                    .next()
                    .unwrap_or(&source)
                    .trim()
                    .to_owned());
            }
        }
        anyhow::bail!("Symbol source missing from snapshot")
    }
}
