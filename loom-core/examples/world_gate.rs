//! cargo run --example world_gate -- <repository> <symgraph binary>
//! Compares every legacy item against the experiment oracle and records baseline cost.
use anyhow::{Context, Result, bail};
use loom_core::world::{SymbolKind, graph, repository::RepositoryExplorer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::{Seek, SeekFrom, Write},
    path::Path,
    process::Command,
    time::Instant,
};

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let repository = Path::new(args.get(1).context("repository argument required")?);
    let oracle = args.get(2).context("symgraph argument required")?;
    let started = Instant::now();
    let index = RepositoryExplorer::new(repository)?;
    let snapshot = index.snapshot()?;
    let _graph = loom_core::world::GraphIndex::default().graph(&snapshot)?;
    let graph_ms = started.elapsed().as_millis();
    let mut sources = BTreeMap::new();
    let mut source_bytes = 0;
    let mut actual = BTreeMap::new();
    let mut members = 0;
    for source in snapshot.sources() {
        let (path, _, text) = source?;
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        let file = path.to_string_lossy().replace('\\', "/");
        source_bytes += text.len();
        for symbol in graph::extract(&file, &text).map_err(anyhow::Error::msg)? {
            if matches!(symbol.kind, SymbolKind::Field | SymbolKind::Variant) {
                members += 1;
                continue;
            }
            let key = format!("{}:{}:{}", symbol.id, symbol.start, symbol.end);
            actual.insert(key, json!({"id": symbol.id, "name": symbol.name, "file": symbol.file,
                "start": symbol.start, "end": symbol.end, "refs": symbol.refs, "test": symbol.test}));
        }
        sources.insert(file, text.into_owned());
    }
    let mut input = tempfile::tempfile()?;
    serde_json::to_writer(&mut input, &sources)?;
    input.flush()?;
    input.seek(SeekFrom::Start(0))?;
    let output = Command::new(oracle).stdin(input).output()?;
    if !output.status.success() {
        bail!(
            "symgraph failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let expected: Value = serde_json::from_slice(&output.stdout)?;
    let mut expected_items = BTreeMap::new();
    for symbol in expected["symbols"]
        .as_array()
        .context("oracle symbols missing")?
    {
        let key = format!(
            "{}:{}:{}",
            symbol["id"].as_str().unwrap_or(""),
            symbol["start"],
            symbol["end"]
        );
        let mut symbol = symbol.clone();
        symbol.as_object_mut().unwrap().remove("kind");
        expected_items.insert(key, symbol);
    }
    if actual != expected_items {
        let different: Vec<_> = actual
            .iter()
            .filter(|(k, v)| expected_items.get(*k) != Some(*v))
            .map(|(k, _)| k)
            .take(8)
            .collect();
        bail!(
            "parity mismatch: actual {}, expected {}, first differences {:?}",
            actual.len(),
            expected_items.len(),
            different
        );
    }
    let head = Command::new("git")
        .current_dir(repository)
        .args(["rev-parse", "HEAD"])
        .output()?;
    let unique_ids: BTreeSet<_> = actual.values().map(|v| v["id"].as_str().unwrap()).collect();
    println!(
        "{}",
        json!({"repository": repository, "commit": String::from_utf8_lossy(&head.stdout).trim(),
        "legacy_ids": unique_ids.len(), "legacy_spans": actual.len(), "matched_spans": expected_items.len(), "member_items": members,
        "parity_sha256": format!("{:x}", Sha256::digest(serde_json::to_vec(&actual)?)),
        "graph_ms": graph_ms, "rust_files": sources.len(), "source_bytes": source_bytes})
    );
    Ok(())
}
