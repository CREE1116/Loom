//! Import-only MLX/HF parsing. Native execution needs none of these source files.
//! MLX affine 3/5/6-bit codes are continuous LE row bitstreams, including across
//! U32 boundaries (verified against MLX 0.32.3 Metal quantize/dequantize kernels).
use super::{
    lmx_native::*,
    lmx_native_file::{self, NativeFile},
    local,
    model_package::{self, TensorSpec},
};
use anyhow::{Context, Result, bail, ensure};
use serde_json::Value;
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File, Metadata},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

const SCRATCH: usize = 64 << 10;

struct SourceFile {
    file: File,
    metadata: Metadata,
}
#[derive(Clone)]
struct Slice {
    file: String,
    offset: u64,
    bytes: u64,
    destination: u64,
}

fn ordinary(path: &Path) -> Result<File> {
    local::CheckpointLocation::checked(path)?.open(false, false)
}

fn asset(source: &Path, name: &str, optional: bool) -> Result<Option<Vec<u8>>> {
    let path = source.join(name);
    if optional
        && matches!(fs::symlink_metadata(&path), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
    {
        return Ok(None);
    }
    let mut file = ordinary(&path).with_context(|| format!("read {name}"))?;
    ensure!(
        file.metadata()?.len() <= METADATA_LIMIT,
        "metadata too large: {name}"
    );
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take(METADATA_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= METADATA_LIMIT,
        "metadata too large: {name}"
    );
    Ok(Some(bytes))
}

fn json(source: &Path, name: &str, optional: bool) -> Result<Value> {
    asset(source, name, optional)?
        .map(|b| serde_json::from_slice(&b).with_context(|| format!("invalid {name}")))
        .transpose()
        .map(|v| v.unwrap_or(Value::Null))
}

fn boolean(config: &Value, key: &str, default: bool) -> Result<bool> {
    if config[key].is_null() {
        Ok(default)
    } else {
        config[key]
            .as_bool()
            .with_context(|| format!("invalid {key}"))
    }
}

fn required_boolean(config: &Value, key: &str) -> Result<bool> {
    config[key]
        .as_bool()
        .with_context(|| format!("missing or invalid {key}"))
}

fn required_positive_float(config: &Value, key: &str) -> Result<f64> {
    config[key]
        .as_f64()
        .filter(|v| v.is_finite() && *v > 0.0)
        .with_context(|| format!("missing or invalid {key}"))
}

fn dimension(config: &Value, key: &str, default: Option<u32>) -> Result<u32> {
    if config[key].is_null() {
        return default.with_context(|| format!("missing {key}"));
    }
    Ok(u32::try_from(model_package::positive(config, key)?)?)
}

fn model(config: &Value) -> Result<Model> {
    model_package::validate_mlx_config(config)?;
    let architecture = match config["model_type"].as_str() {
        Some("qwen2") => Architecture::Qwen2,
        Some("qwen3") => Architecture::Qwen3,
        Some("qwen3_moe") => Architecture::Qwen3Moe,
        _ => bail!("native importer supports Qwen2/Qwen3/Qwen3 MoE only"),
    };
    if architecture == Architecture::Qwen3Moe {
        ensure!(
            config["decoder_sparse_step"].as_u64() == Some(1),
            "native MoE requires explicit decoder_sparse_step=1"
        );
        ensure!(
            config["mlp_only_layers"]
                .as_array()
                .is_some_and(|v| v.is_empty()),
            "native MoE requires explicit empty mlp_only_layers"
        );
    }
    ensure!(
        config["decoder_sparse_step"].is_null()
            || config["decoder_sparse_step"].as_u64() == Some(1),
        "native recipe requires every MoE layer to be sparse"
    );
    ensure!(
        config["mlp_only_layers"].is_null()
            || config["mlp_only_layers"]
                .as_array()
                .is_some_and(|v| v.is_empty()),
        "native recipe has no mlp_only_layers"
    );
    for key in [
        "num_shared_experts",
        "n_shared_experts",
        "shared_expert_intermediate_size",
    ] {
        ensure!(
            config[key].is_null() || config[key].as_u64() == Some(0),
            "native recipe has no shared experts: {key}"
        );
    }
    ensure!(
        config["rope_scaling"].is_null() && config["rope_parameters"].is_null(),
        "native recipe has no scaled/custom RoPE"
    );
    ensure!(
        !boolean(config, "rope_traditional", false)?,
        "native recipe requires split-half RoPE"
    );
    ensure!(
        !boolean(config, "attention_output_bias", false)? && !boolean(config, "mlp_bias", false)?,
        "native recipe has no output/MLP biases"
    );
    let hidden = dimension(config, "hidden_size", None)?;
    let heads = dimension(config, "num_attention_heads", None)?;
    // Qwen2's MLX implementation derives this value. Qwen3/MoE declare it
    // explicitly and may have heads*head_dim != hidden (the real 30B does).
    let head_dim = if architecture == Architecture::Qwen2 {
        ensure!(hidden % heads == 0, "invalid Qwen2 head dimension");
        let derived = hidden / heads;
        ensure!(
            config["head_dim"].is_null() || config["head_dim"].as_u64() == Some(derived.into()),
            "Qwen2 head_dim disagrees with hidden/heads"
        );
        derived
    } else {
        dimension(config, "head_dim", None)?
    };
    let feed_forward = if architecture == Architecture::Qwen3Moe {
        FeedForward::Moe {
            intermediate: dimension(config, "moe_intermediate_size", None)?,
            experts: dimension(config, "num_experts", None)?,
            active: dimension(config, "num_experts_per_tok", None)?,
            normalize_topk: required_boolean(config, "norm_topk_prob")?,
        }
    } else {
        FeedForward::Dense {
            intermediate: dimension(config, "intermediate_size", None)?,
        }
    };
    let result = Model {
        recipe_version: 1,
        architecture,
        hidden,
        layers: dimension(config, "num_hidden_layers", None)?,
        vocab: dimension(config, "vocab_size", None)?,
        context: dimension(config, "max_position_embeddings", None)?,
        attention: Attention {
            heads,
            kv_heads: dimension(config, "num_key_value_heads", None)?,
            head_dim,
            qkv_bias: boolean(
                config,
                "attention_bias",
                architecture == Architecture::Qwen2,
            )?,
            qk_norm: architecture != Architecture::Qwen2,
            rope_theta: required_positive_float(config, "rope_theta")?,
        },
        rms_epsilon: required_positive_float(config, "rms_norm_eps")?,
        feed_forward,
        tied_embeddings: required_boolean(config, "tie_word_embeddings")?,
        hybrid: None,
    };
    result.roles()?;
    Ok(result)
}

fn tokenizer(
    source: &Path,
    config: &Value,
    vocab_size: u32,
) -> Result<(Vec<u8>, Tokenizer, Prompt)> {
    let bytes = asset(source, "tokenizer.json", false)?.unwrap();
    let value: Value = serde_json::from_slice(&bytes).context("invalid tokenizer JSON")?;
    ensure!(
        value["model"]["type"] == "BPE",
        "native tokenizer recipe requires BPE"
    );
    ensure!(
        value["model"]["merges"].is_array(),
        "BPE merges must be embedded"
    );
    let vocab = value["model"]["vocab"]
        .as_object()
        .context("BPE vocabulary must be embedded")?;
    ensure!(!vocab.is_empty(), "empty BPE vocabulary");
    let mut tokens = BTreeMap::new();
    let mut ids = BTreeMap::new();
    let mut add = |token: &str, id: &Value| -> Result<()> {
        let id = u32::try_from(id.as_u64().context("invalid tokenizer token ID")?)?;
        ensure!(id < vocab_size, "tokenizer ID outside model vocabulary");
        ensure!(
            tokens
                .insert(token.to_owned(), id)
                .is_none_or(|old| old == id),
            "conflicting tokenizer token ID"
        );
        ensure!(
            ids.insert(id, token.to_owned())
                .is_none_or(|old| old == token),
            "duplicate tokenizer ID"
        );
        Ok(())
    };
    for (token, id) in vocab {
        add(token, id)?;
    }
    if !value["added_tokens"].is_null() {
        for token in value["added_tokens"]
            .as_array()
            .context("invalid added_tokens")?
        {
            add(
                token["content"].as_str().context("invalid added token")?,
                &token["id"],
            )?;
        }
    }
    fn processor_ids(value: &Value, ids: &BTreeMap<u32, String>) -> Result<()> {
        let check = |value: &Value| -> Result<()> {
            let id = u32::try_from(value.as_u64().context("invalid post-processor token ID")?)?;
            ensure!(
                ids.contains_key(&id),
                "post-processor token ID absent from vocabulary"
            );
            Ok(())
        };
        match value["type"].as_str() {
            Some("TemplateProcessing") => {
                for special in value["special_tokens"]
                    .as_object()
                    .context("invalid post-processor special tokens")?
                    .values()
                {
                    for id in special["ids"]
                        .as_array()
                        .context("invalid post-processor special IDs")?
                    {
                        check(id)?;
                    }
                }
            }
            Some("BertProcessing" | "RobertaProcessing") => {
                for key in ["cls", "sep"] {
                    let pair = value[key]
                        .as_array()
                        .context("invalid post-processor special token")?;
                    ensure!(pair.len() == 2, "invalid post-processor special token");
                    check(&pair[1])?;
                }
            }
            Some("Sequence") => {
                for processor in value["processors"]
                    .as_array()
                    .context("invalid tokenizer processors")?
                {
                    processor_ids(processor, ids)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    processor_ids(&value["post_processor"], &ids)?;
    #[cfg(target_os = "macos")]
    tokenizers::Tokenizer::from_bytes(&bytes)
        .map_err(|error| anyhow::anyhow!("invalid self-contained tokenizer: {error}"))?;
    let tc = json(source, "tokenizer_config.json", true)?;
    let generation = json(source, "generation_config.json", true)?;
    let special = |key: &str| -> Result<Option<u32>> {
        if tc[key].is_null() {
            return Ok(None);
        }
        let token = tc[key]
            .as_str()
            .or_else(|| tc[key]["content"].as_str())
            .with_context(|| format!("invalid {key}"))?;
        Ok(Some(
            *tokens
                .get(token)
                .with_context(|| format!("{key} absent from tokenizer"))?,
        ))
    };
    let checked_ids = |v: &Value| -> Result<Vec<u32>> {
        let values: Vec<&Value> = if v.is_null() {
            vec![]
        } else if let Some(v) = v.as_array() {
            v.iter().collect()
        } else {
            vec![v]
        };
        values
            .into_iter()
            .map(|v| {
                let id = u32::try_from(v.as_u64().context("invalid special token ID")?)?;
                ensure!(
                    ids.contains_key(&id),
                    "special token ID absent from tokenizer vocabulary"
                );
                Ok(id)
            })
            .collect()
    };
    let mut bos = None;
    for v in [
        &config["bos_token_id"],
        &tc["bos_token_id"],
        &generation["bos_token_id"],
    ] {
        let candidates = checked_ids(v)?;
        ensure!(candidates.len() <= 1, "multiple BOS IDs are unsupported");
        if let Some(&id) = candidates.first() {
            ensure!(bos.is_none_or(|old| old == id), "conflicting BOS IDs");
            bos = Some(id);
        }
    }
    if let Some(id) = special("bos_token")? {
        ensure!(bos.is_none_or(|old| old == id), "conflicting BOS token");
        bos = Some(id);
    }
    let mut eos = BTreeSet::new();
    for v in [
        &config["eos_token_id"],
        &tc["eos_token_id"],
        &generation["eos_token_id"],
    ] {
        eos.extend(checked_ids(v)?);
    }
    eos.extend(special("eos_token")?);
    ensure!(!eos.is_empty(), "missing EOS token policy");
    let template = match &tc["chat_template"] {
        Value::String(s) => s.clone(),
        Value::Null => String::from_utf8(asset(source, "chat_template.jinja", false)?.unwrap())
            .context("chat template is not UTF-8")?,
        _ => bail!("native recipe requires one string chat template"),
    };
    ensure!(
        !template.trim().is_empty() && template.len() <= 1 << 20,
        "missing/oversized chat template"
    );
    let mut environment = minijinja::Environment::new();
    minijinja_contrib::add_to_environment(&mut environment);
    environment.set_unknown_method_callback(minijinja_contrib::pycompat::unknown_method_callback);
    environment
        .add_template("chat", &template)
        .context("invalid chat template")?;
    let eos: Vec<_> = eos.into_iter().collect();
    let tokenizer = Tokenizer {
        version: 1,
        algorithm: TokenizerAlgorithm::Bpe,
        data: Buffer {
            block: "tokenizer".into(),
            offset: 0,
            bytes: bytes.len() as u64,
        },
        bos,
        eog: eos.clone(),
        eos,
        add_bos: boolean(
            &tc,
            "add_bos_token",
            boolean(config, "add_bos_token", false)?,
        )?,
    };
    ensure!(
        !tokenizer.add_bos || tokenizer.bos.is_some(),
        "add_bos requires a BOS token"
    );
    Ok((
        bytes,
        tokenizer,
        Prompt {
            version: 1,
            template,
        },
    ))
}

fn dtype(name: &str) -> Result<Dtype> {
    match name {
        "F32" => Ok(Dtype::F32),
        "F16" => Ok(Dtype::F16),
        "BF16" => Ok(Dtype::Bf16),
        _ => bail!("unsupported native coefficient dtype: {name}"),
    }
}

fn append(
    block: &str,
    spec: &TensorSpec,
    expert: Option<u32>,
    alignment: u64,
    slices: &mut Vec<Slice>,
    cursor: &mut u64,
) -> Result<Buffer> {
    let (offset, bytes) = if let Some(expert) = expert {
        let count = *spec.shape.first().context("expert bank has no axis")?;
        ensure!(
            u64::from(expert) < count && spec.bytes % count == 0,
            "invalid expert slice"
        );
        let bytes = spec.bytes / count;
        (
            spec.offset
                .checked_add(
                    u64::from(expert)
                        .checked_mul(bytes)
                        .context("expert offset overflow")?,
                )
                .context("expert offset overflow")?,
            bytes,
        )
    } else {
        (spec.offset, spec.bytes)
    };
    let destination = cursor
        .checked_add(alignment - 1)
        .context("block size overflow")?
        / alignment
        * alignment;
    *cursor = destination
        .checked_add(bytes)
        .context("block size overflow")?;
    slices.push(Slice {
        file: spec.file.clone(),
        offset,
        bytes,
        destination,
    });
    Ok(Buffer {
        block: block.into(),
        offset: destination,
        bytes,
    })
}

fn unchanged(source: &SourceFile) -> Result<()> {
    let now = source.file.metadata()?;
    ensure!(
        now.len() == source.metadata.len() && now.modified()? == source.metadata.modified()?,
        "source shard changed during import"
    );
    Ok(())
}

fn identical(
    a: &TensorSpec,
    b: &TensorSpec,
    files: &mut BTreeMap<String, SourceFile>,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<()> {
    ensure!(
        a.dtype == b.dtype && a.shape == b.shape && a.bytes == b.bytes,
        "tied head encoding differs from embedding"
    );
    let mut scratch = [0u8; SCRATCH];
    let (left, right) = scratch.split_at_mut(SCRATCH / 2);
    let mut done = 0;
    while done < a.bytes {
        ensure!(progress(done, a.bytes), "native import cancelled");
        let n = (a.bytes - done).min(left.len() as u64) as usize;
        for (spec, out) in [(a, &mut left[..n]), (b, &mut right[..n])] {
            let source = files.get_mut(&spec.file).context("missing source shard")?;
            unchanged(source)?;
            source.file.seek(SeekFrom::Start(spec.offset + done))?;
            source.file.read_exact(out)?;
        }
        ensure!(
            left[..n] == right[..n],
            "tied head bytes differ from embedding"
        );
        done += n as u64;
    }
    Ok(())
}

/// Normalize supported MLX checkpoints directly to native LMX, without extraction,
/// dequantization, requantization, or retaining whole source weight tensors.
pub fn import_mlx(
    source: &Path,
    output: &Path,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<NativeFile> {
    ensure!(progress(0, 0), "native import cancelled");
    let source: PathBuf = if source.is_absolute() {
        source.into()
    } else {
        std::env::current_dir()?.join(source)
    };
    ensure!(
        !source
            .components()
            .any(|c| c == std::path::Component::ParentDir),
        "source path must not contain parent traversal"
    );
    // Do not canonicalize away a symlink in the source path. This checks every
    // directory component and the leaf with the existing no-follow file opener.
    let config = json(&source, "config.json", false)?;
    let model = model(&config)?;
    let (tokenizer_bytes, tokenizer, prompt) = tokenizer(&source, &config, model.vocab)?;
    let mut specs = BTreeMap::new();
    let mut files = BTreeMap::new();
    let mut paths = Vec::new();
    for entry in fs::read_dir(&source)? {
        let entry = entry?;
        if entry.path().extension().is_some_and(|e| e == "safetensors") {
            paths.push(entry.path());
        }
    }
    paths.sort();
    ensure!(
        !paths.is_empty() && paths.len() <= 1024,
        "invalid source shard count"
    );
    for path in paths {
        ensure!(progress(0, 0), "native import cancelled");
        let name = path
            .file_name()
            .and_then(|s| s.to_str())
            .context("non-UTF8 shard name")?
            .to_owned();
        local::validate_relative_file(&name)?;
        let file = ordinary(&path)?;
        let metadata = file.metadata()?;
        model_package::read_tensors(&path, &name, &mut specs)?;
        let source_file = SourceFile { file, metadata };
        unchanged(&source_file)?;
        files.insert(name, source_file);
    }
    let index = json(&source, "model.safetensors.index.json", true)?;
    if !index.is_null() {
        let map = index["weight_map"]
            .as_object()
            .context("invalid shard weight_map")?;
        ensure!(
            map.len() == specs.len(),
            "shard index must cover every tensor"
        );
        for (name, file) in map {
            ensure!(
                specs
                    .get(name)
                    .is_some_and(|s| Some(s.file.as_str()) == file.as_str()),
                "missing/misplaced shard tensor: {name}"
            );
        }
    } else {
        ensure!(files.len() == 1, "sharded checkpoint requires weight_map");
    }
    for (name, spec) in &specs {
        let file = files
            .get(&spec.file)
            .context("tensor references unknown source shard")?;
        ensure!(
            spec.offset
                .checked_add(spec.bytes)
                .is_some_and(|end| end <= file.metadata.len()),
            "tensor escapes its opened source shard: {name}"
        );
    }
    let mut validation_config = config.clone();
    if specs.contains_key("lm_head.weight") {
        validation_config["tie_word_embeddings"] = Value::Bool(false);
    }
    model_package::validate_tensors(&validation_config, &mut specs)?;
    let mut consumed = BTreeSet::new();
    if model.tied_embeddings && specs.contains_key("lm_head.weight") {
        let embed = &specs["model.embed_tokens.weight"];
        let head = &specs["lm_head.weight"];
        match (&embed.affine, &head.affine) {
            (None, None) => {}
            (Some(a), Some(b)) => {
                ensure!(
                    a.bits == b.bits
                        && a.group_size == b.group_size
                        && a.axis == b.axis
                        && a.packing == b.packing
                        && a.biases.is_some() == b.biases.is_some(),
                    "tied head quantization differs from embedding"
                );
                identical(&specs[&a.scales], &specs[&b.scales], &mut files, progress)?;
                consumed.insert(b.scales.clone());
                if let (Some(a), Some(b)) = (&a.biases, &b.biases) {
                    identical(&specs[a], &specs[b], &mut files, progress)?;
                    consumed.insert(b.clone());
                }
            }
            _ => bail!("tied head storage differs from embedding"),
        }
        identical(embed, head, &mut files, progress)?;
        consumed.insert("lm_head.weight".into());
    }
    let mut tensors = BTreeMap::new();
    let mut blocks = BTreeMap::new();
    let mut plan: BTreeMap<String, Vec<Slice>> = BTreeMap::new();
    for (role, shape) in model.roles()? {
        if model.tied_embeddings && role == Role::Head {
            continue;
        }
        let name = role.hf_name();
        let (source_name, expert, block, kind) = match role {
            Role::Expert { layer, expert, .. } => {
                let source_name = name.replace(&format!(".experts.{expert}."), ".switch_mlp.");
                (
                    source_name,
                    Some(expert),
                    format!("expert.{layer:04}.{expert:06}"),
                    BlockKind::Expert { layer, expert },
                )
            }
            _ => (
                name.clone(),
                None,
                format!("weight.{name}"),
                BlockKind::Resident,
            ),
        };
        let spec = specs
            .get(&source_name)
            .with_context(|| format!("missing required tensor: {source_name}"))?;
        let mut expected = shape.clone();
        if expert.is_some() {
            if let FeedForward::Moe { experts, .. } = model.feed_forward {
                expected.insert(0, experts.into());
            }
        }
        let slices = plan.entry(block.clone()).or_default();
        let b = blocks.entry(block.clone()).or_insert(Block {
            kind,
            offset: 0,
            bytes: 0,
            sha256: String::new(),
        });
        consumed.insert(source_name.clone());
        let storage = if let Some(affine) = &spec.affine {
            let last = expected.last_mut().unwrap();
            *last = last
                .checked_mul(affine.bits)
                .context("packing size overflow")?
                / 32;
            ensure!(
                expected == spec.shape,
                "wrong packed source shape: {source_name}"
            );
            let codes = append(&block, spec, expert, 4, slices, &mut b.bytes)?;
            consumed.insert(affine.scales.clone());
            let scales_spec = &specs[&affine.scales];
            let scales_type = dtype(&scales_spec.dtype)?;
            let scales = TypedBuffer {
                dtype: scales_type,
                data: append(
                    &block,
                    scales_spec,
                    expert,
                    scales_type.bytes(),
                    slices,
                    &mut b.bytes,
                )?,
            };
            let biases = if let Some(name) = &affine.biases {
                consumed.insert(name.clone());
                let spec = &specs[name];
                let dt = dtype(&spec.dtype)?;
                Some(TypedBuffer {
                    dtype: dt,
                    data: append(&block, spec, expert, dt.bytes(), slices, &mut b.bytes)?,
                })
            } else {
                None
            };
            Storage::Affine {
                version: 1,
                bits: u8::try_from(affine.bits)?,
                group_size: u32::try_from(affine.group_size)?,
                codes,
                scales,
                biases,
            }
        } else {
            ensure!(expected == spec.shape, "wrong source shape: {source_name}");
            let dt = dtype(&spec.dtype)?;
            Storage::Dense {
                values: TypedBuffer {
                    dtype: dt,
                    data: append(&block, spec, expert, dt.bytes(), slices, &mut b.bytes)?,
                },
            }
        };
        tensors.insert(
            name,
            Tensor {
                role,
                shape,
                storage,
            },
        );
    }
    if model.tied_embeddings {
        let mut head = tensors[&Role::Embedding.hf_name()].clone();
        head.role = Role::Head;
        tensors.insert(Role::Head.hf_name(), head);
    }
    let extras: Vec<_> = specs
        .keys()
        .filter(|name| !consumed.contains(*name))
        .take(4)
        .collect();
    ensure!(
        extras.is_empty(),
        "source tensors have no supported native role: {extras:?}"
    );
    blocks.insert(
        "tokenizer".into(),
        Block {
            kind: BlockKind::Tokenizer,
            offset: 0,
            bytes: tokenizer_bytes.len() as u64,
            sha256: String::new(),
        },
    );
    let manifest = Manifest {
        version: VERSION,
        model,
        tensors,
        blocks,
        tokenizer,
        prompt,
        provenance: Provenance {
            source: source.to_string_lossy().into(),
            revision: config["_commit_hash"].as_str().map(str::to_owned),
            importer: "loom-mlx-native-v1".into(),
            precision_changed: false,
        },
    };
    let mut scratch = [0u8; SCRATCH];
    let mut emit = |id: &str, writer: &mut dyn Write| -> Result<()> {
        if id == "tokenizer" {
            writer.write_all(&tokenizer_bytes)?;
            return Ok(());
        }
        let mut written = 0;
        for slice in plan.get(id).context("unknown planned block")? {
            ensure!(slice.destination >= written, "overlapping import slices");
            let padding = usize::try_from(slice.destination - written)?;
            ensure!(padding < 4, "unexpected native padding");
            writer.write_all(&[0u8; 3][..padding])?;
            let source = files.get_mut(&slice.file).context("missing source shard")?;
            unchanged(source)?;
            source.file.seek(SeekFrom::Start(slice.offset))?;
            let mut remaining = slice.bytes;
            while remaining > 0 {
                let n = remaining.min(SCRATCH as u64) as usize;
                source.file.read_exact(&mut scratch[..n])?;
                writer.write_all(&scratch[..n])?;
                remaining -= n as u64;
            }
            unchanged(source)?;
            written = slice.destination + slice.bytes;
        }
        Ok(())
    };
    lmx_native_file::write(output, manifest, &mut emit, progress)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    type FixtureTensors = BTreeMap<String, (String, Vec<u64>, Vec<u8>)>;

    fn write_weights(dir: &Path, tensors: &FixtureTensors) {
        let mut header = BTreeMap::new();
        let mut offset = 0;
        for (name, (dtype, shape, bytes)) in tensors {
            header.insert(
                name,
                json!({"dtype":dtype,"shape":shape,"data_offsets":[offset,offset + bytes.len()]}),
            );
            offset += bytes.len();
        }
        let mut header = serde_json::to_vec(&header).unwrap();
        while header.len() % 8 != 0 {
            header.push(b' ');
        }
        let mut file = File::create(dir.join("model.safetensors")).unwrap();
        file.write_all(&(header.len() as u64).to_le_bytes())
            .unwrap();
        file.write_all(&header).unwrap();
        for (_, _, bytes) in tensors.values() {
            file.write_all(bytes).unwrap();
        }
    }

    fn fixture(kind: &str, bits: u64) -> (tempfile::TempDir, FixtureTensors) {
        // /tmp can itself be a symlink on macOS; use its canonical location.
        let dir = tempfile::tempdir().unwrap();
        let path = fs::canonicalize(dir.path()).unwrap();
        let config = json!({"model_type":kind,"hidden_size":64,"intermediate_size":64,
            "moe_intermediate_size":128,"num_hidden_layers":1,"num_attention_heads":1,
            "num_key_value_heads":1,"head_dim":64,"vocab_size":8,"max_position_embeddings":128,
            "tie_word_embeddings":true,"eos_token_id":3,"bos_token_id":2,
            "num_experts":2,"num_experts_per_tok":1,"norm_topk_prob":true,
            "decoder_sparse_step":1,"mlp_only_layers":[],"rope_theta":1000000.0,"rms_norm_eps":1e-6,
            "quantization":{"bits":bits,"group_size":64}});
        fs::write(path.join("config.json"), config.to_string()).unwrap();
        fs::write(path.join("tokenizer.json"), json!({"version":"1.0","truncation":null,
            "padding":null,"added_tokens":[],"normalizer":null,"pre_tokenizer":{"type":"Whitespace"},
            "post_processor":null,"decoder":null,"model":{"type":"BPE","dropout":null,
            "unk_token":null,"continuing_subword_prefix":null,"end_of_word_suffix":null,
            "fuse_unk":false,"byte_fallback":false,"vocab":{"a":0,"b":1,"<bos>":2,"<eos>":3},"merges":[]}}).to_string()).unwrap();
        fs::write(path.join("tokenizer_config.json"), json!({"bos_token":"<bos>","eos_token":"<eos>",
            "add_bos_token":true,"chat_template":"{% for m in messages %}{{ m.content }}{% endfor %}"}).to_string()).unwrap();
        let model = model(&config).unwrap();
        let mut tensors = FixtureTensors::new();
        for (role, mut shape) in model.roles().unwrap() {
            if role == Role::Head {
                continue;
            }
            let mut name = role.hf_name();
            if let Role::Expert { expert, .. } = role {
                if expert != 0 {
                    continue;
                }
                name = name.replace(".experts.0.", ".switch_mlp.");
                shape.insert(0, 2);
            }
            let values = shape.iter().product::<u64>();
            if shape.len() >= 2 && role != Role::Embedding {
                let mut packed = shape.clone();
                *packed.last_mut().unwrap() = shape.last().unwrap() * bits / 32;
                let codes = (0..values * bits / 8)
                    .map(|n| (n.wrapping_mul(43) + n / 7) as u8)
                    .collect();
                tensors.insert(name.clone(), ("U32".into(), packed, codes));
                let mut side = shape;
                *side.last_mut().unwrap() /= 64;
                let size = side.iter().product::<u64>() * 2;
                let prefix = name.strip_suffix(".weight").unwrap();
                for suffix in ["scales", "biases"] {
                    let bytes = (0..size)
                        .map(|n| (n * 17 + if suffix == "scales" { 3 } else { 11 }) as u8)
                        .collect();
                    tensors.insert(
                        format!("{prefix}.{suffix}"),
                        ("BF16".into(), side.clone(), bytes),
                    );
                }
            } else {
                let bytes = (0..values * 4).map(|n| (n * 29 + 1) as u8).collect();
                tensors.insert(name, ("F32".into(), shape, bytes));
            }
        }
        write_weights(&path, &tensors);
        (dir, tensors)
    }

    fn import(dir: &Path) -> Result<NativeFile> {
        let path = fs::canonicalize(dir)?;
        import_mlx(&path, &path.join("model.lmx"), &mut |_, _| true)
    }

    #[test]
    fn odd_bit_experts_preserve_every_component_and_share_one_block() {
        for bits in [3, 5, 6] {
            let (dir, originals) = fixture("qwen3_moe", bits);
            let native = import(dir.path()).unwrap();
            native.verify(&mut |_, _| true).unwrap();
            assert_eq!(native.manifest.expert_blocks().len(), 2);
            assert!(!native.manifest.provenance.precision_changed);
            for expert in 0..2 {
                let id = format!("expert.0000.{expert:06}");
                let block = &native.manifest.blocks[&id];
                assert_eq!(block.offset % ALIGNMENT, 0);
                let mut payload = vec![0; block.bytes as usize];
                native.read_block_into(&id, &mut payload).unwrap();
                for projection in [Projection::Gate, Projection::Up, Projection::Down] {
                    let role = Role::Expert {
                        layer: 0,
                        expert,
                        projection,
                    };
                    let tensor = &native.manifest.tensors[&role.hf_name()];
                    assert_eq!(
                        tensor.shape,
                        if projection == Projection::Down {
                            vec![64, 128]
                        } else {
                            vec![128, 64]
                        }
                    );
                    let bank_name = role
                        .hf_name()
                        .replace(&format!(".experts.{expert}."), ".switch_mlp.");
                    let prefix = bank_name.strip_suffix(".weight").unwrap();
                    let Storage::Affine {
                        codes,
                        scales,
                        biases,
                        bits: actual_bits,
                        ..
                    } = &tensor.storage
                    else {
                        panic!("expected packed expert")
                    };
                    assert_eq!(u64::from(*actual_bits), bits);
                    for (suffix, buffer) in [
                        ("weight", codes),
                        ("scales", &scales.data),
                        ("biases", &biases.as_ref().unwrap().data),
                    ] {
                        assert_eq!(buffer.block, id);
                        let raw = &originals[&format!("{prefix}.{suffix}")].2;
                        let n = raw.len() / 2;
                        assert_eq!(
                            &payload
                                [buffer.offset as usize..(buffer.offset + buffer.bytes) as usize],
                            &raw[expert as usize * n..(expert as usize + 1) * n]
                        );
                    }
                    assert_eq!(scales.dtype, Dtype::Bf16);
                }
            }
            assert_eq!(
                native.manifest.tensors["lm_head.weight"].storage,
                native.manifest.tensors["model.embed_tokens.weight"].storage
            );
        }
    }

    #[test]
    fn dense_qwen_recipes_and_standalone_tokenizer_roundtrip() {
        for kind in ["qwen2", "qwen3"] {
            let (dir, originals) = fixture(kind, 4);
            let native = import(dir.path()).unwrap();
            assert!(native.manifest.expert_blocks().is_empty());
            assert_eq!(native.manifest.tokenizer.bos, Some(2));
            assert_eq!(native.manifest.tokenizer.eos, vec![3]);
            assert!(native.manifest.tokenizer.add_bos);
            let buffer = &native.manifest.tokenizer.data;
            let mut bytes = vec![0; buffer.bytes as usize];
            native.read_block_into(&buffer.block, &mut bytes).unwrap();
            assert_eq!(bytes, fs::read(dir.path().join("tokenizer.json")).unwrap());
            let tensor = &native.manifest.tensors["model.embed_tokens.weight"];
            let Storage::Dense { values } = &tensor.storage else {
                panic!()
            };
            assert_eq!(values.dtype, Dtype::F32);
            let mut bytes = vec![0; values.data.bytes as usize];
            native
                .read_block_into(&values.data.block, &mut bytes)
                .unwrap();
            assert_eq!(bytes, originals["model.embed_tokens.weight"].2);
            // No source parser or external file is needed to reopen and verify.
            fs::remove_file(dir.path().join("config.json")).unwrap();
            fs::remove_file(dir.path().join("tokenizer.json")).unwrap();
            fs::remove_file(dir.path().join("model.safetensors")).unwrap();
            NativeFile::open(&dir.path().join("model.lmx"))
                .unwrap()
                .verify(&mut |_, _| true)
                .unwrap();
        }
    }

    #[test]
    fn tied_head_is_checked_instead_of_silently_discarded() {
        for differs in [false, true] {
            let (dir, mut tensors) = fixture("qwen3", 4);
            let mut head = tensors["model.embed_tokens.weight"].clone();
            if differs {
                head.2[0] ^= 1;
            }
            tensors.insert("lm_head.weight".into(), head);
            write_weights(dir.path(), &tensors);
            let result = import(dir.path());
            assert_eq!(result.is_err(), differs);
            if differs {
                assert!(!dir.path().join("model.lmx").exists());
            }
        }
    }

    #[test]
    fn tied_quantized_head_checks_codes_scales_and_biases() {
        for differs in [None, Some("weight"), Some("scales"), Some("biases")] {
            let (dir, mut tensors) = fixture("qwen3", 3);
            for (suffix, shape, size) in [
                ("weight", vec![8, 6], 192),
                ("scales", vec![8, 1], 16),
                ("biases", vec![8, 1], 16),
            ] {
                let dtype = if suffix == "weight" { "U32" } else { "BF16" };
                let bytes: Vec<u8> = (0..size).map(|i| (i * 37) as u8).collect();
                tensors.insert(
                    format!("model.embed_tokens.{suffix}"),
                    (dtype.into(), shape.clone(), bytes.clone()),
                );
                let mut head = bytes;
                if differs == Some(suffix) {
                    head[size - 1] ^= 1;
                }
                tensors.insert(format!("lm_head.{suffix}"), (dtype.into(), shape, head));
            }
            write_weights(dir.path(), &tensors);
            let result = import(dir.path());
            assert_eq!(result.is_err(), differs.is_some(), "{differs:?}");
            if let Ok(native) = result {
                assert_eq!(
                    native.manifest.tensors["lm_head.weight"].storage,
                    native.manifest.tensors["model.embed_tokens.weight"].storage
                );
            }
        }
    }

    #[test]
    fn missing_template_invalid_bpe_and_special_ids_fail_before_publication() {
        for case in 0..4 {
            let (dir, _) = fixture("qwen3", 4);
            match case {
                0 => {
                    fs::remove_file(dir.path().join("tokenizer_config.json")).unwrap();
                }
                1 | 2 => {
                    let path = dir.path().join("tokenizer.json");
                    let mut v: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                    if case == 1 {
                        v["model"]["type"] = json!("WordLevel");
                    } else {
                        v["model"]["vocab"]["a"] = json!(8);
                    }
                    fs::write(path, v.to_string()).unwrap();
                }
                _ => {
                    let path = dir.path().join("config.json");
                    let mut v: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                    v["eos_token_id"] = json!(7); // Inside padded embedding, absent in tokenizer.
                    fs::write(path, v.to_string()).unwrap();
                }
            }
            assert!(import(dir.path()).is_err(), "case {case}");
            assert!(!dir.path().join("model.lmx").exists());
        }
    }

    #[test]
    fn cancellation_and_noclobber_leave_destination_unchanged() {
        let (dir, _) = fixture("qwen3_moe", 3);
        let source = fs::canonicalize(dir.path()).unwrap();
        let output = source.join("model.lmx");
        assert!(import_mlx(&source, &output, &mut |done, _| done < 1024).is_err());
        assert!(!output.exists());
        fs::write(&output, b"existing").unwrap();
        assert!(import_mlx(&source, &output, &mut |_, _| true).is_err());
        assert_eq!(fs::read(&output).unwrap(), b"existing");
    }

    #[test]
    fn unsupported_recipe_and_unclaimed_tensors_are_rejected() {
        for (key, value) in [
            ("decoder_sparse_step", json!(2)),
            ("mlp_only_layers", json!([0])),
            ("num_shared_experts", json!(1)),
            ("rope_scaling", json!({"type":"linear","factor":2})),
        ] {
            let (dir, _) = fixture("qwen3_moe", 3);
            let path = dir.path().join("config.json");
            let mut config: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            config[key] = value;
            fs::write(path, config.to_string()).unwrap();
            assert!(import(dir.path()).is_err(), "{key}");
            assert!(!dir.path().join("model.lmx").exists());
        }
        let (dir, mut tensors) = fixture("qwen3", 4);
        tensors.insert("unused.weight".into(), ("F32".into(), vec![1], vec![0; 4]));
        write_weights(dir.path(), &tensors);
        assert!(
            import(dir.path())
                .unwrap_err()
                .to_string()
                .contains("no supported native role")
        );
    }

    #[test]
    fn missing_semantic_fields_are_not_replaced_by_engine_defaults() {
        for kind in ["qwen2", "qwen3", "qwen3_moe"] {
            let (dir, _) = fixture(kind, 3);
            let config =
                json(&fs::canonicalize(dir.path()).unwrap(), "config.json", false).unwrap();
            let mut keys = vec![
                "rope_theta",
                "rms_norm_eps",
                "max_position_embeddings",
                "num_key_value_heads",
                "tie_word_embeddings",
            ];
            if kind != "qwen2" {
                keys.push("head_dim");
            }
            if kind == "qwen3_moe" {
                keys.extend(["norm_topk_prob", "decoder_sparse_step", "mlp_only_layers"]);
            }
            for key in keys {
                let mut incomplete = config.clone();
                incomplete.as_object_mut().unwrap().remove(key);
                assert!(model(&incomplete).is_err(), "{kind}: missing {key}");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn source_symlinks_are_rejected() {
        let (dir, _) = fixture("qwen3", 4);
        let path = dir.path().join("config.json");
        let saved = dir.path().join("real-config.json");
        fs::rename(&path, &saved).unwrap();
        std::os::unix::fs::symlink(&saved, &path).unwrap();
        assert!(import(dir.path()).is_err());
    }

    /// Set LOOM_NATIVE_MLX_PREFLIGHT to a checkpoint directory. This exercises
    /// the complete normalization path, cancelling at the writer's first
    /// payload-sized progress callback, BEFORE creating its temporary file.
    #[test]
    #[ignore = "read-only preflight of an explicitly selected local checkpoint"]
    fn local_checkpoint_preflight_without_weight_output() {
        let source =
            std::env::var_os("LOOM_NATIVE_MLX_PREFLIGHT").expect("set LOOM_NATIVE_MLX_PREFLIGHT");
        let source = PathBuf::from(source);
        let config = json(&source, "config.json", false).unwrap();
        let recipe = model(&config).unwrap();
        let role_count = recipe.roles().unwrap().len();
        let (tokenizer_bytes, tokenizer, _) = tokenizer(&source, &config, recipe.vocab).unwrap();
        let output = tempfile::tempdir().unwrap();
        let path = output.path().join("must-not-be-written.lmx");
        let mut total = None;
        let result = import_mlx(&source, &path, &mut |done, bytes| {
            assert_eq!(done, 0, "preflight unexpectedly started copying weights");
            if bytes == 0 {
                true
            } else {
                total = Some(bytes);
                false
            }
        });
        assert!(result.unwrap_err().to_string().contains("cancelled"));
        assert!(!path.exists());
        assert_eq!(fs::read_dir(output.path()).unwrap().count(), 0);
        println!(
            "native MLX preflight OK: {role_count} logical roles, {} payload bytes, {} tokenizer bytes, vocab={}, BOS={:?}, EOS={:?}, recipe={recipe:?}",
            total.expect("normalization must reach writer"),
            tokenizer_bytes.len(),
            recipe.vocab,
            tokenizer.bos,
            tokenizer.eos
        );
    }
}
