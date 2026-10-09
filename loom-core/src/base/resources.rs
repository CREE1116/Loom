//! Process-local admission for model buffers and tools. Reservations are estimates,
//! not OS limits on child processes. No dependency on models or execution policy.
use serde::Serialize;
use std::{
    collections::VecDeque,
    fmt,
    sync::{Arc, Condvar, Mutex, OnceLock},
    time::{Duration, Instant},
};

const MIB: u64 = 1 << 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceKind {
    Resident,
    Inference,
    Tool,
}

impl ResourceKind {
    fn index(self) -> usize {
        match self {
            Self::Resident => 0,
            Self::Inference => 1,
            Self::Tool => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AdmissionError {
    Cancelled,
    TimedOut,
    TooLarge {
        requested: u64,
        resident: u64,
        budget: u64,
    },
}

impl fmt::Display for AdmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("resource wait cancelled"),
            Self::TimedOut => f.write_str("resource wait timed out"),
            Self::TooLarge {
                requested,
                resident,
                budget,
            } => write!(
                f,
                "local memory budget cannot fit this work: {requested} requested + {resident} non-reclaimable resident bytes, budget {budget}; reduce context/workload or configure LOOM_LOCAL_MEMORY_MB"
            ),
        }
    }
}
impl std::error::Error for AdmissionError {}

#[derive(Clone, Debug, Serialize)]
pub struct ResourceSnapshot {
    pub budget_bytes: u64,
    pub resident_bytes: u64,
    /// Idle state/cache that its owner promises can be recomputed, never weights
    /// or state belonging to an active request.
    pub reclaimable_resident_bytes: u64,
    pub inference_bytes: u64,
    pub tool_reserved_bytes: u64,
    pub peak_reserved_bytes: u64,
    pub waiting_inference: usize,
    pub waiting_tools: usize,
    pub waits: u64,
    pub wait_ms: u64,
    /// An observed allocator footprint exceeded its reservation. Never hidden.
    pub estimate_overruns: u64,
    pub over_budget_observations: u64,
    pub reclaims: u64,
    pub reclaimed_bytes: u64,
}

struct Waiter {
    id: u64,
    kind: ResourceKind,
}

#[derive(Default)]
struct State {
    bytes: [u64; 3],
    reclaimable: u64,
    queue: VecDeque<Waiter>,
    next_id: u64,
    peak: u64,
    waits: u64,
    wait_ms: u64,
    overruns: u64,
    over_budget: u64,
    reclaims: u64,
    reclaimed_bytes: u64,
}

impl State {
    fn total(&self) -> u64 {
        self.bytes.iter().fold(0u64, |a, b| a.saturating_add(*b))
    }
    fn peak(&mut self) {
        self.peak = self.peak.max(self.total());
    }
}

type Reclaimer = Arc<dyn Fn(&ResourceGovernor, u64) -> u64 + Send + Sync>;

pub struct ResourceGovernor {
    budget: u64,
    tool_bytes: u64,
    state: Mutex<State>,
    changed: Condvar,
    reclaimer: Mutex<Option<Reclaimer>>,
}

impl ResourceGovernor {
    /// Explicit construction also lets embedded runtimes and tests share a budget
    /// without modifying process-wide environment variables.
    pub fn new(budget_bytes: u64, tool_bytes: u64) -> Arc<Self> {
        Arc::new(Self {
            budget: budget_bytes,
            tool_bytes,
            state: Mutex::new(State::default()),
            changed: Condvar::new(),
            reclaimer: Mutex::new(None),
        })
    }

    pub fn tool_bytes(&self) -> u64 {
        self.tool_bytes
    }

    /// The backend must use a nonblocking ownership check and must not capture a
    /// strong governor/lease (which would form a reference cycle). It is called
    /// outside accounting locks, only when admission actually needs memory.
    pub(crate) fn set_reclaimer(&self, reclaimer: Reclaimer) {
        *self.reclaimer.lock().unwrap_or_else(|e| e.into_inner()) = Some(reclaimer);
    }

    pub(crate) fn notify_idle(&self) {
        self.changed.notify_all();
    }

    pub fn snapshot(&self) -> ResourceSnapshot {
        let s = self.state.lock().unwrap_or_else(|e| e.into_inner());
        ResourceSnapshot {
            budget_bytes: self.budget,
            resident_bytes: s.bytes[0],
            reclaimable_resident_bytes: s.reclaimable,
            inference_bytes: s.bytes[1],
            tool_reserved_bytes: s.bytes[2],
            peak_reserved_bytes: s.peak,
            waiting_inference: s
                .queue
                .iter()
                .filter(|w| w.kind == ResourceKind::Inference)
                .count(),
            waiting_tools: s
                .queue
                .iter()
                .filter(|w| w.kind == ResourceKind::Tool)
                .count(),
            waits: s.waits,
            wait_ms: s.wait_ms,
            estimate_overruns: s.overruns,
            over_budget_observations: s.over_budget,
            reclaims: s.reclaims,
            reclaimed_bytes: s.reclaimed_bytes,
        }
    }

    /// Tool waiters have priority over new inference/load work. Within each
    /// class use FIFO. Existing GPU calls are never preempted or freed early.
    pub fn acquire(
        self: &Arc<Self>,
        kind: ResourceKind,
        bytes: u64,
        mut check: impl FnMut() -> Result<(), AdmissionError>,
    ) -> Result<ResourceLease, AdmissionError> {
        check()?;
        let id = {
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let id = s.next_id;
            s.next_id = s.next_id.wrapping_add(1);
            s.queue.push_back(Waiter { id, kind });
            id
        };
        let mut waiting = Waiting {
            owner: Arc::clone(self),
            id,
            started: None,
        };
        loop {
            // No caller callback runs under the accounting mutex.
            check()?;
            let mut s = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let resident = s.bytes[0].saturating_sub(s.reclaimable);
            if bytes > self.budget || resident > self.budget.saturating_sub(bytes) {
                return Err(AdmissionError::TooLarge {
                    requested: bytes,
                    resident,
                    budget: self.budget,
                });
            }
            let next = s
                .queue
                .iter()
                .find(|w| w.kind == ResourceKind::Tool)
                .or_else(|| s.queue.front())
                .map(|w| w.id);
            if next == Some(id) && s.total() <= self.budget.saturating_sub(bytes) {
                s.bytes[kind.index()] += bytes;
                s.peak();
                return Ok(ResourceLease {
                    owner: Arc::clone(self),
                    kind,
                    bytes,
                    reclaimable: 0,
                });
            }
            if next == Some(id)
                && s.reclaimable > 0
                && s.total() > self.budget.saturating_sub(bytes)
            {
                drop(s);
                let reclaim = self
                    .reclaimer
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                let freed = reclaim.map_or(0, |reclaim| reclaim(self, bytes));
                s = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if freed > 0 {
                    s.reclaims = s.reclaims.saturating_add(1);
                    s.reclaimed_bytes = s.reclaimed_bytes.saturating_add(freed);
                    drop(s);
                    continue;
                }
            }
            if waiting.started.is_none() {
                s.waits = s.waits.saturating_add(1);
                waiting.started = Some(Instant::now());
            }
            // Bounded polling makes cancellation/deadlines independent of a
            // resource release; notifications make normal admission immediate.
            drop(
                self.changed
                    .wait_timeout(s, Duration::from_millis(20))
                    .unwrap_or_else(|e| e.into_inner()),
            );
        }
    }
}

struct Waiting {
    owner: Arc<ResourceGovernor>,
    id: u64,
    started: Option<Instant>,
}
impl Drop for Waiting {
    fn drop(&mut self) {
        let mut s = self.owner.state.lock().unwrap_or_else(|e| e.into_inner());
        s.queue.retain(|w| w.id != self.id);
        if let Some(started) = self.started {
            s.wait_ms = s
                .wait_ms
                .saturating_add(started.elapsed().as_millis().min(u64::MAX as u128) as u64);
        }
        self.owner.changed.notify_all();
    }
}

pub struct ResourceLease {
    owner: Arc<ResourceGovernor>,
    kind: ResourceKind,
    bytes: u64,
    reclaimable: u64,
}

impl ResourceLease {
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn belongs_to(&self, owner: &Arc<ResourceGovernor>) -> bool {
        Arc::ptr_eq(&self.owner, owner)
    }

    pub(crate) fn owned_by(&self, owner: &ResourceGovernor) -> bool {
        std::ptr::eq(Arc::as_ptr(&self.owner), owner)
    }

    pub(crate) fn mark_reclaimable(&mut self, bytes: u64) {
        assert_eq!(self.kind, ResourceKind::Resident);
        let bytes = bytes.min(self.bytes);
        let mut s = self.owner.state.lock().unwrap_or_else(|e| e.into_inner());
        s.reclaimable = s.reclaimable - self.reclaimable + bytes;
        self.reclaimable = bytes;
        self.owner.changed.notify_all();
    }

    /// After synchronous device completion, atomically replace scratch with the
    /// surviving weight/KV footprint. There is no unaccounted admission window.
    pub(crate) fn settle(&mut self, resident: &mut Self, resident_bytes: u64) {
        assert!(Arc::ptr_eq(&self.owner, &resident.owner));
        assert_eq!(resident.kind, ResourceKind::Resident);
        assert_ne!(self.kind, ResourceKind::Resident);
        let mut s = self.owner.state.lock().unwrap_or_else(|e| e.into_inner());
        if resident_bytes > resident.bytes.saturating_add(self.bytes) {
            s.overruns = s.overruns.saturating_add(1);
        }
        s.bytes[self.kind.index()] -= self.bytes;
        s.bytes[0] = s.bytes[0]
            .saturating_sub(resident.bytes)
            .saturating_add(resident_bytes);
        resident.bytes = resident_bytes;
        let reclaimable = resident.reclaimable.min(resident_bytes);
        s.reclaimable -= resident.reclaimable - reclaimable;
        resident.reclaimable = reclaimable;
        self.bytes = 0;
        if s.total() > self.owner.budget {
            s.over_budget = s.over_budget.saturating_add(1);
        }
        s.peak();
        self.owner.changed.notify_all();
    }

    /// Return only memory known to have been released (e.g. reset KV + cache).
    pub(crate) fn shrink_to(&mut self, bytes: u64) -> u64 {
        if bytes >= self.bytes {
            return 0;
        }
        let mut s = self.owner.state.lock().unwrap_or_else(|e| e.into_inner());
        let freed = self.bytes - bytes;
        s.bytes[self.kind.index()] -= freed;
        let reclaimable = self.reclaimable.saturating_sub(freed);
        s.reclaimable -= self.reclaimable - reclaimable;
        self.reclaimable = reclaimable;
        self.bytes = bytes;
        self.owner.changed.notify_all();
        freed
    }
}

impl Drop for ResourceLease {
    fn drop(&mut self) {
        let mut s = self.owner.state.lock().unwrap_or_else(|e| e.into_inner());
        s.bytes[self.kind.index()] -= self.bytes;
        s.reclaimable -= self.reclaimable;
        self.owner.changed.notify_all();
    }
}

/// One budget for the process, shared by local inference, Runner and PTYs.
/// Reserve 25% of physical RAM for OS/UI/other applications by default. This is
/// a static admission budget, not a live system-memory pressure guarantee.
pub fn local_resources() -> Arc<ResourceGovernor> {
    static SHARED: OnceLock<Arc<ResourceGovernor>> = OnceLock::new();
    Arc::clone(SHARED.get_or_init(|| {
        let budget = env_mib("LOOM_LOCAL_MEMORY_MB").unwrap_or_else(|| {
            physical_memory_bytes()
                .map(|n| n - n / 4)
                .unwrap_or(4 << 30)
        });
        ResourceGovernor::new(budget, env_mib("LOOM_TOOL_MEMORY_MB").unwrap_or(1024 * MIB))
    }))
}

fn env_mib(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()?
        .parse::<u64>()
        .ok()?
        .checked_mul(MIB)
        .filter(|n| *n > 0)
}

pub fn physical_memory_bytes() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        let mut bytes = 0u64;
        let mut size = std::mem::size_of_val(&bytes);
        let status = unsafe {
            libc::sysctlbyname(
                c"hw.memsize".as_ptr(),
                (&mut bytes as *mut u64).cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if status == 0 && bytes > 0 {
            return Some(bytes);
        }
    }
    #[cfg(target_os = "linux")]
    {
        let content = std::fs::read_to_string("/proc/meminfo").ok()?;
        let kb = content
            .lines()
            .find_map(|l| l.strip_prefix("MemTotal:"))?
            .split_whitespace()
            .next()?
            .parse::<u64>()
            .ok()?;
        return kb.checked_mul(1024).filter(|n| *n > 0);
    }
    #[cfg(windows)]
    {
        #[repr(C)]
        struct MemoryStatus {
            length: u32,
            load: u32,
            total: u64,
            available: u64,
            page_total: u64,
            page_available: u64,
            virtual_total: u64,
            virtual_available: u64,
            extended_available: u64,
        }
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GlobalMemoryStatusEx(status: *mut MemoryStatus) -> i32;
        }
        let mut status: MemoryStatus = unsafe { std::mem::zeroed() };
        status.length = std::mem::size_of::<MemoryStatus>() as u32;
        if unsafe { GlobalMemoryStatusEx(&mut status) } != 0 && status.total > 0 {
            return Some(status.total);
        }
    }
    #[allow(unreachable_code)]
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scratch_is_promoted_atomically_and_overruns_are_visible() {
        let governor = ResourceGovernor::new(100, 10);
        let mut resident = governor
            .acquire(ResourceKind::Resident, 10, || Ok(()))
            .unwrap();
        let mut scratch = governor
            .acquire(ResourceKind::Inference, 30, || Ok(()))
            .unwrap();
        scratch.settle(&mut resident, 25);
        let snapshot = governor.snapshot();
        assert_eq!(snapshot.resident_bytes, 25);
        assert_eq!(snapshot.inference_bytes, 0);
        assert_eq!(snapshot.peak_reserved_bytes, 40);
        assert_eq!(snapshot.estimate_overruns, 0);
        let mut scratch = governor
            .acquire(ResourceKind::Inference, 10, || Ok(()))
            .unwrap();
        scratch.settle(&mut resident, 110);
        assert_eq!(governor.snapshot().estimate_overruns, 1);
        assert_eq!(governor.snapshot().over_budget_observations, 1);
        resident.shrink_to(10);
        drop(resident);
        drop(scratch);
        assert_eq!(governor.snapshot().resident_bytes, 0);
    }

    #[test]
    fn admission_reclaims_idle_state_without_holding_accounting_locks() {
        let governor = ResourceGovernor::new(100, 35);
        let mut resident = governor
            .acquire(ResourceKind::Resident, 80, || Ok(()))
            .unwrap();
        resident.mark_reclaimable(30);
        let resident = Arc::new(Mutex::new(resident));
        let weak = Arc::downgrade(&resident);
        governor.set_reclaimer(Arc::new(move |owner, _requested| {
            let _snapshot = owner.snapshot(); // would deadlock if callback held the ledger
            let resident = weak.upgrade().unwrap();
            let mut lease = resident.try_lock().unwrap();
            lease.shrink_to(50)
        }));
        let tool = governor.acquire(ResourceKind::Tool, 35, || Ok(())).unwrap();
        let snapshot = governor.snapshot();
        assert_eq!(snapshot.resident_bytes, 50);
        assert_eq!(snapshot.reclaimable_resident_bytes, 0);
        assert_eq!(snapshot.tool_reserved_bytes, 35);
        assert_eq!(snapshot.reclaims, 1);
        assert_eq!(snapshot.reclaimed_bytes, 30);
        drop(tool);
        drop(resident);
        assert_eq!(governor.snapshot().resident_bytes, 0);
        assert!(
            Arc::try_unwrap(governor).is_ok(),
            "reclaimer must not keep its governor alive"
        );
    }

    #[test]
    fn active_state_is_not_reclaimable_and_cannot_deadlock_its_owner() {
        let governor = ResourceGovernor::new(100, 35);
        let mut resident = governor
            .acquire(ResourceKind::Resident, 80, || Ok(()))
            .unwrap();
        resident.mark_reclaimable(30);
        resident.mark_reclaimable(0); // request begins and pins its entire KV
        governor.set_reclaimer(Arc::new(|_, _| {
            panic!("active state must not be reclaimed")
        }));
        assert!(matches!(
            governor.acquire(ResourceKind::Tool, 35, || Ok(())),
            Err(AdmissionError::TooLarge { .. })
        ));
        let decode = governor
            .acquire(ResourceKind::Inference, 20, || Ok(()))
            .unwrap();
        drop(decode);
        drop(resident);
        assert_eq!(governor.snapshot().reclaimable_resident_bytes, 0);
    }

    #[test]
    fn a_busy_idle_owner_can_be_cancelled_without_spinning_or_leaking_a_waiter() {
        let governor = ResourceGovernor::new(100, 35);
        let mut resident = governor
            .acquire(ResourceKind::Resident, 80, || Ok(()))
            .unwrap();
        resident.mark_reclaimable(30);
        governor.set_reclaimer(Arc::new(|_, _| 0)); // nonblocking ownership check failed
        let deadline = Instant::now() + Duration::from_millis(60);
        let result = governor.acquire(ResourceKind::Tool, 35, || {
            if Instant::now() >= deadline {
                Err(AdmissionError::TimedOut)
            } else {
                Ok(())
            }
        });
        assert!(matches!(result, Err(AdmissionError::TimedOut)));
        let snapshot = governor.snapshot();
        assert_eq!(snapshot.waiting_tools, 0);
        assert_eq!(snapshot.waits, 1);
        assert!(snapshot.wait_ms >= 50);
        assert_eq!(snapshot.resident_bytes, 80);
    }

    #[test]
    fn impossible_work_does_not_discard_a_useful_idle_cache() {
        let governor = ResourceGovernor::new(100, 60);
        let mut resident = governor
            .acquire(ResourceKind::Resident, 80, || Ok(()))
            .unwrap();
        resident.mark_reclaimable(30); // 50 bytes of weights cannot be reclaimed
        governor.set_reclaimer(Arc::new(|_, _| {
            panic!("reclaim cannot make this request fit")
        }));
        assert!(matches!(
            governor.acquire(ResourceKind::Tool, 60, || Ok(())),
            Err(AdmissionError::TooLarge { resident: 50, .. })
        ));
        assert_eq!(governor.snapshot().reclaimable_resident_bytes, 30);
    }
}
