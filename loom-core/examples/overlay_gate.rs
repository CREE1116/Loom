//! Measure worktree creation and cold/warm builds in two independent overlays.
//! cargo run --example overlay_gate -- <Rust repository> [validation command]
use anyhow::{Context, Result, ensure};
use loom_core::{
    base::Revision,
    effects::{Overlay, RunStatus, Runner},
};
use serde_json::json;
use std::{
    path::Path,
    sync::atomic::AtomicBool,
    time::{Duration, Instant},
};

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let root = Path::new(args.get(1).context("repository argument required")?);
    let command = args
        .get(2)
        .map(String::as_str)
        .unwrap_or("cargo build --locked --quiet");
    let base = Revision::capture(root)?;
    let mut creation = Vec::new();
    let mut builds = Vec::new();
    for _ in 0..2 {
        let start = Instant::now();
        let overlay = Overlay::create(root, &base)?;
        creation.push(start.elapsed().as_millis() as u64);
        let result = Runner::new(overlay.artifacts().clone()).run(
            overlay.dir(),
            command,
            Duration::from_secs(300),
            &AtomicBool::new(false),
            Some(&overlay.target_dir()),
        )?;
        ensure!(
            result.status == RunStatus::Passed,
            "overlay build failed; artifact {}",
            result.artifact.id
        );
        builds.push(result.ms);
    }
    let ratio = builds[1] as f64 / builds[0].max(1) as f64;
    println!(
        "{}",
        json!({"revision":base, "creation_ms":creation, "build_ms":builds,
        "warm_ratio":ratio, "gate_passed":ratio <= 0.20})
    );
    ensure!(
        ratio <= 0.20,
        "second overlay build exceeds 20% of the first"
    );
    Ok(())
}
