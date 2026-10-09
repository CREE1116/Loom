//! Background checkpoint transfers; the UI polls snapshots and never blocks on a download.
use std::sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc};

enum Update { Progress(u64, u64), Stage(String), Done(Result<String, String>) }
#[derive(Default)]
pub struct Transfer {
    pub spec: String,
    /// What runs now: empty while downloading, otherwise e.g. the LMX conversion.
    pub stage: String,
    pub done: u64,
    pub total: u64,
    pub active: bool,
    pub cancelling: bool,
    pub result: Option<Result<String, String>>,
}
pub struct Manager {
    pub transfer: Option<Transfer>,
    cancel: Arc<AtomicBool>,
    rx: Option<mpsc::Receiver<Update>>,
}
impl Default for Manager {
    fn default() -> Self { Self { transfer: None, cancel: Arc::new(AtomicBool::new(false)), rx: None } }
}
impl Manager {
    /// Downloads `spec`; then, when `convert`, installs it as LMX (keeping the GGUF when
    /// `keep_original`).
    pub fn start(&mut self, spec: String, convert: bool, keep_original: bool) -> anyhow::Result<()> {
        anyhow::ensure!(!self.transfer.as_ref().is_some_and(|t| t.active), "이미 다운로드 중입니다.");
        anyhow::ensure!(spec.len() < 4096, "모델 주소가 너무 깁니다.");
        loom_core::models::checkpoints::validate_spec(&spec)?;
        let (tx, rx) = mpsc::channel();
        self.cancel = Arc::new(AtomicBool::new(false));
        let cancel = self.cancel.clone();
        self.transfer = Some(Transfer { spec: spec.clone(), active: true, ..Default::default() });
        self.rx = Some(rx);
        std::thread::spawn(move || {
            let result = std::panic::catch_unwind(|| {
                let path = loom_core::models::checkpoints::pull(&spec, &mut |done, total| {
                    let _ = tx.send(Update::Progress(done, total));
                    !cancel.load(Ordering::Acquire)
                }).map_err(|e| e.to_string())?;
                let is_gguf = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("gguf"));
                if !convert || !is_gguf {
                    return Ok(path.display().to_string());
                }
                let _ = tx.send(Update::Stage("LMX로 변환 중".into()));
                let _ = tx.send(Update::Progress(0, 0));
                loom_core::models::lmx_native_runtime::install_gguf(&path, keep_original, &mut |done, total| {
                    let _ = tx.send(Update::Progress(done, total));
                    !cancel.load(Ordering::Acquire)
                })
                .map(|installed| installed.name)
                .map_err(|e| format!("다운로드는 완료됐지만 LMX 변환에 실패했습니다 (GGUF는 그대로 남아 있습니다): {e}"))
            }).unwrap_or_else(|_| Err("다운로드 중 오류가 발생했습니다.".into()));
            let _ = tx.send(Update::Done(result));
        });
        Ok(())
    }
    pub fn cancel(&mut self) {
        self.cancel.store(true, Ordering::Release);
        if let Some(transfer) = &mut self.transfer { transfer.cancelling = transfer.active; }
    }
    /// Returns (changed, completed successfully).
    pub fn poll(&mut self) -> (bool, bool) {
        let mut changed = false;
        let mut completed = false;
        if let (Some(rx), Some(transfer)) = (&self.rx, &mut self.transfer) {
            while let Ok(update) = rx.try_recv() {
                changed = true;
                match update {
                    Update::Progress(done, total) => { transfer.done = done; transfer.total = total; }
                    Update::Stage(stage) => { transfer.stage = stage; }
                    Update::Done(result) => { completed = result.is_ok(); transfer.active = false; transfer.result = Some(result); }
                }
            }
        }
        (changed, completed)
    }
}
impl Drop for Manager { fn drop(&mut self) { self.cancel(); } }
