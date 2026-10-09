//! Native LMX v2 semantics. No GGUF, safetensors, model loader or device dependency.
//! Binary layout and streaming IO live in `lmx_native_file`.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

pub const VERSION: u32 = 2;
pub const ALIGNMENT: u64 = 16_384;
pub const HEADER_BYTES: u64 = 128;
pub const METADATA_LIMIT: u64 = 32 << 20;
const MAX_TENSORS: usize = 100_000;

// Duplicate JSON keys must not silently replace a tensor or an authenticated block.
fn unique_map<'de, D, T>(de: D) -> std::result::Result<BTreeMap<String, T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    struct Visitor<T>(std::marker::PhantomData<T>);
    impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for Visitor<T> {
        type Value = BTreeMap<String, T>;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("unique names")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut a: A,
        ) -> std::result::Result<Self::Value, A::Error> {
            let mut out = BTreeMap::new();
            while let Some((key, value)) = a.next_entry()? {
                if out.insert(key, value).is_some() {
                    return Err(serde::de::Error::custom("duplicate name"));
                }
            }
            Ok(out)
        }
    }
    de.deserialize_map(Visitor(std::marker::PhantomData))
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub version: u32,
    pub model: Model,
    #[serde(deserialize_with = "unique_map")]
    pub tensors: BTreeMap<String, Tensor>,
    #[serde(deserialize_with = "unique_map")]
    pub blocks: BTreeMap<String, Block>,
    pub tokenizer: Tokenizer,
    pub prompt: Prompt,
    pub provenance: Provenance,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Architecture {
    Qwen2,
    Qwen3,
    Qwen3Moe,
    /// Recipe 2: Qwen3.5/3.6 MoE, Gated DeltaNet layers between gated full attention.
    Qwen35Moe,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Model {
    pub recipe_version: u32,
    pub architecture: Architecture,
    pub hidden: u32,
    pub layers: u32,
    pub vocab: u32,
    pub context: u32,
    pub attention: Attention,
    pub rms_epsilon: f64,
    pub feed_forward: FeedForward,
    pub tied_embeddings: bool,
    /// Recipe 2 only; absent from recipe 1 files so their identity is unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hybrid: Option<Hybrid>,
}

/// Recipe 2 (Qwen3.5 MoE). Layers listed in `linear_layers` use Gated DeltaNet linear
/// attention; the others use full attention whose query projection also yields a
/// per-head sigmoid output gate (`[query, gate]` per head). RoPE is split-half over the
/// first `rope_dims` dimensions of each head; `rope_sections` are the multimodal
/// sections, all at the token position for text. RMS norms of layers, the final norm
/// and Q/K norms are zero-centered (`x̂·(1+w)`); the linear-attention output norm is a
/// plain gated RMS norm (`x̂·w·silu(z)`); `A_log` keeps HF's form (`A = −exp(A_log)`).
/// Every MoE layer adds a shared expert scaled by `sigmoid(shared_expert_gate·x)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hybrid {
    pub linear_layers: Vec<u32>,
    pub conv_kernel: u32,
    pub key_heads: u32,
    pub value_heads: u32,
    pub key_dim: u32,
    pub value_dim: u32,
    pub rope_dims: u32,
    pub rope_sections: [u32; 4],
    pub shared_intermediate: u32,
}

/// Linear-attention (Gated DeltaNet) tensors of one layer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LinearPart {
    /// Query, key and value channels in convolution order.
    Qkv,
    /// Output gate `z`.
    Z,
    /// Per-head write strength β (sigmoid).
    B,
    /// Per-head decay input (softplus with `dt_bias`, scaled by `A`).
    A,
    Conv,
    DtBias,
    ALog,
    Norm,
    Output,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attention {
    pub heads: u32,
    pub kv_heads: u32,
    pub head_dim: u32,
    pub qkv_bias: bool,
    pub qk_norm: bool,
    /// Recipe 1: causal attention, split-half RoPE on the entire head, no scaling.
    pub rope_theta: f64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FeedForward {
    /// Recipe 1 activation is SiLU(gate) * up, followed by down.
    Dense { intermediate: u32 },
    /// All layers use softmax(router) then top-k; optional renormalization after top-k.
    Moe {
        intermediate: u32,
        experts: u32,
        active: u32,
        normalize_topk: bool,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Projection {
    Query,
    Key,
    Value,
    Output,
    Gate,
    Up,
    Down,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Role {
    Embedding,
    Head,
    FinalNorm,
    AttentionNorm {
        layer: u32,
    },
    FeedForwardNorm {
        layer: u32,
    },
    Attention {
        layer: u32,
        projection: Projection,
    },
    AttentionBias {
        layer: u32,
        projection: Projection,
    },
    QueryNorm {
        layer: u32,
    },
    KeyNorm {
        layer: u32,
    },
    FeedForward {
        layer: u32,
        projection: Projection,
    },
    Router {
        layer: u32,
    },
    Expert {
        layer: u32,
        expert: u32,
        projection: Projection,
    },
    LinearAttention {
        layer: u32,
        part: LinearPart,
    },
    SharedExpert {
        layer: u32,
        projection: Projection,
    },
    SharedExpertGate {
        layer: u32,
    },
}

impl Role {
    pub fn hf_name(&self) -> String {
        fn attention(p: Projection) -> &'static str {
            match p {
                Projection::Query => "q_proj",
                Projection::Key => "k_proj",
                Projection::Value => "v_proj",
                Projection::Output => "o_proj",
                _ => "invalid",
            }
        }
        fn mlp(p: Projection) -> &'static str {
            match p {
                Projection::Gate => "gate_proj",
                Projection::Up => "up_proj",
                Projection::Down => "down_proj",
                _ => "invalid",
            }
        }
        match self {
            Self::Embedding => "model.embed_tokens.weight".into(),
            Self::Head => "lm_head.weight".into(),
            Self::FinalNorm => "model.norm.weight".into(),
            Self::AttentionNorm { layer } => format!("model.layers.{layer}.input_layernorm.weight"),
            Self::FeedForwardNorm { layer } => {
                format!("model.layers.{layer}.post_attention_layernorm.weight")
            }
            Self::Attention { layer, projection } => format!(
                "model.layers.{layer}.self_attn.{}.weight",
                attention(*projection)
            ),
            Self::AttentionBias { layer, projection } => format!(
                "model.layers.{layer}.self_attn.{}.bias",
                attention(*projection)
            ),
            Self::QueryNorm { layer } => format!("model.layers.{layer}.self_attn.q_norm.weight"),
            Self::KeyNorm { layer } => format!("model.layers.{layer}.self_attn.k_norm.weight"),
            Self::FeedForward { layer, projection } => {
                format!("model.layers.{layer}.mlp.{}.weight", mlp(*projection))
            }
            Self::Router { layer } => format!("model.layers.{layer}.mlp.gate.weight"),
            Self::Expert {
                layer,
                expert,
                projection,
            } => format!(
                "model.layers.{layer}.mlp.experts.{expert}.{}.weight",
                mlp(*projection)
            ),
            Self::LinearAttention { layer, part } => format!(
                "model.layers.{layer}.linear_attn.{}",
                match part {
                    LinearPart::Qkv => "in_proj_qkv.weight",
                    LinearPart::Z => "in_proj_z.weight",
                    LinearPart::B => "in_proj_b.weight",
                    LinearPart::A => "in_proj_a.weight",
                    LinearPart::Conv => "conv1d.weight",
                    LinearPart::DtBias => "dt_bias",
                    LinearPart::ALog => "A_log",
                    LinearPart::Norm => "norm.weight",
                    LinearPart::Output => "out_proj.weight",
                }
            ),
            Self::SharedExpert { layer, projection } => format!(
                "model.layers.{layer}.mlp.shared_expert.{}.weight",
                mlp(*projection)
            ),
            Self::SharedExpertGate { layer } => {
                format!("model.layers.{layer}.mlp.shared_expert_gate.weight")
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dtype {
    F32,
    F16,
    Bf16,
}
impl Dtype {
    pub fn bytes(self) -> u64 {
        if self == Self::F32 { 4 } else { 2 }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Buffer {
    pub block: String,
    pub offset: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TypedBuffer {
    pub dtype: Dtype,
    pub data: Buffer,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Storage {
    /// Row-major, little-endian, exact logical rank; no implicit F32 -> F16 conversion.
    Dense { values: TypedBuffer },
    /// Recipe 1: value = unsigned_code * scale + bias (bias defaults to zero).
    /// Codes form a continuous little-endian bitstream within EACH row, byte padded.
    /// Grouping is along the last logical axis; codes may cross U32 boundaries.
    Affine {
        version: u32,
        bits: u8,
        group_size: u32,
        codes: Buffer,
        scales: TypedBuffer,
        biases: Option<TypedBuffer>,
    },
    /// Recipe 1: llama.cpp quantization blocks kept byte for byte. Each row of the last
    /// logical axis is a whole number of `format` blocks, rows are consecutive.
    Ggml {
        version: u32,
        format: GgmlBlock,
        data: Buffer,
    },
}

/// llama.cpp quantization blocks. Values follow llama.cpp's `dequantize_row_*` for the
/// type of the same name. A GGUF import keeps them byte for byte: no requantization, and
/// the native engine runs them with ggml's kernels directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GgmlBlock {
    Q4_0,
    Q4_1,
    Q5_0,
    Q5_1,
    Q8_0,
    Q2K,
    Q3K,
    Q4K,
    Q5K,
    Q6K,
    Iq2Xxs,
    Iq2Xs,
    Iq2S,
    Iq3Xxs,
    Iq3S,
    Iq1S,
    Iq1M,
    Iq4Nl,
    Iq4Xs,
    Tq1_0,
    Tq2_0,
    Mxfp4,
}

/// (format, ggml type name, values per block, bytes per block).
const GGML_BLOCKS: [(GgmlBlock, &str, u64, u64); 22] = [
    (GgmlBlock::Q4_0, "q4_0", 32, 18),
    (GgmlBlock::Q4_1, "q4_1", 32, 20),
    (GgmlBlock::Q5_0, "q5_0", 32, 22),
    (GgmlBlock::Q5_1, "q5_1", 32, 24),
    (GgmlBlock::Q8_0, "q8_0", 32, 34),
    (GgmlBlock::Q2K, "q2_K", 256, 84),
    (GgmlBlock::Q3K, "q3_K", 256, 110),
    (GgmlBlock::Q4K, "q4_K", 256, 144),
    (GgmlBlock::Q5K, "q5_K", 256, 176),
    (GgmlBlock::Q6K, "q6_K", 256, 210),
    (GgmlBlock::Iq2Xxs, "iq2_xxs", 256, 66),
    (GgmlBlock::Iq2Xs, "iq2_xs", 256, 74),
    (GgmlBlock::Iq2S, "iq2_s", 256, 82),
    (GgmlBlock::Iq3Xxs, "iq3_xxs", 256, 98),
    (GgmlBlock::Iq3S, "iq3_s", 256, 110),
    (GgmlBlock::Iq1S, "iq1_s", 256, 50),
    (GgmlBlock::Iq1M, "iq1_m", 256, 56),
    (GgmlBlock::Iq4Nl, "iq4_nl", 32, 18),
    (GgmlBlock::Iq4Xs, "iq4_xs", 256, 136),
    (GgmlBlock::Tq1_0, "tq1_0", 256, 54),
    (GgmlBlock::Tq2_0, "tq2_0", 256, 66),
    (GgmlBlock::Mxfp4, "mxfp4", 32, 17),
];

impl GgmlBlock {
    fn entry(self) -> &'static (GgmlBlock, &'static str, u64, u64) {
        GGML_BLOCKS.iter().find(|e| e.0 == self).expect("every format has a table entry")
    }
    /// (values, bytes) per block.
    pub fn layout(self) -> (u64, u64) {
        let e = self.entry();
        (e.2, e.3)
    }
    /// The ggml type name (`iq3_s`).
    pub fn name(self) -> &'static str {
        self.entry().1
    }
    /// The format of a ggml type name, if it is one of these.
    pub fn named(name: &str) -> Option<Self> {
        GGML_BLOCKS.iter().find(|e| e.1 == name).map(|e| e.0)
    }
}

impl Storage {
    pub fn buffers(&self) -> Vec<&Buffer> {
        match self {
            Self::Dense { values } => vec![&values.data],
            Self::Affine {
                codes,
                scales,
                biases,
                ..
            } => {
                let mut out = vec![codes, &scales.data];
                if let Some(b) = biases {
                    out.push(&b.data);
                }
                out
            }
            Self::Ggml { data, .. } => vec![data],
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tensor {
    pub role: Role,
    pub shape: Vec<u64>,
    pub storage: Storage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BlockKind {
    Resident,
    Expert { layer: u32, expert: u32 },
    Tokenizer,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Block {
    pub kind: BlockKind,
    pub offset: u64,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tokenizer {
    /// Recipe 1 is self-contained Hugging Face tokenizers JSON with BPE algorithm.
    /// Normalizer/pre-tokenizer/decoder/added tokens are included in the same asset.
    pub version: u32,
    pub algorithm: TokenizerAlgorithm,
    pub data: Buffer,
    pub bos: Option<u32>,
    pub eos: Vec<u32>,
    pub eog: Vec<u32>,
    pub add_bos: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenizerAlgorithm {
    Bpe,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Prompt {
    /// Recipe 1: Jinja with minijinja's JSON + Python compatibility semantics.
    pub version: u32,
    pub template: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Provenance {
    pub source: String,
    pub revision: Option<String>,
    pub importer: String,
    pub precision_changed: bool,
}

fn hash_valid(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn elements(shape: &[u64]) -> Result<u64> {
    ensure!(
        !shape.is_empty() && shape.len() <= 4 && shape.iter().all(|&n| n > 0),
        "invalid logical rank/shape"
    );
    shape.iter().try_fold(1u64, |n, &v| {
        n.checked_mul(v).context("tensor shape overflow")
    })
}

impl Model {
    pub(crate) fn roles(&self) -> Result<BTreeMap<Role, Vec<u64>>> {
        use Projection::*;
        let a = &self.attention;
        let hybrid_recipe = self.architecture == Architecture::Qwen35Moe;
        ensure!(
            (self.recipe_version, self.hybrid.is_some()) == if hybrid_recipe { (2, true) } else { (1, false) },
            "unsupported model recipe"
        );
        ensure!(
            [
                self.hidden,
                self.layers,
                self.vocab,
                self.context,
                a.heads,
                a.kv_heads,
                a.head_dim
            ]
            .iter()
            .all(|&v| v > 0),
            "zero model dimension"
        );
        ensure!(self.layers <= 4096, "too many layers");
        ensure!(
            a.heads % a.kv_heads == 0 && a.head_dim % 2 == 0,
            "invalid attention heads/RoPE dimension"
        );
        ensure!(
            a.rope_theta.is_finite()
                && a.rope_theta > 0.0
                && self.rms_epsilon.is_finite()
                && self.rms_epsilon > 0.0,
            "invalid RoPE or RMS norm parameter"
        );
        ensure!(
            a.qk_norm == (self.architecture != Architecture::Qwen2),
            "architecture and Q/K normalization disagree"
        );
        let h = u64::from(self.hidden);
        let q = u64::from(a.heads) * u64::from(a.head_dim);
        let kv = u64::from(a.kv_heads) * u64::from(a.head_dim);
        let mut roles = BTreeMap::from([
            (Role::Embedding, vec![self.vocab.into(), h]),
            (Role::Head, vec![self.vocab.into(), h]),
            (Role::FinalNorm, vec![h]),
        ]);
        let (intermediate, experts) = match self.feed_forward {
            FeedForward::Dense { intermediate } => {
                ensure!(
                    !matches!(self.architecture, Architecture::Qwen3Moe | Architecture::Qwen35Moe),
                    "MoE recipe requires experts"
                );
                (intermediate, None)
            }
            FeedForward::Moe {
                intermediate,
                experts,
                active,
                ..
            } => {
                ensure!(
                    matches!(self.architecture, Architecture::Qwen3Moe | Architecture::Qwen35Moe)
                        && active > 0
                        && active <= experts,
                    "invalid MoE routing"
                );
                ensure!(
                    u64::from(experts) * u64::from(self.layers) * 3 <= MAX_TENSORS as u64,
                    "too many expert tensors"
                );
                (intermediate, Some(experts))
            }
        };
        ensure!(intermediate > 0, "zero intermediate dimension");
        if let Some(hy) = &self.hybrid {
            ensure!(
                [hy.conv_kernel, hy.key_heads, hy.value_heads, hy.key_dim, hy.value_dim, hy.shared_intermediate]
                    .iter()
                    .all(|&v| v > 0)
                    && hy.value_heads % hy.key_heads == 0,
                "invalid linear attention dimensions"
            );
            ensure!(
                hy.rope_dims > 0 && hy.rope_dims % 2 == 0 && hy.rope_dims <= a.head_dim,
                "invalid partial RoPE dimension"
            );
            ensure!(
                hy.linear_layers.windows(2).all(|w| w[0] < w[1])
                    && hy.linear_layers.last().is_none_or(|&l| l < self.layers),
                "linear attention layers must be sorted, unique and in range"
            );
        }
        for layer in 0..self.layers {
            roles.insert(Role::AttentionNorm { layer }, vec![h]);
            roles.insert(Role::FeedForwardNorm { layer }, vec![h]);
            if let Some(hy) = &self.hybrid {
                if hy.linear_layers.binary_search(&layer).is_ok() {
                    let (kh, vh) = (u64::from(hy.key_heads), u64::from(hy.value_heads));
                    let (key, value) = (kh * u64::from(hy.key_dim), vh * u64::from(hy.value_dim));
                    let conv = 2 * key + value;
                    for (part, shape) in [
                        (LinearPart::Qkv, vec![conv, h]),
                        (LinearPart::Z, vec![value, h]),
                        (LinearPart::B, vec![vh, h]),
                        (LinearPart::A, vec![vh, h]),
                        (LinearPart::Conv, vec![conv, hy.conv_kernel.into()]),
                        (LinearPart::DtBias, vec![vh]),
                        (LinearPart::ALog, vec![vh]),
                        (LinearPart::Norm, vec![hy.value_dim.into()]),
                        (LinearPart::Output, vec![h, value]),
                    ] {
                        roles.insert(Role::LinearAttention { layer, part }, shape);
                    }
                } else {
                    // The query projection carries a sigmoid output gate per head.
                    for (projection, shape) in [
                        (Query, vec![2 * q, h]),
                        (Key, vec![kv, h]),
                        (Value, vec![kv, h]),
                        (Output, vec![h, q]),
                    ] {
                        roles.insert(Role::Attention { layer, projection }, shape);
                    }
                    roles.insert(Role::QueryNorm { layer }, vec![a.head_dim.into()]);
                    roles.insert(Role::KeyNorm { layer }, vec![a.head_dim.into()]);
                }
                let si = u64::from(hy.shared_intermediate);
                for (projection, shape) in [(Gate, vec![si, h]), (Up, vec![si, h]), (Down, vec![h, si])] {
                    roles.insert(Role::SharedExpert { layer, projection }, shape);
                }
                roles.insert(Role::SharedExpertGate { layer }, vec![1, h]);
            }
            for (projection, shape) in [
                (Query, vec![q, h]),
                (Key, vec![kv, h]),
                (Value, vec![kv, h]),
                (Output, vec![h, q]),
            ]
            .into_iter()
            .filter(|_| self.hybrid.is_none())
            {
                if a.qkv_bias && projection != Output {
                    roles.insert(Role::AttentionBias { layer, projection }, vec![shape[0]]);
                }
                roles.insert(Role::Attention { layer, projection }, shape);
            }
            if a.qk_norm && self.hybrid.is_none() {
                roles.insert(Role::QueryNorm { layer }, vec![a.head_dim.into()]);
                roles.insert(Role::KeyNorm { layer }, vec![a.head_dim.into()]);
            }
            if let Some(experts) = experts {
                roles.insert(Role::Router { layer }, vec![experts.into(), h]);
                for expert in 0..experts {
                    for projection in [Gate, Up, Down] {
                        let shape = if projection == Down {
                            vec![h, intermediate.into()]
                        } else {
                            vec![intermediate.into(), h]
                        };
                        roles.insert(
                            Role::Expert {
                                layer,
                                expert,
                                projection,
                            },
                            shape,
                        );
                    }
                }
            } else {
                for projection in [Gate, Up, Down] {
                    let shape = if projection == Down {
                        vec![h, intermediate.into()]
                    } else {
                        vec![intermediate.into(), h]
                    };
                    roles.insert(Role::FeedForward { layer, projection }, shape);
                }
            }
        }
        ensure!(roles.len() <= MAX_TENSORS, "too many model tensors");
        Ok(roles)
    }
}

impl Manifest {
    fn check_buffer(&self, b: &Buffer) -> Result<&Block> {
        let block = self
            .blocks
            .get(&b.block)
            .with_context(|| format!("missing block {}", b.block))?;
        ensure!(
            b.bytes > 0
                && b.offset
                    .checked_add(b.bytes)
                    .is_some_and(|end| end <= block.bytes),
            "buffer escapes block {}",
            b.block
        );
        Ok(block)
    }

    pub fn validate(&self, index_offset: u64) -> Result<()> {
        ensure!(self.version == VERSION, "unsupported native LMX version");
        ensure!(
            !self.blocks.is_empty()
                && self.blocks.len() <= MAX_TENSORS + 1
                && self.tensors.len() <= MAX_TENSORS,
            "invalid table size"
        );
        let mut occupied = Vec::new();
        for (id, b) in &self.blocks {
            ensure!(
                !id.is_empty() && id.len() <= 512 && !id.chars().any(char::is_control),
                "invalid block name"
            );
            ensure!(
                b.offset >= HEADER_BYTES && b.offset % ALIGNMENT == 0 && b.bytes > 0,
                "invalid block alignment/size: {id}"
            );
            let end = b
                .offset
                .checked_add(b.bytes)
                .context("block size overflow")?;
            ensure!(
                end <= index_offset && hash_valid(&b.sha256),
                "block escapes weights or has invalid hash: {id}"
            );
            occupied.push((b.offset, end));
        }
        occupied.sort_unstable();
        ensure!(
            occupied.windows(2).all(|p| p[0].1 <= p[1].0),
            "overlapping blocks"
        );
        let mut required = self.model.roles()?;
        let mut references: BTreeMap<&str, Vec<(u64, u64)>> = BTreeMap::new();
        for (name, tensor) in &self.tensors {
            ensure!(
                *name == tensor.role.hf_name(),
                "noncanonical HF tensor name: {name}"
            );
            let shape = required
                .remove(&tensor.role)
                .with_context(|| format!("unexpected/duplicate role: {name}"))?;
            ensure!(tensor.shape == shape, "wrong logical shape: {name}");
            let n = elements(&tensor.shape)?;
            match &tensor.storage {
                Storage::Dense { values } => {
                    ensure!(
                        values.data.offset % values.dtype.bytes() == 0,
                        "unaligned dense values: {name}"
                    );
                    ensure!(
                        n.checked_mul(values.dtype.bytes()) == Some(values.data.bytes),
                        "dense byte count mismatch: {name}"
                    );
                }
                Storage::Affine {
                    version,
                    bits,
                    group_size,
                    codes,
                    scales,
                    biases,
                } => {
                    ensure!(
                        *version == 1 && matches!(bits, 2 | 3 | 4 | 5 | 6 | 8) && *group_size > 0,
                        "unsupported affine schema: {name}"
                    );
                    ensure!(
                        tensor.shape.len() >= 2,
                        "affine requires matrix rows: {name}"
                    );
                    let last = *tensor.shape.last().unwrap();
                    ensure!(
                        last % u64::from(*group_size) == 0,
                        "incomplete affine group: {name}"
                    );
                    let row = last
                        .checked_mul(u64::from(*bits))
                        .and_then(|v| v.checked_add(7))
                        .context("packed size overflow")?
                        / 8;
                    ensure!(
                        (n / last).checked_mul(row) == Some(codes.bytes),
                        "code byte count mismatch: {name}"
                    );
                    for side in std::iter::once(scales).chain(biases) {
                        ensure!(
                            side.data.offset % side.dtype.bytes() == 0,
                            "unaligned affine coefficients: {name}"
                        );
                        ensure!(
                            (n / u64::from(*group_size)).checked_mul(side.dtype.bytes())
                                == Some(side.data.bytes),
                            "affine coefficient size mismatch: {name}"
                        );
                    }
                }
                Storage::Ggml { version, format, data } => {
                    ensure!(*version == 1, "unsupported ggml block schema: {name}");
                    let (values, bytes) = format.layout();
                    let last = *tensor.shape.last().unwrap();
                    ensure!(last % values == 0, "rows do not hold whole ggml blocks: {name}");
                    ensure!(
                        (n / values).checked_mul(bytes) == Some(data.bytes),
                        "ggml block byte count mismatch: {name}"
                    );
                }
            }
            let mut expert_block = None;
            for b in tensor.storage.buffers() {
                let block = self.check_buffer(b)?;
                match tensor.role {
                    Role::Expert { layer, expert, .. } => {
                        ensure!(
                            block.kind == (BlockKind::Expert { layer, expert }),
                            "expert must have its own block: {name}"
                        );
                        ensure!(
                            expert_block.is_none_or(|id| id == &b.block),
                            "expert coefficients must share the code block: {name}"
                        );
                        expert_block = Some(&b.block);
                    }
                    _ => ensure!(
                        block.kind == BlockKind::Resident,
                        "resident tensor in pageable block: {name}"
                    ),
                }
                if !(self.model.tied_embeddings && tensor.role == Role::Head) {
                    references
                        .entry(&b.block)
                        .or_default()
                        .push((b.offset, b.offset + b.bytes));
                }
            }
        }
        ensure!(
            required.is_empty(),
            "missing model tensors: {:?}",
            required.keys().take(4).collect::<Vec<_>>()
        );
        if self.model.tied_embeddings {
            let embedding = &self.tensors[&Role::Embedding.hf_name()];
            let head = &self.tensors[&Role::Head.hf_name()];
            ensure!(
                embedding.storage == head.storage,
                "tied head must share embedding storage"
            );
        }
        // All gate/up/down tensors for one expert use ONE independently readable block.
        let mut experts = BTreeMap::new();
        for b in self.blocks.values() {
            if let BlockKind::Expert { layer, expert } = b.kind {
                ensure!(
                    experts.insert((layer, expert), ()).is_none(),
                    "multiple blocks for one expert"
                );
            }
        }
        ensure!(
            self.tokenizer.version == 1 && self.prompt.version == 1,
            "unsupported tokenizer/template recipe"
        );
        ensure!(
            !self.prompt.template.trim().is_empty() && self.prompt.template.len() <= 1 << 20,
            "invalid chat template"
        );
        ensure!(
            !self.tokenizer.eos.is_empty()
                && (!self.tokenizer.add_bos || self.tokenizer.bos.is_some()),
            "invalid BOS/EOS policy"
        );
        for id in self
            .tokenizer
            .bos
            .iter()
            .chain(&self.tokenizer.eos)
            .chain(&self.tokenizer.eog)
        {
            ensure!(
                *id < self.model.vocab,
                "special token ID outside vocabulary"
            );
        }
        let t = &self.tokenizer.data;
        ensure!(
            self.check_buffer(t)?.kind == BlockKind::Tokenizer && t.bytes <= METADATA_LIMIT,
            "invalid tokenizer asset"
        );
        references
            .entry(&t.block)
            .or_default()
            .push((t.offset, t.offset + t.bytes));
        ensure!(
            references.len() == self.blocks.len(),
            "unreferenced native block"
        );
        for ranges in references.values_mut() {
            ranges.sort_unstable();
            // The explicitly tied head was checked against embedding storage above.
            // All other exact or partial aliases, including code/scale aliases, fail.
            ensure!(
                ranges.windows(2).all(|p| p[0].1 <= p[1].0),
                "overlapping tensor buffers"
            );
        }
        Ok(())
    }

    /// Stable across physical block ordering, outer alignment and provenance changes.
    /// Native block names and internal packing are part of schema 1's base identity.
    pub fn base_identity(&self) -> Result<String> {
        let blocks: BTreeMap<_, _> = self
            .blocks
            .iter()
            .map(|(id, b)| (id, (&b.kind, b.bytes, &b.sha256)))
            .collect();
        let bytes = serde_json::to_vec(&(
            "loom-native-base-v1",
            &self.model,
            &self.tensors,
            &blocks,
            &self.tokenizer,
            &self.prompt,
        ))?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }

    /// Independent expert ranges for the runtime; this never reads expert weights.
    pub fn expert_blocks(&self) -> BTreeMap<(u32, u32), &Block> {
        self.blocks
            .values()
            .filter_map(|b| match b.kind {
                BlockKind::Expert { layer, expert } => Some(((layer, expert), b)),
                _ => None,
            })
            .collect()
    }
}

/// Paging admission counts resident weights, KV, workspace AND slot buffers.
/// Slots cannot be recycled until the backend completes the consuming dispatch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExpertPlan {
    base_identity: String,
    budget: u64,
    kv_bytes: u64,
    workspace_bytes: u64,
    pub slot_bytes: u64,
    pub slots: u32,
    pub resident_bytes: u64,
    pub cache_bytes: u64,
    pub total_bytes: u64,
    pub max_prefill_tokens: u32,
}
impl ExpertPlan {
    pub fn new(m: &Manifest, budget: u64, kv_bytes: u64, workspace_bytes: u64) -> Result<Self> {
        m.validate(u64::MAX)?;
        let FeedForward::Moe { active, .. } = m.model.feed_forward else {
            anyhow::bail!("expert paging requires MoE");
        };
        let expert = m.expert_blocks();
        let max = expert
            .values()
            .map(|b| b.bytes)
            .max()
            .context("no expert blocks")?;
        let slot_bytes = max
            .checked_add(ALIGNMENT - 1)
            .context("slot size overflow")?
            / ALIGNMENT
            * ALIGNMENT;
        let resident_bytes = m
            .blocks
            .values()
            .filter(|b| !matches!(b.kind, BlockKind::Expert { .. }))
            .try_fold(0u64, |n, b| {
                n.checked_add(b.bytes).context("resident size overflow")
            })?;
        let fixed = resident_bytes
            .checked_add(kv_bytes)
            .and_then(|n| n.checked_add(workspace_bytes))
            .context("memory plan overflow")?;
        let free = budget
            .checked_sub(fixed)
            .context("resident weights/KV/workspace exceed memory budget")?;
        let slots = (free / slot_bytes)
            .min(expert.len() as u64)
            .min(u64::from(u32::MAX)) as u32;
        ensure!(
            slots >= active,
            "expert budget cannot hold one token's top-k experts"
        );
        let cache_bytes = u64::from(slots) * slot_bytes;
        Ok(Self {
            base_identity: m.base_identity()?,
            budget,
            kv_bytes,
            workspace_bytes,
            slot_bytes,
            slots,
            resident_bytes,
            cache_bytes,
            total_bytes: fixed + cache_bytes,
            max_prefill_tokens: slots / active,
        })
    }

    pub(crate) fn validate_for(&self, m: &Manifest) -> Result<()> {
        ensure!(
            *self == Self::new(m, self.budget, self.kv_bytes, self.workspace_bytes)?,
            "expert plan does not match this model or its derived memory totals"
        );
        Ok(())
    }
}

#[cfg(test)]
pub(crate) mod fixtures {
    use super::*;
    /// Each block is synthetic native values, never a wrapped source checkpoint.
    pub fn manifest(moe: bool) -> (Manifest, BTreeMap<String, Vec<u8>>) {
        manifest_for(model(moe))
    }

    pub fn model(moe: bool) -> Model {
        Model {
            recipe_version: 1,
            architecture: if moe {
                Architecture::Qwen3Moe
            } else {
                Architecture::Qwen3
            },
            hidden: 32,
            layers: 1,
            vocab: 8,
            context: 128,
            attention: Attention {
                heads: 2,
                kv_heads: 1,
                head_dim: 16,
                qkv_bias: false,
                qk_norm: true,
                rope_theta: 10000.0,
            },
            rms_epsilon: 1e-6,
            feed_forward: if moe {
                FeedForward::Moe {
                    intermediate: 32,
                    experts: 4,
                    active: 2,
                    normalize_topk: true,
                }
            } else {
                FeedForward::Dense { intermediate: 32 }
            },
            tied_embeddings: true,
            hybrid: None,
        }
    }

    /// Qwen3.5 MoE: layer 0 linear attention, layer 1 gated full attention.
    pub fn hybrid_model() -> Model {
        let mut model = model(true);
        model.recipe_version = 2;
        model.architecture = Architecture::Qwen35Moe;
        model.layers = 2;
        model.hybrid = Some(Hybrid {
            linear_layers: vec![0],
            conv_kernel: 4,
            key_heads: 2,
            value_heads: 4,
            key_dim: 8,
            value_dim: 8,
            rope_dims: 4,
            rope_sections: [1, 1, 0, 0],
            shared_intermediate: 16,
        });
        model
    }

    pub fn manifest_for(model: Model) -> (Manifest, BTreeMap<String, Vec<u8>>) {
        let mut data: BTreeMap<String, Vec<u8>> = BTreeMap::new();
        let mut tensors = BTreeMap::new();
        let mut kinds = BTreeMap::new();
        for (role, shape) in model.roles().unwrap() {
            if role == Role::Head {
                continue;
            }
            let (block, kind) = match role {
                Role::Expert { layer, expert, .. } => (
                    format!("expert.{layer}.{expert}"),
                    BlockKind::Expert { layer, expert },
                ),
                _ => (role.hf_name(), BlockKind::Resident),
            };
            let bytes = elements(&shape).unwrap() * 4;
            let values = data.entry(block.clone()).or_default();
            let offset = values.len() as u64;
            values.resize(values.len() + bytes as usize, 0);
            kinds.insert(block.clone(), kind);
            tensors.insert(
                role.hf_name(),
                Tensor {
                    role,
                    shape,
                    storage: Storage::Dense {
                        values: TypedBuffer {
                            dtype: Dtype::F32,
                            data: Buffer {
                                block,
                                offset,
                                bytes,
                            },
                        },
                    },
                },
            );
        }
        let mut head = tensors[&Role::Embedding.hf_name()].clone();
        head.role = Role::Head;
        tensors.insert(Role::Head.hf_name(), head);
        let tokenizer =
            br#"{"version":"1.0","model":{"type":"BPE","vocab":{"a":0},"merges":[]}}"#.to_vec();
        let tokenizer_bytes = tokenizer.len() as u64;
        data.insert("tokenizer".into(), tokenizer);
        kinds.insert("tokenizer".into(), BlockKind::Tokenizer);
        let mut offset = ALIGNMENT;
        let blocks = data
            .iter()
            .map(|(id, bytes)| {
                let block = Block {
                    kind: kinds[id].clone(),
                    offset,
                    bytes: bytes.len() as u64,
                    sha256: format!("{:x}", Sha256::digest(bytes)),
                };
                offset += (block.bytes + ALIGNMENT - 1) / ALIGNMENT * ALIGNMENT;
                (id.clone(), block)
            })
            .collect();
        (
            Manifest {
                version: VERSION,
                model,
                tensors,
                blocks,
                tokenizer: Tokenizer {
                    version: 1,
                    algorithm: TokenizerAlgorithm::Bpe,
                    data: Buffer {
                        block: "tokenizer".into(),
                        offset: 0,
                        bytes: tokenizer_bytes,
                    },
                    bos: None,
                    eos: vec![1],
                    eog: vec![1],
                    add_bos: false,
                },
                prompt: Prompt {
                    version: 1,
                    template: "{{ messages }}".into(),
                },
                provenance: Provenance {
                    source: "fixture".into(),
                    revision: None,
                    importer: "test".into(),
                    precision_changed: false,
                },
            },
            data,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        lmx_expert_cache::ExpertCache,
        lmx_native_file::{self, NativeFile},
    };
    use super::*;
    use std::{
        fs,
        io::{Seek, SeekFrom, Write},
        sync::Arc,
    };

    fn valid(m: &Manifest) -> Result<()> {
        m.validate(u64::MAX)
    }
    fn write_fixture(path: &std::path::Path, moe: bool) -> NativeFile {
        let (m, mut data) = fixtures::manifest(moe);
        for (id, bytes) in &mut data {
            if id.starts_with("expert.") {
                bytes.fill(id.as_bytes().last().copied().unwrap());
            }
        }
        lmx_native_file::write(
            path,
            m,
            &mut |id, out| {
                out.write_all(&data[id])?;
                Ok(())
            },
            &mut |_, _| true,
        )
        .unwrap()
    }

    #[test]
    fn native_schema_requires_complete_model_roles_exact_rank_and_storage_aliases() {
        let (m, _) = fixtures::manifest(true);
        valid(&m).unwrap();
        let mut broken = m.clone();
        broken.tensors.remove(&Role::Router { layer: 0 }.hf_name());
        assert!(
            valid(&broken)
                .unwrap_err()
                .to_string()
                .contains("missing model tensors")
        );
        let mut broken = m.clone();
        broken
            .tensors
            .get_mut(&Role::FinalNorm.hf_name())
            .unwrap()
            .shape
            .push(1);
        assert!(
            valid(&broken)
                .unwrap_err()
                .to_string()
                .contains("logical shape")
        );
        let mut broken = m.clone();
        let head = broken.tensors.get_mut(&Role::Head.hf_name()).unwrap();
        let Storage::Dense { values } = &mut head.storage else {
            panic!()
        };
        values.dtype = Dtype::F16;
        values.data.bytes /= 2;
        assert!(valid(&broken).is_err());
        let mut broken = m.clone();
        broken.model.recipe_version = 2;
        assert!(valid(&broken).unwrap_err().to_string().contains("recipe"));
    }

    #[test]
    fn schema_rejects_escape_overlap_duplicate_and_noncanonical_names() {
        let (m, _) = fixtures::manifest(false);
        valid(&m).unwrap();
        let mut broken = m.clone();
        broken.blocks.values_mut().next().unwrap().offset = 1;
        assert!(valid(&broken).is_err());
        let mut broken = m.clone();
        let offset = broken.blocks.values().next().unwrap().offset;
        broken.blocks.values_mut().nth(1).unwrap().offset = offset;
        assert!(
            valid(&broken)
                .unwrap_err()
                .to_string()
                .contains("overlapping blocks")
        );
        let mut broken = m.clone();
        let Storage::Dense { values } = &mut broken.tensors.values_mut().next().unwrap().storage
        else {
            panic!()
        };
        values.data.offset = u64::MAX;
        assert!(valid(&broken).is_err());
        let mut broken = m.clone();
        let tensor = broken.tensors.remove(&Role::FinalNorm.hf_name()).unwrap();
        broken.tensors.insert("gguf.output_norm".into(), tensor);
        assert!(
            valid(&broken)
                .unwrap_err()
                .to_string()
                .contains("canonical")
        );
        let json = serde_json::to_string(&m).unwrap();
        assert!(
            serde_json::from_str::<Manifest>(&json.replacen(
                "\"version\":2",
                "\"version\":2,\"version\":2",
                1
            ))
            .is_err()
        );
        let key = "model.norm.weight";
        let duplicated = format!(
            "\"{key}\":{},\"{key}\":{}",
            serde_json::to_string(&m.tensors[key]).unwrap(),
            serde_json::to_string(&m.tensors[key]).unwrap()
        );
        let json = json.replacen(
            &format!(
                "\"{key}\":{}",
                serde_json::to_string(&m.tensors[key]).unwrap()
            ),
            &duplicated,
            1,
        );
        assert!(
            serde_json::from_str::<Manifest>(&json)
                .unwrap_err()
                .to_string()
                .contains("duplicate")
        );
    }

    #[test]
    fn expert_components_are_colocated_and_never_resident() {
        let (m, _) = fixtures::manifest(true);
        let mut broken = m.clone();
        let key = Role::Expert {
            layer: 0,
            expert: 0,
            projection: Projection::Gate,
        }
        .hf_name();
        let Storage::Dense { values } = &mut broken.tensors.get_mut(&key).unwrap().storage else {
            panic!()
        };
        values.data.block = "expert.0.1".into();
        assert!(
            valid(&broken)
                .unwrap_err()
                .to_string()
                .contains("own block")
        );
        let mut broken = m.clone();
        broken.blocks.get_mut("expert.0.0").unwrap().kind = BlockKind::Resident;
        assert!(valid(&broken).is_err());
    }

    #[test]
    fn qwen35_hybrid_recipe_has_linear_and_gated_attention_layers() {
        let (m, _) = fixtures::manifest_for(fixtures::hybrid_model());
        valid(&m).unwrap();
        let names: Vec<_> = m.tensors.keys().cloned().collect();
        for expected in [
            "model.layers.0.linear_attn.in_proj_qkv.weight",
            "model.layers.0.linear_attn.A_log",
            "model.layers.0.linear_attn.conv1d.weight",
            "model.layers.0.mlp.shared_expert.down_proj.weight",
            "model.layers.0.mlp.shared_expert_gate.weight",
            "model.layers.1.self_attn.q_norm.weight",
            "model.layers.1.mlp.experts.3.up_proj.weight",
        ] {
            assert!(names.iter().any(|n| n == expected), "missing {expected}");
        }
        assert!(!names.iter().any(|n| n.starts_with("model.layers.0.self_attn.")));
        assert!(!names.iter().any(|n| n.starts_with("model.layers.1.linear_attn.")));
        // Query rows hold [query, gate] per head; conv channels are 2·keys + values.
        assert_eq!(m.tensors["model.layers.1.self_attn.q_proj.weight"].shape, vec![64, 32]);
        assert_eq!(m.tensors["model.layers.0.linear_attn.conv1d.weight"].shape, vec![64, 4]);
        ExpertPlan::new(&m, 1 << 30, 0, 0).unwrap();
        // The recipe and the architecture must agree.
        let mut wrong = fixtures::hybrid_model();
        wrong.recipe_version = 1;
        assert!(wrong.roles().is_err());
        let mut wrong = fixtures::model(true);
        wrong.hybrid = fixtures::hybrid_model().hybrid;
        assert!(wrong.roles().is_err());
        let mut wrong = fixtures::hybrid_model();
        wrong.hybrid.as_mut().unwrap().linear_layers = vec![2];
        assert!(wrong.roles().is_err());
    }

    #[test]
    fn recipe_one_files_serialize_without_hybrid_fields() {
        let (m, _) = fixtures::manifest(true);
        let json = serde_json::to_value(&m.model).unwrap();
        assert!(json.get("hybrid").is_none(), "{json}");
    }

    /// llama.cpp quantizations are stored as their exact ggml blocks so a converted model
    /// is neither expanded nor requantized.
    #[test]
    fn ggml_blocks_are_stored_verbatim_with_exact_sizes() {
        use llama_cpp_sys_2 as sys;
        for (format, name, ..) in GGML_BLOCKS {
            let ggml = (0..sys::GGML_TYPE_COUNT)
                .find(|t| unsafe { std::ffi::CStr::from_ptr(sys::ggml_type_name(*t)) }.to_str() == Ok(name))
                .unwrap_or_else(|| panic!("ggml has no type {name}"));
            let traits = unsafe { &*sys::ggml_get_type_traits(ggml) };
            assert_eq!(format.layout(), (traits.blck_size as u64, traits.type_size as u64), "{format:?}");
            assert_eq!(GgmlBlock::named(name), Some(format));
            assert_eq!(format.name(), name);
            let json = serde_json::to_string(&format).unwrap();
            assert_eq!(serde_json::from_str::<GgmlBlock>(&json).unwrap(), format, "{json}");
        }
        let (mut m, _) = fixtures::manifest(false);
        let key = Role::Attention { layer: 0, projection: Projection::Key }.hf_name();
        // [16,32]: one 32-value iq4_nl block of 18 bytes per row.
        m.tensors.get_mut(&key).unwrap().storage = Storage::Ggml {
            version: 1,
            format: GgmlBlock::Iq4Nl,
            data: Buffer { block: key.clone(), offset: 0, bytes: 16 * 18 },
        };
        m.blocks.get_mut(&key).unwrap().bytes = 16 * 18;
        valid(&m).unwrap();
        let Storage::Ggml { data, .. } = &mut m.tensors.get_mut(&key).unwrap().storage else { panic!() };
        data.bytes -= 1;
        assert!(valid(&m).is_err());
        // 32-value rows cannot hold 256-value super-blocks.
        let Storage::Ggml { format, data, .. } = &mut m.tensors.get_mut(&key).unwrap().storage else { panic!() };
        (*format, data.bytes) = (GgmlBlock::Iq3S, 16 * 18);
        assert!(valid(&m).is_err());
    }

    #[test]
    fn affine_size_formula_supports_group16_and_three_bit_rows() {
        let (mut m, _) = fixtures::manifest(false);
        let key = Role::Attention {
            layer: 0,
            projection: Projection::Key,
        }
        .hf_name();
        let block_id = key.clone();
        // [16,32]: 16 rows of 12 code bytes, 2 F32 scales and 2 BF16 biases per row.
        m.tensors.get_mut(&key).unwrap().storage = Storage::Affine {
            version: 1,
            bits: 3,
            group_size: 16,
            codes: Buffer {
                block: block_id.clone(),
                offset: 0,
                bytes: 192,
            },
            scales: TypedBuffer {
                dtype: Dtype::F32,
                data: Buffer {
                    block: block_id.clone(),
                    offset: 192,
                    bytes: 128,
                },
            },
            biases: Some(TypedBuffer {
                dtype: Dtype::Bf16,
                data: Buffer {
                    block: block_id.clone(),
                    offset: 320,
                    bytes: 64,
                },
            }),
        };
        m.blocks.get_mut(&block_id).unwrap().bytes = 384;
        valid(&m).unwrap();
        let Storage::Affine { group_size, .. } = &mut m.tensors.get_mut(&key).unwrap().storage
        else {
            panic!()
        };
        *group_size = 17;
        assert!(valid(&m).is_err());
    }

    #[test]
    fn only_explicit_tied_head_may_alias_a_tensor_buffer() {
        let (mut m, _) = fixtures::manifest(false);
        let a = Role::AttentionNorm { layer: 0 }.hf_name();
        let b = Role::FeedForwardNorm { layer: 0 }.hf_name();
        let old_block = m.tensors[&b].storage.buffers()[0].block.clone();
        m.tensors.get_mut(&b).unwrap().storage = m.tensors[&a].storage.clone();
        m.blocks.remove(&old_block);
        assert!(
            valid(&m).is_err(),
            "unrelated norms must not silently alias"
        );
        let (mut m, _) = fixtures::manifest(false);
        let name = Role::Attention {
            layer: 0,
            projection: Projection::Key,
        }
        .hf_name();
        let alias = Buffer {
            block: name.clone(),
            offset: 0,
            bytes: 512,
        };
        m.tensors.get_mut(&name).unwrap().storage = Storage::Affine {
            version: 1,
            bits: 8,
            group_size: 2,
            codes: alias.clone(),
            scales: TypedBuffer {
                dtype: Dtype::F16,
                data: alias,
            },
            biases: None,
        };
        m.blocks.get_mut(&name).unwrap().bytes = 512;
        assert!(valid(&m).is_err(), "code and scale buffers cannot alias");
    }

    #[test]
    fn expert_plan_is_bound_to_the_model_and_its_derived_totals() {
        let dir = tempfile::tempdir().unwrap();
        let file = Arc::new(write_fixture(&dir.path().join("model.lmx"), true));
        let mut plan = ExpertPlan::new(&file.manifest, 1 << 20, 0, 0).unwrap();
        plan.total_bytes = 0;
        assert!(
            ExpertCache::new(
                file.clone(),
                &plan,
                (0..plan.slots)
                    .map(|_| vec![0; plan.slot_bytes as usize])
                    .collect()
            )
            .is_err()
        );
        let plan = ExpertPlan::new(&file.manifest, 1 << 20, 0, 0).unwrap();
        let (mut manifest, mut data) = fixtures::manifest(true);
        manifest.model.vocab *= 2;
        for role in [Role::Embedding, Role::Head] {
            let t = manifest.tensors.get_mut(&role.hf_name()).unwrap();
            t.shape[0] *= 2;
            let Storage::Dense { values } = &mut t.storage else {
                panic!()
            };
            values.data.bytes *= 2;
        }
        let id = Role::Embedding.hf_name();
        manifest.blocks.get_mut(&id).unwrap().bytes *= 2;
        let bytes = data.get_mut(&id).unwrap();
        bytes.resize(bytes.len() * 2, 0);
        let different = Arc::new(
            lmx_native_file::write(
                &dir.path().join("different.lmx"),
                manifest,
                &mut |id, out| {
                    out.write_all(&data[id])?;
                    Ok(())
                },
                &mut |_, _| true,
            )
            .unwrap(),
        );
        assert!(
            ExpertCache::new(
                different,
                &plan,
                (0..plan.slots)
                    .map(|_| vec![0; plan.slot_bytes as usize])
                    .collect()
            )
            .is_err(),
            "plan for smaller resident weights must not admit another model"
        );
    }

    #[test]
    fn expert_slot_writable_capacity_is_checked_without_panicking() {
        struct ShortView {
            data: Vec<u8>,
            calls: std::cell::Cell<u32>,
            initially_full: bool,
        }
        impl AsRef<[u8]> for ShortView {
            fn as_ref(&self) -> &[u8] {
                &self.data
            }
        }
        impl AsMut<[u8]> for ShortView {
            fn as_mut(&mut self) -> &mut [u8] {
                let full = self.initially_full && self.calls.get() == 0;
                self.calls.set(self.calls.get() + 1);
                let size = if full { self.data.len() } else { 0 };
                &mut self.data[..size]
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let file = Arc::new(write_fixture(&dir.path().join("model.lmx"), true));
        let plan = ExpertPlan::new(&file.manifest, 1 << 20, 0, 0).unwrap();
        let buffers = |full| {
            (0..plan.slots)
                .map(|_| ShortView {
                    data: vec![0; plan.slot_bytes as usize],
                    calls: std::cell::Cell::new(0),
                    initially_full: full,
                })
                .collect()
        };
        assert!(
            ExpertCache::new(file.clone(), &plan, buffers(false)).is_err(),
            "constructor must check writable views"
        );
        let mut cache = ExpertCache::new(file, &plan, buffers(true)).unwrap();
        assert!(
            cache.acquire(0, &[0]).is_err(),
            "a changing writable view must return an error, not panic"
        );
    }

    #[test]
    fn native_identity_ignores_provenance_and_physical_placement_but_binds_weights() {
        let (m, _) = fixtures::manifest(true);
        let identity = m.base_identity().unwrap();
        let mut moved = m.clone();
        moved.provenance.source = "/other/download/path".into();
        for b in moved.blocks.values_mut() {
            b.offset += ALIGNMENT;
        }
        assert_eq!(identity, moved.base_identity().unwrap());
        moved.blocks.values_mut().next().unwrap().sha256 = "1".repeat(64);
        assert_ne!(identity, moved.base_identity().unwrap());
        let mut changed = m.clone();
        changed.prompt.template.push(' ');
        assert_ne!(identity, changed.base_identity().unwrap());
    }

    #[test]
    fn native_file_alone_opens_and_reads_without_source_parsers_or_cache_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.lmx");
        let file = write_fixture(&path, true);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        file.verify(&mut |_, _| true).unwrap();
        let mut out = vec![0; file.manifest.blocks["expert.0.2"].bytes as usize];
        file.read_block_into("expert.0.2", &mut out).unwrap();
        assert!(out.iter().all(|&b| b == b'2'));
        drop(file);
        let file = NativeFile::open(&path).unwrap();
        assert_eq!(file.manifest.model.architecture, Architecture::Qwen3Moe);
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).unwrap();
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
            let read = NativeFile::open(&path).and_then(|f| f.verify(&mut |_, _| true));
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
            read.unwrap();
        }
    }

    #[test]
    fn paging_is_bounded_reuses_hits_and_pins_the_whole_requested_union() {
        let dir = tempfile::tempdir().unwrap();
        let file = Arc::new(write_fixture(&dir.path().join("model.lmx"), true));
        let resident: u64 = file
            .manifest
            .blocks
            .values()
            .filter(|b| !matches!(b.kind, BlockKind::Expert { .. }))
            .map(|b| b.bytes)
            .sum();
        let plan =
            ExpertPlan::new(&file.manifest, resident + 2 * ALIGNMENT + 1024, 512, 512).unwrap();
        assert_eq!(plan.slots, 2);
        assert_eq!(plan.max_prefill_tokens, 1);
        assert_eq!(plan.total_bytes, resident + 2 * ALIGNMENT + 1024);
        assert!(ExpertPlan::new(&file.manifest, resident + ALIGNMENT, 0, 0).is_err());
        let mut cache = ExpertCache::new(
            file,
            &plan,
            (0..plan.slots)
                .map(|_| vec![0; plan.slot_bytes as usize])
                .collect(),
        )
        .unwrap();
        {
            let batch = cache.acquire(0, &[2, 3, 3]).unwrap();
            assert_eq!(batch.mapping().len(), 2);
            assert!(batch.bytes(2).unwrap().iter().all(|&b| b == b'2'));
        }
        let (hits, misses, bytes) = cache.stats();
        assert_eq!((hits, misses), (0, 2));
        // Lower-ID miss comes first. It must NOT evict the later requested hit (3).
        {
            let batch = cache.acquire(0, &[1, 3]).unwrap();
            assert!(batch.bytes(3).unwrap().iter().all(|&b| b == b'3'));
            assert!(batch.bytes(1).unwrap().iter().all(|&b| b == b'1'));
        }
        assert_eq!(cache.stats(), (1, 3, bytes + bytes / 2));
        let before = cache.stats();
        assert!(cache.acquire(0, &[0, 1, 2]).is_err());
        assert!(cache.acquire(0, &[99]).is_err());
        assert_eq!(cache.stats(), before);
        {
            let _batch = cache.acquire(0, &[1, 3]).unwrap();
        }
        assert_eq!(cache.stats(), (3, 3, bytes + bytes / 2));
    }

    #[test]
    fn corrupt_cold_expert_is_detected_on_demand_without_reading_the_whole_bank() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("model.lmx");
        let file = write_fixture(&path, true);
        let corrupt = &file.manifest.blocks["expert.0.3"];
        let mut writer = fs::OpenOptions::new().write(true).open(&path).unwrap();
        writer.seek(SeekFrom::Start(corrupt.offset)).unwrap();
        writer.write_all(&[255]).unwrap();
        // Open authenticates only metadata. Cold weights aren't touched until requested.
        let file = Arc::new(NativeFile::open(&path).unwrap());
        let plan = ExpertPlan::new(&file.manifest, 1 << 20, 0, 0).unwrap();
        let mut cache = ExpertCache::new(
            file,
            &plan,
            (0..plan.slots)
                .map(|_| vec![0; plan.slot_bytes as usize])
                .collect(),
        )
        .unwrap();
        {
            let _batch = cache.acquire(0, &[0, 1]).unwrap();
        }
        assert!(cache.acquire(0, &[3]).is_err());
        assert!(
            cache.acquire(0, &[3]).is_err(),
            "a failed expert read must not become a cache hit"
        );
    }

    #[test]
    #[ignore = "requires LOOM_NATIVE_MOE pointing at a converted native MoE file"]
    fn real_native_moe_paging_reads_only_requested_blocks() {
        let path = std::env::var_os("LOOM_NATIVE_MOE").expect("LOOM_NATIVE_MOE");
        let started = std::time::Instant::now();
        let file = Arc::new(NativeFile::open(std::path::Path::new(&path)).unwrap());
        let metadata_ms = started.elapsed().as_secs_f64() * 1000.0;
        let FeedForward::Moe {
            active, experts, ..
        } = file.manifest.model.feed_forward
        else {
            panic!("expected MoE")
        };
        assert!(file.manifest.model.layers >= 2 && experts >= active);
        let resident: u64 = file
            .manifest
            .blocks
            .values()
            .filter(|b| !matches!(b.kind, BlockKind::Expert { .. }))
            .map(|b| b.bytes)
            .sum();
        let max = file
            .manifest
            .expert_blocks()
            .values()
            .map(|b| b.bytes)
            .max()
            .unwrap();
        let slot = (max + ALIGNMENT - 1) / ALIGNMENT * ALIGNMENT;
        let reserve = 64 << 20;
        let plan = ExpertPlan::new(
            &file.manifest,
            resident + slot * u64::from(active) * 2 + reserve,
            0,
            reserve,
        )
        .unwrap();
        assert_eq!(plan.slots, active * 2);
        let mut cache = ExpertCache::new(
            file.clone(),
            &plan,
            (0..plan.slots)
                .map(|_| vec![0; plan.slot_bytes as usize])
                .collect(),
        )
        .unwrap();
        let selected: Vec<_> = (0..active).collect();
        let cold = std::time::Instant::now();
        {
            let batch = cache.acquire(0, &selected).unwrap();
            assert_eq!(batch.mapping().len(), active as usize);
        }
        let cold_ms = cold.elapsed().as_secs_f64() * 1000.0;
        let first = cache.stats();
        assert_eq!((first.0, first.1), (0, u64::from(active)));
        let warm = std::time::Instant::now();
        {
            let _batch = cache.acquire(0, &selected).unwrap();
        }
        let warm_ms = warm.elapsed().as_secs_f64() * 1000.0;
        assert_eq!(
            cache.stats(),
            (u64::from(active), u64::from(active), first.2)
        );
        {
            let _batch = cache.acquire(1, &selected).unwrap();
        }
        assert_eq!(cache.stats().1, u64::from(active) * 2);
        assert!(
            plan.cache_bytes < file.bytes / 8,
            "slot buffers must be much smaller than the bank"
        );
        eprintln!(
            "native paging IO (not generation): file={} resident_payload={} slots={} slot_pool={} metadata_ms={metadata_ms:.2} first_selected_ms={cold_ms:.2} cached_selected_ms={warm_ms:.3} first_selected_read_bytes={}",
            file.bytes, plan.resident_bytes, plan.slots, plan.cache_bytes, first.2
        );
    }
}
