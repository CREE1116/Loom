use bench_task::{
    model::{Job, Status},
    render::render,
    summary::summarize,
};

fn main() {
    let jobs = [
        Job {
            status: Status::Success,
            elapsed_ms: 30,
            input_tokens: Some(10),
            output_tokens: Some(3),
        },
        Job {
            status: Status::Failed,
            elapsed_ms: 20,
            input_tokens: None,
            output_tokens: Some(2),
        },
        Job {
            status: Status::Cancelled,
            elapsed_ms: 40,
            input_tokens: Some(8),
            output_tokens: Some(1),
        },
        Job {
            status: Status::Success,
            elapsed_ms: 50,
            input_tokens: Some(5),
            output_tokens: Some(6),
        },
    ];
    println!("{}", render(&summarize(&jobs)));
}
