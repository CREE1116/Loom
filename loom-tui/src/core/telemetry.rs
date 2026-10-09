//! Per-turn cost telemetry split by work phase.
//!
//! Experiments showed remote context is dominated by orientation and validation output,
//! not edits (docs/loom-experiment-findings.md §2.5). Core records that split for every
//! turn so the claim stays checkable in normal use. Byte shares are measured; tokens are
//! the runtime's reported turn deltas. Tokens are never apportioned to phases here.
use crate::agent::{AgentUpdate, Kind, TokenUsage};
use serde::Serialize;
use std::{
    collections::HashMap,
    fs::OpenOptions,
    io::Write,
    path::{Path, PathBuf},
    time::{Instant, SystemTime, UNIX_EPOCH},
};

pub use loom_core::base::Phase;

pub use loom_core::effects::classify_command;

#[derive(Clone, Debug, Default, Serialize)]
pub struct PhaseBytes {
    pub orient: u64,
    pub validate: u64,
    pub edit: u64,
    pub message: u64,
}

impl PhaseBytes {
    fn add(&mut self, phase: Phase, bytes: u64) {
        match phase {
            Phase::Orient => self.orient += bytes,
            Phase::Validate => self.validate += bytes,
            Phase::Edit => self.edit += bytes,
            Phase::Message => self.message += bytes,
        }
    }
    pub fn total(&self) -> u64 {
        self.orient + self.validate + self.edit + self.message
    }
}

/// Runtime-reported usage for one turn (difference of cumulative counters).
#[derive(Clone, Debug, Default, Serialize)]
pub struct TokenDelta {
    pub input: Option<u64>,
    pub cached_input: Option<u64>,
    pub output: Option<u64>,
    /// Usage reports seen during the turn; roughly one per model call.
    pub reports: u32,
}

#[derive(Clone, Debug, Serialize)]
pub struct TurnCost {
    pub session: Option<String>,
    pub turn: Option<String>,
    pub status: String,
    pub finished_at: u64,
    pub elapsed_ms: u64,
    pub bytes: PhaseBytes,
    pub commands: HashMap<Phase, u32>,
    pub tokens: TokenDelta,
}

#[derive(Default)]
struct OpenTurn {
    id: Option<String>,
    started: Option<Instant>,
    /// Completed item sizes by id; a re-sent item replaces its earlier size.
    items: HashMap<String, (Phase, u64)>,
    baseline: Option<TokenUsage>,
    latest: Option<TokenUsage>,
    reports: u32,
}

/// Observes remote events and closes one `TurnCost` per completed turn.
#[derive(Default)]
pub struct PhaseLedger {
    turn: Option<OpenTurn>,
    /// Cumulative usage before the current turn began.
    last_usage: Option<TokenUsage>,
}

impl PhaseLedger {
    pub fn observe(&mut self, session: Option<&str>, update: &AgentUpdate) -> Option<TurnCost> {
        match update {
            AgentUpdate::TurnStarted { id } => {
                self.turn = Some(OpenTurn {
                    id: id.clone(),
                    started: Some(Instant::now()),
                    baseline: self.last_usage.clone(),
                    ..OpenTurn::default()
                });
            }
            AgentUpdate::TokenUsageUpdated(usage) => {
                if let Some(turn) = &mut self.turn {
                    turn.latest = Some(usage.clone());
                    turn.reports += 1;
                }
                self.last_usage = Some(usage.clone());
            }
            AgentUpdate::EntryUpdated(entry) if entry.status != "inProgress" => {
                let turn = self.turn.as_mut()?;
                let measured = match entry.kind {
                    Kind::Tool => Some((
                        classify_command(&entry.title),
                        entry.title.len() + entry.body.len(),
                    )),
                    Kind::Change => Some((Phase::Edit, entry.body.len())),
                    Kind::Assistant => Some((Phase::Message, entry.body.len())),
                    _ => None,
                };
                if let Some((phase, bytes)) = measured {
                    turn.items.insert(entry.id.clone(), (phase, bytes as u64));
                }
            }
            AgentUpdate::TurnCompleted { id, status, .. } => {
                let turn = self.turn.take()?;
                return Some(close(turn, session, id.clone(), status));
            }
            _ => {}
        }
        None
    }
}

fn close(turn: OpenTurn, session: Option<&str>, id: Option<String>, status: &str) -> TurnCost {
    let mut bytes = PhaseBytes::default();
    let mut commands = HashMap::new();
    for (phase, size) in turn.items.values() {
        bytes.add(*phase, *size);
        if *phase != Phase::Message {
            *commands.entry(*phase).or_insert(0) += 1;
        }
    }
    let diff = |pick: fn(&TokenUsage) -> Option<u64>| {
        let end = turn.latest.as_ref().and_then(pick)?;
        let start = turn.baseline.as_ref().and_then(pick).unwrap_or(0);
        end.checked_sub(start)
    };
    TurnCost {
        session: session.map(str::to_owned),
        turn: id.or(turn.id),
        status: status.into(),
        finished_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0),
        elapsed_ms: turn
            .started
            .map(|s| s.elapsed().as_millis().min(u128::from(u64::MAX)) as u64)
            .unwrap_or(0),
        bytes,
        commands,
        tokens: TokenDelta {
            input: diff(|u| u.input),
            cached_input: diff(|u| u.cached_input),
            output: diff(|u| u.output),
            reports: turn.reports,
        },
    }
}

/// Append-only JSONL record of closed turns. Failures never interrupt the session.
pub struct TelemetryLog {
    path: PathBuf,
}

impl TelemetryLog {
    pub fn new(state_dir: &Path) -> Self {
        Self {
            path: state_dir.join("telemetry.jsonl"),
        }
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn append(&self, cost: &TurnCost) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        let line = serde_json::to_string(cost).map_err(std::io::Error::other)?;
        writeln!(file, "{line}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::Entry;

    #[test]
    fn classifies_wrapped_and_compound_commands() {
        assert_eq!(
            classify_command(r#"/bin/zsh -lc "sed -n '1,80p' src/a.rs""#),
            Phase::Orient
        );
        assert_eq!(classify_command("rg -n foo src | head"), Phase::Orient);
        assert_eq!(
            classify_command("/bin/zsh -lc 'cargo test --bin fd'"),
            Phase::Validate
        );
        assert_eq!(
            classify_command("cd x && cargo fmt --check"),
            Phase::Validate
        );
        assert_eq!(classify_command("git diff --stat"), Phase::Validate);
        assert_eq!(classify_command("sed -i '' 's/a/b/' x.rs"), Phase::Edit);
        assert_eq!(classify_command("cat <<EOF > x.rs"), Phase::Edit);
        assert_eq!(classify_command("git log --oneline -5"), Phase::Orient);
        assert_eq!(classify_command("rg foo 2> /dev/null"), Phase::Orient);
        assert_eq!(classify_command("ls >/dev/null 2>&1"), Phase::Orient);
        assert_eq!(classify_command("echo x >> notes.md"), Phase::Edit);
        assert_eq!(
            classify_command(
                "PYTHONPATH=custom-tui/tests python3 -c 'import terminal_smoke; terminal_smoke.markdown_test()'"
            ),
            Phase::Validate
        );
        assert_eq!(
            classify_command("RUST_LOG=debug cargo test"),
            Phase::Validate
        );
        assert_eq!(classify_command("python3 scripts/format.py"), Phase::Orient);
        assert_eq!(classify_command("rg -n 'test' src"), Phase::Orient);
    }

    fn entry(id: &str, kind: Kind, title: &str, body: &str, status: &str) -> AgentUpdate {
        AgentUpdate::EntryUpdated(Entry {
            id: id.into(),
            kind,
            title: title.into(),
            body: body.into(),
            status: status.into(),
            expanded: false,
        })
    }

    fn usage(input: u64, cached: u64, output: u64) -> AgentUpdate {
        AgentUpdate::TokenUsageUpdated(TokenUsage {
            input: Some(input),
            cached_input: Some(cached),
            output: Some(output),
            ..TokenUsage::default()
        })
    }

    #[test]
    fn closes_a_turn_with_phase_bytes_and_usage_delta() {
        let mut ledger = PhaseLedger::default();
        assert!(ledger.observe(Some("s"), &usage(1_000, 0, 10)).is_none());
        ledger.observe(
            Some("s"),
            &AgentUpdate::TurnStarted {
                id: Some("t1".into()),
            },
        );
        ledger.observe(
            Some("s"),
            &entry("c1", Kind::Tool, "rg foo", "", "inProgress"),
        );
        ledger.observe(
            Some("s"),
            &entry("c1", Kind::Tool, "rg foo", "0123456789", "completed"),
        );
        ledger.observe(
            Some("s"),
            &entry("c2", Kind::Tool, "cargo test", "ok", "completed"),
        );
        ledger.observe(
            Some("s"),
            &entry("f1", Kind::Change, "파일 변경", "+x", "completed"),
        );
        ledger.observe(
            Some("s"),
            &entry("m1", Kind::Assistant, "●", "done", "completed"),
        );
        ledger.observe(Some("s"), &usage(5_000, 3_000, 60));
        ledger.observe(Some("s"), &usage(9_000, 7_000, 90));
        let cost = ledger
            .observe(
                Some("s"),
                &AgentUpdate::TurnCompleted {
                    id: Some("t1".into()),
                    status: "completed".into(),
                    error: None,
                },
            )
            .unwrap();
        assert_eq!(cost.bytes.orient, 16);
        assert_eq!(cost.bytes.validate, 12);
        assert_eq!(cost.bytes.edit, 2);
        assert_eq!(cost.bytes.message, 4);
        assert_eq!(cost.commands[&Phase::Orient], 1);
        assert_eq!(cost.tokens.input, Some(8_000));
        assert_eq!(cost.tokens.cached_input, Some(7_000));
        assert_eq!(cost.tokens.output, Some(80));
        assert_eq!(cost.tokens.reports, 2);
    }

    #[test]
    fn missing_usage_stays_unknown_instead_of_zero() {
        let mut ledger = PhaseLedger::default();
        ledger.observe(None, &AgentUpdate::TurnStarted { id: None });
        let cost = ledger
            .observe(
                None,
                &AgentUpdate::TurnCompleted {
                    id: None,
                    status: "completed".into(),
                    error: None,
                },
            )
            .unwrap();
        assert_eq!(cost.tokens.input, None);
        assert_eq!(cost.bytes.total(), 0);
    }
}
