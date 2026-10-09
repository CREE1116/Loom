//! LMX v1: aligned, unchanged payloads + bounded JSON index + checksummed footer.
//! The first GGUF payload starts at zero, so CPU/CUDA can mmap this file directly.
//! MLX reads bounded safetensors slices; only small tokenizer assets need a cache.
use super::{
    import, local,
    model_package::{METADATA_LIMIT, ModelManifest},
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    ffi::{CStr, CString},
    fs,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

const MAGIC: &[u8; 8] = b"LOOMLMX\0";
const FOOTER: u64 = 64;
const ALIGN: u64 = 4096;
const BUFFER: usize = 1 << 20;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub offset: u64,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct GgufTensor {
    pub name: String,
    pub storage_type: String,
    pub offset: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "encoding", rename_all = "snake_case")]
pub enum Payload {
    Gguf {
        architecture: String,
        tensors: Vec<GgufTensor>,
    },
    MlxAffine {
        model: ModelManifest,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Manifest {
    pub version: u32,
    pub profile: String,
    pub source: String,
    pub entries: Vec<Entry>,
    pub payload: Payload,
}

#[derive(Clone, Debug)]
pub struct LmxFile {
    pub path: PathBuf,
    /// SHA-256 of the index (which includes all payload hashes and the plan version).
    pub identity: String,
    pub manifest: Manifest,
    pub bytes: u64,
    /// Derived on open from the payload headers; never stored in the file.
    pub ir: super::lmx_ir::ModelIr,
}

fn ordinary_file(path: &Path) -> Result<fs::File> {
    let parent = fs::canonicalize(path.parent().unwrap_or(Path::new(".")))?;
    local::CheckpointLocation::checked(&parent.join(path.file_name().context("missing filename")?))?
        .open(false, false)
}

fn check(progress: &mut dyn FnMut(u64, u64) -> bool, done: u64, total: u64) -> Result<()> {
    ensure!(progress(done, total), "LMX preparation cancelled");
    Ok(())
}

fn aligned(value: u64) -> Result<u64> {
    Ok(value
        .checked_add(ALIGN - 1)
        .context("LMX offset overflow")?
        / ALIGN
        * ALIGN)
}
fn valid_hash(hash: &str) -> bool {
    hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit())
}

impl LmxFile {
    pub fn open(path: &Path) -> Result<Self> {
        let mut file = ordinary_file(path)?;
        let bytes = file.metadata()?.len();
        ensure!(bytes >= FOOTER, "truncated LMX footer");
        file.seek(SeekFrom::End(-(FOOTER as i64)))?;
        let mut footer = [0; FOOTER as usize];
        file.read_exact(&mut footer)?;
        ensure!(&footer[..8] == MAGIC, "not an LMX executable");
        ensure!(
            u32::from_le_bytes(footer[8..12].try_into()?) == 1 && footer[12..16] == [0; 4],
            "unsupported LMX version/flags"
        );
        let start = u64::from_le_bytes(footer[16..24].try_into()?);
        let length = u64::from_le_bytes(footer[24..32].try_into()?);
        ensure!(
            start % ALIGN == 0
                && length > 0
                && length <= METADATA_LIMIT
                && start.checked_add(length) == Some(bytes - FOOTER),
            "invalid or truncated LMX index range"
        );
        file.seek(SeekFrom::Start(start))?;
        let mut index = vec![0; length as usize];
        file.read_exact(&mut index)?;
        let hash = Sha256::digest(&index);
        ensure!(
            hash.as_slice() == &footer[32..64],
            "LMX index checksum mismatch"
        );
        let manifest: Manifest = serde_json::from_slice(&index)?;
        ensure!(
            manifest.version == 1 && manifest.profile == "preserve",
            "unsupported LMX execution profile"
        );
        ensure!(
            !manifest.entries.is_empty() && manifest.entries.len() <= 1024,
            "invalid LMX entry count"
        );
        let mut names = BTreeSet::new();
        let mut cursor = 0;
        for entry in &manifest.entries {
            local::validate_relative_file(&entry.name)?;
            ensure!(
                !entry.name.contains('/') && names.insert(&entry.name),
                "duplicate or nested LMX entry"
            );
            ensure!(
                valid_hash(&entry.sha256)
                    && entry.offset == aligned(cursor)?
                    && entry
                        .offset
                        .checked_add(entry.bytes)
                        .is_some_and(|end| end <= start),
                "invalid LMX payload range/hash"
            );
            cursor = entry.offset + entry.bytes;
        }
        ensure!(aligned(cursor)? == start, "unindexed LMX payload region");
        let mut gguf_layouts = Vec::new();
        match &manifest.payload {
            Payload::Gguf {
                architecture,
                tensors,
            } => {
                ensure!(
                    manifest.entries.len() == 1
                        && manifest.entries[0].offset == 0
                        && !architecture.is_empty(),
                    "invalid GGUF executable layout"
                );
                let size = manifest.entries[0].bytes;
                ensure!(
                    !tensors.is_empty() && tensors.len() <= 50_000,
                    "invalid GGUF tensor count"
                );
                let mut names = BTreeSet::new();
                for t in tensors {
                    ensure!(
                        names.insert(&t.name)
                            && t.offset.checked_add(t.bytes).is_some_and(|end| end <= size),
                        "GGUF tensor escapes its payload"
                    );
                }
                let (actual_arch, actual_tensors, layouts) = inspect_gguf(path, size)?;
                ensure!(
                    actual_arch == *architecture && actual_tensors == *tensors,
                    "GGUF index disagrees with its payload header"
                );
                gguf_layouts = layouts;
            }
            Payload::MlxAffine { model } => {
                super::model_package::validate_mlx_config(&model.config)?;
                ensure!(
                    model.version == 1
                        && model.profile == "preserve"
                        && model.files.len() == manifest.entries.len(),
                    "invalid MLX executable manifest"
                );
                for f in &model.files {
                    ensure!(
                        manifest.entries.iter().any(|e| e.name == f.path
                            && e.bytes == f.bytes
                            && e.sha256 == f.sha256),
                        "MLX payload disagrees with source manifest"
                    );
                }
                for tensor in model.tensors.values() {
                    let e = manifest
                        .entries
                        .iter()
                        .find(|e| e.name == tensor.file)
                        .context("missing tensor payload")?;
                    ensure!(
                        tensor
                            .offset
                            .checked_add(tensor.bytes)
                            .is_some_and(|end| end <= e.bytes),
                        "tensor escapes its safetensors slice"
                    );
                }
                let mut tensors = std::collections::BTreeMap::new();
                for entry in manifest
                    .entries
                    .iter()
                    .filter(|entry| entry.name.ends_with(".safetensors"))
                {
                    super::model_package::read_tensors_slice(
                        path,
                        &entry.name,
                        entry.offset,
                        entry.bytes,
                        &mut tensors,
                    )?;
                }
                super::model_package::validate_tensors(&model.config, &mut tensors)?;
                ensure!(
                    tensors == model.tensors,
                    "MLX quantization descriptor disagrees with tensor layout"
                );
            }
        }
        let ir = match &manifest.payload {
            Payload::Gguf { architecture, tensors } => super::lmx_ir::ModelIr::from_gguf(
                architecture,
                &manifest.entries[0].name,
                tensors,
                &gguf_layouts,
            )?,
            Payload::MlxAffine { model } => super::lmx_ir::ModelIr::from_mlx(model)?,
        };
        let ir = super::lmx_ir::checked(ir, &manifest.entries)?;
        Ok(Self {
            path: fs::canonicalize(path)?,
            identity: format!("{hash:x}"),
            manifest,
            bytes,
            ir,
        })
    }

    pub fn id(&self) -> String {
        format!("lmx/{}", self.identity)
    }
    /// Readable name: the imported checkpoint's file or directory name.
    pub fn name(&self) -> String {
        let source = Path::new(&self.manifest.source);
        source
            .file_stem()
            .or_else(|| source.file_name())
            .map(|name| name.to_string_lossy().into_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| self.identity.chars().take(12).collect())
    }

    pub fn display_name(&self) -> String {
        let source = &self.manifest.source;
        if source.is_empty() {
            return format!("lmx-{}", &self.identity[..8.min(self.identity.len())]);
        }
        let models_dir = local::models_dir();
        let path = Path::new(source);
        if let Ok(rel) = path.strip_prefix(&models_dir) {
            return rel.to_string_lossy().replace('\\', "/");
        }
        if let Some(pos) = source.find("/models/") {
            return source[pos + "/models/".len()..].to_string();
        }
        if let Some(pos) = source.find("\\models\\") {
            return source[pos + "\\models\\".len()..].replace('\\', "/");
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if let Some(parent) = path.parent().and_then(|p| p.file_name()).and_then(|n| n.to_str()) {
                if !parent.is_empty() && parent != "." && parent != "/" && parent != "models" {
                    return format!("{parent}/{name}");
                }
            }
            return name.to_string();
        }
        source.clone()
    }
    pub fn verify(&self, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<()> {
        let mut file = ordinary_file(&self.path)?;
        let mut buffer = vec![0; BUFFER];
        for e in &self.manifest.entries {
            file.seek(SeekFrom::Start(e.offset))?;
            let mut remaining = e.bytes;
            let mut hash = Sha256::new();
            while remaining > 0 {
                check(progress, e.bytes - remaining, e.bytes)?;
                let count = remaining.min(BUFFER as u64) as usize;
                file.read_exact(&mut buffer[..count])?;
                hash.update(&buffer[..count]);
                remaining -= count as u64;
            }
            ensure!(
                format!("{:x}", hash.finalize()) == e.sha256,
                "LMX payload checksum mismatch: {}",
                e.name
            );
        }
        check(progress, 0, 0)
    }

    /// Only metadata is materialized. The weight shards remain in this LMX file.
    pub(crate) fn metadata_dir(&self) -> Result<PathBuf> {
        let root = local::models_dir();
        let id = format!(".lmx-cache/{}/metadata", self.identity);
        let marker = local::managed_file_path(&root, &format!("{id}/marker"), true)?;
        let directory = marker.parent().unwrap();
        let mut input = ordinary_file(&self.path)?;
        for entry in self
            .manifest
            .entries
            .iter()
            .filter(|entry| !entry.name.ends_with(".safetensors"))
        {
            ensure!(entry.bytes <= METADATA_LIMIT, "LMX metadata exceeds limit");
            let destination =
                local::managed_file_path(&root, &format!("{id}/{}", entry.name), true)?;
            input.seek(SeekFrom::Start(entry.offset))?;
            let mut data = vec![0; entry.bytes as usize];
            input.read_exact(&mut data)?;
            ensure!(
                format!("{:x}", Sha256::digest(&data)) == entry.sha256,
                "LMX metadata checksum mismatch"
            );
            if destination.exists() {
                let mut existing = ordinary_file(&destination)?;
                let mut saved = Vec::new();
                std::io::Read::by_ref(&mut existing)
                    .take(METADATA_LIMIT + 1)
                    .read_to_end(&mut saved)?;
                ensure!(saved == data, "LMX metadata cache was modified");
            } else {
                let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
                temporary.write_all(&data)?;
                temporary.as_file().sync_all()?;
                match temporary.persist_noclobber(&destination) {
                    Ok(_) => {}
                    Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                        ensure!(
                            fs::read(&destination)? == data,
                            "concurrent LMX metadata cache mismatch"
                        );
                    }
                    Err(error) => return Err(error.error.into()),
                }
            }
        }
        Ok(directory.to_owned())
    }
}

/// The v1 index tensors plus the shape/block facts the IR needs, from one header read.
fn inspect_gguf(
    path: &Path,
    payload_bytes: u64,
) -> Result<(String, Vec<GgufTensor>, Vec<super::lmx_ir::GgufLayout>)> {
    use llama_cpp_sys_2 as sys;
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
    // SAFETY: gguf validates the header and owns these strings until Guard drops.
    unsafe {
        let key = CString::new("general.architecture")?;
        let index = sys::gguf_find_key(guard.0, key.as_ptr());
        ensure!(
            index >= 0 && sys::gguf_get_kv_type(guard.0, index) == sys::GGUF_TYPE_STRING,
            "missing GGUF architecture"
        );
        let arch = CStr::from_ptr(sys::gguf_get_val_str(guard.0, index))
            .to_str()?
            .to_owned();
        let template_key = CString::new("tokenizer.chat_template")?;
        let template_index = sys::gguf_find_key(guard.0, template_key.as_ptr());
        ensure!(
            template_index < 0
                || sys::gguf_get_kv_type(guard.0, template_index) == sys::GGUF_TYPE_STRING,
            "GGUF chat template must be a string"
        );
        ensure!(
            arch == "spark2_5" || template_index >= 0,
            "GGUF has no supported chat template"
        );
        let split = CString::new("split.count")?;
        ensure!(
            sys::gguf_find_key(guard.0, split.as_ptr()) < 0,
            "split GGUF requires a merge before LMX import"
        );
        let count = sys::gguf_get_n_tensors(guard.0);
        ensure!((1..=50_000).contains(&count), "invalid GGUF tensor count");
        let base = sys::gguf_get_data_offset(guard.0) as u64;
        ensure!(base <= METADATA_LIMIT, "GGUF header exceeds import limit");
        let mut tensors = Vec::new();
        let mut layouts = Vec::new();
        for index in 0..count {
            let offset = base
                .checked_add(sys::gguf_get_tensor_offset(guard.0, index) as u64)
                .context("GGUF offset overflow")?;
            let bytes = sys::gguf_get_tensor_size(guard.0, index) as u64;
            ensure!(
                offset
                    .checked_add(bytes)
                    .is_some_and(|end| end <= payload_bytes),
                "truncated GGUF tensor data"
            );
            let name = CStr::from_ptr(sys::gguf_get_tensor_name(guard.0, index))
                .to_str()?
                .to_owned();
            let kind = sys::gguf_get_tensor_type(guard.0, index);
            let storage_type = CStr::from_ptr(sys::ggml_type_name(kind)).to_str()?.to_owned();
            // gguf_get_tensor_ne points at GGML_MAX_DIMS (4) counts owned by the context.
            let ne = std::slice::from_raw_parts(sys::gguf_get_tensor_ne(guard.0, index), 4);
            ensure!(ne.iter().all(|&n| n > 0), "invalid GGUF tensor dimensions");
            layouts.push(super::lmx_ir::GgufLayout {
                ne: [ne[0] as u64, ne[1] as u64, ne[2] as u64, ne[3] as u64],
                block_size: u64::try_from(sys::ggml_blck_size(kind)).context("invalid ggml block size")?,
                type_size: sys::ggml_type_size(kind) as u64,
            });
            tensors.push(GgufTensor {
                name,
                storage_type,
                offset,
                bytes,
            });
        }
        Ok((arch, tensors, layouts))
    }
}

#[derive(Debug, Serialize)]
pub struct Prepared {
    pub id: String,
    pub path: PathBuf,
    pub bytes: u64,
    pub reused: bool,
    pub engines: Vec<super::lmx_runtime::EngineStatus>,
}

pub fn prepare(source: &Path, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<Prepared> {
    prepare_into(&local::models_dir(), source, progress)
}

fn prepare_into(
    root: &Path,
    source: &Path,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Prepared> {
    check(progress, 0, 0)?;
    let _reservation = import::reserve(progress)?;
    let source = fs::canonicalize(source)?;
    if source.is_file()
        && source
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("lmx"))
    {
        let executable = LmxFile::open(&source)?;
        executable.verify(progress)?;
        return publish_existing(root, &executable, progress);
    }
    let (payload, sources): (Payload, Vec<(String, PathBuf, Option<String>)>) = if source.is_dir() {
        let model = import::inspect_package(&source, &source.to_string_lossy(), None, progress)?;
        let sources = model
            .files
            .iter()
            .map(|f| (f.path.clone(), source.join(&f.path), Some(f.sha256.clone())))
            .collect();
        (Payload::MlxAffine { model }, sources)
    } else {
        ensure!(
            source
                .extension()
                .is_some_and(|e| e.eq_ignore_ascii_case("gguf")),
            "expected a GGUF file, MLX directory or LMX file"
        );
        let bytes = ordinary_file(&source)?.metadata()?.len();
        let (architecture, tensors, _) = inspect_gguf(&source, bytes)?;
        ensure!(
            super::native_runtime::ModelInfo::inspect(&source, &source).is_some(),
            "GGUF has no supported chat template"
        );
        (
            Payload::Gguf {
                architecture,
                tensors,
            },
            vec![("weights.gguf".into(), source.clone(), None)],
        )
    };
    let marker = local::managed_file_path(root, ".imports/lmx/marker", true)?;
    let mut stage = tempfile::NamedTempFile::new_in(marker.parent().unwrap())?;
    let mut sources = sources;
    sources.sort_by_key(|(name, _, _)| (!name.ends_with(".safetensors"), name.clone()));
    let mut entries = Vec::new();
    let mut buffer = vec![0; BUFFER];
    for (name, path, expected) in sources {
        let offset = aligned(stage.stream_position()?)?;
        stage.as_file_mut().set_len(offset)?;
        stage.seek(SeekFrom::Start(offset))?;
        let mut input = ordinary_file(&path)?;
        let bytes = input.metadata()?.len();
        let mut done = 0;
        let mut hash = Sha256::new();
        loop {
            check(progress, done, bytes)?;
            let n = input.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            stage.write_all(&buffer[..n])?;
            hash.update(&buffer[..n]);
            done += n as u64;
        }
        ensure!(done == bytes, "source changed during LMX import");
        let sha256 = format!("{:x}", hash.finalize());
        ensure!(
            expected.is_none_or(|expected| expected == sha256),
            "source hash changed during LMX import"
        );
        entries.push(Entry {
            name,
            offset,
            bytes,
            sha256,
        });
    }
    let manifest = Manifest {
        version: 1,
        profile: "preserve".into(),
        source: source.to_string_lossy().into(),
        entries,
        payload,
    };
    write_index(stage.as_file_mut(), &manifest)?;
    stage.as_file().sync_all()?;
    check(progress, 0, 0)?;
    let executable = LmxFile::open(stage.path())?;
    publish_stage(root, stage, &executable, progress)
}

fn write_index(file: &mut fs::File, manifest: &Manifest) -> Result<()> {
    let offset = aligned(file.stream_position()?)?;
    file.set_len(offset)?;
    file.seek(SeekFrom::Start(offset))?;
    let index = serde_json::to_vec(manifest)?;
    ensure!(index.len() as u64 <= METADATA_LIMIT, "LMX index too large");
    let mut footer = [0; FOOTER as usize];
    footer[..8].copy_from_slice(MAGIC);
    footer[8..12].copy_from_slice(&1u32.to_le_bytes());
    footer[16..24].copy_from_slice(&offset.to_le_bytes());
    footer[24..32].copy_from_slice(&(index.len() as u64).to_le_bytes());
    footer[32..].copy_from_slice(&Sha256::digest(&index));
    file.write_all(&index)?;
    file.write_all(&footer)?;
    Ok(())
}

fn publish_existing(
    root: &Path,
    executable: &LmxFile,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Prepared> {
    let marker = local::managed_file_path(root, ".imports/lmx/marker", true)?;
    let mut stage = tempfile::NamedTempFile::new_in(marker.parent().unwrap())?;
    let mut input = ordinary_file(&executable.path)?;
    let mut buffer = vec![0; BUFFER];
    let mut done = 0;
    loop {
        check(progress, done, executable.bytes)?;
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        };
        stage.write_all(&buffer[..n])?;
        done += n as u64;
    }
    ensure!(done == executable.bytes, "LMX source changed during copy");
    stage.as_file().sync_all()?;
    let copied = LmxFile::open(stage.path())?;
    ensure!(
        copied.identity == executable.identity,
        "LMX source index changed during copy"
    );
    copied.verify(progress)?;
    publish_stage(root, stage, &copied, progress)
}

fn publish_stage(
    root: &Path,
    stage: tempfile::NamedTempFile,
    executable: &LmxFile,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Prepared> {
    let destination =
        local::managed_file_path(root, &format!("lmx/{}.lmx", executable.identity), true)?;
    check(progress, 0, 0)?;
    let reused = match stage.persist_noclobber(&destination) {
        Ok(_) => false,
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            let cached = LmxFile::open(&destination)?;
            ensure!(
                cached.identity == executable.identity,
                "LMX cache identity mismatch"
            );
            cached.verify(progress)?;
            true
        }
        Err(error) => return Err(error.error.into()),
    };
    Ok(Prepared {
        id: executable.id(),
        path: destination,
        bytes: executable.bytes,
        reused,
        engines: super::lmx_runtime::statuses(executable),
    })
}

pub fn installed() -> Result<Vec<LmxFile>> {
    let root = local::models_dir();
    let directory = root.join("lmx");
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() || !entry.path().extension().is_some_and(|e| e == "lmx") {
            continue;
        }
        let name = entry
            .file_name()
            .to_str()
            .context("invalid LMX filename")?
            .to_owned();
        let path = local::managed_file_path(&root, &format!("lmx/{name}"), false)?;
        let executable = LmxFile::open(&path)?;
        ensure!(
            name == format!("{}.lmx", executable.identity),
            "LMX filename identity mismatch"
        );
        out.push(executable);
    }
    out.sort_by_key(|e| e.id());
    Ok(out)
}

/// Readable name of an installed LMX model (`lmx/<identity>`), for model lists.
pub fn display_name(id: &str) -> Option<String> {
    if let Some(native) = super::lmx_native_runtime::by_name(id) {
        return Some(native.name());
    }
    by_id(id).ok().map(|executable| executable.name())
}

pub fn by_id(id: &str) -> Result<LmxFile> {
    if let Ok(installed) = installed() {
        if let Some(found) = installed.iter().find(|m| {
            m.id() == id
                || m.identity == id
                || m.display_name() == id
                || m.path.to_string_lossy() == id
                || id.strip_prefix("lmx/").is_some_and(|rest| m.identity.starts_with(rest) || m.display_name() == rest)
                || m.identity.starts_with(id)
                || m.display_name().contains(id)
        }) {
            return Ok(found.clone());
        }
    }
    let identity = id.strip_prefix("lmx/").unwrap_or(id);
    ensure!(valid_hash(identity), "invalid LMX identity: {id}");
    let path =
        local::managed_file_path(&local::models_dir(), &format!("lmx/{identity}.lmx"), false)?;
    let executable = LmxFile::open(&path)?;
    ensure!(executable.identity == identity, "LMX identity mismatch");
    Ok(executable)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn gguf_fixture(path: &Path) {
        fn string(out: &mut Vec<u8>, value: &str) {
            out.extend((value.len() as u64).to_le_bytes());
            out.extend(value.as_bytes());
        }
        let mut data = b"GGUF".to_vec();
        data.extend(3u32.to_le_bytes());
        data.extend(1u64.to_le_bytes());
        data.extend(2u64.to_le_bytes());
        for (key, value) in [
            ("general.architecture", "qwen2"),
            ("tokenizer.chat_template", "{{ messages }}"),
        ] {
            string(&mut data, key);
            data.extend(8u32.to_le_bytes());
            string(&mut data, value);
        }
        string(&mut data, "dummy");
        data.extend(1u32.to_le_bytes());
        data.extend(2u64.to_le_bytes());
        data.extend(0u32.to_le_bytes());
        data.extend(0u64.to_le_bytes());
        while data.len() % 32 != 0 {
            data.push(0)
        }
        data.extend([0; 8]);
        fs::write(path, data).unwrap();
    }
    #[test]
    fn gguf_lmx_preserves_prefix_can_be_read_by_native_and_reuses_verified_identity() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("weights.gguf");
        gguf_fixture(&source);
        let root = dir.path().join("models");
        let prepared = prepare_into(&root, &source, &mut |_, _| true).unwrap();
        let executable = LmxFile::open(&prepared.path).unwrap();
        executable.verify(&mut |_, _| true).unwrap();
        let original = fs::read(&source).unwrap();
        assert_eq!(
            &fs::read(&prepared.path).unwrap()[..original.len()],
            original
        );
        assert!(
            super::super::native_runtime::ModelInfo::named(&prepared.path, prepared.id.clone())
                .is_some()
        );
        assert!(
            prepare_into(&root, &source, &mut |_, _| true)
                .unwrap()
                .reused
        );
        assert_eq!(fs::read_dir(root.join(".imports/lmx")).unwrap().count(), 0);
        assert!(
            prepared
                .engines
                .iter()
                .find(|s| s.engine == super::super::lmx_runtime::Engine::Cpu)
                .unwrap()
                .payload_supported
        );
        let copied = prepare_into(
            &dir.path().join("another-root"),
            &prepared.path,
            &mut |_, _| true,
        )
        .unwrap();
        assert_eq!(prepared.id, copied.id);
    }
    #[test]
    fn mlx_lmx_preserves_all_shards_and_tensor_offsets() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        super::super::model_package::tests::fixture(&source);
        let prepared = prepare_into(&dir.path().join("models"), &source, &mut |_, _| true).unwrap();
        let executable = LmxFile::open(&prepared.path).unwrap();
        executable.verify(&mut |_, _| true).unwrap();
        let bytes = fs::read(&prepared.path).unwrap();
        for e in &executable.manifest.entries {
            assert_eq!(
                &bytes[e.offset as usize..(e.offset + e.bytes) as usize],
                fs::read(source.join(&e.name)).unwrap()
            );
            assert_eq!(e.offset % ALIGN, 0);
        }
        assert!(matches!(
            executable.manifest.payload,
            Payload::MlxAffine { .. }
        ));
        assert!(
            !prepared
                .engines
                .iter()
                .find(|s| s.engine == super::super::lmx_runtime::Engine::Cpu)
                .unwrap()
                .payload_supported
        );
    }
    #[test]
    fn corrupt_footer_index_payload_and_ranges_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("weights.gguf");
        gguf_fixture(&source);
        let prepared = prepare_into(&dir.path().join("models"), &source, &mut |_, _| true).unwrap();
        let original = fs::read(&prepared.path).unwrap();
        let bad = dir.path().join("bad.lmx");
        for index in [original.len() - 64, original.len() - 60, original.len() - 1] {
            let mut bytes = original.clone();
            bytes[index] ^= 1;
            fs::write(&bad, bytes).unwrap();
            assert!(LmxFile::open(&bad).is_err());
        }
        fs::write(&bad, &original[..original.len() - 1]).unwrap();
        assert!(LmxFile::open(&bad).is_err());
        let good = LmxFile::open(&prepared.path).unwrap();
        let e = &good.manifest.entries[0];
        let mut bytes = original.clone();
        bytes[(e.bytes - 1) as usize] ^= 1;
        fs::write(&bad, bytes).unwrap();
        assert!(
            LmxFile::open(&bad)
                .unwrap()
                .verify(&mut |_, _| true)
                .is_err()
        );
        let mut manifest = good.manifest.clone();
        manifest.entries[0].offset = u64::MAX;
        let mut file = fs::File::create(&bad).unwrap();
        file.write_all(&original[..e.bytes as usize]).unwrap();
        write_index(&mut file, &manifest).unwrap();
        drop(file);
        assert!(LmxFile::open(&bad).is_err());
    }
    #[test]
    fn cancel_and_truncated_gguf_never_publish_or_change_source() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("weights.gguf");
        gguf_fixture(&source);
        let original = fs::read(&source).unwrap();
        let root = dir.path().join("models");
        assert!(prepare_into(&root, &source, &mut |done, _| done == 0).is_err());
        assert!(!root.join("lmx").exists());
        assert_eq!(fs::read(&source).unwrap(), original);
        assert_eq!(fs::read_dir(root.join(".imports/lmx")).unwrap().count(), 0);
        fs::write(&source, &original[..original.len() - 1]).unwrap();
        assert!(
            prepare_into(&root, &source, &mut |_, _| true)
                .unwrap_err()
                .to_string()
                .contains("truncated")
        );
    }
    #[cfg(unix)]
    #[test]
    fn publication_rejects_symlink_destinations() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("weights.gguf");
        gguf_fixture(&source);
        let root = dir.path().join("models");
        let prepared = prepare_into(&root, &source, &mut |_, _| true).unwrap();
        fs::remove_file(&prepared.path).unwrap();
        let outside = dir.path().join("outside");
        fs::write(&outside, "keep").unwrap();
        symlink(&outside, &prepared.path).unwrap();
        assert!(prepare_into(&root, &source, &mut |_, _| true).is_err());
        assert_eq!(fs::read_to_string(outside).unwrap(), "keep");
    }
}
