//! Persistent native PTY sessions. A supervisor owns each child and enforces its
//! deadline even when nobody polls. stdout and stderr share the terminal stream.
use super::sandbox::ExecutionPolicy;
use crate::base::resources::{
    AdmissionError, ResourceGovernor, ResourceKind, ResourceLease, local_resources,
};
use crate::base::{ArtifactRef, ArtifactStore, FileArtifactStore};
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

/// Maximum UTF-8 text bytes returned by a single API call. Raw artifacts are
/// cumulative, unmodified PTY bytes (including echo, ANSI codes and CRLF).
pub const MAX_OUTPUT_BYTES: usize = 32 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TerminalOutput {
    pub session_id: Option<u64>,
    pub exit_code: Option<i32>,
    pub output: String,
    pub artifact: ArtifactRef,
}

pub struct TerminalManager {
    root: PathBuf,
    store: FileArtifactStore,
    policy: ExecutionPolicy,
    sessions: Mutex<HashMap<u64, Session>>,
    next_id: AtomicU64,
    resources: Arc<ResourceGovernor>,
}

struct State {
    raw: tempfile::NamedTempFile,
    pending: Vec<u8>,
    exit_code: Option<i32>,
    error: Option<String>,
}
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
    stop: AtomicBool,
    #[cfg(unix)]
    input: Mutex<File>,
}
struct Session {
    shared: Arc<Shared>,
    supervisor: Option<JoinHandle<()>>,
}
impl Drop for Session {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.changed.notify_all();
        if let Some(thread) = self.supervisor.take() {
            let _ = thread.join();
        }
    }
}

impl TerminalManager {
    pub fn new(root: &Path, store: FileArtifactStore, policy: ExecutionPolicy) -> Self {
        Self::with_resources(root, store, policy, local_resources())
    }

    pub fn with_resources(
        root: &Path,
        store: FileArtifactStore,
        policy: ExecutionPolicy,
        resources: Arc<ResourceGovernor>,
    ) -> Self {
        Self {
            root: root.to_owned(),
            store,
            policy,
            sessions: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(1),
            resources,
        }
    }

    /// Launch `/bin/sh -c command` on a controlling PTY. A running result has a
    /// session id; a completed result has an exit code (negative for signals).
    /// Pre-cancellation and zero timeouts never launch a process.
    pub fn exec(
        &self,
        command: &str,
        yield_ms: u64,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<TerminalOutput> {
        if cancel.load(Ordering::SeqCst) || timeout.is_zero() {
            return Ok(TerminalOutput {
                session_id: None,
                exit_code: Some(-9),
                output: String::new(),
                artifact: self.store.put(&[])?,
            });
        }
        #[cfg(not(unix))]
        {
            let _ = (command, yield_ms);
            bail!("native PTY execution is only supported on Unix");
        }
        #[cfg(unix)]
        {
            let started = Instant::now();
            let root = self
                .root
                .canonicalize()
                .context("opening terminal workspace")?;
            let temp = root.join(".loom/tmp");
            let target = root.join(".loom/target");
            fs::create_dir_all(&temp)?;
            fs::create_dir_all(&target)?;
            let artifact_path = self.store.path(&ArtifactRef {
                id: "0".repeat(64),
                bytes: 0,
            })?;
            let artifacts = artifact_path
                .parent()
                .context("artifact directory")?
                .to_owned();
            let raw = tempfile::NamedTempFile::new_in(&temp)?;
            let (master, slave) = open_pty()?;
            let mut process = std::process::Command::new("/bin/sh");
            process
                .args(["-c", command])
                .current_dir(&root)
                .env("TMPDIR", &temp)
                .env("TMP", &temp)
                .env("TEMP", &temp)
                .env("CARGO_TARGET_DIR", &target)
                .env("TERM", "xterm-256color");
            self.policy
                .configure_command(&mut process, &root, &[temp, target, artifacts])?;
            process
                .stdin(slave.try_clone()?)
                .stdout(slave.try_clone()?)
                .stderr(slave);
            use std::os::unix::process::CommandExt;
            // Only async-signal-safe syscalls after fork. setsid makes the child
            // the process-group leader, then the slave becomes its controlling tty.
            unsafe {
                process.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    if libc::ioctl(0, libc::TIOCSCTTY as _, 0) == -1 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
            let reservation = match self.resources.acquire(
                ResourceKind::Tool,
                self.resources.tool_bytes(),
                || {
                    if cancel.load(Ordering::SeqCst) {
                        Err(AdmissionError::Cancelled)
                    } else if started.elapsed() >= timeout {
                        Err(AdmissionError::TimedOut)
                    } else {
                        Ok(())
                    }
                },
            ) {
                Ok(lease) => lease,
                Err(AdmissionError::Cancelled | AdmissionError::TimedOut) => {
                    return Ok(TerminalOutput {
                        session_id: None,
                        exit_code: Some(-9),
                        output: String::new(),
                        artifact: self.store.put(&[])?,
                    });
                }
                Err(error) => return Err(error.into()),
            };
            let mut child = process.spawn().context("launching sandboxed PTY command")?;
            // Command retains its Stdio files: close the parent's slave copies
            // before reading, otherwise PTY EOF can never arrive.
            drop(process);
            let input = match master.try_clone() {
                Ok(input) => input,
                Err(error) => {
                    kill_group(&mut child);
                    let _ = child.wait();
                    return Err(error.into());
                }
            };
            let shared = Arc::new(Shared {
                state: Mutex::new(State {
                    raw,
                    pending: Vec::new(),
                    exit_code: None,
                    error: None,
                }),
                changed: Condvar::new(),
                stop: AtomicBool::new(false),
                input: Mutex::new(input),
            });
            let worker = shared.clone();
            // Spawn cannot leave an orphan on thread-creation failure: the child
            // guard moves into the closure and kills/reaps on every exit path.
            let owned = ChildGuard {
                child,
                cleaned: false,
                _resources: reservation,
            };
            let supervisor = thread::Builder::new()
                .name("loom-pty".into())
                .spawn(move || supervise(owned, master, worker, started, timeout))?;
            let id = self.next_id.fetch_add(1, Ordering::Relaxed);
            self.sessions.lock().unwrap().insert(
                id,
                Session {
                    shared: shared.clone(),
                    supervisor: Some(supervisor),
                },
            );
            self.collect(id, &shared, yield_ms, cancel)
        }
    }

    /// Write literal terminal input; empty input polls. Text is the bounded tail
    /// of bytes received since the previous call. The artifact is the full stream.
    pub fn write_stdin(
        &self,
        session_id: u64,
        chars: &str,
        yield_ms: u64,
        cancel: &AtomicBool,
    ) -> Result<TerminalOutput> {
        let shared = self.shared(session_id)?;
        if cancel.load(Ordering::SeqCst) {
            shared.stop.store(true, Ordering::SeqCst);
        }
        #[cfg(unix)]
        if !chars.is_empty() && !shared.stop.load(Ordering::SeqCst) {
            let input = shared.input.lock().unwrap();
            let mut input = &*input;
            let mut bytes = chars.as_bytes();
            while !bytes.is_empty() {
                if cancel.load(Ordering::SeqCst) {
                    shared.stop.store(true, Ordering::SeqCst);
                    break;
                }
                if shared.state.lock().unwrap().exit_code.is_some() {
                    break;
                }
                match input.write(bytes) {
                    Ok(0) => bail!("PTY input closed"),
                    Ok(n) => bytes = &bytes[n..],
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(5))
                    }
                    Err(error) if matches!(error.raw_os_error(), Some(libc::EIO)) => break,
                    Err(error) => return Err(error).context("writing PTY input"),
                }
            }
        }
        self.collect(session_id, &shared, yield_ms, cancel)
    }

    /// Kill and reap the process group, retaining its final output for polling.
    pub fn cancel(&self, session_id: u64) -> Result<()> {
        let shared = self.shared(session_id)?;
        shared.stop.store(true, Ordering::SeqCst);
        let mut state = shared.state.lock().unwrap();
        while state.exit_code.is_none() {
            state = shared.changed.wait(state).unwrap();
        }
        Ok(())
    }

    /// Release the session and spool, killing and reaping an unfinished group.
    pub fn close(&self, session_id: u64) -> Result<()> {
        let session = self
            .sessions
            .lock()
            .unwrap()
            .remove(&session_id)
            .with_context(|| format!("unknown terminal session {session_id}"))?;
        drop(session);
        Ok(())
    }

    fn shared(&self, id: u64) -> Result<Arc<Shared>> {
        self.sessions
            .lock()
            .unwrap()
            .get(&id)
            .map(|s| s.shared.clone())
            .with_context(|| format!("unknown terminal session {id}"))
    }

    fn collect(
        &self,
        id: u64,
        shared: &Shared,
        yield_ms: u64,
        cancel: &AtomicBool,
    ) -> Result<TerminalOutput> {
        let started = Instant::now();
        let duration = Duration::from_millis(yield_ms);
        let mut state = shared.state.lock().unwrap();
        loop {
            if cancel.load(Ordering::SeqCst) {
                shared.stop.store(true, Ordering::SeqCst);
            }
            if state.exit_code.is_some() {
                break;
            }
            // A cancellation waits for cleanup, even with yield_ms == 0.
            if !shared.stop.load(Ordering::SeqCst) && started.elapsed() >= duration {
                break;
            }
            let wait = if shared.stop.load(Ordering::SeqCst) {
                Duration::from_millis(10)
            } else {
                duration
                    .saturating_sub(started.elapsed())
                    .min(Duration::from_millis(10))
            };
            state = shared.changed.wait_timeout(state, wait).unwrap().0;
        }
        if let Some(error) = &state.error {
            bail!("terminal supervisor: {error}");
        }
        // The store's slice API requires materializing the snapshot, but the
        // running session retains only a bounded text tail and an on-disk spool.
        let snapshot = state.raw.reopen()?;
        let bytes = snapshot.metadata()?.len();
        let exit_code = state.exit_code;
        let mut output = String::from_utf8_lossy(&state.pending).into_owned();
        if output.len() > MAX_OUTPUT_BYTES {
            let mut start = output.len() - MAX_OUTPUT_BYTES;
            while !output.is_char_boundary(start) {
                start += 1;
            }
            output.drain(..start);
        }
        state.pending.clear();
        drop(state);
        // Read only the committed prefix through an independent file offset.
        // Never hold the supervisor lock during a large artifact snapshot.
        let mut raw = Vec::new();
        snapshot.take(bytes).read_to_end(&mut raw)?;
        Ok(TerminalOutput {
            session_id: if exit_code.is_none() { Some(id) } else { None },
            exit_code,
            output,
            artifact: self.store.put(&raw)?,
        })
    }
}

impl Drop for TerminalManager {
    fn drop(&mut self) {
        // Signal all sessions first so shutdown time does not scale with sessions.
        let sessions = self.sessions.get_mut().unwrap();
        for session in sessions.values() {
            session.shared.stop.store(true, Ordering::SeqCst);
        }
        sessions.clear();
    }
}

#[cfg(unix)]
fn open_pty() -> Result<(File, File)> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let mut master = -1;
    let mut slave = -1;
    let mut size = libc::winsize {
        ws_row: 24,
        ws_col: 80,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut size,
        )
    } == -1
    {
        return Err(std::io::Error::last_os_error()).context("opening native PTY");
    }
    let master = unsafe { File::from_raw_fd(master) };
    let slave = unsafe { File::from_raw_fd(slave) };
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } == -1 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    if flags == -1
        || unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1
    {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok((master, slave))
}

#[cfg(unix)]
struct ChildGuard {
    child: std::process::Child,
    cleaned: bool,
    _resources: ResourceLease,
}
#[cfg(unix)]
impl ChildGuard {
    fn terminate(&mut self) {
        if !self.cleaned {
            kill_group(&mut self.child);
            self.cleaned = true;
        }
    }
}
#[cfg(unix)]
impl Drop for ChildGuard {
    fn drop(&mut self) {
        self.terminate();
        let _ = self.child.wait();
    }
}
#[cfg(unix)]
fn kill_group(child: &mut std::process::Child) {
    let session = child.id() as i32;
    // Interactive shells give foreground/background jobs their own groups.
    // Freeze the leader's group, then kill every process still in this PTY
    // session, including orphaned jobs whose shell has already exited.
    unsafe {
        libc::kill(-session, libc::SIGSTOP);
    }
    for pid in session_pids() {
        if pid > 0 && unsafe { libc::getsid(pid) } == session {
            unsafe {
                libc::kill(pid, libc::SIGKILL);
            }
        }
    }
    unsafe {
        libc::kill(-session, libc::SIGKILL);
    }
    let _ = child.kill();
}

#[cfg(target_os = "macos")]
fn session_pids() -> Vec<i32> {
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) }.max(0) as usize;
    let mut pids = vec![0i32; count.saturating_add(1024)];
    let count = unsafe {
        libc::proc_listallpids(
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<i32>()) as i32,
        )
    }
    .max(0) as usize;
    pids.truncate(count.min(pids.len()));
    pids
}

#[cfg(target_os = "linux")]
fn session_pids() -> Vec<i32> {
    fs::read_dir("/proc")
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse::<i32>().ok())
        .collect()
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
fn session_pids() -> Vec<i32> {
    Vec::new()
}

#[cfg(unix)]
fn supervise(
    mut child: ChildGuard,
    mut master: File,
    shared: Arc<Shared>,
    started: Instant,
    timeout: Duration,
) {
    use std::os::unix::process::ExitStatusExt;
    let mut run = || -> Result<i32> {
        let mut chunk = [0u8; 8192];
        let mut status = None;
        loop {
            if status.is_none()
                && (shared.stop.load(Ordering::SeqCst) || started.elapsed() >= timeout)
            {
                child.terminate();
                status = Some(child.child.wait()?);
            }
            if status.is_none()
                && let Some(exit) = child.child.try_wait()?
            {
                // Kill descendants even on normal shell exit; they must not
                // retain a PTY or outlive their owning command.
                child.terminate();
                status = Some(exit);
            }
            let mut eof = false;
            let mut drained = false;
            // Limit each drain pass so an infinite output producer cannot starve
            // cancellation or deadline checks.
            for _ in 0..64 {
                match master.read(&mut chunk) {
                    Ok(0) => {
                        eof = true;
                        break;
                    }
                    Ok(n) => {
                        let mut state = shared.state.lock().unwrap();
                        state.raw.as_file_mut().write_all(&chunk[..n])?;
                        state.pending.extend_from_slice(&chunk[..n]);
                        let excess = state.pending.len().saturating_sub(MAX_OUTPUT_BYTES);
                        if excess > 0 {
                            state.pending.drain(..excess);
                        }
                        shared.changed.notify_all();
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        drained = true;
                        break;
                    }
                    Err(error) if error.raw_os_error() == Some(libc::EIO) => {
                        eof = true;
                        break;
                    }
                    Err(error) => return Err(error.into()),
                }
            }
            if let Some(status) = status
                && (eof || drained)
            {
                return Ok(status
                    .code()
                    .unwrap_or_else(|| -status.signal().unwrap_or(9)));
            }
            thread::sleep(Duration::from_millis(5));
        }
    };
    let result = run();
    // Cleanup before advertising completion, including every I/O error path.
    drop(child);
    let mut state = shared.state.lock().unwrap();
    match result {
        Ok(code) => state.exit_code = Some(code),
        Err(error) => {
            state.error = Some(format!("{error:#}"));
            state.exit_code = Some(-9);
        }
    }
    shared.changed.notify_all();
}
