//! Product-level command dispatch shared by the installed `loom` binary.
//!
//! Interactive TUI flags keep their existing parser. Stable non-interactive commands
//! are intercepted here so the release no longer needs the repository shell wrapper to
//! choose between the TUI and Core binaries.
use anyhow::{Context, Result, bail};
use std::{ffi::OsString, io::Read, path::Path};

const CORE_COMMANDS: &[&str] = &[
    "run", "resume", "approve", "status", "session", "goal", "models",
];

pub fn dispatch(args: Vec<OsString>) -> Result<Option<i32>> {
    let Some(command) = args.get(1).and_then(|arg| arg.to_str()) else {
        return Ok(None);
    };
    if CORE_COMMANDS.contains(&command) {
        return loom_core::surfaces::cli_from(args).map(Some);
    }
    match command {
        "provider" => loom_core::models::provider_cli::execute(args[2..].iter().map(|arg| arg.to_string_lossy().into_owned())).map(|()| Some(0)),
        "runtime" => runtime(&args[2..]).map(|()| Some(0)),
        "model" => model(&args[2..]).map(|()| Some(0)),
        _ => Ok(None),
    }
}

fn arg(args: &[OsString], index: usize, usage: &str) -> Result<String> {
    args.get(index)
        .and_then(|value| value.to_str())
        .map(ToOwned::to_owned)
        .with_context(|| format!("usage: {usage}"))
}

fn runtime(args: &[OsString]) -> Result<()> {
    match args.first().and_then(|arg| arg.to_str()).unwrap_or("status") {
        "status" => {
            let backend = loom_core::models::native_runtime::Backend::configured()?;
            println!("Loom in-process engine · {}", backend.label());
            let status = loom_core::models::runtime::status();
            match &status.selected {
                Some(server) => println!("Optional compatibility server · {}", server.display()),
                None => println!("Optional compatibility server · not installed"),
            }
            if let Some(managed) = status.managed {
                println!("  managed · {}", managed.display());
            }
            if let Some(external) = status.external {
                println!("  external · {}", external.display());
            }
            Ok(())
        }
        "backend" => {
            let mode = arg(args, 1, "loom runtime backend auto|cpu|mps|cuda")?;
            loom_core::models::registry::Registry::open()?.set_inference_backend(&mode)?;
            println!("Saved inference backend: {mode}"); Ok(())
        }
        "install" => {
            let force = args.iter().skip(1).any(|arg| arg == "--force");
            let mut last_percent = u64::MAX;
            let installed = loom_core::models::runtime::install(force, &mut |done, total| {
                if total == 0 {
                    eprint!("\rLOOM runtime · {:.1} MB", done as f64 / 1e6);
                    return;
                }
                let percent = done.saturating_mul(100) / total.max(1);
                if percent != last_percent {
                    last_percent = percent;
                    eprint!(
                        "\rLOOM runtime · {:>3}% · {:.1}/{:.1} MB",
                        percent,
                        done as f64 / 1e6,
                        total as f64 / 1e6
                    );
                }
            })?;
            eprintln!();
            println!(
                "installed {} · {}\n{}",
                installed.release,
                installed.asset,
                installed.server.display()
            );
            Ok(())
        }
        other => bail!("unknown runtime command `{other}`; use `loom runtime status|backend|install`"),
    }
}

fn model(args: &[OsString]) -> Result<()> {
    match args.first().and_then(|arg| arg.to_str()).unwrap_or("list") {
        "list" => {
            let models = loom_core::models::checkpoints::installed()?;
            let prepared = loom_core::models::import::installed()?;
            let executables = loom_core::models::lmx::installed()?;
            if models.is_empty() && prepared.is_empty() && executables.is_empty() {
                println!(
                    "no LOOM-managed models in {}",
                    loom_core::models::local::models_dir().display()
                );
            } else {
                for model in models {
                    println!("{:.2} GB · {}", model.bytes as f64 / 1e9, model.id);
                }
                for model in prepared {
                    println!("{:.2} GB · mlx/{}", model.bytes as f64 / 1e9, model.id);
                }
                for model in executables {
                    println!("{:.2} GB · {} ({})", model.bytes as f64 / 1e9, model.display_name(), model.id());
                }
            }
            Ok(())
        }
        "pull" => {
            let spec = arg(args, 1, "loom model pull <hf:owner/repo[/file.gguf]|mlx:owner/repo[@revision]>")?;
            loom_core::models::cli::pull(&spec)?;
            Ok(())
        }
        "import" => {
            if args.iter().skip(2).any(|arg| arg == "--native") {
                return model_import_native(args);
            }
            let source = arg(args, 1, "loom model import <GGUF file|MLX directory|LMX file> [--json]")?;
            if args.len() > 3 || args.get(2).is_some_and(|arg| arg != "--json") {
                bail!("usage: loom model import <GGUF file|MLX directory|LMX file> [--json]");
            }
            let prepared = loom_core::models::lmx::prepare(Path::new(&source), &mut |_, _| true)?;
            if args.get(2).is_some() {
                println!("{}", serde_json::to_string_pretty(&prepared)?);
            } else {
                println!("Prepared {} · {:.2} GB\n{}", prepared.id, prepared.bytes as f64 / 1e9, prepared.path.display());
                for engine in prepared.engines {
                    println!("  {:?} · {}",engine.engine,engine.reason.as_deref().unwrap_or("available"));
                }
            }
            Ok(())
        }
        "inspect" => {
            let id=arg(args,1,"loom model inspect <path.lmx|lmx/ID> [--verify] [--json]")?;
            if args.iter().skip(2).any(|arg|arg!="--verify"&&arg!="--json"){bail!("unknown model inspect option")}
            let verify=args.iter().any(|arg|arg=="--verify");
            let path = Path::new(&id);
            if path.is_file() {
                let mut magic = [0u8; 8];
                let mut file = std::fs::File::open(path)?;
                match file.read_exact(&mut magic) {
                    Ok(()) if &magic == b"LOOMLM2\0" => {
                        // Once native magic matches, malformed native metadata must
                        // fail here rather than being reinterpreted as a v1 payload.
                        let native = loom_core::models::lmx_native_file::NativeFile::open(path)?;
                        if verify { native.verify(&mut |_, _| true)?; }
                        return report_native(path, &native, verify, args.iter().any(|arg| arg == "--json"));
                    }
                    Ok(()) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => {}
                    Err(error) => return Err(error.into()),
                }
            }
            let executable=loom_core::models::lmx::by_id(&id)?;
            if verify{executable.verify(&mut |_,_|true)?;}
            let engines=loom_core::models::lmx_runtime::statuses(&executable);
            if args.iter().any(|arg|arg=="--json"){
                let ir=&executable.ir;
                println!("{}",serde_json::to_string_pretty(&serde_json::json!({"id":executable.id(),"path":executable.path,"format":"lmx","version":1,"bytes":executable.bytes,"profile":executable.manifest.profile,"payload_verified":verify,"engines":engines,
                    "ir":{"version":ir.version,"format":ir.format,"architecture":ir.architecture,"tensors":ir.tensors.len(),"storages":ir.storages(),"weight_bytes":ir.weight_bytes()}}))?);
            }else{
                let ir=&executable.ir;
                println!("{} · LMX v1 · {:.2} GB · payload hashes {}",executable.id(),executable.bytes as f64/1e9,if verify{"verified"}else{"not checked"});
                println!("  model · {} · {:?} · {} tensors · {:.2} GB weights · {}",ir.architecture,ir.format,ir.tensors.len(),ir.weight_bytes() as f64/1e9,ir.storages().into_iter().collect::<Vec<_>>().join(", "));
                for engine in engines{println!("  {:?} · {}",engine.engine,engine.reason.as_deref().unwrap_or("available"));}
            }
            Ok(())
        }
        "remove" => {
            let id = arg(args, 1, "loom model remove <ID from loom model list>")?;
            loom_core::models::checkpoints::remove(&id)?;
            println!("Removed {id}"); Ok(())
        }
        "serve" => {
            let spec = arg(args, 1, "loom model serve <model.gguf|hf:owner/repo> [--port N]")?;
            let mut port = 8080u16;
            let mut index = 2;
            while index < args.len() {
                let option = args[index].to_str().context("model serve option is not UTF-8")?;
                match option {
                    "--port" => {
                        port = arg(args, index + 1, "loom model serve <spec> --port <N>")?
                            .parse()
                            .context("--port must be an integer")?;
                        index += 2;
                    }
                    other => bail!("unknown model serve option `{other}`"),
                }
            }
            if !Path::new(&spec).exists() && !spec.contains('/') {
                bail!("model serve expects a GGUF path or Hugging Face owner/repo spec");
            }
            loom_core::models::cli::serve(&spec, port)
        }
        "bench" => model_bench(args),
        other => bail!("unknown model command `{other}`; use `loom model list|pull|import|inspect|remove|serve|bench`"),
    }
}

fn model_import_native(args: &[OsString]) -> Result<()> {
    let usage = "loom model import <MLX directory|GGUF file> --native --output <new.lmx> [--json]";
    let source = arg(args, 1, usage)?;
    let (mut native, mut output, mut json) = (false, None, false);
    let mut index = 2;
    while index < args.len() {
        match args[index].to_str() {
            Some("--native") if !native => native = true,
            Some("--json") if !json => json = true,
            Some("--output") if output.is_none() => {
                let value = args.get(index + 1).with_context(|| format!("usage: {usage}"))?;
                if value.to_string_lossy().starts_with("--") { bail!("usage: {usage}"); }
                output = Some(Path::new(value));
                index += 1;
            }
            _ => bail!("usage: {usage}"),
        }
        index += 1;
    }
    let output = output.with_context(|| format!("usage: {usage}"))?;
    let source_path = Path::new(&source);
    let native = if source_path.is_file() && source_path.extension().is_some_and(|ext| ext == "gguf") {
        loom_core::models::lmx_native_gguf::import_gguf(
            source_path, output, &mut |_, _| true,
        )?
    } else {
        loom_core::models::lmx_native_import::import_mlx(
            source_path, output, &mut |_, _| true,
        )?
    };
    report_native(output, &native, false, json)
}

fn report_native(
    path: &Path,
    native: &loom_core::models::lmx_native_file::NativeFile,
    verified: bool,
    json: bool,
) -> Result<()> {
    use loom_core::models::lmx_native::{BlockKind, FeedForward};
    let manifest = &native.manifest;
    let expert_blocks = manifest.blocks.values()
        .filter(|block| matches!(block.kind, BlockKind::Expert { .. })).count();
    let (experts, active_experts) = match manifest.model.feed_forward {
        FeedForward::Dense { .. } => (0, 0),
        FeedForward::Moe { experts, active, .. } => (experts, active),
    };
    let reason = "native runtime hookup pending; this file is not installed, discovered, or executable by the current runtime";
    if json {
        println!("{}", serde_json::to_string_pretty(&serde_json::json!({
            "path": path, "format": "lmx", "version": manifest.version, "native": true,
            "identity": native.identity, "bytes": native.bytes, "model": manifest.model,
            "tensors": manifest.tensors.len(), "expert_blocks": expert_blocks,
            "experts": experts, "active_experts": active_experts,
            "payload_verified": verified, "runtime_ready": false, "runtime_reason": reason
        }))?);
    } else {
        println!("{} · native LMX v{} · {} bytes\n{}", native.identity, manifest.version, native.bytes, path.display());
        println!("  model · {:?} · {} layers · {} tensors · {} expert blocks · {} experts ({} active)",
            manifest.model.architecture, manifest.model.layers, manifest.tensors.len(), expert_blocks, experts, active_experts);
        println!("  payload hashes · {}", if verified { "verified" } else { "not checked" });
        println!("  runtime_ready: false · {reason}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::dispatch;
    use std::ffi::OsString;

    #[test]
    fn unrelated_flags_stay_on_the_interactive_parser() {
        assert_eq!(
            dispatch(vec![OsString::from("loom"), OsString::from("--demo")]).unwrap(),
            None
        );
    }
}

/// `loom model bench <id> [--kv f16|q8_0|q4_0] [--flash auto|on|off] [--ctx N] [--prompt N] [--gen N] [--json]`
fn model_bench(args: &[OsString]) -> Result<()> {
    use loom_core::models::native_runtime::{ContextOptions, KvPrecision};
    let usage = "loom model bench <ID|mlx/ID|lmx/ID> [--engine auto|cpu|mlx|cuda] [--kv f16|q8_0|q4_0] [--flash auto|on|off] [--ctx N] [--prompt N] [--gen N] [--json]";
    let id = arg(args, 1, usage)?;
    let mut options = ContextOptions::default();
    let (mut context, mut prompt, mut generate, mut json) = (8192u32, 2048usize, 128usize, false);
    let mut engine=None;
    let mut index = 2;
    while index < args.len() {
        let option = args[index].to_str().context("model bench option is not UTF-8")?;
        if option == "--json" {
            json = true;
            index += 1;
            continue;
        }
        let value = arg(args, index + 1, usage)?;
        match option {
            "--engine" => engine=Some(loom_core::models::lmx_runtime::Engine::parse(&value).context("--engine must be auto, cpu, mlx or cuda")?),
            "--kv" => options.kv = KvPrecision::parse(&value).context("--kv must be f16, q8_0 or q4_0")?,
            "--flash" => options.flash = match value.as_str() {
                "on" => Some(true),
                "off" => Some(false),
                "auto" => None,
                _ => bail!("--flash must be auto, on or off"),
            },
            "--ctx" => context = value.parse().context("--ctx must be an integer")?,
            "--prompt" => prompt = value.parse().context("--prompt must be an integer")?,
            "--gen" => generate = value.parse().context("--gen must be an integer")?,
            other => bail!("unknown model bench option `{other}`; {usage}"),
        }
        index += 2;
    }
    let report = if id.starts_with("lmx/") || loom_core::models::lmx::by_id(&id).is_ok() {
        let engine = match engine {
            Some(engine) => engine,
            None => loom_core::models::lmx_runtime::Engine::configured()?,
        };
        loom_core::models::lmx_runtime::bench(&id, engine, options, context, prompt, generate)?
    } else {
        if engine.is_some(){bail!("--engine is for LMX models; import this model first")}
        loom_core::models::native_runtime::bench(&id, options, context, prompt, generate)?
    };
    if json {
        println!("{}", serde_json::to_string(&report)?);
        return Ok(());
    }
    let gb = |bytes: u64| bytes as f64 / 1e9;
    println!("{} · {} · kv {} · flash {}", report.model, report.backend, report.kv,
        report.flash.map_or("auto", |on| if on { "on" } else { "off" }));
    println!("  weights file      {:>8.2} GB", gb(report.weights_bytes));
    println!("  load              {:>8} ms   (+{:.2} GB)", report.load_ms, gb(report.weights_memory));
    println!("  context {:>6} tok         (+{:.2} GB)", report.context_tokens, gb(report.context_memory));
    println!("  prompt  {:>6} tok {:>8.1} tok/s", report.prompt_tokens, report.prefill_per_second());
    println!("  generate {:>5} tok {:>8.1} tok/s", report.generated, report.decode_per_second());
    println!("  peak footprint    {:>8.2} GB", gb(report.peak_footprint));
    Ok(())
}
