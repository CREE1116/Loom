//! Version adapters shared by the TUI and paired benchmark entry point.
//! Each Core owns execution and approval; this module translates protocols only.
use anyhow::{Result, ensure};
use loom_core::models::{self as m, ModelPort};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Version {
    Loom,
}
impl Version {
    pub fn label(self) -> &'static str {
        match self {
            Self::Loom => "Loom",
        }
    }
}
#[derive(Clone)]
pub struct Settings {
    pub model: String,
    pub readonly: bool,
    pub review_model: String,
    pub checks: Vec<String>,
    pub steps: usize,
    pub seconds: u64,
    pub attempts: usize,
    pub state: Option<PathBuf>,
    pub reference: Option<String>,
    pub effort: Option<String>,
    pub policy: loom_core::effects::ExecutionPolicy,
    pub call_seconds: u64,
    pub split_experiment: bool,
    pub edit_handoff: bool,
    pub write_files: Vec<String>,
    pub plan: bool,
    pub input_control: loom_core::agent::InputControl,
    pub history: Vec<m::Message>,
    /// SKILL.md files attached to the request.
    pub skills: Vec<PathBuf>,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            model: "auto".into(),
            readonly: false,
            review_model: "auto".into(),
            checks: vec![],
            steps: 12,
            seconds: 300,
            attempts: 3,
            state: None,
            reference: None,
            effort: None,
            policy: Default::default(),
            call_seconds: 45,
            split_experiment: false,
            edit_handoff: false,
            write_files: vec![],
            plan: false,
            input_control: Default::default(),
            history: vec![],
            skills: vec![],
        }
    }
}
pub fn state_directory(settings: &Settings) -> PathBuf {
    settings.state.clone().unwrap_or_else(|| {
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".loom")
    })
}
#[derive(Clone, Debug)]
pub struct Progress {
    pub operation: String,
    pub summary: String,
    pub detail: String,
    pub event: Option<crate::backend::activity_events::Event>,
}
pub type Observer = Arc<dyn Fn(Progress) + Send + Sync>;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Call {
    #[serde(default)]
    pub purpose: String,
    pub requested: String,
    pub served: Option<String>,
    pub input: Option<u64>,
    pub cached: Option<u64>,
    pub output: Option<u64>,
    pub ms: u64,
    pub error: Option<String>,
    #[serde(default)]
    pub start_ms: Option<u64>,
    #[serde(default)]
    pub end_ms: Option<u64>,
    #[serde(default)]
    pub first_text_ms: Option<u64>,
    #[serde(default)]
    pub stop: Option<String>,
    #[serde(default)]
    pub tool_calls: usize,
    #[serde(default)]
    pub text_chars: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Outcome {
    pub version: Version,
    pub id: String,
    pub goal: String,
    pub status: String,
    pub summary: String,
    pub blocker: Option<String>,
    pub remaining: String,
    pub diff: String,
    pub files: Vec<String>,
    #[serde(default)]
    pub sources: Vec<String>,
    pub records: PathBuf,
    pub elapsed_ms: u64,
    pub calls: usize,
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cached: Option<u64>,
    pub attempts: usize,
    pub validation: Vec<Value>,
    pub plan_mode: String,
    pub plan_reason: String,
    pub groups: Vec<Vec<String>>,
    pub model_calls: Vec<Call>,
    #[serde(default)]
    pub activity_report: String,
    #[serde(default)]
    pub events: Vec<crate::backend::activity_events::Event>,
}
impl Outcome {
    pub fn pending(&self) -> bool {
        self.status == "AwaitingApproval"
    }
    pub fn success(&self) -> bool {
        matches!(
            self.status.as_str(),
            "Accepted" | "Applied" | "Grounded" | "Inspected" | "Answered"
        )
    }
    pub fn cancelled(&self) -> bool {
        self.status == "Cancelled"
    }
}
struct ObservedPort {
    inner: Arc<dyn ModelPort>,
    selected: Mutex<(String, Option<String>)>,
    calls: Mutex<Vec<Call>>,
    cancel: Arc<AtomicBool>,
    started: Instant,
    seconds: u64,
    call_seconds: u64,
    limit: usize,
    reference: Option<String>,
    effort: Option<String>,
    review_model: String,
    observe: Observer,
}
impl ObservedPort {
    fn new(
        inner: Arc<dyn ModelPort>,
        settings: &Settings,
        cancel: Arc<AtomicBool>,
        observe: Observer,
    ) -> Self {
        Self {
            inner,
            selected: Mutex::new((
                if settings.model == "auto" {
                    String::new()
                } else {
                    settings.model.clone()
                },
                None,
            )),
            calls: Mutex::new(vec![]),
            cancel,
            started: Instant::now(),
            seconds: limit_seconds(settings.seconds),
            call_seconds: settings.call_seconds,
            limit: limit_steps(settings.steps),
            reference: settings.reference.clone(),
            effort: settings.effort.clone(),
            review_model: settings.review_model.clone(),
            observe,
        }
    }
    fn request(
        &self,
        need: m::Need,
        request: &m::Request,
        observe: &mut dyn FnMut(m::StreamEvent),
        independent_review: bool,
    ) -> std::result::Result<m::Served, m::ProviderError> {
        let remaining = Duration::from_secs(self.seconds).saturating_sub(self.started.elapsed());
        if self.cancel.load(Ordering::SeqCst) {
            return Err(m::ProviderError::new(
                m::ErrorKind::Cancelled,
                "model request cancelled",
            ));
        }
        if remaining.is_zero() {
            return Err(m::ProviderError::new(
                m::ErrorKind::TimedOut,
                "execution deadline reached",
            ));
        }
        let mut request = request.clone();
        let selected = if independent_review {
            (
                if self.review_model == "auto" {
                    String::new()
                } else {
                    self.review_model.clone()
                },
                None,
            )
        } else {
            self.selected.lock().unwrap().clone()
        };
        if independent_review {
            request.model = selected.0.clone();
        }
        if !selected.0.is_empty() {
            request.model = selected.0;
        }
        request.gateway_connection = selected.1;
        request.cancel = Some(self.cancel.clone());
        if self.effort.is_some() { request.reasoning_effort = self.effort.clone(); }
        if let Some(reference) = &self.reference {
            let system = request.system.get_or_insert_with(String::new);
            system.push_str(&format!("\nBenchmark execution reference: {reference}. This identifier does not change the task."));
        }
        request.timeout = Some(request.timeout.unwrap_or(Duration::from_secs(self.call_seconds))
            .min(Duration::from_secs(self.call_seconds)).min(remaining));
        // Capability preference, not a hard Flagship requirement. Profiles with
        // quota/cooldown/auth failures, excessive cost or insufficient context
        // cannot make an otherwise usable model inaccessible.
        let min_tier = if !request.model.is_empty() {
            m::Tier::Local
        } else if independent_review {
            let tokens = m::router::estimate_tokens(&request);
            self.inner
                .model_profiles()
                .into_iter()
                .filter(|p| {
                    p.unavailable_reason.is_none()
                        && p.candidate.cost <= m::cost_ceiling()
                        && (!need.tools || p.candidate.tools)
                        && p.candidate.tier >= need.min_tier
                        && p.candidate.context.is_none_or(|limit| limit >= tokens)
                })
                .map(|p| p.candidate.tier)
                .max()
                .unwrap_or(need.min_tier)
        } else {
            need.min_tier
        };
        let number = {
            let mut calls = self.calls.lock().unwrap();
            if calls.len() >= self.limit {
                return Err(m::ProviderError::new(
                    m::ErrorKind::BadRequest,
                    "shared model-call limit reached",
                ));
            }
            let number = calls.len();
            calls.push(Call {
                purpose: if independent_review { "review" } else { "work" }.into(),
                requested: request.model.clone(),
                served: None,
                input: None,
                cached: None,
                output: None,
                ms: 0,
                error: None,
                start_ms: Some(self.started.elapsed().as_millis() as u64),
                end_ms: None,
                first_text_ms: None,
                stop: None,
                tool_calls: 0,
                text_chars: 0,
            });
            number
        };
        let started = Instant::now();
        let mut first_text_ms = None;
        enum StreamMessage {
            Event(m::StreamEvent),
            Done(std::result::Result<m::Served, m::ProviderError>),
        }
        let inner = self.inner.clone();
        let stream_request = request.clone();
        let stream_need = m::Need {
            min_tier,
            tools: need.tools,
            max_cost: m::cost_ceiling(),
        };
        let (stream_tx, stream_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let event_tx = stream_tx.clone();
            let result = inner.complete_stream(stream_need, &stream_request, &mut |event| {
                let _ = event_tx.send(StreamMessage::Event(event));
            });
            let _ = stream_tx.send(StreamMessage::Done(result));
        });
        let result = loop {
            if self.cancel.load(Ordering::Acquire) {
                break Err(m::ProviderError::new(
                    m::ErrorKind::Cancelled,
                    "model request cancelled",
                ));
            }
            match stream_rx.recv_timeout(Duration::from_millis(10)) {
                Ok(StreamMessage::Event(event)) => {
                    if matches!(&event,m::StreamEvent::TextDelta(text) if !text.is_empty())
                        && first_text_ms.is_none()
                    {
                        first_text_ms = Some(started.elapsed().as_millis() as u64);
                    }
                    observe(event);
                }
                Ok(StreamMessage::Done(result)) => break result,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    break Err(m::ProviderError::new(
                        m::ErrorKind::Transient,
                        "model stream thread ended",
                    ));
                }
            }
        };
        let mut calls = self.calls.lock().unwrap();
        let call = &mut calls[number];
        call.ms = started.elapsed().as_millis() as u64;
        call.end_ms = Some(self.started.elapsed().as_millis() as u64);
        call.first_text_ms = first_text_ms;
        match &result {
            Ok(served) => {
                let response = &served.response;
                let id = if let Some(route) = &response.route {
                    format!(
                        "{}/{}/{}",
                        served.candidate.endpoint,
                        route,
                        response.model.trim_start_matches(&format!("{route}/"))
                    )
                } else {
                    format!("{}/{}", served.candidate.endpoint, response.model)
                };
                call.served = Some(id.clone());
                call.stop = Some(format!("{:?}", response.stop));
                call.tool_calls = response.tool_calls().count();
                call.text_chars = response.text().chars().count();
                call.input = response.usage.input;
                call.cached = response.usage.cached_input;
                call.output = response.usage.output;
                let mut selected = self.selected.lock().unwrap();
                if !independent_review {
                    selected.1 = response.gateway_connection.clone();
                }
                (self.observe)(Progress {
                    operation: "model".into(),
                    summary: format!("{} · input {:?} / output {:?}", id, call.input, call.output),
                    detail: serde_json::to_string(response).unwrap_or_default(),
                    event: None,
                });
            }
            Err(error) => {
                call.error = Some(error.to_string());
                if !independent_review {
                    self.selected.lock().unwrap().1 = None;
                }
            }
        }
        result
    }
}
impl ModelPort for ObservedPort {
    fn complete(
        &self,
        need: m::Need,
        request: &m::Request,
    ) -> std::result::Result<m::Served, m::ProviderError> {
        self.request(need, request, &mut |_| {}, false)
    }
    fn complete_stream(
        &self,
        need: m::Need,
        request: &m::Request,
        observe: &mut dyn FnMut(m::StreamEvent),
    ) -> std::result::Result<m::Served, m::ProviderError> {
        self.request(need, request, observe, false)
    }
}
#[derive(Clone, Copy, Debug)]
pub enum Intent {
    Auto,
    Inspect,
    Analyze,
    Edit,
    Answer,
}
pub fn parse(text: &str) -> (Intent, String) {
    for (prefix, intent) in [
        (":edit ", Intent::Edit),
        (":inspect ", Intent::Inspect),
        (":analyze ", Intent::Analyze),
        (":chat ", Intent::Answer),
    ] {
        if let Some(goal) = text.strip_prefix(prefix) {
            return (intent, goal.trim().into());
        }
    }
    (Intent::Auto, text.trim().into())
}

pub fn execute(
    version: Version,
    root: &Path,
    goal: &str,
    intent: Intent,
    settings: Settings,
    model: Arc<dyn ModelPort>,
    cancel: Arc<AtomicBool>,
    observe: Observer,
) -> Result<Outcome> {
    execute_with_index(
        version, root, goal, intent, settings, model, cancel, observe, None,
    )
}

pub fn execute_with_index(
    version: Version,
    root: &Path,
    goal: &str,
    intent: Intent,
    settings: Settings,
    model: Arc<dyn ModelPort>,
    cancel: Arc<AtomicBool>,
    observe: Observer,
    _session_context: Option<String>,
) -> Result<Outcome> {
    ensure!(
        !(settings.readonly
            || settings.policy.sandbox == loom_core::effects::SandboxMode::ReadOnly)
            || !matches!(intent, Intent::Edit),
        "read-only mode cannot edit"
    );
    let mut settings = settings;
    let goal = if let Some(goal) = goal.strip_prefix("/plan ") {
        ensure!(
            version == Version::Loom,
            "/plan orchestration requires the Loom backend"
        );
        settings.plan = true;
        goal.trim()
    } else {
        goal
    };
    let port = ObservedPort::new(model, &settings, cancel.clone(), observe.clone());
    let started = Instant::now();
    let mut core = loom_core::agent::Core::open(root)?;
    let observer = observe.clone();
    let task = core.execute(
        goal,
        match intent {
            Intent::Edit => loom_core::agent::Intent::Edit,
            Intent::Inspect | Intent::Analyze => loom_core::agent::Intent::Inspect,
            Intent::Answer | Intent::Auto => loom_core::agent::Intent::Auto,
        },
        loom_core::agent::Options {
            history: settings.history.clone(),
            plan: settings.plan,
            read_only: settings.readonly,
            checks: settings.checks.clone(),
            steps: limit_steps(settings.steps),
            seconds: limit_seconds(settings.seconds),
            model: (settings.model != "auto").then(|| settings.model.clone()),
            reasoning_effort: settings.effort.clone(),
            policy: settings.policy,
            call_seconds: settings.call_seconds,
            model_attempts: settings.attempts,
            control: settings.input_control.clone(),
            skills: settings.skills.clone(),
            ..Default::default()
        },
        &port,
        cancel.clone(),
        &move |event| {
            observer(Progress {
                operation: "loom".into(),
                summary: event.target.clone(),
                detail: serde_json::to_string(&event).unwrap(),
                event: None,
            });
        },
    )?;
    let mut outcome = from_v5(&task);
    outcome.elapsed_ms = started.elapsed().as_millis() as u64;
    outcome.model_calls = port.calls.lock().unwrap().clone();
    outcome.calls = outcome.model_calls.len();
    let sum =
        |get: fn(&Call) -> Option<u64>| outcome.model_calls.iter().map(get).sum::<Option<u64>>();
    outcome.input = sum(|c| c.input);
    outcome.output = sum(|c| c.output);
    outcome.cached = sum(|c| c.cached);
    save_surface(root, &settings, &outcome)?;
    Ok(outcome)
}
fn base(
    version: Version,
    id: String,
    goal: String,
    status: String,
    summary: String,
    records: PathBuf,
) -> Outcome {
    let good = matches!(
        status.as_str(),
        "Accepted" | "Applied" | "Grounded" | "Inspected" | "Answered" | "AwaitingApproval"
    );
    Outcome {
        version,
        id,
        remaining: if good { String::new() } else { goal.clone() },
        goal,
        status,
        blocker: if good { None } else { Some(summary.clone()) },
        summary,
        records,
        diff: String::new(),
        files: vec![],
        sources: vec![],
        elapsed_ms: 0,
        calls: 0,
        input: None,
        output: None,
        cached: None,
        attempts: 0,
        validation: vec![],
        plan_mode: "single".into(),
        plan_reason: String::new(),
        groups: vec![],
        model_calls: vec![],
        activity_report: String::new(),
        events: vec![],
    }
}
pub fn approve(
    version: Version,
    root: &Path,
    id: &str,
    settings: &Settings,
    cancel: &AtomicBool,
) -> Result<Outcome> {
    approve_with_progress(version, root, id, settings, cancel, Arc::new(|_| {}))
}
pub fn approve_with_progress(
    version: Version,
    root: &Path,
    id: &str,
    settings: &Settings,
    cancel: &AtomicBool,
    observe: Observer,
) -> Result<Outcome> {
    ensure!(
        !settings.readonly && settings.policy.sandbox != loom_core::effects::SandboxMode::ReadOnly,
        "read-only mode cannot approve changes"
    );
    let mut core = loom_core::agent::Core::open(root)?;
    let task = core.approve(id, cancel, &move |event| {
        observe(Progress {
            operation: "loom".into(),
            summary: event.target.clone(),
            detail: serde_json::to_string(&event).unwrap(),
            event: None,
        });
    })?;
    let mut outcome = from_v5(&task);
    if let Some(saved) = surface_history(version, root, settings)?
        .into_iter()
        .find(|o| o.id == id)
    {
        restore_observations(&mut outcome, &saved);
    }
    save_surface(root, settings, &outcome)?;
    Ok(outcome)
}
#[derive(Serialize, Deserialize)]
struct SurfaceRecord {
    source: PathBuf,
    outcome: Outcome,
}
fn surface_directory(version: Version, root: &Path, _settings: &Settings) -> PathBuf {
    match version {
        Version::Loom => root.join(".loom/surface"),
    }
}
fn save_surface(root: &Path, settings: &Settings, outcome: &Outcome) -> Result<()> {
    ensure!(
        outcome
            .id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "invalid record id"
    );
    let dir = surface_directory(outcome.version, root, settings);
    fs::create_dir_all(&dir)?;
    let file = dir.join(format!("{}.json", outcome.id));
    let temporary = dir.join(format!("{}.{}.tmp", outcome.id, std::process::id()));
    fs::write(
        &temporary,
        serde_json::to_vec(&SurfaceRecord {
            source: root.canonicalize()?,
            outcome: outcome.clone(),
        })?,
    )?;
    fs::rename(temporary, file)?;
    Ok(())
}
fn surface_history(version: Version, root: &Path, settings: &Settings) -> Result<Vec<Outcome>> {
    let dir = surface_directory(version, root, settings);
    let source = root.canonicalize()?;
    let mut records = vec![];
    for directory in [dir, root.join(".loom/v5/surface")] {
        let Ok(entries) = fs::read_dir(directory) else {
            continue;
        };
        for entry in entries {
            let path = entry?.path();
            if path.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            if let Ok(bytes) = fs::read(path) {
                if let Ok(record) = serde_json::from_slice::<SurfaceRecord>(&bytes) {
                    if record.source == source
                        && !records.iter().any(|saved: &Outcome| saved.id == record.outcome.id)
                    {
                        records.push(record.outcome);
                    }
                }
            }
        }
    }
    Ok(records)
}
fn restore_observations(current: &mut Outcome, saved: &Outcome) {
    current.calls = saved.calls;
    current.input = saved.input;
    current.output = saved.output;
    current.cached = saved.cached;
    current.model_calls = saved.model_calls.clone();
    current.elapsed_ms = saved.elapsed_ms;
    current.events = saved.events.clone();
}
pub fn history(version: Version, root: &Path, settings: &Settings) -> Result<Vec<Outcome>> {
    let mut history: Vec<Outcome> = loom_core::agent::Core::open(root)?
        .history()?
        .iter()
        .map(from_v5)
        .collect();
    for saved in surface_history(version, root, settings)? {
        if let Some(current) = history.iter_mut().find(|o| o.id == saved.id) {
            restore_observations(current, &saved);
        } else if saved.status == "Answered" {
            history.push(saved);
        }
    }
    history.sort_by(|a, b| a.id.cmp(&b.id));
    // Never hide an actionable proposal when ordinary history is capped.
    if history.len() > 40 {
        let cutoff = history.len() - 40;
        history = history
            .into_iter()
            .enumerate()
            .filter_map(|(i, o)| (i >= cutoff || o.pending()).then_some(o))
            .collect();
    }
    Ok(history)
}
pub fn reject(version: Version, root: &Path, id: &str, _settings: &Settings) -> Result<()> {
    match version {
        Version::Loom => {
            loom_core::agent::Core::open(root)?.reject(id)?;
        }
    }
    Ok(())
}

fn from_v5(t: &loom_core::agent::Task) -> Outcome {
    let mut o = base(
        Version::Loom,
        t.id.clone(),
        t.goal.clone(),
        t.status.as_str().into(),
        t.summary.clone(),
        t.records.clone(),
    );
    o.diff = String::from_utf8_lossy(&t.patch.diff).into_owned();
    o.files = t
        .patch
        .files
        .iter()
        .map(|f| f.path.to_string_lossy().into_owned())
        .collect();
    o.calls = t.calls;
    o.input = t.usage.input;
    o.output = t.usage.output;
    o.cached = t.usage.cached_input;
    o.elapsed_ms = t.elapsed_ms;
    o.attempts = 1;
    o.validation = t
        .validation
        .iter()
        .map(|r| serde_json::to_value(r).unwrap())
        .collect();
    o.plan_mode = if t.delegated { "dag" } else { "single" }.into();
    o.plan_reason = if t.delegated {
        "의존 관계에 따라 작업 분할"
    } else {
        ""
    }
    .into();
    o
}

/// A configured limit of 0 means none; the core takes a bound too large to reach.
pub fn limit_steps(steps: usize) -> usize {
    if steps == 0 { 1_000_000 } else { steps }
}
pub fn limit_seconds(seconds: u64) -> u64 {
    if seconds == 0 { 30 * 24 * 3600 } else { seconds }
}
