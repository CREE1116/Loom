//! Rehydrate prior user/assistant turns from durable tasks, with bounded receipts.
//! This is conversation history, not a second mutable representation of code.
use crate::agent::{Status, Task, events::Event, tools::clip};
use anyhow::Result;
use crate::models::{Message, Part, Role};
use std::fs;

pub fn messages(tasks: &[Task], budget: usize) -> Result<Vec<Message>> {
    let mut history = Vec::new();
    for task in tasks {
        let mut evidence = Vec::new();
        let log = task
            .records
            .with_file_name(format!("{}.events.jsonl", task.id));
        if let Ok(contents) = fs::read_to_string(log) {
            for line in contents
                .split_inclusive('\n')
                .filter(|line| line.ends_with('\n'))
            {
                let event: Event = serde_json::from_str(line)?;
                // Model traffic and loading instruction files are not evidence of the outcome.
                if event.target == "model" || event.target == "model_result" || event.detail["kind"] == "instructions" {
                    continue;
                }
                if event.detail["completed"] == true
                    || event.failed
                    || matches!(
                        event.stage,
                        crate::agent::events::Stage::Merge
                            | crate::agent::events::Stage::Orchestrating
                            | crate::agent::events::Stage::AgentFinished
                    )
                {
                    evidence.push(format!(
                        "{} · {} · {}",
                        event.worker,
                        event.target,
                        clip(&event.detail.to_string(), 1800)
                    ));
                }
            }
        }
        // The assistant turn is what the user saw. Models imitate the form of their own
        // earlier turns, so bookkeeping must not look like part of an answer: a plain
        // answer stays plain, and work that touched the repository carries its record in
        // a tagged block after the answer (stripped again if a model copies it).
        let plain = matches!(task.status, Status::Answered | Status::Inspected)
            && task.patch.files.is_empty()
            && task.validation.is_empty()
            && evidence.is_empty();
        let receipt = if plain {
            clip(&without_receipts(&task.summary), 6000)
        } else {
            format!(
                "{}\n\n<loom-record>\noutcome: {}\nevidence, most recent first (may be stale; reopen source before changing it):\n{}\nvalidation: {}\n</loom-record>",
                clip(&without_receipts(&task.summary), 6000),
                task.status.as_str(),
                clip(&evidence.iter().rev().cloned().collect::<Vec<_>>().join("\n"), 6000),
                clip(&serde_json::to_string(&task.validation)?, 1500)
            )
        };
        history.push(Message::user(clip(&task.goal, 6000)));
        history.push(Message {
            role: Role::Assistant,
            parts: vec![Part::Text(receipt)],
        });
        while history.len() > 2 && history.iter().map(|m| m.text().len()).sum::<usize>() > budget {
            history.drain(0..2);
        }
    }
    // Always retain the newest request/result pair, even for a tiny budget.
    Ok(history)
}

/// An answer without history bookkeeping a model copied from earlier turns: `<loom-record>`
/// blocks, and the receipt lines older histories used (`Outcome: Answered`, the evidence
/// list, `Validation: [...]`).
pub fn without_receipts(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("<loom-record>") {
        out.push_str(&rest[..start]);
        rest = match rest[start..].find("</loom-record>") {
            Some(end) => &rest[start + end + "</loom-record>".len()..],
            None => "",
        };
    }
    out.push_str(rest);
    let statuses = ["Running", "AwaitingApproval", "Applying", "Applied", "Rejected", "Answered", "Inspected", "Unresolved", "Cancelled"];
    let mut kept = Vec::new();
    let mut in_evidence = false;
    for line in out.lines() {
        let trimmed = line.trim();
        if trimmed.strip_prefix("Outcome: ").is_some_and(|s| statuses.contains(&s.trim())) {
            continue;
        }
        if trimmed.starts_with("Most recent observed evidence first") {
            in_evidence = true;
            continue;
        }
        if trimmed.starts_with("Validation: [") || trimmed == "Validation: []" {
            in_evidence = false;
            continue;
        }
        if in_evidence && (trimmed.contains(" · ") || trimmed.is_empty()) {
            continue;
        }
        in_evidence = false;
        kept.push(line);
    }
    let cleaned = kept.join("\n").trim().to_owned();
    if cleaned.is_empty() { text.trim().to_owned() } else { cleaned }
}

#[cfg(test)]
mod tests {
    use super::without_receipts;

    #[test]
    fn copied_history_bookkeeping_is_removed_from_answers() {
        let leaked = "Outcome: Answered\nOutcome: Answered\n하이하이! 😄\n오늘 기분은 좋아.\nMost recent observed evidence first (may be stale; reopen source before changing it):\nmain · /x/AGENTS.md · {\"completed\":true}\nValidation: []";
        assert_eq!(without_receipts(leaked), "하이하이! 😄\n오늘 기분은 좋아.");
        let tagged = "Done.\n\n<loom-record>\noutcome: Applied\n</loom-record>";
        assert_eq!(without_receipts(tagged), "Done.");
        // Ordinary text that merely mentions an outcome stays.
        assert_eq!(without_receipts("Outcome: the test passes."), "Outcome: the test passes.");
    }
}
