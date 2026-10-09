use bench_task::engine::*;
use std::collections::VecDeque;

struct Probe {
    replies: VecDeque<Result<Command, ModelError>>,
    contexts: Vec<Vec<String>>,
}
impl Probe {
    fn new(replies: Vec<Result<Command, ModelError>>) -> Self {
        Self {
            replies: replies.into(),
            contexts: vec![],
        }
    }
}
impl Model for Probe {
    fn next(&mut self, context: &[String]) -> Result<Command, ModelError> {
        self.contexts.push(context.to_vec());
        self.replies.pop_front().expect("extra model request")
    }
}
struct Memory {
    text: String,
    reads: usize,
    edits: usize,
}
impl Memory {
    fn new() -> Self {
        Self {
            text: "original".into(),
            reads: 0,
            edits: 0,
        }
    }
}
impl Files for Memory {
    fn read(&mut self, _: &str) -> Result<String, String> {
        self.reads += 1;
        Ok(self.text.clone())
    }
    fn replace(&mut self, _: &str, old: &str, new: &str) -> Result<(), String> {
        if !self.text.contains(old) {
            return Err("old text not found".into());
        }
        self.edits += 1;
        self.text = self.text.replacen(old, new, 1);
        Ok(())
    }
}
fn read() -> Result<Command, ModelError> {
    Ok(Command::Read {
        path: "value.txt".into(),
    })
}
fn finish() -> Result<Command, ModelError> {
    Ok(Command::Finish {
        summary: "done".into(),
    })
}

#[test]
fn retry_retains_the_exact_context_and_successful_edit() {
    let mut model = Probe::new(vec![
        read(),
        Ok(Command::Replace {
            path: "value.txt".into(),
            old: "original".into(),
            new: "changed".into(),
        }),
        Err(ModelError::Transient),
        finish(),
    ]);
    let mut files = Memory::new();
    let result = run(&mut model, &mut files, 4).unwrap();
    assert_eq!(model.contexts[2], model.contexts[3]);
    assert!(model.contexts[3].iter().any(|s| s.contains("original")));
    assert!(model.contexts[3].iter().any(|s| s.contains("EDIT")));
    assert_eq!(files.text, "changed");
    assert_eq!(files.reads, 1);
    assert_eq!(files.edits, 1);
    assert_eq!(result.model_calls, 4);
    assert_eq!(result.tool_calls, 2);
}

#[test]
fn auth_is_not_retried() {
    let mut model = Probe::new(vec![Err(ModelError::Auth)]);
    assert_eq!(
        run(&mut model, &mut Memory::new(), 10),
        Err(RunError::Model(ModelError::Auth))
    );
    assert_eq!(model.contexts.len(), 1);
}

#[test]
fn persistent_transients_stop_after_three_total_attempts() {
    let mut model = Probe::new(vec![Err(ModelError::Transient); 3]);
    assert_eq!(
        run(&mut model, &mut Memory::new(), 20),
        Err(RunError::Model(ModelError::Transient))
    );
    assert_eq!(model.contexts.len(), 3);
}

#[test]
fn recovery_does_not_exceed_the_shared_call_limit() {
    let mut model = Probe::new(vec![Err(ModelError::Transient)]);
    assert_eq!(run(&mut model, &mut Memory::new(), 1), Err(RunError::Limit));
    assert_eq!(model.contexts.len(), 1);
    let mut empty = Probe::new(vec![]);
    assert_eq!(run(&mut empty, &mut Memory::new(), 0), Err(RunError::Limit));
    assert!(empty.contexts.is_empty());
}

#[test]
fn successful_responses_reset_the_per_request_failure_counter() {
    let mut model = Probe::new(vec![
        Err(ModelError::Transient),
        Err(ModelError::Transient),
        read(),
        Err(ModelError::Transient),
        Err(ModelError::Transient),
        finish(),
    ]);
    let result = run(&mut model, &mut Memory::new(), 6).unwrap();
    assert_eq!(result.model_calls, 6);
    assert_eq!(result.tool_calls, 1);
}

#[test]
fn failed_tool_is_real_feedback_and_not_a_success_receipt() {
    let mut model = Probe::new(vec![
        Ok(Command::Replace {
            path: "value.txt".into(),
            old: "stale".into(),
            new: "changed".into(),
        }),
        finish(),
    ]);
    let mut files = Memory::new();
    let result = run(&mut model, &mut files, 2).unwrap();
    assert!(model.contexts[1]
        .iter()
        .any(|s| s.starts_with("ERROR: ") && s.contains("old text not found")));
    assert!(!model.contexts[1]
        .iter()
        .any(|s| s.contains("EDIT value.txt: done")));
    assert_eq!(files.text, "original");
    assert_eq!(files.edits, 0);
    assert_eq!(result.tool_calls, 1);
}
