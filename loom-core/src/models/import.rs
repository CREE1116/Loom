//! Precision-preserving MLX package import. Originals stay untouched. Publication is
//! atomic; incomplete HF transfers live outside discovery and can be resumed.
use super::{local, model_package::*};
use crate::base::resources::{AdmissionError, ResourceKind, local_resources};
use anyhow::{Context, Result, ensure};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

const BUFFER_BYTES: usize = 1 << 20;
const IMPORT_RESERVATION: u64 = 256 << 20;

#[derive(Debug, Serialize)]
pub struct ImportedModel {
    /// Pass `mlx/<id>` to `loom model bench` or select it through normal discovery.
    pub id: String,
    pub path: PathBuf,
    pub identity: String,
    pub bytes: u64,
    pub reused: bool,
    pub backend_available: bool,
}

fn check(progress: &mut dyn FnMut(u64, u64) -> bool, done: u64, total: u64) -> Result<()> {
    ensure!(progress(done, total), "model import cancelled");
    Ok(())
}

pub(crate) fn reserve(
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<crate::base::resources::ResourceLease> {
    let started = Instant::now();
    Ok(
        local_resources().acquire(ResourceKind::Inference, IMPORT_RESERVATION, || {
            if !progress(0, 0) {
                Err(AdmissionError::Cancelled)
            } else if started.elapsed() > Duration::from_secs(30) {
                Err(AdmissionError::TimedOut)
            } else {
                Ok(())
            }
        })?,
    )
}

fn payload_name(name: &str) -> bool {
    !name.contains('/')
        && (name.ends_with(".safetensors")
            || matches!(
                name,
                "config.json"
                    | "tokenizer.json"
                    | "tokenizer_config.json"
                    | "generation_config.json"
                    | "chat_template.jinja"
                    | "model.safetensors.index.json"
                    | "special_tokens_map.json"
                    | "added_tokens.json"
                    | "LICENSE"
                    | "LICENSE.txt"
                    | "LICENSE.md"
                    | "README.md"
            ))
}

fn payload_names(directory: &Path) -> Result<Vec<String>> {
    let mut names = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if !payload_name(&name) {
            continue;
        }
        ensure!(
            entry.file_type()?.is_file(),
            "checkpoint payload must be a regular file: {name}"
        );
        ensure!(
            name.ends_with(".safetensors") || entry.metadata()?.len() <= METADATA_LIMIT,
            "metadata too large: {name}"
        );
        local::validate_relative_file(&name)?;
        names.push(name);
    }
    names.sort();
    ensure!(names.len() <= 1024, "too many checkpoint files");
    ensure!(
        names.iter().any(|name| name == "config.json")
            && names.iter().any(|name| name == "tokenizer.json"),
        "config.json and tokenizer.json are required"
    );
    ensure!(
        names.iter().any(|name| name.ends_with(".safetensors")),
        "no safetensors weights found"
    );
    Ok(names)
}

fn open_payload(path: &Path) -> Result<fs::File> {
    let parent = fs::canonicalize(path.parent().context("missing payload directory")?)?;
    local::CheckpointLocation::checked(
        &parent.join(path.file_name().context("missing payload filename")?),
    )?
    .open(false, false)
}

fn digest_file(path: &Path, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<PayloadFile> {
    let mut file = open_payload(path)?;
    let bytes = file.metadata()?.len();
    let mut hash = Sha256::new();
    let mut buffer = vec![0; BUFFER_BYTES];
    let mut done = 0;
    loop {
        check(progress, done, bytes)?;
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        hash.update(&buffer[..n]);
        done += n as u64;
    }
    ensure!(done == bytes, "payload changed during verification");
    Ok(PayloadFile {
        path: path
            .file_name()
            .unwrap()
            .to_str()
            .context("non-UTF8 payload")?
            .into(),
        bytes,
        sha256: format!("{:x}", hash.finalize()),
    })
}

fn copy_payload(
    source: &Path,
    dest: &Path,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<()> {
    let mut input = open_payload(source)?;
    let size = input.metadata()?.len();
    let mut output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)?;
    let mut buffer = vec![0; BUFFER_BYTES];
    let mut done = 0;
    loop {
        check(progress, done, size)?;
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        output.write_all(&buffer[..n])?;
        done += n as u64;
    }
    ensure!(done == size, "source changed during import");
    output.sync_all()?;
    Ok(())
}

pub(crate) fn inspect_package(
    directory: &Path,
    source: &str,
    revision: Option<String>,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<ModelManifest> {
    let names = payload_names(directory)?;
    let config = read_json(&directory.join("config.json"))?;
    validate_mlx_config(&config)?;
    {
        let tokenizer = read_json(&directory.join("tokenizer.json"))?;
        ensure!(
            tokenizer["model"].is_object(),
            "invalid tokenizer.json model"
        );
    }
    #[cfg(loom_mlx)]
    tokenizers::Tokenizer::from_file(directory.join("tokenizer.json"))
        .map_err(|error| anyhow::anyhow!("invalid tokenizer: {error}"))?;
    let template = if names.iter().any(|name| name == "tokenizer_config.json") {
        read_json(&directory.join("tokenizer_config.json"))?["chat_template"]
            .as_str()
            .map(str::to_owned)
    } else {
        None
    };
    let template = template
        .or_else(|| {
            let path = directory.join("chat_template.jinja");
            if fs::metadata(&path).ok()?.len() > METADATA_LIMIT {
                return None;
            }
            fs::read_to_string(path).ok()
        })
        .context("missing chat template (tokenizer_config.json or chat_template.jinja)")?;
    let mut environment = minijinja::Environment::new();
    environment
        .add_template("chat", &template)
        .context("invalid chat template")?;
    if names.iter().any(|name| name == "generation_config.json") {
        read_json(&directory.join("generation_config.json"))?;
    }
    let mut tensors = BTreeMap::new();
    let mut files = Vec::new();
    for name in &names {
        check(progress, 0, 0)?;
        let path = directory.join(name);
        if name.ends_with(".safetensors") {
            read_tensors(&path, name, &mut tensors)?;
        }
        files.push(digest_file(&path, progress)?);
    }
    if names
        .iter()
        .any(|name| name == "model.safetensors.index.json")
    {
        let index = read_json(&directory.join("model.safetensors.index.json"))?;
        let map = index["weight_map"]
            .as_object()
            .context("invalid shard weight_map")?;
        ensure!(
            map.len() == tensors.len(),
            "shard index does not cover all tensors"
        );
        for (name, file) in map {
            let file = file.as_str().context("invalid shard filename")?;
            ensure!(
                payload_name(file) && file.ends_with(".safetensors"),
                "unsafe shard filename"
            );
            ensure!(
                tensors.get(name).is_some_and(|tensor| tensor.file == file),
                "missing or misplaced shard tensor: {name}"
            );
        }
    } else {
        ensure!(
            names
                .iter()
                .filter(|name| name.ends_with(".safetensors"))
                .count()
                == 1
                && !names
                    .iter()
                    .any(|name| name.ends_with(".safetensors") && name.contains("-of-")),
            "sharded checkpoint requires model.safetensors.index.json"
        );
    }
    validate_tensors(&config, &mut tensors)?;
    let identity = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(
            PLAN_VERSION,
            source,
            &revision,
            &files
        ))?)
    );
    Ok(ModelManifest {
        version: 1,
        plan_version: PLAN_VERSION,
        profile: "preserve".into(),
        backend: "mlx".into(),
        source: source.into(),
        revision,
        identity,
        files,
        config,
        tensors,
        validation: "hashes+shards+tensor-layout+tokenizer+template; inference not certified"
            .into(),
    })
}

fn lock(path: &Path, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<fs::File> {
    let file = local::CheckpointLocation::checked(path)?.open(false, true)?;
    let started = Instant::now();
    loop {
        check(progress, 0, 0)?;
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => return Ok(file),
            Err(error)
                if (error.kind() == std::io::ErrorKind::WouldBlock
                    || error.raw_os_error() == fs2::lock_contended_error().raw_os_error())
                    && started.elapsed() < Duration::from_secs(30) =>
            {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn finish(
    root: &Path,
    stage: &Path,
    source: &str,
    revision: Option<String>,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<ImportedModel> {
    let manifest = inspect_package(stage, source, revision, progress)?;
    let id = format!("imported/{}", manifest.identity);
    let manifest_path = local::managed_file_path(root, &format!("mlx/{id}/{MANIFEST}"), true)?;
    let destination = manifest_path.parent().unwrap();
    // managed_file_path creates parents; the empty leaf is removed only under the
    // publication lock. A directory becomes discoverable only in one rename below.
    let lock_path = local::managed_file_path(root, ".imports/publish.lock", true)?;
    let _lock = lock(&lock_path, progress)?;
    let reused = if destination.join("config.json").exists() {
        let existing = inspect_package(destination, source, manifest.revision.clone(), progress)?;
        ensure!(
            existing == manifest,
            "prepared model was modified; refusing cache reuse"
        );
        ensure!(
            read_json(&manifest_path)? == serde_json::to_value(&manifest)?,
            "prepared manifest was modified"
        );
        true
    } else {
        let bytes = serde_json::to_vec_pretty(&manifest)?;
        ensure!(
            bytes.len() as u64 <= METADATA_LIMIT,
            "manifest exceeds metadata limit"
        );
        {
            // Closed before the rename: Windows refuses to move a directory holding an
            // open file.
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(stage.join(MANIFEST))?;
            file.write_all(&bytes)?;
            file.sync_all()?;
        }
        check(progress, 0, 0)?;
        fs::remove_dir(destination).context("publication destination is not empty")?;
        fs::rename(stage, destination)?;
        false
    };
    Ok(ImportedModel {
        id,
        path: destination.to_owned(),
        identity: manifest.identity,
        bytes: manifest.files.iter().map(|file| file.bytes).sum(),
        reused,
        backend_available: cfg!(loom_mlx),
    })
}

fn cached_snapshot(
    root: &Path,
    source: &str,
    revision: &str,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<Option<ImportedModel>> {
    let directory = root.join("mlx/imported");
    if !directory.exists() {
        return Ok(None);
    }
    for entry in fs::read_dir(directory)? {
        check(progress, 0, 0)?;
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        if !entry.path().join(MANIFEST).exists() {
            continue;
        }
        let name = entry
            .file_name()
            .to_str()
            .context("non-UTF8 prepared id")?
            .to_owned();
        let manifest =
            local::managed_file_path(root, &format!("mlx/imported/{name}/{MANIFEST}"), false)?;
        let path = manifest.parent().unwrap().to_owned();
        let saved = read_json(&manifest)?;
        if saved["source"] != source || saved["revision"] != revision {
            continue;
        }
        let expected: ModelManifest = serde_json::from_value(saved)?;
        let actual = inspect_package(&path, source, Some(revision.into()), progress)?;
        ensure!(
            expected == actual && entry.file_name().to_str() == Some(actual.identity.as_str()),
            "prepared model was modified; refusing cache reuse"
        );
        return Ok(Some(ImportedModel {
            id: format!("imported/{}", actual.identity),
            path,
            identity: actual.identity,
            bytes: actual.files.iter().map(|file| file.bytes).sum(),
            reused: true,
            backend_available: cfg!(loom_mlx),
        }));
    }
    Ok(None)
}

/// Cheap inventory for the product CLI. Full hashes are rechecked when an import is reused.
pub fn installed() -> Result<Vec<ImportedModel>> {
    let root = local::models_dir();
    let directory = root.join("mlx/imported");
    if !directory.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() || !entry.path().join(MANIFEST).exists() {
            continue;
        }
        let name = entry
            .file_name()
            .to_str()
            .context("non-UTF8 prepared id")?
            .to_owned();
        let path =
            local::managed_file_path(&root, &format!("mlx/imported/{name}/{MANIFEST}"), false)?;
        let manifest: ModelManifest = serde_json::from_value(read_json(&path)?)?;
        ensure!(
            manifest.version == 1 && manifest.identity == name,
            "invalid prepared manifest"
        );
        out.push(ImportedModel {
            id: format!("imported/{name}"),
            path: path.parent().unwrap().to_owned(),
            identity: manifest.identity,
            bytes: manifest.files.iter().map(|file| file.bytes).sum(),
            reused: false,
            backend_available: cfg!(loom_mlx),
        });
    }
    out.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(out)
}

/// Import a local, already MLX-compatible checkpoint with an independent byte-identical copy.
pub fn directory(
    source: &Path,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<ImportedModel> {
    directory_into(&local::models_dir(), source, progress)
}

fn directory_into(
    root: &Path,
    source: &Path,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<ImportedModel> {
    check(progress, 0, 0)?;
    let _reservation = reserve(progress)?;
    let source = fs::canonicalize(source)?;
    let names = payload_names(&source)?;
    validate_mlx_config(&read_json(&source.join("config.json"))?)?;
    let marker = local::managed_file_path(root, ".imports/local/marker", true)?;
    let stage = tempfile::Builder::new()
        .prefix("prepare-")
        .tempdir_in(marker.parent().unwrap())?;
    for name in names {
        copy_payload(&source.join(&name), &stage.path().join(&name), progress)?;
    }
    finish(
        root,
        stage.path(),
        &source.to_string_lossy(),
        None,
        progress,
    )
}

struct HubSnapshot {
    revision: String,
    files: BTreeMap<String, (u64, Option<String>)>,
}

fn parse_snapshot(value: &Value) -> Result<HubSnapshot> {
    let revision = value["sha"]
        .as_str()
        .context("HF response has no immutable revision")?;
    ensure!(
        revision.len() == 40 && revision.bytes().all(|b| b.is_ascii_hexdigit()),
        "invalid HF commit SHA"
    );
    let mut files: BTreeMap<String, (u64, Option<String>)> = BTreeMap::new();
    for file in value["siblings"]
        .as_array()
        .context("HF response has no file inventory")?
    {
        let name = file["rfilename"].as_str().context("invalid HF filename")?;
        if !payload_name(name) {
            continue;
        }
        local::validate_relative_file(name)?;
        let size = file["size"]
            .as_u64()
            .or_else(|| file["lfs"]["size"].as_u64())
            .context("HF inventory is missing file size")?;
        ensure!(
            name.ends_with(".safetensors") || size <= METADATA_LIMIT,
            "HF metadata exceeds import limit"
        );
        let hash = file["lfs"]["sha256"].as_str().map(str::to_owned);
        ensure!(
            !name.ends_with(".safetensors") || hash.is_some(),
            "weight payload is missing upstream sha256"
        );
        ensure!(
            files.insert(name.into(), (size, hash)).is_none(),
            "duplicate HF filename"
        );
    }
    ensure!(
        files.len() <= 1024
            && files.contains_key("config.json")
            && files.contains_key("tokenizer.json")
            && files.keys().any(|name| name.ends_with(".safetensors")),
        "HF repository has no complete MLX checkpoint inventory"
    );
    Ok(HubSnapshot {
        revision: revision.into(),
        files,
    })
}

/// `mlx:owner/repo[@revision]`; resolve the revision once, then fetch every file at that commit.
pub fn pull(spec: &str, progress: &mut dyn FnMut(u64, u64) -> bool) -> Result<ImportedModel> {
    pull_into(
        &local::models_dir(),
        spec,
        "https://huggingface.co",
        progress,
    )
}

fn pull_into(
    root: &Path,
    spec: &str,
    origin: &str,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> Result<ImportedModel> {
    let spec = spec
        .strip_prefix("mlx:")
        .context("expected mlx:owner/repo[@revision]")?;
    let (repo, requested) = spec.rsplit_once('@').unwrap_or((spec, "main"));
    local::validate_repo(repo)?;
    local::validate_relative_file(requested)?;
    ensure!(
        !requested.contains('/'),
        "use a branch, tag or commit SHA without slashes"
    );
    check(progress, 0, 0)?;
    let _reservation = reserve(progress)?;
    let url = format!("{origin}/api/models/{repo}/revision/{requested}?blobs=true");
    let snapshot = local::cancellable(
        move || Ok(super::get_json(&url, &local::hf_headers())?),
        progress,
        0,
        0,
    )?;
    let snapshot = parse_snapshot(&snapshot)?;
    if let Some(prepared) =
        cached_snapshot(root, &format!("hf:{repo}"), &snapshot.revision, progress)?
    {
        return Ok(prepared);
    }
    let marker = local::managed_file_path(
        root,
        &format!(".imports/hf/{repo}/{}/import.lock", snapshot.revision),
        true,
    )?;
    let _lock = lock(&marker, progress)?;
    let stage = marker.parent().unwrap().join("payload");
    fs::create_dir_all(&stage)?;
    ensure!(
        fs::symlink_metadata(&stage)?.is_dir()
            && !fs::symlink_metadata(&stage)?.file_type().is_symlink(),
        "invalid import staging directory"
    );
    let fetch = |name: &str, progress: &mut dyn FnMut(u64, u64) -> bool| -> Result<()> {
        let (size, hash) = snapshot.files.get(name).context("missing HF payload")?;
        let destination = local::managed_file_path(&stage, name, true)?;
        let url = format!("{origin}/{repo}/resolve/{}/{name}", snapshot.revision);
        local::download_payload_at(&destination, *size, hash.as_deref(), &url, progress)?;
        Ok(())
    };
    // Reject unsupported architectures before transferring any large weights.
    fetch("config.json", progress)?;
    validate_mlx_config(&read_json(&stage.join("config.json"))?)?;
    let required: BTreeSet<String> = if snapshot.files.contains_key("model.safetensors.index.json")
    {
        fetch("model.safetensors.index.json", progress)?;
        let index = read_json(&stage.join("model.safetensors.index.json"))?;
        let map = index["weight_map"]
            .as_object()
            .context("invalid shard index")?;
        let required: BTreeSet<String> = map
            .values()
            .map(|file| file.as_str().unwrap_or("").to_owned())
            .collect();
        ensure!(!required.is_empty() && required.iter().all(|file| file.ends_with(".safetensors") && snapshot.files.contains_key(file)), "shard inventory is incomplete");
        required
    } else {
        let required: BTreeSet<_> = snapshot
            .files
            .keys()
            .filter(|name| name.ends_with(".safetensors"))
            .cloned()
            .collect();
        ensure!(
            required.len() == 1 && required.iter().all(|name| !name.contains("-of-")),
            "sharded checkpoint requires an index"
        );
        required
    };
    for name in snapshot.files.keys().filter(|name| {
        name.as_str() != "config.json" && name.as_str() != "model.safetensors.index.json"
    }) {
        if !name.ends_with(".safetensors") || required.contains(name) {
            fetch(name, progress)?;
        }
    }
    // A previous cancellation after writing the manifest is safe to retry.
    if stage.join(MANIFEST).exists() {
        fs::remove_file(stage.join(MANIFEST))?;
    }
    let result = finish(
        root,
        &stage,
        &format!("hf:{repo}"),
        Some(snapshot.revision),
        progress,
    )?;
    if result.reused {
        fs::remove_dir_all(&stage)?;
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::super::model_package::tests::fixture;
    use super::*;
    use serde_json::json;

    #[test]
    fn local_import_preserves_every_payload_reuses_identity_and_detects_tampering() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        fixture(&source);
        let root = dir.path().join("models");
        let first = directory_into(&root, &source, &mut |_, _| true).unwrap();
        assert!(!first.reused);
        for name in payload_names(&source).unwrap() {
            assert_eq!(
                fs::read(source.join(&name)).unwrap(),
                fs::read(first.path.join(name)).unwrap()
            );
        }
        let manifest: ModelManifest =
            serde_json::from_value(read_json(&first.path.join(MANIFEST)).unwrap()).unwrap();
        assert_eq!(manifest.profile, "preserve");
        assert_eq!(manifest.files.len(), 5);
        assert!(
            manifest
                .tensors
                .values()
                .any(|tensor| tensor.affine.is_some())
        );
        let second = directory_into(&root, &source, &mut |_, _| true).unwrap();
        assert!(second.reused);
        assert_eq!(first.identity, second.identity);
        assert_eq!(first.path, second.path);
        assert_eq!(
            fs::read_dir(root.join(".imports/local")).unwrap().count(),
            0
        );
        fs::write(first.path.join("LICENSE"), "tampered license").unwrap();
        assert!(
            directory_into(&root, &source, &mut |_, _| true)
                .unwrap_err()
                .to_string()
                .contains("modified")
        );
        assert_eq!(
            fs::read_to_string(source.join("LICENSE")).unwrap(),
            "fixture license"
        );
    }

    #[test]
    fn incomplete_shards_unsupported_config_and_cancel_never_publish() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        fixture(&source);
        let root = dir.path().join("models");
        fs::write(
            source.join("model.safetensors.index.json"),
            json!({"weight_map":{"missing":"missing.safetensors"}}).to_string(),
        )
        .unwrap();
        assert!(directory_into(&root, &source, &mut |_, _| true).is_err());
        assert!(!root.join("mlx").exists());
        fs::remove_file(source.join("model.safetensors.index.json")).unwrap();
        let mut c = read_json(&source.join("config.json")).unwrap();
        c["rope_scaling"] = json!({"factor":2});
        fs::write(source.join("config.json"), c.to_string()).unwrap();
        assert!(
            directory_into(&root, &source, &mut |_, _| true)
                .unwrap_err()
                .to_string()
                .contains("RoPE")
        );
        fixture(&source);
        let error = directory_into(&root, &source, &mut |done, _| done == 0).unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert!(!root.join("mlx").exists());
        assert_eq!(
            fs::read_dir(root.join(".imports/local")).unwrap().count(),
            0
        );
    }

    #[test]
    fn publication_cancellation_is_retryable_and_preserves_originals() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        fixture(&source);
        let root = dir.path().join("models");
        let error = directory_into(&root, &source, &mut |_, _| {
            !fs::read_dir(root.join(".imports/local")).is_ok_and(|entries| {
                entries
                    .flatten()
                    .any(|entry| entry.path().join(MANIFEST).exists())
            })
        })
        .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert_eq!(
            fs::read_dir(root.join(".imports/local")).unwrap().count(),
            0
        );
        assert!(
            fs::read_dir(root.join("mlx/imported"))
                .unwrap()
                .flatten()
                .all(|entry| !entry.path().join("config.json").exists())
        );
        let ready = directory_into(&root, &source, &mut |_, _| true).unwrap();
        assert!(directory_into(&root, &source, &mut |_, _| false).is_err());
        assert!(ready.path.join(MANIFEST).exists());
        assert!(
            directory_into(&root, &source, &mut |_, _| true)
                .unwrap()
                .reused
        );
    }

    #[cfg(unix)]
    #[test]
    fn source_symlinks_and_managed_path_escapes_are_rejected() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        fixture(&source);
        let outside = dir.path().join("outside");
        fs::write(&outside, "keep").unwrap();
        fs::remove_file(source.join("model.safetensors")).unwrap();
        symlink(&outside, source.join("model.safetensors")).unwrap();
        let root = dir.path().join("models");
        assert!(directory_into(&root, &source, &mut |_, _| true).is_err());
        assert_eq!(fs::read_to_string(&outside).unwrap(), "keep");
        fs::remove_file(source.join("model.safetensors")).unwrap();
        fixture(&source);
        fs::create_dir_all(&root).unwrap();
        symlink(dir.path(), root.join(".imports")).unwrap();
        assert!(directory_into(&root, &source, &mut |_, _| true).is_err());
    }

    #[test]
    fn hf_download_is_pinned_resumable_and_reuses_verified_package_without_payload_requests() {
        use std::{
            io::{BufRead, BufReader},
            net::TcpListener,
            sync::{
                Arc, Mutex,
                atomic::{AtomicBool, Ordering},
            },
            thread,
        };
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        fixture(&source);
        let revision = "a".repeat(40);
        let mut routes = BTreeMap::new();
        let mut siblings = Vec::new();
        let weight_size = fs::metadata(source.join("model.safetensors"))
            .unwrap()
            .len();
        for name in payload_names(&source).unwrap() {
            let bytes = fs::read(source.join(&name)).unwrap();
            siblings.push(json!({"rfilename":name,"size":bytes.len(),"lfs":{"sha256":format!("{:x}",Sha256::digest(&bytes))}}));
            routes.insert(format!("/owner/model/resolve/{revision}/{name}"), bytes);
        }
        routes.insert(
            "/api/models/owner/model/revision/main?blobs=true".into(),
            json!({"sha":revision,"siblings":siblings})
                .to_string()
                .into_bytes(),
        );
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let stop = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let finished = stop.clone();
        let seen = requests.clone();
        let server = thread::spawn(move || {
            while !finished.load(Ordering::Relaxed) {
                let (mut socket, _) = match listener.accept() {
                    Ok(pair) => pair,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(e) => panic!("{e}"),
                };
                socket
                    .set_read_timeout(Some(Duration::from_secs(2)))
                    .unwrap();
                let mut reader = BufReader::new(socket.try_clone().unwrap());
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 {
                    continue;
                }
                let path = line.split_whitespace().nth(1).unwrap().to_owned();
                seen.lock().unwrap().push(path.clone());
                loop {
                    line.clear();
                    if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                        break;
                    }
                }
                if let Some(bytes) = routes.get(&path) {
                    let _ = write!(
                        socket,
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        bytes.len()
                    );
                    let _ = socket.write_all(bytes);
                } else {
                    let _ = socket.write_all(
                        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
            }
        });
        // Make the server stop even when an assertion panics.
        struct Server(
            std::sync::Arc<std::sync::atomic::AtomicBool>,
            Option<std::thread::JoinHandle<()>>,
        );
        impl Drop for Server {
            fn drop(&mut self) {
                self.0.store(true, std::sync::atomic::Ordering::Relaxed);
                self.1.take().unwrap().join().unwrap();
            }
        }
        let _server = Server(stop, Some(server));
        let root = dir.path().join("models");
        let error = pull_into(&root, "mlx:owner/model", &origin, &mut |done, total| {
            !(total == weight_size && done == total)
        })
        .unwrap_err();
        assert!(error.to_string().contains("cancelled"), "{error}");
        assert!(!root.join("mlx").exists());
        let part = root.join(format!(
            ".imports/hf/owner/model/{revision}/payload/model.part"
        ));
        assert_eq!(fs::metadata(part).unwrap().len(), weight_size);
        let first = pull_into(&root, "mlx:owner/model", &origin, &mut |_, _| true).unwrap();
        assert!(!first.reused);
        let count = requests.lock().unwrap().len();
        let second = pull_into(&root, "mlx:owner/model", &origin, &mut |_, _| true).unwrap();
        assert!(second.reused);
        assert_eq!(second.identity, first.identity);
        assert_eq!(requests.lock().unwrap().len(), count + 1); // metadata only, no payload network transfer
        let manifest = read_json(&first.path.join(MANIFEST)).unwrap();
        assert_eq!(manifest["revision"], revision);
        assert!(
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|path| path.contains("/resolve/"))
                .all(|path| !path.contains("/main/"))
        );
        assert_eq!(
            requests
                .lock()
                .unwrap()
                .iter()
                .filter(|path| path.ends_with("model.safetensors"))
                .count(),
            1
        );
    }
}
