//! Free-tier accounting: how much of each provider's daily free allowance is used today.
//!
//! Limits are approximate public figures that providers change without notice; override
//! them in `~/.loom/free-tiers.toml`:
//!
//! ```toml
//! [groq]
//! requests_per_day = 1000
//! tokens_per_day = 500000
//! ```
//!
//! Usage comes from Loom's own call ledger (`~/.loom/model-usage.jsonl`). Calls that went
//! through a gateway are counted against the upstream provider the gateway reported.
use super::router::CallRecord;
use serde::Deserialize;
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
pub struct Allowance {
    pub requests_per_day: Option<u64>,
    pub tokens_per_day: Option<u64>,
    /// Shown next to the numbers, e.g. which models the limit applies to.
    #[serde(default)]
    pub note: String,
}

/// Approximate defaults (2026-10). Free tiers move; edit ~/.loom/free-tiers.toml.
fn defaults() -> BTreeMap<String, Allowance> {
    let a = |r: Option<u64>, t: Option<u64>, note: &str| Allowance {
        requests_per_day: r,
        tokens_per_day: t,
        note: note.into(),
    };
    BTreeMap::from([
        (
            "gemini".into(),
            a(Some(250), None, "flash models; pro is lower"),
        ),
        (
            "groq".into(),
            a(Some(1000), Some(500_000), "per model, typical"),
        ),
        (
            "mistral".into(),
            a(None, Some(33_000_000), "≈1B tokens/month"),
        ),
        ("cerebras".into(), a(None, Some(1_000_000), "typical")),
        (
            "openrouter".into(),
            a(Some(50), None, ":free models without credits"),
        ),
    ])
}

pub fn config_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".loom/free-tiers.toml"))
}

pub fn allowances() -> BTreeMap<String, Allowance> {
    let mut all = defaults();
    if let Some(text) = config_path().and_then(|p| std::fs::read_to_string(p).ok())
        && let Ok(custom) = toml::from_str::<BTreeMap<String, Allowance>>(&text)
    {
        all.extend(custom);
    }
    all
}

/// Provider a call is charged to: the gateway's upstream if known, else the endpoint.
pub fn provider_of(record: &CallRecord) -> String {
    record
        .route
        .clone()
        .unwrap_or_else(|| record.endpoint.clone())
}

/// Start of today in UTC (most free tiers reset at a fixed UTC or Pacific time).
pub fn day_start(now: u64) -> u64 {
    now - now % 86_400
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Today {
    pub requests: u64,
    pub tokens: u64,
}

pub fn usage_today(records: &[CallRecord], now: u64) -> BTreeMap<String, Today> {
    let start = day_start(now);
    let mut out: BTreeMap<String, Today> = BTreeMap::new();
    for r in records.iter().filter(|r| r.at >= start && r.ok) {
        let t = out.entry(provider_of(r)).or_default();
        t.requests += 1;
        t.tokens += r.input.unwrap_or(0) + r.output.unwrap_or(0);
    }
    out
}

/// Whether `provider` has used up today's allowance (either limit).
pub fn exhausted(allowance: &Allowance, today: &Today) -> bool {
    allowance
        .requests_per_day
        .is_some_and(|l| today.requests >= l)
        || allowance.tokens_per_day.is_some_and(|l| today.tokens >= l)
}

/// `custom-tui --free-tiers`.
pub fn print(records: &[CallRecord], now: u64) {
    let allow = allowances();
    let used = usage_today(records, now);
    let reset = 86_400 - now % 86_400;
    println!(
        "free tiers today (UTC day, resets in {}h{:02}m) · limits are approximate, edit {}",
        reset / 3600,
        reset % 3600 / 60,
        config_path()
            .map(|p| p.display().to_string())
            .unwrap_or_default()
    );
    println!(
        "{:<12} {:>16} {:>22}  note",
        "provider", "requests", "tokens"
    );
    let mut names: Vec<&String> = allow.keys().chain(used.keys()).collect();
    names.sort();
    names.dedup();
    for name in names {
        let a = allow.get(name).cloned().unwrap_or_default();
        let t = used.get(name).cloned().unwrap_or_default();
        let fmt = |n: u64, limit: Option<u64>| match limit {
            Some(l) => format!("{n}/{l} ({}%)", n * 100 / l.max(1)),
            None => n.to_string(),
        };
        let mark = if exhausted(&a, &t) {
            "  ← used up"
        } else {
            ""
        };
        println!(
            "{:<12} {:>16} {:>22}  {}{mark}",
            name,
            fmt(t.requests, a.requests_per_day),
            fmt(t.tokens, a.tokens_per_day),
            a.note
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(endpoint: &str, route: Option<&str>, at: u64, tokens: u64) -> CallRecord {
        CallRecord {
            at,
            endpoint: endpoint.into(),
            model: "m".into(),
            served_model: None,
            route: route.map(str::to_owned),
            gateway_upstream_ms: None,
            gateway_overhead_ms: None,
            ok: true,
            error: None,
            input: Some(tokens),
            cached_input: None,
            output: Some(0),
            usd: Some(0.0),
            latency_ms: 1,
        }
    }

    #[test]
    fn gateway_calls_count_against_the_upstream_provider_for_today_only() {
        let now = 10 * 86_400 + 3600;
        let records = vec![
            record("openrouter", Some("gemini"), now - 60, 100),
            record("openrouter", Some("gemini"), now - 120, 50),
            record("groq", None, now - 60, 10),
            record("openrouter", Some("gemini"), now - 86_400, 999), // yesterday
        ];
        let used = usage_today(&records, now);
        assert_eq!(
            used["gemini"],
            Today {
                requests: 2,
                tokens: 150
            }
        );
        assert_eq!(used["groq"].requests, 1);
        let small = Allowance {
            requests_per_day: Some(2),
            ..Allowance::default()
        };
        assert!(exhausted(&small, &used["gemini"]));
        assert!(!exhausted(&small, &used["groq"]));
    }
}
