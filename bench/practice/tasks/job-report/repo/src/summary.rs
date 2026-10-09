use crate::model::{Job, Summary};

pub fn summarize(_jobs: &[Job]) -> Summary {
    // TODO: Aggregate real records while preserving unknown usage.
    Summary {
        success: 0,
        failed: 0,
        cancelled: 0,
        elapsed_ms: 0,
        input_tokens: Some(0),
        output_tokens: Some(0),
    }
}
