//! Direct connections are managed locally, outside model conversation history.
use super::*;
use crate::agent::{AgentEvent, AgentUpdate};
use crate::agent::{InputRequest, Question, QuestionAnswer, QuestionOption};
use anyhow::Context;
use loom_core::models::registry::{Account, Registry, TEMPLATES};

pub const FORM_ID: &str = "loom-provider-registration";
impl App {
    pub fn refresh_providers(&mut self) {
        if let Ok(config) = Registry::open().and_then(|registry| registry.config()) { self.routing_max_cost = config.max_cost; }
        match Registry::open().and_then(|registry| registry.summaries()) {
            Ok(accounts) => self.provider_accounts = accounts,
            Err(error) => self.error(error.to_string()),
        }
    }
    pub(super) fn provider_command(&mut self, argument: &str) -> Option<Intent> {
        if argument == "policy free" && self.view == View::Models && !self.readonly {
            self.model = "auto".into();
            self.effort = None;
            self.remember_model_choice();
            return Some(Intent::Provider(argument.into()));
        }
        if !(self.view == View::Routing && (argument.starts_with("policy ") || argument.starts_with("route "))) { self.switch_view(View::Providers); }
        self.panel_open = true;
        if argument.is_empty() || argument == "list" {
            self.refresh_providers();
            return None;
        }
        if self.readonly {
            self.error("열람 창에서는 공급자 연결을 변경할 수 없습니다.");
            return None;
        }
        if argument == "cancel" {
            self.apply_event(AgentEvent::global(AgentUpdate::InputResolved(
                FORM_ID.into(),
            )));
            self.notify("연결 등록을 취소했습니다.");
            return None;
        }
        if argument == "add" {
            if !self.input_forms.is_empty() {
                self.error("현재 입력 요청을 먼저 마무리하세요.");
                return None;
            }
            let text = |id: &str, header: &str, prompt: &str, secret: bool| Question {
                id: id.into(),
                header: header.into(),
                prompt: prompt.into(),
                options: vec![],
                allow_custom: true,
                secret,
            };
            self.input_requested(InputRequest { id: FORM_ID.into(), blocking: false, questions: vec![
                Question { id: "provider".into(), header: "공급자".into(), prompt: "연결할 공급자를 선택하세요.".into(),
                    options: TEMPLATES.iter().map(|(name, _, _)| QuestionOption { label: (*name).into(), description: if *name == "compatible" {"OpenAI 호환 서버 · 주소 직접 지정".into()} else {"공급자에 직접 연결".into()} }).collect(), allow_custom: false, secret: false },
                text("name", "연결 이름", "연결 이름 (예: work, personal). 같은 공급자를 여러 계정으로 등록할 수 있습니다.", false),
                text("url", "서버 주소", "공급자 기본 주소를 쓰려면 default, 직접 지정하려면 https://…/v1 주소를 입력하세요.", false),
                text("key", "API 키", "API 키 또는 env:환경변수이름. 인증 없는 로컬 서버는 none. 키는 대화에 남지 않습니다.", true),
                Question { id: "routing".into(), header: "라우팅".into(), prompt: "이 연결을 자동 선택에 포함할까요?".into(),
                    options: vec![QuestionOption { label: "자동".into(), description: "Loom이 연결 상태에 따라 선택".into() }, QuestionOption { label: "직접 선택".into(), description: "모델 목록에서 직접 고를 때만 사용".into() }], allow_custom: false, secret: false },
                Question { id: "cost".into(), header: "비용".into(), prompt: "이 연결의 비용 유형을 선택하세요. 자동 라우팅은 허용한 비용 범위에서 동작합니다.".into(),
                    options: [("paid", "유료 API · 실제 청구는 공급자 기준"), ("free", "무료 API 요금제를 사용하는 연결"), ("local", "내 컴퓨터의 로컬 서버")].into_iter().map(|(label, description)| QuestionOption { label: label.into(), description: description.into() }).collect(), allow_custom: false, secret: false },
            ] });
            return None;
        }
        Some(Intent::Provider(argument.into()))
    }
    pub fn save_provider_answers(&mut self, answers: &[QuestionAnswer]) -> anyhow::Result<()> {
        let request = self
            .input_forms
            .iter()
            .find(|form| form.request.id == FORM_ID)
            .context("등록 요청이 없습니다.")?
            .request
            .clone();
        let values: std::collections::BTreeMap<_, _> =
            request.validate_answers(answers)?.into_iter().collect();
        let mut account = Account::template(&values["provider"], values["name"].trim())?;
        if values["url"].trim() != "default" {
            account.base_url = values["url"].trim().into();
        }
        account.automatic = values["routing"] == "자동";
        account.cost = loom_core::models::provider_cli::cost_class(&values["cost"])?;
        let key = values["key"].trim();
        let raw_key = if let Some(env) = key.strip_prefix("env:") {
            account.key_env = Some(env.into());
            None
        } else if key == "none" {
            None
        } else {
            Some(key)
        };
        let id = account.id.clone();
        Registry::open()?.put(account, raw_key)?;
        self.apply_event(AgentEvent::global(AgentUpdate::InputResolved(
            FORM_ID.into(),
        )));
        self.refresh_providers();
        self.open_provider_models(&id);
        self.notify("연결을 저장했습니다. 사용 가능한 모델을 조회합니다.");
        Ok(())
    }
    pub(super) fn provider_rows(&self, width: usize) -> Vec<(String, Tone, Option<Action>)> {
        let mut rows = vec![
            ("공급자 연결".into(), Tone::Accent, None),
            ("API 키와 계정 연결을 관리합니다.".into(), Tone::Muted, None),
            (String::new(), Tone::Normal, None),
            (
                "+ 연결 추가".into(),
                Tone::Accent,
                (!self.readonly).then(|| Action::Command("/provider add".into())),
            ),
            (
                "ChatGPT 계정 연결".into(),
                Tone::Accent,
                (!self.readonly).then(|| Action::Command("/provider login chatgpt".into())),
            ),
            (
                "↻ 연결·모델 새로고침".into(),
                Tone::Muted,
                Some(Action::Command("/provider refresh".into())),
            ),
            (String::new(), Tone::Normal, None),
        ];
        if self
            .input_forms
            .iter()
            .any(|form| form.request.id == FORM_ID)
        {
            rows.push(("등록 계속".into(), Tone::Accent, Some(Action::Questions)));
            rows.push((
                "등록 취소".into(),
                Tone::Muted,
                Some(Action::Command("/provider cancel".into())),
            ));
        }
        if self.provider_accounts.is_empty() {
            rows.push(("등록한 연결이 없습니다.".into(), Tone::Muted, None));
        }
        for summary in &self.provider_accounts {
            let a = &summary.account;
            rows.push((
                one_line(
                    &format!(
                        "{} {} · {}",
                        if a.enabled { "●" } else { "○" },
                        a.id,
                        a.provider
                    ),
                    width,
                ),
                if a.enabled { Tone::Accent } else { Tone::Muted },
                None,
            ));
            for line in wrap(
                &format!(
                    "{} · {} · {}",
                    if a.automatic {
                        "자동 선택"
                    } else {
                        "직접 선택"
                    },
                    if summary.authenticated {
                        "인증 설정됨"
                    } else {
                        "인증 없음"
                    },
                    match a.cost {
                        loom_core::models::CostClass::Local => "로컬",
                        loom_core::models::CostClass::Free => "무료",
                        loom_core::models::CostClass::Subscription => "구독",
                        loom_core::models::CostClass::Paid => "유료 API",
                        _ => "비용 미확인",
                    }
                ),
                width,
            ) {
                rows.push((line, Tone::Muted, None));
            }
            for line in wrap(&a.base_url, width) {
                rows.push((line, Tone::Muted, None));
            }
            rows.push(("모델 선택".into(), Tone::Accent, Some(Action::Command(format!("/model search {}/", a.id)))));
            rows.push((
                "연결 확인".into(),
                Tone::Accent,
                Some(Action::Command(format!("/provider test {}", a.id))),
            ));
            if a.provider == "chatgpt" {
                rows.push((
                    (if summary.authenticated {
                        "ChatGPT 요금제 사용 가능"
                    } else {
                        "다시 로그인하여 요금제 사용 승인"
                    })
                    .into(),
                    Tone::Muted,
                    None,
                ));
                rows.push((
                    "사용량 관리 ↗".into(),
                    Tone::Accent,
                    Some(Action::OpenLink(
                        "https://chatgpt.com/settings/usage".into(),
                    )),
                ));
                if !self.readonly {
                    rows.push((
                        "다시 로그인".into(),
                        Tone::Accent,
                        Some(Action::Command(format!("/provider login chatgpt {}", a.id))),
                    ));
                    rows.push((
                        "로그아웃".into(),
                        Tone::Muted,
                        Some(Action::Command(format!("/provider logout {}", a.id))),
                    ));
                }
            }
            if !self.readonly {
                rows.push((
                    (if a.enabled {
                        "비활성화"
                    } else {
                        "활성화"
                    })
                    .into(),
                    Tone::Muted,
                    Some(Action::Command(format!(
                        "/provider {} {}",
                        if a.enabled { "disable" } else { "enable" },
                        a.id
                    ))),
                ));
                rows.push((
                    "연결 삭제".into(),
                    Tone::Muted,
                    Some(Action::Command(format!("/provider remove {}", a.id))),
                ));
            }
            rows.push((String::new(), Tone::Normal, None));
        }
        rows
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registration_masks_keys_and_preserves_the_conversation_draft() {
        let mut app = App::new(false);
        app.editor.insert("keep draft");
        assert!(app.provider_command("add").is_none());
        assert_eq!(app.view, View::Providers);
        let request = &app.input_forms[0].request;
        assert_eq!(request.id, FORM_ID);
        assert!(
            request
                .questions
                .iter()
                .find(|q| q.id == "key")
                .unwrap()
                .secret
        );
        assert_eq!(app.editor.text, "keep draft");
        assert!(app.entries.is_empty());
        let mut read = App::new(true);
        read.provider_command("add");
        assert!(read.input_forms.is_empty());
    }
}
