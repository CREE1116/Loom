use crate::model::Summary;

pub fn render(_summary: &Summary) -> String {
    // TODO: Present observed values as a readable two-line result.
    "완료 0 · 실패 0 · 중지 0\n시간 0ms · 입력 0 · 출력 0".into()
}
