//! Diagnose first-model-token delay without calling a provider.
use loom_core::{base::Revision, effects::Overlay, world::{GraphIndex, World}};
use std::{path::Path, sync::{Arc, atomic::AtomicBool}, time::Instant};

fn main() -> anyhow::Result<()> {
    let root = std::env::args().nth(1).unwrap_or_else(|| ".".into());
    let root = Path::new(&root);
    let cancel = AtomicBool::new(false);
    let started = Instant::now();
    let base = Revision::capture_cancellable(root, &cancel)?;
    eprintln!("revision: {:.2}s", started.elapsed().as_secs_f64());
    let after_revision = Instant::now();
    let overlay = Overlay::create_cancellable(root, &base, &cancel)?;
    eprintln!("overlay: {:.2}s", after_revision.elapsed().as_secs_f64());
    let after_overlay = Instant::now();
    let world = World::open_at_cancellable(overlay.dir(), &overlay.base, Arc::new(GraphIndex::default()), &cancel)?;
    eprintln!("index: {:.2}s ({} files, {} symbols)", after_overlay.elapsed().as_secs_f64(), world.snapshot.files.entries().count(), world.graph.len());
    eprintln!("total: {:.2}s", started.elapsed().as_secs_f64());
    Ok(())
}
