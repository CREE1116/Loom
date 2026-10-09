//! The experiment corpus is optional; its oracle digest is pinned in the repository.
use loom_core::world::{SymbolKind, graph, repository::RepositoryExplorer};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
    process::Command,
};

#[test]
#[ignore = "requires the pinned ripgrep experiment corpus; set LOOM_PARITY_REPO if stored elsewhere"]
fn ripgrep_4827_symbols_match_the_frozen_symgraph_oracle() {
    let repository = std::env::var_os("LOOM_PARITY_REPO")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../experiments/effect-contract/.work/repos/ripgrep")
        });
    let fixture: Value =
        serde_json::from_str(include_str!("fixtures/ripgrep-parity.json")).unwrap();
    let head = Command::new("git")
        .current_dir(&repository)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    assert!(head.status.success());
    assert_eq!(
        String::from_utf8(head.stdout).unwrap().trim(),
        fixture["commit"].as_str().unwrap()
    );
    let index = RepositoryExplorer::new(&repository).unwrap();
    let snapshot = index.snapshot().unwrap();
    let mut items = BTreeMap::new();
    let mut ids = BTreeSet::new();
    for source in snapshot.sources() {
        let (path, _, text) = source.unwrap();
        if path.extension().is_none_or(|e| e != "rs") {
            continue;
        }
        for symbol in graph::extract(&path.to_string_lossy().replace('\\', "/"), &text).unwrap() {
            if matches!(symbol.kind, SymbolKind::Field | SymbolKind::Variant) {
                continue;
            }
            ids.insert(symbol.id.clone());
            items.insert(format!("{}:{}:{}", symbol.id, symbol.start, symbol.end),
                json!({"id":symbol.id,"name":symbol.name,"file":symbol.file,"start":symbol.start,"end":symbol.end,"refs":symbol.refs,"test":symbol.test}));
        }
    }
    assert_eq!(ids.len() as u64, fixture["legacy_ids"].as_u64().unwrap());
    assert_eq!(
        items.len() as u64,
        fixture["legacy_spans"].as_u64().unwrap()
    );
    let actual = format!("{:x}", Sha256::digest(serde_json::to_vec(&items).unwrap()));
    assert_eq!(actual, fixture["parity_sha256"].as_str().unwrap());
}
