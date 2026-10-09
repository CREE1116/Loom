//! LOOM-owned local inference runtime.
//!
//! The first implementation owns a pinned llama.cpp distribution as a sidecar.  The
//! boundary intentionally exposes only "where is the server" and "install/update it"
//! so a future in-process backend can replace the sidecar without changing callers.
use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::Value;
use std::{
    fs,
    io::{Read, Write},
    path::{Path, PathBuf},
};

const LLAMA_RELEASES_API: &str =
    "https://api.github.com/repos/ggml-org/llama.cpp/releases?per_page=12";

#[derive(Clone, Debug, Serialize)]
pub struct RuntimeStatus {
    pub managed: Option<PathBuf>,
    pub external: Option<PathBuf>,
    pub selected: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize)]
pub struct InstalledRuntime {
    pub release: String,
    pub asset: String,
    pub server: PathBuf,
}

pub fn runtime_dir() -> PathBuf {
    std::env::var_os("LOOM_RUNTIME_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .map(|home| PathBuf::from(home).join(".loom/runtime/llama.cpp"))
        })
        .unwrap_or_else(|| PathBuf::from(".loom/runtime/llama.cpp"))
}

fn current_file() -> PathBuf {
    runtime_dir().join("current")
}

pub fn managed_server() -> Option<PathBuf> {
    let marker = fs::read_to_string(current_file()).ok()?;
    let path = PathBuf::from(marker.trim());
    path.is_file().then_some(path)
}

fn external_server() -> Option<PathBuf> {
    let name = if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    };
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

/// Resolve the server used by local inference. Explicit override wins, then LOOM's
/// managed runtime, then an existing developer installation for compatibility.
pub fn server_binary() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("LOOM_LLAMA_SERVER").map(PathBuf::from) {
        if path.is_file() {
            return Ok(path);
        }
        bail!("LOOM_LLAMA_SERVER does not exist: {}", path.display());
    }
    managed_server()
        .or_else(external_server)
        .context("LOOM local runtime is not installed; run `loom runtime install`")
}

pub fn status() -> RuntimeStatus {
    let managed = managed_server();
    let external = external_server();
    let selected = std::env::var_os("LOOM_LLAMA_SERVER")
        .map(PathBuf::from)
        .filter(|path| path.is_file())
        .or_else(|| managed.clone())
        .or_else(|| external.clone());
    RuntimeStatus {
        managed,
        external,
        selected,
    }
}

fn releases() -> Result<Value> {
    super::get_json(
        LLAMA_RELEASES_API,
        &[("User-Agent".into(), "Loom/1.0".into())],
    )
    .map_err(anyhow::Error::from)
}

fn platform_asset_score(name: &str, os: &str, arch: &str) -> Option<u16> {
    let name = name.to_ascii_lowercase();
    if !(name.ends_with(".zip") || name.ends_with(".tar.gz")) || !name.contains("bin-") {
        return None;
    }
    let os_match = match os {
        "macos" => name.contains("macos"),
        "linux" => name.contains("ubuntu") || name.contains("linux"),
        "windows" => name.contains("win"),
        _ => false,
    };
    if !os_match {
        return None;
    }
    let arch_match = match arch {
        "aarch64" => name.contains("arm64") || name.contains("aarch64"),
        "x86_64" => name.contains("x64") || name.contains("x86_64"),
        _ => false,
    };
    if !arch_match {
        return None;
    }
    // Prefer the portable CPU build. macOS uses the normal Metal-capable archive.
    if os != "macos"
        && ["cuda", "cudart", "hip", "sycl", "vulkan", "rocm"]
            .iter()
            .any(|token| name.contains(token))
    {
        return None;
    }
    let mut score = 10;
    if name.contains("cpu") {
        score += 5;
    }
    if (os == "macos" && name.contains("macos"))
        || (os == "linux" && name.contains("ubuntu"))
        || (os == "windows" && name.contains("win"))
    {
        score += 3;
    }
    Some(score)
}

fn choose_asset(release: &Value) -> Option<(String, String)> {
    let mut choices = release["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|asset| {
            let name = asset["name"].as_str()?;
            let url = asset["browser_download_url"].as_str()?;
            let score = platform_asset_score(name, std::env::consts::OS, std::env::consts::ARCH)?;
            Some((score, name.to_owned(), url.to_owned()))
        })
        .collect::<Vec<_>>();
    choices.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    choices
        .into_iter()
        .next()
        .map(|(_, name, url)| (name, url))
}

fn choose_release(releases: &Value) -> Result<(String, String, String)> {
    for release in releases.as_array().into_iter().flatten() {
        let Some(tag) = release["tag_name"].as_str() else {
            continue;
        };
        if let Some((asset, url)) = choose_asset(release) {
            return Ok((tag.to_owned(), asset, url));
        }
    }
    bail!(
        "no recent llama.cpp prebuilt runtime for {}/{}",
        std::env::consts::OS,
        std::env::consts::ARCH
    )
}

fn download(url: &str, destination: &Path, progress: &mut dyn FnMut(u64, u64)) -> Result<()> {
    let response = ureq::get(url)
        .set("User-Agent", "Loom/1.0")
        .timeout(std::time::Duration::from_secs(3600))
        .call()?;
    let total = response
        .header("content-length")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(0);
    let mut input = response.into_reader();
    let mut output = fs::File::create(destination)?;
    let mut buffer = vec![0u8; 1024 * 1024];
    let mut done = 0u64;
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read])?;
        done += read as u64;
        progress(done, total);
    }
    output.flush()?;
    Ok(())
}

fn extract_zip(archive: &Path, destination: &Path) -> Result<()> {
    let file = fs::File::open(archive)?;
    let mut zip = zip::ZipArchive::new(file)?;
    for index in 0..zip.len() {
        let mut entry = zip.by_index(index)?;
        let Some(relative) = entry.enclosed_name() else {
            continue;
        };
        let target = destination.join(relative);
        if entry.is_dir() {
            fs::create_dir_all(&target)?;
            continue;
        }
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut output = fs::File::create(&target)?;
        std::io::copy(&mut entry, &mut output)?;
        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&target, fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(())
}

fn extract_tar_gz(archive: &Path, destination: &Path) -> Result<()> {
    let file = fs::File::open(archive)?;
    let decoder = flate2::read::GzDecoder::new(file);
    let mut archive = tar::Archive::new(decoder);
    archive.unpack(destination)?;
    Ok(())
}

fn extract_archive(archive: &Path, asset: &str, destination: &Path) -> Result<()> {
    if asset.ends_with(".zip") {
        extract_zip(archive, destination)
    } else if asset.ends_with(".tar.gz") {
        extract_tar_gz(archive, destination)
    } else {
        bail!("unsupported runtime archive: {asset}")
    }
}

fn find_server(root: &Path) -> Option<PathBuf> {
    let wanted = if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    };
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(directory).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                pending.push(path);
            } else if path.file_name().is_some_and(|name| name == wanted) {
                return Some(path);
            }
        }
    }
    None
}

fn safe_release_name(tag: &str) -> String {
    tag.chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_') {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

pub fn install(force: bool, progress: &mut dyn FnMut(u64, u64)) -> Result<InstalledRuntime> {
    let releases = releases()?;
    let (tag, asset, url) = choose_release(&releases)?;
    let root = runtime_dir();
    let releases = root.join("releases");
    fs::create_dir_all(&releases)?;
    let destination = releases.join(safe_release_name(&tag));

    if destination.exists() && !force {
        if let Some(server) = find_server(&destination) {
            fs::write(current_file(), server.to_string_lossy().as_bytes())?;
            return Ok(InstalledRuntime {
                release: tag,
                asset,
                server,
            });
        }
    }

    let archive_suffix = if asset.ends_with(".tar.gz") {
        "tar.gz"
    } else {
        "zip"
    };
    let archive = root.join(format!(
        ".llama-runtime-{}.{}",
        std::process::id(),
        archive_suffix
    ));
    let staging = root.join(format!(".installing-{}", std::process::id()));
    let _ = fs::remove_file(&archive);
    let _ = fs::remove_dir_all(&staging);
    download(&url, &archive, progress)?;
    fs::create_dir_all(&staging)?;
    if let Err(error) = extract_archive(&archive, &asset, &staging) {
        let _ = fs::remove_file(&archive);
        let _ = fs::remove_dir_all(&staging);
        return Err(error);
    }
    let server = find_server(&staging).context("llama.cpp archive did not contain llama-server")?;
    let relative = server
        .strip_prefix(&staging)
        .context("runtime server escaped staging directory")?
        .to_owned();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&server, fs::Permissions::from_mode(0o755))?;
    }
    if destination.exists() {
        fs::remove_dir_all(&destination)?;
    }
    fs::rename(&staging, &destination)?;
    let server = destination.join(relative);
    fs::write(current_file(), server.to_string_lossy().as_bytes())?;
    let _ = fs::remove_file(archive);
    Ok(InstalledRuntime {
        release: tag,
        asset,
        server,
    })
}

#[cfg(test)]
mod tests {
    use super::platform_asset_score;

    #[test]
    fn chooses_cpu_archives_for_supported_platforms() {
        assert!(platform_asset_score("llama-b9999-bin-macos-arm64.tar.gz", "macos", "aarch64").is_some());
        assert!(platform_asset_score("llama-b9999-bin-ubuntu-x64.tar.gz", "linux", "x86_64").is_some());
        assert!(platform_asset_score("llama-b9999-bin-win-cpu-x64.zip", "windows", "x86_64").is_some());
        assert!(platform_asset_score("llama-b9999-bin-win-cuda-12.4-x64.zip", "windows", "x86_64").is_none());
        assert!(platform_asset_score("llama-b9999-bin-macos-x64.zip", "macos", "aarch64").is_none());
    }
}
