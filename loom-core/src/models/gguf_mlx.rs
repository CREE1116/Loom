//! GGUF block quantization repacked as MLX affine groups, in memory, one tensor at a time.
//!
//! Every type here stores 32-value groups as `level × scale + bias`, which is exactly MLX's
//! affine layout at group size 32, so the repack keeps the quantized levels and never
//! expands weights to FP16. Scales and biases become FP16 (K-quant sub-block scales are
//! `d·sc` products and round once). Types with 16-value groups (`q6_K`, `q3_K`, `q2_K`)
//! convert the same way for storage (LMX v2) but need a kernel MLX does not have. Names become canonical Hugging Face names so adapters
//! trained against HF checkpoints apply to the same tensors; the LMX file is never changed.
use anyhow::{Context, Result, bail, ensure};

/// MLX affine group size every supported GGUF block type maps onto.
pub const GROUP: u64 = 32;

/// Architectures whose GGUF tensors map to the MLX engine without reordering. Llama-family
/// GGUFs permute Q/K rows for llama.cpp's RoPE and need the inverse permutation first.
pub const ARCHITECTURES: &[&str] = &["qwen2", "qwen3"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(i32)]
pub enum Dtype {
    U32 = 0,
    F16 = 1,
}

/// Final MLX storage allocated before streaming conversion begins.
#[derive(Debug)]
pub struct TensorLayout {
    pub name: String,
    pub dtype: Dtype,
    pub shape: Vec<i64>,
    pub bytes: usize,
}

#[cfg(test)]
#[derive(Debug)]
pub struct Converted {
    pub name: String,
    pub shape: Vec<i64>,
    pub data: Vec<u8>,
}

/// (bits, group values, block values, block bytes) of a GGUF block type whose values are
/// `level × scale + bias` per group, i.e. an affine layout.
fn block(type_name: &str) -> Option<(u32, usize, usize, usize)> {
    Some(match type_name {
        "q8_0" => (8, 32, 32, 34),
        "q4_0" => (4, 32, 32, 18),
        "q4_1" => (4, 32, 32, 20),
        "q5_0" => (5, 32, 32, 22),
        "q5_1" => (5, 32, 32, 24),
        "q4_K" => (4, 32, 256, 144),
        "q5_K" => (5, 32, 256, 176),
        "q6_K" => (6, 16, 256, 210),
        "q3_K" => (3, 16, 256, 110),
        "q2_K" => (2, 16, 256, 84),
        _ => return None,
    })
}

/// (bits, group size) of the affine form a GGUF type converts to without expanding its
/// values, whether or not an engine can run that group size yet.
pub fn affine(type_name: &str) -> Option<(u32, usize)> {
    block(type_name).map(|(bits, group, ..)| (bits, group))
}

fn dense_size(type_name: &str) -> Option<usize> {
    match type_name {
        "f32" | "F32" => Some(4),
        "f16" | "F16" | "bf16" | "BF16" => Some(2),
        _ => None,
    }
}

/// Why MLX cannot run a tensor stored as `type_name`, or `None` when it can.
pub fn unsupported(type_name: &str) -> Option<String> {
    if block(type_name).is_some_and(|(_, group, ..)| group as u64 == GROUP) || dense_size(type_name).is_some() {
        return None;
    }
    Some(match type_name {
        "q6_K" | "q3_K" | "q2_K" => {
            format!("{type_name} uses 16-value groups, which need a dedicated MLX kernel")
        }
        _ => format!("{type_name} has no MLX mapping"),
    })
}

/// Bytes a tensor occupies once converted: packed levels plus FP16 scales and biases.
pub fn converted_bytes(type_name: &str, shape: &[u64]) -> u64 {
    let values: u64 = shape.iter().product();
    match block(type_name) {
        Some((bits, group, ..)) => values * u64::from(bits) / 8 + values / group as u64 * 4,
        None => values * 2,
    }
}

/// Canonical Hugging Face name of a GGUF tensor (`blk.3.attn_q.weight` →
/// `model.layers.3.self_attn.q_proj.weight`).
pub fn hf_name(gguf: &str) -> Option<String> {
    // Qwen3.5 linear-attention parameters whose GGUF names have no `.weight` form.
    if let Some((layer, part)) = gguf.strip_prefix("blk.").and_then(|rest| rest.split_once('.')) {
        layer.parse::<u32>().ok()?;
        match part {
            "ssm_a" => return Some(format!("model.layers.{layer}.linear_attn.A_log")),
            "ssm_dt.bias" => return Some(format!("model.layers.{layer}.linear_attn.dt_bias")),
            // LFM2-MoE: the expert selection bias.
            "exp_probs_b.bias" => return Some(format!("model.layers.{layer}.mlp.expert_bias")),
            _ => {}
        }
    }
    let (base, suffix) = gguf.rsplit_once('.')?;
    if !matches!(suffix, "weight" | "bias") {
        return None;
    }
    let mapped = match base {
        "token_embd" => "model.embed_tokens".to_owned(),
        // LFM2 names its final norm `token_embd_norm`.
        "output_norm" | "token_embd_norm" => "model.norm".to_owned(),
        "output" => "lm_head".to_owned(),
        _ => {
            let (layer, part) = base.strip_prefix("blk.")?.split_once('.')?;
            layer.parse::<u32>().ok()?;
            let part = match part {
                "attn_norm" => "input_layernorm",
                "ffn_norm" => "post_attention_layernorm",
                "attn_q" => "self_attn.q_proj",
                "attn_k" => "self_attn.k_proj",
                "attn_v" => "self_attn.v_proj",
                "attn_output" => "self_attn.o_proj",
                "attn_q_norm" => "self_attn.q_norm",
                "attn_k_norm" => "self_attn.k_norm",
                "ffn_gate" => "mlp.gate_proj",
                "ffn_up" => "mlp.up_proj",
                "ffn_down" => "mlp.down_proj",
                "ffn_gate_inp" => "mlp.gate",
                "ffn_gate_exps" => "mlp.switch_mlp.gate_proj",
                "ffn_up_exps" => "mlp.switch_mlp.up_proj",
                "ffn_down_exps" => "mlp.switch_mlp.down_proj",
                // Qwen3.5: post-attention norm, gated DeltaNet and the shared expert.
                "post_attention_norm" => "post_attention_layernorm",
                "attn_qkv" => "linear_attn.in_proj_qkv",
                "attn_gate" => "linear_attn.in_proj_z",
                "ssm_beta" => "linear_attn.in_proj_b",
                "ssm_alpha" => "linear_attn.in_proj_a",
                "ssm_conv1d" => "linear_attn.conv1d",
                "ssm_norm" => "linear_attn.norm",
                "ssm_out" => "linear_attn.out_proj",
                "ffn_gate_shexp" => "mlp.shared_expert.gate_proj",
                "ffn_up_shexp" => "mlp.shared_expert.up_proj",
                "ffn_down_shexp" => "mlp.shared_expert.down_proj",
                "ffn_gate_inp_shexp" => "mlp.shared_expert_gate",
                // LFM2: gated short convolution.
                "shortconv.conv" => "conv.conv",
                "shortconv.in_proj" => "conv.in_proj",
                "shortconv.out_proj" => "conv.out_proj",
                _ => return None,
            };
            format!("model.layers.{layer}.{part}")
        }
    };
    Some(format!("{mapped}.{suffix}"))
}

pub fn f16_to_f32(bits: u16) -> f32 {
    let sign = u32::from(bits >> 15) << 31;
    let exponent = u32::from((bits >> 10) & 0x1f);
    let mantissa = u32::from(bits & 0x3ff);
    let value = match (exponent, mantissa) {
        (0, 0) => sign,
        (0, _) => {
            // Subnormal: renormalize into an f32 exponent.
            let shift = mantissa.leading_zeros() - 21;
            sign | ((113 - shift) << 23) | (((mantissa << shift) & 0x3ff) << 13)
        }
        (0x1f, _) => sign | 0x7f80_0000 | (mantissa << 13),
        _ => sign | ((exponent + 112) << 23) | (mantissa << 13),
    };
    f32::from_bits(value)
}

/// Round to nearest even, as hardware conversion does.
pub fn f32_to_f16(value: f32) -> u16 {
    let bits = value.to_bits();
    let sign = ((bits >> 16) & 0x8000) as u16;
    let exponent = ((bits >> 23) & 0xff) as i32;
    let mantissa = bits & 0x7f_ffff;
    if exponent == 0xff {
        return sign | 0x7c00 | if mantissa != 0 { 0x200 } else { 0 };
    }
    let half_exponent = exponent - 127 + 15;
    if half_exponent >= 0x1f {
        return sign | 0x7c00;
    }
    if half_exponent <= 0 {
        if half_exponent < -10 {
            return sign;
        }
        let full = mantissa | 0x80_0000;
        let shift = (14 - half_exponent) as u32;
        let mut half = full >> shift;
        let rest = full & ((1 << shift) - 1);
        let halfway = 1 << (shift - 1);
        if rest > halfway || (rest == halfway && half & 1 == 1) {
            half += 1;
        }
        return sign | half as u16;
    }
    let mut half = ((half_exponent as u32) << 10) | (mantissa >> 13);
    let rest = mantissa & 0x1fff;
    if rest > 0x1000 || (rest == 0x1000 && half & 1 == 1) {
        half += 1; // A carry into the exponent is the correct rounding.
    }
    sign | half as u16
}

fn half(bytes: &[u8], at: usize) -> f32 {
    f16_to_f32(u16::from_le_bytes([bytes[at], bytes[at + 1]]))
}

/// llama.cpp's `get_scale_min_k4`: 6-bit sub-block scale and min of K-quants.
fn scale_min_k4(j: usize, q: &[u8]) -> (u8, u8) {
    if j < 4 {
        (q[j] & 63, q[j + 4] & 63)
    } else {
        (
            (q[j + 4] & 0xf) | ((q[j - 4] >> 6) << 4),
            (q[j + 4] >> 4) | ((q[j] >> 6) << 4),
        )
    }
}

/// One block's levels (unsigned, `0..2^bits`) and per-group scale and bias, so that
/// `value = level × scale + bias` reproduces llama.cpp's dequantization.
fn decode(type_name: &str, b: &[u8], levels: &mut [u8], scales: &mut [f32], biases: &mut [f32]) {
    let nibbles = |qs: &[u8], levels: &mut [u8]| {
        // ggml order: low nibbles are values 0..16, high nibbles values 16..32.
        for j in 0..16 {
            levels[j] = qs[j] & 0xf;
            levels[j + 16] = qs[j] >> 4;
        }
    };
    let high_bits = |qh: u32, levels: &mut [u8]| {
        for j in 0..16 {
            levels[j] |= (((qh >> j) << 4) & 0x10) as u8;
            levels[j + 16] |= ((qh >> (j + 12)) & 0x10) as u8;
        }
    };
    match type_name {
        "q8_0" => {
            let d = half(b, 0);
            for j in 0..32 {
                levels[j] = (b[2 + j] as i8 as i16 + 128) as u8;
            }
            (scales[0], biases[0]) = (d, -128.0 * d);
        }
        "q4_0" => {
            let d = half(b, 0);
            nibbles(&b[2..18], levels);
            (scales[0], biases[0]) = (d, -8.0 * d);
        }
        "q4_1" => {
            nibbles(&b[4..20], levels);
            (scales[0], biases[0]) = (half(b, 0), half(b, 2));
        }
        "q5_0" => {
            let d = half(b, 0);
            nibbles(&b[6..22], levels);
            high_bits(u32::from_le_bytes(b[2..6].try_into().unwrap()), levels);
            (scales[0], biases[0]) = (d, -16.0 * d);
        }
        "q5_1" => {
            nibbles(&b[8..24], levels);
            high_bits(u32::from_le_bytes(b[4..8].try_into().unwrap()), levels);
            (scales[0], biases[0]) = (half(b, 0), half(b, 2));
        }
        "q4_K" | "q5_K" => {
            let (d, dmin) = (half(b, 0), half(b, 2));
            let packed = &b[4..16];
            let five = type_name == "q5_K";
            let (qh, qs) = if five {
                (&b[16..48], &b[48..176])
            } else {
                (&b[0..0], &b[16..144])
            };
            for chunk in 0..4 {
                let q = &qs[chunk * 32..chunk * 32 + 32];
                for half_index in 0..2 {
                    let sub = chunk * 2 + half_index;
                    let (sc, m) = scale_min_k4(sub, packed);
                    scales[sub] = d * f32::from(sc);
                    biases[sub] = -(dmin * f32::from(m));
                    for l in 0..32 {
                        let mut level = if half_index == 0 {
                            q[l] & 0xf
                        } else {
                            q[l] >> 4
                        };
                        if five && qh[l] & (1 << sub) != 0 {
                            level += 16;
                        }
                        levels[sub * 32 + l] = level;
                    }
                }
            }
        }
        "q6_K" => {
            // ql: low 4 bits, qh: high 2 bits, 16 signed sub-block scales; value = d·sc·(q − 32).
            let (ql, qh, sc, d) = (&b[0..128], &b[128..192], &b[192..208], half(b, 208));
            for n in 0..2 {
                let (ql, qh) = (&ql[n * 64..], &qh[n * 32..]);
                for l in 0..32 {
                    let at = n * 128 + l;
                    levels[at] = (ql[l] & 0xf) | ((qh[l] & 3) << 4);
                    levels[at + 32] = (ql[l + 32] & 0xf) | (((qh[l] >> 2) & 3) << 4);
                    levels[at + 64] = (ql[l] >> 4) | (((qh[l] >> 4) & 3) << 4);
                    levels[at + 96] = (ql[l + 32] >> 4) | (((qh[l] >> 6) & 3) << 4);
                }
            }
            for (sub, &scale) in sc.iter().enumerate() {
                let scale = d * f32::from(scale as i8);
                (scales[sub], biases[sub]) = (scale, -32.0 * scale);
            }
        }
        "q3_K" => {
            // 2 low bits in qs, the third in hmask (set means +4); value = d·(sc − 32)·(q − 4).
            let (hmask, qs, packed, d) = (&b[0..32], &b[32..96], &b[96..108], half(b, 108));
            let word = |i: usize| u32::from_le_bytes(packed[i * 4..i * 4 + 4].try_into().unwrap());
            let (a0, a1, tmp) = (word(0), word(1), word(2));
            let (m2, m1) = (0x0f0f_0f0fu32, 0x0303_0303u32);
            let aux = [
                (a0 & m2) | ((tmp & m1) << 4),
                (a1 & m2) | (((tmp >> 2) & m1) << 4),
                ((a0 >> 4) & m2) | (((tmp >> 4) & m1) << 4),
                ((a1 >> 4) & m2) | (((tmp >> 6) & m1) << 4),
            ];
            let sub_scales: Vec<i8> = aux.iter().flat_map(|w| w.to_le_bytes()).map(|b| b as i8).collect();
            for n in 0..2 {
                let q = &qs[n * 32..];
                for j in 0..4 {
                    let (shift, mask) = (j * 2, 1u8 << (n * 4 + j));
                    for l in 0..32 {
                        let high = if hmask[l] & mask != 0 { 4 } else { 0 };
                        levels[n * 128 + j * 32 + l] = ((q[l] >> shift) & 3) | high;
                    }
                }
            }
            for (sub, &scale) in sub_scales.iter().enumerate() {
                let scale = d * f32::from(i16::from(scale) - 32);
                (scales[sub], biases[sub]) = (scale, -4.0 * scale);
            }
        }
        "q2_K" => {
            // 4-bit scale and 4-bit min per 16 values; value = d·sc·q − dmin·m.
            let (sc, qs, d, dmin) = (&b[0..16], &b[16..80], half(b, 80), half(b, 82));
            for n in 0..2 {
                let q = &qs[n * 32..];
                for j in 0..4 {
                    for l in 0..32 {
                        levels[n * 128 + j * 32 + l] = (q[l] >> (j * 2)) & 3;
                    }
                }
            }
            for (sub, &packed) in sc.iter().enumerate() {
                scales[sub] = d * f32::from(packed & 0xf);
                biases[sub] = -(dmin * f32::from(packed >> 4));
            }
        }
        _ => unreachable!("checked by block()"),
    }
}

/// Little-endian bitstream: value `i` occupies bits `[i·bits, (i+1)·bits)` across u32 words.
fn pack(levels: &[u8], bits: u32, out: &mut [u8]) {
    let (mut word, mut filled, mut at) = (0u64, 0u32, 0usize);
    for &level in levels {
        word |= u64::from(level) << filled;
        filled += bits;
        if filled >= 32 {
            out[at..at + 4].copy_from_slice(&(word as u32).to_le_bytes());
            at += 4;
            word >>= 32;
            filled -= 32;
        }
    }
    debug_assert_eq!(filled, 0, "rows are whole words");
}

/// Validate one source tensor and describe its final MLX buffers without reading weights.
pub fn layout(
    gguf_name: &str,
    type_name: &str,
    shape: &[u64],
    bytes: u64,
) -> Result<Vec<TensorLayout>> {
    let name =
        hf_name(gguf_name).with_context(|| format!("no MLX name for GGUF tensor {gguf_name}"))?;
    let dims: Vec<i64> = shape
        .iter()
        .map(|&n| i64::try_from(n))
        .collect::<Result<_, _>>()?;
    let values = usize::try_from(
        shape
            .iter()
            .try_fold(1u64, |n, &dim| n.checked_mul(dim))
            .context("tensor size overflow")?,
    )?;
    let source_bytes = usize::try_from(bytes)?;
    if let Some(size) = dense_size(type_name) {
        ensure!(
            values.checked_mul(size) == Some(source_bytes),
            "{gguf_name}: {type_name} bytes do not match its shape"
        );
        return Ok(vec![TensorLayout {
            name,
            dtype: Dtype::F16,
            shape: dims,
            bytes: values * 2,
        }]);
    }
    let Some((bits, group, block_values, block_bytes)) = block(type_name) else {
        bail!(
            "{gguf_name}: {}",
            unsupported(type_name).unwrap_or_default()
        )
    };
    let columns = usize::try_from(*shape.last().context("scalar tensor")?)?;
    ensure!(
        columns > 0 && columns % block_values == 0,
        "{gguf_name}: rows do not divide into {type_name} blocks"
    );
    ensure!(
        (values / block_values).checked_mul(block_bytes) == Some(source_bytes),
        "{gguf_name}: {type_name} bytes do not match its shape"
    );
    let base = name
        .strip_suffix(".weight")
        .with_context(|| format!("{gguf_name}: quantized tensor is not a weight"))?;
    let outer = &dims[..dims.len() - 1];
    let with_last = |last: usize| {
        outer
            .iter()
            .copied()
            .chain([last as i64])
            .collect::<Vec<_>>()
    };
    Ok(vec![
        TensorLayout {
            name: name.clone(),
            dtype: Dtype::U32,
            shape: with_last(columns / 32 * bits as usize),
            bytes: values / 8 * bits as usize,
        },
        TensorLayout {
            name: format!("{base}.scales"),
            dtype: Dtype::F16,
            shape: with_last(columns / group),
            bytes: values / group * 2,
        },
        TensorLayout {
            name: format!("{base}.biases"),
            dtype: Dtype::F16,
            shape: with_last(columns / group),
            bytes: values / group * 2,
        },
    ])
}

/// Stream GGUF blocks straight into caller-owned final buffers. Scratch is at most 64 KiB,
/// independent of tensor size; there is no whole-source or converted-tensor CPU copy.
pub fn convert_into(
    type_name: &str,
    source: &mut impl std::io::Read,
    outputs: &mut [&mut [u8]],
) -> Result<()> {
    let mut scratch = [0u8; 64 * 1024];
    if let Some(size) = dense_size(type_name) {
        ensure!(
            outputs.len() == 1 && outputs[0].len() % 2 == 0,
            "invalid dense output buffers"
        );
        for out in outputs[0].chunks_mut(scratch.len() / size * 2) {
            let input = &mut scratch[..out.len() / 2 * size];
            source.read_exact(input)?;
            for (chunk, target) in input.chunks_exact(size).zip(out.chunks_exact_mut(2)) {
                let half = match type_name {
                    "f32" | "F32" => f32_to_f16(f32::from_le_bytes(chunk.try_into().unwrap())),
                    "bf16" | "BF16" => f32_to_f16(f32::from_bits(
                        u32::from(u16::from_le_bytes([chunk[0], chunk[1]])) << 16,
                    )),
                    _ => u16::from_le_bytes([chunk[0], chunk[1]]),
                };
                target.copy_from_slice(&half.to_le_bytes());
            }
        }
        return Ok(());
    }
    let (bits, group, block_values, block_bytes) =
        block(type_name).context("unsupported GGUF block type")?;
    let groups = block_values / group;
    let packed_bytes = block_values / 8 * bits as usize;
    ensure!(outputs.len() == 3, "invalid affine output buffers");
    let (packed, rest) = outputs.split_at_mut(1);
    let (scales, biases) = rest.split_at_mut(1);
    let blocks = packed[0].len() / packed_bytes;
    ensure!(
        packed[0].len() % packed_bytes == 0
            && scales[0].len() == blocks * groups * 2
            && biases[0].len() == scales[0].len(),
        "invalid affine output sizes"
    );
    let mut levels = [0u8; 256];
    let (mut block_scales, mut block_biases) = ([0f32; 16], [0f32; 16]);
    let blocks_per_read = scratch.len() / block_bytes;
    for start in (0..blocks).step_by(blocks_per_read) {
        let count = (blocks - start).min(blocks_per_read);
        let input = &mut scratch[..count * block_bytes];
        source.read_exact(input)?;
        for (index, chunk) in input.chunks_exact(block_bytes).enumerate() {
            let at = start + index;
            decode(
                type_name,
                chunk,
                &mut levels[..block_values],
                &mut block_scales[..groups],
                &mut block_biases[..groups],
            );
            pack(
                &levels[..block_values],
                bits,
                &mut packed[0][at * packed_bytes..(at + 1) * packed_bytes],
            );
            for group in 0..groups {
                let offset = (at * groups + group) * 2;
                scales[0][offset..offset + 2]
                    .copy_from_slice(&f32_to_f16(block_scales[group]).to_le_bytes());
                biases[0][offset..offset + 2]
                    .copy_from_slice(&f32_to_f16(block_biases[group]).to_le_bytes());
            }
        }
    }
    Ok(())
}

/// Test helper uses the same streaming converter with owned output buffers.
#[cfg(test)]
fn convert(gguf_name: &str, type_name: &str, shape: &[u64], data: &[u8]) -> Result<Vec<Converted>> {
    let mut out: Vec<Converted> = layout(gguf_name, type_name, shape, data.len() as u64)?
        .into_iter()
        .map(|t| Converted {
            name: t.name,
            shape: t.shape,
            data: vec![0; t.bytes],
        })
        .collect();
    let mut buffers: Vec<&mut [u8]> = out.iter_mut().map(|t| t.data.as_mut_slice()).collect();
    convert_into(type_name, &mut &*data, &mut buffers)?;
    Ok(out)
}

/// What the MLX engine needs to know about a GGUF model, read from its own metadata.
#[derive(Clone, Debug)]
pub struct GgufConfig {
    pub architecture: String,
    pub hidden: i32,
    pub layers: i32,
    pub heads: i32,
    pub kv_heads: i32,
    pub head_dim: i32,
    pub eps: f32,
    pub rope_theta: f32,
    pub context: usize,
    pub intermediate: usize,
    pub template: String,
}

pub fn read_config(path: &std::path::Path) -> Result<GgufConfig> {
    use llama_cpp_sys_2 as sys;
    use std::ffi::{CStr, CString};
    super::native_runtime::silence_native_logs();
    let name = CString::new(path.to_str().context("GGUF path is not UTF-8")?)?;
    let ctx = unsafe {
        sys::gguf_init_from_file(
            name.as_ptr(),
            sys::gguf_init_params {
                no_alloc: true,
                ctx: std::ptr::null_mut(),
            },
        )
    };
    ensure!(!ctx.is_null(), "invalid GGUF header");
    struct Guard(*mut sys::gguf_context);
    impl Drop for Guard {
        fn drop(&mut self) {
            unsafe { sys::gguf_free(self.0) };
        }
    }
    let guard = Guard(ctx);
    let find = |key: &str| -> Option<i64> {
        let key = CString::new(key).ok()?;
        let index = unsafe { sys::gguf_find_key(guard.0, key.as_ptr()) };
        (index >= 0).then_some(index)
    };
    // SAFETY: indices come from gguf_find_key on the live context; types are checked first.
    let string = |key: &str| -> Option<String> {
        let index = find(key)?;
        unsafe {
            (sys::gguf_get_kv_type(guard.0, index) == sys::GGUF_TYPE_STRING).then(|| {
                CStr::from_ptr(sys::gguf_get_val_str(guard.0, index))
                    .to_string_lossy()
                    .into_owned()
            })
        }
    };
    let number = |key: &str| -> Option<f64> {
        let index = find(key)?;
        unsafe {
            let kind = sys::gguf_get_kv_type(guard.0, index);
            Some(if kind == sys::GGUF_TYPE_UINT32 {
                f64::from(sys::gguf_get_val_u32(guard.0, index))
            } else if kind == sys::GGUF_TYPE_INT32 {
                f64::from(sys::gguf_get_val_i32(guard.0, index))
            } else if kind == sys::GGUF_TYPE_UINT64 {
                sys::gguf_get_val_u64(guard.0, index) as f64
            } else if kind == sys::GGUF_TYPE_FLOAT32 {
                f64::from(sys::gguf_get_val_f32(guard.0, index))
            } else {
                return None;
            })
        }
    };
    let architecture = string("general.architecture").context("missing GGUF architecture")?;
    let key = |suffix: &str| format!("{architecture}.{suffix}");
    let required =
        |suffix: &str| number(&key(suffix)).with_context(|| format!("GGUF has no {}", key(suffix)));
    let hidden = required("embedding_length")? as i32;
    let heads = required("attention.head_count")? as i32;
    ensure!(hidden > 0 && heads > 0, "invalid GGUF attention shape");
    Ok(GgufConfig {
        hidden,
        layers: required("block_count")? as i32,
        heads,
        kv_heads: number(&key("attention.head_count_kv")).map_or(heads, |n| n as i32),
        head_dim: number(&key("attention.key_length")).map_or(hidden / heads, |n| n as i32),
        eps: number(&key("attention.layer_norm_rms_epsilon")).unwrap_or(1e-6) as f32,
        rope_theta: number(&key("rope.freq_base")).unwrap_or(10_000.0) as f32,
        context: number(&key("context_length")).unwrap_or(32_768.0) as usize,
        intermediate: number(&key("feed_forward_length")).unwrap_or(f64::from(hidden) * 4.0)
            as usize,
        template: string("tokenizer.chat_template").context("GGUF has no chat template")?,
        architecture,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unpack(words: &[u8], bits: u32, count: usize) -> Vec<u8> {
        (0..count)
            .map(|i| {
                let bit = i * bits as usize;
                let word = |w: usize| {
                    u64::from(u32::from_le_bytes(
                        words[w * 4..w * 4 + 4].try_into().unwrap(),
                    ))
                };
                let low = word(bit / 32);
                let high = if bit / 32 + 1 < words.len() / 4 {
                    word(bit / 32 + 1)
                } else {
                    0
                };
                (((low | high << 32) >> (bit % 32)) & ((1 << bits) - 1)) as u8
            })
            .collect()
    }

    /// Reference dequantization straight from llama.cpp's `dequantize_row_*`.
    fn reference(type_name: &str, b: &[u8]) -> Vec<f32> {
        let d = half(b, 0);
        match type_name {
            "q8_0" => (0..32).map(|j| d * f32::from(b[2 + j] as i8)).collect(),
            "q4_0" => {
                let mut y = vec![0.0; 32];
                for j in 0..16 {
                    y[j] = (f32::from(b[2 + j] & 0xf) - 8.0) * d;
                    y[j + 16] = (f32::from(b[2 + j] >> 4) - 8.0) * d;
                }
                y
            }
            "q4_1" => {
                let m = half(b, 2);
                let mut y = vec![0.0; 32];
                for j in 0..16 {
                    y[j] = f32::from(b[4 + j] & 0xf) * d + m;
                    y[j + 16] = f32::from(b[4 + j] >> 4) * d + m;
                }
                y
            }
            "q5_0" | "q5_1" => {
                let (qh_at, qs_at, m) = if type_name == "q5_0" {
                    (2, 6, None)
                } else {
                    (4, 8, Some(half(b, 2)))
                };
                let qh = u32::from_le_bytes(b[qh_at..qh_at + 4].try_into().unwrap());
                let mut y = vec![0.0; 32];
                for j in 0..16 {
                    let xh0 = ((qh >> j) << 4) & 0x10;
                    let xh1 = (qh >> (j + 12)) & 0x10;
                    let x0 = (u32::from(b[qs_at + j] & 0xf) | xh0) as f32;
                    let x1 = (u32::from(b[qs_at + j] >> 4) | xh1) as f32;
                    (y[j], y[j + 16]) = match m {
                        None => ((x0 - 16.0) * d, (x1 - 16.0) * d),
                        Some(m) => (x0 * d + m, x1 * d + m),
                    };
                }
                y
            }
            "q4_K" | "q5_K" => {
                let dmin = half(b, 2);
                let five = type_name == "q5_K";
                let (qh, ql) = if five {
                    (&b[16..48], &b[48..176])
                } else {
                    (&b[0..0], &b[16..144])
                };
                let mut y = Vec::new();
                let (mut is, mut u1, mut u2) = (0, 1u8, 2u8);
                for j in 0..4 {
                    let q = &ql[j * 32..];
                    let (sc, m) = scale_min_k4(is, &b[4..16]);
                    let (d1, m1) = (d * f32::from(sc), dmin * f32::from(m));
                    let (sc, m) = scale_min_k4(is + 1, &b[4..16]);
                    let (d2, m2) = (d * f32::from(sc), dmin * f32::from(m));
                    for l in 0..32 {
                        let h = if five && qh[l] & u1 != 0 { 16.0 } else { 0.0 };
                        y.push(d1 * (f32::from(q[l] & 0xf) + h) - m1);
                    }
                    for l in 0..32 {
                        let h = if five && qh[l] & u2 != 0 { 16.0 } else { 0.0 };
                        y.push(d2 * (f32::from(q[l] >> 4) + h) - m2);
                    }
                    is += 2;
                    u1 <<= 2;
                    u2 <<= 2;
                }
                y
            }
            _ => unreachable!(),
        }
    }

    /// Deterministic block bytes with small, finite f16 scales.
    fn random_block(type_name: &str, seed: u64) -> Vec<u8> {
        let (_, _, _, size) = block(type_name).unwrap();
        let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
        let mut bytes: Vec<u8> = (0..size)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state as u8
            })
            .collect();
        bytes[0..2].copy_from_slice(&f32_to_f16(0.0123 + seed as f32 * 1e-4).to_le_bytes());
        if matches!(type_name, "q4_1" | "q5_1" | "q4_K" | "q5_K") {
            bytes[2..4].copy_from_slice(&f32_to_f16(-0.031).to_le_bytes());
        }
        bytes
    }

    #[test]
    fn every_supported_block_type_repacks_to_the_same_values_as_llama_cpp() {
        for type_name in ["q8_0", "q4_0", "q4_1", "q5_0", "q5_1", "q4_K", "q5_K"] {
            let (bits, _, values, _) = block(type_name).unwrap();
            let rows = 2;
            let columns = values * 2;
            let data: Vec<u8> = (0..rows * 2)
                .flat_map(|i| random_block(type_name, i as u64 + 1))
                .collect();
            let expected: Vec<f32> = data
                .chunks_exact(block(type_name).unwrap().3)
                .flat_map(|b| reference(type_name, b))
                .collect();
            let out = convert(
                "blk.0.ffn_up.weight",
                type_name,
                &[rows as u64, columns as u64],
                &data,
            )
            .unwrap();
            assert_eq!(out[0].name, "model.layers.0.mlp.up_proj.weight");
            assert_eq!(
                out[0].shape,
                vec![rows as i64, (columns * bits as usize / 32) as i64]
            );
            assert_eq!(out[1].name, "model.layers.0.mlp.up_proj.scales");
            assert_eq!(out[1].shape, vec![rows as i64, (columns / 32) as i64]);
            let levels = unpack(&out[0].data, bits, rows * columns);
            let read = |data: &[u8], i: usize| {
                f16_to_f32(u16::from_le_bytes([data[i * 2], data[i * 2 + 1]]))
            };
            for (i, want) in expected.iter().enumerate() {
                let got =
                    f32::from(levels[i]) * read(&out[1].data, i / 32) + read(&out[2].data, i / 32);
                // Legacy types are exact; K-quant sub-block products round once to f16.
                let tolerance = if type_name.ends_with("_K") {
                    want.abs() * 2e-3 + 2e-3
                } else {
                    1e-6
                };
                assert!(
                    (got - want).abs() <= tolerance,
                    "{type_name}[{i}]: {got} vs {want}"
                );
            }
        }
    }

    /// 16-value-group K-quants become affine groups of 16; llama.cpp's own dequantizer is
    /// the reference, so the repack matches what llama.cpp computes.
    #[test]
    fn sixteen_value_k_quants_repack_to_llama_cpp_values() {
        use llama_cpp_sys_2 as sys;
        for (type_name, ggml) in [("q6_K", sys::GGML_TYPE_Q6_K), ("q3_K", sys::GGML_TYPE_Q3_K), ("q2_K", sys::GGML_TYPE_Q2_K)] {
            let (bits, group) = affine(type_name).unwrap();
            assert_eq!(group, 16);
            let traits = unsafe { &*sys::ggml_get_type_traits(ggml) };
            let size = traits.type_size;
            let mut state = 0x9e37_79b9_7f4a_7c15u64;
            let mut data: Vec<u8> = (0..size * 4)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
                .collect();
            // Finite super-block scales where each type keeps them.
            for (index, chunk) in data.chunks_exact_mut(size).enumerate() {
                let d = f32_to_f16(0.0123 + index as f32 * 1e-3).to_le_bytes();
                match type_name {
                    "q6_K" => chunk[208..210].copy_from_slice(&d),
                    "q3_K" => chunk[108..110].copy_from_slice(&d),
                    _ => {
                        chunk[80..82].copy_from_slice(&d);
                        chunk[82..84].copy_from_slice(&f32_to_f16(0.007).to_le_bytes());
                    }
                }
            }
            let mut expected = vec![0f32; 4 * 256];
            unsafe { traits.to_float.unwrap()(data.as_ptr().cast(), expected.as_mut_ptr(), expected.len() as i64) };
            let out = convert("blk.0.ffn_up.weight", type_name, &[2, 512], &data).unwrap();
            assert_eq!(out[0].shape, vec![2, (512 * bits as usize / 32) as i64]);
            assert_eq!(out[1].shape, vec![2, 512 / 16]);
            let levels = unpack(&out[0].data, bits, 1024);
            let read = |data: &[u8], i: usize| f16_to_f32(u16::from_le_bytes([data[i * 2], data[i * 2 + 1]]));
            for (i, want) in expected.iter().enumerate() {
                let got = f32::from(levels[i]) * read(&out[1].data, i / 16) + read(&out[2].data, i / 16);
                // Sub-block scales are d·sc products that round once to f16.
                assert!((got - want).abs() <= want.abs() * 2e-3 + 2e-3, "{type_name}[{i}]: {got} vs {want}");
            }
            // MLX's quantized kernels have no 16-value groups yet.
            assert!(unsupported(type_name).is_some());
        }
    }

    #[test]
    fn conversion_streams_bounded_reads_across_chunk_boundaries() {
        struct Bounded<'a> {
            remaining: &'a [u8],
            largest: usize,
        }
        impl std::io::Read for Bounded<'_> {
            fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
                assert!(out.len() <= 64 * 1024, "whole tensor read requested");
                self.largest = self.largest.max(out.len());
                // Partial reads must also work, including inside a GGUF block.
                let count = out.len().min(self.remaining.len()).min(997);
                out[..count].copy_from_slice(&self.remaining[..count]);
                self.remaining = &self.remaining[count..];
                Ok(count)
            }
        }
        for kind in ["q8_0", "q4_0", "q4_1", "q5_0", "q5_1", "q4_K", "q5_K"] {
            let (bits, _, values, bytes) = block(kind).unwrap();
            let count = 64 * 1024 / bytes + 3;
            let one = random_block(kind, 7);
            let data = one.repeat(count);
            let layouts = layout(
                "blk.0.ffn_up.weight",
                kind,
                &[count as u64, values as u64],
                data.len() as u64,
            )
            .unwrap();
            let mut storage: Vec<Vec<u8>> = layouts.iter().map(|t| vec![0; t.bytes]).collect();
            let mut outputs: Vec<&mut [u8]> = storage.iter_mut().map(Vec::as_mut_slice).collect();
            let mut reader = Bounded {
                remaining: &data,
                largest: 0,
            };
            convert_into(kind, &mut reader, &mut outputs).unwrap();
            assert!(reader.remaining.is_empty());
            assert!(reader.largest > 60 * 1024);
            let expected = reference(kind, &one);
            let levels = unpack(&storage[0], bits, count * values);
            for at in [0, 1, count - 2, count - 1] {
                for (index, &want) in expected.iter().enumerate() {
                    let i = at * values + index;
                    let half_at = i / 32 * 2;
                    let read = |v: &[u8]| half(v, half_at);
                    let got = f32::from(levels[i]) * read(&storage[1]) + read(&storage[2]);
                    let tolerance = if kind.ends_with("_K") {
                        want.abs() * 2e-3 + 2e-3
                    } else {
                        1e-6
                    };
                    assert!(
                        (got - want).abs() <= tolerance,
                        "{kind}[{i}]: {got} vs {want}"
                    );
                }
            }
            let mut outputs: Vec<&mut [u8]> = storage.iter_mut().map(Vec::as_mut_slice).collect();
            assert!(convert_into(kind, &mut &data[..data.len() - 1], &mut outputs).is_err());
        }
        for kind in ["f32", "f16", "bf16"] {
            let value = match kind {
                "f32" => 1.5f32.to_le_bytes().to_vec(),
                "f16" => f32_to_f16(1.5).to_le_bytes().to_vec(),
                _ => ((1.5f32.to_bits() >> 16) as u16).to_le_bytes().to_vec(),
            };
            let data = value.repeat(40_000);
            let mut out = vec![0; 80_000];
            let mut reader = Bounded {
                remaining: &data,
                largest: 0,
            };
            convert_into(kind, &mut reader, &mut [&mut out]).unwrap();
            assert!(reader.remaining.is_empty());
            assert!(
                out.chunks_exact(2)
                    .all(|v| v == f32_to_f16(1.5).to_le_bytes())
            );
        }
    }

    #[test]
    fn dense_tensors_become_f16_and_unsupported_types_are_named() {
        let data: Vec<u8> = [1.5f32, -2.25]
            .iter()
            .flat_map(|v| v.to_le_bytes())
            .collect();
        let out = convert("output_norm.weight", "f32", &[2], &data).unwrap();
        assert_eq!(out[0].name, "model.norm.weight");
        assert_eq!(
            out[0].data,
            [
                f32_to_f16(1.5).to_le_bytes(),
                f32_to_f16(-2.25).to_le_bytes()
            ]
            .concat()
        );
        assert!(unsupported("q6_K").unwrap().contains("16-value groups"));
        assert!(unsupported("q8_0").is_none());
        // q6_K now converts for storage (MLX still refuses to run it, above); codebook
        // types such as iq3_s have no affine form.
        assert!(convert("blk.0.ffn_up.weight", "iq3_s", &[1, 256], &[0; 110]).is_err());
        assert_eq!(
            hf_name("blk.12.attn_k.bias").as_deref(),
            Some("model.layers.12.self_attn.k_proj.bias")
        );
        assert_eq!(
            hf_name("token_embd.weight").as_deref(),
            Some("model.embed_tokens.weight")
        );
        assert_eq!(hf_name("rope_freqs.weight"), None);
        assert_eq!(hf_name("blk.2.ssm_a").as_deref(), Some("model.layers.2.linear_attn.A_log"));
        assert_eq!(hf_name("blk.2.ssm_dt.bias").as_deref(), Some("model.layers.2.linear_attn.dt_bias"));
        assert_eq!(
            hf_name("blk.2.ffn_gate_inp_shexp.weight").as_deref(),
            Some("model.layers.2.mlp.shared_expert_gate.weight")
        );
    }

    #[test]
    fn half_precision_round_trips() {
        for value in [0.0f32, 1.0, -2.5, 65504.0, 6.1e-5, 5.96e-8, 0.333_333] {
            let back = f16_to_f32(f32_to_f16(value));
            assert!(
                (back - value).abs() <= value.abs() * 1e-3 + 6e-8,
                "{value} -> {back}"
            );
        }
        assert_eq!(f32_to_f16(1e6), 0x7c00);
        assert_eq!(f16_to_f32(0x3c00), 1.0);
    }
}
