//! Capture subprocess output without delaying cancellation on pipes or descendants.
use anyhow::{Result, bail};
use std::{
    process::{Command, Output},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

pub fn cancellable_output(command: &mut Command, cancel: &AtomicBool) -> Result<Output> {
    Ok(cancellable_output_bounded(command, cancel, usize::MAX, Duration::MAX)?.0)
}

/// Spool to disk, then retain at most `limit` bytes per stream in memory.
/// The flag distinguishes an incomplete preview from an exhaustive result.
pub fn cancellable_output_bounded(
    command: &mut Command,
    cancel: &AtomicBool,
    limit: usize,
    timeout: Duration,
) -> Result<(Output, bool)> {
    if cancel.load(Ordering::Acquire) {
        bail!("operation cancelled");
    }
    // Files avoid pipe deadlocks and retain any stdin configured by the caller.
    let stdout = tempfile::tempfile()?;
    let stderr = tempfile::tempfile()?;
    command
        .stdout(stdout.try_clone()?)
        .stderr(stderr.try_clone()?);
    let status = cancellable_status_with_timeout(command, cancel, timeout)?;
    use std::io::{Read, Seek, SeekFrom};
    let read = |mut file: std::fs::File| -> Result<(Vec<u8>, bool)> {
        file.seek(SeekFrom::Start(0))?;
        let truncated = file.metadata()?.len() > limit as u64;
        let mut bytes = Vec::new();
        file.take(limit as u64).read_to_end(&mut bytes)?;
        Ok((bytes, truncated))
    };
    let (stdout, stdout_truncated) = read(stdout)?;
    let (stderr, stderr_truncated) = read(stderr)?;
    Ok((
        Output {
            status,
            stdout,
            stderr,
        },
        stdout_truncated || stderr_truncated,
    ))
}

/// Preserve caller-owned output files (e.g. a large Git blob spool).
pub fn cancellable_status(
    command: &mut Command,
    cancel: &AtomicBool,
) -> Result<std::process::ExitStatus> {
    cancellable_status_with_timeout(command, cancel, Duration::MAX)
}

fn cancellable_status_with_timeout(
    command: &mut Command,
    cancel: &AtomicBool,
    timeout: Duration,
) -> Result<std::process::ExitStatus> {
    if cancel.load(Ordering::Acquire) {
        bail!("operation cancelled");
    }
    if timeout.is_zero() {
        bail!("operation timed out");
    }
    let started = std::time::Instant::now();
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    loop {
        let cancelled = cancel.load(Ordering::Acquire);
        if cancelled || started.elapsed() >= timeout {
            #[cfg(unix)]
            unsafe {
                libc::kill(-(child.id() as i32), libc::SIGKILL);
            }
            #[cfg(windows)]
            {
                let _ = Command::new("taskkill")
                    .args(["/PID", &child.id().to_string(), "/T", "/F"])
                    .output();
            }
            let _ = child.kill();
            let _ = child.wait();
            bail!(if cancelled {
                "operation cancelled"
            } else {
                "operation timed out"
            });
        }
        if let Some(status) = child.try_wait()? {
            return Ok(status);
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(unix)]
    fn bounded_capture_terminates_a_stalled_process_at_its_deadline() {
        let started = std::time::Instant::now();
        let error = cancellable_output_bounded(
            Command::new("sh").args(["-c", "sleep 5"]),
            &AtomicBool::new(false),
            4096,
            Duration::from_millis(30),
        )
        .unwrap_err();
        assert!(error.to_string().contains("timed out"));
        assert!(started.elapsed() < Duration::from_secs(1));
    }

    #[test]
    #[cfg(unix)]
    fn bounded_capture_keeps_previews_without_pipe_deadlock() {
        let (output, truncated) = cancellable_output_bounded(
            Command::new("sh").args([
                "-c",
                "head -c 2097152 /dev/zero; head -c 2097152 /dev/zero >&2",
            ]),
            &AtomicBool::new(false),
            4096,
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(output.status.success());
        assert!(truncated);
        assert_eq!(output.stdout, vec![0; 4096]);
        assert_eq!(output.stderr, vec![0; 4096]);
        let (output, truncated) = cancellable_output_bounded(
            Command::new("sh").args(["-c", "printf short"]),
            &AtomicBool::new(false),
            4096,
            Duration::from_secs(5),
        )
        .unwrap();
        assert!(!truncated);
        assert_eq!(output.stdout, b"short");
    }

    #[test]
    #[cfg(unix)]
    fn cancellation_kills_a_preparation_process_without_waiting_for_sleep() {
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let started = std::time::Instant::now();
        let worker = std::thread::spawn(move || {
            cancellable_output(Command::new("sh").args(["-c", "sleep 5"]), &flag)
        });
        std::thread::sleep(Duration::from_millis(30));
        cancel.store(true, Ordering::Release);
        assert!(
            worker
                .join()
                .unwrap()
                .unwrap_err()
                .to_string()
                .contains("cancelled")
        );
        assert!(started.elapsed() < Duration::from_millis(350));
    }
    #[test]
    fn capture_preserves_large_piped_input() {
        use std::io::Write;
        let mut input = tempfile::tempfile().unwrap();
        let bytes = vec![b'x'; 200_000];
        input.write_all(&bytes).unwrap();
        use std::io::{Seek, SeekFrom};
        input.seek(SeekFrom::Start(0)).unwrap();
        let output = cancellable_output(
            Command::new("git")
                .args(["hash-object", "--stdin"])
                .stdin(input),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 41);
    }
}
