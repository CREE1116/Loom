//! Observed work survives conversation compaction and worker replacement.
//! Entries are observations, not model assertions or proof of task completion.
use crate::base::ArtifactRef;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::VecDeque;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct WorkMemory {
    /// Any possibly mutating operation advances the epoch conservatively.
    epoch: u64,
    observations: VecDeque<Observation>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Observation {
    epoch: u64,
    tool: String,
    query: String,
    error: bool,
    excerpt: String,
    #[serde(default)]
    result_summary: String,
    artifact: ArtifactRef,
    priority: u8,
}

fn bounded(text: &str, cap: usize) -> String {
    let mut end = text.len().min(cap);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

impl WorkMemory {
    pub(crate) fn recent_activity(&self) -> String {
        self.observations
            .iter()
            .rev()
            .take(4)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|entry| {
                let args = serde_json::from_str::<Value>(&entry.query)
                    .unwrap_or_else(|_| serde_json::json!({"path":entry.query}));
                let result = if entry.result_summary.is_empty() {
                    super::progress::result_summary(&entry.tool, &entry.excerpt, entry.error, false)
                } else {
                    entry.result_summary.clone()
                };
                format!(
                    "- {} · {result}",
                    super::progress::describe(&entry.tool, &args)
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    pub(super) fn invalidate(&mut self) {
        self.epoch += 1;
    }

    pub(super) fn observe(
        &mut self,
        tool: &str,
        args: &Value,
        output: &str,
        error: bool,
        artifact: ArtifactRef,
    ) {
        if matches!(tool, "finish" | "enable_tools" | "update_plan") {
            return;
        }
        let query = match tool {
            "edit" | "write" => args["path"].as_str().unwrap_or("").to_owned(),
            _ => bounded(&args.to_string(), 300),
        };
        if self.observations.iter().any(|entry| {
            entry.epoch == self.epoch
                && entry.tool == tool
                && entry.query == query
                && entry.artifact == artifact
        }) {
            return;
        }
        let priority = if error || tool == "check" {
            4
        } else if matches!(
            tool,
            "edit"
                | "write"
                | "merge_agent"
                | "replace_symbol"
                | "rename_symbol"
                | "insert_after_symbol"
        ) {
            3
        } else if matches!(tool, "find" | "ast_search" | "refs") {
            2
        } else {
            1
        };
        self.observations.push_back(Observation {
            epoch: self.epoch,
            tool: tool.into(),
            query,
            error,
            excerpt: bounded(output, 600),
            result_summary: super::progress::result_summary(tool, output, error, false),
            artifact,
            priority,
        });
        // Raw evidence remains in artifacts/trace. Bound the hot index, too.
        if self.observations.len() > 64 {
            self.observations.pop_front();
        }
    }

    /// A budgeted projection prioritizes failures, edits and source locations.
    /// Historical evidence is labeled stale rather than silently reused as fact.
    pub(crate) fn render(&self, cap: usize) -> String {
        let mut entries: Vec<_> = self.observations.iter().enumerate().collect();
        entries.sort_by_key(|(index, entry)| {
            std::cmp::Reverse((entry.epoch == self.epoch, entry.priority, *index))
        });
        let mut out = String::new();
        for (_, entry) in entries {
            let text = format!(
                "{} {} {}\n{}\nEvidence (read_artifact): {}\n",
                if entry.epoch == self.epoch {
                    "observed"
                } else {
                    "possibly stale after mutation"
                },
                if entry.error { "failed" } else { "succeeded" },
                format_args!("{} {}", entry.tool, entry.query),
                entry.excerpt,
                entry.artifact.id
            );
            if out.len() + text.len() <= cap {
                out.push_str(&text);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mutation_marks_prior_evidence_stale_and_keeps_its_provenance() {
        let mut memory = WorkMemory::default();
        memory.observe(
            "find",
            &json!({"pattern":"expired"}),
            "src/auth.rs:12 expired",
            false,
            ArtifactRef {
                id: "a".repeat(64),
                bytes: 22,
            },
        );
        memory.invalidate();
        let saved = serde_json::to_vec(&memory).unwrap();
        let restored: WorkMemory = serde_json::from_slice(&saved).unwrap();
        let text = restored.render(2000);
        assert!(text.contains("possibly stale"));
        assert!(text.contains("src/auth.rs:12"));
        assert!(text.contains(&"a".repeat(64)));
    }
}
