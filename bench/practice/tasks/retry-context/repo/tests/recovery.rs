use bench_task::engine::*;
use std::collections::VecDeque;

struct Replies(VecDeque<Result<Command, ModelError>>);
impl Model for Replies {
    fn next(&mut self, _context: &[String]) -> Result<Command, ModelError> {
        self.0.pop_front().expect("unexpected extra call")
    }
}

struct Memory {
    text: String,
    reads: usize,
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
        self.text = self.text.replacen(old, new, 1);
        Ok(())
    }
}

#[test]
fn temporary_model_failure_does_not_repeat_completed_read() {
    let mut model = Replies(
        vec![
            Ok(Command::Read {
                path: "value.txt".into(),
            }),
            Err(ModelError::Transient),
            Ok(Command::Finish {
                summary: "done".into(),
            }),
        ]
        .into(),
    );
    let mut files = Memory {
        text: "old".into(),
        reads: 0,
    };
    let report = run(&mut model, &mut files, 3).expect("temporary failure should recover");
    assert_eq!(report.model_calls, 3);
    assert_eq!(report.tool_calls, 1);
    assert_eq!(files.reads, 1);
}

#[test]
fn rejected_edit_can_be_corrected() {
    let mut model = Replies(
        vec![
            Ok(Command::Replace {
                path: "value.txt".into(),
                old: "stale".into(),
                new: "new".into(),
            }),
            Ok(Command::Read {
                path: "value.txt".into(),
            }),
            Ok(Command::Replace {
                path: "value.txt".into(),
                old: "old".into(),
                new: "new".into(),
            }),
            Ok(Command::Finish {
                summary: "fixed".into(),
            }),
        ]
        .into(),
    );
    let mut files = Memory {
        text: "old".into(),
        reads: 0,
    };
    let report = run(&mut model, &mut files, 4).expect("rejected edit should get feedback");
    assert_eq!(files.text, "new");
    assert_eq!(report.tool_calls, 3);
}
