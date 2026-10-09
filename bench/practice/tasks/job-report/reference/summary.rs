use crate::model::{Job, Status, Summary};

pub fn summarize(jobs: &[Job]) -> Summary {
    let mut result = Summary {
        success: 0,
        failed: 0,
        cancelled: 0,
        elapsed_ms: 0,
        input_tokens: Some(0),
        output_tokens: Some(0),
    };
    for job in jobs {
        match job.status {
            Status::Success => result.success += 1,
            Status::Failed => result.failed += 1,
            Status::Cancelled => result.cancelled += 1,
        }
        result.elapsed_ms = result.elapsed_ms.saturating_add(job.elapsed_ms);
        result.input_tokens = result
            .input_tokens
            .zip(job.input_tokens)
            .map(|(a, b)| a.saturating_add(b));
        result.output_tokens = result
            .output_tokens
            .zip(job.output_tokens)
            .map(|(a, b)| a.saturating_add(b));
    }
    result
}
