//! One LMX entry point for device selection, chat, streaming and benchmarks.
//! Kernel implementations remain in the existing MLX and llama.cpp executors.
use super::{
    lmx::{self, LmxFile},
    lmx_ir::{ModelIr, SourceFormat, Storage},
    native_runtime::{self, ContextOptions},
    *,
};
use anyhow::{Result, bail};

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Engine {
    Auto,
    Cpu,
    Mlx,
    Cuda,
}

impl Engine {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "auto" => Self::Auto,
            "cpu" => Self::Cpu,
            "mlx" | "metal" | "mps" => Self::Mlx,
            "cuda" => Self::Cuda,
            _ => return None,
        })
    }
    pub fn configured() -> Result<Self> {
        match std::env::var("LOOM_LMX_ENGINE") {
            Ok(value) => Self::parse(&value).ok_or_else(|| {
                anyhow::anyhow!("unknown LOOM_LMX_ENGINE; use auto, cpu, mlx or cuda")
            }),
            Err(_) => Ok(Self::Auto),
        }
    }
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct EngineStatus {
    pub engine: Engine,
    pub payload_supported: bool,
    pub compiled: bool,
    pub available: bool,
    /// Compilation, payload and actual device availability are checked separately.
    pub reason: Option<String>,
}

/// Architectures the MLX engine implements (`native/mlx/engine.cpp`).
const MLX_ARCHITECTURES: &[&str] = &["qwen2", "qwen3", "qwen3_moe", "llama", "mistral"];

/// Why `engine` cannot run this model, from the model itself: its payload layout, its
/// architecture and every tensor storage. `None` means the model's needs are met; whether
/// the engine is compiled into this build is reported separately.
pub fn capability(ir: &ModelIr, engine: Engine) -> Option<String> {
    let storages = ir.storages();
    match engine {
        // llama.cpp reads the GGUF file it was given; it validates the architecture and
        // every ggml type itself when the model loads.
        Engine::Cpu | Engine::Cuda => (ir.format != SourceFormat::Gguf).then(|| {
            format!(
                "{} runs GGUF payloads; this model's weights are {}",
                if engine == Engine::Cpu { "CPU" } else { "CUDA" },
                storages.iter().cloned().collect::<Vec<_>>().join(", ")
            )
        }),
        // GGUF blocks with 32-value groups are repacked as MLX affine groups while loading
        // (`gguf_mlx`); the file keeps its GGUF payload for llama.cpp.
        Engine::Mlx if ir.format == SourceFormat::Gguf => {
            let unsupported: Vec<_> = ir
                .tensors
                .iter()
                .filter_map(|(name, tensor)| match &tensor.storage {
                    // One line per tensor kind, not per layer.
                    _ if super::gguf_mlx::hf_name(name).is_none() => Some(format!(
                        "{} has no MLX name",
                        name.strip_prefix("blk.")
                            .and_then(|rest| rest.split_once('.'))
                            .map_or_else(|| name.clone(), |(_, part)| format!("blk.*.{part}"))
                    )),
                    Storage::GgmlBlock { type_name, .. } => super::gguf_mlx::unsupported(type_name),
                    Storage::Dense { dtype } => super::gguf_mlx::unsupported(dtype),
                    other => Some(format!("{} has no MLX mapping", other.label())),
                })
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect();
            if !unsupported.is_empty() {
                let shown = unsupported.iter().take(6).cloned().collect::<Vec<_>>().join("; ");
                let more = unsupported.len().saturating_sub(6);
                return Some(format!(
                    "MLX cannot run this GGUF: {shown}{}",
                    if more > 0 { format!("; {more} more") } else { String::new() }
                ));
            }
            (!super::gguf_mlx::ARCHITECTURES.contains(&ir.architecture.as_str()))
                .then(|| format!("MLX does not run GGUF architecture {} yet", ir.architecture))
        }
        Engine::Mlx => {
            if !MLX_ARCHITECTURES.contains(&ir.architecture.as_str()) {
                return Some(format!("MLX engine does not implement architecture {}", ir.architecture));
            }
            let unsupported: Vec<_> = storages
                .iter()
                .filter(|s| !matches!(s.as_str(), "dense:F16" | "dense:BF16" | "dense:F32" | "dense:U32") && !s.starts_with("mlx_affine:"))
                .cloned()
                .collect();
            (!unsupported.is_empty()).then(|| format!("MLX engine has no kernel for {}", unsupported.join(", ")))
        }
        Engine::Auto => Some("auto is resolved to a concrete engine first".into()),
    }
}

pub fn statuses(executable: &LmxFile) -> Vec<EngineStatus> {
    [Engine::Cpu, Engine::Mlx, Engine::Cuda]
        .into_iter()
        .map(|engine| {
            let unmet = capability(&executable.ir, engine);
            let payload_supported = unmet.is_none();
            let compiled = match engine {
                Engine::Cpu => true,
                Engine::Mlx => cfg!(loom_mlx),
                Engine::Cuda => cfg!(feature = "cuda"),
                Engine::Auto => false,
            };
            let available = compiled && match engine {
                Engine::Cpu => true,
                Engine::Mlx => {
                    #[cfg(loom_mlx)]
                    { super::mlx_runtime::gpu_available() }
                    #[cfg(not(loom_mlx))]
                    { false }
                }
                Engine::Cuda => native_runtime::cuda_available(),
                Engine::Auto => false,
            };
            let reason = if let Some(unmet) = unmet {
                Some(unmet)
            } else if !compiled {
                Some(
                    match engine {
                        Engine::Mlx => "MLX is absent from this build",
                        _ => "build with the cuda feature on a CUDA host",
                    }
                    .into(),
                )
            } else if !available {
                Some(format!("{engine:?} device or driver is unavailable"))
            } else {
                None
            };
            EngineStatus {
                engine,
                payload_supported,
                compiled,
                available,
                reason,
            }
        })
        .collect()
}

fn select(executable: &LmxFile, requested: Engine) -> Result<Engine> {
    select_statuses(&statuses(executable), requested)
}

fn select_statuses(statuses: &[EngineStatus], requested: Engine) -> Result<Engine> {
    let ready = |engine: Engine| {
        statuses
            .iter()
            .find(|s| s.engine == engine)
            .is_some_and(|s| s.payload_supported && s.compiled && s.available)
    };
    let engine = if requested == Engine::Auto {
        // Prefer Loom's MLX/Metal executor, then available CUDA, then the CPU baseline.
        [Engine::Mlx, Engine::Cuda, Engine::Cpu]
            .into_iter()
            .find(|&engine| ready(engine))
            // Nothing is ready: explain through the engine that matches the model.
            .or_else(|| statuses.iter().find(|s| s.payload_supported).map(|s| s.engine))
            .unwrap_or(Engine::Cpu)
    } else {
        requested
    };
    let status = statuses.iter().find(|s| s.engine == engine).unwrap();
    if !status.payload_supported || !status.compiled || !status.available {
        bail!("LMX {engine:?}: {}", status.reason.clone().unwrap_or_default())
    }
    Ok(engine)
}

fn bad(error: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::BadRequest, error.to_string())
}

enum Executor {
    Native(native_runtime::NativeProvider),
    #[cfg(loom_mlx)]
    Mlx(super::mlx_runtime::MlxProvider),
}

fn executor(executable: &LmxFile, requested: Engine) -> Result<Executor> {
    match select(executable, requested)? {
        Engine::Cpu | Engine::Cuda => {
            let engine = select(executable, requested)?;
            let info = native_runtime::ModelInfo::named(&executable.path, executable.id())
                .ok_or_else(|| anyhow::anyhow!("LMX GGUF metadata is unsupported"))?;
            Ok(Executor::Native(
                native_runtime::NativeProvider::from_models(
                    if engine == Engine::Cuda {
                        native_runtime::Backend::Cuda
                    } else if requested == Engine::Cpu {
                        native_runtime::Backend::Cpu
                    } else {
                        // The GPU (Metal) when present; layers that do not fit its working
                        // set stay on the CPU (`fitted_params`).
                        native_runtime::Backend::Auto
                    },
                    vec![info],
                ),
            ))
        }
        #[cfg(loom_mlx)]
        Engine::Mlx => {
            let model = super::mlx_runtime::MlxModel::from_lmx(executable)?;
            Ok(Executor::Mlx(super::mlx_runtime::MlxProvider::from_models(
                vec![model],
            )))
        }
        _ => bail!("LMX engine is unavailable"),
    }
}

pub fn bench(
    id: &str,
    engine: Engine,
    options: ContextOptions,
    context: u32,
    prompt: usize,
    generate: usize,
) -> Result<native_runtime::BenchReport, ProviderError> {
    let executable = lmx::by_id(id).map_err(bad)?;
    let lmx_id = executable.id();
    match executor(&executable, engine).map_err(bad)? {
        Executor::Native(provider) => provider.bench(&lmx_id, options, context, prompt, generate),
        #[cfg(loom_mlx)]
        Executor::Mlx(provider) => provider.bench(&lmx_id, options.kv, prompt, generate),
    }
}

/// Source files of installed LMX models (v1 and native), canonicalized.
pub fn converted_sources() -> std::collections::HashSet<std::path::PathBuf> {
    let canonical = |s: &str| std::fs::canonicalize(s).unwrap_or_else(|_| s.into());
    lmx::installed()
        .unwrap_or_default()
        .iter()
        .map(|m| canonical(&m.manifest.source))
        .chain(super::lmx_native_runtime::installed().iter().map(|m| canonical(&m.manifest.provenance.source)))
        .collect()
}

pub struct LmxProvider {
    pub(crate) models: Vec<LmxFile>,
    /// Native v2 files, run by Loom's own engine.
    pub(crate) native: Vec<super::lmx_native_runtime::NativeModel>,
    engine: Engine,
}
impl LmxProvider {
    pub fn discover() -> Result<Self> {
        Ok(Self {
            models: lmx::installed()?,
            native: super::lmx_native_runtime::installed(),
            engine: Engine::configured()?,
        })
    }
    /// Router names of the native v2 models.
    pub fn native_names(&self) -> Vec<String> {
        self.native.iter().map(|m| m.display_name()).collect()
    }
    fn native_model(&self, name: &str) -> Option<&super::lmx_native_runtime::NativeModel> {
        self.native.iter().find(|m| m.matches(name))
    }
    pub fn available_ids(&self) -> Vec<String> {
        self.models
            .iter()
            .filter(|model| select(model, self.engine).is_ok())
            .map(LmxFile::id)
            .collect()
    }
    /// Runnable v1 files, except those whose source also has a native (v2) file: the list
    /// shows one entry per model, and the native file is the one Loom runs.
    pub fn available_models(&self) -> Vec<&LmxFile> {
        self.models
            .iter()
            .filter(|model| select(model, self.engine).is_ok())
            .filter(|model| !self.native.iter().any(|n| n.manifest.provenance.source == model.manifest.source))
            .collect()
    }
    /// The engine for the requested model, and the request renamed to the id that engine
    /// registered (callers may name an LMX by its display name).
    fn executor(&self, request: &Request) -> Result<(Executor, Request), ProviderError> {
        request.check_cancelled()?;
        let model = self
            .models
            .iter()
            .find(|model| {
                model.id() == request.model
                    || model.identity == request.model
                    || model.display_name() == request.model
                    || request.model.strip_prefix("lmx/").is_some_and(|rest| {
                        rest == model.display_name()
                            || rest == model.identity
                            || model.identity.starts_with(rest)
                    })
                    || model.identity.starts_with(&request.model)
            })
            .ok_or_else(|| bad("LMX model is not installed"))?;
        // Reopen the bounded index to detect replacement/corruption since discovery.
        let current = LmxFile::open(&model.path).map_err(bad)?;
        if current.identity != model.identity {
            return Err(bad("LMX model changed since discovery"));
        }
        let engine = executor(&current, self.engine).map_err(bad)?;
        Ok((engine, Request { model: current.id(), ..request.clone() }))
    }
}
impl Provider for LmxProvider {
    fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        Ok(self.available_models().into_iter().map(LmxFile::display_name).chain(self.native_names()).collect())
    }
    fn name(&self) -> &str {
        "lmx"
    }
    fn reasoning_profile(&self, _model: &str) -> ReasoningProfile {
        ReasoningProfile {
            supported_efforts: vec!["none".into(), "on".into()],
            default_effort: Some("none".into()),
        }
    }
    fn complete(&self, request: &Request) -> Result<Response, ProviderError> {
        if let Some(model) = self.native_model(&request.model) {
            return super::lmx_native_runtime::infer(model, request, &mut |_| {});
        }
        let (engine, request) = self.executor(request)?;
        match engine {
            Executor::Native(provider) => provider.complete(&request),
            #[cfg(loom_mlx)]
            Executor::Mlx(provider) => provider.complete(&request),
        }
    }
    fn complete_stream(
        &self,
        request: &Request,
        observe: &mut dyn FnMut(StreamEvent),
    ) -> Result<Response, ProviderError> {
        if let Some(model) = self.native_model(&request.model) {
            return super::lmx_native_runtime::infer(model, request, observe);
        }
        let (engine, request) = self.executor(request)?;
        match engine {
            Executor::Native(provider) => provider.complete_stream(&request, observe),
            #[cfg(loom_mlx)]
            Executor::Mlx(provider) => provider.complete_stream(&request, observe),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::lmx_ir::Tensor;

    fn ir(format: SourceFormat, architecture: &str, storages: &[Storage]) -> ModelIr {
        ModelIr {
            version: super::super::lmx_ir::IR_VERSION,
            format,
            architecture: architecture.into(),
            tensors: storages
                .iter()
                .enumerate()
                .map(|(index, storage)| {
                    (format!("blk.{index}.ffn_up.weight"), Tensor { entry: "e".into(), offset: 0, bytes: 0, shape: vec![1], storage: storage.clone() })
                })
                .collect(),
        }
    }

    #[test]
    fn engines_are_chosen_by_what_the_model_needs_not_by_file_type() {
        let dense = Storage::Dense { dtype: "BF16".into() };
        let q6k = Storage::GgmlBlock { type_name: "q6_K".into(), block_size: 256, type_size: 210 };
        let affine = Storage::MlxAffine { bits: 4, group_size: 64, axis: -1, scales: "s".into(), biases: None };
        let gguf = ir(SourceFormat::Gguf, "qwen3", &[q6k.clone(), dense.clone()]);
        assert_eq!(capability(&gguf, Engine::Cpu), None);
        let mlx_reason = capability(&gguf, Engine::Mlx).unwrap();
        assert!(mlx_reason.contains("q6_K uses 16-value groups"), "{mlx_reason}");
        // 32-value GGUF groups run on MLX, for the architectures mapped without reordering.
        let q8 = Storage::GgmlBlock { type_name: "q8_0".into(), block_size: 32, type_size: 34 };
        let q4k = Storage::GgmlBlock { type_name: "q4_K".into(), block_size: 256, type_size: 144 };
        let f32 = Storage::Dense { dtype: "F32".into() };
        assert_eq!(capability(&ir(SourceFormat::Gguf, "qwen2", &[q8.clone(), q4k, f32]), Engine::Mlx), None);
        assert!(capability(&ir(SourceFormat::Gguf, "llama", &[q8]), Engine::Mlx).unwrap().contains("llama"));
        let mlx = ir(SourceFormat::Safetensors, "qwen3_moe", &[affine.clone(), dense.clone()]);
        assert_eq!(capability(&mlx, Engine::Mlx), None);
        assert!(capability(&mlx, Engine::Cpu).unwrap().contains("mlx_affine:4"));
        // An architecture the MLX engine does not implement is named, not guessed.
        let other = ir(SourceFormat::Safetensors, "gemma3", &[affine, dense]);
        assert!(capability(&other, Engine::Mlx).unwrap().contains("gemma3"));
        assert_eq!(Engine::parse("cuda"), Some(Engine::Cuda));
        assert_eq!(Engine::parse("other"), None);
    }

    #[test]
    fn auto_prefers_available_mlx_metal_then_cuda_then_cpu() {
        let mut statuses: Vec<_> = [Engine::Cpu, Engine::Mlx, Engine::Cuda].into_iter()
            .map(|engine| EngineStatus { engine, payload_supported: true, compiled: true, available: true, reason: None }).collect();
        assert_eq!(select_statuses(&statuses, Engine::Auto).unwrap(), Engine::Mlx);
        assert_eq!(select_statuses(&statuses, Engine::Cpu).unwrap(), Engine::Cpu);
        statuses[1].payload_supported = false;
        assert_eq!(select_statuses(&statuses, Engine::Auto).unwrap(), Engine::Cuda);
        statuses[2].available = false; // Compiled CUDA without a device/driver.
        statuses[2].reason = Some("CUDA device unavailable".into());
        assert_eq!(select_statuses(&statuses, Engine::Auto).unwrap(), Engine::Cpu);
        assert!(select_statuses(&statuses, Engine::Cuda).unwrap_err().to_string().contains("unavailable"));
        statuses[1].payload_supported = true;
        statuses[1].available = false;
        statuses[1].reason = Some("Metal device unavailable".into());
        assert_eq!(select_statuses(&statuses, Engine::Auto).unwrap(), Engine::Cpu);
        assert!(select_statuses(&statuses, Engine::Mlx).is_err());
        statuses[1].available = true;
        statuses[1].compiled = false;
        statuses[1].reason = Some("MLX is absent from this build".into());
        assert!(select_statuses(&statuses, Engine::Mlx).unwrap_err().to_string().contains("absent"));
        statuses[1].compiled = true;
        assert_eq!(select_statuses(&statuses, Engine::Auto).unwrap(), Engine::Mlx);
        assert_eq!(Engine::parse("metal"), Some(Engine::Mlx));
        assert_eq!(Engine::parse("mps"), Some(Engine::Mlx));
    }

    /// Greedy replies of one GGUF LMX on llama.cpp and on MLX (blocks repacked as affine
    /// groups). `LOOM_LMX_PARITY=/path/model.lmx cargo test --release --lib
    /// gguf_on_mlx_matches_llama_cpp -- --ignored --nocapture`
    #[cfg(loom_mlx)]
    #[test]
    #[ignore]
    fn gguf_on_mlx_matches_llama_cpp() {
        let path = std::env::var("LOOM_LMX_PARITY").expect("set LOOM_LMX_PARITY to a GGUF .lmx file");
        let executable = LmxFile::open(std::path::Path::new(&path)).unwrap();
        let prompts = ["Write a Rust function that reverses a string.", "서울의 날씨를 한 문장으로 설명해줘."];
        for prompt in prompts {
            let request = Request {
                model: executable.id(),
                messages: vec![Message::user(prompt)],
                max_output_tokens: Some(32),
                temperature: Some(0.0),
                reasoning_effort: Some("none".into()),
                ..Request::default()
            };
            let run = |engine| {
                let reply = match executor(&executable, engine).unwrap() {
                    Executor::Native(provider) => provider.complete(&request),
                    Executor::Mlx(provider) => provider.complete(&request),
                };
                reply.unwrap().text()
            };
            let (cpu, mlx) = (run(Engine::Cpu), run(Engine::Mlx));
            let same = cpu.chars().zip(mlx.chars()).take_while(|(a, b)| a == b).count();
            eprintln!("--- {prompt}\ncpu: {cpu:?}\nmlx: {mlx:?}\ncommon prefix {same}/{} chars", cpu.chars().count());
            assert_eq!(cpu, mlx, "greedy 32-token reply differs for {prompt}");
        }
    }

    /// Installed LMX models answer when named the way the model list names them.
    /// `LOOM_LMX_ROUTE_TEST=1 cargo test --release --lib lmx_answers_by_display_name -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn lmx_answers_by_display_name() {
        std::env::var("LOOM_LMX_ROUTE_TEST").expect("set LOOM_LMX_ROUTE_TEST=1");
        let provider = LmxProvider::discover().unwrap();
        let model = provider.available_models().into_iter().next().expect("an installed LMX");
        let request = Request {
            model: model.display_name(),
            messages: vec![crate::models::Message::user("Reply with the single word: ok")],
            max_output_tokens: Some(8),
            ..Request::default()
        };
        let response = provider.complete(&request).unwrap();
        eprintln!("{} -> {:?}", request.model, response.parts);
    }
}
