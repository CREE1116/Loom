use crate::model::Summary;

pub fn render(summary: &Summary) -> String {
    let usage = |value: Option<u64>| {
        value
            .map(|n| n.to_string())
            .unwrap_or_else(|| "미보고".into())
    };
    format!(
        "완료 {} · 실패 {} · 중지 {}\n시간 {}ms · 입력 {} · 출력 {}",
        summary.success,
        summary.failed,
        summary.cancelled,
        summary.elapsed_ms,
        usage(summary.input_tokens),
        usage(summary.output_tokens)
    )
}
