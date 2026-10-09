//! Chat on native LMX v2 files with Loom's own engine (`lmx_engine`).
//!
//! Installed models live in `<models>/lmx-native/<identity>.lmx`. One model stays loaded.
//! Agent loops resend a growing conversation, so the engine keeps the live token sequence
//! and checkpoints the sequence state at the end of every prompt: the next request only
//! evaluates what follows the longest shared prefix. Hybrid (recurrent) models cannot cut
//! their state at an arbitrary position, so a checkpoint is what makes that reuse possible.

use super::lmx_engine::{Engine, EngineOptions, EngineStats};
use super::lmx_native::Manifest;
use super::lmx_native_file::NativeFile;
use super::native_runtime::{SparkOutput, THINK_OPEN, check_limits, parse_tool_calls_for_tools, render_template, render_template_prompt};
use super::*;
use std::{
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard},
    time::Instant,
};

/// Prompt-end checkpoints kept per loaded model (each holds the recurrent state).
const CHECKPOINTS: usize = 4;
/// Shorter shared prefixes are cheaper to recompute than to restore.
const MIN_DISK_PREFIX: usize = 256;
/// Saved prefixes kept per model (most recently used).
const DISK_PREFIXES: usize = 8;

#[derive(Clone, Debug)]
pub struct NativeModel {
    pub path: PathBuf,
    pub identity: String,
    pub manifest: Manifest,
}

impl NativeModel {
    pub fn id(&self) -> String {
        format!("lmx/{}", self.identity)
    }

    /// Source file stem, for model lists.
    pub fn name(&self) -> String {
        let source = Path::new(&self.manifest.provenance.source);
        source
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| self.identity.chars().take(12).collect())
    }

    /// Name the router and catalog use: `native/<source relative to the models dir>`.
    pub fn display_name(&self) -> String {
        let source = &self.manifest.provenance.source;
        let models = local::models_dir();
        let relative = Path::new(source)
            .strip_prefix(&models)
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| self.name());
        format!("native/{relative}")
    }

    pub fn matches(&self, name: &str) -> bool {
        name == self.id()
            || name == self.identity
            || name == self.display_name()
            || name.strip_prefix("lmx/").is_some_and(|rest| rest == self.display_name() || rest == self.identity)
    }
}

pub fn directory() -> PathBuf {
    local::models_dir().join("lmx-native")
}

/// Installed native models that this build can run. Unreadable files are skipped.
/// Opening a file parses and validates its whole index (tens of thousands of tensors), so
/// results are cached by path, size and modification time; model lists call this often.
pub fn installed() -> Vec<NativeModel> {
    if !Engine::available() {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(directory()) else {
        return Vec::new();
    };
    type Key = (PathBuf, u64, std::time::SystemTime);
    static CACHE: Mutex<Vec<(Key, Option<NativeModel>)>> = Mutex::new(Vec::new());
    let keys: Vec<Key> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "lmx"))
        .filter_map(|e| {
            let meta = e.metadata().ok()?;
            Some((e.path(), meta.len(), meta.modified().ok()?))
        })
        .collect();
    let mut cache = CACHE.lock().unwrap_or_else(|p| p.into_inner());
    cache.retain(|(key, _)| keys.contains(key));
    let mut out = Vec::new();
    for key in keys {
        if let Some((_, model)) = cache.iter().find(|(k, _)| *k == key) {
            out.extend(model.clone());
            continue;
        }
        let model = (|| {
            let file = NativeFile::open(&key.0).ok()?;
            let runnable = super::lmx_engine::runnable(file.manifest.model.architecture);
            runnable.then(|| NativeModel { path: key.0.clone(), identity: file.identity.clone(), manifest: file.manifest })
        })();
        out.extend(model.clone());
        cache.push((key, model));
    }
    out.sort_by_key(NativeModel::id);
    out
}

/// Loads `name` in the background when it is an installed native model, so its first
/// request does not pay for loading and expert warm-up. Cheap no-op for other names.
pub fn preload(name: &str) {
    let name = name.strip_prefix("lmx/").unwrap_or(name);
    if !name.starts_with("native/") && !name.chars().all(|c| c.is_ascii_hexdigit()) || name.is_empty() {
        return;
    }
    let name = name.to_owned();
    std::thread::spawn(move || {
        let Some(model) = by_name(&name) else { return };
        let mut slot = live();
        let _ = load(&mut slot, &model);
    });
}

/// What `install_gguf` produced.
#[derive(Clone, Debug)]
pub struct Installed {
    /// Router model name (`native/...` or the v1 LMX display name).
    pub name: String,
    pub path: PathBuf,
    /// Run by Loom's native engine (LMX v2); otherwise an LMX v1 file.
    pub native: bool,
    pub removed_source: bool,
}

/// Converts a downloaded GGUF into an installed LMX: a native (v2) file when the native
/// engine runs its architecture, else an LMX v1 file. The source is removed only after the
/// new file opened and verified, and only when `keep_source` is false.
pub fn install_gguf(source: &Path, keep_source: bool, progress: &mut dyn FnMut(u64, u64) -> bool) -> anyhow::Result<Installed> {
    use anyhow::Context as _;
    let source = std::fs::canonicalize(source)?;
    let runnable = Engine::available()
        && matches!(
            super::lmx_native_gguf::architecture_of(&source).as_deref(),
            Some("qwen35moe" | "qwen35" | "lfm2" | "lfm2moe")
        );
    let installed = if runnable {
        let dir = directory();
        std::fs::create_dir_all(&dir)?;
        let stem = source.file_stem().and_then(|s| s.to_str()).context("source file name")?;
        let output = (0..)
            .map(|n| dir.join(if n == 0 { format!("{stem}.lmx") } else { format!("{stem}-{n}.lmx") }))
            .find(|p| !p.exists())
            .context("no free output name")?;
        let file = super::lmx_native_gguf::import_gguf(&source, &output, progress)?;
        let model = NativeModel { path: output.clone(), identity: file.identity.clone(), manifest: file.manifest.clone() };
        Installed { name: model.display_name(), path: output, native: true, removed_source: false }
    } else {
        let prepared = super::lmx::prepare(&source, progress)?;
        let file = super::lmx::LmxFile::open(&prepared.path)?;
        file.verify(progress)?;
        Installed { name: file.display_name(), path: prepared.path, native: false, removed_source: false }
    };
    if keep_source {
        return Ok(installed);
    }
    std::fs::remove_file(&source).with_context(|| format!("remove {}", source.display()))?;
    Ok(Installed { removed_source: true, ..installed })
}

/// Deletes an installed native model and its saved prompt prefixes.
pub fn remove(model: &NativeModel) -> anyhow::Result<()> {
    use anyhow::Context as _;
    std::fs::remove_file(&model.path).with_context(|| format!("remove {}", model.path.display()))?;
    let prefixes = directory().join(".prefix").join(&model.identity[..16.min(model.identity.len())]);
    if prefixes.exists() {
        std::fs::remove_dir_all(&prefixes).with_context(|| format!("remove {}", prefixes.display()))?;
    }
    Ok(())
}

pub fn by_name(name: &str) -> Option<NativeModel> {
    installed().into_iter().find(|m| m.matches(name))
}

struct Live {
    identity: String,
    engine: Engine,
    tokenizer: tokenizers::Tokenizer,
    /// Tokens whose state the engine holds, in order.
    tokens: Vec<u32>,
    /// (tokens up to the checkpoint, engine checkpoint id)
    checkpoints: Vec<(Vec<u32>, i32)>,
}

impl Live {
    /// Moves the engine to the longest reusable prefix of `next` (at most `next.len() - 1`
    /// so at least one token is evaluated for logits). Returns the reused length.
    fn reuse(&mut self, next: &[u32]) -> anyhow::Result<usize> {
        let limit = next.len().saturating_sub(1);
        let shared = self.tokens.iter().zip(next).take_while(|(a, b)| a == b).count().min(limit);
        if shared == self.tokens.len() {
            return Ok(shared);
        }
        // The live sequence diverges: return to the longest checkpoint that is a prefix.
        let best = self
            .checkpoints
            .iter()
            .enumerate()
            .filter(|(_, (prefix, _))| prefix.len() <= limit && next.starts_with(prefix))
            .max_by_key(|(_, (prefix, _))| prefix.len())
            .map(|(i, _)| i);
        match best {
            Some(i) => {
                let (prefix, id) = self.checkpoints[i].clone();
                self.engine.rollback(id)?;
                self.tokens = prefix;
            }
            None => {
                self.engine.reset();
                self.checkpoints.clear();
                self.tokens.clear();
            }
        }
        // Checkpoints past the new position will be overwritten.
        let keep = self.tokens.len();
        let (stale, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut self.checkpoints)
            .into_iter()
            .partition(|(prefix, _)| prefix.len() > keep);
        for (_, id) in stale {
            self.engine.forget(id);
        }
        self.checkpoints = kept;
        Ok(self.tokens.len())
    }

    fn remember(&mut self) -> anyhow::Result<()> {
        if self.checkpoints.iter().any(|(prefix, _)| prefix == &self.tokens) {
            return Ok(());
        }
        if self.checkpoints.len() >= CHECKPOINTS {
            let (_, id) = self.checkpoints.remove(0);
            self.engine.forget(id);
        }
        let id = self.engine.checkpoint()?;
        self.checkpoints.push((self.tokens.clone(), id));
        Ok(())
    }

    fn eval(&mut self, tokens: &[u32]) -> anyhow::Result<Vec<f32>> {
        let logits = self.engine.eval(tokens)?;
        self.tokens.extend_from_slice(tokens);
        Ok(logits)
    }
}

static LIVE: Mutex<Option<Live>> = Mutex::new(None);

fn live() -> MutexGuard<'static, Option<Live>> {
    LIVE.lock().unwrap_or_else(|poisoned| {
        // A panic mid-request may leave the sequence in any state; start it over.
        let mut slot = poisoned.into_inner();
        if let Some(live) = slot.as_mut() {
            live.engine.reset();
            live.tokens.clear();
            live.checkpoints.clear();
        }
        slot
    })
}

/// Frees the engine unless a request is using it (another runtime is loading).
pub(super) fn release_idle() {
    if let Ok(mut slot) = LIVE.try_lock() {
        *slot = None;
    }
}

/// ggml frees its backend devices from C++ static destructors at exit and aborts if a
/// buffer is still alive. `atexit` handlers registered after ggml initialized run first.
extern "C" fn release_at_exit() {
    if let Ok(mut slot) = LIVE.try_lock() {
        *slot = None;
    }
}

fn transient(error: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::Transient, error.to_string())
}

fn bad(error: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(ErrorKind::BadRequest, error.to_string())
}

/// `<models>/lmx-native/.prefix/<identity>/<key>.seq`, keyed by the engine settings that
/// shape the state and by the exact tokens.
fn prefix_path(model: &NativeModel, tokens: &[u32]) -> PathBuf {
    use sha2::{Digest, Sha256};
    let options = EngineOptions::default();
    let mut hash = Sha256::new();
    hash.update(model.identity.as_bytes());
    hash.update(options.kv.as_bytes());
    hash.update([options.kv_rotate as u8]);
    for t in tokens {
        hash.update(t.to_le_bytes());
    }
    let key: String = hash.finalize().iter().take(16).map(|b| format!("{b:02x}")).collect();
    directory().join(".prefix").join(&model.identity[..16.min(model.identity.len())]).join(format!("{key}.seq"))
}

fn touch(path: &Path) {
    if let Ok(file) = std::fs::File::options().append(true).open(path) {
        let _ = file.set_modified(std::time::SystemTime::now());
    }
}

/// Best effort: a failure only costs the next session a recompute.
fn save_prefix(live: &mut Live, model: &NativeModel, tokens: &[u32]) {
    let path = prefix_path(model, tokens);
    if path.exists() {
        touch(&path);
        return;
    }
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let partial = path.with_extension("partial");
    if live.engine.save_state(&partial).is_ok() && std::fs::rename(&partial, &path).is_ok() {
        // Keep the most recently used few.
        if let Ok(entries) = std::fs::read_dir(dir) {
            let mut files: Vec<_> = entries
                .flatten()
                .filter(|e| e.path().extension().is_some_and(|x| x == "seq"))
                .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
                .collect();
            files.sort();
            let excess = files.len().saturating_sub(DISK_PREFIXES);
            for (_, old) in files.into_iter().take(excess) {
                let _ = std::fs::remove_file(old);
            }
        }
    } else {
        let _ = std::fs::remove_file(&partial);
    }
}

fn load<'a>(slot: &'a mut Option<Live>, model: &NativeModel) -> Result<&'a mut Live, ProviderError> {
    if slot.as_ref().is_some_and(|l| l.identity == model.identity) {
        return Ok(slot.as_mut().unwrap());
    }
    *slot = None; // free the previous model first
    // One local model in memory at a time: drop llama.cpp's and MLX's if they are idle.
    super::native_runtime::release_idle();
    #[cfg(loom_mlx)]
    super::mlx_runtime::release_idle();
    let file = NativeFile::open(&model.path).map_err(bad)?;
    ensure_same(&file, model)?;
    let mut bytes = vec![0u8; file.manifest.tokenizer.data.bytes as usize];
    file.read_range_into(&file.manifest.tokenizer.data, &mut bytes).map_err(transient)?;
    let tokenizer = tokenizers::Tokenizer::from_bytes(&bytes).map_err(|e| bad(format!("LMX tokenizer: {e}")))?;
    drop(file);
    let engine = Engine::open(&model.path, &EngineOptions::default()).map_err(transient)?;
    static AT_EXIT: std::sync::Once = std::sync::Once::new();
    AT_EXIT.call_once(|| unsafe {
        libc::atexit(release_at_exit);
    });
    *slot = Some(Live { identity: model.identity.clone(), engine, tokenizer, tokens: Vec::new(), checkpoints: Vec::new() });
    Ok(slot.as_mut().unwrap())
}

fn ensure_same(file: &NativeFile, model: &NativeModel) -> Result<(), ProviderError> {
    if file.identity == model.identity {
        Ok(())
    } else {
        Err(bad("native LMX model changed since discovery"))
    }
}

/// Stats of the loaded engine, if the model is loaded.
pub fn stats(model: &NativeModel) -> Option<EngineStats> {
    live().as_ref().filter(|l| l.identity == model.identity).map(|l| l.engine.stats())
}

/// Temperature + nucleus sampling; greedy at temperature 0.
struct Sampler {
    temperature: f32,
    top_p: f32,
    state: u64,
}

impl Sampler {
    fn new(temperature: f32) -> Self {
        Self { temperature, top_p: 0.95, state: 0x100D_F00D_5EED_1234 }
    }

    fn next_f32(&mut self) -> f32 {
        self.state ^= self.state << 13;
        self.state ^= self.state >> 7;
        self.state ^= self.state << 17;
        (self.state >> 40) as f32 / (1u64 << 24) as f32
    }

    fn sample(&mut self, logits: &[f32]) -> u32 {
        let argmax = || logits.iter().enumerate().max_by(|a, b| a.1.total_cmp(b.1)).map_or(0, |(i, _)| i as u32);
        if self.temperature <= 0.0 {
            return argmax();
        }
        let max = logits.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        let mut probs: Vec<(u32, f32)> = logits
            .iter()
            .enumerate()
            .map(|(i, &l)| (i as u32, ((l - max) / self.temperature).exp()))
            .filter(|(_, p)| *p > 1e-9)
            .collect();
        probs.sort_unstable_by(|a, b| b.1.total_cmp(&a.1));
        let total: f32 = probs.iter().map(|(_, p)| p).sum();
        let mut kept = 0.0;
        let mut cut = probs.len();
        for (i, (_, p)) in probs.iter().enumerate() {
            kept += p / total;
            if kept >= self.top_p {
                cut = i + 1;
                break;
            }
        }
        probs.truncate(cut.max(1));
        let mass: f32 = probs.iter().map(|(_, p)| p).sum();
        let mut target = self.next_f32() * mass;
        for (id, p) in &probs {
            target -= p;
            if target <= 0.0 {
                return *id;
            }
        }
        probs[0].0
    }
}

/// Byte-level BPE pieces can split a UTF-8 character; emit text only once it is whole.
struct Detokenizer {
    ids: Vec<u32>,
    prefix: usize,
    read: usize,
}

impl Detokenizer {
    fn push(&mut self, tokenizer: &tokenizers::Tokenizer, id: u32) -> String {
        self.ids.push(id);
        let before = tokenizer.decode(&self.ids[self.prefix..self.read], false).unwrap_or_default();
        let after = tokenizer.decode(&self.ids[self.prefix..], false).unwrap_or_default();
        if after.len() > before.len() && !after.ends_with('\u{FFFD}') && after.starts_with(&before) {
            self.prefix = self.read;
            self.read = self.ids.len();
            after[before.len()..].to_owned()
        } else {
            String::new()
        }
    }
}

pub fn infer(
    model: &NativeModel,
    request: &Request,
    observe: &mut dyn FnMut(StreamEvent),
) -> Result<Response, ProviderError> {
    request.check_cancelled()?;
    let started = Instant::now();
    let mut slot = live();
    let live = load(&mut slot, model)?;
    let loaded_at = started.elapsed();

    let thinking = request.reasoning_effort.as_deref().is_some_and(|effort| effort != "none");
    let t = &model.manifest.tokenizer;
    let piece = |id: Option<u32>| id.and_then(|id| live.tokenizer.id_to_token(id)).unwrap_or_default();
    let bos = if t.add_bos { piece(t.bos) } else { String::new() };
    let eos = piece(t.eos.first().copied());
    let prompt = render_template_prompt(&model.manifest.prompt.template, request, thinking, &bos, &eos)
        .map_err(|error| bad(format!("chat template of {}: {error}", model.name())))?;
    let mut tokens: Vec<u32> = live
        .tokenizer
        .encode(prompt.as_str(), false)
        .map_err(|e| bad(format!("tokenize: {e}")))?
        .get_ids()
        .to_vec();
    if t.add_bos && !prompt.starts_with(&bos) {
        if let Some(b) = t.bos {
            tokens.insert(0, b);
        }
    }
    if tokens.is_empty() {
        return Err(bad("native prompt tokenized to zero tokens"));
    }
    let max_output = request.max_output_tokens.unwrap_or(2048).max(1) as usize;
    let limit = live.engine.stats().context as usize;
    if tokens.len() + max_output > limit {
        return Err(bad(format!(
            "request ({} tokens) exceeds the native context ({limit} tokens); set LOOM_LMX_CONTEXT to raise it",
            tokens.len() + max_output
        )));
    }

    // The conversation without the generation prompt is the next turn's stable prefix
    // (templates often rewrite the generation prompt and the answer, e.g. drop <think>).
    let template = &model.manifest.prompt.template;
    // Length of `tokens` that `text` tokenizes to, when it is a whole prefix of them.
    let prefix_of = |text: String, whole: bool| -> Option<usize> {
        let enc = live.tokenizer.encode(text.as_str(), false).ok()?;
        let ids = enc.get_ids();
        let offset = usize::from(tokens.first() != ids.first());
        let shared = tokens[offset..].iter().zip(ids).take_while(|(a, b)| a == b).count();
        (!whole || shared == ids.len()).then_some(shared + offset).filter(|&n| n > 0 && n < tokens.len())
    };
    // The conversation without the generation prompt is the next turn's stable prefix
    // (templates often rewrite the generation prompt and the answer, e.g. drop <think>).
    let boundary = render_template(template, request, thinking, &bos, &eos, false).ok().and_then(|t| prefix_of(t, false));
    // System prompt and tool definitions alone: shared by every session of this agent, so
    // their state is kept on disk and restored instead of recomputed.
    // (Templates may refuse a conversation without a user turn, so render one with a
    // placeholder and keep what precedes its text.)
    let shared = {
        let head = Request { messages: vec![Message::user("\u{1}")], tools: request.tools.clone(), ..request.clone() };
        render_template(template, &head, thinking, &bos, &eos, false).ok().and_then(|t| prefix_of(t, false))
    }
    .filter(|&n| n >= MIN_DISK_PREFIX);

    let mut reused = live.reuse(&tokens).map_err(transient)?;
    if let Some(n) = shared.filter(|&n| reused < n) {
        let path = prefix_path(model, &tokens[..n]);
        if live.engine.load_state(&path).map_err(transient)? {
            live.tokens = tokens[..n].to_vec();
            live.checkpoints.clear(); // the engine dropped them
            reused = n;
            touch(&path);
            live.remember().map_err(transient)?;
        } else {
            live.tokens.clear();
            live.checkpoints.clear();
            reused = live.reuse(&tokens).map_err(transient)?;
        }
    }
    let restored_at = started.elapsed();
    // A local model's time limit is for a stall, not for the whole answer: loading,
    // each prompt slice and each token are progress, and restart the clock.
    let mut progress = Instant::now();
    let mut logits = Vec::new();
    // Prefill in engine-sized slices so cancellation and timeouts are honoured; checkpoint
    // at the shared prefix (also saved to disk), the conversation boundary and the end.
    let slice = live.engine.stats().chunk.max(1) as usize;
    let mut at = reused;
    for stop in shared.into_iter().chain(boundary).chain([tokens.len()]) {
        if stop <= at {
            continue;
        }
        for chunk in tokens[at..stop].chunks(slice) {
            check_limits(request, progress)?;
            logits = live.eval(chunk).map_err(transient)?;
            progress = Instant::now();
        }
        at = stop;
        live.remember().map_err(transient)?;
        if Some(stop) == shared {
            save_prefix(live, model, &tokens[..stop]);
        }
    }

    if std::env::var_os("LOOM_LMX_TRACE").is_some() {
        eprintln!(
            "lmx trace: load {:.2}s, prefix restore {:.2}s, prefill {} tokens {:.2}s, {:?}",
            loaded_at.as_secs_f64(),
            (restored_at - loaded_at).as_secs_f64(),
            tokens.len() - reused,
            (started.elapsed() - restored_at).as_secs_f64(),
            live.engine.stats()
        );
    }
    let mut sampler = Sampler::new(request.temperature.unwrap_or(0.2));
    let opened = prompt.trim_end().ends_with(THINK_OPEN);
    let mut output = SparkOutput::new(opened);
    if model.manifest.prompt.template.contains("<function=") {
        output = output.with_xml_tools(&request.tools);
    }
    let mut detok = Detokenizer { ids: Vec::new(), prefix: 0, read: 0 };
    let mut generated = 0usize;
    let mut hit_eog = false;
    let decode_started = Instant::now();
    let mut last_speed = Instant::now();
    while generated < max_output {
        if last_speed.elapsed().as_millis() >= 500 {
            last_speed = Instant::now();
            observe(StreamEvent::Speed { tokens: generated as u64, seconds: decode_started.elapsed().as_secs_f64() });
        }
        check_limits(request, progress)?;
        let token = sampler.sample(&logits);
        if t.eog.contains(&token) {
            hit_eog = true;
            break;
        }
        let text = detok.push(&live.tokenizer, token);
        for delta in output.feed(&text) {
            if !delta.is_empty() {
                observe(StreamEvent::TextDelta(delta));
            }
        }
        generated += 1;
        logits = live.eval(&[token]).map_err(transient)?;
        progress = Instant::now();
    }
    if generated > 0 {
        observe(StreamEvent::Speed { tokens: generated as u64, seconds: decode_started.elapsed().as_secs_f64() });
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
    parts.extend(parse_tool_calls_for_tools(&output.tools, &request.tools));
    let stop = if parts.iter().any(|part| matches!(part, Part::ToolCall { .. })) {
        Stop::ToolUse
    } else if hit_eog {
        Stop::EndTurn
    } else {
        Stop::MaxTokens
    };
    let usage = Usage { input: Some(tokens.len() as u64), cached_input: Some(reused as u64), output: Some(generated as u64) };
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nucleus_sampling_is_greedy_at_zero_and_stays_in_the_nucleus() {
        let logits = [0.0, 5.0, 1.0, 4.9];
        assert_eq!(Sampler::new(0.0).sample(&logits), 1);
        let mut s = Sampler::new(0.7);
        for _ in 0..200 {
            assert!(matches!(s.sample(&logits), 1 | 3));
        }
    }

    /// The installed native model the ignored tests use: `LOOM_LMX_NATIVE_MODEL` (a name
    /// fragment) or the first one.
    fn test_model() -> NativeModel {
        let wanted = std::env::var("LOOM_LMX_NATIVE_MODEL").unwrap_or_default();
        installed()
            .into_iter()
            .find(|m| m.display_name().contains(&wanted))
            .expect("an installed native LMX")
    }

    /// A follow-up request reuses the previous prompt through its checkpoint even though
    /// the template rewrote the previous answer. `LOOM_LMX_NATIVE_CHAT=1 cargo test
    /// --release --lib native_chat_reuses_prompt_prefix -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn native_chat_reuses_prompt_prefix() {
        std::env::var("LOOM_LMX_NATIVE_CHAT").expect("set LOOM_LMX_NATIVE_CHAT=1");
        let model = test_model();
        let mut request = Request {
            model: model.id(),
            system: Some("You are a terse assistant.".into()),
            messages: vec![Message::user("Name one prime number greater than 10. Reply with the number only.")],
            max_output_tokens: Some(std::env::var("LOOM_LMX_TEST_TOKENS").ok().and_then(|v| v.parse().ok()).unwrap_or(16)),
            temperature: Some(0.0),
            ..Request::default()
        };
        let started = Instant::now();
        let first = infer(&model, &request, &mut |_| {}).unwrap();
        eprintln!("first {:?} {:?} in {:.1}s", first.parts, first.usage, started.elapsed().as_secs_f64());
        request.messages.push(Message { role: Role::Assistant, parts: first.parts.clone() });
        request.messages.push(Message::user("And one greater than 100?"));
        let started = Instant::now();
        let second = infer(&model, &request, &mut |_| {}).unwrap();
        eprintln!("second {:?} {:?} in {:.1}s", second.parts, second.usage, started.elapsed().as_secs_f64());
        assert!(second.usage.cached_input.unwrap() > first.usage.input.unwrap() / 2, "{:?}", second.usage);
        eprintln!("{:?}", stats(&model));
    }

    /// Time to first token of an agent-sized request (long system prompt and tools) in a
    /// fresh process. Run twice: the second run restores the shared prefix from disk.
    /// `LOOM_LMX_NATIVE_CHAT=1 cargo test --release --lib first_token_latency -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn first_token_latency() {
        std::env::var("LOOM_LMX_NATIVE_CHAT").expect("set LOOM_LMX_NATIVE_CHAT=1");
        let model = test_model();
        let rules: String = (0..60)
            .map(|i| format!("Rule {i}: keep edits minimal, explain the change, run the tests that cover file number {i}.\n"))
            .collect();
        let tools = ["read", "write", "search", "run", "list"]
            .iter()
            .map(|name| ToolSpec {
                name: (*name).into(),
                description: format!("{name} files in the workspace. Paths are relative to the repository root."),
                parameters: serde_json::json!({"type":"object","properties":{"path":{"type":"string"},"query":{"type":"string"}},"required":["path"]}),
            })
            .collect();
        let request = Request {
            model: model.id(),
            system: Some(format!("You are Loom, a coding agent.\n{rules}")),
            tools,
            messages: vec![Message::user("Which file holds the main function? Answer in one short sentence.")],
            max_output_tokens: Some(24),
            temperature: Some(0.0),
            ..Request::default()
        };
        let started = Instant::now();
        let mut first = None;
        let response = infer(&model, &request, &mut |event| {
            if matches!(event, StreamEvent::TextDelta(_)) && first.is_none() {
                first = Some(started.elapsed());
            }
        })
        .unwrap();
        eprintln!(
            "first token after {:.2}s (load included), total {:.2}s, {:?}",
            first.unwrap_or_default().as_secs_f64(),
            started.elapsed().as_secs_f64(),
            response.usage
        );
    }
}

#[cfg(test)]
mod open_cost {
    /// `cargo test --release --lib native_open_cost -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn native_open_cost() {
        let t = std::time::Instant::now();
        let models = super::installed();
        eprintln!("installed(): {} models in {:.0} ms", models.len(), t.elapsed().as_secs_f64() * 1e3);
    }
}
