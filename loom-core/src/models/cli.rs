//! Command-line entry points for the model layer: `--models`, `--ask`, `--model-stats`.
use super::discover::discover;
use super::router::{CostClass, Need, Tier, load_ledger, stats};
use super::*;
use std::path::PathBuf;

/// `~/.loom/model-usage.jsonl`: every routed call, across projects.
pub fn ledger_path() -> Option<PathBuf> {
    super::state_dir().map(|root| root.join("model-usage.jsonl"))
}

pub fn parse_tier(text: &str) -> Option<Tier> {
    Some(match text.to_ascii_lowercase().as_str() {
        "local" => Tier::Local,
        "small" => Tier::Small,
        "medium" => Tier::Medium,
        "flagship" => Tier::Flagship,
        _ => return None,
    })
}

pub fn parse_cost(text: &str) -> Option<CostClass> {
    Some(match text.to_ascii_lowercase().as_str() {
        "local" => CostClass::Local,
        "free" => CostClass::Free,
        "subscription" => CostClass::Subscription,
        "paid" => CostClass::Paid,
        "any" => CostClass::Unknown,
        _ => return None,
    })
}

pub fn models(need: Need) {
    let (router, sources) = discover(None);
    println!("sources");
    for s in &sources {
        match &s.status {
            Ok(n) => println!("  ✓ {:<22} {n} models", s.name),
            Err(e) => println!("  · {:<22} {e}", s.name),
        }
    }
    println!(
        "\nrouting order (min tier {:?}, tools {}, max cost {:?})",
        need.min_tier, need.tools, need.max_cost
    );
    for (i, (c, reason)) in router.plan(need).into_iter().enumerate().take(40) {
        let price = c
            .price
            .map(|(i, o)| format!("${i:.2}/${o:.2} per M"))
            .unwrap_or_else(|| "price ?".into());
        println!(
            "  {:>2}. {:<12} {:<9} {:<8} {:<50} {}{}",
            i + 1,
            c.endpoint,
            format!("{:?}", c.cost),
            format!("{:?}", c.tier),
            c.model,
            price,
            reason.map(|r| format!("  [{r}]")).unwrap_or_default()
        );
    }
}

pub fn ask(prompt: &str, mut need: Need) -> anyhow::Result<()> {
    need.max_cost = need.max_cost.min(cost_ceiling());
    let (router, _) = discover(ledger_path());
    let served = router.complete(
        need,
        &Request {
            messages: vec![Message::user(prompt)],
            max_output_tokens: Some(2048),
            ..Request::default()
        },
    )?;
    for (candidate, reason) in &served.skipped {
        eprintln!("skipped {candidate}: {reason}");
    }
    let c = &served.candidate;
    let r = &served.response;
    eprintln!(
        "served by {}/{} ({:?}, {:?}) in {:.1}s · input {:?} cached {:?} output {:?} · cost {}",
        c.endpoint,
        r.model,
        c.cost,
        c.tier,
        r.latency.as_secs_f64(),
        r.usage.input,
        r.usage.cached_input,
        r.usage.output,
        c.usd(&r.usage)
            .map(|u| format!("${u:.5}"))
            .unwrap_or_else(|| "unknown".into())
    );
    println!("{}", r.text());
    Ok(())
}

pub fn print_stats() {
    let Some(path) = ledger_path() else { return };
    let records = load_ledger(&path);
    if records.is_empty() {
        println!("no calls recorded yet ({})", path.display());
        return;
    }
    println!(
        "{:<12} {:<44} {:>6} {:>6} {:>10} {:>10} {:>9} {:>10} {:>9}",
        "endpoint", "model", "calls", "fail", "input", "cached", "output", "cost", "latency"
    );
    for s in stats(&records) {
        println!(
            "{:<24} {:<44} {:>6} {:>6} {:>10} {:>10} {:>9} {:>10} {:>8}ms",
            s.endpoint,
            s.model.chars().take(44).collect::<String>(),
            s.calls,
            s.failures,
            s.input,
            s.cached_input,
            s.output,
            s.usd
                .map(|u| format!("${u:.4}"))
                .unwrap_or_else(|| "unknown".into()),
            s.mean_latency_ms
        );
    }
}

/// Prepare HF GGUF or MLX downloads as LMX. Ollama keeps its own model store.
pub fn pull(spec: &str) -> anyhow::Result<Option<PathBuf>> {
    let mut progress = |done: u64, total: u64| {
        if total > 0 {
            eprint!(
                "\r  {:>6.1}% {:>8.1} / {:.1} MB",
                done as f64 * 100.0 / total as f64,
                done as f64 / 1e6,
                total as f64 / 1e6
            );
        } else {
            eprint!("\r  {:.1} MB", done as f64 / 1e6);
        }
    };
    if spec.starts_with("mlx:") {
        let prepared = super::import::pull(spec, &mut |done, total| { progress(done, total); true })?;
        let executable = super::lmx::prepare(&prepared.path,&mut |done,total|{progress(done,total);true})?;
        eprintln!("\nprepared {} · {}", executable.id, executable.path.display());
        return Ok(Some(executable.path));
    }
    if let Some(name) = spec.strip_prefix("ollama:") {
        super::local::ollama_pull(name, &mut |status, done, total| {
            if total > 0 {
                progress(done, total)
            } else {
                eprint!("\r  {status:<40}")
            }
        })?;
        eprintln!("\npulled {name} into Ollama");
        return Ok(None);
    }
    let spec = spec.strip_prefix("hf:").unwrap_or(spec);
    let (repo, wanted) = match spec.splitn(3, '/').collect::<Vec<_>>().as_slice() {
        [owner, name, file] => (format!("{owner}/{name}"), Some((*file).to_owned())),
        [owner, name] => (format!("{owner}/{name}"), None),
        _ => anyhow::bail!("expected hf:owner/repo[/file.gguf] or ollama:name"),
    };
    let files = super::local::list_gguf(&repo)?;
    let file = match wanted {
        Some(w) => files
            .iter()
            .find(|f| f.path == w)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("{w} not in {repo}"))?,
        None => {
            let memory = super::local::total_memory().unwrap_or(8 << 30);
            super::local::recommend(&files, memory)
                .cloned()
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "no GGUF in {repo} fits {:.0} GB of memory",
                        memory as f64 / 1e9
                    )
                })?
        }
    };
    eprintln!("{repo}/{} ({:.2} GB)", file.path, file.size as f64 / 1e9);
    let path = super::local::download(&repo, &file, &mut progress)?;
    eprintln!("\nsaved {}", path.display());
    let executable = super::lmx::prepare(&path,&mut |done,total|{progress(done,total);true})?;
    eprintln!("\nprepared {} · {}",executable.id,executable.path.display());
    Ok(Some(executable.path))
}

/// Serve a GGUF (path or hf: spec) with llama-server until interrupted.
pub fn serve(spec: &str, port: u16) -> anyhow::Result<()> {
    let path = if std::path::Path::new(spec).exists() {
        PathBuf::from(spec)
    } else {
        pull(spec)?.ok_or_else(|| anyhow::anyhow!("--serve needs a GGUF path or hf: spec"))?
    };
    let server = super::local::LocalServer::start(&path, port, 16384)?;
    println!(
        "serving {} at {} (Ctrl+C to stop; log {})",
        server.model,
        server.endpoint().base_url,
        server.log.display()
    );
    loop {
        std::thread::sleep(std::time::Duration::from_secs(3600));
    }
}

pub fn print_free_tiers() {
    let records = ledger_path().map(|p| load_ledger(&p)).unwrap_or_default();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    super::freetier::print(&records, now);
}
