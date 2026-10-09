//! MLX checkpoints (safetensors from `mlx-community` and similar) run in-process on Apple
//! silicon. The forward pass lives in `native/mlx/engine.cpp`; this side owns discovery,
//! tokenization, the chat template, prefix reuse and parsing, shared with the GGUF engine.
use super::native_runtime::{
    SparkOutput, THINK_OPEN, check_limits, render_template_prompt,
};
use super::*;
use crate::base::resources::{
    AdmissionError, ResourceGovernor, ResourceKind, ResourceLease, local_resources,
};
use serde_json::Value;
use std::{
    ffi::{CString, c_char, c_void},
    path::{Path, PathBuf},
    sync::{Arc, Mutex, TryLockError},
    time::{Duration, Instant},
};

#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct EngineConfig {
    hidden: i32,
    layers: i32,
    heads: i32,
    kv_heads: i32,
    head_dim: i32,
    eps: f32,
    rope_theta: f32,
    tied_embeddings: i32,
    qk_norm: i32,
    group_size: i32,
    bits: i32,
    prefill_chunk: i32,
    kv_bits: i32,
    kv_group: i32,
    kv_rotate: i32,
    experts: i32,
    experts_per_token: i32,
    norm_topk_prob: i32,
}

unsafe extern "C" {
    fn loom_mlx_prefill(
        handle: *mut c_void,
        tokens: *const i32,
        count: usize,
        keep: usize,
        error: *mut c_char,
        capacity: usize,
    ) -> i32;
    fn loom_mlx_reset(handle: *mut c_void);
    fn loom_mlx_set_sampling(handle: *mut c_void, top_k: i32, top_p: f32, repetition: f32);
    fn loom_mlx_clear_cache();
    fn loom_mlx_load(
        files: *const c_char,
        config: *const EngineConfig,
        error: *mut c_char,
        capacity: usize,
    ) -> *mut c_void;
    #[allow(clippy::too_many_arguments)]
    fn loom_mlx_next(
        handle: *mut c_void,
        tokens: *const i32,
        count: usize,
        keep: usize,
        temperature: f32,
        seed: u64,
        next: *mut i32,
        error: *mut c_char,
        capacity: usize,
    ) -> i32;
    #[allow(clippy::too_many_arguments)]
    fn loom_mlx_verify(
        handle: *mut c_void,
        tokens: *const i32,
        count: usize,
        keep: usize,
        temperature: f32,
        seed: u64,
        next: *mut i32,
        error: *mut c_char,
        capacity: usize,
    ) -> i32;
    fn loom_mlx_available() -> i32;
    fn loom_mlx_active_memory() -> usize;
    fn loom_mlx_peak_memory() -> usize;
    fn loom_mlx_cache_memory() -> usize;
    fn loom_mlx_reset_peak_memory();
    fn loom_mlx_free(handle: *mut c_void);
    fn loom_mlx_weights_new() -> *mut c_void;
    fn loom_mlx_weights_free(weights: *mut c_void);
    #[allow(clippy::too_many_arguments)]
    fn loom_mlx_weights_alloc(
        weights: *mut c_void,
        name: *const c_char,
        dtype: i32,
        shape: *const i64,
        ndim: usize,
        bytes: usize,
        error: *mut c_char,
        capacity: usize,
    ) -> *mut c_void;
    fn loom_mlx_load_weights(
        weights: *mut c_void,
        config: *const EngineConfig,
        error: *mut c_char,
        capacity: usize,
    ) -> *mut c_void;
    fn loom_mlx_set_cache_limit(bytes: usize);
}

fn engine_error(buffer: &[u8]) -> String {
    let end = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..end]).into_owned()
}

/// Whether Loom's MLX runtime has a usable GPU device.
pub(super) fn gpu_available() -> bool {
    unsafe { loom_mlx_available() != 0 }
}

/// Bytes MLX holds in GPU buffers right now and at most since start.
pub fn engine_memory() -> (u64, u64) {
    unsafe {
        (
            loom_mlx_active_memory() as u64,
            loom_mlx_peak_memory() as u64,
        )
    }
}

/// An MLX checkpoint directory Loom can run.
#[derive(Clone, Debug)]
pub struct MlxModel {
    pub id: String,
    pub dir: PathBuf,
    config: EngineConfig,
    weights: Vec<PathBuf>,
    lmx: Option<(PathBuf, Vec<super::lmx::Entry>)>,
    /// GGUF blocks inside an LMX, repacked as MLX affine groups while loading.
    gguf: Option<Arc<GgufSource>>,
    eos: Vec<u32>,
    context: usize,
    intermediate: usize,
    vocab: usize,
    sampling: Sampling,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct Sampling {
    temperature: f32,
    top_k: i32,
    top_p: f32,
    repetition: f32,
}

impl Default for Sampling {
    fn default() -> Self { Self { temperature: 0.2, top_k: 0, top_p: 1.0, repetition: 1.0 } }
}

impl Sampling {
    fn from_generation(value: &Value) -> Option<Self> {
        let defaults = Self::default();
        let result = Self {
            temperature: if value["do_sample"] == false { 0.0 } else {
                value.get("temperature").map_or(Some(defaults.temperature), |v| v.as_f64().map(|n| n as f32))?
            },
            top_k: value.get("top_k").map_or(Some(0), |v| v.as_i64().and_then(|n| i32::try_from(n).ok()))?,
            top_p: value.get("top_p").map_or(Some(1.0), |v| v.as_f64().map(|n| n as f32))?,
            repetition: value.get("repetition_penalty").map_or(Some(1.0), |v| v.as_f64().map(|n| n as f32))?,
        };
        (result.temperature.is_finite() && result.temperature >= 0.0 && result.top_k >= 0
            && result.top_p.is_finite() && result.top_p > 0.0 && result.top_p <= 1.0
            && result.repetition.is_finite() && result.repetition > 0.0).then_some(result)
    }

    fn for_request(self, request: &Request) -> Result<Self, ProviderError> {
        let temperature = request.temperature.unwrap_or(self.temperature);
        if !temperature.is_finite() || temperature < 0.0 { return Err(bad("invalid sampling temperature")); }
        // Explicit greedy requests retain the unfiltered argmax path used for parity.
        Ok(if temperature == 0.0 { Self { temperature, ..Self::default() } } else { Self { temperature, ..self } })
    }
}

impl MlxModel {
    /// Architectures implemented by the native engine; unsupported variants are skipped.
    fn inspect(dir: &Path, id: String) -> Option<Self> {
        let config: Value =
            serde_json::from_slice(&std::fs::read(dir.join("config.json")).ok()?).ok()?;
        if !dir.join("tokenizer.json").is_file() {
            return None;
        }
        let mut weights: Vec<PathBuf> = std::fs::read_dir(dir)
            .ok()?
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "safetensors"))
            .collect();
        weights.sort();
        Self::from_config(dir,id,config,weights)
    }

    fn from_config(dir: &Path,id: String,config:Value,weights:Vec<PathBuf>)->Option<Self>{
        super::model_package::validate_mlx_config(&config).ok()?;
        let kind = config["model_type"].as_str()?;
        if weights.is_empty() {
            return None;
        }
        let number = |key: &str| config[key].as_i64().map(|value| value as i32);
        let hidden = number("hidden_size")?;
        let heads = number("num_attention_heads")?;
        let layers = number("num_hidden_layers")?;
        let kv_heads = number("num_key_value_heads").unwrap_or(heads);
        let head_dim = number("head_dim").unwrap_or(hidden / heads);
        let quantization = super::model_package::quantization(&config);
        let group_size = quantization["group_size"].as_i64().unwrap_or(64) as i32;
        let bits = quantization["bits"].as_i64().unwrap_or(0) as i32;
        let experts = if kind == "qwen3_moe" {
            number("num_experts")?
        } else {
            0
        };
        let experts_per_token = if experts > 0 {
            number("num_experts_per_tok")?
        } else {
            0
        };
        let mut eos: Vec<u32> = match &config["eos_token_id"] {
            Value::Number(id) => id.as_u64().map(|id| vec![id as u32]).unwrap_or_default(),
            Value::Array(ids) => ids
                .iter()
                .filter_map(Value::as_u64)
                .map(|id| id as u32)
                .collect(),
            _ => Vec::new(),
        };
        let generation = match std::fs::read(dir.join("generation_config.json")) {
            Ok(bytes) => serde_json::from_slice::<Value>(&bytes).ok()?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => serde_json::json!({}),
            Err(_) => return None,
        };
        let sampling = Sampling::from_generation(&generation)?;
        {
            let tokens = &generation;
            match &tokens["eos_token_id"] {
                Value::Number(id) => eos.extend(id.as_u64().map(|id| id as u32)),
                Value::Array(ids) => {
                    eos.extend(ids.iter().filter_map(Value::as_u64).map(|id| id as u32))
                }
                _ => {}
            }
        }
        Some(Self {
            id,
            dir: dir.to_owned(),
            config: EngineConfig {
                hidden,
                layers,
                heads,
                kv_heads,
                head_dim,
                eps: config["rms_norm_eps"].as_f64().unwrap_or(1e-6) as f32,
                rope_theta: config["rope_theta"].as_f64().unwrap_or(10_000.0) as f32,
                tied_embeddings: i32::from(
                    config["tie_word_embeddings"].as_bool().unwrap_or(false),
                ),
                qk_norm: i32::from(matches!(kind, "qwen3" | "qwen3_moe")),
                group_size,
                bits,
                // Bound prefill activations, including tokens × top-k for MoE.
                prefill_chunk: 128,
                kv_bits: 0,
                kv_group: 64,
                kv_rotate: 0,
                experts,
                experts_per_token,
                norm_topk_prob: i32::from(config["norm_topk_prob"].as_bool().unwrap_or(false)),
            },
            weights,
            lmx: None,
            gguf: None,
            eos,
            context: config["max_position_embeddings"].as_u64().unwrap_or(32_768) as usize,
            intermediate: config["moe_intermediate_size"]
                .as_u64()
                .or_else(|| config["intermediate_size"].as_u64())
                .unwrap_or(hidden as u64 * 4) as usize,
            vocab: config["vocab_size"].as_u64().unwrap_or(160_000) as usize,
            sampling,
        })
    }

    pub(crate) fn from_lmx(executable: &super::lmx::LmxFile) -> anyhow::Result<Self> {
        if let super::lmx::Payload::Gguf { .. } = &executable.manifest.payload {
            return Self::from_gguf(executable);
        }
        let super::lmx::Payload::MlxAffine { model } = &executable.manifest.payload else { anyhow::bail!("LMX encoding is not MLX affine") };
        let directory = executable.metadata_dir()?;
        let mut result = Self::from_config(&directory,executable.id(),model.config.clone(),vec![executable.path.clone()])
            .ok_or_else(||anyhow::anyhow!("unsupported LMX MLX configuration"))?;
        result.lmx = Some((executable.path.clone(),executable.manifest.entries.iter().filter(|entry| entry.name.ends_with(".safetensors")).cloned().collect()));
        Ok(result)
    }

    /// GGUF quantized blocks run as MLX affine groups; see `gguf_mlx`.
    fn from_gguf(executable: &super::lmx::LmxFile) -> anyhow::Result<Self> {
        use super::lmx_ir::Storage;
        let gguf = super::gguf_mlx::read_config(&executable.path)?;
        anyhow::ensure!(
            super::gguf_mlx::ARCHITECTURES.contains(&gguf.architecture.as_str()),
            "MLX does not run GGUF architecture {}",
            gguf.architecture
        );
        let mut tensors = Vec::new();
        for (name, tensor) in &executable.ir.tensors {
            let entry = executable
                .manifest
                .entries
                .iter()
                .find(|entry| entry.name == tensor.entry)
                .ok_or_else(|| anyhow::anyhow!("{name}: unknown payload entry"))?;
            let kind = match &tensor.storage {
                Storage::GgmlBlock { type_name, .. } => type_name.clone(),
                Storage::Dense { dtype } => dtype.to_ascii_lowercase(),
                other => anyhow::bail!("{name}: unexpected {} in a GGUF payload", other.label()),
            };
            if let Some(reason) = super::gguf_mlx::unsupported(&kind) {
                anyhow::bail!("{name}: {reason}");
            }
            tensors.push((name.clone(), kind, tensor.shape.clone(), entry.offset + tensor.offset, tensor.bytes));
        }
        let vocab = executable.ir.tensors.get("token_embd.weight").map_or(0, |t| t.shape[0] as usize);
        let tied = !executable.ir.tensors.contains_key("output.weight");
        Ok(Self {
            id: executable.id(),
            dir: executable.path.parent().map(Path::to_owned).unwrap_or_default(),
            config: EngineConfig {
                hidden: gguf.hidden,
                layers: gguf.layers,
                heads: gguf.heads,
                kv_heads: gguf.kv_heads,
                head_dim: gguf.head_dim,
                eps: gguf.eps,
                rope_theta: gguf.rope_theta,
                tied_embeddings: i32::from(tied),
                qk_norm: i32::from(gguf.architecture == "qwen3"),
                group_size: super::gguf_mlx::GROUP as i32,
                bits: 0,
                prefill_chunk: 128,
                kv_bits: 0,
                kv_group: 64,
                kv_rotate: 0,
                experts: 0,
                experts_per_token: 0,
                norm_topk_prob: 0,
            },
            weights: vec![executable.path.clone()],
            lmx: None,
            gguf: Some(Arc::new(GgufSource { path: executable.path.clone(), template: gguf.template, tensors })),
            // End-of-generation tokens come from the GGUF vocabulary once it loads.
            eos: Vec::new(),
            context: gguf.context,
            intermediate: gguf.intermediate,
            vocab,
            sampling: Sampling::default(),
        })
    }

    fn weight_bytes(&self)->std::io::Result<u64>{
        if let Some(gguf) = &self.gguf { return Ok(gguf.converted_bytes()) }
        if let Some((_,entries))=&self.lmx {return Ok(entries.iter().map(|e|e.bytes).sum())}
        self.weights.iter().try_fold(0u64,|sum,path|std::fs::metadata(path).map(|meta|sum.saturating_add(meta.len())))
    }

    fn native_files(&self)->String {
        if let Some((path,entries))=&self.lmx {
            return entries.iter().map(|entry|format!("lmx:{}:{}:{}",entry.offset,entry.bytes,path.to_string_lossy())).collect::<Vec<_>>().join("\n")
        }
        self.weights.iter().map(|path|path.to_string_lossy()).collect::<Vec<_>>().join("\n")
    }
}

/// A GGUF payload inside an LMX file. Everything the engine needs comes from that one
/// file: tensors, configuration, vocabulary and chat template.
#[derive(Debug)]
struct GgufSource {
    path: PathBuf,
    template: String,
    /// (GGUF name, ggml type, shape outermost first, file offset, bytes)
    tensors: Vec<(String, String, Vec<u64>, u64, u64)>,
}

impl GgufSource {
    fn converted_bytes(&self) -> u64 {
        self.tensors
            .iter()
            .map(|(_, kind, shape, ..)| super::gguf_mlx::converted_bytes(kind, shape))
            .sum()
    }

    /// Stream each tensor into its final MLX buffers with bounded conversion scratch.
    fn load(&self, config: &EngineConfig) -> Result<*mut c_void, String> {
        use std::io::{Read, Seek, SeekFrom};
        struct Pending(*mut c_void);
        impl Drop for Pending {
            fn drop(&mut self) {
                if !self.0.is_null() {
                    unsafe { loom_mlx_weights_free(self.0) };
                }
            }
        }
        let mut file = std::fs::File::open(&self.path).map_err(|error| error.to_string())?;
        let mut pending = Pending(unsafe { loom_mlx_weights_new() });
        let mut error = [0u8; 1024];
        for (name, kind, shape, offset, bytes) in &self.tensors {
            let layout = super::gguf_mlx::layout(name, kind, shape, *bytes)
                .map_err(|error| error.to_string())?;
            let mut outputs = Vec::with_capacity(layout.len());
            for tensor in &layout {
                let label = CString::new(tensor.name.as_str()).map_err(|_| "tensor name contains NUL")?;
                let data = unsafe {
                    loom_mlx_weights_alloc(
                        pending.0,
                        label.as_ptr(),
                        tensor.dtype as i32,
                        tensor.shape.as_ptr(),
                        tensor.shape.len(),
                        tensor.bytes,
                        error.as_mut_ptr().cast(),
                        error.len(),
                    )
                };
                if data.is_null() {
                    return Err(engine_error(&error));
                }
                // Each fresh named allocation is disjoint and remains owned by `pending`.
                // No MLX operation reads these buffers before conversion has finished.
                outputs.push(unsafe { std::slice::from_raw_parts_mut(data.cast::<u8>(), tensor.bytes) });
            }
            file.seek(SeekFrom::Start(*offset)).map_err(|error| format!("{name}: {error}"))?;
            super::gguf_mlx::convert_into(kind, &mut (&mut file).take(*bytes), &mut outputs)
                .map_err(|error| format!("{name}: {error}"))?;
        }
        let weights = std::mem::replace(&mut pending.0, std::ptr::null_mut());
        let handle = unsafe { loom_mlx_load_weights(weights, config, error.as_mut_ptr().cast(), error.len()) };
        if handle.is_null() {
            return Err(engine_error(&error));
        }
        Ok(handle)
    }
}

/// Text ↔ tokens for a loaded model: a Hugging Face tokenizer beside MLX checkpoints, or
/// the vocabulary of the GGUF inside an LMX (llama.cpp loads it without any weights).
enum Vocab {
    Hf(tokenizers::Tokenizer),
    Gguf {
        model: llama_cpp_2::model::LlamaModel,
        eog: Vec<u32>,
    },
}

impl Vocab {
    fn gguf(path: &Path) -> Result<Self, ProviderError> {
        let backend = super::native_runtime::llama_backend()?;
        let params = llama_cpp_2::model::params::LlamaModelParams::default().with_vocab_only(true);
        let model = llama_cpp_2::model::LlamaModel::load_from_file(backend, path, &params)
            .map_err(|error| bad(format!("GGUF vocabulary: {error}")))?;
        let vocab = model.vocab();
        let eog = vocab.tokens().filter(|&token| vocab.is_eog(token)).map(|token| token.0 as u32).collect();
        Ok(Self::Gguf { model, eog })
    }

    fn encode(&self, text: &str) -> Result<Vec<u32>, ProviderError> {
        match self {
            Self::Hf(tokenizer) => Ok(tokenizer
                .encode(text, false)
                .map_err(|error| bad(format!("tokenize: {error}")))?
                .get_ids()
                .to_vec()),
            // Templates spell special tokens out; parse them as tokens, add nothing.
            Self::Gguf { model, .. } => Ok(model
                .vocab()
                .tokenize(text.as_bytes(), false, true)
                .into_iter()
                .map(|token| token.0 as u32)
                .collect()),
        }
    }

    fn decode(&self, tokens: &[u32]) -> String {
        match self {
            Self::Hf(tokenizer) => tokenizer.decode(tokens, true).unwrap_or_default(),
            Self::Gguf { model, .. } => {
                let tokens: Vec<_> = tokens.iter().map(|&t| llama_cpp_2::token::LlamaToken(t as i32)).collect();
                String::from_utf8_lossy(&model.vocab().detokenize(&tokens, true, false)).into_owned()
            }
        }
    }

    fn is_eog(&self, token: u32) -> bool {
        matches!(self, Self::Gguf { eog, .. } if eog.contains(&token))
    }

    /// Text of each next token, holding back incomplete UTF-8 sequences.
    fn stream(&self) -> Box<dyn FnMut(u32) -> Result<Option<String>, ProviderError> + '_> {
        match self {
            Self::Hf(tokenizer) => {
                // Tokenizers' DecodeStream retains the minimum prefix needed for multi-byte
                // characters and contextual decoders; re-decoding every token from the
                // start makes long replies quadratic.
                let mut decoder = tokenizer.decode_stream(true);
                Box::new(move |token| decoder.step(token).map_err(|error| bad(format!("detokenize: {error}"))))
            }
            Self::Gguf { model, .. } => {
                let mut decoder = encoding_rs::UTF_8.new_decoder();
                Box::new(move |token| {
                    let bytes = model.vocab().token_to_piece(llama_cpp_2::token::LlamaToken(token as i32), false, None);
                    let mut text = String::with_capacity(bytes.len() * 3 + 4);
                    let _ = decoder.decode_to_string(&bytes, &mut text, false);
                    Ok((!text.is_empty()).then_some(text))
                })
            }
        }
    }
}

/// `~/.loom/models/mlx/<owner>/<repo>` directories; ids are `owner/repo`.
pub fn installed() -> Vec<MlxModel> {
    let root = super::local::models_dir().join("mlx");
    let mut found = Vec::new();
    let mut pending = vec![root.clone()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        if dir.join("config.json").is_file() {
            let id = dir
                .strip_prefix(&root)
                .unwrap_or(&dir)
                .to_string_lossy()
                .replace('\\', "/");
            found.extend(MlxModel::inspect(&dir, id));
            continue;
        }
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                pending.push(entry.path());
            }
        }
    }
    found.sort_by(|a, b| a.id.cmp(&b.id));
    found
}

struct Loaded {
    id: String,
    kv: super::native_runtime::KvPrecision,
    handle: *mut c_void,
    tokenizer: std::sync::Arc<Vocab>,
    template: String,
    /// Tokens whose keys and values the engine's cache holds, in order.
    cached: Vec<u32>,
    resources: Arc<ResourceGovernor>,
    resident: ResourceLease,
    weight_estimate: u64,
    capacity_tokens: usize,
    config: EngineConfig,
    intermediate: usize,
    vocab: usize,
    cache_limit: usize,
    reuse_cache_reserved: u64,
}

// SAFETY: the engine handle is used only under `LOADED`'s mutex, one call at a time.
unsafe impl Send for Loaded {}

impl Drop for Loaded {
    fn drop(&mut self) {
        unsafe { loom_mlx_free(self.handle) };
    }
}

static LOADED: Mutex<Option<Loaded>> = Mutex::new(None);

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

fn loaded_slot() -> std::sync::MutexGuard<'static, Option<Loaded>> {
    LOADED.lock().unwrap_or_else(|poisoned| {
        let mut slot = poisoned.into_inner();
        *slot = None;
        slot
    })
}

type Limits<'a> = Option<(&'a Request, Instant)>;

fn admission_check(limits: Limits<'_>) -> Result<(), AdmissionError> {
    if let Some((request, started)) = limits {
        if request
            .cancel
            .as_ref()
            .is_some_and(|flag| flag.load(std::sync::atomic::Ordering::Acquire))
        {
            return Err(AdmissionError::Cancelled);
        }
        if request
            .timeout
            .is_some_and(|limit| started.elapsed() >= limit)
        {
            return Err(AdmissionError::TimedOut);
        }
    }
    Ok(())
}

fn admission_error(error: AdmissionError) -> ProviderError {
    let kind = match error {
        AdmissionError::Cancelled => ErrorKind::Cancelled,
        AdmissionError::TimedOut => ErrorKind::TimedOut,
        AdmissionError::TooLarge { .. } => ErrorKind::BadRequest,
    };
    let mut error = ProviderError::new(kind, error.to_string());
    error.code = Some(LOCAL_RESOURCE_ADMISSION.into());
    error
}

fn request_slot(
    request: &Request,
    started: Instant,
) -> Result<std::sync::MutexGuard<'static, Option<Loaded>>, ProviderError> {
    loop {
        admission_check(Some((request, started))).map_err(admission_error)?;
        match LOADED.try_lock() {
            Ok(slot) => return Ok(slot),
            Err(TryLockError::Poisoned(error)) => {
                let mut slot = error.into_inner();
                *slot = None;
                return Ok(slot);
            }
            Err(TryLockError::WouldBlock) => std::thread::sleep(Duration::from_millis(10)),
        }
    }
}

fn bad(message: impl Into<String>) -> ProviderError {
    ProviderError::new(ErrorKind::BadRequest, message)
}

/// (bits, rotated) of the engine's KV cache for a precision.
fn kv_layout(kv: super::native_runtime::KvPrecision) -> (i32, i32) {
    use super::native_runtime::KvPrecision::*;
    match kv {
        F16 => (0, 0),
        Q8 => (8, 0),
        Q4 => (4, 0),
        Q3 => (3, 0),
        Turbo4 => (4, 1),
        Turbo3 => (3, 1),
    }
}

fn ensure_loaded<'a>(
    slot: &'a mut Option<Loaded>,
    model: &MlxModel,
    kv: super::native_runtime::KvPrecision,
    resources: &Arc<ResourceGovernor>,
    limits: Limits<'_>,
) -> Result<&'a mut Loaded, ProviderError> {
    resources.set_reclaimer(Arc::new(reclaim_idle_mlx));
    let mut config = model.config;
    (config.kv_bits, config.kv_rotate) = kv_layout(kv);
    // Prompt tokens per pass: smaller chunks lower the transient peak, larger ones run faster.
    config.prefill_chunk = std::env::var("LOOM_MLX_PREFILL")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|chunk: &i32| *chunk > 0)
        .unwrap_or(config.prefill_chunk);
    if slot.as_ref().is_some_and(|loaded| {
        loaded.id == model.id && loaded.kv == kv && loaded.resident.belongs_to(resources)
    }) {
        return Ok(slot.as_mut().expect("checked above"));
    }
    *slot = None;
    // One local model in memory at a time: drop the native LMX engine's if it is idle.
    super::lmx_native_runtime::release_idle();
    // Bound reusable buffers before loading/fusing weights, not after the peak.
    static AT_EXIT: std::sync::Once = std::sync::Once::new();
    AT_EXIT.call_once(|| unsafe {
        libc::atexit(release_at_exit);
    });
    let cache_mb = std::env::var("LOOM_MLX_CACHE_MB")
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(64);
    let cache_limit = cache_mb.saturating_mul(1 << 20);
    let file_bytes = model.weight_bytes()
        .map_err(|error| bad(format!("weight files of {}: {error}", model.id)))?;
    // Includes a conservative packing/metadata margin and CPU tokenizer/runtime
    // allowance. MLX allocator observations after each pass can raise it.
    let weight_estimate = file_bytes
        .saturating_add(file_bytes / 10)
        .saturating_add(128 << 20);
    let mut resident = resources
        .acquire(ResourceKind::Resident, 0, || admission_check(limits))
        .map_err(admission_error)?;
    let mut loading = resources
        .acquire(
            ResourceKind::Inference,
            weight_estimate
                .saturating_add(cache_limit as u64)
                .saturating_add(256 << 20),
            || admission_check(limits),
        )
        .map_err(admission_error)?;
    unsafe {
        loom_mlx_set_cache_limit(cache_limit);
    }
    let (tokenizer, template, handle) = if let Some(gguf) = &model.gguf {
        let vocab = Vocab::gguf(&gguf.path)?;
        let handle = gguf
            .load(&config)
            .map_err(|error| bad(format!("cannot load {}: {error}", model.id)))?;
        (vocab, gguf.template.clone(), handle)
    } else {
        let tokenizer = tokenizers::Tokenizer::from_file(model.dir.join("tokenizer.json"))
            .map_err(|error| bad(format!("tokenizer of {}: {error}", model.id)))?;
        let template = std::fs::read(model.dir.join("tokenizer_config.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|config| config["chat_template"].as_str().map(str::to_owned))
            .or_else(|| std::fs::read_to_string(model.dir.join("chat_template.jinja")).ok())
            .ok_or_else(|| bad(format!("{} has no chat template", model.id)))?;
        let files = model.native_files();
        let files = CString::new(files).map_err(|_| bad("weight path contains NUL"))?;
        let mut error = [0u8; 1024];
        let handle = unsafe {
            loom_mlx_load(
                files.as_ptr(),
                &config,
                error.as_mut_ptr().cast(),
                error.len(),
            )
        };
        if handle.is_null() {
            return Err(bad(format!(
                "cannot load {}: {}",
                model.id,
                engine_error(&error)
            )));
        }
        (Vocab::Hf(tokenizer), template, handle)
    };
    loading.settle(
        &mut resident,
        weight_estimate
            .saturating_add(cache_limit as u64)
            .max(observed_resident()),
    );
    *slot = Some(Loaded {
        id: model.id.clone(),
        kv,
        handle,
        tokenizer: std::sync::Arc::new(tokenizer),
        template,
        cached: Vec::new(),
        resources: Arc::clone(resources),
        resident,
        weight_estimate,
        capacity_tokens: 0,
        config,
        intermediate: model.intermediate,
        vocab: model.vocab,
        cache_limit,
        reuse_cache_reserved: cache_limit as u64,
    });
    Ok(slot.as_mut().expect("just loaded"))
}

/// Prompt lookup decoding: the reply often repeats text already in the conversation
/// (files Core read, tool results, the reply's own earlier lines). Find the latest earlier
/// occurrence of the last few tokens and propose what followed it.
fn lookup_draft(history: &[u32], limit: usize) -> Vec<u32> {
    if limit == 0 {
        return Vec::new();
    }
    for size in (2..=4).rev() {
        if history.len() <= size {
            continue;
        }
        let tail = &history[history.len() - size..];
        for start in (0..history.len() - size).rev() {
            if &history[start..start + size] == tail {
                let from = start + size;
                let end = (from + limit).min(history.len());
                if from < end {
                    return history[from..end].to_vec();
                }
            }
        }
    }
    Vec::new()
}

fn observed_resident() -> u64 {
    // Allocator buffers and a CPU allowance, not a sum of overlapping process
    // footprint + GPU memory. External tool reservations are accounted separately.
    unsafe {
        (loom_mlx_active_memory() as u64)
            .saturating_add(loom_mlx_cache_memory() as u64)
            .saturating_add(128 << 20)
    }
}

/// Admission never waits for the model mutex. Holding it means a request (or
/// benchmark) still owns the state, even between synchronous GPU passes.
fn reclaim_idle_mlx(resources: &ResourceGovernor, requested: u64) -> u64 {
    let mut slot = match LOADED.try_lock() {
        Ok(slot) => slot,
        Err(TryLockError::Poisoned(error)) => error.into_inner(),
        Err(TryLockError::WouldBlock) => return 0,
    };
    let Some(loaded) = slot
        .as_mut()
        .filter(|loaded| loaded.resident.owned_by(resources))
    else {
        return 0;
    };
    let before = loaded.resident.bytes();
    // Prefer the cheap reclaim. Preserve the prefix if freeing reusable buffers
    // alone is enough; otherwise recompute idle KV on the next model request.
    loaded.clear_reuse_cache();
    let snapshot = resources.snapshot();
    let used = snapshot
        .resident_bytes
        .saturating_add(snapshot.inference_bytes)
        .saturating_add(snapshot.tool_reserved_bytes);
    if used > snapshot.budget_bytes.saturating_sub(requested) {
        loaded.clear_session();
    }
    loaded.mark_idle();
    before.saturating_sub(loaded.resident.bytes())
}

impl Loaded {
    fn clear_reuse_cache(&mut self) {
        unsafe {
            loom_mlx_set_cache_limit(0);
            loom_mlx_clear_cache();
        }
        self.reuse_cache_reserved = 0;
        self.resident.shrink_to(
            self.weight_estimate
                .saturating_add(self.kv_bytes(self.capacity_tokens))
                .max(observed_resident()),
        );
    }

    fn clear_session(&mut self) {
        unsafe {
            loom_mlx_set_cache_limit(0);
            loom_mlx_reset(self.handle);
        }
        self.cached.clear();
        self.capacity_tokens = 0;
        self.reuse_cache_reserved = 0;
        self.resident
            .shrink_to(self.weight_estimate.max(observed_resident()));
        self.resident.mark_reclaimable(0);
    }

    fn mark_idle(&mut self) {
        let bytes = self
            .kv_bytes(self.capacity_tokens)
            .saturating_add(self.reuse_cache_reserved)
            .min(self.resident.bytes().saturating_sub(self.weight_estimate));
        self.resident.mark_reclaimable(bytes);
    }

    fn kv_bytes(&self, capacity: usize) -> u64 {
        let elements = [
            capacity as u64,
            self.config.layers as u64,
            self.config.kv_heads as u64,
            self.config.head_dim as u64,
            2,
        ]
        .into_iter()
        .fold(1u64, u64::saturating_mul);
        if self.config.kv_bits == 0 {
            elements.saturating_mul(2)
        } else {
            elements
                .saturating_mul(self.config.kv_bits as u64)
                .saturating_add(7)
                / 8
                + elements.saturating_mul(8) / self.config.kv_group.max(1) as u64
        }
    }

    fn target_capacity(&self, keep: usize, count: usize) -> usize {
        self.capacity_tokens
            .max(keep.saturating_add(count).saturating_add(255) / 256 * 256)
    }

    fn step_bytes(&self, keep: usize, count: usize) -> u64 {
        let n = count as u64;
        let context = keep.saturating_add(count) as u64;
        let experts = self.config.experts_per_token.max(1) as u64;
        let activation = n
            .saturating_mul(self.intermediate as u64)
            .saturating_mul(experts)
            .saturating_mul(24)
            .saturating_add(
                n.saturating_mul(self.config.hidden as u64)
                    .saturating_mul(48),
            );
        // Quantized attention currently materializes scores; keep the same
        // conservative estimate for floating SDPA until shape profiling proves a
        // smaller bound. Include logits and overlapping old/new KV allocations.
        let scores = n
            .saturating_mul(context)
            .saturating_mul(self.config.heads as u64)
            .saturating_mul(12);
        let logits = n.saturating_mul(self.vocab as u64).saturating_mul(4);
        let capacity = self.target_capacity(keep, count);
        let target = self
            .weight_estimate
            .saturating_add(self.kv_bytes(capacity))
            .saturating_add(self.reuse_cache_reserved);
        let growth = target.saturating_sub(self.resident.bytes());
        let overlap = if capacity > self.capacity_tokens {
            self.kv_bytes(self.capacity_tokens)
        } else {
            0
        };
        activation
            .saturating_add(scores)
            .saturating_add(logits)
            .saturating_add(overlap)
            .max(32 << 20)
            .saturating_add(growth)
    }

    fn release_unused(&mut self, reset: bool) {
        if reset {
            unsafe { loom_mlx_reset(self.handle) };
            self.cached.clear();
            self.capacity_tokens = 0;
        }
        let snapshot = self.resources.snapshot();
        let tool_pressure = snapshot.waiting_tools > 0 || snapshot.tool_reserved_bytes > 0;
        unsafe {
            loom_mlx_set_cache_limit(if tool_pressure { 0 } else { self.cache_limit });
            if tool_pressure {
                loom_mlx_clear_cache();
            }
        }
        let cache = if tool_pressure {
            0
        } else {
            self.cache_limit as u64
        };
        self.reuse_cache_reserved = cache;
        self.resident.shrink_to(
            self.weight_estimate
                .saturating_add(self.kv_bytes(self.capacity_tokens))
                .saturating_add(cache)
                .max(observed_resident()),
        );
    }

    fn step<T>(
        &mut self,
        keep: usize,
        count: usize,
        limits: Limits<'_>,
        run: impl FnOnce(&mut Self) -> Result<T, ProviderError>,
    ) -> Result<T, ProviderError> {
        admission_check(limits).map_err(admission_error)?;
        self.release_unused(keep == 0);
        let mut scratch = self
            .resources
            .acquire(
                ResourceKind::Inference,
                self.step_bytes(keep, count),
                || admission_check(limits),
            )
            .map_err(admission_error)?;
        let result = run(self);
        if result.is_ok() {
            self.capacity_tokens = self.target_capacity(keep, count);
        } else {
            // A partially failed native graph must never be reused as a prefix.
            unsafe { loom_mlx_reset(self.handle) };
            self.cached.clear();
            self.capacity_tokens = 0;
        }
        self.release_unused(false);
        let snapshot = self.resources.snapshot();
        let cache = if snapshot.waiting_tools > 0 || snapshot.tool_reserved_bytes > 0 {
            0
        } else {
            self.cache_limit as u64
        };
        let bytes = self
            .weight_estimate
            .saturating_add(self.kv_bytes(self.capacity_tokens))
            .saturating_add(cache)
            .max(observed_resident());
        scratch.settle(&mut self.resident, bytes);
        let value = result?;
        admission_check(limits).map_err(admission_error)?;
        Ok(value)
    }

    fn prefill_raw(&mut self, keep: usize, tokens: &[u32]) -> Result<(), ProviderError> {
        let ids: Vec<i32> = tokens.iter().map(|&token| token as i32).collect();
        let mut error = [0u8; 1024];
        let status = unsafe {
            loom_mlx_prefill(
                self.handle,
                ids.as_ptr(),
                ids.len(),
                keep,
                error.as_mut_ptr().cast(),
                error.len(),
            )
        };
        if status != 0 {
            return Err(ProviderError::new(
                ErrorKind::Transient,
                format!("MLX engine: {}", engine_error(&error)),
            ));
        }
        self.cached.truncate(keep);
        self.cached.extend_from_slice(tokens);
        Ok(())
    }

    fn next_guarded(
        &mut self,
        mut keep: usize,
        mut tokens: &[u32],
        temperature: f32,
        seed: u64,
        limits: Limits<'_>,
    ) -> Result<u32, ProviderError> {
        if tokens.is_empty() {
            return Err(bad("no tokens to run"));
        }
        while !tokens.is_empty() {
            admission_check(limits).map_err(admission_error)?;
            self.release_unused(keep == 0);
            let mut count = tokens.len().min(self.config.prefill_chunk.max(1) as usize);
            let snapshot = self.resources.snapshot();
            let available = snapshot
                .budget_bytes
                .saturating_sub(snapshot.resident_bytes)
                .saturating_sub(snapshot.tool_reserved_bytes);
            while count > 1 && self.step_bytes(keep, count) > available {
                count = (count / 2).max(1);
            }
            if count == tokens.len() {
                return self.step(keep, count, limits, |loaded| {
                    loaded.next_raw(keep, tokens, temperature, seed)
                });
            }
            self.step(keep, count, limits, |loaded| {
                loaded.prefill_raw(keep, &tokens[..count])
            })?;
            keep += count;
            tokens = &tokens[count..];
        }
        unreachable!("nonempty prompt returns its last sampled token")
    }

    fn next(
        &mut self,
        keep: usize,
        tokens: &[u32],
        temperature: f32,
        seed: u64,
    ) -> Result<u32, ProviderError> {
        self.next_guarded(keep, tokens, temperature, seed, None)
    }

    fn verify_guarded(
        &mut self,
        keep: usize,
        tokens: &[u32],
        temperature: f32,
        seed: u64,
        limits: Limits<'_>,
    ) -> Result<Vec<u32>, ProviderError> {
        self.step(keep, tokens.len(), limits, |loaded| {
            loaded.verify_raw(keep, tokens, temperature, seed)
        })
    }

    /// Feed `tokens` after the first `keep` cached ones; `next[i]` is sampled after
    /// `tokens[i]`. The cache then holds all of `tokens`; callers trim rejected drafts.
    fn verify_raw(
        &mut self,
        keep: usize,
        tokens: &[u32],
        temperature: f32,
        seed: u64,
    ) -> Result<Vec<u32>, ProviderError> {
        let ids: Vec<i32> = tokens.iter().map(|&token| token as i32).collect();
        let mut next = vec![0i32; ids.len()];
        let mut error = [0u8; 1024];
        let status = unsafe {
            loom_mlx_verify(
                self.handle,
                ids.as_ptr(),
                ids.len(),
                keep,
                temperature,
                seed,
                next.as_mut_ptr(),
                error.as_mut_ptr().cast(),
                error.len(),
            )
        };
        if status != 0 {
            self.cached.clear();
            return Err(ProviderError::new(
                ErrorKind::Transient,
                format!("MLX engine: {}", engine_error(&error)),
            ));
        }
        self.cached.truncate(keep);
        self.cached.extend_from_slice(tokens);
        Ok(next.into_iter().map(|token| token as u32).collect())
    }

    /// Feed `tokens` after the first `keep` cached ones and return the next token.
    fn next_raw(
        &mut self,
        keep: usize,
        tokens: &[u32],
        temperature: f32,
        seed: u64,
    ) -> Result<u32, ProviderError> {
        let ids: Vec<i32> = tokens.iter().map(|&token| token as i32).collect();
        let mut next = 0i32;
        let mut error = [0u8; 1024];
        let status = unsafe {
            loom_mlx_next(
                self.handle,
                ids.as_ptr(),
                ids.len(),
                keep,
                temperature,
                seed,
                &mut next,
                error.as_mut_ptr().cast(),
                error.len(),
            )
        };
        if status != 0 {
            self.cached.clear();
            return Err(ProviderError::new(
                ErrorKind::Transient,
                format!("MLX engine: {}", engine_error(&error)),
            ));
        }
        self.cached.truncate(keep);
        self.cached.extend_from_slice(tokens);
        Ok(next as u32)
    }
}

pub struct MlxProvider {
    models: Vec<MlxModel>,
    resources: Arc<ResourceGovernor>,
}

impl MlxProvider {
    pub(crate) fn from_models(models: Vec<MlxModel>) -> Self {
        Self { models, resources: local_resources() }
    }
    pub fn discover() -> Self {
        super::native_runtime::silence_native_logs();
        Self {
            models: installed(),
            resources: local_resources(),
        }
    }

    pub fn models(&self) -> &[MlxModel] {
        &self.models
    }

    fn model(&self, id: &str) -> Result<&MlxModel, ProviderError> {
        self.models
            .iter()
            .find(|model| model.id == id)
            .ok_or_else(|| bad(format!("MLX model `{id}` is not installed or supported")))
    }

    fn infer(
        &self,
        request: &Request,
        observe: &mut dyn FnMut(StreamEvent),
    ) -> Result<Response, ProviderError> {
        request.check_cancelled()?;
        let started = Instant::now();
        let model = self.model(&request.model)?;
        let mut slot = request_slot(request, started)?;
        if let Some(loaded) = slot.as_mut() {
            loaded.resident.mark_reclaimable(0);
        }
        let result = self
            .infer_loaded(model, request, observe, &mut slot, started)
            .and_then(|response| {
                check_limits(request, started)?;
                Ok(response)
            });
        if let Some(loaded) = slot.as_mut() {
            if result.is_err() {
                loaded.clear_session();
            }
            loaded.mark_idle();
        }
        drop(slot);
        self.resources.notify_idle();
        result
    }

    fn infer_loaded(
        &self,
        model: &MlxModel,
        request: &Request,
        observe: &mut dyn FnMut(StreamEvent),
        slot: &mut Option<Loaded>,
        started: Instant,
    ) -> Result<Response, ProviderError> {
        let loaded = ensure_loaded(
            slot,
            model,
            super::native_runtime::ContextOptions::configured().kv,
            &self.resources,
            Some((request, started)),
        )?;
        let thinking = request
            .reasoning_effort
            .as_deref()
            .is_some_and(|effort| effort != "none");
        let prompt = render_template_prompt(&loaded.template, request, thinking, "", "")
            .map_err(|error| bad(format!("chat template of {}: {error}", model.id)))?;
        let tokens: Vec<u32> = loaded
            .tokenizer
            .encode(prompt.as_str())?;
        if tokens.is_empty() {
            return Err(bad("MLX prompt tokenized to zero tokens"));
        }
        let max_output = request.max_output_tokens.unwrap_or(2048).max(1) as usize;
        let limit = std::env::var("LOOM_NATIVE_CONTEXT")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(32_768)
            .min(model.context);
        if tokens.len() + max_output > limit {
            return Err(bad(format!(
                "request ({} tokens) exceeds the MLX context limit ({limit} tokens); set LOOM_NATIVE_CONTEXT to raise it",
                tokens.len() + max_output
            )));
        }
        // Core resends the same instructions and history each step; reuse their cache.
        let common = loaded
            .cached
            .iter()
            .zip(&tokens)
            .take_while(|(a, b)| a == b)
            .count();
        let reused = common.min(tokens.len() - 1);
        let sampling = model.sampling.for_request(request)?;
        let temperature = sampling.temperature;
        unsafe { loom_mlx_set_sampling(loaded.handle, sampling.top_k, sampling.top_p, sampling.repetition); }
        let seed = 0x10_0D_F00D;
        let mut pending = loaded.next_guarded(
            reused,
            &tokens[reused..],
            temperature,
            seed,
            Some((request, started)),
        )?;
        check_limits(request, started)?;
        let decode_started = Instant::now();
        let opened = prompt.trim_end().ends_with(THINK_OPEN);
        let mut output = SparkOutput::new(opened);
        if loaded.template.contains("<function=") {
            output = output.with_xml_tools(&request.tools);
        }
        let mut generated: Vec<u32> = Vec::new();
        let mut history = tokens.clone();
        let mut shown = String::new();
        let mut hit_eos = false;
        // Equality-based prompt lookup is exact for greedy decoding. Sampling needs
        // a probability-based speculative acceptance algorithm, which this is not.
        let lookup = temperature == 0.0 && std::env::var("LOOM_MLX_LOOKUP").map_or(true, |value| value != "0");
        let (mut drafted, mut accepted_total, mut passes) = (0usize, 0usize, 0usize);
        let mut last_speed = Instant::now();
        let tokenizer = std::sync::Arc::clone(&loaded.tokenizer);
        // Decode each generated token once.
        let mut decoder = tokenizer.stream();
        let ends = |token: u32| model.eos.contains(&token) || tokenizer.is_eog(token);
        let mut emit = |new_tokens: &[u32],
                        shown: &mut String,
                        output: &mut SparkOutput,
                        observe: &mut dyn FnMut(StreamEvent)|
         -> Result<(), ProviderError> {
            for &token in new_tokens {
                if let Some(text) = decoder(token)? {
                    for delta in output.feed(&text) {
                        if !delta.is_empty() {
                            observe(StreamEvent::TextDelta(delta));
                        }
                    }
                    shown.push_str(&text);
                }
            }
            Ok(())
        };
        'generate: while generated.len() < max_output {
            if ends(pending) {
                hit_eos = true;
                break;
            }
            generated.push(pending);
            history.push(pending);
            emit(&[pending], &mut shown, &mut output, observe)?;
            if generated.len() >= max_output {
                break;
            }
            check_limits(request, started)?;
            let room = max_output - generated.len();
            let draft = if lookup {
                lookup_draft(&history, room.min(8))
            } else {
                Vec::new()
            };
            let mut feed = Vec::with_capacity(1 + draft.len());
            feed.push(pending);
            feed.extend_from_slice(&draft);
            let keep = loaded.cached.len();
            let sampled = loaded.verify_guarded(
                keep,
                &feed,
                temperature,
                seed.wrapping_add(passes as u64 + 1),
                Some((request, started)),
            )?;
            passes += 1;
            drafted += draft.len();
            let accepted_start = generated.len();
            let mut accepted = 0;
            while accepted < draft.len() && sampled[accepted] == draft[accepted] {
                let token = draft[accepted];
                accepted += 1;
                if ends(token) {
                    hit_eos = true;
                    loaded.cached.truncate(keep + accepted);
                    accepted_total += accepted;
                    break 'generate;
                }
                generated.push(token);
                history.push(token);
                if generated.len() >= max_output {
                    break;
                }
            }
            accepted_total += accepted;
            if last_speed.elapsed().as_millis() >= 500 {
                last_speed = Instant::now();
                observe(StreamEvent::Speed {
                    tokens: generated.len() as u64,
                    seconds: decode_started.elapsed().as_secs_f64(),
                });
            }
            // The engine cached every fed token; only the accepted prefix is real.
            loaded.cached.truncate(keep + 1 + accepted);
            pending = sampled[accepted];
            if accepted > 0 {
                emit(
                    &generated[accepted_start..],
                    &mut shown,
                    &mut output,
                    observe,
                )?;
            }
        }
        let decode_seconds = decode_started.elapsed().as_secs_f64();
        if !generated.is_empty() {
            observe(StreamEvent::Speed {
                tokens: generated.len() as u64,
                seconds: decode_seconds,
            });
        }
        if std::env::var_os("LOOM_MLX_STATS").is_some() {
            eprintln!(
                "mlx: {} tokens in {:.2}s ({:.1} tok/s), {passes} passes, drafts {accepted_total}/{drafted} accepted",
                generated.len(),
                decode_seconds,
                generated.len() as f64 / decode_seconds.max(1e-9),
            );
        }
        let text = loaded
            .tokenizer
            .decode(&generated);
        if text.len() > shown.len() && text.starts_with(&shown) {
            for delta in output.feed(&text[shown.len()..]) {
                if !delta.is_empty() {
                    observe(StreamEvent::TextDelta(delta));
                }
            }
        }
        for delta in output.finish() {
            if !delta.is_empty() {
                observe(StreamEvent::TextDelta(delta));
            }
        }
        let mut parts = Vec::new();
        if !output.visible.trim().is_empty() {
            parts.push(Part::Text(output.visible.clone()));
        }
        parts.extend(super::native_runtime::parse_tool_calls_for_tools(&output.tools, &request.tools));
        let stop = if parts
            .iter()
            .any(|part| matches!(part, Part::ToolCall { .. }))
        {
            Stop::ToolUse
        } else if hit_eos {
            Stop::EndTurn
        } else {
            Stop::MaxTokens
        };
        let usage = Usage {
            input: Some(tokens.len() as u64),
            cached_input: Some(reused as u64),
            output: Some(generated.len() as u64),
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

    /// Load `id` fresh and measure memory, prompt processing and generation.
    pub fn bench(
        &self,
        id: &str,
        kv: super::native_runtime::KvPrecision,
        prompt_tokens: usize,
        generate: usize,
    ) -> Result<super::native_runtime::BenchReport, ProviderError> {
        let model = self.model(id)?;
        let mut slot = loaded_slot();
        *slot = None;
        unsafe {
            loom_mlx_reset_peak_memory();
        }
        let before = super::native_runtime::footprint_bytes();
        let started = Instant::now();
        let loaded = ensure_loaded(&mut slot, model, kv, &self.resources, None)?;
        // Weights are lazy: the first pass maps them in, so warm up with one token.
        loaded.next(0, &[0], 0.0, 0)?;
        let load_ms = started.elapsed().as_millis() as u64;
        let after_load = super::native_runtime::footprint_bytes();
        let sample = "fn route(request: &Request) -> Result<Response> {\n    let path = request.path.trim_start_matches('/');\n    match path.split_once('/') { Some((head, rest)) => dispatch(head, rest), None => not_found(path) }\n}\n";
        let mut tokens = Vec::new();
        while tokens.len() < prompt_tokens.max(1) {
            tokens.extend_from_slice(
                loaded
                    .tokenizer
                    .encode(sample)?.as_slice(),
            );
        }
        tokens.truncate(prompt_tokens.max(1));
        let started = Instant::now();
        let mut next = loaded.next(0, &tokens, 0.0, 0)?;
        let prefill_ms = started.elapsed().as_millis() as u64;
        let started = Instant::now();
        let mut continuation = Vec::with_capacity(generate);
        for _ in 0..generate {
            continuation.push(next);
            let keep = loaded.cached.len();
            next = loaded.next(keep, &[next], 0.0, 0)?;
        }
        let decode_ms = started.elapsed().as_millis() as u64;
        loaded.mark_idle();
        let (active, peak) = engine_memory();
        Ok(super::native_runtime::BenchReport {
            model: id.to_owned(),
            backend: "mlx",
            kv: kv.label(),
            flash: Some(kv == super::native_runtime::KvPrecision::F16),
            weights_bytes: model.weight_bytes().map_err(|error|bad(error.to_string()))?,
            load_ms,
            weights_memory: after_load.saturating_sub(before),
            context_tokens: (tokens.len() + generate) as u32,
            context_memory: super::native_runtime::footprint_bytes().saturating_sub(after_load),
            prompt_tokens: tokens.len(),
            prefill_ms,
            generated: generate,
            decode_ms,
            // Process memory and MLX allocator memory measure different things.
            peak_footprint: super::native_runtime::peak_footprint_bytes(),
            mlx_memory: Some(super::native_runtime::MlxMemoryReport {
                active_bytes: active,
                peak_active_bytes: peak,
                cache_bytes: unsafe { loom_mlx_cache_memory() as u64 },
            }),
            local_resources: Some(self.resources.snapshot()),
            tokens: continuation,
        })
    }
}

impl Provider for MlxProvider {
    fn name(&self) -> &str {
        "mlx"
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

#[cfg(test)]
mod tests {
    use super::*;

    fn inspect_fixture(config: Value) -> Option<MlxModel> {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), config.to_string()).unwrap();
        std::fs::write(dir.path().join("tokenizer.json"), "{}").unwrap();
        std::fs::write(dir.path().join("model.safetensors"), "").unwrap();
        MlxModel::inspect(dir.path(), "test/model".into())
    }

    fn moe_config() -> Value {
        serde_json::json!({
            "model_type": "qwen3_moe", "hidden_size": 128,
            "num_hidden_layers": 2, "num_attention_heads": 4,
            "num_key_value_heads": 2, "head_dim": 32,
            "num_experts": 8, "num_experts_per_tok": 2,
            "norm_topk_prob": true,
            "quantization": {"bits": 3, "group_size": 64}
        })
    }

    #[test]
    fn generation_sampling_defaults_and_explicit_greedy_are_preserved() {
        let generation = serde_json::json!({"do_sample":true,"temperature":0.7,
            "top_p":0.8,"top_k":20,"repetition_penalty":1.05});
        let sampling = Sampling::from_generation(&generation).unwrap();
        assert_eq!(sampling, Sampling { temperature:0.7, top_k:20, top_p:0.8, repetition:1.05 });
        assert_eq!(sampling.for_request(&Request::default()).unwrap(), sampling);
        let greedy = sampling.for_request(&Request {temperature:Some(0.0),..Request::default()}).unwrap();
        assert_eq!(greedy.temperature, 0.0);
        assert_eq!((greedy.top_k,greedy.top_p,greedy.repetition),(0,1.0,1.0));
        for invalid in [serde_json::json!({"top_p":0}),serde_json::json!({"top_k":-1}),
            serde_json::json!({"temperature":-0.1}),serde_json::json!({"repetition_penalty":0})] {
            assert!(Sampling::from_generation(&invalid).is_none());
        }
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("config.json"), moe_config().to_string()).unwrap();
        std::fs::write(dir.path().join("tokenizer.json"), "{}").unwrap();
        std::fs::write(dir.path().join("model.safetensors"), "").unwrap();
        std::fs::write(dir.path().join("generation_config.json"),generation.to_string()).unwrap();
        assert_eq!(MlxModel::inspect(dir.path(),"test/model".into()).unwrap().sampling,sampling);
    }

    #[test]
    fn engine_sampling_filters_and_repetition_use_only_the_current_prefix() {
        if !gpu_available() { return; }
        // A zero-layer decoder gives known logits [10, 9.8, -1, -2]. Exercise
        // the same sampler used by real models, including speculative rollback.
        let config = EngineConfig {hidden:32,layers:0,heads:1,kv_heads:1,head_dim:32,
            eps:1e-6,rope_theta:10_000.0,tied_embeddings:0,qk_norm:0,group_size:32,
            bits:0,prefill_chunk:128,kv_bits:0,kv_group:32,kv_rotate:0,
            experts:0,experts_per_token:0,norm_topk_prob:0};
        let mut error = [0u8;1024];
        let weights = unsafe { loom_mlx_weights_new() };
        for (name, shape, values) in [
            ("model.embed_tokens.weight",vec![4,32],vec![1.0f32;128]),
            ("model.norm.weight",vec![32],vec![1.0f32;32]),
            ("lm_head.weight",vec![4,32],[10.0f32,9.8,-1.0,-2.0].into_iter()
                .flat_map(|score| std::iter::repeat_n(score/32.0,32)).collect()),
        ] {
            let name = CString::new(name).unwrap();
            let data = unsafe {loom_mlx_weights_alloc(weights,name.as_ptr(),2,shape.as_ptr(),shape.len(),
                values.len()*4,error.as_mut_ptr().cast(),error.len())};
            assert!(!data.is_null(),"{}",engine_error(&error));
            unsafe { std::ptr::copy_nonoverlapping(values.as_ptr(),data.cast(),values.len()); }
        }
        let handle = unsafe {loom_mlx_load_weights(weights,&config,error.as_mut_ptr().cast(),error.len())};
        assert!(!handle.is_null(),"{}",engine_error(&error));
        struct Guard(*mut c_void);
        impl Drop for Guard {fn drop(&mut self){unsafe {loom_mlx_free(self.0);}}}
        let _guard = Guard(handle);
        for (top_k,top_p,repetition,token,expected) in [
            (1,1.0,1.0,0,0), (1,1.0,1.05,0,1), (0,0.001,1.05,0,1),
            // Rewind must discard the preceding request's penalty history.
            (1,1.0,1.05,1,0),
        ] {
            unsafe {loom_mlx_set_sampling(handle,top_k,top_p,repetition);}
            for seed in 0..4 {
                let mut next = -1;
                let status = unsafe {loom_mlx_next(handle,&token,1,0,0.7,seed,&mut next,
                    error.as_mut_ptr().cast(),error.len())};
                assert_eq!(status,0,"{}",engine_error(&error));
                assert_eq!(next,expected);
            }
        }
        unsafe {loom_mlx_set_sampling(handle,1,1.0,1.05);}
        let mut next = [-1;2];
        let status = unsafe {loom_mlx_verify(handle,[0,1].as_ptr(),2,0,0.7,0,next.as_mut_ptr(),
            error.as_mut_ptr().cast(),error.len())};
        assert_eq!(status,0,"{}",engine_error(&error));
        assert_eq!(next,[1,0],"a future draft token affected an earlier sample");
    }

    /// Run alone with LOOM_LMX_PARITY=/path/model.lmx and --ignored --nocapture.
    /// Isolates conversion from tokenizer, Metal warmup, KV and inference scratch.
    #[test]
    #[ignore]
    fn gguf_mlx_loading_has_no_tensor_sized_cpu_copies() {
        let path = std::env::var("LOOM_LMX_PARITY").expect("set LOOM_LMX_PARITY to a GGUF LMX");
        let executable = super::super::lmx::LmxFile::open(Path::new(&path)).unwrap();
        let model = MlxModel::from_lmx(&executable).unwrap();
        let source = model.gguf.as_ref().expect("GGUF payload");
        let mut slot = loaded_slot();
        *slot = None;
        unsafe { loom_mlx_clear_cache(); loom_mlx_reset_peak_memory(); }
        let active_before = engine_memory().0;
        let physical_before = super::super::native_runtime::footprint_bytes();
        let handle = source.load(&model.config).unwrap();
        struct ModelGuard(*mut c_void);
        impl Drop for ModelGuard {
            fn drop(&mut self) { unsafe { loom_mlx_free(self.0); } }
        }
        let _guard = ModelGuard(handle);
        let (active, peak) = engine_memory();
        let physical = super::super::native_runtime::footprint_bytes().saturating_sub(physical_before);
        let physical_peak = super::super::native_runtime::peak_footprint_bytes().saturating_sub(physical_before);
        let weights = source.converted_bytes();
        // Per-array allocator alignment and metadata, not a whole-tensor work buffer.
        let allowance = 64 << 20;
        eprintln!("weights={weights} active_delta={} peak_active_delta={} physical_delta={physical} physical_peak_delta={physical_peak}",
            active.saturating_sub(active_before), peak.saturating_sub(active_before));
        assert!(peak.saturating_sub(active_before) <= weights + allowance);
        assert!(physical <= weights + allowance);
        assert!(physical_peak <= weights + allowance);
    }

    #[test]
    fn discovers_qwen3_moe_with_bounded_prefill() {
        let model = inspect_fixture(moe_config()).unwrap();
        assert_eq!(model.config.experts, 8);
        assert_eq!(model.config.experts_per_token, 2);
        assert_eq!(model.config.norm_topk_prob, 1);
        assert_eq!(model.config.qk_norm, 1);
        assert_eq!(model.config.prefill_chunk, 128);
    }

    #[test]
    fn skips_unsupported_or_invalid_moe_variants() {
        for (field, value) in [
            ("num_experts_per_tok", serde_json::json!(9)),
            ("num_experts", serde_json::json!(0)),
            ("num_attention_heads", serde_json::json!(0)),
            ("num_key_value_heads", serde_json::json!(3)),
            ("rope_scaling", serde_json::json!({"factor": 2})),
            ("hidden_act", serde_json::json!("gelu")),
            ("shared_expert_intermediate_size", serde_json::json!(128)),
            ("sliding_window", serde_json::json!(4096)),
            (
                "quantization",
                serde_json::json!({"bits": 4, "mode": "mxfp4"}),
            ),
            (
                "quantization",
                serde_json::json!({"bits": 3, "group_size": 0}),
            ),
            (
                "quantization",
                serde_json::json!({"bits": 3, "group_size": 64,
                "model.layers.0.mlp.gate": {"bits": 8, "group_size": 32}}),
            ),
        ] {
            let mut config = moe_config();
            config[field] = value;
            assert!(
                inspect_fixture(config).is_none(),
                "accepted unsupported {field}"
            );
        }
    }

    /// Exercise the streaming path against a real MLX tokenizer, including UTF-8
    /// boundaries, spaces, and tool-call markup. No model inference is required.
    /// `LOOM_MLX_TOKENIZER=/path/to/tokenizer.json cargo test -- streaming_decoder_matches_batch --ignored`
    #[test]
    #[ignore]
    fn streaming_decoder_matches_batch() {
        let path = std::env::var("LOOM_MLX_TOKENIZER")
            .expect("set LOOM_MLX_TOKENIZER to a tokenizer.json path");
        let tokenizer = tokenizers::Tokenizer::from_file(path).unwrap();
        for text in [
            "fn main() { println!(\"hello world\"); }\n",
            "한국어와 English 日本語 😀 이모지 섞기\n두 번째 줄",
            "<tool_call>{\"name\":\"find\",\"arguments\":{\"path\":\"src\"}}</tool_call>",
            "  start  and  trailing spaces   ",
        ] {
            let ids = tokenizer.encode(text, false).unwrap().get_ids().to_vec();
            let expected = tokenizer.decode(&ids, true).unwrap();
            let mut stream = tokenizer.decode_stream(true);
            let mut actual = String::new();
            for id in ids {
                if let Some(delta) = stream.step(id).unwrap() {
                    actual.push_str(&delta);
                }
            }
            assert_eq!(
                actual, expected,
                "stream and full decode differ for {text:?}"
            );
        }
    }

    /// End-to-end guard: generated text and streamed deltas must agree after
    /// switching to incremental decoding. Uses a small, installed dense model.
    /// `LOOM_MLX_SMOKE=<id> cargo test --release -- mlx_stream_matches_response --ignored`
    #[test]
    #[ignore]
    fn mlx_stream_matches_response() {
        let id = std::env::var("LOOM_MLX_SMOKE")
            .expect("set LOOM_MLX_SMOKE to an installed dense model id");
        let provider = MlxProvider::discover();
        let request = Request {
            model: id,
            messages: vec![Message::user("Say a short hello in Korean.")],
            max_output_tokens: Some(24),
            temperature: Some(0.0),
            reasoning_effort: Some("none".into()),
            ..Request::default()
        };
        let mut streamed = String::new();
        let response = provider
            .complete_stream(&request, &mut |event| {
                if let StreamEvent::TextDelta(delta) = event {
                    streamed.push_str(&delta);
                }
            })
            .unwrap();
        assert_eq!(streamed, response.text());
    }

    /// Real engine + real tool path, with a deliberately tight admission budget.
    /// The shell compiles/runs a Rust test then remains alive until the fixture
    /// releases it. This proves scheduling, not a throughput improvement.
    #[test]
    #[ignore]
    fn mlx_tool_reservation_pauses_and_resumes_inference() {
        use crate::{
            base::FileArtifactStore,
            effects::{RunStatus, Runner},
        };
        use std::sync::atomic::AtomicBool;
        struct RestoreEnv(Option<std::ffi::OsString>);
        impl Drop for RestoreEnv {
            fn drop(&mut self) {
                unsafe {
                    match &self.0 {
                        Some(value) => std::env::set_var("LOOM_MLX_CACHE_MB", value),
                        None => std::env::remove_var("LOOM_MLX_CACHE_MB"),
                    }
                }
            }
        }
        let _restore = RestoreEnv(std::env::var_os("LOOM_MLX_CACHE_MB"));
        // No reusable cache can mask the intended low-headroom admission case.
        unsafe { std::env::set_var("LOOM_MLX_CACHE_MB", "0") };
        let id = std::env::var("LOOM_MLX_SMOKE").expect("set LOOM_MLX_SMOKE");
        let mut provider = MlxProvider::discover();
        provider.resources = ResourceGovernor::new(8 << 30, 512 << 20);
        let request = Request {
            model: id,
            messages: vec![Message::user("Say a short hello in Korean.")],
            max_output_tokens: Some(8),
            temperature: Some(0.0),
            reasoning_effort: Some("none".into()),
            timeout: Some(Duration::from_secs(15)),
            ..Request::default()
        };
        provider.complete(&request).unwrap();
        let resident = provider.resources.snapshot().resident_bytes;
        let resources = ResourceGovernor::new(resident + (512 << 20) + (8 << 20), 512 << 20);
        provider.resources = Arc::clone(&resources);
        // Re-load under the measured budget, obtaining a reference continuation.
        let expected = provider.complete(&request).unwrap().text();
        let before = resources.snapshot();
        assert!(
            before
                .budget_bytes
                .saturating_sub(before.resident_bytes + resources.tool_bytes())
                < 32 << 20
        );
        let dir = tempfile::tempdir().unwrap();
        let runner = Runner::with_resources(
            FileArtifactStore::open(dir.path()).unwrap(),
            Arc::clone(&resources),
        );
        let cancel = AtomicBool::new(false);
        let until = |ready: &dyn Fn() -> bool| {
            let start = Instant::now();
            while !ready() {
                assert!(
                    start.elapsed() < Duration::from_secs(10),
                    "fixture did not reach its scheduling boundary"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
        };
        let (waiting, after, model_ms, tool_ms) = std::thread::scope(|scope| {
            let tool = scope.spawn(|| runner.run(dir.path(),
                "printf '#[test] fn smoke() { assert_eq!(2 + 2, 4); }' > smoke.rs; rustc --test smoke.rs -o smoke && ./smoke || exit 1; printf ready > ready; while [ ! -f finish ]; do sleep 0.02; done",
                Duration::from_secs(15), &cancel, None).unwrap());
            until(&|| dir.path().join("ready").exists());
            let start = Instant::now();
            let inference = scope.spawn(|| {
                let mut streamed = String::new();
                let response = provider
                    .complete_stream(&request, &mut |event| {
                        if let StreamEvent::TextDelta(delta) = event {
                            streamed.push_str(&delta);
                        }
                    })
                    .unwrap();
                assert_eq!(streamed, response.text());
                assert_eq!(response.text(), expected);
            });
            until(&|| resources.snapshot().waiting_inference == 1);
            let waiting = resources.snapshot();
            assert_eq!(waiting.tool_reserved_bytes, 512 << 20);
            // A second request is blocked by the loaded-model mutex, but still
            // observes its own deadline rather than waiting for the first reply.
            let deadline_start = Instant::now();
            let mut queued = request.clone();
            queued.timeout = Some(Duration::from_millis(50));
            assert_eq!(
                provider.complete(&queued).unwrap_err().kind,
                ErrorKind::TimedOut
            );
            assert!(deadline_start.elapsed() < Duration::from_secs(1));
            std::fs::write(dir.path().join("finish"), "").unwrap();
            let completed = tool.join().unwrap();
            assert_eq!(completed.status, RunStatus::Passed);
            inference.join().unwrap();
            (
                waiting,
                resources.snapshot(),
                start.elapsed().as_millis(),
                completed.ms,
            )
        });
        assert_eq!(after.tool_reserved_bytes, 0);
        assert_eq!(after.inference_bytes, 0);
        assert_eq!(after.waiting_inference, 0);
        let report = serde_json::json!({
            "purpose": "admission regression, not a performance comparison",
            "model": request.model, "before_tool": before, "while_waiting": waiting,
            "after_resume": after, "model_ms_including_wait": model_ms,
            "tool_ms_including_fixture_hold": tool_ms, "greedy_output_matches": true,
            "process_lifetime_peak_footprint": super::super::native_runtime::peak_footprint_bytes()
        });
        if let Ok(path) = std::env::var("LOOM_RESOURCE_SMOKE_OUTPUT") {
            std::fs::write(path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();
        }
        eprintln!("{report}");
        *loaded_slot() = None;
        assert_eq!(resources.snapshot().resident_bytes, 0);
    }

    /// Idle cache pressure must admit tools without unloading/requantizing the
    /// model. Covers cache-only reclaim and dropping KV when that is necessary.
    #[test]
    #[ignore]
    fn mlx_idle_reclaim_keeps_weights_and_recomputes_the_same_output() {
        use crate::{
            base::FileArtifactStore,
            effects::{RunStatus, Runner},
        };
        use std::sync::atomic::AtomicBool;
        let id = std::env::var("LOOM_MLX_SMOKE").expect("set LOOM_MLX_SMOKE");
        let mut reports = Vec::new();
        for (label, deficit, drop_kv) in [
            ("reuse_buffers", 32u64 << 20, false),
            ("idle_kv", 96u64 << 20, true),
        ] {
            let mut provider = MlxProvider::discover();
            provider.resources = ResourceGovernor::new(8 << 30, 512 << 20);
            let request = Request {
                model: id.clone(),
                messages: vec![Message::user(format!(
                    "Say a short hello in Korean after reading this code:\n{}",
                    "fn route(x: i32) -> i32 { if x > 0 { x + 1 } else { 0 } }\n".repeat(32)
                ))],
                max_output_tokens: Some(8),
                temperature: Some(0.0),
                reasoning_effort: Some("none".into()),
                timeout: Some(Duration::from_secs(20)),
                ..Request::default()
            };
            let warm = provider.complete(&request).unwrap();
            assert!(warm.usage.input.unwrap() >= 512);
            let resident = provider.resources.snapshot().resident_bytes;
            let resources = ResourceGovernor::new(resident + (512 << 20) - deficit, 512 << 20);
            provider.resources = Arc::clone(&resources);
            let expected = provider.complete(&request).unwrap().text();
            let before = resources.snapshot();
            let handle = loaded_slot().as_ref().unwrap().handle as usize;
            let active_before = engine_memory().0;
            let footprint_before = super::super::native_runtime::footprint_bytes();
            // Under no pressure, keep the useful prefix and avoid a callback.
            let small = resources
                .acquire(ResourceKind::Tool, 1 << 20, || Ok(()))
                .unwrap();
            assert_eq!(resources.snapshot().reclaims, 0);
            assert!(!loaded_slot().as_ref().unwrap().cached.is_empty());
            drop(small);

            let dir = tempfile::tempdir().unwrap();
            let runner = Runner::with_resources(
                FileArtifactStore::open(dir.path()).unwrap(),
                Arc::clone(&resources),
            );
            let cancel = AtomicBool::new(false);
            let (during, active_after, footprint_after) =
                std::thread::scope(|scope| {
                    let tool =
                        scope.spawn(|| {
                            runner.run(dir.path(),
                    "printf ready > ready; while [ ! -f finish ]; do sleep 0.02; done",
                    Duration::from_secs(15), &cancel, None).unwrap()
                        });
                    let started = Instant::now();
                    while !dir.path().join("ready").exists() {
                        assert!(
                            started.elapsed() < Duration::from_secs(10),
                            "tool could not reclaim idle memory"
                        );
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    let during = resources.snapshot();
                    assert_eq!(during.tool_reserved_bytes, 512 << 20);
                    assert!(before.resident_bytes - during.resident_bytes >= deficit);
                    {
                        let slot = loaded_slot();
                        let loaded = slot.as_ref().unwrap();
                        assert_eq!(loaded.handle as usize, handle, "weights were unloaded");
                        assert_eq!(loaded.cached.is_empty(), drop_kv);
                        assert_eq!(loaded.capacity_tokens == 0, drop_kv);
                    }
                    let active_after = engine_memory().0;
                    let footprint_after = super::super::native_runtime::footprint_bytes();
                    std::fs::write(dir.path().join("finish"), "").unwrap();
                    assert_eq!(tool.join().unwrap().status, RunStatus::Passed);
                    (during, active_after, footprint_after)
                });
            let response = provider.complete(&request).unwrap();
            assert_eq!(response.text(), expected);
            assert_eq!(response.usage.cached_input.unwrap() == 0, drop_kv);
            assert_eq!(loaded_slot().as_ref().unwrap().handle as usize, handle);
            if drop_kv {
                assert!(active_before > active_after + (64 << 20));
            }
            assert_eq!(resources.snapshot().tool_reserved_bytes, 0);
            reports.push(serde_json::json!({
                "case": label, "model": id, "input_tokens": response.usage.input,
                "before": before, "during_tool": during, "after_resume": resources.snapshot(),
                "mlx_active_before": active_before, "mlx_active_after_reclaim": active_after,
                "process_footprint_before": footprint_before, "process_footprint_after_reclaim": footprint_after,
                "same_weight_handle": true, "same_greedy_output": true,
                "kv_recomputed": drop_kv,
                "purpose": "idle reclaim regression; reservations and allocator/process memory are distinct"
            }));
            *loaded_slot() = None;
            assert_eq!(resources.snapshot().resident_bytes, 0);
            assert_eq!(resources.snapshot().reclaimable_resident_bytes, 0);
        }
        if let Ok(path) = std::env::var("LOOM_RECLAIM_SMOKE_OUTPUT") {
            std::fs::write(path, serde_json::to_vec_pretty(&reports).unwrap()).unwrap();
        }
        eprintln!("{}", serde_json::to_string(&reports).unwrap());
    }

    #[test]
    #[ignore]
    fn mlx_cancel_releases_session_state_and_keeps_the_model_loaded() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let id = std::env::var("LOOM_MLX_SMOKE").expect("set LOOM_MLX_SMOKE");
        let mut provider = MlxProvider::discover();
        provider.resources = ResourceGovernor::new(8 << 30, 512 << 20);
        let request = Request {
            model: id,
            messages: vec![Message::user("Say a short hello in Korean.")],
            max_output_tokens: Some(8),
            temperature: Some(0.0),
            reasoning_effort: Some("none".into()),
            ..Request::default()
        };
        let expected = provider.complete(&request).unwrap().text();
        let handle = loaded_slot().as_ref().unwrap().handle as usize;
        let cancel = Arc::new(AtomicBool::new(false));
        let mut cancelled = request.clone();
        cancelled.cancel = Some(Arc::clone(&cancel));
        // Even cancellation on the final emitted token must release the session.
        cancelled.max_output_tokens = Some(1);
        let error = provider
            .complete_stream(&cancelled, &mut |event| {
                if let StreamEvent::TextDelta(_) = event {
                    cancel.store(true, Ordering::Release);
                }
            })
            .unwrap_err();
        assert_eq!(error.kind, ErrorKind::Cancelled);
        {
            let slot = loaded_slot();
            let loaded = slot.as_ref().unwrap();
            assert_eq!(loaded.handle as usize, handle);
            assert!(loaded.cached.is_empty());
            assert_eq!(loaded.capacity_tokens, 0);
        }
        let snapshot = provider.resources.snapshot();
        assert_eq!(snapshot.inference_bytes, 0);
        assert_eq!(snapshot.waiting_inference, 0);
        assert_eq!(snapshot.reclaimable_resident_bytes, 0);
        let response = provider.complete(&request).unwrap();
        assert_eq!(response.text(), expected);
        assert_eq!(response.usage.cached_input, Some(0));
        *loaded_slot() = None;
    }

    #[test]
    fn lookup_proposes_what_followed_the_latest_repeat_of_the_tail() {
        // ... 7 8 9 | 1 2 3 4 5 | 7 8 → propose 9 then what came after 9 earlier.
        let history = [1, 2, 7, 8, 9, 1, 2, 3, 4, 5, 6, 7, 8];
        assert_eq!(lookup_draft(&history, 3), vec![9, 1, 2]);
        assert_eq!(lookup_draft(&history, 0), Vec::<u32>::new());
        assert!(
            lookup_draft(&[1, 2, 3, 4], 4).is_empty(),
            "no earlier occurrence"
        );
        // The longest matching tail wins over a shorter, more recent one.
        let history = [5, 6, 7, 100, 9, 6, 7, 200, 5, 6, 7];
        assert_eq!(lookup_draft(&history, 1), vec![100]);
    }

    /// `LOOM_MLX_SMOKE=<id> cargo test --release -- --ignored mlx_lookup --nocapture`
    #[test]
    #[ignore]
    fn mlx_lookup_speeds_up_rewrites_without_changing_greedy_output() {
        let id = std::env::var("LOOM_MLX_SMOKE").expect("set LOOM_MLX_SMOKE");
        let source = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/models/scripted.rs"
        ))
        .unwrap();
        let source: String = source.lines().take(60).collect::<Vec<_>>().join("\n");
        let provider = MlxProvider::discover();
        let request = Request {
            model: id,
            system: Some("You edit code. Reply with the complete file only.".into()),
            messages: vec![Message::user(format!(
                "Here is a file:\n```rust\n{source}\n```\nRewrite the complete file, renaming `ScriptedModel` to `ReplayModel`. Output only the code."
            ))],
            max_output_tokens: Some(700),
            temperature: Some(0.0),
            reasoning_effort: Some("none".into()),
            ..Request::default()
        };
        let mut texts = Vec::new();
        for lookup in ["0", "1"] {
            unsafe { std::env::set_var("LOOM_MLX_LOOKUP", lookup) };
            let started = Instant::now();
            let response = provider.complete(&request).unwrap();
            let output = response.usage.output.unwrap_or(0);
            eprintln!(
                "lookup={lookup}: {output} tokens in {:?}",
                started.elapsed()
            );
            texts.push(response.text());
        }
        assert_eq!(texts[0], texts[1], "lookup changed greedy output");
    }
}
