use loom_core::{context, world::World};
use std::{collections::BTreeSet, fs, process::Command};
#[test]
fn contracts_are_deterministic_slices_are_bounded_and_scope_expansion_is_explicit() {
    let d = tempfile::tempdir().unwrap();
    Command::new("git")
        .current_dir(d.path())
        .args(["init", "-q"])
        .status()
        .unwrap();
    fs::write(
        d.path().join("lib.rs"),
        "pub fn value() -> u32 { 1 }\npub fn caller() -> u32 { value() }\n",
    )
    .unwrap();
    let world = World::open(d.path()).unwrap();
    let contract = context::derive("fix value", &world).unwrap();
    assert_eq!(
        serde_json::to_value(&contract).unwrap(),
        serde_json::to_value(context::derive("fix value", &world).unwrap()).unwrap()
    );
    let slice = context::compile(&contract, &world, 4096).unwrap();
    assert!(slice.text.contains("{ 1 }"));
    assert!(slice.bytes <= 4096);
    assert!(
        context::compile(&contract, &world, 0)
            .unwrap()
            .text
            .is_empty()
    );
    let report = context::monitor(
        &contract,
        &BTreeSet::from(["lib.rs".into(), "new.rs".into()]),
    );
    assert_eq!(report.recall, 0.5);
    assert_eq!(report.events.len(), 1);
    assert_eq!(report.revised.provenance.revision, 1);
}
