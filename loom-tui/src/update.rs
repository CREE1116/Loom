//! Cross-platform release updates. Downloads are verified before replacing the installation.
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::Deserialize;
use sha2::{Digest, Sha256};
#[cfg(windows)]
use std::process::{Command, Stdio};
use std::{
    ffi::OsString,
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
    sync::mpsc::{self, Receiver},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const REPO: &str = "CREE1116/Loom";
const INTERVAL: u64 = 24 * 60 * 60;
const MAX_DOWNLOAD: u64 = 250 * 1024 * 1024;

#[derive(Debug, Clone, Copy)]
struct Package {
    name: &'static str,
    binary: &'static str,
}

impl Package {
    fn for_target(os: &str, arch: &str) -> Option<Self> {
        let (name, binary) = match (os, arch) {
            ("macos", "aarch64") => ("loom-darwin-aarch64", "loom"),
            ("linux", "x86_64") => ("loom-linux-x86_64", "loom"),
            ("windows", "x86_64") => ("loom-windows-x86_64", "loom.exe"),
            _ => return None,
        };
        Some(Self { name, binary })
    }

    fn current() -> Option<Self> {
        Self::for_target(std::env::consts::OS, std::env::consts::ARCH)
    }

    fn asset(self) -> String {
        format!("{}.tar.gz", self.name)
    }
}

#[derive(Deserialize)]
struct Release {
    tag_name: String,
}

pub fn cli(args: &[OsString]) -> Result<()> {
    if matches!(args, [flag] if flag == "--help" || flag == "-h") {
        println!("Usage: loom update [--check]\n\nInstall the latest verified release, or check without installing.\nSet LOOM_AUTO_UPDATE=0 to disable background updates.");
        return Ok(());
    }
    let check_only = match args {
        [] => false,
        [flag] if flag == "--check" => true,
        _ => bail!("usage: loom update [--check]"),
    };
    let _lock = lock()?;
    match update(check_only)? {
        Some(message) => println!("{message}"),
        None => println!("Loom {} is up to date.", env!("CARGO_PKG_VERSION")),
    }
    Ok(())
}

/// Check once a day without slowing launch; only recognized release installations update themselves.
pub fn auto_check() -> Option<Receiver<Option<String>>> {
    if std::env::var("LOOM_AUTO_UPDATE").is_ok_and(|v| v == "0" || v.eq_ignore_ascii_case("false"))
        || installed_path().is_none()
    {
        return None;
    }
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let result = (|| -> Result<Option<String>> {
            let _lock = match lock() {
                Ok(lock) => lock,
                Err(_) => return Ok(None), // another Loom process is checking
            };
            let stamp = state_dir()?.join("update-checked");
            let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
            if fs::read_to_string(&stamp)
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok())
                .is_some_and(|last| now >= last && now - last < INTERVAL)
            {
                return Ok(None);
            }
            // Rate-limit failed checks too. A manual `loom update` always checks immediately.
            fs::write(&stamp, now.to_string())?;
            update(false)
        })();
        let message = match result {
            Ok(message) => message,
            Err(error) => Some(format!(
                "Loom 자동 업데이트 실패: {error:#}. `loom update`로 재시도할 수 있어요."
            )),
        };
        let _ = tx.send(message);
    });
    Some(rx)
}

fn state_dir() -> Result<PathBuf> {
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .context("HOME/USERPROFILE is not set")?;
    let path = PathBuf::from(home).join(".loom");
    fs::create_dir_all(&path)?;
    Ok(path)
}

fn lock() -> Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(state_dir()?.join("update.lock"))?;
    file.try_lock_exclusive()
        .context("another Loom update is already running")?;
    Ok(file)
}

fn version(tag: &str) -> Option<(u64, u64, u64)> {
    let mut parts = tag.strip_prefix('v').unwrap_or(tag).split('.');
    let parsed = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(parsed)
}

fn installed_path() -> Option<PathBuf> {
    if cfg!(feature = "cuda") {
        return None;
    } // Never replace a custom CUDA build with a CPU package.
    let package = Package::current()?;
    let path = std::env::current_exe().ok()?;
    (path.file_name().is_some_and(|name| name == package.binary)
        && path.parent()?.file_name().is_some_and(|name| name == "bin")
        && !fs::symlink_metadata(&path).ok()?.file_type().is_symlink())
    .then_some(path)
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(4))
        .timeout_read(Duration::from_secs(20))
        .user_agent("Loom-updater")
        .build()
}

fn latest() -> Result<Option<String>> {
    let release: Release = agent()
        .get(&format!(
            "https://api.github.com/repos/{REPO}/releases/latest"
        ))
        .set("Accept", "application/vnd.github+json")
        .call()
        .context("cannot check GitHub releases")?
        .into_json()
        .context("invalid release metadata")?;
    let remote = version(&release.tag_name).context("invalid stable release version")?;
    let current = version(env!("CARGO_PKG_VERSION")).context("invalid installed version")?;
    Ok((remote > current).then_some(release.tag_name))
}

fn update(check_only: bool) -> Result<Option<String>> {
    let Some(tag) = latest()? else {
        return Ok(None);
    };
    if check_only {
        return Ok(Some(format!(
            "Loom {tag} available (installed: {}). Run `loom update` to install.",
            env!("CARGO_PKG_VERSION")
        )));
    }
    let Some(binary) = installed_path() else {
        return Ok(Some(format!(
            "Loom {tag} available. Automatic installation supports macOS ARM64, Linux x86_64, and Windows x86_64, installed under PREFIX/bin. Use the installer from README.md."
        )));
    };
    install(&agent(), &tag, &binary)?;
    Ok(Some(completion_message(&tag, cfg!(windows))))
}

pub fn completion_message(tag: &str, pending: bool) -> String {
    let status = if pending {
        "다운로드와 검증 완료. Loom 종료 후 교체됩니다. 그다음 다시 실행하세요."
    } else {
        "설치 완료. 실행 중인 Loom을 종료(Ctrl+Q)하고 다시 실행하면 적용됩니다."
    };
    format!("Loom {tag}: {status}")
}

fn fetch(agent: &ureq::Agent, url: &str, path: &Path) -> Result<()> {
    let response = agent
        .get(url)
        .call()
        .with_context(|| format!("download failed: {url}"))?;
    let mut input = response.into_reader().take(MAX_DOWNLOAD + 1);
    let mut output = File::create(path)?;
    let size = io::copy(&mut input, &mut output)?;
    if size > MAX_DOWNLOAD {
        bail!("release asset exceeds size limit")
    }
    output.flush()?;
    Ok(())
}

fn verify_archive(archive: &Path, checksum: &str, asset: &str) -> Result<()> {
    let mut fields = checksum.split_whitespace();
    let expected = fields.next().context("missing checksum")?;
    if expected.len() != 64
        || !expected.bytes().all(|b| b.is_ascii_hexdigit())
        || fields.next() != Some(asset)
        || fields.next().is_some()
    {
        bail!("invalid release checksum file")
    }
    let mut digest = Sha256::new();
    io::copy(&mut File::open(archive)?, &mut digest)?;
    if format!("{:x}", digest.finalize()) != expected.to_ascii_lowercase() {
        bail!("release checksum mismatch; existing Loom was not changed")
    }
    Ok(())
}

fn unpack_verified(archive: &Path, stage: &Path, package: Package) -> Result<()> {
    let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(File::open(archive)?));
    let mut bytes = 0u64;
    for item in tar.entries()? {
        let mut entry = item?;
        bytes = bytes
            .checked_add(entry.size())
            .context("archive size overflow")?;
        if bytes > MAX_DOWNLOAD * 8 {
            bail!("expanded release exceeds size limit")
        }
        let path = entry.path()?.into_owned();
        if !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
            || !path.starts_with(package.name)
        {
            bail!("unsafe release archive path")
        }
        let dest = stage.join(&path);
        if entry.header().entry_type().is_dir() {
            fs::create_dir_all(&dest)?;
        } else if entry.header().entry_type().is_file() {
            fs::create_dir_all(dest.parent().context("missing parent directory")?)?;
            entry.unpack(&dest)?;
        } else {
            bail!("unsupported release archive entry")
        }
    }
    Ok(())
}

fn install(agent: &ureq::Agent, tag: &str, binary: &Path) -> Result<()> {
    let package = Package::current().context("unsupported platform")?;
    let bin_dir = binary.parent().context("missing bin directory")?;
    let prefix = bin_dir.parent().context("missing installation prefix")?;
    let stage = tempfile::tempdir_in(prefix).context("cannot stage update beside installation")?;
    let asset = package.asset();
    let archive = stage.path().join(&asset);
    let url = format!("https://github.com/{REPO}/releases/download/{tag}/{asset}");
    fetch(agent, &url, &archive)?;
    let checksum = stage.path().join("checksum.sha256");
    fetch(agent, &format!("{url}.sha256"), &checksum)?;
    verify_archive(&archive, &fs::read_to_string(checksum)?, &asset)?;
    unpack_verified(&archive, stage.path(), package)?;

    let unpacked = stage.path().join(package.name);
    let incoming = unpacked.join("bin").join(package.binary);
    let libs = unpacked.join("lib/loom");
    if !incoming.is_file() {
        bail!("incomplete release package")
    }
    // Validate the packaged binary while its library is still alongside it.
    let output = std::process::Command::new(&incoming)
        .arg("--version")
        .output()
        .context("cannot run downloaded Loom")?;
    if !output.status.success()
        || String::from_utf8_lossy(&output.stdout).trim()
            != format!("loom-cli {}", tag.trim_start_matches('v'))
    {
        bail!("downloaded Loom did not pass version check")
    }

    if cfg!(windows) {
        return schedule_windows_update(&incoming, binary, stage);
    }

    // Linux is binary-only. On macOS the MLX dylib, if present, lives next to the binary.
    if !libs.exists() {
        replace_binary(&incoming, binary)?;
        return Ok(());
    }
    let lib_parent = prefix.join("lib");
    fs::create_dir_all(&lib_parent)?;
    let current_lib = lib_parent.join("loom");
    let backup = stage.path().join("old-lib");
    let had_libs = current_lib.exists();
    if had_libs {
        fs::rename(&current_lib, &backup).context("cannot back up existing runtime")?;
    }
    if let Err(error) = fs::rename(&libs, &current_lib) {
        if had_libs {
            let _ = fs::rename(&backup, &current_lib);
        }
        return Err(error).context("cannot install new runtime");
    }
    if let Err(error) = replace_binary(&incoming, binary) {
        let _ = fs::remove_dir_all(&current_lib);
        if had_libs {
            let _ = fs::rename(&backup, &current_lib);
        }
        return Err(error).context("cannot install new executable");
    }
    Ok(())
}

/// Rename on the same volume atomically switches the command; existing Unix processes keep their inode.
fn replace_binary(incoming: &Path, destination: &Path) -> Result<()> {
    fs::rename(incoming, destination).context("could not atomically replace Loom executable")
}

#[cfg(windows)]
fn ps_literal(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "''"))
}

#[cfg(windows)]
fn schedule_windows_update(incoming: &Path, binary: &Path, stage: tempfile::TempDir) -> Result<()> {
    spawn_windows_helper(incoming, binary, stage, std::process::id())?;
    Ok(())
}

#[cfg(windows)]
fn spawn_windows_helper(
    incoming: &Path,
    binary: &Path,
    stage: tempfile::TempDir,
    parent: u32,
) -> Result<std::process::Child> {
    // Windows forbids replacing a running .exe. An independent PowerShell process
    // waits for this process to exit, then applies the already verified image.
    let staged = stage.keep();
    let script = staged.join("apply.ps1");
    let backup = staged.join("loom-backup.exe");
    let source = ps_literal(incoming);
    let dest = ps_literal(binary);
    let backup = ps_literal(&backup);
    let root = ps_literal(&staged);
    let contents = format!(
        r#"
$ErrorActionPreference = 'Stop'
$source = {source}
$destination = {dest}
$backup = {backup}
for ($i = 0; $i -lt 86400; $i++) {{
    if (-not (Get-Process -Id {parent} -ErrorAction SilentlyContinue)) {{ break }}
    Start-Sleep -Milliseconds 250
}}
if (-not (Get-Process -Id {parent} -ErrorAction SilentlyContinue)) {{
    for ($attempt = 0; $attempt -lt 120; $attempt++) {{
        try {{
            [IO.File]::Replace($source, $destination, $backup)
            Remove-Item -LiteralPath {root} -Recurse -Force -ErrorAction SilentlyContinue
            exit 0
        }} catch {{ Start-Sleep -Seconds 1 }}
    }}
}}
exit 1
"#
    );
    let mut file = File::create(&script)?;
    file.write_all(b"\xef\xbb\xbf")?; // Windows PowerShell reads non-ASCII script paths as UTF-8.
    file.write_all(contents.as_bytes())?;
    file.flush()?;
    drop(file);
    use std::os::windows::process::CommandExt;
    Command::new("powershell.exe")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-File",
        ])
        .arg(&script)
        .creation_flags(0x08000000) // CREATE_NO_WINDOW
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("could not launch Windows update helper")
}

#[cfg(not(windows))]
fn schedule_windows_update(_: &Path, _: &Path, _: tempfile::TempDir) -> Result<()> {
    bail!("Windows update helper is unavailable")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compares_only_complete_stable_versions() {
        assert_eq!(version("v0.1.3"), Some((0, 1, 3)));
        assert!(version("v0.1.3-rc1").is_none());
        assert!(version("garbage").is_none());
        assert!(version("v0.1.3.1").is_none());
        assert!(version("v0.1.12") > version("0.1.9"));
    }

    #[test]
    fn platform_asset_names_match_packager() {
        assert_eq!(
            Package::for_target("macos", "aarch64").unwrap().asset(),
            "loom-darwin-aarch64.tar.gz"
        );
        assert_eq!(
            Package::for_target("linux", "x86_64").unwrap().asset(),
            "loom-linux-x86_64.tar.gz"
        );
        assert_eq!(
            Package::for_target("windows", "x86_64").unwrap().asset(),
            "loom-windows-x86_64.tar.gz"
        );
        assert!(Package::for_target("windows", "aarch64").is_none());
    }

    #[test]
    fn refuses_checksum_mismatch_and_wrong_asset_name() {
        let dir = tempfile::tempdir().unwrap();
        let asset = Package::for_target("macos", "aarch64").unwrap().asset();
        let archive = dir.path().join(&asset);
        fs::write(&archive, b"test payload").unwrap();
        let correct = format!("{:x}  {asset}", Sha256::digest(b"test payload"));
        verify_archive(&archive, &correct, &asset).unwrap();
        assert!(
            verify_archive(
                &archive,
                &format!(
                    "{}  other.tar.gz",
                    correct.split_whitespace().next().unwrap()
                ),
                &asset
            )
            .is_err()
        );
        fs::write(&archive, b"changed payload").unwrap();
        assert!(verify_archive(&archive, &correct, &asset).is_err());
    }

    #[test]
    fn rejects_archive_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let archive = dir.path().join("bad.tar.gz");
        let out = File::create(&archive).unwrap();
        let gz = flate2::write::GzEncoder::new(out, flate2::Compression::default());
        let mut tar = tar::Builder::new(gz);
        let mut header = tar::Header::new_gnu();
        header.set_entry_type(tar::EntryType::Symlink);
        header.set_size(0);
        header.set_mode(0o777);
        header.set_cksum();
        tar.append_link(
            &mut header,
            "loom-darwin-aarch64/lib/loom/bad",
            "../../escape",
        )
        .unwrap();
        tar.finish().unwrap();
        drop(tar);
        assert!(
            unpack_verified(
                &archive,
                dir.path(),
                Package::for_target("macos", "aarch64").unwrap()
            )
            .is_err()
        );
    }

    #[test]
    fn failed_replacement_preserves_existing_executable() {
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("loom");
        fs::write(&binary, b"original").unwrap();
        assert!(replace_binary(&dir.path().join("missing"), &binary).is_err());
        assert_eq!(fs::read(binary).unwrap(), b"original");
    }

    #[cfg(windows)]
    #[test]
    fn windows_helper_waits_for_exit_and_replaces_file_with_quoted_paths() {
        let dir = tempfile::tempdir().unwrap();
        let prefix = dir.path().join("Loom's test 폴더");
        fs::create_dir(&prefix).unwrap();
        let binary = prefix.join("loom.exe");
        fs::write(&binary, b"original").unwrap();
        let stage = tempfile::tempdir_in(&prefix).unwrap();
        let incoming = stage.path().join("new.exe");
        fs::write(&incoming, b"updated").unwrap();
        let mut parent = Command::new("cmd.exe")
            .args(["/C", "ping -n 3 127.0.0.1 >NUL"])
            .spawn()
            .unwrap();
        let mut helper = spawn_windows_helper(&incoming, &binary, stage, parent.id()).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(fs::read(&binary).unwrap(), b"original");
        parent.wait().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(15);
        loop {
            if let Some(status) = helper.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            if std::time::Instant::now() > deadline {
                let _ = helper.kill();
                panic!("update helper did not finish");
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        assert_eq!(fs::read(binary).unwrap(), b"updated");
    }
}
