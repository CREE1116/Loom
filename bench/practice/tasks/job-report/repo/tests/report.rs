use bench_task::{
    model::{Job, Status},
    render::render,
    summary::summarize,
};

#[test]
fn reports_a_failed_call_without_inventing_usage() {
    let jobs = [
        Job {
            status: Status::Success,
            elapsed_ms: 12,
            input_tokens: Some(100),
            output_tokens: Some(7),
        },
        Job {
            status: Status::Failed,
            elapsed_ms: 8,
            input_tokens: None,
            output_tokens: Some(2),
        },
    ];
    assert_eq!(
        render(&summarize(&jobs)),
        "완료 1 · 실패 1 · 중지 0\n시간 20ms · 입력 미보고 · 출력 9"
    );
}
