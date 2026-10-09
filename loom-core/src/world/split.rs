//! Candidate parallel work groups derived from explicit symbol anchors.
//! The name-resolved graph is approximate: these groups are proposals. The
//! worker's file leases and patch validation provide the final write boundary.
use super::World;
use super::graph::CodeGraph;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize, serde::Deserialize)]
pub struct Group {
    pub anchors: Vec<String>,
    pub write_files: Vec<String>,
    pub read_files: Vec<String>,
}

/// (anchor ids, write files, read files) of one union-find group.
type Members = (BTreeSet<String>, BTreeSet<String>, BTreeSet<String>);

fn root(parent: &mut [usize], at: usize) -> usize {
    if parent[at] != at {
        parent[at] = root(parent, parent[at]);
    }
    parent[at]
}

/// Merge anchors when they share a write file or one group reads another's
/// declared write file. No ranking or user-tunable expansion threshold.
pub fn candidates(graph: &CodeGraph, ids: &[String]) -> Result<Vec<Group>, String> {
    if ids.is_empty() || ids.len() > 32 {
        return Err("plan_split requires 1–32 explicit symbol ids".into());
    }
    let unique: Vec<_> = ids.iter().collect::<BTreeSet<_>>().into_iter().collect();
    let anchors = unique
        .iter()
        .map(|id| {
            graph
                .get(id)
                .ok_or_else(|| format!("unknown symbol `{id}`"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let neighbors: Vec<BTreeSet<_>> = anchors
        .iter()
        .map(|a| {
            graph
                .neighbors(&a.id)
                .into_iter()
                .map(|s| s.file.as_str())
                .collect()
        })
        .collect();
    let mut parents: Vec<usize> = (0..anchors.len()).collect();
    for i in 0..anchors.len() {
        for j in i + 1..anchors.len() {
            if anchors[i].file == anchors[j].file
                || neighbors[i].contains(anchors[j].file.as_str())
                || neighbors[j].contains(anchors[i].file.as_str())
            {
                let (a, b) = (root(&mut parents, i), root(&mut parents, j));
                parents[b] = a;
            }
        }
    }
    let mut groups: BTreeMap<usize, Members> = BTreeMap::new();
    for (i, anchor) in anchors.iter().enumerate() {
        let key = root(&mut parents, i);
        let group = groups.entry(key).or_default();
        group.0.insert(anchor.id.clone());
        group.1.insert(anchor.file.clone());
        group.2.extend(neighbors[i].iter().map(ToString::to_string));
    }
    Ok(groups
        .into_values()
        .map(|(anchors, write_files, mut read_files)| {
            for file in &write_files {
                read_files.remove(file);
            }
            Group {
                anchors: anchors.into_iter().collect(),
                write_files: write_files.into_iter().collect(),
                read_files: read_files.into_iter().collect(),
            }
        })
        .collect())
}

/// Small, explicit graph slice for a scoped worker. All bodies remain accessible
/// through the worker's normal read tool; this is only a bounded orientation map.
pub fn scoped_slice(
    world: &World,
    ids: &[String],
    writes: &BTreeSet<PathBuf>,
    reads: &BTreeSet<PathBuf>,
    budget: usize,
) -> Result<String, String> {
    if ids.is_empty() || ids.len() > 32 {
        return Err("graph slice requires 1–32 explicit anchors".into());
    }
    let allowed: BTreeSet<PathBuf> = writes.union(reads).cloned().collect();
    let mut output = format!(
        "Loom graph slice @ {}\nwrite_files: {writes:?}\nread_files: {reads:?}\nReferences are name-resolved approximations.\n",
        world.snapshot.revision,
    );
    let unique: BTreeSet<_> = ids.iter().collect();
    let mut seen = BTreeSet::new();
    for id in &unique {
        let symbol = world
            .graph
            .get(id)
            .ok_or_else(|| format!("unknown symbol `{id}`"))?;
        if !allowed.contains(&PathBuf::from(&symbol.file)) {
            return Err(format!(
                "anchor `{id}` lies outside declared read/write files"
            ));
        }
        let signature = world.signature(id).map_err(|e| e.to_string())?;
        let line = format!(
            "anchor: {}:{}..{} {}\n",
            symbol.id, symbol.start, symbol.end, signature
        );
        if output.len() + line.len() > budget {
            return Err("graph slice budget too small for explicit anchors".into());
        }
        output.push_str(&line);
        seen.insert(symbol.id.as_str());
    }
    for id in unique {
        for dependency in world.graph.neighbors(id) {
            if !allowed.contains(&PathBuf::from(&dependency.file)) || !seen.insert(&dependency.id) {
                continue;
            }
            let signature = world.signature(&dependency.id).unwrap_or_default();
            let line = format!(
                "related: {}:{}..{} {}\n",
                dependency.id, dependency.start, dependency.end, signature
            );
            if output.len() + line.len() > budget {
                output.push_str("… graph slice limited; use map/refs for more\n");
                return Ok(output);
            }
            output.push_str(&line);
        }
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::world::{graph::GraphIndex, repository::RepositoryExplorer};

    #[test]
    fn split_groups_shared_writes_and_read_write_dependencies() {
        let temp = tempfile::tempdir().unwrap();
        assert!(
            std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(temp.path())
                .status()
                .unwrap()
                .success()
        );
        std::fs::write(
            temp.path().join("a.rs"),
            "fn a() { shared(); }\nfn same_file() {}\n",
        )
        .unwrap();
        std::fs::write(temp.path().join("b.rs"), "fn b() { shared(); }\n").unwrap();
        std::fs::write(temp.path().join("shared.rs"), "fn shared() {}\n").unwrap();
        let repo = RepositoryExplorer::new(temp.path()).unwrap();
        let graph = GraphIndex::default()
            .graph(&repo.snapshot().unwrap())
            .unwrap();
        let selected = |ids: &[&str]| {
            candidates(
                &graph,
                &ids.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            )
            .unwrap()
        };
        let independent = selected(&["a.rs::a", "b.rs::b"]);
        assert_eq!(independent.len(), 2);
        assert!(
            independent
                .iter()
                .all(|g| g.read_files.contains(&"shared.rs".into()))
        );
        let same_file = selected(&["a.rs::a", "a.rs::same_file"]);
        assert_eq!(same_file.len(), 1);
        let dependent = selected(&["a.rs::a", "b.rs::b", "shared.rs::shared"]);
        assert_eq!(dependent.len(), 1);
        assert!(candidates(&graph, &["missing::symbol".into()]).is_err());
        let world = World::open(temp.path()).unwrap();
        let writes = [PathBuf::from("a.rs")].into();
        let reads = [PathBuf::from("shared.rs")].into();
        let slice = scoped_slice(&world, &["a.rs::a".into()], &writes, &reads, 8000).unwrap();
        assert!(
            slice.contains("anchor: a.rs::a") && slice.contains("related: shared.rs::shared"),
            "{slice}"
        );
        assert!(!slice.contains("b.rs::b"), "{slice}");
        assert!(scoped_slice(&world, &["b.rs::b".into()], &writes, &reads, 8000).is_err());
    }
}
