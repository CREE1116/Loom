//! Menu ownership is independent from the physical pane used at each width.
use super::*;
impl App {
    fn in_work(&self) -> bool {
        matches!(
            self.view,
            View::Agents | View::Diff | View::Approvals | View::Rewind
        )
    }
    fn in_environment(&self) -> bool {
        matches!(self.view, View::Models | View::Providers | View::Routing | View::Runtime | View::Skills | View::Permissions)
    }
    pub(super) fn root_tabs(&self) -> Vec<(String, Action, bool)> {
        let work = if self.approvals.is_empty() {
            self.label("작업").to_owned()
        } else {
            format!("{} !{}", self.label("작업"), self.approvals.len())
        };
        vec![
            (
                self.label("대화").into(),
                Action::Back,
                self.view == View::Chat,
            ),
            (work, Action::Agents, self.in_work()),
            (
                self.label("세션").into(),
                Action::Sessions,
                self.view == View::Sessions,
            ),
            (
                self.label("환경").into(),
                Action::Command("/environment".into()),
                self.in_environment(),
            ),
            (
                self.label("설정").into(),
                Action::Command("/settings".into()),
                matches!(self.view, View::Settings | View::Help),
            ),
        ]
    }
    pub(super) fn navigation_rows(&self) -> Vec<(String, Tone, Option<Action>)> {
        let choices = if self.in_work() {
            vec![
                (
                    "목록",
                    Action::Agents,
                    self.view == View::Agents && !self.usage_expanded,
                ),
                ("변경", Action::Diff, self.view == View::Diff),
                ("승인", Action::Approvals, self.view == View::Approvals),
                (
                    "사용량",
                    Action::Command("/usage".into()),
                    self.view == View::Agents && self.usage_expanded,
                ),
            ]
        } else if self.in_environment() {
            vec![
                (
                    "모델",
                    Action::Command("/model".into()),
                    self.view == View::Models,
                ),
                ("연결", Action::Command("/provider".into()), self.view == View::Providers),
                ("라우팅", Action::Command("/routing".into()), self.view == View::Routing),
                ("추론 엔진", Action::Command("/runtime".into()), self.view == View::Runtime),
                (
                    "스킬",
                    Action::Command("/skills".into()),
                    self.view == View::Skills,
                ),
                (
                    "권한",
                    Action::Command("/permissions".into()),
                    self.view == View::Permissions,
                ),
            ]
        } else {
            return Vec::new();
        };
        let mut rows: Vec<_> = choices
            .into_iter()
            .map(|(label, action, active)| {
                let badge = if label == "승인" && !self.approvals.is_empty() {
                    format!(" !{}", self.approvals.len())
                } else {
                    String::new()
                };
                (
                    format!(
                        "{} {}{badge}",
                        if active { "●" } else { "○" },
                        self.label(label)
                    ),
                    if active {
                        Tone::Accent
                    } else if !badge.is_empty() {
                        Tone::Warning
                    } else {
                        Tone::Muted
                    },
                    Some(action),
                )
            })
            .collect();
        rows.push((String::new(), Tone::Normal, None));
        rows
    }
}
