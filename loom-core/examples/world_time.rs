//! Time the per-request setup on a repository: `cargo run --release --example world_time -- PATH`.
fn main() -> anyhow::Result<()> {
    let root = std::path::PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    let t = std::time::Instant::now();
    let revision = loom_core::base::Revision::capture(&root)?;
    println!("revision capture   {:>6} ms", t.elapsed().as_millis());
    let t = std::time::Instant::now();
    let world = loom_core::world::World::open(&root)?;
    println!("world open         {:>6} ms ({} symbols)", t.elapsed().as_millis(), world.graph.len());
    let t = std::time::Instant::now();
    let overlay = loom_core::effects::Overlay::create(&root, &revision)?;
    println!("overlay create     {:>6} ms", t.elapsed().as_millis());
    drop(overlay);
    Ok(())
}
