//! Dump the Core code graph of a repository: `cargo run --example graph_dump -- <root>`.
//! One line per symbol: `<id>\t<start>\t<end>\t<resolved ids, comma-separated>`.
use loom_tui::core::{graph::GraphIndex, repository::RepositoryExplorer};
use std::path::PathBuf;

fn main() -> anyhow::Result<()> {
    let root = PathBuf::from(std::env::args().nth(1).unwrap_or_else(|| ".".into()));
    let snapshot = RepositoryExplorer::new(&root)?.snapshot()?;
    let started = std::time::Instant::now();
    let graph = GraphIndex::default().graph(&snapshot)?;
    eprintln!(
        "{} symbols, {} parse errors, {:?}",
        graph.len(),
        graph.parse_errors.len(),
        started.elapsed()
    );
    let mut files: Vec<_> = graph.parse_errors.iter().map(|(f, _)| f.clone()).collect();
    files.sort();
    for symbol in snapshot
        .sources()
        .filter_map(Result::ok)
        .map(|(p, _, _)| p.to_string_lossy().into_owned())
    {
        for s in graph.in_file(&symbol) {
            let resolved: Vec<_> = graph
                .resolve(&s.id)
                .into_iter()
                .map(|r| r.id.as_str())
                .collect();
            println!("{}\t{}\t{}\t{}", s.id, s.start, s.end, resolved.join(","));
        }
    }
    Ok(())
}
