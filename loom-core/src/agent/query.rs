//! Exact set algebra over revision-pinned targets; projections never truncate membership.
use crate::agent::world::{Node, View};
use anyhow::{Context, Result, bail, ensure};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug)]
pub struct NodeSet {
    pub revision: String,
    pub nodes: BTreeSet<Node>,
}
#[derive(Default)]
pub struct Sets {
    serial: u64,
    sets: BTreeMap<String, NodeSet>,
}

#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relation {
    Owner,
    Children,
    Descendants,
    Files,
    Symbols,
    References,
    ReferencedBy,
    Tests,
    Package,
}
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Operator {
    Union,
    Intersect,
    Difference,
}
#[derive(Clone, Copy, Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FindKind {
    Symbol,
    Path,
    Text,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Find {
    pub query: String,
    pub kind: FindKind,
    #[serde(default)]
    pub scope: Option<String>,
}

impl Sets {
    pub fn store(&mut self, view: &View, nodes: BTreeSet<Node>) -> Result<Value> {
        // The same members on the same revision keep their handle: a repeated query is
        // recognizable as such and does not use up the handle limit.
        if let Some(handle) = self
            .sets
            .iter()
            .find(|(_, set)| set.revision == view.revision() && set.nodes == nodes)
            .map(|(handle, _)| handle.clone())
        {
            return self.project(view, &handle, 8, 0);
        }
        ensure!(self.sets.len() < 4096, "query handle limit reached");
        self.serial += 1;
        let handle = format!("S{}", self.serial);
        self.sets.insert(
            handle.clone(),
            NodeSet {
                revision: view.revision().into(),
                nodes,
            },
        );
        self.project(view, &handle, 8, 0)
    }
    pub fn get(&self, view: &View, handle: &str) -> Result<&NodeSet> {
        let set = self.sets.get(handle).context(
            "unknown set handle; use a returned handle such as S1, not a path. For a path, call find with kind=path and query=the path, then use its returned set handle",
        )?;
        ensure!(
            set.revision == view.revision(),
            "stale set {handle}; find again in the current revision"
        );
        Ok(set)
    }
    pub fn find(&mut self, view: &View, args: Find) -> Result<Value> {
        ensure!(!args.query.trim().is_empty(), "query must not be empty");
        let scope = args.scope.as_deref().unwrap_or(".");
        if scope != "." {
            view.resolve(scope)?;
        }
        let scoped = |n: &Node| {
            scope == "."
                || n.id() == scope
                || n.id().starts_with(&format!("{scope}/"))
                || n.id().starts_with(&format!("{scope}::"))
        };
        let q = args.query.to_lowercase();
        let mut nodes = BTreeSet::new();
        match args.kind {
            FindKind::Symbol => {
                for node in view.nodes.iter().filter(|n| scoped(n)) {
                    if let Node::Symbol(id) = node {
                        let symbol = view.world.graph.get(id).context("missing symbol")?;
                        if symbol.name.to_lowercase().contains(&q) || id.to_lowercase().contains(&q)
                        {
                            nodes.insert(node.clone());
                        }
                    }
                }
            }
            FindKind::Path => {
                for node in view.nodes.iter().filter(|n| scoped(n)) {
                    if !matches!(node, Node::Symbol(_)) && node.id().to_lowercase().contains(&q) {
                        nodes.insert(node.clone());
                    }
                }
            }
            FindKind::Text => {
                for source in view.world.snapshot.files.sources() {
                    let (path, _, text) = source?;
                    let file = path.to_string_lossy().replace('\\', "/");
                    let symbol_scope = view
                        .world
                        .graph
                        .get(scope)
                        .filter(|symbol| !symbol.is_pseudo());
                    if !scoped(&Node::File(file.clone()))
                        && !symbol_scope.is_some_and(|symbol| symbol.file == file)
                    {
                        continue;
                    }
                    for (i, line) in text.lines().enumerate().filter(|(i, line)| {
                        line.contains(&args.query)
                            && symbol_scope
                                .is_none_or(|symbol| i + 1 >= symbol.start && i + 1 <= symbol.end)
                    }) {
                        let _ = line;
                        nodes.insert(view.world.graph.symbol_at(&file, i + 1).map_or_else(
                            || Node::File(file.clone()),
                            |s| Node::Symbol(s.id.clone()),
                        ));
                    }
                }
            }
        }
        self.store(view, nodes)
    }
    pub fn expand(&mut self, view: &View, handle: &str, relation: Relation) -> Result<Value> {
        let mut out = BTreeSet::new();
        for node in &self.get(view, handle)?.nodes {
            match relation {
                Relation::Owner => { if let Some(owner) = view.owner(node) { out.insert(owner); } }
                Relation::Children => out.extend(view.nodes.iter().filter(|n| view.owner(n).as_ref() == Some(node)).cloned()),
                Relation::Descendants => out.extend(view.nodes.iter().filter(|n| *n != node && (n.id().starts_with(&format!("{}/", node.id())) || n.id().starts_with(&format!("{}::",node.id())) || node.id() == ".")).cloned()),
                Relation::Files => match node {
                    Node::Directory(path) => out.extend(view.nodes.iter().filter(|n| matches!(n,Node::File(p) if path=="." || p.starts_with(&format!("{path}/")))).cloned()),
                    _ => { if let Some(p) = node.file() { out.insert(Node::File(p.into())); } }
                },
                Relation::Symbols => out.extend(view.nodes.iter().filter(|n| matches!(n,Node::Symbol(id) if id == node.id() || id.starts_with(&format!("{}::", node.id())) || id.starts_with(&format!("{}/", node.id())) || node.id()==".")).cloned()),
                Relation::References | Relation::ReferencedBy | Relation::Tests => {
                    let ids: Vec<_> = match node { Node::Symbol(id) => vec![id.as_str()], Node::File(file) => view.world.graph.in_file(file).map(|s| s.id.as_str()).collect(), Node::Directory(_) => vec![] };
                    for id in ids {
                        if matches!(relation, Relation::Tests) {
                            let mut todo = vec![id.to_owned()]; let mut seen = BTreeSet::new();
                            while let Some(next) = todo.pop() {
                                if !seen.insert(next.clone()) { continue; }
                                if view.world.graph.get(&next).is_some_and(|s| s.test) { out.insert(Node::Symbol(next.clone())); }
                                todo.extend(view.world.graph.referrers(&next).iter().map(|s| s.id.clone()));
                            }
                        } else {
                            let related = if matches!(relation,Relation::References) { view.world.graph.resolve(id) } else { view.world.graph.referrers(id) };
                            out.extend(related.iter().filter(|s| !s.is_pseudo()).map(|s|Node::Symbol(s.id.clone())));
                        }
                    }
                }
                Relation::Package => {
                    if let Some((dir,_)) = view.manifests.iter().filter(|(dir,_)| *dir=="." || node.id()==dir.as_str() || node.id().starts_with(&format!("{dir}/"))).max_by_key(|(dir,_)|dir.len()) { out.insert(Node::Directory(dir.clone())); }
                }
            }
        }
        self.store(view, out)
    }
    pub fn combine(&mut self, view: &View, op: Operator, handles: &[String]) -> Result<Value> {
        ensure!(
            handles.len() >= 2 && handles.len() <= 32,
            "combine requires 2-32 sets"
        );
        let mut out = self.get(view, &handles[0])?.nodes.clone();
        for handle in &handles[1..] {
            let rhs = &self.get(view, handle)?.nodes;
            out = match op {
                Operator::Union => out.union(rhs).cloned().collect(),
                Operator::Intersect => out.intersection(rhs).cloned().collect(),
                Operator::Difference => out.difference(rhs).cloned().collect(),
            };
        }
        self.store(view, out)
    }
    pub fn project(&self, view: &View, handle: &str, limit: usize, offset: usize) -> Result<Value> {
        ensure!((1..=80).contains(&limit), "projection limit must be 1-80");
        let set = self.get(view, handle)?;
        // Deterministic representatives: shallow owners first, then stable logical identity.
        let mut nodes: Vec<_> = set.nodes.iter().collect();
        nodes.sort_by_key(|n| {
            (
                n.id().matches('/').count() + n.id().matches("::").count(),
                n.id(),
            )
        });
        let shown: Vec<_> = nodes
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|node| match node {
                Node::Symbol(id) => {
                    let s = view.world.graph.get(id).unwrap();
                    json!({"target":id,"name":s.name})
                }
                Node::File(path) | Node::Directory(path) => json!({"target":path}),
            })
            .collect();
        Ok(
            json!({"set":handle,"revision":set.revision,"count":set.nodes.len(),"representatives":shown,"next":(offset+shown.len()<set.nodes.len()).then_some(offset+shown.len())}),
        )
    }
    pub fn targets(&self, view: &View, target: &str) -> Result<Vec<Node>> {
        if self.sets.contains_key(target) {
            Ok(self.get(view, target)?.nodes.iter().cloned().collect())
        } else {
            Ok(vec![view.resolve(target)?])
        }
    }
    pub fn single(&self, view: &View, target: &str) -> Result<Node> {
        let nodes = self.targets(view, target)?;
        if nodes.len() != 1 {
            bail!("select one target; set contains {} nodes", nodes.len());
        }
        Ok(nodes[0].clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_requires_explicit_kind_and_rejects_scope_as_kind() {
        assert!(serde_json::from_value::<Find>(json!({"query":"router"})).is_err());
        assert!(serde_json::from_value::<Find>(json!({"query":"router","scope":"path"})).is_err());
        assert!(serde_json::from_value::<Find>(json!({"query":"router","kind":"path"})).is_ok());
    }
}
