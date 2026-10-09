//! Local models: download GGUF weights from Hugging Face, pull into Ollama, and serve
//! with llama.cpp's `llama-server`. Served models speak Chat Completions on localhost.
use super::*;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{BufRead, BufReader, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::Instant,
};

#[derive(Clone, Debug, PartialEq)]
pub struct GgufFile {
    pub path: String,
    pub size: u64,
    pub sha256: Option<String>,
}

pub fn models_dir() -> PathBuf {
    std::env::var_os("LOOM_MODELS")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".loom/models")))
        .unwrap_or_else(|| PathBuf::from(".loom/models"))
}

/// GGUF weights owned by LOOM, sorted by path for stable CLI output.
pub fn installed_ggufs() -> Vec<PathBuf> {
    fn walk(directory: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_symlink() {
                continue;
            }
            if kind.is_dir() {
                walk(&path, out);
            } else if kind.is_file()
                && path
                    .extension()
                    .is_some_and(|extension| extension.eq_ignore_ascii_case("gguf"))
            {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(&models_dir(), &mut out);
    out.sort();
    out
}

pub(crate) fn hf_headers() -> Vec<(String, String)> {
    std::env::var("HF_TOKEN")
        .ok()
        .filter(|t| !t.is_empty())
        .map(|t| vec![("Authorization".into(), format!("Bearer {t}"))])
        .unwrap_or_default()
}

/// GGUF files in a Hugging Face repository, with LFS sizes and hashes.
pub fn list_gguf(repo: &str) -> Result<Vec<GgufFile>, ProviderError> {
    validate_repo(repo).map_err(|e| ProviderError::new(ErrorKind::BadRequest, e.to_string()))?;
    let value = get_json(
        &format!("https://huggingface.co/api/models/{repo}/tree/main?recursive=1"),
        &hf_headers(),
    )?;
    Ok(parse_tree(&value))
}

pub(crate) fn parse_tree(value: &Value) -> Vec<GgufFile> {
    value
        .as_array()
        .into_iter()
        .flatten()
        .filter(|e| e["type"] == "file")
        .filter_map(|e| {
            let path = e["path"].as_str()?;
            path.to_ascii_lowercase()
                .ends_with(".gguf")
                .then(|| GgufFile {
                    path: path.to_owned(),
                    size: e["lfs"]["size"]
                        .as_u64()
                        .or_else(|| e["size"].as_u64())
                        .unwrap_or(0),
                    sha256: e["lfs"]["oid"].as_str().map(str::to_owned),
                })
        })
        .collect()
}

/// Bytes of physical memory, if the platform reports it.
pub fn total_memory() -> Option<u64> {
    if cfg!(target_os = "macos") {
        let out = Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()?;
        String::from_utf8(out.stdout).ok()?.trim().parse().ok()
    } else {
        let info = fs::read_to_string("/proc/meminfo").ok()?;
        let kb: u64 = info
            .lines()
            .find(|l| l.starts_with("MemTotal:"))?
            .split_whitespace()
            .nth(1)?
            .parse()
            .ok()?;
        Some(kb * 1024)
    }
}

/// Best quantization that leaves room for the KV cache and the rest of the system.
/// Split files (`-00001-of-00003`) and vision projectors are skipped.
pub fn recommend(files: &[GgufFile], memory: u64) -> Option<&GgufFile> {
    const PREFERENCE: &[&str] = &[
        "q4_k_m", "q5_k_m", "q4_k_s", "q5_k_s", "q6_k", "iq4_xs", "q4_0", "q8_0", "q3_k_m",
    ];
    let budget = memory / 100 * 55;
    files
        .iter()
        .filter(|f| {
            let p = f.path.to_ascii_lowercase();
            f.size > 0 && f.size <= budget && !p.contains("-of-") && !p.contains("mmproj")
        })
        .min_by_key(|f| {
            let p = f.path.to_ascii_lowercase();
            PREFERENCE
                .iter()
                .position(|q| p.contains(q))
                .unwrap_or(PREFERENCE.len())
        })
}

/// Download `file` from `repo` into the models directory, resuming a partial download and
/// verifying the LFS sha256. Returns the local path.
pub fn download(
    repo: &str,
    file: &GgufFile,
    progress: &mut dyn FnMut(u64, u64),
) -> anyhow::Result<PathBuf> {
    download_progress(repo, file, &mut |done, total| {
        progress(done, total);
        true
    })
}

/// Cancellable HF-only pull. Accepts `hf:owner/repo[/file.gguf]` (or a bare repo).
/// An omitted filename uses the existing memory-aware quantization recommendation.
pub fn pull_progress(
    spec: &str,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> anyhow::Result<PathBuf> {
    let (repo, wanted) = parse_spec(spec)?;
    check_progress(progress, 0, 0)?;
    let lookup = repo.clone();
    let files = cancellable(move || Ok(list_gguf(&lookup)?), progress, 0, 0)?;
    let file = select_file(&files, wanted.as_deref())?;
    download_progress(&repo, file, progress)
}

pub(crate) fn parse_spec(spec: &str) -> anyhow::Result<(String, Option<String>)> {
    let spec = spec.trim();
    let spec = spec.strip_prefix("hf:").unwrap_or(spec);
    let spec = if let Some(rest) = hub_path(spec) {
        hub_link_spec(rest)?
    } else if let Some((repo, file)) = spec.strip_suffix(']').and_then(|prefix| prefix.rsplit_once(" [")) {
        format!("{repo}/{file}")
    } else {
        spec.to_owned()
    };
    // Also accept owner/repo:filename and Ollama's owner/repo:QUANT.
    let (repo, wanted) = if let Some((repo, file)) = spec.split_once(':') {
        (repo.to_owned(), Some(file.to_owned()))
    } else {
        let mut parts = spec.splitn(3, '/');
        let owner = parts.next().unwrap_or_default();
        let name = parts.next().unwrap_or_default();
        (format!("{owner}/{name}"), parts.next().map(str::to_owned))
    };
    validate_repo(&repo)?;
    if let Some(file) = &wanted {
        if !is_quant_tag(file) {
            validate_gguf_path(file)?;
        }
    }
    Ok((repo, wanted))
}

/// The path after an exact Hugging Face host, with or without scheme and `www.`.
fn hub_path(spec: &str) -> Option<&str> {
    let rest = spec
        .strip_prefix("https://")
        .or_else(|| spec.strip_prefix("http://"))
        .unwrap_or(spec);
    let rest = rest.strip_prefix("www.").unwrap_or(rest);
    rest.strip_prefix("huggingface.co/")
        .or_else(|| rest.strip_prefix("hf.co/"))
}

/// Reduce a repo, tree or file link on main to `owner/repo[/file]`.
fn hub_link_spec(rest: &str) -> anyhow::Result<String> {
    // Queries and anchors (`?download=true`, `#L1`) never select a different file.
    let rest = rest.split(['?', '#']).next().unwrap_or_default();
    let rest = rest.strip_suffix('/').unwrap_or(rest);
    let pieces: Vec<_> = rest.splitn(5, '/').collect();
    match pieces.as_slice() {
        [owner, name] => Ok(format!("{owner}/{name}")),
        [owner, name, "tree", "main"] => Ok(format!("{owner}/{name}")),
        [owner, name, "blob" | "resolve", "main", file] => Ok(format!("{owner}/{name}/{file}")),
        [_, _, ..] => anyhow::bail!("expected a Hugging Face GGUF file link on main"),
        _ => anyhow::bail!("expected a Hugging Face owner/repo"),
    }
}

/// `Q4_K_M`, `IQ3_XS`, `F16`: a quantization name rather than a file path.
fn is_quant_tag(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

fn select_file<'a>(files: &'a [GgufFile], wanted: Option<&str>) -> anyhow::Result<&'a GgufFile> {
    let safe = files
        .iter()
        .filter(|file| validate_gguf_path(&file.path).is_ok());
    if let Some(wanted) = wanted.filter(|wanted| is_quant_tag(wanted)) {
        let tag = wanted.to_ascii_lowercase();
        let matches: Vec<_> = safe
            .filter(|file| {
                let name = file.path.rsplit('/').next().unwrap_or_default().to_ascii_lowercase();
                let stem = &name[..name.len() - ".gguf".len()];
                stem.strip_suffix(tag.as_str())
                    .is_some_and(|head| head.is_empty() || head.ends_with(['-', '.']))
            })
            .collect();
        return match matches.as_slice() {
            [file] => Ok(file),
            [] => anyhow::bail!("no {wanted} GGUF in this repository"),
            many => anyhow::bail!(
                "{wanted} matches several GGUF files; name one: {}",
                many.iter().map(|file| file.path.as_str()).collect::<Vec<_>>().join(", ")
            ),
        };
    }
    if let Some(wanted) = wanted {
        return safe
            .into_iter()
            .find(|file| file.path == wanted)
            .ok_or_else(|| anyhow::anyhow!("{wanted} is not a GGUF in this repository"));
    }
    let safe: Vec<_> = safe.cloned().collect();
    let path = recommend(&safe, total_memory().unwrap_or(8 << 30))
        .ok_or_else(|| anyhow::anyhow!("no GGUF in this repository fits available memory"))?;
    Ok(files.iter().find(|file| file.path == path.path).unwrap())
}

pub(crate) fn validate_repo(repo: &str) -> anyhow::Result<()> {
    let parts: Vec<_> = repo.split('/').collect();
    anyhow::ensure!(
        parts.len() == 2 && parts.iter().all(|s| safe_component(s)),
        "expected a Hugging Face owner/repo"
    );
    Ok(())
}

fn safe_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && value
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(&c))
}

pub(crate) fn validate_gguf_path(path: &str) -> anyhow::Result<()> {
    validate_relative_file(path)?;
    anyhow::ensure!(
        path.split('/').all(safe_component) && path.to_ascii_lowercase().ends_with(".gguf"),
        "expected a relative GGUF filename"
    );
    Ok(())
}

pub(crate) fn validate_relative_file(path: &str) -> anyhow::Result<()> {
    anyhow::ensure!(path.split('/').all(safe_component), "expected a relative checkpoint filename");
    Ok(())
}

/// Resolve only ordinary components beneath the managed root. Never follow a child symlink.
pub(crate) fn managed_path(root: &Path, id: &str, create: bool) -> anyhow::Result<PathBuf> {
    validate_gguf_path(id)?;
    managed_file_path(root, id, create)
}

pub(crate) fn managed_file_path(root: &Path, id: &str, create: bool) -> anyhow::Result<PathBuf> {
    validate_relative_file(id)?;
    if create {
        fs::create_dir_all(root)?;
    }
    let root = fs::canonicalize(root)?;
    let mut path = root.clone();
    let parts: Vec<_> = id.split('/').collect();
    for (index, component) in parts.iter().enumerate() {
        path.push(component);
        let last = index + 1 == parts.len();
        match fs::symlink_metadata(&path) {
            Ok(meta) => {
                anyhow::ensure!(
                    !meta.file_type().is_symlink(),
                    "checkpoint path contains a symlink"
                );
                anyhow::ensure!(
                    if last { meta.is_file() } else { meta.is_dir() },
                    "checkpoint path has an unexpected file type"
                );
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && create => {
                if !last {
                    fs::create_dir(&path)?;
                }
            }
            Err(e) => return Err(e.into()),
        }
        if !last {
            anyhow::ensure!(
                fs::canonicalize(&path)?.starts_with(&root),
                "checkpoint escapes models root"
            );
        }
    }
    Ok(path)
}

pub fn download_progress(
    repo: &str,
    file: &GgufFile,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> anyhow::Result<PathBuf> {
    download_into(&models_dir(), repo, file, progress)
}

fn download_into(
    root: &Path,
    repo: &str,
    file: &GgufFile,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> anyhow::Result<PathBuf> {
    validate_repo(repo)?;
    validate_gguf_path(&file.path)?;
    validate_hash(file.sha256.as_deref())?;
    check_progress(progress, 0, file.size)?;
    let dest = managed_path(root, &format!("{repo}/{}", file.path), true)?;
    let location = CheckpointLocation::checked(&dest)?;
    if dest.exists() && (file.size == 0 || fs::metadata(&dest)?.len() == file.size) {
        let mut cached = location.open(false, false)?;
        check_progress(progress, cached.metadata()?.len(), file.size)?;
        verify_hash(&mut cached, file.sha256.as_deref(), progress, file.size)?;
        return Ok(dest);
    }
    let url = format!("https://huggingface.co/{repo}/resolve/main/{}", file.path);
    download_from(root, repo, file, &url, progress)
}

/// Fetch into the managed path, resuming from the `.part` after a dropped connection.
/// Large checkpoints routinely lose a CDN connection midway; one that made progress is
/// resumed with a Range request. An attempt that adds nothing ends the download.
fn download_from(
    root: &Path,
    repo: &str,
    file: &GgufFile,
    url: &str,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> anyhow::Result<PathBuf> {
    let dest = managed_path(root, &format!("{repo}/{}", file.path), true)?;
    download_payload_at(&dest, file.size, file.sha256.as_deref(), url, progress)
}

/// Shared bounded/resumable transfer for pinned safetensors packages as well as GGUF.
pub(crate) fn download_payload_at(
    dest: &Path,
    size: u64,
    sha256: Option<&str>,
    url: &str,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> anyhow::Result<PathBuf> {
    let location = CheckpointLocation::checked(&dest)?;
    validate_hash(sha256)?;
    if dest.exists() && (size == 0 || fs::metadata(dest)?.len() == size) {
        let mut cached = location.open(false, false)?;
        check_progress(progress, cached.metadata()?.len(), size)?;
        verify_hash(&mut cached, sha256, progress, size)?;
        return Ok(dest.to_owned());
    }
    let part = || fs::metadata(dest.with_extension("part")).map_or(0, |meta| meta.len());
    let mut resumes = 0;
    loop {
        let before = part();
        let error = match fetch_location(url, &location, size, sha256, progress) {
            Ok(()) => return Ok(dest.to_owned()),
            Err(error) => error,
        };
        let text = error.to_string();
        let final_error = text.contains("cancelled") || text.starts_with("sha256 mismatch");
        if final_error || part() <= before || resumes >= 20 {
            return Err(error);
        }
        resumes += 1;
        // Brief pause so a flapping connection is not hammered; cancellation still wins.
        let pause = Instant::now();
        while pause.elapsed() < Duration::from_millis(250 * resumes.min(8)) {
            check_progress(progress, part(), size)?;
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

#[cfg(test)]
pub(crate) fn fetch(
    url: &str,
    dest: &Path,
    size: u64,
    sha256: Option<&str>,
    progress: &mut dyn FnMut(u64, u64),
) -> anyhow::Result<()> {
    fetch_progress(url, dest, size, sha256, &mut |done, total| {
        progress(done, total);
        true
    })
}

fn validate_hash(hash: Option<&str>) -> anyhow::Result<()> {
    anyhow::ensure!(
        hash.is_none_or(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())),
        "invalid sha256 digest"
    );
    Ok(())
}

fn check_progress(
    progress: &mut dyn FnMut(u64, u64) -> bool,
    done: u64,
    total: u64,
) -> anyhow::Result<()> {
    anyhow::ensure!(progress(done, total), "checkpoint download cancelled");
    Ok(())
}

/// Poll network setup on the caller; a cancelled request cannot write checkpoint files.
pub(crate) fn cancellable<T: Send + 'static>(
    task: impl FnOnce() -> anyhow::Result<T> + Send + 'static,
    progress: &mut dyn FnMut(u64, u64) -> bool,
    done: u64,
    total: u64,
) -> anyhow::Result<T> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let _ = tx.send(task());
    });
    let started = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(result) => return result,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                check_progress(progress, done, total)?;
                anyhow::ensure!(
                    started.elapsed() < Duration::from_secs(15),
                    "checkpoint request timed out"
                );
            }
            Err(e) => return Err(e.into()),
        }
    }
}

/// Pin the parent directory so a rename/symlink swap during download cannot redirect
/// writes, publication or deletion. Unix operations use the same directory descriptor.
pub(crate) struct CheckpointLocation {
    path: PathBuf,
    #[cfg(unix)]
    directory: fs::File,
}

impl CheckpointLocation {
    pub(crate) fn checked(path: &Path) -> anyhow::Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| anyhow::anyhow!("checkpoint needs a parent directory"))?;
        #[cfg(unix)]
        let directory = {
            use std::os::fd::{AsRawFd, FromRawFd};
            anyhow::ensure!(
                parent.is_absolute(),
                "checkpoint directory must be absolute"
            );
            let mut directory = fs::File::open("/")?;
            for component in parent.components() {
                let std::path::Component::Normal(name) = component else {
                    continue;
                };
                use std::os::unix::ffi::OsStrExt;
                let name = std::ffi::CString::new(name.as_bytes())?;
                let fd = unsafe {
                    libc::openat(
                        directory.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if fd < 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                directory = unsafe { fs::File::from_raw_fd(fd) };
            }
            directory
        };
        Ok(Self {
            path: path.to_owned(),
            #[cfg(unix)]
            directory,
        })
    }

    fn path(&self, partial: bool) -> PathBuf {
        if partial {
            self.path.with_extension("part")
        } else {
            self.path.clone()
        }
    }

    #[cfg(unix)]
    fn name(&self, partial: bool) -> anyhow::Result<std::ffi::CString> {
        use std::os::unix::ffi::OsStrExt;
        Ok(std::ffi::CString::new(
            self.path(partial)
                .file_name()
                .ok_or_else(|| anyhow::anyhow!("checkpoint needs a filename"))?
                .as_bytes(),
        )?)
    }

    pub(crate) fn open(&self, partial: bool, write: bool) -> anyhow::Result<fs::File> {
        #[cfg(unix)]
        let file = {
            use std::os::fd::{AsRawFd, FromRawFd};
            let name = self.name(partial)?;
            let flags = if write {
                libc::O_RDWR | libc::O_CREAT
            } else {
                libc::O_RDONLY
            };
            let fd = unsafe {
                libc::openat(
                    self.directory.as_raw_fd(),
                    name.as_ptr(),
                    flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            unsafe { fs::File::from_raw_fd(fd) }
        };
        #[cfg(not(unix))]
        let file = open_regular(&self.path(partial), write)?;
        anyhow::ensure!(
            file.metadata()?.is_file(),
            "checkpoint is not a regular file"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            anyhow::ensure!(
                file.metadata()?.nlink() == 1,
                "checkpoint must not be hard-linked"
            );
        }
        Ok(file)
    }

    pub(crate) fn remove(&self, partial: bool) -> anyhow::Result<()> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            let name = self.name(partial)?;
            if unsafe { libc::unlinkat(self.directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        #[cfg(not(unix))]
        fs::remove_file(self.path(partial))?;
        Ok(())
    }

    fn publish(&self, downloaded: &fs::File) -> anyhow::Result<()> {
        let partial = self.open(true, false)?;
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd;
            use std::os::unix::fs::MetadataExt;
            let current = partial.metadata()?;
            let expected = downloaded.metadata()?;
            anyhow::ensure!(
                current.dev() == expected.dev() && current.ino() == expected.ino(),
                "partial checkpoint was replaced during download"
            );
            let current_directory = Self::checked(&self.path)?.directory.metadata()?;
            let expected_directory = self.directory.metadata()?;
            anyhow::ensure!(
                current_directory.dev() == expected_directory.dev()
                    && current_directory.ino() == expected_directory.ino(),
                "checkpoint directory was replaced during download"
            );
            let from = self.name(true)?;
            let to = self.name(false)?;
            let directory = self.directory.as_raw_fd();
            if unsafe { libc::renameat(directory, from.as_ptr(), directory, to.as_ptr()) } != 0 {
                return Err(std::io::Error::last_os_error().into());
            }
        }
        #[cfg(not(unix))]
        {
            let _ = (partial, downloaded);
            fs::rename(self.path(true), &self.path)?;
        }
        Ok(())
    }
}

#[cfg(not(unix))]
fn open_regular(path: &Path, write: bool) -> anyhow::Result<fs::File> {
    if let Ok(meta) = fs::symlink_metadata(path) {
        anyhow::ensure!(
            meta.is_file() && !meta.file_type().is_symlink(),
            "checkpoint is not a regular file"
        );
    }
    let mut options = fs::OpenOptions::new();
    options.read(true).write(write).create(write);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x00200000); // FILE_FLAG_OPEN_REPARSE_POINT
    }
    let file = options.open(path)?;
    anyhow::ensure!(
        file.metadata()?.is_file(),
        "checkpoint is not a regular file"
    );
    Ok(file)
}

fn verify_hash(
    file: &mut fs::File,
    sha256: Option<&str>,
    progress: &mut dyn FnMut(u64, u64) -> bool,
    total: u64,
) -> anyhow::Result<()> {
    if let Some(expected) = sha256 {
        file.seek(SeekFrom::Start(0))?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0; 1 << 20];
        loop {
            check_progress(progress, file.metadata()?.len(), total)?;
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        let actual = format!("{:x}", hasher.finalize());
        anyhow::ensure!(
            actual.eq_ignore_ascii_case(expected),
            "sha256 mismatch: expected {expected}, got {actual}"
        );
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn fetch_progress(
    url: &str,
    dest: &Path,
    size: u64,
    sha256: Option<&str>,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> anyhow::Result<()> {
    // This lower-level compatibility/test helper accepts an arbitrary trusted directory.
    // Managed downloads above pin the already validated directory without resolving links.
    let parent = fs::canonicalize(dest.parent().unwrap_or(Path::new(".")))?;
    let location = CheckpointLocation::checked(
        &parent.join(
            dest.file_name()
                .ok_or_else(|| anyhow::anyhow!("checkpoint needs a filename"))?,
        ),
    )?;
    fetch_location(url, &location, size, sha256, progress)
}

fn fetch_location(
    url: &str,
    location: &CheckpointLocation,
    size: u64,
    sha256: Option<&str>,
    progress: &mut dyn FnMut(u64, u64) -> bool,
) -> anyhow::Result<()> {
    validate_hash(sha256)?;
    check_progress(progress, 0, size)?;
    let mut file = location.open(true, true)?;
    let lock_started = Instant::now();
    loop {
        match fs2::FileExt::try_lock_exclusive(&file) {
            Ok(()) => break,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    && lock_started.elapsed() < Duration::from_millis(250) =>
            {
                check_progress(progress, file.metadata()?.len(), size)?;
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) => return Err(e.into()),
        }
    }
    let have = file.metadata()?.len();
    let mut expected_total = size;
    check_progress(progress, have, size)?;
    anyhow::ensure!(
        size == 0 || have <= size,
        "partial checkpoint exceeds expected size"
    );
    // A completed part may have been cancelled during verification or publication.
    let response = if size > 0 && have == size {
        None
    } else {
        let url = url.to_owned();
        let headers = hf_headers();
        Some(cancellable(
            move || {
                let agent = ureq::builder()
                    .timeout_connect(Duration::from_secs(5))
                    .timeout_read(Duration::from_secs(2))
                    .timeout_write(Duration::from_secs(5))
                    .redirects(5)
                    .build();
                let mut request = agent.get(&url);
                for (k, v) in headers {
                    request = request.set(&k, &v);
                }
                if have > 0 {
                    request = request.set("Range", &format!("bytes={have}-"));
                }
                Ok(request.call()?)
            },
            progress,
            have,
            size,
        )?)
    };
    if let Some(response) = response {
        anyhow::ensure!(
            matches!(response.status(), 200 | 206),
            "unexpected checkpoint HTTP status"
        );
        let resumed = response.status() == 206;
        let length = response
            .header("content-length")
            .and_then(|l| l.parse::<u64>().ok())
            .unwrap_or(0);
        let total = if resumed {
            let (start, end, total) = response
                .header("content-range")
                .and_then(|h| h.strip_prefix("bytes "))
                .and_then(|h| h.split_once('/'))
                .and_then(|(range, total)| {
                    let (start, end) = range.split_once('-')?;
                    Some((
                        start.parse::<u64>().ok()?,
                        end.parse::<u64>().ok()?,
                        total.parse::<u64>().ok()?,
                    ))
                })
                .ok_or_else(|| anyhow::anyhow!("missing or invalid Content-Range"))?;
            anyhow::ensure!(
                start == have
                    && start <= end
                    && end < total
                    && (size == 0 || total == size)
                    && (length == 0 || length == end - start + 1),
                "inconsistent checkpoint Content-Range"
            );
            total
        } else {
            anyhow::ensure!(
                size == 0 || length == 0 || length == size,
                "unexpected checkpoint Content-Length"
            );
            if size > 0 { size } else { length }
        };
        expected_total = total;
        check_progress(progress, if resumed { have } else { 0 }, total)?;
        if !resumed {
            file.set_len(0)?;
        }
        file.seek(SeekFrom::Start(if resumed { have } else { 0 }))?;
        let mut done = if resumed { have } else { 0 };
        let mut reader = response.into_reader();
        let mut buf = vec![0u8; 1 << 20];
        let mut last_bytes = Instant::now();
        loop {
            check_progress(progress, done, total)?;
            let n = match reader.read(&mut buf) {
                Ok(n) => n,
                // The short read timeout only keeps cancellation responsive. A CDN pause
                // is not a failure until nothing has arrived for a full minute.
                Err(e)
                    if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock)
                        && last_bytes.elapsed() < Duration::from_secs(60) =>
                {
                    continue;
                }
                Err(e) => {
                    check_progress(progress, done, total)?;
                    return Err(anyhow::anyhow!("checkpoint connection lost: {e}"));
                }
            };
            last_bytes = Instant::now();
            if n == 0 {
                break;
            }
            anyhow::ensure!(
                total == 0 || done + n as u64 <= total,
                "checkpoint exceeds expected size"
            );
            file.write_all(&buf[..n])?;
            done += n as u64;
            check_progress(progress, done, total)?;
        }
        file.flush()?;
        anyhow::ensure!(
            total == 0 || done == total,
            "incomplete checkpoint download"
        );
    }
    check_progress(progress, file.metadata()?.len(), expected_total)?;
    if let Err(e) = verify_hash(&mut file, sha256, progress, expected_total) {
        if e.to_string().starts_with("sha256 mismatch") {
            location.remove(true)?;
        }
        return Err(e);
    }
    check_progress(progress, file.metadata()?.len(), expected_total)?;
    if let Ok(meta) = fs::symlink_metadata(&location.path) {
        anyhow::ensure!(
            meta.is_file() && !meta.file_type().is_symlink(),
            "checkpoint destination is not a regular file"
        );
    }
    location.publish(&file)?;
    Ok(())
}

/// `ollama pull` through the local Ollama API, reporting progress.
pub fn ollama_pull(model: &str, progress: &mut dyn FnMut(&str, u64, u64)) -> anyhow::Result<()> {
    let response = ureq::post("http://127.0.0.1:11434/api/pull")
        .timeout(std::time::Duration::from_secs(24 * 3600))
        .send_json(json!({"model": model, "stream": true}))?;
    for line in BufReader::new(response.into_reader()).lines() {
        let event: Value = serde_json::from_str(&line?)?;
        if let Some(error) = event["error"].as_str() {
            anyhow::bail!("ollama: {error}");
        }
        progress(
            event["status"].as_str().unwrap_or(""),
            event["completed"].as_u64().unwrap_or(0),
            event["total"].as_u64().unwrap_or(0),
        );
    }
    Ok(())
}

static SERVED: std::sync::Mutex<Vec<u32>> = std::sync::Mutex::new(Vec::new());

/// Stop every llama-server this process started, then exit. Installed for SIGINT and
/// SIGTERM so `kill` or Ctrl+C never leaves a model loaded in memory.
#[cfg(unix)]
extern "C" fn stop_served(_: libc::c_int) {
    if let Ok(pids) = SERVED.try_lock() {
        for pid in pids.iter() {
            unsafe { libc::kill(*pid as libc::pid_t, libc::SIGTERM) };
        }
    }
    unsafe { libc::_exit(130) };
}

/// A `llama-server` process owned by Loom; stopped when dropped or on SIGINT/SIGTERM.
pub struct LocalServer {
    child: Child,
    pub port: u16,
    pub model: String,
    pub log: PathBuf,
}

impl LocalServer {
    pub fn start(gguf: &Path, port: u16, context: u32) -> anyhow::Result<Self> {
        let log = models_dir().join(format!("llama-server-{port}.log"));
        fs::create_dir_all(log.parent().unwrap_or(Path::new(".")))?;
        let out = fs::File::create(&log)?;
        let binary = super::runtime::server_binary()?;
        let mut command = Command::new(&binary);
        if let Some(directory) = binary.parent() {
            command.current_dir(directory);
        }
        let child = command
            .args([
                "-m",
                &gguf.to_string_lossy(),
                "--host",
                "127.0.0.1",
                "--port",
                &port.to_string(),
            ])
            .args(["-c", &context.to_string(), "--jinja"])
            .stdin(Stdio::null())
            .stdout(out.try_clone()?)
            .stderr(out)
            .spawn()
            .map_err(|e| anyhow::anyhow!("{} is not runnable ({e})", binary.display()))?;
        let model = gguf
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Ok(mut pids) = SERVED.lock() {
            pids.push(child.id());
        }
        #[cfg(unix)]
        unsafe {
            libc::signal(
                libc::SIGINT,
                stop_served as extern "C" fn(libc::c_int) as libc::sighandler_t,
            );
            libc::signal(
                libc::SIGTERM,
                stop_served as extern "C" fn(libc::c_int) as libc::sighandler_t,
            );
        }
        let mut server = Self {
            child,
            port,
            model,
            log,
        };
        let started = Instant::now();
        loop {
            if let Some(status) = server.child.try_wait()? {
                anyhow::bail!(
                    "llama-server exited ({status}); see {}",
                    server.log.display()
                );
            }
            if ureq::get(&format!("http://127.0.0.1:{port}/health"))
                .timeout(std::time::Duration::from_secs(2))
                .call()
                .is_ok()
            {
                return Ok(server);
            }
            if started.elapsed().as_secs() > 300 {
                anyhow::bail!(
                    "llama-server did not become healthy in 300s; see {}",
                    server.log.display()
                );
            }
            std::thread::sleep(std::time::Duration::from_millis(500));
        }
    }

    pub fn endpoint(&self) -> Endpoint {
        Endpoint {
            name: format!("llama-server:{}", self.port),
            protocol: Protocol::Chat,
            base_url: format!("http://127.0.0.1:{}/v1", self.port),
            api_key: None,
            headers: vec![],
        }
    }
}

impl Drop for LocalServer {
    fn drop(&mut self) {
        if let Ok(mut pids) = SERVED.lock() {
            pids.retain(|p| *p != self.child.id());
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn f(path: &str, gb: f64) -> GgufFile {
        GgufFile {
            path: path.into(),
            size: (gb * 1e9) as u64,
            sha256: None,
        }
    }

    #[test]
    fn recommends_the_preferred_quant_that_fits() {
        let files = vec![
            f("m-Q8_0.gguf", 8.0),
            f("m-Q4_K_M.gguf", 4.6),
            f("m-Q2_K.gguf", 3.0),
            f("mmproj-f16.gguf", 0.8),
            f("big-Q4_K_M-00001-of-00002.gguf", 4.0),
        ];
        assert_eq!(recommend(&files, 24 << 30).unwrap().path, "m-Q4_K_M.gguf");
        assert_eq!(
            recommend(&files, 6 << 30).unwrap().path,
            "m-Q2_K.gguf",
            "Q4_K_M exceeds 55% of 6 GiB"
        );
        assert!(recommend(&files, 4 << 30).is_none());
    }

    #[test]
    fn tree_lists_gguf_with_lfs_hash() {
        let tree = json!([
            {"type": "file", "path": "README.md", "size": 10},
            {"type": "file", "path": "q.Q4_K_M.gguf", "size": 133, "lfs": {"oid": "abc", "size": 4_000_000_000u64}},
            {"type": "directory", "path": "sub"}
        ]);
        assert_eq!(
            parse_tree(&tree),
            vec![GgufFile {
                path: "q.Q4_K_M.gguf".into(),
                size: 4_000_000_000,
                sha256: Some("abc".into())
            }]
        );
    }

    #[test]
    fn fetch_verifies_sha256() {
        let dir = tempfile::tempdir().unwrap();
        let body = r#"{"weights":true}"#;
        let good = format!("{:x}", Sha256::digest(body.as_bytes()));
        let (base, _rx) = mock::serve(200, body);
        let dest = dir.path().join("w.gguf");
        fetch(&format!("{base}/w"), &dest, 0, Some(&good), &mut |_, _| {}).unwrap();
        assert_eq!(fs::read_to_string(&dest).unwrap(), body);
        let (base, _rx) = mock::serve(200, body);
        let bad = dir.path().join("x.gguf");
        assert!(
            fetch(
                &format!("{base}/x"),
                &bad,
                0,
                Some(&"0".repeat(64)),
                &mut |_, _| {}
            )
            .is_err()
        );
        assert!(
            !bad.exists() && !bad.with_extension("part").exists(),
            "a corrupt download leaves nothing behind"
        );
    }

    #[test]
    fn download_rejects_paths_even_when_a_cached_file_exists() {
        let dir = tempfile::tempdir().unwrap();
        let cached = dir.path().join("outside.gguf");
        fs::write(&cached, b"weights").unwrap();
        let file = GgufFile {
            path: cached.to_string_lossy().into_owned(),
            size: 7,
            sha256: None,
        };
        assert!(download("owner/repo", &file, &mut |_, _| {}).is_err());
        assert!(download("../repo", &file, &mut |_, _| {}).is_err());
        assert_eq!(fs::read(&cached).unwrap(), b"weights");
    }

    #[test]
    fn fetch_rejects_an_unproven_partial_response() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("weights.gguf");
        fs::write(dest.with_extension("part"), b"abc").unwrap();
        let (base, _rx) = mock::serve(206, "def");
        assert!(fetch(&base, &dest, 6, None, &mut |_, _| {}).is_err());
        assert!(!dest.exists());
        assert_eq!(fs::read(dest.with_extension("part")).unwrap(), b"abc");
    }

    #[cfg(unix)]
    #[test]
    fn fetch_does_not_follow_a_partial_file_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        fs::write(&outside, b"keep").unwrap();
        let dest = dir.path().join("weights.gguf");
        std::os::unix::fs::symlink(&outside, dest.with_extension("part")).unwrap();
        let (base, _rx) = mock::serve(200, "changed");
        assert!(fetch(&base, &dest, 7, None, &mut |_, _| {}).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"keep");
    }

    /// A tiny server that adds range headers and can stall at either network phase.
    fn raw_server(
        response: String,
        delay_headers: bool,
        delay_body: bool,
    ) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request = String::new();
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                    break;
                }
                request.push_str(&line);
            }
            let _ = tx.send(request);
            if delay_headers {
                std::thread::sleep(Duration::from_millis(500));
            }
            if delay_body {
                let (headers, body) = response.split_once("\r\n\r\n").unwrap();
                let _ = write!(stream, "{headers}\r\n\r\n");
                let _ = stream.flush();
                std::thread::sleep(Duration::from_millis(2500));
                let _ = stream.write_all(body.as_bytes());
            } else {
                let _ = stream.write_all(response.as_bytes());
            }
        });
        (url, rx)
    }

    fn response(status: u16, headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} OK\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn cancelled_download_retains_part_and_resumes_with_range_and_hash() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("weights.gguf");
        let good = format!("{:x}", Sha256::digest(b"abcdef"));
        let (url, _) = raw_server(response(200, "", "abcdef"), false, false);
        let error =
            fetch_progress(&url, &dest, 6, Some(&good), &mut |done, _| done == 0).unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert!(!dest.exists());
        assert_eq!(fs::read(dest.with_extension("part")).unwrap(), b"abcdef");
        // Model interruption after only the first half had arrived.
        fs::write(dest.with_extension("part"), b"abc").unwrap();
        let (url, rx) = raw_server(
            response(206, "Content-Range: bytes 3-5/6\r\n", "def"),
            false,
            false,
        );
        let mut updates = Vec::new();
        fetch_progress(&url, &dest, 6, Some(&good), &mut |done, total| {
            updates.push((done, total));
            true
        })
        .unwrap();
        assert!(
            rx.recv()
                .unwrap()
                .to_ascii_lowercase()
                .contains("range: bytes=3-")
        );
        assert_eq!(fs::read(&dest).unwrap(), b"abcdef");
        assert!(!dest.with_extension("part").exists());
        assert_eq!(updates.last(), Some(&(6, 6)));
    }

    #[test]
    fn ignored_range_restarts_instead_of_appending_and_unknown_size_uses_length() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("weights.gguf");
        fs::write(dest.with_extension("part"), b"old").unwrap();
        let (url, rx) = raw_server(response(200, "", "abcdef"), false, false);
        let mut last = (0, 0);
        fetch_progress(&url, &dest, 0, None, &mut |done, total| {
            last = (done, total);
            true
        })
        .unwrap();
        assert!(
            rx.recv()
                .unwrap()
                .to_ascii_lowercase()
                .contains("range: bytes=3-")
        );
        assert_eq!(fs::read(dest).unwrap(), b"abcdef");
        assert_eq!(last, (6, 6));
    }

    #[test]
    fn incorrect_ranges_and_incomplete_bodies_are_not_published() {
        for headers in [
            "Content-Range: bytes 0-2/6\r\n",
            "Content-Range: bytes 3-5/7\r\n",
            "Content-Range: bytes 3-6/6\r\n",
            "Content-Range: bytes 3-4/6\r\n",
        ] {
            let dir = tempfile::tempdir().unwrap();
            let dest = dir.path().join("weights.gguf");
            fs::write(dest.with_extension("part"), b"abc").unwrap();
            let (url, _) = raw_server(response(206, headers, "def"), false, false);
            assert!(fetch_progress(&url, &dest, 6, None, &mut |_, _| true).is_err());
            assert!(!dest.exists());
            assert_eq!(fs::read(dest.with_extension("part")).unwrap(), b"abc");
        }
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("weights.gguf");
        let (url, _) = raw_server(
            "HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabc".into(),
            false,
            false,
        );
        assert!(fetch_progress(&url, &dest, 6, None, &mut |_, _| true).is_err());
        assert!(!dest.exists());
        assert_eq!(fs::read(dest.with_extension("part")).unwrap(), b"abc");
    }

    #[test]
    fn cancellation_is_observed_during_header_wait_and_stalled_body() {
        for (headers, body) in [(true, false), (false, true)] {
            let dir = tempfile::tempdir().unwrap();
            let dest = dir.path().join("weights.gguf");
            let (url, _) = raw_server(response(200, "", "abcdef"), headers, body);
            let started = Instant::now();
            let error = fetch_progress(&url, &dest, 6, None, &mut |_, _| {
                started.elapsed() < Duration::from_millis(50)
            })
            .unwrap_err();
            assert!(error.to_string().contains("cancelled"), "{error}");
            assert!(started.elapsed() < Duration::from_secs(4));
            assert!(!dest.exists());
            assert!(dest.with_extension("part").exists());
        }
    }

    #[test]
    fn a_body_pause_longer_than_one_read_timeout_does_not_abort_the_download() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("weights.gguf");
        let (url, _) = raw_server(response(200, "", "abcdef"), false, true);
        fetch_progress(&url, &dest, 6, None, &mut |_, _| true).unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"abcdef");
    }

    #[test]
    fn a_dropped_connection_resumes_from_the_part_with_a_new_range_request() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for reply in [
                // Promises six bytes, sends three, then the connection drops.
                "HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nabc".to_owned(),
                response(206, "Content-Range: bytes 3-5/6\r\n", "def"),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut request = String::new();
                loop {
                    let mut line = String::new();
                    if reader.read_line(&mut line).unwrap() == 0 || line == "\r\n" {
                        break;
                    }
                    request.push_str(&line);
                }
                let _ = tx.send(request);
                let _ = stream.write_all(reply.as_bytes());
            }
        });
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("models");
        let good = format!("{:x}", Sha256::digest(b"abcdef"));
        let file = GgufFile { path: "w.gguf".into(), size: 6, sha256: Some(good) };
        let location_url = format!("{base}/w");
        let dest = download_from(&root, "owner/repo", &file, &location_url, &mut |_, _| true).unwrap();
        assert_eq!(fs::read(dest).unwrap(), b"abcdef");
        let _ = rx.recv().unwrap();
        assert!(rx.recv().unwrap().to_ascii_lowercase().contains("range: bytes=3-"));
    }

    #[test]
    fn a_complete_cancelled_part_is_verified_and_published_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let dest = dir.path().join("weights.gguf");
        fs::write(dest.with_extension("part"), b"abcdef").unwrap();
        let good = format!("{:x}", Sha256::digest(b"abcdef"));
        let mut complete_updates = 0;
        let error = fetch_progress(
            "http://127.0.0.1:1",
            &dest,
            6,
            Some(&good),
            &mut |done, _| {
                if done == 6 {
                    complete_updates += 1;
                }
                complete_updates < 3
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        assert!(!dest.exists());
        assert_eq!(fs::read(dest.with_extension("part")).unwrap(), b"abcdef");
        fetch_progress("http://127.0.0.1:1", &dest, 6, Some(&good), &mut |_, _| {
            true
        })
        .unwrap();
        assert_eq!(fs::read(dest).unwrap(), b"abcdef");
    }

    #[test]
    fn cached_weights_are_hash_checked_and_cancellable_without_network() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let dest = root.join("owner/repo/weights.gguf");
        fs::create_dir_all(dest.parent().unwrap()).unwrap();
        fs::write(&dest, b"abcdef").unwrap();
        let mut file = GgufFile {
            path: "weights.gguf".into(),
            size: 6,
            sha256: Some(format!("{:x}", Sha256::digest(b"abcdef"))),
        };
        assert_eq!(
            download_into(root, "owner/repo", &file, &mut |_, _| true).unwrap(),
            fs::canonicalize(&dest).unwrap()
        );
        assert!(
            download_into(root, "owner/repo", &file, &mut |_, _| false)
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
        file.sha256 = Some("0".repeat(64));
        assert!(
            download_into(root, "owner/repo", &file, &mut |_, _| true)
                .unwrap_err()
                .to_string()
                .contains("sha256 mismatch")
        );
        assert_eq!(fs::read(&dest).unwrap(), b"abcdef");
        file.path = "../outside.gguf".into();
        assert!(download_into(root, "owner/repo", &file, &mut |_, _| true).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn directory_or_partial_replacement_cannot_redirect_publication() {
        use std::os::unix::fs::symlink;
        for replace_directory in [true, false] {
            let dir = tempfile::tempdir().unwrap();
            let root = fs::canonicalize(dir.path()).unwrap();
            let managed = root.join("managed");
            let outside = root.join("outside");
            fs::create_dir(&managed).unwrap();
            fs::create_dir(&outside).unwrap();
            fs::write(outside.join("weights.part"), b"keep").unwrap();
            let dest = managed.join("weights.gguf");
            let location = CheckpointLocation::checked(&dest).unwrap();
            let (url, _) = raw_server(response(200, "", "abcdef"), false, false);
            let mut replaced = false;
            let result = fetch_location(&url, &location, 6, None, &mut |done, _| {
                if done == 6 && !replaced {
                    replaced = true;
                    if replace_directory {
                        fs::rename(&managed, root.join("original")).unwrap();
                        symlink(&outside, &managed).unwrap();
                    } else {
                        fs::rename(dest.with_extension("part"), managed.join("original.part"))
                            .unwrap();
                        symlink(outside.join("weights.part"), dest.with_extension("part")).unwrap();
                    }
                }
                true
            });
            assert!(result.is_err());
            assert_eq!(fs::read(outside.join("weights.part")).unwrap(), b"keep");
            assert!(!outside.join("weights.gguf").exists());
            assert!(!dest.exists());
        }
    }

    #[test]
    fn hf_specs_and_repo_metadata_cannot_escape_or_change_protocol() {
        for bad in [
            "ollama:foo",
            "https://example.com/w.gguf",
            "hf:../repo/w.gguf",
            "hf:owner/../w.gguf",
            "hf:owner/repo/../w.gguf",
            "hf:owner/repo//w.gguf",
            "hf:owner/repo/./w.gguf",
            "hf:owner/repo/%2e%2e/w.gguf",
            "hf:owner/repo/sub\\w.gguf",
            "hf:owner/repo/w.gguf?x=1",
            "hf:owner/repo/w.gguf#x",
            "hf:owner/repo/w.safetensors",
            "hf:owner/repo/",
            "hf:/repo",
        ] {
            assert!(parse_spec(bad).is_err(), "accepted {bad}");
            assert!(pull_progress(bad, &mut |_, _| false).is_err());
        }
        assert_eq!(
            parse_spec("hf:owner/repo/sub/w.gguf").unwrap(),
            ("owner/repo".into(), Some("sub/w.gguf".into()))
        );
        assert_eq!(
            parse_spec("hf:owner/repo:sub/w.gguf").unwrap(),
            ("owner/repo".into(), Some("sub/w.gguf".into()))
        );
        assert_eq!(
            parse_spec("owner/repo").unwrap(),
            ("owner/repo".into(), None)
        );
        let files = vec![f("../w.gguf", 1.0), f("q4_k_m.gguf", 1.0)];
        assert!(select_file(&files, Some("../w.gguf")).is_err());
        assert_eq!(select_file(&files, None).unwrap().path, "q4_k_m.gguf");
        assert!(
            pull_progress("hf:owner/repo", &mut |_, _| false)
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
    }

    #[test]
    fn pasted_hugging_face_checkpoint_labels_and_links_are_valid_specs() {
        let repo = "empero-ai/Qwen3.8-4B-Distill-GGUF";
        let file = "Qwen3.8-4B-Q6_K.gguf";
        for spec in [
            format!("hf:{repo} [{file}]"),
            format!("{repo} [{file}]"),
            format!("https://huggingface.co/{repo}/blob/main/{file}"),
            format!("https://huggingface.co/{repo}/resolve/main/{file}"),
        ] {
            assert_eq!(parse_spec(&spec).unwrap(), (repo.into(), Some(file.into())), "{spec}");
        }
    }

    #[test]
    fn hub_links_accept_common_hosts_and_shapes_but_not_lookalikes() {
        let repo = "empero-ai/Qwen3.8-4B-Distill-GGUF";
        let file = "Qwen3.8-4B-Q6_K.gguf";
        for spec in [
            format!("hf:https://huggingface.co/{repo}/blob/main/{file}"),
            format!("huggingface.co/{repo}/resolve/main/{file}"),
            format!("http://www.huggingface.co/{repo}/blob/main/{file}"),
            format!("https://huggingface.co/{repo}/resolve/main/{file}?download=true"),
            format!("https://hf.co/{repo}/blob/main/{file}#L1"),
        ] {
            assert_eq!(parse_spec(&spec).unwrap(), (repo.into(), Some(file.into())), "{spec}");
        }
        for spec in [
            format!("hf.co/{repo}"),
            format!("https://huggingface.co/{repo}/"),
            format!("https://huggingface.co/{repo}/tree/main"),
            format!("https://huggingface.co/{repo}?library=gguf"),
        ] {
            assert_eq!(parse_spec(&spec).unwrap(), (repo.into(), None), "{spec}");
        }
        // Ollama's `hf.co/owner/repo:QUANT` names a quantization, not a file.
        assert_eq!(
            parse_spec(&format!("hf.co/{repo}:Q6_K")).unwrap(),
            (repo.into(), Some("Q6_K".into()))
        );
        for bad in [
            "https://huggingface.co.evil.com/owner/repo",
            "https://evil.com/huggingface.co/owner/repo",
            "https://user@huggingface.co/owner/repo",
            "https://huggingface.co/owner/repo/blob/dev/w.gguf",
            "https://huggingface.co/owner/repo/blob/main/../w.gguf",
            "https://huggingface.co/owner/repo/blob/main/w.safetensors",
            "https://huggingface.co/owner",
            "hf.co/owner/repo:../x",
            "hf.co/owner/repo:sub/Q4",
        ] {
            assert!(parse_spec(bad).is_err(), "accepted {bad}");
        }
    }

    #[test]
    fn quantization_tags_pick_exactly_one_matching_file() {
        let files = vec![
            f("m-Q4_K_M.gguf", 1.0),
            f("m-Q6_K.gguf", 1.0),
            f("big/m-Q8_0.gguf", 1.0),
            f("big/m-UD-Q8_0.gguf", 1.0),
        ];
        assert_eq!(select_file(&files, Some("q6_k")).unwrap().path, "m-Q6_K.gguf");
        assert_eq!(select_file(&files, Some("Q4_K_M")).unwrap().path, "m-Q4_K_M.gguf");
        assert_eq!(select_file(&files, Some("m-Q6_K.gguf")).unwrap().path, "m-Q6_K.gguf");
        let ambiguous = select_file(&files, Some("Q8_0")).unwrap_err().to_string();
        assert!(ambiguous.contains("big/m-Q8_0.gguf") && ambiguous.contains("big/m-UD-Q8_0.gguf"), "{ambiguous}");
        assert!(select_file(&files, Some("K_M")).is_err());
        assert!(select_file(&files, Some("Q2_K")).is_err());
    }
}
