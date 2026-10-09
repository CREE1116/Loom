//! Common model IR: one description of every tensor (where it lives, its logical shape,
//! how its values are stored) for all LMX payloads, so engines decide what they can run
//! from the model itself instead of from the file format.
//!
//! The IR is derived, never stored: LMX v1 indexes stay byte-identical and existing files
//! keep opening. It is rebuilt on every open from data the open already reads (the GGUF
//! header, the MLX tensor descriptors), so building it reads no weights and copies none.
use super::{
    lmx::{Entry, GgufTensor},
    model_package::ModelManifest,
};
use anyhow::{Context, Result, bail, ensure};
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};

pub const IR_VERSION: u32 = 1;

/// Payload layout the tensors were read from; engines read files, not abstractions.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceFormat {
    Gguf,
    Safetensors,
}

/// How a tensor's values are stored. Logical meaning (shape) is kept separately.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Storage {
    /// One value per element: F32, F16, BF16, U32, …
    Dense { dtype: String },
    /// MLX affine groups along the last axis; `scales`/`biases` name sibling tensors.
    MlxAffine {
        bits: u64,
        group_size: u64,
        axis: i32,
        scales: String,
        biases: Option<String>,
    },
    /// ggml block quantization (Q4_0, Q4_K, Q6_K, …): `block_size` values per
    /// `type_size`-byte block along the last axis.
    GgmlBlock {
        type_name: String,
        block_size: u64,
        type_size: u64,
    },
}

impl Storage {
    /// A short stable name for reports: `dense:BF16`, `mlx_affine:4`, `ggml:q6_K`.
    pub fn label(&self) -> String {
        match self {
            Self::Dense { dtype } => format!("dense:{dtype}"),
            Self::MlxAffine { bits, .. } => format!("mlx_affine:{bits}"),
            Self::GgmlBlock { type_name, .. } => format!("ggml:{type_name}"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Tensor {
    /// LMX entry (payload file) holding the bytes.
    pub entry: String,
    /// Byte offset within that entry.
    pub offset: u64,
    pub bytes: u64,
    /// Logical shape, outermost dimension first (row-major), before quantization.
    pub shape: Vec<u64>,
    pub storage: Storage,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ModelIr {
    pub version: u32,
    pub format: SourceFormat,
    pub architecture: String,
    pub tensors: BTreeMap<String, Tensor>,
}

/// Per-tensor facts from a GGUF header beyond what the LMX v1 index stores.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GgufLayout {
    /// ggml `ne`: element counts, fastest-varying dimension first, unused ones 1.
    pub ne: [u64; 4],
    pub block_size: u64,
    pub type_size: u64,
}

/// GGUF lists the fastest-varying dimension first; the IR lists the outermost first, like
/// safetensors. Trailing unit dimensions of `ne` are padding and are dropped (at least one
/// dimension stays), so a `[n, 1]` tensor and an `[n]` tensor have the same IR shape.
pub fn gguf_shape(ne: [u64; 4]) -> Vec<u64> {
    let used = ne.iter().rposition(|&n| n != 1).map_or(1, |last| last + 1);
    ne[..used].iter().rev().copied().collect()
}

fn dense_size(dtype: &str) -> Option<u64> {
    Some(match dtype.to_ascii_uppercase().as_str() {
        "F64" | "I64" | "U64" => 8,
        "F32" | "I32" | "U32" => 4,
        "F16" | "BF16" | "I16" | "U16" => 2,
        "I8" | "U8" | "BOOL" => 1,
        _ => return None,
    })
}

fn product(shape: &[u64]) -> Option<u64> {
    shape.iter().try_fold(1u64, |acc, &n| acc.checked_mul(n))
}

impl ModelIr {
    /// `tensors` and `layouts` come from one GGUF header read, in the same order.
    pub fn from_gguf(
        architecture: &str,
        entry: &str,
        tensors: &[GgufTensor],
        layouts: &[GgufLayout],
    ) -> Result<Self> {
        ensure!(tensors.len() == layouts.len(), "GGUF layout count mismatch");
        let mut out = BTreeMap::new();
        for (tensor, layout) in tensors.iter().zip(layouts) {
            let storage = if layout.block_size == 1 {
                Storage::Dense { dtype: tensor.storage_type.to_ascii_uppercase() }
            } else {
                Storage::GgmlBlock {
                    type_name: tensor.storage_type.clone(),
                    block_size: layout.block_size,
                    type_size: layout.type_size,
                }
            };
            out.insert(
                tensor.name.clone(),
                Tensor {
                    entry: entry.to_owned(),
                    offset: tensor.offset,
                    bytes: tensor.bytes,
                    shape: gguf_shape(layout.ne),
                    storage,
                },
            );
        }
        Ok(Self {
            version: IR_VERSION,
            format: SourceFormat::Gguf,
            architecture: architecture.to_owned(),
            tensors: out,
        })
    }

    pub fn from_mlx(model: &ModelManifest) -> Result<Self> {
        let architecture = model.config["model_type"]
            .as_str()
            .context("MLX config has no model_type")?
            .to_owned();
        let mut out = BTreeMap::new();
        for (name, spec) in &model.tensors {
            let (shape, storage) = match &spec.affine {
                Some(affine) => {
                    // The stored weight is packed U32; the logical last axis holds
                    // 32 / bits values per packed element.
                    let mut shape = spec.shape.clone();
                    let last = shape.last_mut().context("affine tensor has no shape")?;
                    ensure!(affine.bits > 0, "affine tensor with zero bits: {name}");
                    *last = last
                        .checked_mul(32)
                        .context("affine shape overflow")?
                        / affine.bits;
                    (
                        shape,
                        Storage::MlxAffine {
                            bits: affine.bits,
                            group_size: affine.group_size,
                            axis: affine.axis,
                            scales: affine.scales.clone(),
                            biases: affine.biases.clone(),
                        },
                    )
                }
                None => (spec.shape.clone(), Storage::Dense { dtype: spec.dtype.clone() }),
            };
            out.insert(
                name.clone(),
                Tensor { entry: spec.file.clone(), offset: spec.offset, bytes: spec.bytes, shape, storage },
            );
        }
        Ok(Self {
            version: IR_VERSION,
            format: SourceFormat::Safetensors,
            architecture,
            tensors: out,
        })
    }

    /// Every tensor's bytes match its shape and storage, stay inside its entry, and every
    /// quantization reference resolves to a dense tensor of the expected shape.
    pub fn validate(&self, entries: &[Entry]) -> Result<()> {
        ensure!(self.version == IR_VERSION, "unsupported model IR version");
        ensure!(!self.architecture.is_empty(), "model IR has no architecture");
        ensure!(!self.tensors.is_empty(), "model IR has no tensors");
        for (name, tensor) in &self.tensors {
            let entry = entries
                .iter()
                .find(|entry| entry.name == tensor.entry)
                .with_context(|| format!("{name}: unknown payload entry {}", tensor.entry))?;
            ensure!(
                tensor.offset.checked_add(tensor.bytes).is_some_and(|end| end <= entry.bytes),
                "{name}: tensor escapes its payload entry"
            );
            ensure!(
                !tensor.shape.is_empty() && tensor.shape.iter().all(|&n| n > 0),
                "{name}: empty or zero-sized shape"
            );
            let last = *tensor.shape.last().unwrap();
            let outer = product(&tensor.shape[..tensor.shape.len() - 1])
                .with_context(|| format!("{name}: shape overflow"))?;
            match &tensor.storage {
                Storage::Dense { dtype } => {
                    let size = dense_size(dtype).with_context(|| format!("{name}: unknown dtype {dtype}"))?;
                    let expected = outer.checked_mul(last).and_then(|n| n.checked_mul(size));
                    ensure!(expected == Some(tensor.bytes), "{name}: {dtype} bytes do not match its shape");
                }
                Storage::MlxAffine { bits, group_size, axis, scales, biases } => {
                    ensure!(matches!(bits, 2 | 3 | 4 | 5 | 6 | 8), "{name}: unsupported affine bit width {bits}");
                    ensure!(*axis == -1, "{name}: affine grouping on axis {axis} is not supported");
                    ensure!(*group_size > 0 && last % group_size == 0, "{name}: last axis does not divide group size");
                    let row_bits = last.checked_mul(*bits).context("affine size overflow")?;
                    ensure!(row_bits % 32 == 0, "{name}: affine rows do not fill packed words");
                    let expected = outer.checked_mul(row_bits / 8);
                    ensure!(expected == Some(tensor.bytes), "{name}: packed bytes do not match its shape");
                    let mut group_shape = tensor.shape.clone();
                    *group_shape.last_mut().unwrap() = last / group_size;
                    for reference in std::iter::once(scales).chain(biases) {
                        let side = self
                            .tensors
                            .get(reference)
                            .with_context(|| format!("{name}: missing quantization tensor {reference}"))?;
                        ensure!(
                            matches!(side.storage, Storage::Dense { .. }) && side.shape == group_shape,
                            "{name}: {reference} must be dense with one value per group"
                        );
                    }
                }
                Storage::GgmlBlock { type_name, block_size, type_size } => {
                    ensure!(*block_size > 0 && *type_size > 0, "{name}: invalid {type_name} block layout");
                    ensure!(last % block_size == 0, "{name}: last axis does not divide the {type_name} block");
                    let expected = outer
                        .checked_mul(last / block_size)
                        .and_then(|blocks| blocks.checked_mul(*type_size));
                    ensure!(expected == Some(tensor.bytes), "{name}: {type_name} bytes do not match its shape");
                }
            }
        }
        Ok(())
    }

    /// Distinct storages, for capability checks and reports.
    pub fn storages(&self) -> BTreeSet<String> {
        self.tensors.values().map(|tensor| tensor.storage.label()).collect()
    }

    /// Sum of payload bytes the tensors reference (the resident weight size when nothing
    /// is expanded).
    pub fn weight_bytes(&self) -> u64 {
        self.tensors.values().map(|tensor| tensor.bytes).sum()
    }
}

/// Reject an IR whose validation fails before any engine sees it.
pub fn checked(ir: ModelIr, entries: &[Entry]) -> Result<ModelIr> {
    if let Err(error) = ir.validate(entries) {
        bail!("invalid model IR: {error}");
    }
    Ok(ir)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, bytes: u64) -> Entry {
        Entry { name: name.into(), offset: 0, bytes, sha256: "0".repeat(64) }
    }

    #[test]
    fn gguf_dimensions_are_reversed_and_padding_is_dropped() {
        assert_eq!(gguf_shape([4096, 11008, 1, 1]), vec![11008, 4096]);
        assert_eq!(gguf_shape([4096, 1, 1, 1]), vec![4096]);
        assert_eq!(gguf_shape([128, 8, 32, 1]), vec![32, 8, 128]);
        assert_eq!(gguf_shape([1, 1, 1, 1]), vec![1]);
    }

    #[test]
    fn gguf_blocks_and_dense_tensors_validate_against_their_bytes() {
        let tensors = vec![
            GgufTensor { name: "blk.0.ffn_down.weight".into(), storage_type: "q6_K".into(), offset: 0, bytes: 2 * 1 * 210 },
            GgufTensor { name: "output_norm.weight".into(), storage_type: "f32".into(), offset: 1024, bytes: 256 * 4 },
        ];
        let layouts = vec![
            GgufLayout { ne: [256, 2, 1, 1], block_size: 256, type_size: 210 },
            GgufLayout { ne: [256, 1, 1, 1], block_size: 1, type_size: 4 },
        ];
        let ir = ModelIr::from_gguf("qwen3", "weights.gguf", &tensors, &layouts).unwrap();
        ir.validate(&[entry("weights.gguf", 4096)]).unwrap();
        assert_eq!(ir.tensors["blk.0.ffn_down.weight"].shape, vec![2, 256]);
        assert_eq!(ir.storages(), BTreeSet::from(["dense:F32".to_owned(), "ggml:q6_K".to_owned()]));
        // A size that disagrees with the shape, or bytes past the entry, are refused.
        let mut wrong = ir.clone();
        wrong.tensors.get_mut("blk.0.ffn_down.weight").unwrap().bytes += 1;
        assert!(wrong.validate(&[entry("weights.gguf", 4096)]).unwrap_err().to_string().contains("q6_K bytes"));
        assert!(ir.validate(&[entry("weights.gguf", 1200)]).unwrap_err().to_string().contains("escapes"));
    }

    #[test]
    fn mlx_affine_weights_use_logical_shapes_and_resolve_scales() {
        let mut ir = ModelIr {
            version: IR_VERSION,
            format: SourceFormat::Safetensors,
            architecture: "qwen3".into(),
            tensors: BTreeMap::new(),
        };
        // 8 rows × 64 logical columns at 4 bits = 8 × 8 packed U32 = 256 bytes.
        ir.tensors.insert("w.weight".into(), Tensor {
            entry: "model.safetensors".into(), offset: 0, bytes: 256, shape: vec![8, 64],
            storage: Storage::MlxAffine { bits: 4, group_size: 32, axis: -1, scales: "w.scales".into(), biases: Some("w.biases".into()) },
        });
        for (name, offset) in [("w.scales", 256), ("w.biases", 288)] {
            ir.tensors.insert(name.into(), Tensor {
                entry: "model.safetensors".into(), offset, bytes: 8 * 2 * 2, shape: vec![8, 2],
                storage: Storage::Dense { dtype: "BF16".into() },
            });
        }
        ir.validate(&[entry("model.safetensors", 512)]).unwrap();
        let mut missing = ir.clone();
        missing.tensors.remove("w.biases");
        assert!(missing.validate(&[entry("model.safetensors", 512)]).unwrap_err().to_string().contains("missing quantization tensor"));
        let mut grouped = ir.clone();
        grouped.tensors.get_mut("w.scales").unwrap().shape = vec![8, 4];
        assert!(grouped.validate(&[entry("model.safetensors", 512)]).is_err());
    }
}
