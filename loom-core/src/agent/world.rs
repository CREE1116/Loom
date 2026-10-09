use anyhow::{Result, ensure};
use crate::world::{GraphIndex, World};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
};

#[derive(Clone, Default)]
pub struct WorldHub {
    graph: Arc<GraphIndex>,
    /// The last overlay snapshot: the next turn on the same revision reuses its files
    /// instead of reading every blob again.
    last: Arc<std::sync::Mutex<Option<Arc<crate::world::FileTable>>>>,
}
impl WorldHub {
    pub fn view(&self, root: &Path) -> Result<View> {
        Self::from_world(World::open_with_graph(root, self.graph.clone())?)
    }
    pub fn view_cancellable(&self, root: &Path, cancel: &std::sync::atomic::AtomicBool) -> Result<View> {
        let revision = crate::base::Revision::capture_cancellable(root, cancel)?;
        Self::from_world(World::open_at_cancellable(root, &revision, self.graph.clone(), cancel)?)
    }
    /// Only for a newly materialized overlay, before any worker/tool mutations.
    pub(crate) fn initial_view(&self, overlay: &crate::effects::Overlay, cancel: &std::sync::atomic::AtomicBool) -> Result<View> {
        let cached = self.last.lock().ok().and_then(|last| last.clone()).filter(|s| s.revision == overlay.base.as_str());
        let world = match cached {
            Some(snapshot) => World::open_with_snapshot(overlay.dir(), &overlay.base, &snapshot, self.graph.clone(), cancel)?,
            None => World::open_at_cancellable(overlay.dir(), &overlay.base, self.graph.clone(), cancel)?,
        };
        if let Ok(mut last) = self.last.lock() {
            *last = Some(world.snapshot.files.clone());
        }
        Self::from_world(world)
    }
    fn from_world(world: World) -> Result<View> {
        let mut nodes = BTreeSet::from([Node::Directory(".".into())]);
        let mut manifests = BTreeMap::new();
        for file in world.snapshot.files.paths() {
            let path = file.to_string_lossy().replace('\\', "/");
            nodes.insert(Node::File(path.clone()));
            for parent in file
                .ancestors()
                .skip(1)
                .filter(|p| !p.as_os_str().is_empty())
            {
                nodes.insert(Node::Directory(parent.to_string_lossy().replace('\\', "/")));
            }
            if matches!(
                file.file_name().and_then(|s| s.to_str()),
                Some("Cargo.toml" | "package.json" | "pyproject.toml" | "go.mod")
            ) {
                let dir = file
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                manifests.insert(dir.to_string_lossy().into_owned(), path);
            }
        }
        for symbol in world.graph.symbols() {
            if !symbol.is_pseudo() {
                nodes.insert(Node::Symbol(symbol.id.clone()));
            }
        }
        Ok(View {
            world,
            nodes,
            manifests,
        })
    }
    pub fn parses(&self) -> usize {
        self.graph.parses()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum Node {
    Directory(String),
    File(String),
    Symbol(String),
}
impl Node {
    pub fn id(&self) -> &str {
        match self {
            Self::Directory(s) | Self::File(s) | Self::Symbol(s) => s,
        }
    }
    pub fn file(&self) -> Option<&str> {
        match self {
            Self::Symbol(s) => s.split_once("::").map(|(p, _)| p),
            Self::File(s) => Some(s),
            _ => None,
        }
    }
}

pub struct View {
    pub world: World,
    pub nodes: BTreeSet<Node>,
    pub manifests: BTreeMap<String, String>,
}
impl View {
    pub fn revision(&self) -> &str {
        self.world.snapshot.revision.as_str()
    }
    pub fn resolve(&self, id: &str) -> Result<Node> {
        let matches: Vec<_> = self
            .nodes
            .iter()
            .filter(|n| n.id() == id)
            .cloned()
            .collect();
        ensure!(matches.len() == 1, "unknown or ambiguous target: {id}");
        Ok(matches[0].clone())
    }
    pub fn owner(&self, node: &Node) -> Option<Node> {
        match node {
            Node::Symbol(id) => {
                let (parent, _) = id.rsplit_once("::")?;
                if self.nodes.contains(&Node::Symbol(parent.into())) {
                    Some(Node::Symbol(parent.into()))
                } else {
                    node.file().map(|p| Node::File(p.into()))
                }
            }
            Node::File(path) | Node::Directory(path) if path != "." => {
                let dir = Path::new(path)
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                Some(Node::Directory(dir.to_string_lossy().into_owned()))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod preparation_timing {
    /// Time of the steps before the first model call on a real repository.
    /// `LOOM_PREP_ROOT=/path/repo cargo test --release --lib preparation_steps -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn preparation_steps() {
        use std::{sync::atomic::AtomicBool, time::Instant};
        let root = std::path::PathBuf::from(std::env::var("LOOM_PREP_ROOT").expect("set LOOM_PREP_ROOT"));
        let cancel = AtomicBool::new(false);
        let hub = super::WorldHub::default();
        for turn in 1..=2 {
            let t = Instant::now();
            let base = crate::base::Revision::capture_cancellable(&root, &cancel).unwrap();
            let revision = t.elapsed();
            let t = Instant::now();
            let overlay = crate::effects::Overlay::create_cancellable(&root, &base, &cancel).unwrap();
            let created = t.elapsed();
            let t = Instant::now();
            let view = hub.initial_view(&overlay, &cancel).unwrap();
            let indexed = t.elapsed();
            let (files, nodes) = (view.world.snapshot.files.paths().count(), view.nodes.len());
            drop(view);
            let t = Instant::now();
            drop(overlay);
            let dropped = t.elapsed();
            eprintln!("turn {turn}: cleanup {:.2}s", dropped.as_secs_f64());
            eprintln!(
                "turn {turn}: revision {:.2}s, overlay {:.2}s, index {:.2}s ({} files, {} nodes, {} parses)",
                revision.as_secs_f64(),
                created.as_secs_f64(),
                indexed.as_secs_f64(),
                files,
                nodes,
                hub.parses()
            );
        }
    }
}
