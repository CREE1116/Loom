//! Device-independent description and structural validation of preserved MLX payloads.
//! Reads headers only; tensor data is streamed by the importer, never expanded to FP16.
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

pub const MANIFEST: &str = "loom-model.json";
pub const PLAN_VERSION: u32 = 1;
pub const METADATA_LIMIT: u64 = 32 << 20;
const HEADER_LIMIT: u64 = 8 << 20;
const TENSOR_LIMIT: usize = 50_000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct PayloadFile {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct AffineLayout {
    pub bits: u64,
    pub group_size: u64,
    pub axis: i32,
    pub packing: String,
    pub scales: String,
    pub biases: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct TensorSpec {
    pub file: String,
    pub dtype: String,
    pub shape: Vec<u64>,
    /// Absolute byte offset in the safetensors payload, unlike the header's relative offset.
    pub offset: u64,
    pub bytes: u64,
    pub affine: Option<AffineLayout>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ModelManifest {
    pub version: u32,
    pub plan_version: u32,
    pub profile: String,
    pub backend: String,
    pub source: String,
    pub revision: Option<String>,
    pub identity: String,
    pub files: Vec<PayloadFile>,
    pub config: Value,
    pub tensors: BTreeMap<String, TensorSpec>,
    pub validation: String,
}

pub fn read_json(path: &Path) -> Result<Value> {
    let mut file = File::open(path)?;
    ensure!(
        file.metadata()?.len() <= METADATA_LIMIT,
        "metadata too large: {}",
        path.display()
    );
    let mut data = Vec::new();
    file.by_ref()
        .take(METADATA_LIMIT + 1)
        .read_to_end(&mut data)?;
    ensure!(data.len() as u64 <= METADATA_LIMIT, "metadata too large");
    Ok(serde_json::from_slice(&data).with_context(|| path.display().to_string())?)
}

pub fn positive(config: &Value, name: &str) -> Result<u64> {
    let value = config[name]
        .as_u64()
        .filter(|value| *value > 0 && *value <= i32::MAX as u64);
    value.with_context(|| format!("invalid or missing {name}"))
}

/// Shared by admission and MLX discovery. Unsupported operations fail before weight download.
pub fn validate_mlx_config(config: &Value) -> Result<()> {
    let kind = config["model_type"]
        .as_str()
        .context("missing model_type")?;
    ensure!(
        matches!(kind, "qwen2" | "qwen3" | "qwen3_moe" | "llama" | "mistral"),
        "unsupported MLX architecture: {kind}"
    );
    ensure!(
        config["rope_scaling"].is_null(),
        "scaled RoPE is not implemented"
    );
    ensure!(
        config["hidden_act"]
            .as_str()
            .is_none_or(|act| act == "silu"),
        "unsupported activation"
    );
    ensure!(
        config["use_sliding_window"].as_bool() != Some(true)
            && config["sliding_window"]
                .as_i64()
                .is_none_or(|size| size <= 0),
        "sliding-window attention is not implemented"
    );
    ensure!(
        config["shared_expert_intermediate_size"]
            .as_i64()
            .is_none_or(|size| size <= 0),
        "shared experts are not implemented"
    );
    ensure!(
        config["partial_rotary_factor"].is_null()
            || config["partial_rotary_factor"].as_f64() == Some(1.0),
        "partial rotary dimensions are not implemented"
    );
    let hidden = positive(config, "hidden_size")?;
    let heads = positive(config, "num_attention_heads")?;
    positive(config, "num_hidden_layers")?;
    let kv = if config["num_key_value_heads"].is_null() {
        heads
    } else {
        positive(config, "num_key_value_heads")?
    };
    ensure!(heads % kv == 0, "attention head counts are incompatible");
    let head = if config["head_dim"].is_null() {
        ensure!(
            hidden % heads == 0,
            "hidden size does not divide attention heads"
        );
        hidden / heads
    } else {
        positive(config, "head_dim")?
    };
    ensure!(head > 0 && head % 2 == 0, "invalid RoPE head dimension");
    for field in ["rms_norm_eps", "rope_theta"] {
        ensure!(
            config[field].is_null()
                || config[field]
                    .as_f64()
                    .is_some_and(|v| v.is_finite() && v > 0.0 && v <= f32::MAX as f64),
            "invalid {field}"
        );
    }
    if kind == "qwen3_moe" {
        let experts = positive(config, "num_experts")?;
        ensure!(
            positive(config, "num_experts_per_tok")? <= experts,
            "invalid active expert count"
        );
    }
    let quant = quantization(config);
    ensure!(
        quant.is_null() || quant.is_object(),
        "invalid quantization configuration"
    );
    ensure!(
        quant["quant_method"].is_null(),
        "GPTQ/AWQ and other quant_method layouts need a converter"
    );
    let group = quant["group_size"].as_u64().unwrap_or(64);
    ensure!(matches!(group, 32 | 64 | 128), "unsupported MLX group size");
    validate_quant_entry(quant, group)?;
    if let Some(entries) = quant.as_object() {
        for entry in entries.values().filter(|entry| entry.is_object()) {
            validate_quant_entry(entry, group)?;
        }
    }
    Ok(())
}

fn validate_quant_entry(entry: &Value, group: u64) -> Result<()> {
    ensure!(
        entry["mode"].as_str().is_none_or(|mode| mode == "affine"),
        "unsupported MLX quantization mode"
    );
    ensure!(
        entry["group_size"].is_null() || entry["group_size"].as_u64() == Some(group),
        "mixed group sizes are not implemented"
    );
    ensure!(
        entry["bits"].is_null()
            || entry["bits"]
                .as_u64()
                .is_some_and(|bits| matches!(bits, 0 | 2 | 3 | 4 | 5 | 6 | 8)),
        "unsupported MLX bit width"
    );
    Ok(())
}

pub fn quantization(config: &Value) -> &Value {
    if config["quantization"].is_object() {
        &config["quantization"]
    } else {
        &config["quantization_config"]
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct HeaderTensor {
    dtype: String,
    shape: Vec<u64>,
    data_offsets: [u64; 2],
}

/// JSON maps normally overwrite duplicate names. Safetensors explicitly forbids them.
struct UniqueHeader(BTreeMap<String, Value>);
impl<'de> Deserialize<'de> for UniqueHeader {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = UniqueHeader;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a unique tensor map")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> std::result::Result<Self::Value, M::Error> {
                let mut out = BTreeMap::new();
                while let Some((name, value)) = map.next_entry::<String, Value>()? {
                    if out.insert(name, value).is_some() {
                        return Err(serde::de::Error::custom("duplicate tensor name"));
                    }
                }
                Ok(UniqueHeader(out))
            }
        }
        deserializer.deserialize_map(Visitor)
    }
}

pub fn read_tensors(
    path: &Path,
    file_name: &str,
    out: &mut BTreeMap<String, TensorSpec>,
) -> Result<()> {
    let mut file = File::open(path)?;
    let length = file.metadata()?.len();
    read_tensor_header(&mut file, length, file_name, out)
}

pub(crate) fn read_tensors_slice(
    path: &Path,
    file_name: &str,
    offset: u64,
    length: u64,
    out: &mut BTreeMap<String, TensorSpec>,
) -> Result<()> {
    let mut file = File::open(path)?;
    ensure!(
        offset
            .checked_add(length)
            .is_some_and(|end| end <= file.metadata().map(|m| m.len()).unwrap_or(0)),
        "truncated safetensors slice"
    );
    file.seek(SeekFrom::Start(offset))?;
    read_tensor_header(&mut file, length, file_name, out)
}

fn read_tensor_header(
    file: &mut File,
    length: u64,
    file_name: &str,
    out: &mut BTreeMap<String, TensorSpec>,
) -> Result<()> {
    let mut prefix = [0; 8];
    file.read_exact(&mut prefix)
        .context("truncated safetensors prefix")?;
    let header_len = u64::from_le_bytes(prefix);
    ensure!(
        header_len > 0 && header_len <= HEADER_LIMIT && header_len + 8 <= length,
        "invalid or truncated safetensors header"
    );
    let mut header = vec![0; header_len as usize];
    file.read_exact(&mut header)?;
    ensure!(header.first() == Some(&b'{'), "invalid safetensors header");
    let header: UniqueHeader = serde_json::from_slice(&header)?;
    let start = header_len + 8;
    let mut ranges = Vec::new();
    for (name, value) in header.0 {
        if name == "__metadata__" {
            let _: BTreeMap<String, String> = serde_json::from_value(value)?;
            continue;
        }
        ensure!(
            out.len() < TENSOR_LIMIT && name.len() <= 512,
            "tensor metadata exceeds import limit"
        );
        let tensor: HeaderTensor = serde_json::from_value(value).with_context(|| name.clone())?;
        ensure!(tensor.shape.len() <= 8, "tensor rank exceeds import limit");
        let element_bytes = match tensor.dtype.as_str() {
            "F16" | "BF16" | "I16" | "U16" => 2,
            "F32" | "I32" | "U32" => 4,
            "F64" | "I64" | "U64" => 8,
            "BOOL" | "I8" | "U8" => 1,
            other => bail!("unsupported tensor dtype: {other}"),
        };
        let bytes = tensor
            .shape
            .iter()
            .try_fold(element_bytes, |n: u64, d| n.checked_mul(*d))
            .context("tensor size overflow")?;
        let [begin, end] = tensor.data_offsets;
        ensure!(
            begin <= end && end <= length - start && end - begin == bytes,
            "truncated tensor or invalid shape/range: {name}"
        );
        ranges.push((begin, end));
        ensure!(
            out.insert(
                name.clone(),
                TensorSpec {
                    file: file_name.into(),
                    dtype: tensor.dtype,
                    shape: tensor.shape,
                    offset: start + begin,
                    bytes,
                    affine: None
                }
            )
            .is_none(),
            "duplicate tensor across shards: {name}"
        );
    }
    ranges.sort_unstable();
    let mut cursor = 0;
    for (begin, end) in ranges {
        ensure!(
            begin == cursor,
            "safetensors data contains holes or overlapping tensors"
        );
        cursor = end;
    }
    ensure!(
        cursor == length - start,
        "safetensors contains unindexed or truncated data"
    );
    Ok(())
}

fn float_tensor(tensors: &BTreeMap<String, TensorSpec>, name: &str, shape: &[u64]) -> Result<()> {
    let tensor = tensors
        .get(name)
        .with_context(|| format!("missing tensor: {name}"))?;
    ensure!(
        tensor.shape == shape && matches!(tensor.dtype.as_str(), "F16" | "BF16" | "F32"),
        "unsupported tensor shape/dtype: {name}"
    );
    Ok(())
}

fn linear(
    tensors: &mut BTreeMap<String, TensorSpec>,
    prefix: &str,
    logical: &[u64],
    group: u64,
) -> Result<()> {
    let name = format!("{prefix}.weight");
    let scale_name = format!("{prefix}.scales");
    let bias_name = format!("{prefix}.biases");
    let weight = tensors
        .get(&name)
        .with_context(|| format!("missing tensor: {name}"))?
        .clone();
    if tensors.contains_key(&scale_name) {
        let input = *logical.last().unwrap();
        ensure!(
            input % group == 0,
            "quantized input does not divide group size: {prefix}"
        );
        let mut scales = logical.to_vec();
        *scales.last_mut().unwrap() = input / group;
        float_tensor(tensors, &scale_name, &scales)?;
        ensure!(
            weight.dtype == "U32"
                && weight.shape.len() == logical.len()
                && weight.shape[..weight.shape.len() - 1] == logical[..logical.len() - 1],
            "invalid packed weight: {prefix}"
        );
        let packed_bits = weight
            .shape
            .last()
            .unwrap()
            .checked_mul(32)
            .context("packing overflow")?;
        ensure!(packed_bits % input == 0, "invalid affine packing: {prefix}");
        let bits = packed_bits / input;
        ensure!(
            matches!(bits, 2 | 3 | 4 | 5 | 6 | 8),
            "unsupported packed bit width: {prefix}"
        );
        let biases = if tensors.contains_key(&bias_name) {
            float_tensor(tensors, &bias_name, &scales)?;
            Some(bias_name)
        } else {
            None
        };
        tensors.get_mut(&name).unwrap().affine = Some(AffineLayout {
            bits,
            group_size: group,
            axis: -1,
            packing: "mlx-affine-u32-v1".into(),
            scales: scale_name,
            biases,
        });
    } else {
        ensure!(
            !tensors.contains_key(&bias_name),
            "affine biases without scales: {prefix}"
        );
        float_tensor(tensors, &name, logical)?;
    }
    let bias = format!("{prefix}.bias");
    if tensors.contains_key(&bias) {
        float_tensor(tensors, &bias, &logical[..logical.len() - 1])?;
    }
    Ok(())
}

pub fn validate_tensors(config: &Value, tensors: &mut BTreeMap<String, TensorSpec>) -> Result<()> {
    validate_mlx_config(config)?;
    ensure!(
        tensors
            .values()
            .all(|tensor| matches!(tensor.dtype.as_str(), "F16" | "BF16" | "F32" | "U32")),
        "payload contains a dtype outside the current MLX import contract"
    );
    let hidden = positive(config, "hidden_size")?;
    let vocab = positive(config, "vocab_size")?;
    let layers = positive(config, "num_hidden_layers")?;
    ensure!(layers <= 1024, "layer count exceeds import limit");
    let heads = positive(config, "num_attention_heads")?;
    let kv = config["num_key_value_heads"].as_u64().unwrap_or(heads);
    let head = config["head_dim"].as_u64().unwrap_or(hidden / heads);
    let moe = config["model_type"] == "qwen3_moe";
    let intermediate = positive(
        config,
        if moe {
            "moe_intermediate_size"
        } else {
            "intermediate_size"
        },
    )?;
    let group = quantization(config)["group_size"].as_u64().unwrap_or(64);
    linear(tensors, "model.embed_tokens", &[vocab, hidden], group)?;
    if config["tie_word_embeddings"].as_bool() != Some(true) {
        linear(tensors, "lm_head", &[vocab, hidden], group)?;
    }
    float_tensor(tensors, "model.norm.weight", &[hidden])?;
    for layer in 0..layers {
        let p = format!("model.layers.{layer}");
        for name in ["input_layernorm", "post_attention_layernorm"] {
            float_tensor(tensors, &format!("{p}.{name}.weight"), &[hidden])?;
        }
        for (name, rows, cols) in [
            ("q_proj", heads * head, hidden),
            ("k_proj", kv * head, hidden),
            ("v_proj", kv * head, hidden),
            ("o_proj", hidden, heads * head),
        ] {
            linear(
                tensors,
                &format!("{p}.self_attn.{name}"),
                &[rows, cols],
                group,
            )?;
        }
        if matches!(config["model_type"].as_str(), Some("qwen3" | "qwen3_moe")) {
            for name in ["q_norm", "k_norm"] {
                float_tensor(tensors, &format!("{p}.self_attn.{name}.weight"), &[head])?;
            }
        }
        let bank = if moe {
            let experts = positive(config, "num_experts")?;
            linear(tensors, &format!("{p}.mlp.gate"), &[experts, hidden], group)?;
            Some(experts)
        } else {
            None
        };
        for (name, rows, cols) in [
            ("gate_proj", intermediate, hidden),
            ("up_proj", intermediate, hidden),
            ("down_proj", hidden, intermediate),
        ] {
            let (prefix, shape) = match bank {
                Some(experts) => (
                    format!("{p}.mlp.switch_mlp.{name}"),
                    vec![experts, rows, cols],
                ),
                None => (format!("{p}.mlp.{name}"), vec![rows, cols]),
            };
            linear(tensors, &prefix, &shape, group)?;
        }
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use serde_json::json;
    use std::{fs, io::Write};

    pub fn fixture(directory: &Path) {
        fs::create_dir_all(directory).unwrap();
        let config = json!({"model_type":"qwen3", "hidden_size":128, "intermediate_size":128,
            "num_hidden_layers":1, "num_attention_heads":4, "num_key_value_heads":2,
            "vocab_size":16, "tie_word_embeddings":true, "quantization":{"bits":4,"group_size":64}});
        fs::write(directory.join("config.json"), config.to_string()).unwrap();
        let tokenizer = json!({"version":"1.0", "truncation":null, "padding":null,
            "added_tokens":[], "normalizer":null, "pre_tokenizer":{"type":"Whitespace"},
            "post_processor":null,"decoder":null,
            "model":{"type":"WordLevel","vocab":{"[UNK]":0,"hello":1},"unk_token":"[UNK]"}});
        fs::write(directory.join("tokenizer.json"), tokenizer.to_string()).unwrap();
        fs::write(
            directory.join("tokenizer_config.json"),
            json!({"chat_template":"{% for m in messages %}{{ m.content }}{% endfor %}"})
                .to_string(),
        )
        .unwrap();
        fs::write(directory.join("LICENSE"), "fixture license").unwrap();
        let mut specs = BTreeMap::new();
        let mut add = |name: String, dtype: &str, shape: Vec<u64>| {
            specs.insert(name, (dtype.to_owned(), shape));
        };
        add("model.norm.weight".into(), "F16", vec![128]);
        add("model.embed_tokens.weight".into(), "F16", vec![16, 128]);
        for name in ["input_layernorm", "post_attention_layernorm"] {
            add(format!("model.layers.0.{name}.weight"), "F16", vec![128]);
        }
        for name in ["q_norm", "k_norm"] {
            add(
                format!("model.layers.0.self_attn.{name}.weight"),
                "F16",
                vec![32],
            );
        }
        for (name, rows, cols) in [
            ("self_attn.q_proj", 128, 128),
            ("self_attn.k_proj", 64, 128),
            ("self_attn.v_proj", 64, 128),
            ("self_attn.o_proj", 128, 128),
            ("mlp.gate_proj", 128, 128),
            ("mlp.up_proj", 128, 128),
            ("mlp.down_proj", 128, 128),
        ] {
            add(
                format!("model.layers.0.{name}.weight"),
                "U32",
                vec![rows, cols / 8],
            );
            add(
                format!("model.layers.0.{name}.scales"),
                "F16",
                vec![rows, cols / 64],
            );
            add(
                format!("model.layers.0.{name}.biases"),
                "F16",
                vec![rows, cols / 64],
            );
        }
        let mut header = BTreeMap::new();
        let mut offset = 0;
        for (name, (dtype, shape)) in specs {
            let size = shape.iter().product::<u64>() * if dtype == "U32" { 4 } else { 2 };
            header.insert(
                name,
                json!({"dtype":dtype,"shape":shape,"data_offsets":[offset,offset+size]}),
            );
            offset += size;
        }
        let mut data = serde_json::to_vec(&header).unwrap();
        while data.len() % 8 != 0 {
            data.push(b' ');
        }
        let mut file = fs::File::create(directory.join("model.safetensors")).unwrap();
        file.write_all(&(data.len() as u64).to_le_bytes()).unwrap();
        file.write_all(&data).unwrap();
        file.write_all(&vec![0; offset as usize]).unwrap();
    }

    #[test]
    fn reads_packed_tensor_metadata_and_rejects_truncation() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let path = dir.path().join("model.safetensors");
        let mut tensors = BTreeMap::new();
        read_tensors(&path, "model.safetensors", &mut tensors).unwrap();
        let config = read_json(&dir.path().join("config.json")).unwrap();
        validate_tensors(&config, &mut tensors).unwrap();
        let quant = tensors["model.layers.0.self_attn.q_proj.weight"]
            .affine
            .as_ref()
            .unwrap();
        assert_eq!((quant.bits, quant.group_size, quant.axis), (4, 64, -1));
        assert!(tensors["model.embed_tokens.weight"].affine.is_none());
        let size = fs::metadata(&path).unwrap().len();
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_len(size - 1)
            .unwrap();
        assert!(read_tensors(&path, "model.safetensors", &mut BTreeMap::new()).is_err());
    }

    #[test]
    fn rejects_missing_tensor_wrong_shapes_packing_and_unsupported_operations() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let config = read_json(&dir.path().join("config.json")).unwrap();
        let mut original = BTreeMap::new();
        read_tensors(
            &dir.path().join("model.safetensors"),
            "model.safetensors",
            &mut original,
        )
        .unwrap();
        for name in [
            "model.norm.weight",
            "model.layers.0.self_attn.q_norm.weight",
            "model.layers.0.mlp.up_proj.weight",
        ] {
            let mut tensors = original.clone();
            tensors.remove(name);
            assert!(validate_tensors(&config, &mut tensors).is_err());
        }
        let name = "model.layers.0.self_attn.q_proj.weight";
        let mut tensors = original.clone();
        tensors.get_mut(name).unwrap().shape = vec![128, 7];
        assert!(validate_tensors(&config, &mut tensors).is_err());
        for (key, value) in [
            ("rope_scaling", json!({"factor":2})),
            ("sliding_window", json!(4096)),
            ("shared_expert_intermediate_size", json!(128)),
            ("hidden_act", json!("gelu")),
            ("quantization", json!({"mode":"mxfp4","bits":4})),
            (
                "quantization",
                json!({"bits":4,"model.layers.0.self_attn.q_proj":{"group_size":32}}),
            ),
        ] {
            let mut c = config.clone();
            c[key] = value;
            assert!(validate_mlx_config(&c).is_err(), "accepted {key}");
        }
    }

    #[test]
    fn rejects_duplicate_names_overlaps_holes_and_unindexed_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.safetensors");
        for header in [
            r#"{"x":{"dtype":"F16","shape":[1],"data_offsets":[0,2]},"x":{"dtype":"F16","shape":[1],"data_offsets":[2,4]}}"#,
            r#"{"x":{"dtype":"F16","shape":[1],"data_offsets":[0,2]},"y":{"dtype":"F16","shape":[1],"data_offsets":[0,2]}}"#,
            r#"{"x":{"dtype":"F16","shape":[1],"data_offsets":[2,4]}}"#,
            r#"{"x":{"dtype":"F16","shape":[1],"data_offsets":[0,2]}}"#,
        ] {
            let mut data = (header.len() as u64).to_le_bytes().to_vec();
            data.extend_from_slice(header.as_bytes());
            data.extend([0; 4]);
            fs::write(&path, data).unwrap();
            assert!(read_tensors(&path, "bad.safetensors", &mut BTreeMap::new()).is_err());
        }
    }

    #[test]
    fn validates_packed_moe_banks_with_mixed_router_precision() {
        let dir = tempfile::tempdir().unwrap();
        fixture(dir.path());
        let mut config = read_json(&dir.path().join("config.json")).unwrap();
        config["model_type"] = json!("qwen3_moe");
        config["num_experts"] = json!(8);
        config["num_experts_per_tok"] = json!(2);
        config["moe_intermediate_size"] = json!(128);
        let mut tensors = BTreeMap::new();
        read_tensors(
            &dir.path().join("model.safetensors"),
            "model.safetensors",
            &mut tensors,
        )
        .unwrap();
        for name in ["gate_proj", "up_proj", "down_proj"] {
            for suffix in ["weight", "scales", "biases"] {
                let old = format!("model.layers.0.mlp.{name}.{suffix}");
                let mut tensor = tensors.remove(&old).unwrap();
                tensor.shape.insert(0, 8);
                tensors.insert(
                    format!("model.layers.0.mlp.switch_mlp.{name}.{suffix}"),
                    tensor,
                );
            }
        }
        for (suffix, dtype, shape) in [
            ("weight", "U32", vec![8, 32]),
            ("scales", "F16", vec![8, 2]),
            ("biases", "F16", vec![8, 2]),
        ] {
            tensors.insert(
                format!("model.layers.0.mlp.gate.{suffix}"),
                TensorSpec {
                    file: "model.safetensors".into(),
                    dtype: dtype.into(),
                    shape,
                    offset: 0,
                    bytes: 0,
                    affine: None,
                },
            );
        }
        validate_tensors(&config, &mut tensors).unwrap();
        assert_eq!(
            tensors["model.layers.0.mlp.gate.weight"]
                .affine
                .as_ref()
                .unwrap()
                .bits,
            8
        );
        assert_eq!(
            tensors["model.layers.0.mlp.switch_mlp.gate_proj.weight"]
                .affine
                .as_ref()
                .unwrap()
                .bits,
            4
        );
        tensors
            .get_mut("model.layers.0.mlp.switch_mlp.up_proj.weight")
            .unwrap()
            .shape[0] = 7;
        assert!(validate_tensors(&config, &mut tensors).is_err());
    }
}
