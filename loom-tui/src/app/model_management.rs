//! Environment pages expose routing policy and owned local checkpoints.
use super::*;
use crate::agent::{AgentEvent, AgentUpdate, InputRequest, Question, QuestionAnswer};
use loom_core::models::{CostClass, native_runtime::Backend, registry::Registry};

pub const DOWNLOAD_FORM_ID: &str = "loom-checkpoint-download";
impl App {
    pub fn take_runtime_reload(&mut self) -> bool { std::mem::take(&mut self.runtime_reload) }
    pub fn refresh_runtime(&mut self) {
        match loom_core::models::checkpoints::installed() {
            Ok(models) => self.runtime_checkpoints = models,
            Err(error) => self.error(error.to_string()),
        }
        match Backend::configured() {
            Ok(backend) => self.runtime_backend = backend.label().into(),
            Err(error) => self.error(error.to_string()),
        }
    }
    pub fn save_download_answers(&mut self, answers: &[QuestionAnswer]) -> anyhow::Result<()> {
        let form = self.input_forms.iter().find(|f| f.request.id == DOWNLOAD_FORM_ID)
            .ok_or_else(|| anyhow::anyhow!("다운로드 입력 요청이 없습니다."))?;
        let values = form.request.validate_answers(answers)?;
        let spec = values.first().ok_or_else(|| anyhow::anyhow!("모델 주소를 입력하세요."))?.1.trim().to_owned();
        self.local_models.start(spec, self.preferences.lmx_convert, self.preferences.lmx_keep_original)?;
        self.apply_event(AgentEvent::global(AgentUpdate::InputResolved(DOWNLOAD_FORM_ID.into())));
        self.switch_view(View::Runtime);
        self.notify("모델 다운로드를 시작했습니다. 다른 작업을 계속할 수 있습니다.");
        Ok(())
    }
    pub(super) fn runtime_command(&mut self, argument: &str) -> Option<Intent> {
        self.switch_view(View::Runtime);
        self.panel_open = true;
        if argument.is_empty() || argument == "list" || argument == "refresh" {
            self.refresh_runtime(); return None;
        }
        if self.readonly { self.error("열람 창에서는 모델을 변경할 수 없습니다."); return None; }
        let mut save = false;
        let result = (|| -> anyhow::Result<()> {
            match argument.split_once(' ').unwrap_or((argument, "")) {
                ("pull", "") => {
                    anyhow::ensure!(self.input_forms.is_empty(), "현재 질문을 먼저 마무리하세요.");
                    self.input_requested(InputRequest { id: DOWNLOAD_FORM_ID.into(), blocking: false, questions: vec![Question {
                        id: "spec".into(), header: "모델 다운로드".into(), prompt: "Hugging Face 저장소, GGUF 파일 링크 또는 hf:소유자/저장소 [파일.gguf]".into(),
                        options: vec![], allow_custom: true, secret: false,
                    }] });
                }
                ("pull", spec) => self.local_models.start(spec.trim().into(), self.preferences.lmx_convert, self.preferences.lmx_keep_original)?,
                ("convert", value) => {
                    self.preferences.lmx_convert = value != "off";
                    save = true;
                }
                ("keep-original", value) => {
                    self.preferences.lmx_keep_original = value != "off";
                    save = true;
                }
                ("cancel", _) => self.local_models.cancel(),
                ("backend", mode) => {
                    Registry::open()?.set_inference_backend(mode)?;
                    self.refresh_runtime(); self.runtime_reload = true;
                    self.notify("추론 장치를 저장했습니다. 진행 중인 작업 뒤에 반영합니다.");
                }
                ("remove", id) => {
                    anyhow::ensure!(self.runtime_checkpoints.iter().any(|m| m.id == id), "설치한 모델을 찾을 수 없습니다.");
                    self.runtime_remove = Some(id.into());
                }
                ("confirm-remove", id) => {
                    anyhow::ensure!(self.runtime_remove.as_deref() == Some(id), "삭제할 모델을 먼저 선택하세요.");
                    anyhow::ensure!(!self.busy, "현재 작업이 끝난 뒤 모델을 삭제하세요.");
                    loom_core::models::checkpoints::remove(id)?;
                    self.runtime_remove = None; self.refresh_runtime(); self.runtime_reload = true;
                    if self.model == format!("loom/{id}") { self.model = "auto".into(); self.effort = None; self.remember_model_choice(); }
                }
                ("keep", _) => self.runtime_remove = None,
                _ => anyhow::bail!("/runtime [pull hf:소유자/저장소/파일.gguf|cancel|backend auto|cpu|mps|cuda|remove ID]"),
            }
            Ok(())
        })();
        if let Err(error) = result { self.error(error.to_string()); }
        save.then_some(Intent::SaveSettings)
    }
    pub(super) fn runtime_rows(&self, width: usize) -> Vec<(String, Tone, Option<Action>)> {
        let command = |text: &str| (!self.readonly).then(|| Action::Command(text.into()));
        let mut rows = vec![("로컬 추론 엔진".into(), Tone::Accent, None)];
        panel_section(&mut rows, "추론 장치", width);
        for (mode, label, supported) in [("auto", "자동", true), ("cpu", "CPU", true), ("mps", "Metal (MPS)", cfg!(target_os="macos")), ("cuda", "CUDA", cfg!(feature="cuda"))] {
            rows.push((format!("{} {label}{}", if self.runtime_backend == mode {"●"} else {"○"}, if supported {""} else {" · 이 빌드에서 미지원"}), if supported {Tone::Normal} else {Tone::Muted}, if supported {command(&format!("/runtime backend {mode}"))} else {None}));
        }
        if std::env::var_os("LOOM_INFERENCE_BACKEND").is_some() { rows.push(("환경변수 LOOM_INFERENCE_BACKEND가 우선합니다.".into(), Tone::Warning, None)); }
        panel_section(&mut rows, "모델 관리", width);
        rows.push(("+ 모델 다운로드".into(), Tone::Accent, command("/runtime pull")));
        let toggle = |on: bool| if on {"●"} else {"○"};
        rows.push((format!("{} 받은 GGUF를 LMX로 변환", toggle(self.preferences.lmx_convert)), Tone::Normal,
            command(if self.preferences.lmx_convert {"/runtime convert off"} else {"/runtime convert on"})));
        if self.preferences.lmx_convert {
            rows.push((format!("{} 변환 후 원본 GGUF 보관", toggle(self.preferences.lmx_keep_original)), Tone::Normal,
                command(if self.preferences.lmx_keep_original {"/runtime keep-original off"} else {"/runtime keep-original on"})));
        }
        rows.push(("↻ 설치 목록 새로고침".into(), Tone::Muted, Some(Action::Command("/runtime refresh".into()))));
        if let Some(transfer) = &self.local_models.transfer {
            let what = if transfer.stage.is_empty() {"다운로드"} else {transfer.stage.as_str()};
            rows.extend(wrap(&format!("{what} · {}", transfer.spec), width).into_iter().map(|line| (line, Tone::Normal, None)));
            let progress = if transfer.total > 0 {format!("{:.1}% · {:.1}/{:.1} MB", transfer.done as f64 * 100.0 / transfer.total as f64, transfer.done as f64 / 1e6, transfer.total as f64 / 1e6)} else {format!("{:.1} MB · 크기 조회 중", transfer.done as f64 / 1e6)};
            rows.push((progress, Tone::Accent, None));
            if transfer.active {
                rows.push(((if transfer.cancelling {"취소 요청 중…"} else {"다운로드 취소"}).into(), Tone::Warning, (!transfer.cancelling).then(|| command("/runtime cancel")).flatten()));
            } else if transfer.cancelling {
                rows.push(("다운로드 취소됨 · 받은 부분은 재개할 때 사용".into(), Tone::Muted, None));
                rows.push(("이어받기".into(), Tone::Accent, command(&format!("/runtime pull {}", transfer.spec))));
            } else if let Some(result) = &transfer.result {
                match result {
                    Ok(_) => rows.push((if transfer.stage.is_empty() {"다운로드 완료 · 모델 목록에 반영됨"} else {"다운로드·LMX 변환 완료 · 모델 목록에 반영됨"}.into(), Tone::Success, None)),
                    Err(error) => {
                        rows.extend(wrap(error, width).into_iter().map(|line| (line, Tone::Warning, None)));
                        rows.push(("다시 다운로드".into(), Tone::Accent, command(&format!("/runtime pull {}", transfer.spec))));
                    }
                }
            }
        }
        rows.extend(wrap(&format!("저장 위치 · {}", loom_core::models::local::models_dir().display()), width).into_iter().map(|line| (line, Tone::Muted, None)));
        rows.push((format!("설치된 모델 · {}개", self.runtime_checkpoints.len()), Tone::Normal, None));
        for model in &self.runtime_checkpoints {
            rows.extend(wrap(&model.id, width).into_iter().map(|line| (line, Tone::Normal, None)));
            rows.push((format!("{:.2} GB", model.bytes as f64 / 1e9), Tone::Muted, None));
            let id = format!("loom/{}", model.id);
            if self.models.contains(&id) { rows.push(("이 모델 사용".into(), Tone::Accent, command(&format!("/model {id}")))); }
            rows.push(("모델 삭제".into(), Tone::Muted, command(&format!("/runtime remove {}", model.id))));
            if self.runtime_remove.as_deref() == Some(&model.id) {
                rows.push(("이 체크포인트 파일을 삭제할까요?".into(), Tone::Warning, None));
                rows.push(("삭제 확인".into(), Tone::Danger, command(&format!("/runtime confirm-remove {}", model.id))));
                rows.push(("유지".into(), Tone::Muted, command("/runtime keep")));
            }
        }
        rows
    }
    pub(super) fn routing_rows(&self, width: usize) -> Vec<(String, Tone, Option<Action>)> {
        let command = |text: String| (!self.readonly).then_some(Action::Command(text));
        let mut rows = vec![("자동 라우팅 설정".into(), Tone::Accent, None)];
        rows.extend(wrap("Loom이 허용한 비용·도구 지원·연결 상태로 선택합니다.", width).into_iter().map(|line| (line, Tone::Muted, None)));
        rows.push((format!("{} 자동 선택", if self.model == "auto" {"●"} else {"○"}), Tone::Accent, (!self.readonly).then(|| Action::Model("auto".into()))));
        if self.model != "auto" { rows.extend(wrap(&format!("현재 고정 모델 · {}", self.model), width).into_iter().map(|s| (s, Tone::Normal, None))); }
        panel_section(&mut rows, "비용 상한", width);
        for (cost, key, label) in [(CostClass::Local, "local", "로컬만"), (CostClass::Free, "free", "로컬·무료 API"), (CostClass::Subscription, "subscription", "로컬·무료·구독"), (CostClass::Paid, "paid", "유료 API 포함")] {
            rows.push((format!("{} {label}", if self.routing_max_cost == cost {"●"} else {"○"}), Tone::Normal, command(format!("/provider policy {key}"))));
        }
        rows.extend(wrap("후보가 없거나 실패하면 허용 범위 안에서 다음 후보를 확인합니다. 범위 밖 유료 모델로 넘어가지 않습니다.", width).into_iter().map(|s| (s, Tone::Muted, None)));
        panel_section(&mut rows, "연결별 자동 선택", width);
        if self.provider_accounts.is_empty() { rows.push(("등록한 외부 연결 없음 · 로컬 모델은 자동 탐색".into(), Tone::Muted, None)); }
        for summary in &self.provider_accounts {
            let a = &summary.account;
            rows.push((format!("{} {}{}", if a.enabled && a.automatic {"●"} else {"○"}, a.id, if !a.enabled {" · 비활성화"} else {""}), Tone::Normal, a.enabled.then(|| command(format!("/provider route {} {}", a.id, if a.automatic {"manual"} else {"auto"}))).flatten()));
        }
        rows.push(("공급자 연결 관리".into(), Tone::Accent, Some(Action::Command("/provider".into()))));
        rows
    }
}
