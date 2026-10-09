//! Capture subprocess output without delaying cancellation on pipes or descendants.
use anyhow::{Result, bail};
use std::{process::{Command, Output}, sync::atomic::{AtomicBool, Ordering}, time::Duration};

pub fn cancellable_output(command: &mut Command, cancel: &AtomicBool) -> Result<Output> {
    if cancel.load(Ordering::Acquire) { bail!("operation cancelled"); }
    // Files avoid pipe deadlocks and retain any stdin configured by the caller.
    let stdout = tempfile::tempfile()?;
    let stderr = tempfile::tempfile()?;
    command.stdout(stdout.try_clone()?).stderr(stderr.try_clone()?);
    let status = cancellable_status(command, cancel)?;
    use std::io::{Read, Seek, SeekFrom};
    let read = |mut file: std::fs::File| -> Result<Vec<u8>> {
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(bytes)
    };
    Ok(Output { status, stdout: read(stdout)?, stderr: read(stderr)? })
}

/// Preserve caller-owned output files (e.g. a large Git blob spool).
pub fn cancellable_status(command: &mut Command, cancel: &AtomicBool) -> Result<std::process::ExitStatus> {
    if cancel.load(Ordering::Acquire) { bail!("operation cancelled"); }
    #[cfg(unix)] {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn()?;
    loop {
        if cancel.load(Ordering::Acquire) {
            #[cfg(unix)] unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL); }
            #[cfg(windows)] {
                let _ = Command::new("taskkill").args(["/PID", &child.id().to_string(), "/T", "/F"]).output();
            }
            let _ = child.kill();
            let _ = child.wait();
            bail!("operation cancelled");
        }
        if let Some(status) = child.try_wait()? { return Ok(status); }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[cfg(unix)]
    fn cancellation_kills_a_preparation_process_without_waiting_for_sleep() {
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let flag = cancel.clone();
        let started = std::time::Instant::now();
        let worker = std::thread::spawn(move || cancellable_output(Command::new("sh").args(["-c", "sleep 5"]), &flag));
        std::thread::sleep(Duration::from_millis(30));
        cancel.store(true, Ordering::Release);
        assert!(worker.join().unwrap().unwrap_err().to_string().contains("cancelled"));
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
        let output = cancellable_output(Command::new("git").args(["hash-object", "--stdin"]).stdin(input), &AtomicBool::new(false)).unwrap();
        assert!(output.status.success());
        assert_eq!(output.stdout.len(), 41);
    }
}
