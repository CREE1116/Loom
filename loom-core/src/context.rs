//! L5: deterministic contracts and bounded source slices, independent of model calls.
use crate::{
    base::{Revision, SymbolId},
    world::World,
};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EffectContract {
    pub anchor: Vec<SymbolId>,
    pub write_files: Vec<PathBuf>,
    pub write_hints: Vec<SymbolId>,
    pub test_slots: Vec<PathBuf>,
    pub artifacts: Vec<String>,
    pub reads: Vec<SymbolId>,
    pub provenance: Provenance,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Provenance {
    pub derived_by: String,
    pub revision: u32,
    pub world: Revision,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Slice {
    pub text: String,
    pub symbols: Vec<SymbolId>,
    pub supplied: Vec<crate::worker::ReadSpan>,
    pub bytes: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ScopeReport {
    pub events: Vec<String>,
    pub recall: f64,
    pub revised: EffectContract,
}

pub fn derive(goal: &str, world: &World) -> Result<EffectContract> {
    let words: BTreeSet<_> = goal
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|w| w.len() > 1)
        .collect();
    let mut anchor = BTreeSet::new();
    let mut files = BTreeSet::new();
    let mut hints = BTreeSet::new();
    let mut reads = BTreeSet::new();
    let mut tests = BTreeSet::new();
    for word in words {
        for symbol in world.graph.named(word) {
            anchor.insert(SymbolId(symbol.id.clone()));
            files.insert(PathBuf::from(&symbol.file));
        }
    }
    for source in world.snapshot.files.sources() {
        let (path, _, _) = source?;
        if goal.contains(&path.to_string_lossy().to_string()) {
            files.insert(path.to_owned());
        }
    }
    for id in &anchor {
        for symbol in world.graph.resolve(&id.0) {
            reads.insert(SymbolId(symbol.id.clone()));
        }
        for symbol in world.graph.referrers(&id.0) {
            hints.insert(SymbolId(symbol.id.clone()));
            files.insert(symbol.file.clone().into());
            if symbol.test {
                tests.insert(symbol.file.clone().into());
            }
        }
    }
    Ok(EffectContract {
        anchor: anchor.into_iter().collect(),
        write_files: files.into_iter().collect(),
        write_hints: hints.into_iter().collect(),
        test_slots: tests.into_iter().collect(),
        artifacts: vec!["patch".into()],
        reads: reads.into_iter().collect(),
        provenance: Provenance {
            derived_by: "Rules".into(),
            revision: 0,
            world: world.snapshot.revision.clone(),
        },
    })
}

pub fn compile(contract: &EffectContract, world: &World, cap: usize) -> Result<Slice> {
    anyhow::ensure!(
        contract.provenance.world == world.snapshot.revision,
        "contract world moved"
    );
    let mut text = String::new();
    let mut symbols = vec![];
    let mut supplied = vec![];
    let sources: std::collections::BTreeMap<_, _> = world
        .snapshot
        .files
        .sources()
        .map(|s| s.map(|(p, _, t)| (p.to_owned(), t.into_owned())))
        .collect::<Result<_>>()?;
    for (id, body) in contract.anchor.iter().map(|id| (id, true)).chain(
        contract
            .write_hints
            .iter()
            .chain(&contract.reads)
            .map(|id| (id, false)),
    ) {
        if symbols.contains(id) {
            continue;
        }
        let Some(symbol) = world.graph.get(&id.0) else {
            continue;
        };
        let content = if body {
            sources
                .get(Path::new(&symbol.file))
                .map(|s| {
                    s.lines()
                        .skip(symbol.start.saturating_sub(1))
                        .take(symbol.lines())
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default()
        } else {
            world.signature(&id.0)?
        };
        let entry = format!(
            "{} ({}:{}..{})\n{}\n",
            id.0, symbol.file, symbol.start, symbol.end, content
        );
        if text.len() + entry.len() > cap {
            continue;
        }
        text.push_str(&entry);
        symbols.push(id.clone());
        if body {
            supplied.push(crate::worker::ReadSpan {
                symbol: id.0.clone(),
                path: symbol.file.clone().into(),
                start: symbol.start,
                end: symbol.end,
            });
        }
    }
    let bytes = text.len();
    Ok(Slice {
        text,
        symbols,
        supplied,
        bytes,
    })
}
pub fn monitor(contract: &EffectContract, touched: &BTreeSet<PathBuf>) -> ScopeReport {
    let declared: BTreeSet<_> = contract.write_files.iter().cloned().collect();
    let found = touched.intersection(&declared).count();
    let recall = if touched.is_empty() {
        1.0
    } else {
        found as f64 / touched.len() as f64
    };
    let additions: Vec<_> = touched.difference(&declared).cloned().collect();
    let events = additions
        .iter()
        .map(|p| format!("observed write beyond derived contract: {}", p.display()))
        .collect();
    let mut revised = contract.clone();
    revised.write_files.extend(additions.clone());
    revised.write_files.sort();
    revised.write_files.dedup();
    if !additions.is_empty() {
        revised.provenance.revision += 1;
    }
    ScopeReport {
        events,
        recall,
        revised,
    }
}
