//! Cheapest-first model routing with health tracking and a usage ledger.
//!
//! Candidates are tried in cost order: local (free, private) → free API tiers →
//! subscription gateways → paid with a known price → paid with an unknown price. Within
//! a cost class the smallest tier that satisfies the request goes first. Failures change
//! endpoint health: bad credentials disable the endpoint, exhausted quota cools it down
//! for long, rate limits for `retry-after`, transient errors briefly. Every attempt is
//! recorded so per-model statistics come from real calls.
use super::*;
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    io::Write,
    path::PathBuf,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Tier {
    Local,
    Small,
    Medium,
    Flagship,
}

/// Marginal cost class, cheapest first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum CostClass {
    Local,
    Free,
    Subscription,
    Paid,
    Unknown,
}

#[derive(Clone, Debug, Serialize)]
pub struct Candidate {
    pub endpoint: String,
    pub model: String,
    pub tier: Tier,
    pub cost: CostClass,
    /// USD per million input/output tokens when known.
    pub price: Option<(f64, f64)>,
    pub tools: bool,
    pub context: Option<u64>,
}

impl Candidate {
    pub fn usd(&self, usage: &Usage) -> Option<f64> {
        match self.cost {
            CostClass::Local | CostClass::Free | CostClass::Subscription => Some(0.0),
            _ => {
                let (input, output) = self.price?;
                let fresh = usage.input?.saturating_sub(usage.cached_input.unwrap_or(0));
                Some((fresh as f64 * input + usage.output? as f64 * output) / 1e6)
            }
        }
    }
}

/// What the caller needs from a model.
#[derive(Clone, Copy, Debug)]
pub struct Need {
    pub min_tier: Tier,
    pub tools: bool,
    /// Do not leave these cost classes (e.g. `Free` keeps routing at no marginal cost).
    pub max_cost: CostClass,
}

impl Default for Need {
    fn default() -> Self {
        Self {
            min_tier: Tier::Local,
            tools: false,
            max_cost: CostClass::Unknown,
        }
    }
}

#[derive(Clone, Debug, Default)]
struct Health {
    disabled: Option<String>,
    cooldown_until: Option<SystemTime>,
    strikes: u32,
    no_tools: bool,
    /// Context size learned from a "request exceeds the context" error.
    context_limit: Option<u64>,
}

/// Rough token count of a request (≈ 3 bytes per token, conservative for code).
pub fn estimate_tokens(request: &Request) -> u64 {
    let mut bytes = request.system.as_ref().map_or(0, String::len);
    for m in &request.messages {
        for p in &m.parts {
            bytes += match p {
                Part::Text(t) => t.len(),
                // Image tokens depend on dimensions and provider; base64 length is
                // not a token count. Do not reject a vision request on that basis.
                Part::Image { .. } => 0,
                Part::ToolCall {
                    name, arguments, ..
                } => name.len() + arguments.to_string().len(),
                Part::ToolResult { output, .. } => output.len(),
            };
        }
    }
    for t in &request.tools {
        bytes += t.name.len() + t.description.len() + t.parameters.to_string().len();
    }
    (bytes / 3) as u64 + u64::from(request.max_output_tokens.unwrap_or(1024))
}

/// `n_ctx` from errors like "request (8411 tokens) exceeds the available context size
/// (4096 tokens)" or "maximum context length is 8192 tokens".
fn context_from_error(message: &str) -> Option<u64> {
    let lower = message.to_ascii_lowercase();
    if !(lower.contains("context")
        && (lower.contains("exceed") || lower.contains("maximum") || lower.contains("too long")))
    {
        return None;
    }
    let anchor = [
        "context size (",
        "context length is ",
        "context_length",
        "n_ctx\\\":",
        "n_ctx\":",
        "n_ctx\":",
    ]
    .iter()
    .find_map(|a| lower.find(a).map(|i| i + a.len()))?;
    lower[anchor..]
        .trim_start_matches([' ', ':', '"', '\\'])
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>()
        .parse()
        .ok()
}

#[derive(Clone, Debug, Serialize)]
pub struct CallRecord {
    pub at: u64,
    pub endpoint: String,
    pub model: String,
    pub served_model: Option<String>,
    /// Upstream provider a gateway chose, for free-tier accounting.
    #[serde(default)]
    pub route: Option<String>,
    #[serde(default)]
    pub gateway_upstream_ms: Option<u64>,
    /// Total gateway latency minus reported upstream latency (when reported).
    #[serde(default)]
    pub gateway_overhead_ms: Option<u64>,
    pub ok: bool,
    pub error: Option<String>,
    pub input: Option<u64>,
    pub cached_input: Option<u64>,
    pub output: Option<u64>,
    pub usd: Option<f64>,
    pub latency_ms: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ModelStats {
    pub endpoint: String,
    pub model: String,
    pub calls: u64,
    pub failures: u64,
    pub input: u64,
    pub cached_input: u64,
    pub output: u64,
    /// `None` when any call had an unknown price.
    pub usd: Option<f64>,
    pub mean_latency_ms: u64,
}

#[derive(Debug)]
pub struct Served {
    pub response: Response,
    pub candidate: Candidate,
    /// Earlier candidates that failed, with the reason.
    pub skipped: Vec<(String, String)>,
}

pub struct Router {
    providers: HashMap<String, Box<dyn Provider>>,
    candidates: Vec<Candidate>,
    /// Candidates used only when a request names them (a gateway's long model list);
    /// automatic routing never picks them.
    explicit_only: std::collections::HashSet<(String, String)>,
    reasoning: HashMap<(String, String), ReasoningProfile>,
    health: Mutex<HashMap<(String, String), Health>>,
    records: Mutex<Vec<CallRecord>>,
    /// Calls from earlier runs (the ledger), for today's free-tier usage.
    history: Vec<CallRecord>,
    allowances: std::collections::BTreeMap<String, super::freetier::Allowance>,
    log: Option<PathBuf>,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

impl Router {
    pub fn new(log: Option<PathBuf>) -> Self {
        Self {
            providers: HashMap::new(),
            candidates: Vec::new(),
            explicit_only: Default::default(),
            reasoning: HashMap::new(),
            health: Mutex::new(HashMap::new()),
            records: Mutex::new(Vec::new()),
            history: log.as_deref().map(load_ledger).unwrap_or_default(),
            allowances: if log.is_some() {
                super::freetier::allowances()
            } else {
                Default::default()
            },
            log,
        }
    }

    pub fn add(&mut self, provider: Box<dyn Provider>, candidates: Vec<Candidate>) {
        self.candidates.extend(candidates);
        self.providers.insert(provider.name().to_owned(), provider);
    }

    /// Add candidates that are offered for explicit selection but never chosen
    /// automatically. The provider must already be added for `endpoint`.
    pub fn add_selectable(&mut self, candidates: Vec<Candidate>) {
        for candidate in candidates {
            let key = (candidate.endpoint.clone(), candidate.model.clone());
            if !self.candidates.iter().any(|c| c.endpoint == key.0 && c.model == key.1) {
                self.explicit_only.insert(key);
                self.candidates.push(candidate);
            }
        }
    }

    pub fn candidates(&self) -> &[Candidate] {
        &self.candidates
    }

    /// Explicit model configuration overrides provider-reported reasoning controls.
    pub fn set_reasoning_profile(
        &mut self,
        endpoint: &str,
        model: &str,
        profile: ReasoningProfile,
    ) {
        self.reasoning
            .insert((endpoint.to_owned(), model.to_owned()), profile);
    }

    /// Candidates in the order they would be tried, with any reason one is skipped.
    pub fn plan(&self, need: Need) -> Vec<(&Candidate, Option<String>)> {
        let health = self.health.lock().unwrap();
        let mut order: Vec<&Candidate> = self
            .candidates
            .iter()
            .filter(|c| c.tier >= need.min_tier && c.cost <= need.max_cost)
            .collect();
        // LOOM_PREFER_REMOTE=1: free remote models before local ones (faster on small
        // machines); local stays the fallback. The cost ceiling is unaffected.
        // Tool-using (coding) work defaults to free remote models first: a small local
        // model answered repository questions wrongly and loops on tools. Local stays
        // the fallback; LOOM_PREFER_LOCAL=1 (or LOOM_PREFER_REMOTE=0) restores local-first.
        let prefer_remote = match std::env::var("LOOM_PREFER_REMOTE").ok().as_deref() {
            Some("1") => true,
            Some("0") => false,
            _ => need.tools && std::env::var("LOOM_PREFER_LOCAL").ok().as_deref() != Some("1"),
        };
        let rank = |c: CostClass| match (prefer_remote, c) {
            (true, CostClass::Free) => 0,
            (true, CostClass::Local) => 1,
            (_, c) => c as u8 + u8::from(prefer_remote && c == CostClass::Local),
        };
        order.sort_by(|a, b| {
            (
                rank(a.cost),
                a.tier,
                a.price.map(|p| (p.0 * 1000.0) as u64).unwrap_or(u64::MAX),
            )
                .cmp(&(
                    rank(b.cost),
                    b.tier,
                    b.price.map(|p| (p.0 * 1000.0) as u64).unwrap_or(u64::MAX),
                ))
        });
        let now = SystemTime::now();
        let now_secs = super::router::now();
        let today: Vec<CallRecord> = self
            .history
            .iter()
            .chain(self.records.lock().unwrap().iter())
            .cloned()
            .collect();
        order
            .into_iter()
            .map(|c| {
                let h = health.get(&(c.endpoint.clone(), c.model.clone()));
                let endpoint = health.get(&(c.endpoint.clone(), String::new()));
                let used_up = self.allowances.get(&c.endpoint).filter(|a| {
                    let used = super::freetier::usage_today(&today, now_secs);
                    super::freetier::exhausted(
                        a,
                        &used.get(&c.endpoint).cloned().unwrap_or_default(),
                    )
                });
                let reason = if blocked_route(&c.endpoint) {
                    Some("blocked subscription OAuth endpoint".into())
                } else if used_up.is_some() {
                    Some("free tier used up today".into())
                } else if let Some(d) = endpoint.and_then(|h| h.disabled.clone()) {
                    Some(format!("endpoint disabled: {d}"))
                } else if need.tools && (!c.tools || h.is_some_and(|h| h.no_tools)) {
                    Some("no tool support".into())
                } else {
                    h.and_then(|h| h.cooldown_until)
                        .filter(|u| *u > now)
                        .map(|until| {
                            format!(
                                "cooling down {}s",
                                until.duration_since(now).map(|d| d.as_secs()).unwrap_or(0)
                            )
                        })
                };
                (c, reason)
            })
            .collect()
    }

    /// Send to the cheapest healthy candidate satisfying the optional model selection.
    pub fn complete(&self, need: Need, request: &Request) -> Result<Served, ProviderError> {
        self.complete_with(need, request, None)
    }

    pub fn complete_stream(
        &self,
        need: Need,
        request: &Request,
        observe: &mut dyn FnMut(StreamEvent),
    ) -> Result<Served, ProviderError> {
        self.complete_with(need, request, Some(observe))
    }

    fn complete_with(
        &self,
        need: Need,
        request: &Request,
        mut observe: Option<&mut dyn FnMut(StreamEvent)>,
    ) -> Result<Served, ProviderError> {
        request.check_cancelled()?;
        let request_started = std::time::Instant::now();
        let mut skipped = Vec::new();
        // Explicit user selection supersedes heuristic tier ranking. Tool, cost,
        // context, and route constraints still apply.
        let selection_need = if request.model.is_empty() {
            need
        } else {
            Need {
                min_tier: Tier::Local,
                ..need
            }
        };
        let plan: Vec<Candidate> = self
            .plan(selection_need)
            .into_iter()
            .filter(|(c, _)| {
                if request.model.is_empty() {
                    !self.explicit_only.contains(&(c.endpoint.clone(), c.model.clone()))
                } else {
                    request.model == c.model
                        || request.model == format!("{}/{}", c.endpoint, c.model)
                        || (c.endpoint == "lmx" && (
                            super::lmx::by_id(&request.model)
                                .ok()
                                .is_some_and(|found| found.display_name() == c.model)
                        ))
                }
            })
            .filter_map(|(c, reason)| match reason {
                Some(reason) => {
                    skipped.push((format!("{}/{}", c.endpoint, c.model), reason));
                    None
                }
                None => Some(c.clone()),
            })
            .collect();
        let mut last = None;
        let size = estimate_tokens(request);
        for candidate in plan {
            request.check_cancelled()?;
            let Some(provider) = self.providers.get(&candidate.endpoint) else {
                continue;
            };
            let learned = self
                .health
                .lock()
                .unwrap()
                .get(&(candidate.endpoint.clone(), candidate.model.clone()))
                .and_then(|h| h.context_limit);
            if let Some(limit) = learned.or(candidate.context).filter(|l| *l < size) {
                skipped.push((
                    format!("{}/{}", candidate.endpoint, candidate.model),
                    format!("context {limit} < request ≈{size} tokens"),
                ));
                continue;
            }
            let mut request = request.clone();
            request.model.clone_from(&candidate.model);
            if let Some(limit) = request.timeout {
                let remaining = limit.saturating_sub(request_started.elapsed());
                if remaining.is_zero() {
                    return Err(ProviderError::new(ErrorKind::TimedOut, "model call time limit exhausted across provider attempts"));
                }
                request.timeout = Some(remaining);
            }
            let started = std::time::Instant::now();
            let mut emitted = false;
            let result = match observe.as_mut() {
                Some(observer) => provider.complete_stream(&request, &mut |event| {
                    emitted = true;
                    observer(event);
                }),
                None => provider.complete(&request),
            };
            let result = match request.check_cancelled() {
                Ok(()) => result,
                Err(error) => Err(error),
            };
            match result {
                Ok(response) => {
                    let refused = match response.route.as_deref() {
                        Some(route) if blocked_route(route) => {
                            Some(format!("blocked subscription OAuth route: {route}"))
                        }
                        // fail closed: an unreported upstream may be a subscription login
                        None if provider.reports_route() => {
                            Some("blocked unverified gateway route: no upstream reported".into())
                        }
                        _ => None,
                    };
                    if let Some(message) = refused {
                        let error = ProviderError::new(ErrorKind::BadRequest, message);
                        self.record_call(
                            &candidate,
                            Some(&response),
                            Some(&error),
                            started.elapsed(),
                            false,
                        );
                        self.punish(&candidate, &error, false);
                        if emitted {
                            return Err(error);
                        }
                        skipped.push((
                            format!("{}/{}", candidate.endpoint, candidate.model),
                            error.to_string(),
                        ));
                        last = Some(error);
                        continue;
                    }
                    self.record(&candidate, Ok(&response), started.elapsed());
                    self.heal(&candidate);
                    return Ok(Served {
                        response,
                        candidate,
                        skipped,
                    });
                }
                Err(mut error) => {
                    self.record_call(
                        &candidate,
                        None,
                        Some(&error),
                        started.elapsed(),
                        observe.is_some(),
                    );
                    if error.kind == ErrorKind::Cancelled || error.is_tool_generation_rejected() {
                        return Err(error);
                    }
                    let cooldown = self.punish(&candidate, &error, need.tools);
                    if matches!(error.kind, ErrorKind::Transient | ErrorKind::TimedOut | ErrorKind::RateLimited) {
                        // The caller must wait until this candidate is eligible again.
                        // Returning sooner would turn the next call into a generic
                        // "no available model" error and lose the provider cause.
                        error.retry_after = cooldown;
                    }
                    // Once output escaped, another model would corrupt the stream.
                    if emitted {
                        return Err(error);
                    }
                    skipped.push((
                        format!("{}/{}", candidate.endpoint, candidate.model),
                        error.to_string(),
                    ));
                    last = Some(error);
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            let detail = if skipped.is_empty() {
                format!(
                    "no model matches tier ≥ {:?} and cost ≤ {:?}; start a local runtime or set an API key (see --models)",
                    need.min_tier, need.max_cost
                )
            } else {
                format!(
                    "every matching model is unavailable: {}",
                    skipped.iter().map(|(c, r)| format!("{c}: {r}")).collect::<Vec<_>>().join("; ")
                )
            };
            ProviderError::new(ErrorKind::Quota, detail)
        }))
    }

    fn heal(&self, c: &Candidate) {
        if let Some(h) = self
            .health
            .lock()
            .unwrap()
            .get_mut(&(c.endpoint.clone(), c.model.clone()))
        {
            h.strikes = 0;
            h.cooldown_until = None;
        }
    }

    fn punish(&self, c: &Candidate, error: &ProviderError, needed_tools: bool) -> Option<Duration> {
        if error.kind == ErrorKind::Cancelled || error.code.as_deref() == Some(LOCAL_RESOURCE_ADMISSION) {
            return None;
        }
        let mut health = self.health.lock().unwrap();
        if error.kind == ErrorKind::Auth {
            health
                .entry((c.endpoint.clone(), String::new()))
                .or_default()
                .disabled = Some(error.message.clone());
            return None;
        }
        let h = health
            .entry((c.endpoint.clone(), c.model.clone()))
            .or_default();
        h.strikes += 1;
        let wait = match error.kind {
            ErrorKind::Quota => error.retry_after.unwrap_or(Duration::from_secs(3600)),
            ErrorKind::RateLimited => error
                .retry_after
                .unwrap_or(Duration::from_secs(15 << h.strikes.min(5))),
            ErrorKind::TimedOut => error.retry_after.unwrap_or(Duration::from_secs(1)),
            ErrorKind::Transient => error.retry_after.unwrap_or(Duration::from_secs(10 << h.strikes.min(4))),
            ErrorKind::BadRequest => {
                if let Some(limit) = context_from_error(&error.message) {
                    // Too big for this model: remember its size, no penalty for smaller requests.
                    h.context_limit = Some(limit);
                    h.strikes = 0;
                    return None;
                }
                let lower = error.message.to_ascii_lowercase();
                if needed_tools && (lower.contains("tool") || lower.contains("function")) {
                    h.no_tools = true;
                    return None;
                }
                // A local model is not busy or rationed: the same request fails the same
                // way (a template it cannot render, an unsupported file). Cooling it down
                // only hides that error behind "unavailable" for ten minutes.
                if c.cost == CostClass::Local {
                    return None;
                }
                Duration::from_secs(600)
            }
            ErrorKind::Auth => unreachable!(),
            ErrorKind::Cancelled => unreachable!(),
        };
        h.cooldown_until = Some(SystemTime::now() + wait);
        Some(wait)
    }

    fn record(&self, c: &Candidate, result: Result<&Response, &ProviderError>, latency: Duration) {
        match result {
            Ok(response) => self.record_call(c, Some(response), None, latency, false),
            Err(error) => self.record_call(c, None, Some(error), latency, false),
        }
    }

    fn record_call(
        &self,
        c: &Candidate,
        response: Option<&Response>,
        error: Option<&ProviderError>,
        latency: Duration,
        interrupted_stream: bool,
    ) {
        let usage = response.map(|r| r.usage.clone()).unwrap_or_default();
        let record = CallRecord {
            at: now(),
            endpoint: c.endpoint.clone(),
            model: c.model.clone(),
            served_model: response.map(|r| r.model.clone()),
            route: response.and_then(|r| r.route.clone()),
            gateway_upstream_ms: response.and_then(|r| r.gateway_upstream_ms),
            gateway_overhead_ms: response.and_then(|r| r.gateway_upstream_ms)
                .map(|upstream| (latency.as_millis().min(u128::from(u64::MAX)) as u64).saturating_sub(upstream)),
            ok: error.is_none(),
            error: error.map(|e| format!("{:?}: {}", e.kind, e.message)),
            input: usage.input,
            cached_input: usage.cached_input,
            output: usage.output,
            usd: if response.is_some() {
                c.usd(&usage)
            } else if interrupted_stream && c.cost > CostClass::Subscription {
                // A stream can generate tool arguments without text deltas. Without
                // final usage, a failed generation's billed cost is unknown.
                None
            } else {
                Some(0.0)
            },
            latency_ms: latency.as_millis().min(u128::from(u64::MAX)) as u64,
        };
        if let Some(path) = &self.log
            && let Ok(line) = serde_json::to_string(&record)
        {
            let _ = std::fs::create_dir_all(path.parent().unwrap_or(path));
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = writeln!(file, "{line}");
            }
        }
        self.records.lock().unwrap().push(record);
    }

    pub fn records(&self) -> Vec<CallRecord> {
        self.records.lock().unwrap().clone()
    }

    pub fn ledger(&self) -> Vec<CallRecord> {
        self.history.iter().cloned().chain(self.records()).collect()
    }

    pub fn model_profiles(&self) -> Vec<ModelProfile> {
        self.plan(Need::default())
            .into_iter()
            .map(|(c, reason)| {
                let reasoning = self
                    .reasoning
                    .get(&(c.endpoint.clone(), c.model.clone()))
                    .cloned()
                    .unwrap_or_else(|| {
                        self.providers
                            .get(&c.endpoint)
                            .map(|p| p.reasoning_profile(&c.model))
                            .unwrap_or_default()
                    });
                ModelProfile {
                    candidate: c.clone(),
                    supported_efforts: reasoning.supported_efforts,
                    default_effort: reasoning.default_effort,
                    unavailable_reason: if c.cost > cost_ceiling() {
                        Some("exceeds Core cost ceiling".into())
                    } else {
                        reason
                    },
                }
            })
            .collect()
    }

    pub fn limits(&self) -> Vec<ModelLimits> {
        let health = self.health.lock().unwrap();
        self.candidates
            .iter()
            .map(|c| {
                let h = health.get(&(c.endpoint.clone(), c.model.clone()));
                let endpoint = health.get(&(c.endpoint.clone(), String::new()));
                ModelLimits {
                    endpoint: c.endpoint.clone(),
                    model: c.model.clone(),
                    context: h.and_then(|h| h.context_limit).or(c.context),
                    tools: c.tools && !h.is_some_and(|h| h.no_tools),
                    disabled: endpoint.and_then(|h| h.disabled.clone()),
                    cooldown_until: h.and_then(|h| h.cooldown_until),
                }
            })
            .collect()
    }
}

/// Per-model aggregates over call records (this session's, or a loaded ledger).
pub fn stats(records: &[CallRecord]) -> Vec<ModelStats> {
    let mut by: HashMap<(String, String), (ModelStats, u64)> = HashMap::new();
    for r in records {
        // Gateway calls are shown per upstream provider and the model that actually answered.
        let (endpoint, model) = match (&r.route, &r.served_model) {
            (Some(route), Some(served)) => (format!("{}→{route}", r.endpoint), served.clone()),
            _ => (r.endpoint.clone(), r.model.clone()),
        };
        let (s, latency) = by
            .entry((endpoint.clone(), model.clone()))
            .or_insert_with(|| {
                (
                    ModelStats {
                        endpoint,
                        model,
                        usd: Some(0.0),
                        ..ModelStats::default()
                    },
                    0,
                )
            });
        s.calls += 1;
        s.failures += u64::from(!r.ok);
        s.input += r.input.unwrap_or(0);
        s.cached_input += r.cached_input.unwrap_or(0);
        s.output += r.output.unwrap_or(0);
        s.usd = match (s.usd, r.usd) {
            (Some(a), Some(b)) => Some(a + b),
            _ => None,
        };
        *latency += r.latency_ms;
    }
    let mut out: Vec<ModelStats> = by
        .into_values()
        .map(|(mut s, latency)| {
            s.mean_latency_ms = latency / s.calls.max(1);
            s
        })
        .collect();
    out.sort_by(|a, b| b.calls.cmp(&a.calls).then(a.model.cmp(&b.model)));
    out
}

pub fn load_ledger(path: &std::path::Path) -> Vec<CallRecord> {
    #[derive(serde::Deserialize)]
    struct Row {
        at: u64,
        endpoint: String,
        model: String,
        served_model: Option<String>,
        #[serde(default)]
        route: Option<String>,
        ok: bool,
        error: Option<String>,
        input: Option<u64>,
        cached_input: Option<u64>,
        output: Option<u64>,
        usd: Option<f64>,
        latency_ms: u64,
    }
    std::fs::read_to_string(path)
        .unwrap_or_default()
        .lines()
        .filter_map(|l| serde_json::from_str::<Row>(l).ok())
        .map(|r| CallRecord {
            at: r.at,
            endpoint: r.endpoint,
            model: r.model,
            served_model: r.served_model,
            route: r.route,
            gateway_upstream_ms: None,
            gateway_overhead_ms: None,
            ok: r.ok,
            error: r.error,
            input: r.input,
            cached_input: r.cached_input,
            output: r.output,
            usd: r.usd,
            latency_ms: r.latency_ms,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    struct Scripted {
        name: String,
        replies: Mutex<VecDeque<Result<Response, ProviderError>>>,
        calls: Mutex<Vec<String>>,
    }

    impl Provider for Scripted {
        fn name(&self) -> &str {
            &self.name
        }
        fn complete(&self, request: &Request) -> Result<Response, ProviderError> {
            self.calls.lock().unwrap().push(request.model.clone());
            self.replies
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(ok(&request.model)))
        }
        fn list_models(&self) -> Result<Vec<String>, ProviderError> {
            Ok(vec![])
        }
    }

    fn ok(model: &str) -> Response {
        Response {
            parts: vec![Part::Text("hi".into())],
            usage: Usage {
                input: Some(1000),
                cached_input: Some(0),
                output: Some(100),
            },
            stop: Stop::EndTurn,
            model: model.into(),
            latency: Duration::from_millis(5),
            route: None,
            gateway_connection: None,
            gateway_upstream_ms: None,
        }
    }

    fn candidate(endpoint: &str, model: &str, tier: Tier, cost: CostClass) -> Candidate {
        Candidate {
            endpoint: endpoint.into(),
            model: model.into(),
            tier,
            cost,
            price: None,
            tools: true,
            context: None,
        }
    }

    fn scripted(name: &str, replies: Vec<Result<Response, ProviderError>>) -> Box<Scripted> {
        Box::new(Scripted {
            name: name.into(),
            replies: Mutex::new(replies.into()),
            calls: Mutex::new(vec![]),
        })
    }

    fn router() -> Router {
        let mut r = Router::new(None);
        r.add(
            scripted("paid", vec![]),
            vec![{
                let mut c = candidate("paid", "big", Tier::Flagship, CostClass::Paid);
                c.price = Some((3.0, 15.0));
                c
            }],
        );
        r.add(
            scripted(
                "free",
                vec![Err(ProviderError {
                    kind: ErrorKind::RateLimited,
                    message: "slow".into(),
                    retry_after: Some(Duration::from_secs(30)),
                    code: None,
                })],
            ),
            vec![candidate("free", "mid:free", Tier::Medium, CostClass::Free)],
        );
        r.add(
            scripted("local", vec![]),
            vec![candidate("local", "tiny", Tier::Small, CostClass::Local)],
        );
        r
    }

    #[test]
    fn cheapest_capable_candidate_wins() {
        let r = router();
        let served = r.complete(Need::default(), &Request::default()).unwrap();
        assert_eq!(served.candidate.model, "tiny");
        let order: Vec<_> = r
            .plan(Need {
                min_tier: Tier::Medium,
                ..Need::default()
            })
            .into_iter()
            .map(|(c, _)| c.model.clone())
            .collect();
        assert_eq!(
            order,
            ["mid:free", "big"],
            "local small model is below the requested tier"
        );
    }

    #[test]
    fn rate_limited_free_model_falls_back_and_cools_down() {
        let r = router();
        let need = Need {
            min_tier: Tier::Medium,
            ..Need::default()
        };
        let served = r.complete(need, &Request::default()).unwrap();
        assert_eq!(served.candidate.model, "big");
        assert!(served.skipped[0].1.contains("RateLimited"));
        let plan = r.plan(need);
        assert!(
            plan[0]
                .1
                .as_deref()
                .is_some_and(|s| s.starts_with("cooling down")),
            "{plan:?}"
        );
        let stats = stats(&r.records());
        let big = stats.iter().find(|s| s.model == "big").unwrap();
        assert_eq!(big.usd, Some((1000.0 * 3.0 + 100.0 * 15.0) / 1e6));
        assert_eq!(
            stats
                .iter()
                .find(|s| s.model == "mid:free")
                .unwrap()
                .failures,
            1
        );
    }

    #[test]
    fn auth_failure_disables_the_endpoint_and_cost_cap_holds() {
        let mut r = Router::new(None);
        r.add(
            scripted(
                "bad",
                vec![Err(ProviderError::new(ErrorKind::Auth, "invalid key"))],
            ),
            vec![candidate("bad", "a", Tier::Small, CostClass::Free)],
        );
        r.add(
            scripted("paid", vec![]),
            vec![candidate("paid", "b", Tier::Small, CostClass::Paid)],
        );
        let free_only = Need {
            max_cost: CostClass::Free,
            ..Need::default()
        };
        assert!(
            r.complete(free_only, &Request::default()).is_err(),
            "never escalates past the cost cap"
        );
        assert!(
            r.plan(Need::default())[0]
                .1
                .as_deref()
                .is_some_and(|s| s.contains("disabled"))
        );
        assert_eq!(
            r.complete(Need::default(), &Request::default())
                .unwrap()
                .candidate
                .model,
            "b"
        );
    }

    #[test]
    fn context_overflow_is_learned_not_punished() {
        let mut r = Router::new(None);
        r.add(
            scripted("local", vec![Err(ProviderError::new(ErrorKind::BadRequest, r#"HTTP 400: {"error":{"message":"request (8411 tokens) exceeds the available context size (4096 tokens), try increasing it"}}"#))]),
            vec![candidate("local", "small-ctx", Tier::Small, CostClass::Local)],
        );
        let big = Request {
            messages: vec![Message::user("x".repeat(30_000))],
            ..Request::default()
        };
        assert!(r.complete(Need::default(), &big).is_err());
        let again = r.complete(Need::default(), &big).unwrap_err();
        assert!(
            again.message.contains("context 4096 < request"),
            "{}",
            again.message
        );
        let small = Request {
            messages: vec![Message::user("hi")],
            ..Request::default()
        };
        assert_eq!(
            r.complete(Need::default(), &small).unwrap().candidate.model,
            "small-ctx",
            "small requests still go there"
        );
        assert_eq!(
            context_from_error("maximum context length is 8192 tokens"),
            Some(8192)
        );
    }

    #[test]
    fn local_resource_admission_does_not_disable_tools_or_cool_down_the_model() {
        for kind in [ErrorKind::BadRequest, ErrorKind::TimedOut] {
            let mut r = Router::new(None);
            let mut error = ProviderError::new(kind, "local memory budget cannot fit model and tools");
            error.code = Some(LOCAL_RESOURCE_ADMISSION.into());
            r.add(scripted("local", vec![Err(error)]),
                vec![candidate("local", "small", Tier::Small, CostClass::Local)]);
            let need = Need { tools: true, ..Need::default() };
            assert!(r.complete(need, &Request::default()).is_err());
            assert!(r.plan(need).iter().all(|(_, reason)| reason.is_none()));
            assert_eq!(r.complete(need, &Request::default()).unwrap().candidate.model, "small");
        }
    }

    #[test]
    fn invalid_tool_generation_keeps_model_eligible_for_worker_correction() {
        let mut r = Router::new(None);
        r.add(
            scripted("local", vec![Err(ProviderError::new(ErrorKind::BadRequest,
                r#"HTTP 400: {"error":{"code":"tool_use_failed","message":"attempted to call unknown tool"}}"#))]),
            vec![
                candidate("local", "x", Tier::Small, CostClass::Local),
                candidate("local", "y", Tier::Small, CostClass::Local),
            ],
        );
        let need = Need { tools: true, ..Need::default() };
        assert!(r.complete(need, &Request::default()).is_err(), "generation correction belongs to the worker, not another model");
        assert!(r.plan(need).iter().all(|(_, reason)| reason.is_none()), "generation failure is not missing tool support or endpoint cooldown");
        assert_eq!(r.complete(need, &Request::default()).unwrap().candidate.model, "x");
    }

    #[test]
    fn tool_rejection_marks_the_model_not_the_endpoint() {
        let mut r = Router::new(None);
        r.add(
            scripted(
                "local",
                vec![Err(ProviderError::new(
                    ErrorKind::BadRequest,
                    "model does not support tools",
                ))],
            ),
            vec![
                candidate("local", "x", Tier::Small, CostClass::Local),
                candidate("local", "y", Tier::Small, CostClass::Local),
            ],
        );
        let need = Need {
            tools: true,
            ..Need::default()
        };
        assert_eq!(
            r.complete(need, &Request::default())
                .unwrap()
                .candidate
                .model,
            "y"
        );
        assert!(
            r.plan(need)
                .iter()
                .any(|(c, reason)| c.model == "x" && reason.as_deref() == Some("no tool support"))
        );
        assert!(
            r.plan(Need::default())
                .iter()
                .all(|(_, reason)| reason.is_none()),
            "still usable without tools"
        );
    }
}
