use bench_task::{
    model::{Job, Status, Summary},
    render::render,
    summary::summarize,
};

#[test]
fn empty_records_have_known_zero_totals() {
    assert_eq!(
        summarize(&[]),
        Summary {
            success: 0,
            failed: 0,
            cancelled: 0,
            elapsed_ms: 0,
            input_tokens: Some(0),
            output_tokens: Some(0)
        }
    );
}

#[test]
fn input_and_output_unknowns_are_independent_and_order_invariant() {
    let mut jobs = vec![
        Job {
            status: Status::Cancelled,
            elapsed_ms: 7,
            input_tokens: Some(4),
            output_tokens: None,
        },
        Job {
            status: Status::Success,
            elapsed_ms: 11,
            input_tokens: Some(6),
            output_tokens: Some(2),
        },
        Job {
            status: Status::Failed,
            elapsed_ms: 5,
            input_tokens: Some(3),
            output_tokens: Some(9),
        },
    ];
    let expected = Summary {
        success: 1,
        failed: 1,
        cancelled: 1,
        elapsed_ms: 23,
        input_tokens: Some(13),
        output_tokens: None,
    };
    assert_eq!(summarize(&jobs), expected);
    jobs.reverse();
    assert_eq!(summarize(&jobs), expected);
}

#[test]
fn totals_saturate_instead_of_wrapping_or_panicking() {
    let jobs = [
        Job {
            status: Status::Success,
            elapsed_ms: u64::MAX,
            input_tokens: Some(u64::MAX),
            output_tokens: Some(u64::MAX),
        },
        Job {
            status: Status::Success,
            elapsed_ms: 1,
            input_tokens: Some(1),
            output_tokens: Some(1),
        },
    ];
    let result = summarize(&jobs);
    assert_eq!(result.elapsed_ms, u64::MAX);
    assert_eq!(result.input_tokens, Some(u64::MAX));
    assert_eq!(result.output_tokens, Some(u64::MAX));
    assert_eq!(result.success, 2);
}

#[test]
fn renderer_uses_its_argument_including_zero_and_unknown() {
    let summary = Summary {
        success: 7,
        failed: 3,
        cancelled: 2,
        elapsed_ms: 89,
        input_tokens: Some(0),
        output_tokens: None,
    };
    assert_eq!(
        render(&summary),
        "완료 7 · 실패 3 · 중지 2\n시간 89ms · 입력 0 · 출력 미보고"
    );
}
