//! In-process GGUF inference for Loom.
//!
//! This deliberately stops at the model boundary: no localhost server, HTTP, JSON wire
//! protocol, health polling or sidecar process. llama.cpp is linked as a library because
//! it already carries optimized CPU, Metal and CUDA kernels and supports Spark 2.5 GGUF.
use super::*;
use encoding_rs::UTF_8;
use llama_cpp_2::{
    context::params::{KvCacheType, LlamaContextParams},
    gguf::GgufContext,
    llama_backend::LlamaBackend,
    llama_batch::LlamaBatch,
    context::LlamaContext,
    model::{LlamaModel, params::LlamaModelParams},
    token::LlamaToken,
    sampling::LlamaSampler,
};
use serde_json::{Map, Value, json};
use std::{
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::{Mutex, OnceLock},
    time::Instant,
};

pub(super) const THINK_OPEN: &str = "<think>";
const END_THINK: &str = "</think>";
const TOOL_OPEN: &str = "<tool_call>";
const TOOL_CLOSE: &str = "</tool_call>";
const ARG_KEY_OPEN: &str = "<arg_key>";
const ARG_KEY_CLOSE: &str = "</arg_key>";
const ARG_VALUE_OPEN: &str = "<arg_value>";
const ARG_VALUE_CLOSE: &str = "</arg_value>";

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Backend {
    #[default]
    Auto,
    Cpu,
    Mps,
    Cuda,
}

impl Backend {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.trim().to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            "cpu" => Self::Cpu,
            "mps" | "metal" => Self::Mps,
            "cuda" => Self::Cuda,
            _ => return None,
        })
    }

    pub fn configured() -> Result<Self, ProviderError> {
        match std::env::var("LOOM_INFERENCE_BACKEND") {
            Ok(value) => Self::parse(&value).ok_or_else(|| {
                ProviderError::new(
                    ErrorKind::BadRequest,
                    format!("unknown LOOM_INFERENCE_BACKEND `{value}`; use auto, cpu, mps or cuda"),
                )
            }),
            Err(_) => {
                let configured = super::registry::Registry::open().and_then(|registry| registry.config())
                    .map_err(|error| ProviderError::new(ErrorKind::BadRequest, error.to_string()))?.inference_backend;
                match configured {
                    Some(value) => Self::parse(&value).ok_or_else(|| ProviderError::new(ErrorKind::BadRequest, "invalid saved inference backend")),
                    None => Ok(Self::Auto),
                }
            }
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Cpu => "cpu",
            Self::Mps => "mps",
            Self::Cuda => "cuda",
        }
    }

    fn resolve(self, backend: &LlamaBackend) -> Result<Self, ProviderError> {
        match self {
            Self::Auto => {
                if backend.supports_gpu_offload() {
                    #[cfg(target_os = "macos")]
                    return Ok(Self::Mps);
                    #[cfg(all(not(target_os = "macos"), feature = "cuda"))]
                    return Ok(Self::Cuda);
                }
                Ok(Self::Cpu)
            }
            Self::Cpu => Ok(Self::Cpu),
            Self::Mps => {
                if cfg!(target_os = "macos") && backend.supports_gpu_offload() {
                    Ok(Self::Mps)
                } else {
                    Err(ProviderError::new(
                        ErrorKind::BadRequest,
                        "MPS/Metal backend is unavailable in this Loom build",
                    ))
                }
            }
            Self::Cuda => {
                if cfg!(feature = "cuda") && backend.supports_gpu_offload() {
                    Ok(Self::Cuda)
                } else {
                    Err(ProviderError::new(
                        ErrorKind::BadRequest,
                        "CUDA backend is unavailable; build Loom with `--features cuda` on a CUDA host",
                    ))
                }
            }
        }
    }
}

/// Runtime CUDA availability, distinct from merely compiling the CUDA backend.
pub(super) fn cuda_available() -> bool {
    if !cfg!(feature = "cuda") || llama_backend().is_err() { return false; }
    unsafe {
        let registry = llama_cpp_sys_2::ggml_backend_reg_by_name(c"CUDA".as_ptr());
        !registry.is_null() && llama_cpp_sys_2::ggml_backend_reg_dev_count(registry) > 0
    }
}

#[derive(Clone, Debug)]
pub struct ModelInfo {
    pub id: String,
    pub path: PathBuf,
    pub architecture: String,
    /// The GGUF's own chat template; Spark uses Loom's built-in prompt instead.
    pub template: Option<String>,
}

impl ModelInfo {
    pub fn inspect(path: &Path, root: &Path) -> Option<Self> {
        let relative = path.strip_prefix(root).unwrap_or(path);
        Self::named(path, relative.to_string_lossy().replace('\\', "/"))
    }

    pub(crate) fn named(path: &Path, id: String) -> Option<Self> {
        silence_native_logs();
        let gguf = GgufContext::from_file(path)?;
        let architecture_index = gguf.find_key("general.architecture");
        if architecture_index < 0 {
            return None;
        }
        let architecture = gguf.val_str(architecture_index)?.to_owned();
        let template_index = gguf.find_key("tokenizer.chat_template");
        let template = (template_index >= 0)
            .then(|| gguf.val_str(template_index).map(str::to_owned))
            .flatten();
        // Without a chat template Loom cannot know how the model expects a conversation.
        if architecture != "spark2_5" && template.is_none() {
            return None;
        }
        Some(Self {
            id,
            path: path.to_owned(),
            architecture,
            template,
        })
    }
}

/// Precision of the attention KV cache. Lower precision is the main lever for fitting
/// longer context or a bigger model into the same memory.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KvPrecision {
    #[default]
    F16,
    Q8,
    Q4,
    /// MLX only: 3-bit groups.
    Q3,
    /// MLX only: TurboQuant-style random Hadamard rotation, then 4- or 3-bit groups.
    Turbo4,
    Turbo3,
}

impl KvPrecision {
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value.trim().to_ascii_lowercase().as_str() {
            "f16" => Self::F16,
            "q8" | "q8_0" => Self::Q8,
            "q4" | "q4_0" => Self::Q4,
            "q3" => Self::Q3,
            "turbo4" | "tq4" => Self::Turbo4,
            "turbo3" | "tq3" => Self::Turbo3,
            _ => return None,
        })
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::F16 => "f16",
            Self::Q8 => "q8_0",
            Self::Q4 => "q4_0",
            Self::Q3 => "q3",
            Self::Turbo4 => "turbo4",
            Self::Turbo3 => "turbo3",
        }
    }
    /// llama.cpp has no 3-bit or rotated KV type; those fall back to its 4-bit cache.
    fn cache_type(self) -> KvCacheType {
        match self {
            Self::F16 => KvCacheType::F16,
            Self::Q8 => KvCacheType::Q8_0,
            Self::Q4 | Self::Q3 | Self::Turbo4 | Self::Turbo3 => KvCacheType::Q4_0,
        }
    }
}

/// How a llama context is built. `flash: None` leaves llama.cpp's automatic choice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ContextOptions {
    pub kv: KvPrecision,
    pub flash: Option<bool>,
}

impl ContextOptions {
    /// `LOOM_NATIVE_KV=f16|q8_0|q4_0`, `LOOM_NATIVE_FLASH=auto|on|off`.
    pub fn configured() -> Self {
        Self {
            kv: std::env::var("LOOM_NATIVE_KV")
                .ok()
                .and_then(|value| KvPrecision::parse(&value))
                .unwrap_or_default(),
            flash: match std::env::var("LOOM_NATIVE_FLASH").ok().as_deref() {
                Some("on") => Some(true),
                Some("off") => Some(false),
                _ => None,
            },
        }
    }
    fn apply(self, params: LlamaContextParams) -> LlamaContextParams {
        let params = params
            .with_type_k(self.kv.cache_type())
            .with_type_v(self.kv.cache_type());
        match self.flash {
            Some(enabled) => params.with_flash_attention_policy(i32::from(enabled)),
            None => params,
        }
    }
}

/// Memory this process holds, as Activity Monitor counts it (including GPU buffers on
/// unified memory). Zero where the platform does not report it.
pub fn footprint_bytes() -> u64 {
    #[cfg(target_os = "macos")]
    unsafe {
        let mut info: libc::rusage_info_v4 = std::mem::zeroed();
        let pointer = (&mut info as *mut libc::rusage_info_v4).cast::<libc::rusage_info_t>();
        if libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V4, pointer) == 0 {
            return info.ri_phys_footprint;
        }
    }
    0
}

/// Lifetime physical footprint peak, distinct from the MLX allocator's peak.
pub fn peak_footprint_bytes() -> u64 {
    #[cfg(target_os = "macos")]
    unsafe {
        let mut info: libc::rusage_info_v4 = std::mem::zeroed();
        let pointer = (&mut info as *mut libc::rusage_info_v4).cast::<libc::rusage_info_t>();
        if libc::proc_pid_rusage(libc::getpid(), libc::RUSAGE_INFO_V4, pointer) == 0 {
            return info.ri_lifetime_max_phys_footprint;
        }
    }
    footprint_bytes()
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct BenchReport {
    pub model: String,
    pub backend: &'static str,
    pub kv: &'static str,
    pub flash: Option<bool>,
    pub weights_bytes: u64,
    pub load_ms: u64,
    /// Footprint growth from loading the weights.
    pub weights_memory: u64,
    pub context_tokens: u32,
    /// Footprint growth from creating the context (KV cache and compute buffers).
    pub context_memory: u64,
    pub prompt_tokens: usize,
    pub prefill_ms: u64,
    pub generated: usize,
    pub decode_ms: u64,
    pub peak_footprint: u64,
    /// MLX allocator buffers; kept distinct from the process's physical footprint.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mlx_memory: Option<MlxMemoryReport>,
    /// Process-local reservation estimates; counters/peak are cumulative and are
    /// not physical footprint measurements or OS limits on tools.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local_resources: Option<crate::base::resources::ResourceSnapshot>,
    /// Greedy continuation, to compare cache precisions against a full-precision run.
    pub tokens: Vec<u32>,
}

#[derive(Clone, Debug, serde::Serialize)]
pub struct MlxMemoryReport {
    pub active_bytes: u64,
    pub peak_active_bytes: u64,
    pub cache_bytes: u64,
}

/// Benchmark a local model: `mlx/<id>` on the MLX engine (macOS), otherwise a GGUF.
pub fn bench(
    id: &str,
    options: ContextOptions,
    context_tokens: u32,
    prompt_tokens: usize,
    generate: usize,
) -> Result<BenchReport, ProviderError> {
    if id.starts_with("lmx/") {
        return super::lmx_runtime::bench(id, super::lmx_runtime::Engine::configured().map_err(|error| ProviderError::new(ErrorKind::BadRequest,error.to_string()))?, options, context_tokens, prompt_tokens, generate);
    }
    #[cfg(loom_mlx)]
    if let Some(id) = id.strip_prefix("mlx/") {
        return super::mlx_runtime::MlxProvider::discover().bench(id, options.kv, prompt_tokens, generate);
    }
    let provider = NativeProvider::discover(Backend::configured()?);
    if !provider.models().iter().any(|model| model.id == id) {
        let ids: Vec<_> = provider.models().iter().map(|model| model.id.as_str()).collect();
        return Err(ProviderError::new(
            ErrorKind::BadRequest,
            format!("`{id}` is not a native model; available: {}", ids.join(", ")),
        ));
    }
    provider.bench(id, options, context_tokens, prompt_tokens, generate)
}

impl BenchReport {
    pub fn prefill_per_second(&self) -> f64 {
        self.prompt_tokens as f64 / (self.prefill_ms.max(1) as f64 / 1000.0)
    }
    pub fn decode_per_second(&self) -> f64 {
        self.generated as f64 / (self.decode_ms.max(1) as f64 / 1000.0)
    }
}

struct LoadedModel {
    weights: WeightsKey,
    /// Leaked so the persistent context can borrow it; freed in `Drop` after the context.
    model: &'static LlamaModel,
    session: Option<Session>,
}

/// One llama context kept across requests, with the tokens its KV cache holds. Core
/// resends the same instructions and history every step, so only the new tail is decoded.
struct Session {
    context: LlamaContext<'static>,
    tokens: Vec<LlamaToken>,
}

// SAFETY: a llama model and context may be used from any thread, one at a time. Both
// live only inside `loaded_slot()`'s mutex, which serializes every use.
unsafe impl Send for LoadedModel {}

impl Drop for LoadedModel {
    fn drop(&mut self) {
        // The context references the weights, so it must go first.
        self.session = None;
        // SAFETY: `model` came from `Box::leak` in `ensure_loaded` and nothing else holds it.
        unsafe { drop(Box::from_raw(self.model as *const LlamaModel as *mut LlamaModel)) };
    }
}

/// Tokens of `next` already present in a KV cache holding `cached`. At least one token
/// is always left to decode, because sampling needs the logits of the final prompt token.
fn reusable_prefix<T: PartialEq>(cached: &[T], next: &[T]) -> usize {
    let common = cached.iter().zip(next).take_while(|(a, b)| a == b).count();
    common.min(next.len().saturating_sub(1))
}

/// The file and backend a load used. A re-downloaded file of another size is reloaded.
#[derive(Clone, Debug, PartialEq, Eq)]
struct WeightsKey {
    path: PathBuf,
    bytes: u64,
    backend: Backend,
}

impl WeightsKey {
    fn of(path: &Path, backend: Backend) -> Self {
        Self {
            path: path.to_owned(),
            bytes: std::fs::metadata(path).map_or(0, |meta| meta.len()),
            backend,
        }
    }
}

/// Loaded weights outlive providers. Rediscovering models (opening the model picker,
/// changing providers) builds a new provider and must not reload the GGUF from disk.
static LOADED: Mutex<Option<LoadedModel>> = Mutex::new(None);

/// ggml frees its Metal device from a C++ static destructor at exit and aborts if any
/// buffer is still alive. Release the resident model first: `atexit` handlers registered
/// after ggml initialized run before its destructors.
extern "C" fn release_at_exit() {
    if let Ok(mut slot) = LOADED.try_lock() {
        *slot = None;
    }
}

/// Frees the loaded model unless a request is using it (another runtime is loading).
pub(super) fn release_idle() {
    if let Ok(mut slot) = LOADED.try_lock() {
        *slot = None;
    }
}

fn loaded_slot() -> std::sync::MutexGuard<'static, Option<LoadedModel>> {
    LOADED.lock().unwrap_or_else(|poisoned| {
        // A panic mid-inference may leave the model in any state; load it afresh.
        let mut slot = poisoned.into_inner();
        *slot = None;
        slot
    })
}

pub struct NativeProvider {
    requested_backend: Backend,
    models: Vec<ModelInfo>,
}

/// Ollama's display name for a manifest path below `manifests/`:
/// `registry.ollama.ai/library/qwen3/8b` → `qwen3:8b`, `hf.co/o/r/Q4` → `hf.co/o/r:Q4`.
fn ollama_name(relative: &Path) -> Option<String> {
    let parts: Vec<_> = relative.iter().map(|part| part.to_str()).collect::<Option<_>>()?;
    let (tag, path) = parts.split_last()?;
    let path = match path {
        ["registry.ollama.ai", "library", rest @ ..] | ["registry.ollama.ai", rest @ ..] => rest.to_vec(),
        other => other.to_vec(),
    };
    (!path.is_empty()).then(|| format!("{}:{tag}", path.join("/")))
}

/// Weights Ollama already downloaded, used in place without a second copy.
fn ollama_models() -> Vec<ModelInfo> {
    let Some(root) = std::env::var_os("OLLAMA_MODELS")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".ollama/models")))
    else {
        return Vec::new();
    };
    let manifests = root.join("manifests");
    let mut pending = vec![manifests.clone()];
    let mut found = Vec::new();
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                pending.push(path);
                continue;
            }
            let Some(name) = path.strip_prefix(&manifests).ok().and_then(ollama_name) else { continue };
            let Some(manifest) = std::fs::read(&path).ok().and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok()) else { continue };
            let blob = manifest["layers"].as_array().into_iter().flatten()
                .find(|layer| layer["mediaType"] == "application/vnd.ollama.image.model")
                .and_then(|layer| layer["digest"].as_str())
                .and_then(|digest| digest.strip_prefix("sha256:"))
                .filter(|hex| hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit()))
                .map(|hex| root.join("blobs").join(format!("sha256-{hex}")));
            if let Some(model) = blob.and_then(|blob| ModelInfo::named(&blob, format!("ollama/{name}"))) {
                found.push(model);
            }
        }
    }
    found.sort_by(|a, b| a.id.cmp(&b.id));
    found
}

impl NativeProvider {
    pub(crate) fn from_models(requested_backend: Backend, models: Vec<ModelInfo>) -> Self {
        Self { requested_backend, models }
    }
    pub fn discover(requested_backend: Backend) -> Self {
        let root = super::local::models_dir();
        let models = super::local::installed_ggufs()
            .into_iter()
            .filter_map(|path| ModelInfo::inspect(&path, &root))
            .chain(ollama_models())
            .collect();
        Self {
            requested_backend,
            models,
        }
    }

    pub fn models(&self) -> &[ModelInfo] {
        &self.models
    }

    fn model_info(&self, id: &str) -> Option<&ModelInfo> {
        self.models.iter().find(|model| model.id == id)
    }

    fn ensure_loaded<'a>(
        &self,
        loaded: &'a mut Option<LoadedModel>,
        info: &ModelInfo,
    ) -> Result<&'a mut LoadedModel, ProviderError> {
        let backend = llama_backend()?;
        let resolved = self.requested_backend.resolve(backend)?;
        let weights = WeightsKey::of(&info.path, resolved);
        let replace = !loaded.as_ref().is_some_and(|model| model.weights == weights);
        if replace {
            // Free the previous weights before mapping the next ones, and the native LMX
            // engine's if it is idle: one local model in memory at a time.
            *loaded = None;
            super::lmx_native_runtime::release_idle();
            let params = match resolved {
                Backend::Cpu => Box::pin(LlamaModelParams::default().with_n_gpu_layers(0)),
                Backend::Mps | Backend::Cuda => fitted_params(&info.path),
                Backend::Auto => unreachable!(),
            };
            let model = LlamaModel::load_from_file(backend, &info.path, &params).map_err(|error| {
                ProviderError::new(
                    ErrorKind::BadRequest,
                    format!("cannot load {} on {}: {error}", info.path.display(), resolved.label()),
                )
            })?;
            static AT_EXIT: std::sync::Once = std::sync::Once::new();
            AT_EXIT.call_once(|| unsafe {
                libc::atexit(release_at_exit);
            });
            *loaded = Some(LoadedModel {
                weights,
                model: Box::leak(Box::new(model)),
                session: None,
            });
        }
        Ok(loaded.as_mut().expect("model was just loaded"))
    }

    /// Load `id` fresh and measure memory, prompt processing and generation.
    pub fn bench(
        &self,
        id: &str,
        options: ContextOptions,
        context_tokens: u32,
        prompt_tokens: usize,
        generate: usize,
    ) -> Result<BenchReport, ProviderError> {
        let info = self.model_info(id).ok_or_else(|| {
            ProviderError::new(ErrorKind::BadRequest, format!("native GGUF model `{id}` is not installed or supported"))
        })?;
        let mut slot = loaded_slot();
        *slot = None;
        let before = footprint_bytes();
        let started = Instant::now();
        let loaded = self.ensure_loaded(&mut slot, info)?;
        let load_ms = started.elapsed().as_millis() as u64;
        let after_load = footprint_bytes();
        let model = loaded.model;
        let backend_label = loaded.weights.backend.label();
        let backend = llama_backend()?;
        let batch_size = prompt_tokens.clamp(1, 2048) as u32;
        let params = options.apply(
            LlamaContextParams::default()
                .with_n_ctx(NonZeroU32::new(context_tokens.max(256)))
                .with_n_batch(batch_size),
        );
        let mut context = model
            .new_context(backend, params)
            .map_err(|error| ProviderError::new(ErrorKind::Transient, format!("cannot create native model context: {error}")))?;
        let after_context = footprint_bytes();
        // Source-like text, repeated to the requested length.
        let sample = "fn route(request: &Request) -> Result<Response> {\n    let path = request.path.trim_start_matches('/');\n    match path.split_once('/') { Some((head, rest)) => dispatch(head, rest), None => not_found(path) }\n}\n";
        let mut tokens = Vec::new();
        while tokens.len() < prompt_tokens {
            tokens.extend(model.vocab().tokenize(sample.as_bytes(), tokens.is_empty() && model.vocab().should_add_bos(), false));
        }
        tokens.truncate(prompt_tokens.max(1));
        let mut batch = LlamaBatch::new(batch_size as usize, 1);
        let started = Instant::now();
        for (offset, chunk) in tokens.chunks(batch_size as usize).enumerate() {
            batch.clear();
            let base = offset * batch_size as usize;
            for (index, token) in chunk.iter().enumerate() {
                let position = base + index;
                batch
                    .add(*token, position as i32, &[0], position + 1 == tokens.len())
                    .map_err(|error| ProviderError::new(ErrorKind::BadRequest, error.to_string()))?;
            }
            context
                .decode(&mut batch)
                .map_err(|error| ProviderError::new(ErrorKind::Transient, format!("native prompt decode failed: {error}")))?;
        }
        let prefill_ms = started.elapsed().as_millis() as u64;
        let mut sampler = LlamaSampler::greedy();
        let mut continuation = Vec::with_capacity(generate);
        let mut position = tokens.len() as i32;
        let started = Instant::now();
        for _ in 0..generate {
            let token = sampler.sample(&context, batch.n_tokens() - 1);
            continuation.push(token.0 as u32);
            sampler.accept(token);
            batch.clear();
            batch
                .add(token, position, &[0], true)
                .map_err(|error| ProviderError::new(ErrorKind::BadRequest, error.to_string()))?;
            position += 1;
            context
                .decode(&mut batch)
                .map_err(|error| ProviderError::new(ErrorKind::Transient, format!("native token decode failed: {error}")))?;
        }
        let decode_ms = started.elapsed().as_millis() as u64;
        let peak_footprint = footprint_bytes();
        drop(context);
        Ok(BenchReport {
            model: id.to_owned(),
            backend: backend_label,
            kv: options.kv.label(),
            flash: options.flash,
            weights_bytes: std::fs::metadata(&info.path).map_or(0, |meta| meta.len()),
            load_ms,
            weights_memory: after_load.saturating_sub(before),
            context_tokens: context_tokens.max(256),
            context_memory: after_context.saturating_sub(after_load),
            prompt_tokens: tokens.len(),
            prefill_ms,
            generated: generate,
            decode_ms,
            peak_footprint,
            mlx_memory: None,
            local_resources: None,
            tokens: continuation,
        })
    }

    fn infer(
        &self,
        request: &Request,
        observe: &mut dyn FnMut(StreamEvent),
    ) -> Result<Response, ProviderError> {
        request.check_cancelled()?;
        let started = Instant::now();
        let info = self.model_info(&request.model).ok_or_else(|| {
            ProviderError::new(
                ErrorKind::BadRequest,
                format!("native GGUF model `{}` is not installed or supported", request.model),
            )
        })?;
        let mut slot = loaded_slot();
        let loaded = self.ensure_loaded(&mut slot, info)?;
        if request.timeout.is_some_and(|limit| started.elapsed() >= limit) {
            return Err(ProviderError::new(ErrorKind::TimedOut, "model load exceeded request timeout"));
        }

        // Reasoning is opt-in: the profile's default is "none", and thinking costs time.
        let thinking = request.reasoning_effort.as_deref().is_some_and(|effort| effort != "none");
        let (prompt, add_bos) = match info.template.as_deref().filter(|_| info.architecture != "spark2_5") {
            Some(template) => {
                let vocab = loaded.model.vocab();
                let piece = |token: Option<LlamaToken>| {
                    token.map(|token| String::from_utf8_lossy(&vocab.token_to_piece(token, true, None)).into_owned())
                        .unwrap_or_default()
                };
                // Models without a BOS (Qwen) still report a placeholder id; never use it.
                let uses_bos = vocab.should_add_bos();
                let bos = if uses_bos { piece(Some(vocab.bos())) } else { String::new() };
                let eos = piece(Some(vocab.eos()));
                let prompt = render_template_prompt(template, request, thinking, &bos, &eos)
                    .map_err(|error| ProviderError::new(ErrorKind::BadRequest, format!("chat template of {}: {error}", info.id)))?;
                // Templates that already emit the BOS text must not get a second one.
                let add_bos = uses_bos && !prompt.starts_with(&bos);
                (prompt, add_bos)
            }
            None => (render_spark_prompt(request, thinking), true),
        };
        let tokens = loaded.model.vocab().tokenize(prompt.as_bytes(), add_bos, true);
        let max_output = request.max_output_tokens.unwrap_or(2048).max(1) as usize;
        let required = tokens.len().saturating_add(max_output);
        let trained = loaded.model.n_ctx_train() as usize;
        let configured_cap = configured_context().min(trained.max(1));
        if required > configured_cap {
            return Err(ProviderError::new(
                ErrorKind::BadRequest,
                format!(
                    "request ({required} tokens) exceeds native context limit ({configured_cap} tokens); set LOOM_NATIVE_CONTEXT to raise it"
                ),
            ));
        }
        if tokens.is_empty() {
            return Err(ProviderError::new(ErrorKind::BadRequest, "native prompt tokenized to zero tokens"));
        }
        let model = loaded.model;
        let backend = llama_backend()?;
        // Reuse the live context when it is large enough; otherwise grow it (dropping its
        // cache). Starting at 8K avoids regrowing on every step of a lengthening session.
        let fits = loaded
            .session
            .as_ref()
            .is_some_and(|session| session.context.n_ctx() as usize >= required);
        if !fits {
            loaded.session = None;
            let context_tokens = required
                .next_power_of_two()
                .max(8192)
                .min(configured_cap)
                .min(u32::MAX as usize) as u32;
            let context_tokens = NonZeroU32::new(context_tokens).expect("native context is non-zero");
            let ctx_params = ContextOptions::configured()
                .apply(LlamaContextParams::default().with_n_ctx(Some(context_tokens)));
            let context = model
                .new_context(backend, ctx_params)
                .map_err(|error| ProviderError::new(ErrorKind::Transient, format!("cannot create native model context: {error}")))?;
            loaded.session = Some(Session { context, tokens: Vec::new() });
        }
        let session = loaded.session.as_mut().expect("native context was just created");
        let mut reused = reusable_prefix(&session.tokens, &tokens);
        // Drop the diverging suffix. Models whose memory cannot be trimmed mid-sequence
        // (recurrent layers) start over from an empty cache instead.
        let trimmed = session
            .context
            .clear_kv_cache_seq(Some(0), Some(reused as u32), None)
            .unwrap_or(false);
        if !trimmed {
            session.context.clear_kv_cache();
            reused = 0;
        }
        // Until this request completes, the cache holds only the shared prefix.
        session.tokens.truncate(reused);
        let context = &mut session.context;

        let pending = &tokens[reused..];
        let batch_capacity = pending.len().clamp(1, 2048);
        let mut batch = LlamaBatch::new(batch_capacity, 1);
        let mut position = reused as i32;
        for chunk in pending.chunks(batch_capacity) {
            batch.clear();
            for (index, token) in chunk.iter().enumerate() {
                let is_last = position as usize + index + 1 == tokens.len();
                batch
                    .add(*token, position + index as i32, &[0], is_last)
                    .map_err(|error| ProviderError::new(ErrorKind::BadRequest, error.to_string()))?;
            }
            context
                .decode(&mut batch)
                .map_err(|error| ProviderError::new(ErrorKind::Transient, format!("native prompt decode failed: {error}")))?;
            position += chunk.len() as i32;
            session.tokens.extend_from_slice(chunk);
            check_limits(request, started)?;
        }

        let temperature = request.temperature.unwrap_or(0.2);
        let mut sampler = if temperature <= 0.0 {
            LlamaSampler::greedy()
        } else {
            LlamaSampler::chain_simple([
                LlamaSampler::temp(temperature),
                LlamaSampler::top_p(0.95, 1),
                LlamaSampler::dist(0x10_0D_F00D),
            ])
        };
        let mut decoder = UTF_8.new_decoder();
        // Spark's prompt opens the think block itself, and so do templates whose
        // generation prompt ends in `<think>`; other models open their own.
        let opened = prompt.trim_end().ends_with(THINK_OPEN);
        let mut output = SparkOutput::new((thinking && info.architecture == "spark2_5") || opened);
        if info.template.as_deref().is_some_and(|template| template.contains("<function=")) {
            output = output.with_xml_tools(&request.tools);
        }
        let mut generated = 0usize;
        let mut hit_eog = false;
        let decode_started = Instant::now();
        let mut last_speed = Instant::now();
        while generated < max_output {
            if last_speed.elapsed().as_millis() >= 500 {
                last_speed = Instant::now();
                observe(StreamEvent::Speed {
                    tokens: generated as u64,
                    seconds: decode_started.elapsed().as_secs_f64(),
                });
            }
            check_limits(request, started)?;
            let token = sampler.sample(context, batch.n_tokens() - 1);
            sampler.accept(token);
            if model.vocab().is_eog(token) {
                hit_eog = true;
                break;
            }
            let bytes = model.vocab().token_to_piece(token, true, None);
            let mut decoded = String::with_capacity(
                decoder.max_utf8_buffer_length(bytes.len()).unwrap_or(bytes.len().saturating_mul(4)),
            );
            let _ = decoder.decode_to_string(&bytes, &mut decoded, false);
            for delta in output.feed(&decoded) {
                if !delta.is_empty() {
                    observe(StreamEvent::TextDelta(delta));
                }
            }
            generated += 1;
            batch.clear();
            batch
                .add(token, position, &[0], true)
                .map_err(|error| ProviderError::new(ErrorKind::BadRequest, error.to_string()))?;
            position += 1;
            context
                .decode(&mut batch)
                .map_err(|error| ProviderError::new(ErrorKind::Transient, format!("native token decode failed: {error}")))?;
            session.tokens.push(token);
        }
        if generated > 0 {
            observe(StreamEvent::Speed {
                tokens: generated as u64,
                seconds: decode_started.elapsed().as_secs_f64(),
            });
        }
        let mut tail = String::new();
        let _ = decoder.decode_to_string(&[], &mut tail, true);
        for delta in output.feed(&tail).into_iter().chain(output.finish()) {
            if !delta.is_empty() {
                observe(StreamEvent::TextDelta(delta));
            }
        }
        let mut parts = Vec::new();
        if !output.visible.trim().is_empty() {
            parts.push(Part::Text(output.visible.clone()));
        }
        parts.extend(parse_tool_calls_for_tools(&output.tools, &request.tools));
        let stop = if parts.iter().any(|part| matches!(part, Part::ToolCall { .. })) {
            Stop::ToolUse
        } else if hit_eog {
            Stop::EndTurn
        } else {
            Stop::MaxTokens
        };
        let usage = Usage {
            input: Some(tokens.len() as u64),
            cached_input: Some(reused as u64),
            output: Some(generated as u64),
        };
        observe(StreamEvent::Usage(usage.clone()));
        Ok(Response {
            parts,
            usage,
            stop,
            model: request.model.clone(),
            latency: started.elapsed(),
            route: None,
            gateway_connection: None,
            gateway_upstream_ms: None,
        })
    }
}

impl Provider for NativeProvider {
    fn name(&self) -> &str {
        "loom"
    }

    fn complete(&self, request: &Request) -> Result<Response, ProviderError> {
        self.infer(request, &mut |_| {})
    }

    fn complete_stream(
        &self,
        request: &Request,
        observe: &mut dyn FnMut(StreamEvent),
    ) -> Result<Response, ProviderError> {
        self.infer(request, observe)
    }

    fn list_models(&self) -> Result<Vec<String>, ProviderError> {
        Ok(self.models.iter().map(|model| model.id.clone()).collect())
    }

    fn reasoning_profile(&self, _model: &str) -> ReasoningProfile {
        ReasoningProfile {
            supported_efforts: vec!["none".into(), "on".into()],
            default_effort: Some("none".into()),
        }
    }
}

/// llama.cpp and ggml (Metal device setup, GGUF parsing) print to stderr, which draws
/// over the terminal UI. Silence them before the first call into either library.
pub(super) fn silence_native_logs() {
    static SILENCED: std::sync::Once = std::sync::Once::new();
    SILENCED.call_once(|| {
        llama_cpp_2::send_logs_to_tracing(llama_cpp_2::LogOptions::default().with_logs_enabled(false));
    });
}

pub(super) fn llama_backend() -> Result<&'static LlamaBackend, ProviderError> {
    static BACKEND: OnceLock<Result<LlamaBackend, String>> = OnceLock::new();
    match BACKEND.get_or_init(|| {
        silence_native_logs();
        let mut backend = LlamaBackend::init().map_err(|error| error.to_string())?;
        backend.void_logs();
        Ok(backend)
    }) {
        Ok(backend) => Ok(backend),
        Err(error) => Err(ProviderError::new(
            ErrorKind::Transient,
            format!("cannot initialize native inference backend: {error}"),
        )),
    }
}

/// Largest context Loom creates for a request (`LOOM_NATIVE_CONTEXT`, default 32K); the
/// model's trained length lowers it further once the model is loaded.
fn configured_context() -> usize {
    std::env::var("LOOM_NATIVE_CONTEXT")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(32_768)
}

/// GPU parameters that fit the device's memory with the largest context Loom will create.
/// A model that fits loads entirely on the GPU as before; one that does not keeps as many
/// layers as fit there and, for MoE, places the routed expert banks of the remaining
/// layers in CPU buffers instead of failing at the first decode. `LOOM_NATIVE_FIT=0`
/// restores offloading every layer.
fn fitted_params(path: &Path) -> std::pin::Pin<Box<LlamaModelParams>> {
    let all = || Box::pin(LlamaModelParams::default().with_n_gpu_layers(u32::MAX));
    if std::env::var("LOOM_NATIVE_FIT").is_ok_and(|value| value == "0") {
        return all();
    }
    // Fitting simulates a load (about two seconds); skip it when the weights plainly fit.
    let weights = std::fs::metadata(path).map_or(u64::MAX, |meta| meta.len());
    if gpu_free_memory().is_some_and(|free| weights.saturating_mul(5) / 4 + (3 << 30) <= free) {
        return all();
    }
    let Some(path) = path.to_str().and_then(|path| std::ffi::CString::new(path).ok()) else {
        return all();
    };
    let context = NonZeroU32::new(configured_context().min(u32::MAX as usize) as u32);
    let mut context = ContextOptions::configured().apply(LlamaContextParams::default().with_n_ctx(context));
    // Headroom for the OS, the compute graph and Loom itself; memory comes before speed.
    let margin = std::env::var("LOOM_NATIVE_FIT_MARGIN_MB")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(1024)
        << 20;
    let mut margins = vec![margin; unsafe { llama_cpp_sys_2::llama_max_devices() }];
    let mut params = Box::pin(LlamaModelParams::default());
    match params.as_mut().fit_params(
        &path,
        &mut context,
        &mut margins,
        8192,
        llama_cpp_sys_2::GGML_LOG_LEVEL_ERROR,
    ) {
        Ok(_) => params,
        Err(_) => all(),
    }
}

/// Free memory of the first GPU device as llama.cpp sees it (Metal: the recommended
/// working set), or `None` without a GPU device.
fn gpu_free_memory() -> Option<u64> {
    use llama_cpp_sys_2 as sys;
    // SAFETY: device handles are owned by ggml's registry for the process lifetime.
    unsafe {
        (0..sys::ggml_backend_dev_count())
            .map(|index| sys::ggml_backend_dev_get(index))
            .find(|&device| sys::ggml_backend_dev_type(device) == sys::GGML_BACKEND_DEVICE_TYPE_GPU)
            .map(|device| {
                let (mut free, mut total) = (0usize, 0usize);
                sys::ggml_backend_dev_memory(device, &mut free, &mut total);
                free as u64
            })
    }
}

pub(super) fn check_limits(request: &Request, started: Instant) -> Result<(), ProviderError> {
    request.check_cancelled()?;
    if request.timeout.is_some_and(|limit| started.elapsed() >= limit) {
        Err(ProviderError::new(
            ErrorKind::TimedOut,
            "native model request timed out",
        ))
    } else {
        Ok(())
    }
}

fn render_spark_prompt(request: &Request, thinking: bool) -> String {
    let mut prompt = String::from("<｜start▁of▁sentence｜><|System|>\nyou are a helpful assistant.");
    if !request.tools.is_empty() {
        prompt.push_str("## Tools\nYou have access to the following functions:\n<tools>");
        for tool in &request.tools {
            prompt.push('\n');
            prompt.push_str(
                &json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.parameters,
                })
                .to_string(),
            );
        }
        prompt.push_str("\n</tools>");
    }
    if let Some(system) = &request.system
        && !system.is_empty()
    {
        prompt.push_str("\n\n");
        prompt.push_str(system);
    }
    prompt.push_str("<｜end▁of▁sentence｜>");

    for message in &request.messages {
        match message.role {
            Role::User => {
                let text = message
                    .parts
                    .iter()
                    .filter_map(|part| match part {
                        Part::Text(text) => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if !text.is_empty() {
                    prompt.push_str("<｜start▁of▁sentence｜><|User|>");
                    prompt.push_str(&text);
                    prompt.push_str("<｜end▁of▁sentence｜>");
                }
                let results = message.parts.iter().filter_map(|part| match part {
                    Part::ToolResult { output, .. } => Some(output.as_str()),
                    _ => None,
                });
                let mut any_result = false;
                for output in results {
                    if !any_result {
                        prompt.push_str("<｜start▁of▁sentence｜><|Tool|>");
                        any_result = true;
                    }
                    prompt.push_str("<tool_response>");
                    prompt.push_str(output);
                    prompt.push_str("</tool_response>");
                }
                if any_result {
                    prompt.push_str("<｜end▁of▁sentence｜>");
                }
            }
            Role::Assistant => {
                prompt.push_str("<｜start▁of▁sentence｜><|Bot|></think>");
                for part in &message.parts {
                    match part {
                        Part::Text(text) => prompt.push_str(text),
                        Part::ToolCall { name, arguments, .. } => {
                            prompt.push_str(TOOL_OPEN);
                            prompt.push_str(name);
                            if let Value::Object(arguments) = arguments {
                                for (key, value) in arguments {
                                    prompt.push_str(ARG_KEY_OPEN);
                                    prompt.push_str(key);
                                    prompt.push_str(ARG_KEY_CLOSE);
                                    prompt.push_str(ARG_VALUE_OPEN);
                                    match value {
                                        Value::String(value) => prompt.push_str(value),
                                        value => prompt.push_str(&value.to_string()),
                                    }
                                    prompt.push_str(ARG_VALUE_CLOSE);
                                }
                            }
                            prompt.push_str(TOOL_CLOSE);
                        }
                        _ => {}
                    }
                }
                prompt.push_str("<｜end▁of▁sentence｜>");
            }
        }
    }
    prompt.push_str("<｜start▁of▁sentence｜><|Bot|>");
    if thinking {
        prompt.push_str("<think>");
    } else {
        prompt.push_str(END_THINK);
    }
    prompt
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputMode {
    Thinking,
    Visible,
    Tool,
}

pub(super) struct SparkOutput {
    mode: OutputMode,
    pending: String,
    pub(super) visible: String,
    pub(super) tools: String,
    xml_markers: Vec<String>,
}

impl SparkOutput {
    pub(super) fn new(thinking: bool) -> Self {
        Self {
            mode: if thinking { OutputMode::Thinking } else { OutputMode::Visible },
            pending: String::new(),
            visible: String::new(),
            tools: String::new(),
            xml_markers: Vec::new(),
        }
    }

    /// Qwen3-Coder sometimes omits its outer tool_call tag. Recover only a
    /// declared function at the beginning of a line, outside Markdown fences.
    pub(super) fn with_xml_tools(mut self, tools: &[ToolSpec]) -> Self {
        self.xml_markers = tools.iter().map(|tool| format!("<function={}>", tool.name)).collect();
        self
    }

    fn bare_start(&self) -> Option<usize> {
        self.xml_markers.iter().filter_map(|marker| {
            self.pending.match_indices(marker).find_map(|(start, _)| {
                let prefix = format!("{}{}", self.visible, &self.pending[..start]);
                let line = prefix.rsplit_once('\n').map_or(prefix.as_str(), |(_, line)| line);
                if !line.trim().is_empty() { return None; }
                let mut fence: Option<&str> = None;
                for line in prefix.lines() {
                    let line = line.trim_start();
                    for mark in ["```", "~~~"] {
                        if line.starts_with(mark) {
                            match fence { None => fence = Some(mark), Some(open) if open == mark => fence = None, _ => {} }
                            break;
                        }
                    }
                }
                fence.is_none().then_some(start)
            })
        }).min()
    }

    pub(super) fn feed(&mut self, text: &str) -> Vec<String> {
        self.pending.push_str(text);
        let mut deltas = Vec::new();
        loop {
            match self.mode {
                OutputMode::Thinking => {
                    if let Some(end) = self.pending.find(END_THINK) {
                        self.pending.drain(..end + END_THINK.len());
                        self.mode = OutputMode::Visible;
                        continue;
                    }
                    let keep = partial_marker_suffix(&self.pending, END_THINK);
                    let drop = self.pending.len().saturating_sub(keep);
                    self.pending.drain(..drop);
                    break;
                }
                OutputMode::Visible => {
                    // A model that reasons opens with its own <think> block.
                    if self.visible.trim().is_empty() {
                        let lead = self.pending.trim_start();
                        if let Some(rest) = lead.strip_prefix(THINK_OPEN) {
                            self.pending = rest.to_owned();
                            self.mode = OutputMode::Thinking;
                            continue;
                        }
                        if !lead.is_empty() && THINK_OPEN.starts_with(lead) {
                            break;
                        }
                    }
                    let wrapped = self.pending.find(TOOL_OPEN);
                    let bare = self.bare_start();
                    if let Some(start) = wrapped.into_iter().chain(bare).min() {
                        let before: String = self.pending.drain(..start).collect();
                        if !before.is_empty() {
                            self.visible.push_str(&before);
                            deltas.push(before);
                        }
                        if wrapped == Some(start) { self.pending.drain(..TOOL_OPEN.len()); }
                        self.tools.push_str(TOOL_OPEN);
                        self.tools.push_str(&self.pending);
                        self.pending.clear();
                        self.mode = OutputMode::Tool;
                        break;
                    }
                    let keep = self.xml_markers.iter().map(|marker| partial_marker_suffix(&self.pending, marker))
                        .chain([partial_marker_suffix(&self.pending, TOOL_OPEN)]).max().unwrap_or(0);
                    let emit = self.pending.len().saturating_sub(keep);
                    if emit == 0 {
                        break;
                    }
                    let delta: String = self.pending.drain(..emit).collect();
                    self.visible.push_str(&delta);
                    deltas.push(delta);
                    break;
                }
                OutputMode::Tool => {
                    self.tools.push_str(&self.pending);
                    self.pending.clear();
                    break;
                }
            }
        }
        deltas
    }

    pub(super) fn finish(&mut self) -> std::vec::IntoIter<String> {
        let mut deltas = Vec::new();
        match self.mode {
            OutputMode::Visible if !self.pending.is_empty() => {
                let tail = std::mem::take(&mut self.pending);
                self.visible.push_str(&tail);
                deltas.push(tail);
            }
            OutputMode::Tool => {
                self.tools.push_str(&self.pending);
                self.pending.clear();
            }
            OutputMode::Thinking => self.pending.clear(),
            OutputMode::Visible => {}
        }
        deltas.into_iter()
    }
}

fn partial_marker_suffix(text: &str, marker: &str) -> usize {
    (1..marker.len().min(text.len()) + 1)
        .rev()
        .find(|length| marker.is_char_boundary(*length) && text.ends_with(&marker[..*length]))
        .unwrap_or(0)
}

/// Tool calls in either common form: Hermes/Qwen JSON (`{"name":..,"arguments":..}`)
/// or Spark's `name<arg_key>k</arg_key><arg_value>v</arg_value>` tags.
#[allow(dead_code)]
pub(super) fn parse_tool_calls(text: &str) -> Vec<Part> {
    parse_tool_calls_for_tools(text, &[])
}

pub(super) fn parse_tool_calls_for_tools(text: &str, tools: &[ToolSpec]) -> Vec<Part> {
    let mut calls = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(TOOL_OPEN) {
        rest = &rest[start + TOOL_OPEN.len()..];
        let end = rest.find(TOOL_CLOSE).unwrap_or(rest.len());
        let body = rest[..end].trim();
        rest = &rest[(end + TOOL_CLOSE.len()).min(rest.len())..];
        if let Ok(Value::Object(call)) = serde_json::from_str::<Value>(body)
            && let Some(name) = call.get("name").and_then(Value::as_str)
        {
            let arguments = match call.get("arguments").or_else(|| call.get("parameters")) {
                Some(Value::String(raw)) => serde_json::from_str(raw).unwrap_or_else(|_| json!({})),
                Some(value) => value.clone(),
                None => json!({}),
            };
            calls.push(Part::ToolCall {
                id: format!("loom-call-{}", calls.len() + 1),
                name: name.to_owned(),
                arguments,
            });
            continue;
        }
        if let Some((name, arguments)) = parse_xml_function(body, tools) {
            calls.push(Part::ToolCall {
                id: format!("loom-call-{}", calls.len() + 1),
                name,
                arguments,
            });
            continue;
        }
        for mut call in parse_spark_tool_calls(&format!("{TOOL_OPEN}{body}{TOOL_CLOSE}")) {
            if let Part::ToolCall { id, .. } = &mut call {
                *id = format!("loom-call-{}", calls.len() + 1);
            }
            calls.push(call);
        }
    }
    calls
}

/// Qwen3-Coder form: `<function=name><parameter=key>value</parameter>...</function>`.
fn parse_xml_function(body: &str, tools: &[ToolSpec]) -> Option<(String, Value)> {
    let rest = body.strip_prefix("<function=")?;
    let (name, mut rest) = rest.split_once('>')?;
    let schema = tools.iter().find(|tool| tool.name == name.trim()).map(|tool| &tool.parameters["properties"]);
    let mut arguments = Map::new();
    while let Some(start) = rest.find("<parameter=") {
        let after = &rest[start + "<parameter=".len()..];
        let (key, after) = after.split_once('>')?;
        let end = after.find("</parameter>")?;
        let raw = after[..end].strip_prefix('\n').unwrap_or(&after[..end]);
        let raw = raw.strip_suffix('\n').unwrap_or(raw);
        let value = if schema.is_some_and(|properties| properties[key.trim()]["type"] == "string") {
            Value::String(raw.to_owned())
        } else {
            serde_json::from_str::<Value>(raw).ok().filter(|value| !value.is_string())
                .unwrap_or_else(|| Value::String(raw.to_owned()))
        };
        arguments.insert(key.trim().to_owned(), value);
        rest = &after[end + "</parameter>".len()..];
    }
    Some((name.trim().to_owned(), Value::Object(arguments)))
}

/// Render a GGUF's Hugging Face chat template with OpenAI-style messages and tools.
pub(super) fn render_template_prompt(
    template: &str,
    request: &Request,
    thinking: bool,
    bos: &str,
    eos: &str,
) -> Result<String, String> {
    render_template(template, request, thinking, bos, eos, true)
}

/// The chat template rendering; without the generation prompt it ends where the next
/// turn's prompt continues, a stable point for reusing cached state.
pub(super) fn render_template(
    template: &str,
    request: &Request,
    thinking: bool,
    bos: &str,
    eos: &str,
    generation_prompt: bool,
) -> Result<String, String> {
    let mut messages = Vec::new();
    if let Some(system) = request.system.as_deref().filter(|system| !system.is_empty()) {
        messages.push(json!({"role":"system","content":system}));
    }
    for message in &request.messages {
        let text: String = message
            .parts
            .iter()
            .filter_map(|part| match part {
                Part::Text(text) => Some(text.as_str()),
                _ => None,
            })
            .collect();
        match message.role {
            Role::User => {
                for part in &message.parts {
                    if let Part::ToolResult { call_id, output, .. } = part {
                        messages.push(json!({"role":"tool","tool_call_id":call_id,"content":output}));
                    }
                }
                if !text.is_empty() {
                    messages.push(json!({"role":"user","content":text}));
                }
            }
            Role::Assistant => {
                let calls: Vec<Value> = message
                    .parts
                    .iter()
                    .filter_map(|part| match part {
                        Part::ToolCall { id, name, arguments } => Some(json!({
                            "id": id, "type": "function",
                            "function": {"name": name, "arguments": arguments},
                        })),
                        _ => None,
                    })
                    .collect();
                let mut entry = json!({"role":"assistant","content":text});
                if !calls.is_empty() {
                    entry["tool_calls"] = json!(calls);
                }
                messages.push(entry);
            }
        }
    }
    let tools: Vec<Value> = request
        .tools
        .iter()
        .map(|tool| json!({"type":"function","function":{
            "name": tool.name, "description": tool.description, "parameters": tool.parameters,
        }}))
        .collect();
    let mut environment = minijinja::Environment::new();
    minijinja_contrib::add_to_environment(&mut environment);
    environment.set_unknown_method_callback(minijinja_contrib::pycompat::unknown_method_callback);
    environment.add_function("raise_exception", |message: String| -> Result<String, minijinja::Error> {
        Err(minijinja::Error::new(minijinja::ErrorKind::InvalidOperation, message))
    });
    environment.add_function("strftime_now", |format: String| {
        let _ = format;
        String::new()
    });
    environment
        .add_template("chat", template)
        .map_err(|error| error.to_string())?;
    let mut context = json!({
        "messages": messages,
        "add_generation_prompt": generation_prompt,
        "enable_thinking": thinking,
        "bos_token": bos,
        "eos_token": eos,
    });
    if !tools.is_empty() {
        context["tools"] = json!(tools);
    }
    environment
        .get_template("chat")
        .and_then(|template| template.render(minijinja::Value::from_serialize(&context)))
        .map_err(|error| error.to_string())
}

fn parse_spark_tool_calls(text: &str) -> Vec<Part> {
    let mut calls = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(TOOL_OPEN) {
        rest = &rest[start + TOOL_OPEN.len()..];
        let Some(end) = rest.find(TOOL_CLOSE) else {
            break;
        };
        let call = &rest[..end];
        rest = &rest[end + TOOL_CLOSE.len()..];
        let name_end = call.find(ARG_KEY_OPEN).unwrap_or(call.len());
        let name = call[..name_end].trim();
        if name.is_empty() {
            continue;
        }
        let mut args = Map::new();
        let mut args_rest = &call[name_end..];
        loop {
            let Some(key_start) = args_rest.find(ARG_KEY_OPEN) else {
                break;
            };
            args_rest = &args_rest[key_start + ARG_KEY_OPEN.len()..];
            let Some(key_end) = args_rest.find(ARG_KEY_CLOSE) else {
                break;
            };
            let key = args_rest[..key_end].trim().to_owned();
            args_rest = &args_rest[key_end + ARG_KEY_CLOSE.len()..];
            let Some(value_start) = args_rest.find(ARG_VALUE_OPEN) else {
                break;
            };
            args_rest = &args_rest[value_start + ARG_VALUE_OPEN.len()..];
            let Some(value_end) = args_rest.find(ARG_VALUE_CLOSE) else {
                break;
            };
            let raw = args_rest[..value_end].trim();
            let value = serde_json::from_str(raw).unwrap_or_else(|_| Value::String(raw.to_owned()));
            args.insert(key, value);
            args_rest = &args_rest[value_end + ARG_VALUE_CLOSE.len()..];
        }
        calls.push(Part::ToolCall {
            id: format!("loom-call-{}", calls.len() + 1),
            name: name.to_owned(),
            arguments: Value::Object(args),
        });
    }
    calls
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed Qwen-style template: system/tools header, tool calls and tool results.
    const QWEN_LIKE: &str = r#"{%- if tools %}<|im_start|>system
{% if messages[0].role == 'system' %}{{ messages[0].content }}

{% endif %}# Tools
<tools>{% for tool in tools %}
{{ tool | tojson }}{% endfor %}
</tools><|im_end|>
{% elif messages[0].role == 'system' %}<|im_start|>system
{{ messages[0].content }}<|im_end|>
{% endif %}{%- for message in messages %}{%- if message.role == 'user' %}<|im_start|>user
{{ message.content }}<|im_end|>
{% elif message.role == 'assistant' %}<|im_start|>assistant
{{ message.content }}{%- for call in message.tool_calls or [] %}<tool_call>
{"name": "{{ call.function.name }}", "arguments": {{ call.function.arguments | tojson }}}
</tool_call>{%- endfor %}<|im_end|>
{% elif message.role == 'tool' %}<|im_start|>user
<tool_response>
{{ message.content }}
</tool_response><|im_end|>
{% endif %}{%- endfor %}{%- if add_generation_prompt %}<|im_start|>assistant
{% if enable_thinking is defined and enable_thinking is false %}<think>

</think>

{% endif %}{% endif %}"#;

    #[test]
    fn embedded_chat_templates_render_tools_calls_and_results() {
        let request = Request {
            system: Some("work carefully".into()),
            tools: vec![ToolSpec {
                name: "read".into(),
                description: "read a file".into(),
                parameters: json!({"type":"object"}),
            }],
            messages: vec![
                Message::user("inspect it"),
                Message {
                    role: Role::Assistant,
                    parts: vec![Part::ToolCall { id: "c1".into(), name: "read".into(), arguments: json!({"path":"a.rs"}) }],
                },
                Message {
                    role: Role::User,
                    parts: vec![Part::ToolResult { call_id: "c1".into(), output: "fn a() {}".into(), is_error: false }],
                },
            ],
            ..Request::default()
        };
        let prompt = render_template_prompt(QWEN_LIKE, &request, false, "", "").unwrap();
        assert!(prompt.starts_with("<|im_start|>system\nwork carefully"), "{prompt}");
        assert!(prompt.contains(r#""name":"read""#), "{prompt}");
        assert!(prompt.contains(r#"{"name": "read", "arguments": {"path":"a.rs"}}"#), "{prompt}");
        assert!(prompt.contains("<tool_response>\nfn a() {}\n</tool_response>"), "{prompt}");
        assert!(prompt.ends_with("<|im_start|>assistant\n<think>\n\n</think>\n\n"), "{prompt}");
        let thinking = render_template_prompt(QWEN_LIKE, &request, true, "", "").unwrap();
        assert!(thinking.ends_with("<|im_start|>assistant\n"), "{thinking}");
    }

    #[test]
    fn python_reverse_slices_in_templates_render() {
        let template = "{%- for m in messages[::-1] %}{{ m.content }}{% endfor %}|{{ (messages[::-1])[0].content }}";
        let request = Request { messages: vec![Message::user("a"), Message::user("b")], ..Request::default() };
        assert_eq!(render_template_prompt(template, &request, false, "", "").unwrap(), "ba|b");
    }

    #[test]
    fn json_tool_calls_and_an_opening_think_block_are_understood() {
        let mut output = SparkOutput::new(false);
        let mut shown = String::new();
        for piece in ["<thi", "nk>plan quietly</think>\n\nReading.", "<tool_call>\n{\"name\": \"read\", ", "\"arguments\": {\"path\": \"a.rs\"}}\n</tool_call>"] {
            shown.extend(output.feed(piece));
        }
        shown.extend(output.finish());
        assert_eq!(shown.trim(), "Reading.");
        let calls = parse_tool_calls(&output.tools);
        assert!(matches!(&calls[..], [Part::ToolCall { name, arguments, .. }]
            if name == "read" && arguments == &json!({"path":"a.rs"})), "{calls:?}");
        let xml = parse_tool_calls("<tool_call>\n<function=read>\n<parameter=path>\nsrc/lib.rs\n</parameter>\n<parameter=limit>\n40\n</parameter>\n</function>\n</tool_call>");
        assert!(matches!(&xml[..], [Part::ToolCall { name, arguments, .. }]
            if name == "read" && arguments == &json!({"path":"src/lib.rs","limit":40})), "{xml:?}");
        // Spark's own argument tags still parse.
        let spark = parse_tool_calls("<tool_call>read<arg_key>path</arg_key><arg_value>a.rs</arg_value></tool_call>");
        assert!(matches!(&spark[..], [Part::ToolCall { name, .. }] if name == "read"));
    }

    #[test]
    fn coder_bare_calls_stream_without_leaking_markup_and_keep_schema_types() {
        let tools = vec![ToolSpec {
            name: "read".into(), description: String::new(),
            parameters: json!({"properties":{"path":{"type":"string"},"limit":{"type":"integer"}}}),
        }];
        let text = "확인하겠습니다.\n<function=read>\n<parameter=path>\n123\n</parameter>\n<parameter=limit>\n40\n</parameter>\n</function>\n";
        // Every possible token boundary, including inside the opening marker.
        for boundary in text.char_indices().map(|(i, _)| i).chain([text.len()]) {
            let mut output = SparkOutput::new(false).with_xml_tools(&tools);
            let mut shown: String = output.feed(&text[..boundary]).concat();
            shown.extend(output.feed(&text[boundary..]));
            shown.extend(output.finish());
            assert_eq!(shown, "확인하겠습니다.\n", "boundary {boundary}");
            let calls = parse_tool_calls_for_tools(&output.tools, &tools);
            assert!(matches!(&calls[..], [Part::ToolCall {name, arguments, ..}]
                if name == "read" && arguments == &json!({"path":"123","limit":40})), "{calls:?}");
        }
    }

    #[test]
    fn coder_bare_recovery_preserves_examples_and_unknown_functions() {
        let tools = vec![ToolSpec {name:"read".into(),description:String::new(),parameters:json!({})}];
        for text in [
            "```xml\n<function=read>\n</function>\n```",
            "~~~xml\n<function=read>\n</function>\n~~~",
            "Example: <function=read></function>",
            "<function=unknown></function>",
            "<function=rea",
        ] {
            let mut output = SparkOutput::new(false).with_xml_tools(&tools);
            let mut shown = String::new();
            for ch in text.chars() { shown.extend(output.feed(&ch.to_string())); }
            shown.extend(output.finish());
            assert_eq!(shown, text);
            assert!(output.tools.is_empty());
        }
    }

    /// `LOOM_NATIVE_SMOKE=<model id> cargo test --release -- --ignored native_smoke --nocapture`
    #[test]
    #[ignore]
    fn native_smoke_answers_streams_and_calls_a_tool() {
        let id = std::env::var("LOOM_NATIVE_SMOKE").expect("set LOOM_NATIVE_SMOKE to an installed model id");
        let provider = NativeProvider::discover(Backend::Auto);
        assert!(provider.models().iter().any(|m| m.id == id), "not discovered: {:?}", provider.models().iter().map(|m| &m.id).collect::<Vec<_>>());
        let tools = vec![ToolSpec {
            name: "read".into(),
            description: "Read a file of the repository".into(),
            parameters: json!({"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}),
        }];
        for turn in 0..2 {
            let request = Request {
                model: id.clone(),
                system: Some("You are a coding agent. Use tools to inspect files.".into()),
                tools: tools.clone(),
                messages: vec![Message::user("Read the file src/lib.rs.")],
                max_output_tokens: Some(512),
                reasoning_effort: Some("none".into()),
                ..Request::default()
            };
            let started = Instant::now();
            let mut deltas = 0;
            let response = provider.complete_stream(&request, &mut |event| {
                if matches!(event, StreamEvent::TextDelta(_)) { deltas += 1; }
            }).unwrap();
            eprintln!("turn {turn}: {:?} in {:?}, {deltas} deltas, usage {:?}, parts {:?}", response.stop, started.elapsed(), response.usage, response.parts);
            assert!(response.tool_calls().any(|(_, name, _)| name == "read"), "{:?}", response.parts);
        }
    }

    #[test]
    fn loaded_weights_are_reused_until_the_file_or_backend_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("m.gguf");
        std::fs::write(&path, b"weights").unwrap();
        let first = WeightsKey::of(&path, Backend::Mps);
        assert_eq!(first, WeightsKey::of(&path, Backend::Mps), "a rediscovered provider must reuse the load");
        assert_ne!(first, WeightsKey::of(&path, Backend::Cpu));
        std::fs::write(&path, b"re-downloaded weights").unwrap();
        assert_ne!(first, WeightsKey::of(&path, Backend::Mps));
    }

    #[test]
    fn ollama_manifest_paths_become_ollama_names() {
        for (path, name) in [
            ("registry.ollama.ai/library/qwen3/8b", Some("qwen3:8b")),
            ("registry.ollama.ai/SparkLLM/Spark-X2.5-4B/latest", Some("SparkLLM/Spark-X2.5-4B:latest")),
            ("hf.co/owner/repo/Q4_K_M", Some("hf.co/owner/repo:Q4_K_M")),
            ("latest", None),
        ] {
            assert_eq!(ollama_name(Path::new(path)).as_deref(), name, "{path}");
        }
    }

    #[test]
    fn backend_names_are_product_names() {
        assert_eq!(Backend::parse("metal"), Some(Backend::Mps));
        assert_eq!(Backend::parse("mps"), Some(Backend::Mps));
        assert_eq!(Backend::parse("CUDA"), Some(Backend::Cuda));
        assert_eq!(Backend::parse("cpu"), Some(Backend::Cpu));
        assert_eq!(Backend::parse("wat"), None);
    }

    #[test]
    fn spark_prompt_contains_tools_history_and_tool_results() {
        let request = Request {
            system: Some("work carefully".into()),
            tools: vec![ToolSpec {
                name: "read".into(),
                description: "read a file".into(),
                parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
            }],
            messages: vec![
                Message::user("inspect it"),
                Message {
                    role: Role::Assistant,
                    parts: vec![Part::ToolCall {
                        id: "old".into(),
                        name: "read".into(),
                        arguments: json!({"path":"src/lib.rs"}),
                    }],
                },
                Message {
                    role: Role::User,
                    parts: vec![Part::ToolResult {
                        call_id: "old".into(),
                        output: "source".into(),
                        is_error: false,
                    }],
                },
            ],
            ..Request::default()
        };
        let prompt = render_spark_prompt(&request, false);
        assert!(prompt.contains("## Tools"));
        assert!(prompt.contains("\"name\":\"read\""));
        assert!(prompt.contains("<tool_call>read<arg_key>path</arg_key><arg_value>src/lib.rs</arg_value>"));
        assert!(prompt.contains("<tool_response>source</tool_response>"));
        assert!(prompt.ends_with("<|Bot|></think>"));
    }

    #[test]
    fn spark_output_hides_reasoning_and_parses_tool_calls() {
        let mut output = SparkOutput::new(true);
        assert!(output.feed("private rea").is_empty());
        assert_eq!(output.feed("soning</thi"), Vec::<String>::new());
        assert_eq!(output.feed("nk>hello <tool_"), vec!["hello ".to_string()]);
        assert!(output.feed("call>edit<arg_key>path</arg_key><arg_value>\"a.rs\"</arg_value>").is_empty());
        assert!(output.feed("<arg_key>line</arg_key><arg_value>3</arg_value></tool_call>").is_empty());
        let _ = output.finish();
        let calls = parse_spark_tool_calls(&output.tools);
        assert_eq!(output.visible, "hello ");
        assert_eq!(calls.len(), 1);
        let Part::ToolCall { name, arguments, .. } = &calls[0] else { panic!() };
        assert_eq!(name, "edit");
        assert_eq!(arguments["path"], "a.rs");
        assert_eq!(arguments["line"], 3);
    }
}
