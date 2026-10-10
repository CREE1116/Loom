//! Protected local credentials; descriptor-relative I/O and stable locks.
use super::*;

fn legacy_host(host: &str) -> bool {
    host.strip_prefix("loom-").is_some_and(|suffix| {
        suffix.len() == 43
            && URL_SAFE_NO_PAD
                .decode(suffix)
                .is_ok_and(|bytes| bytes.len() == 32 && URL_SAFE_NO_PAD.encode(bytes) == suffix)
    })
}

fn supported_host(host: &str) -> bool {
    if let Some(uuid) = host.strip_prefix("urn:uuid:") {
        let bytes = uuid.as_bytes();
        return bytes.len() == 36
            && bytes.iter().enumerate().all(|(index, byte)| {
                if [8, 13, 18, 23].contains(&index) {
                    *byte == b'-'
                } else {
                    byte.is_ascii_hexdigit()
                }
            })
            && bytes[14] == b'4'
            && matches!(bytes[19], b'8' | b'9' | b'a' | b'b' | b'A' | b'B');
    }
    // Existing cryptographic identifiers are opaque here; never replace their
    // key material with a randomly generated installation identity.
    ["urn:ietf:params:oauth:jwk-thumbprint:", "did:key:"]
        .iter()
        .any(|prefix| {
            host.strip_prefix(prefix).is_some_and(|suffix| {
                !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_graphic())
            })
        })
}

// Descriptor-relative operations prevent symlink substitution between checking
// and opening/renaming. Locks are stable separate inodes, never renamed.
#[cfg(unix)]
pub(super) struct Store {
    directory: std::fs::File,
}
#[cfg(unix)]
impl Store {
    pub(super) fn open(root: &Path, create: bool) -> Result<Self> {
        use std::os::unix::{
            ffi::OsStrExt,
            fs::MetadataExt,
            io::{AsRawFd, FromRawFd},
        };
        let path = if root.is_absolute() {
            root.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|_| anyhow::anyhow!("credential root unavailable"))?
                .join(root)
        };
        let fd = unsafe {
            libc::open(
                c"/".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        ensure!(fd >= 0, "credential root unavailable");
        let mut directory = unsafe { std::fs::File::from_raw_fd(fd) };
        for component in path.components() {
            match component {
                std::path::Component::RootDir | std::path::Component::CurDir => continue,
                std::path::Component::Normal(name) => {
                    let name = std::ffi::CString::new(name.as_bytes())
                        .map_err(|_| anyhow::anyhow!("unsafe credential path"))?;
                    let parent = directory.as_raw_fd();
                    let mut fd = unsafe {
                        libc::openat(
                            parent,
                            name.as_ptr(),
                            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                        )
                    };
                    if fd < 0
                        && create
                        && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound
                    {
                        let status = unsafe { libc::mkdirat(parent, name.as_ptr(), 0o700) };
                        ensure!(
                            status == 0
                                || std::io::Error::last_os_error().kind()
                                    == std::io::ErrorKind::AlreadyExists,
                            "cannot create credential directory"
                        );
                        fd = unsafe {
                            libc::openat(
                                parent,
                                name.as_ptr(),
                                libc::O_RDONLY
                                    | libc::O_DIRECTORY
                                    | libc::O_NOFOLLOW
                                    | libc::O_CLOEXEC,
                            )
                        };
                    }
                    ensure!(fd >= 0, "credential path missing or contains a symlink");
                    directory = unsafe { std::fs::File::from_raw_fd(fd) };
                    let metadata = directory
                        .metadata()
                        .map_err(|_| anyhow::anyhow!("unsafe credential directory"))?;
                    // Root-owned sticky system temp directories are safe ancestors.
                    ensure!(
                        metadata.mode() & 0o022 == 0
                            || (metadata.uid() == 0
                                && metadata.mode() & u32::from(libc::S_ISVTX) != 0),
                        "credential ancestor is publicly writable"
                    );
                }
                _ => bail!("unsafe credential path"),
            }
        }
        let metadata = directory
            .metadata()
            .map_err(|_| anyhow::anyhow!("unsafe credential root"))?;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o022 == 0,
            "unsafe credential root ownership or permissions"
        );
        let parent = directory.as_raw_fd();
        if create {
            let status = unsafe { libc::mkdirat(parent, c"chatgpt-oauth".as_ptr(), 0o700) };
            ensure!(
                status == 0
                    || std::io::Error::last_os_error().kind() == std::io::ErrorKind::AlreadyExists,
                "cannot create ChatGPT credential directory"
            );
        }
        let fd = unsafe {
            libc::openat(
                parent,
                c"chatgpt-oauth".as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        ensure!(fd >= 0, "ChatGPT credential directory missing or unsafe");
        let directory = unsafe { std::fs::File::from_raw_fd(fd) };
        let metadata = directory
            .metadata()
            .map_err(|_| anyhow::anyhow!("unsafe credential directory"))?;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() } && metadata.mode() & 0o777 == 0o700,
            "ChatGPT credential directory must be private (0700)"
        );
        Ok(Self { directory })
    }
    pub(super) fn file(&self, name: &str, create: bool) -> Result<Option<std::fs::File>> {
        use std::os::unix::{
            fs::MetadataExt,
            io::{AsRawFd, FromRawFd},
        };
        let name = std::ffi::CString::new(name)
            .map_err(|_| anyhow::anyhow!("unsafe credential filename"))?;
        let flags = libc::O_RDWR | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
        let mut fd = unsafe { libc::openat(self.directory.as_raw_fd(), name.as_ptr(), flags) };
        if fd < 0
            && create
            && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound
        {
            // Separate exclusive creation from opening an existing stable lock file.
            // Concurrent first opens with O_CREAT can return ENOENT on macOS.
            fd = unsafe {
                libc::openat(
                    self.directory.as_raw_fd(),
                    name.as_ptr(),
                    flags | libc::O_CREAT | libc::O_EXCL,
                    0o600,
                )
            };
            if fd < 0
                && matches!(
                    std::io::Error::last_os_error().kind(),
                    std::io::ErrorKind::AlreadyExists | std::io::ErrorKind::NotFound
                )
            {
                fd = unsafe { libc::openat(self.directory.as_raw_fd(), name.as_ptr(), flags) };
            }
        }
        if fd < 0
            && !create
            && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound
        {
            return Ok(None);
        }
        ensure!(fd >= 0, "credential file missing or unsafe");
        let file = unsafe { std::fs::File::from_raw_fd(fd) };
        let metadata = file
            .metadata()
            .map_err(|_| anyhow::anyhow!("unsafe credential file"))?;
        ensure!(
            metadata.is_file()
                && metadata.uid() == unsafe { libc::geteuid() }
                && metadata.mode() & 0o777 == 0o600
                && metadata.nlink() == 1,
            "credential file must be owned, unlinked and private (0600)"
        );
        Ok(Some(file))
    }
    pub(super) fn lock(&self, id: &str) -> Result<std::fs::File> {
        safe_id(id)?;
        let file = self
            .file(&format!("{id}.lock"), true)?
            .ok_or_else(|| anyhow::anyhow!("credential lock unavailable"))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match fs2::FileExt::try_lock_exclusive(&file) {
                Ok(()) => return Ok(file),
                Err(e)
                    if (e.kind() == std::io::ErrorKind::WouldBlock
                        || e.raw_os_error() == fs2::lock_contended_error().raw_os_error())
                        && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(25))
                }
                _ => bail!("ChatGPT session busy; retry later"),
            }
        }
    }
    pub(super) fn read(&self, name: &str) -> Result<Option<Vec<u8>>> {
        let Some(file) = self.file(name, false)? else {
            return Ok(None);
        };
        let mut bytes = Vec::new();
        file.take(MAX_JSON + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| anyhow::anyhow!("credential file unreadable"))?;
        ensure!(bytes.len() as u64 <= MAX_JSON, "credential file too large");
        Ok(Some(bytes))
    }
    pub(super) fn atomic(&self, name: &str, bytes: &[u8]) -> Result<()> {
        use std::os::unix::io::{AsRawFd, FromRawFd};
        // Every caller holds the stable session (or host) lock. Recover a previous
        // durable replacement before writing or attempting another token rotation.
        let pending_name = format!(".{name}.pending");
        let incomplete = self
            .read(&pending_name)?
            .is_some_and(|bytes| serde_json::from_slice::<Value>(&bytes).is_err());
        if !incomplete {
            self.recover(name)?;
        }
        let _ = self.file(name, false)?;
        let temporary_name = if incomplete {
            format!(".pending-{}", random()?)
        } else {
            pending_name.clone()
        };
        let mut temporary = std::ffi::CString::new(temporary_name)
            .map_err(|_| anyhow::anyhow!("credential save failed"))?;
        let destination =
            std::ffi::CString::new(name).map_err(|_| anyhow::anyhow!("credential save failed"))?;
        let dir = self.directory.as_raw_fd();
        let fd = unsafe {
            libc::openat(
                dir,
                temporary.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600,
            )
        };
        ensure!(fd >= 0, "cannot create private credential file");
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        let mut durable = false;
        let result = (|| -> Result<()> {
            ensure!(
                unsafe { libc::fchmod(fd, 0o600) } == 0,
                "cannot protect credential file"
            );
            file.write_all(bytes)
                .map_err(|_| anyhow::anyhow!("credential save failed"))?;
            file.sync_all()
                .map_err(|_| anyhow::anyhow!("credential save failed"))?;
            durable = true;
            if incomplete {
                let pending = std::ffi::CString::new(pending_name)
                    .map_err(|_| anyhow::anyhow!("credential replacement failed"))?;
                let _ = self.file(
                    pending
                        .to_str()
                        .map_err(|_| anyhow::anyhow!("credential replacement failed"))?,
                    false,
                )?;
                ensure!(
                    unsafe { libc::renameat(dir, temporary.as_ptr(), dir, pending.as_ptr()) } == 0,
                    "credential replacement failed"
                );
                temporary = pending;
            }
            self.directory
                .sync_all()
                .map_err(|_| anyhow::anyhow!("credential replacement durability not confirmed"))?;
            let _ = self.file(name, false)?;
            ensure!(
                unsafe { libc::renameat(dir, temporary.as_ptr(), dir, destination.as_ptr()) } == 0,
                "credential replacement failed"
            );
            self.directory
                .sync_all()
                .map_err(|_| anyhow::anyhow!("credential save durability not confirmed"))?;
            Ok(())
        })();
        if result.is_err() && !durable {
            unsafe {
                libc::unlinkat(dir, temporary.as_ptr(), 0);
            }
        }
        result
    }
    pub(super) fn recover(&self, name: &str) -> Result<()> {
        use std::os::unix::io::AsRawFd;
        let pending = format!(".{name}.pending");
        let Some(bytes) = self.read(&pending)? else {
            return Ok(());
        };
        // An interrupted/incomplete replacement blocks use of stale credentials.
        // Never silently fall back to a possibly consumed rotating refresh token.
        serde_json::from_slice::<Value>(&bytes)
            .map_err(|_| anyhow::anyhow!("interrupted ChatGPT credential save; sign in again"))?;
        let _ = self.file(name, false)?;
        let source = std::ffi::CString::new(pending)
            .map_err(|_| anyhow::anyhow!("credential recovery failed"))?;
        let destination = std::ffi::CString::new(name)
            .map_err(|_| anyhow::anyhow!("credential recovery failed"))?;
        let dir = self.directory.as_raw_fd();
        ensure!(
            unsafe { libc::renameat(dir, source.as_ptr(), dir, destination.as_ptr()) } == 0,
            "credential recovery failed"
        );
        self.directory
            .sync_all()
            .map_err(|_| anyhow::anyhow!("credential recovery durability not confirmed"))?;
        Ok(())
    }
    pub(super) fn load(&self, id: &str) -> Result<Option<Credentials>> {
        safe_id(id)?;
        let name = format!("{id}.json");
        let incomplete = self
            .read(&format!(".{name}.pending"))?
            .is_some_and(|bytes| serde_json::from_slice::<Value>(&bytes).is_err());
        if !incomplete {
            self.recover(&name)?;
        }
        let mut credentials: Option<Credentials> = self
            .read(&name)?
            .map(|bytes| {
                serde_json::from_slice(&bytes)
                    .map_err(|_| anyhow::anyhow!("invalid ChatGPT credential file"))
            })
            .transpose()?;
        if incomplete && let Some(c) = &mut credentials {
            c.check_registration()?;
            // An explicit login can reuse the account mapping, but requests cannot
            // use the old tokens after an interrupted rotation. Keep the marker
            // until a completely written replacement is ready to publish.
            c.clear_tokens();
        }
        Ok(credentials)
    }
    pub(super) fn save(&self, id: &str, c: &Credentials) -> Result<()> {
        safe_id(id)?;
        let bytes = serde_json::to_vec(c)
            .map_err(|_| anyhow::anyhow!("credential serialization failed"))?;
        self.atomic(&format!("{id}.json"), &bytes)
    }
    pub(super) fn host(&self) -> Result<String> {
        let _lock = self.lock("host")?;
        self.recover("host-id.json")?;
        if let Some(bytes) = self.read("host-id.json")? {
            let host: String = serde_json::from_slice(&bytes)
                .map_err(|_| anyhow::anyhow!("invalid ChatGPT host ID"))?;
            if !legacy_host(&host) {
                ensure!(supported_host(&host), "invalid ChatGPT host ID");
                return Ok(host);
            }
            // Repair only the exact format previously generated by Loom. Account
            // records retain their original host/client binding; authorize rejects
            // a mismatch before browser sign-in or code exchange.
        }
        let mut bytes = [0u8; 16];
        getrandom::getrandom(&mut bytes)
            .map_err(|_| anyhow::anyhow!("secure randomness unavailable"))?;
        bytes[6] = (bytes[6] & 0x0f) | 0x40;
        bytes[8] = (bytes[8] & 0x3f) | 0x80;
        let hex: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        let host = format!(
            "urn:uuid:{}-{}-{}-{}-{}",
            &hex[..8],
            &hex[8..12],
            &hex[12..16],
            &hex[16..20],
            &hex[20..]
        );
        self.atomic(
            "host-id.json",
            &serde_json::to_vec(&host).map_err(|_| anyhow::anyhow!("host ID save failed"))?,
        )?;
        Ok(host)
    }
}

#[cfg(not(unix))]
pub(super) struct Store;
#[cfg(not(unix))]
impl Store {
    pub(super) fn open(_: &Path, _: bool) -> Result<Self> {
        bail!("ChatGPT sign-in needs a supported private credential store on this platform")
    }
    pub(super) fn host(&self) -> Result<String> {
        bail!("private credential store unsupported")
    }
    pub(super) fn lock(&self, _: &str) -> Result<()> {
        bail!("private credential store unsupported")
    }
    pub(super) fn load(&self, _: &str) -> Result<Option<Credentials>> {
        bail!("private credential store unsupported")
    }
    pub(super) fn save(&self, _: &str, _: &Credentials) -> Result<()> {
        bail!("private credential store unsupported")
    }
}
