use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fs, path::Path};
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    Korean,
    English,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Accent {
    #[default]
    Cyan,
    White,
    Hacker,
    Purple,
    Amber,
}
impl Accent {
    pub const ALL: [Self; 5] = [
        Self::Cyan,
        Self::White,
        Self::Hacker,
        Self::Purple,
        Self::Amber,
    ];

    pub fn key(self) -> &'static str {
        match self {
            Self::Cyan => "cyan",
            Self::White => "white",
            Self::Hacker => "hacker",
            Self::Purple => "purple",
            Self::Amber => "amber",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|accent| accent.key() == value)
    }

    pub fn label(self, language: Language) -> &'static str {
        match (self, language) {
            (Self::Cyan, Language::Korean) => "하늘색 (기본)",
            (Self::White, Language::Korean) => "흰색",
            (Self::Hacker, Language::Korean) => "형광초록",
            (Self::Purple, Language::Korean) => "보라색",
            (Self::Amber, Language::Korean) => "호박색",
            (Self::Cyan, Language::English) => "Cyan (default)",
            (Self::White, Language::English) => "White",
            (Self::Hacker, Language::English) => "Hacker green",
            (Self::Purple, Language::English) => "Purple",
            (Self::Amber, Language::English) => "Amber",
        }
    }
}
/// Workspace choices are isolated by engine; effort belongs to a specific model.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ModelSelection {
    pub model: String,
    pub efforts: BTreeMap<String, Option<String>>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub language: Language,
    pub panel_open: bool,
    pub activity_panel: bool,
    pub accent: Accent,
    pub density: Density,
    pub tool_details: ToolDetails,
    pub progress_labels: ProgressLabels,
    pub show_tokens: bool,
    pub timestamps: bool,
    pub enter_mode: EnterMode,
    pub paste_confirm: PasteConfirm,
    pub input_history: InputHistory,
    pub notify_completed: bool,
    pub notify_approval: bool,
    pub notify_errors: bool,
    pub terminal_bell: bool,
    pub startup_screen: StartupScreen,
    pub restore_session: bool,
    pub reopen_workspace: bool,
    pub core_steps: usize,
    pub core_seconds: u64,
    pub core_call_seconds: u64,
    pub core_attempts: usize,
    pub experimental_explore: bool,
    pub experimental_rewind: bool,
    pub last_view: String,
    pub model_selections: BTreeMap<String, ModelSelection>,
    /// Convert a downloaded GGUF to LMX.
    pub lmx_convert: bool,
    /// Keep the downloaded GGUF after converting it.
    pub lmx_keep_original: bool,
}
macro_rules! preference_enum {
    ($name:ident, $default:ident $(, $variant:ident)*) => {
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(rename_all = "lowercase")]
        pub enum $name { #[default] $default, $($variant),* }
    };
}
preference_enum!(Density, Normal, Compact);
preference_enum!(ToolDetails, Auto, Expanded, Collapsed);
preference_enum!(ProgressLabels, Stages, Minimal);
preference_enum!(EnterMode, Queue, Immediate);
preference_enum!(PasteConfirm, Multiline, Always, Never);
preference_enum!(InputHistory, Keep, Clear);
preference_enum!(StartupScreen, Chat, Last);
impl Default for Preferences {
    fn default() -> Self {
        Self {
            language: Language::Korean,
            panel_open: false,
            activity_panel: false,
            accent: Accent::default(),
            density: Density::Normal,
            tool_details: ToolDetails::Auto,
            progress_labels: ProgressLabels::Stages,
            show_tokens: true,
            timestamps: false,
            enter_mode: EnterMode::Queue,
            paste_confirm: PasteConfirm::Multiline,
            input_history: InputHistory::Keep,
            notify_completed: true,
            notify_approval: true,
            notify_errors: true,
            terminal_bell: false,
            startup_screen: StartupScreen::Chat,
            restore_session: true,
            reopen_workspace: false,
            core_steps: 80,
            core_seconds: 600,
            core_call_seconds: 90,
            core_attempts: 3,
            experimental_explore: true,
            experimental_rewind: true,
            last_view: "chat".into(),
            model_selections: BTreeMap::new(),
            lmx_convert: true,
            lmx_keep_original: true,
        }
    }
}
impl Preferences {
    pub fn resolve_model(
        &self,
        engine: &str,
        model: &mut Option<String>,
        effort: &mut Option<String>,
    ) {
        let Some(saved) = self.model_selections.get(engine) else {
            return;
        };
        if model.is_none() && !saved.model.is_empty() {
            *model = Some(saved.model.clone());
        }
        if effort.is_none() {
            *effort = model
                .as_ref()
                .and_then(|model| saved.efforts.get(model))
                .cloned()
                .flatten();
        }
    }

    pub fn set(&mut self, key: &str, value: &str) -> bool {
        let Ok(mut data) = serde_json::to_value(&*self) else {
            return false;
        };
        if data.get(key).is_none() || matches!(key, "last_view" | "model_selections") {
            return false;
        }
        data[key] = if data[key].is_boolean() {
            match value {
                "true" => true.into(),
                "false" => false.into(),
                _ => return false,
            }
        } else if data[key].is_number() {
            let Ok(value) = value.parse::<u64>() else {
                return false;
            };
            value.into()
        } else {
            value.into()
        };
        let Ok(updated) = serde_json::from_value(data) else {
            return false;
        };
        let updated: Self = updated;
        if updated.core_call_seconds == 0 || !(1..=5).contains(&updated.core_attempts) { return false; }
        *self = updated;
        true
    }
}

pub fn launcher_state_dir() -> std::path::PathBuf {
    std::env::var_os("LOOM_SETTINGS_STATE_DIR")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::path::PathBuf::from(
                std::env::var_os("HOME")
                    .or_else(|| std::env::var_os("USERPROFILE"))
                    .unwrap_or_default(),
            )
            .join(".loom")
        })
}
pub fn remembered_workspace() -> Option<std::path::PathBuf> {
    let path: std::path::PathBuf =
        serde_json::from_slice(&fs::read(launcher_state_dir().join("last-workspace.json")).ok()?)
            .ok()?;
    (path.is_dir() && load(&path).ok()?.reopen_workspace).then_some(path)
}
pub fn remember_workspace(cwd: &Path, preferences: &Preferences) -> Result<()> {
    let dir = launcher_state_dir();
    let file = dir.join("last-workspace.json");
    if preferences.reopen_workspace {
        fs::create_dir_all(&dir)?;
        let temp = dir.join(format!("last-workspace-{}.tmp", std::process::id()));
        fs::write(&temp, serde_json::to_vec(cwd)?)?;
        fs::rename(temp, file)?;
    } else if fs::read(&file)
        .ok()
        .and_then(|data| serde_json::from_slice::<std::path::PathBuf>(&data).ok())
        .as_deref()
        == Some(cwd)
    {
        fs::remove_file(file)?;
    }
    Ok(())
}
pub fn load(cwd: &Path) -> Result<Preferences> {
    let path = cwd.join(".custom-tui/settings.json");
    if !path.exists() {
        return Ok(Preferences::default());
    }
    Ok(serde_json::from_slice(&fs::read(path)?)?)
}
pub fn save(cwd: &Path, preferences: &Preferences) -> Result<()> {
    let dir = cwd.join(".custom-tui");
    fs::create_dir_all(&dir)?;
    let file = dir.join(format!("settings-{}.tmp", std::process::id()));
    fs::write(&file, serde_json::to_vec_pretty(preferences)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600))?;
    }
    fs::rename(file, dir.join("settings.json"))?;
    Ok(())
}
pub fn label(language: Language, text: &str) -> &str {
    if language == Language::Korean {
        return text;
    }
    match text {
        "대화" => "Chat",
        "활동" => "Activity",
        "작업" => "Tasks",
        "환경" => "Environment",
        "현황" => "Overview",
        "목록" => "List",
        "변경" => "Changes",
        "사용량" => "Usage",
        "diff" => "Diff",
        "승인" => "Approval",
        "권한" => "Permissions",
        "스킬" => "Skills",
        "모델" => "Model",
        "기록" => "History",
        "세션" => "Sessions",
        "세션 관리" => "Sessions",
        "설정" => "Settings",
        "도움말" => "Help",
        "/ 명령" => "/ Menu",
        "접기" => "Hide",
        "패널 열기" => "Show panel",
        "중단" => "Stop",
        "전송" => "Send",
        "열람 전용" => "Read only",
        "메시지 입력…" => "Message…",
        "Enter 전송  / 명령  $ 스킬  Ctrl+Q 종료" => {
            "Enter send  / commands  $ skills  Ctrl+Q exit"
        }
        "Tab 이동  Enter 선택  Esc 입력" => "Tab navigate  Enter select  Esc compose",
        "←→ ↑↓ 이동  ↵ 선택" => "←→ ↑↓ move  ↵ select",
        "방향키 / Tab 이동  Enter 선택  Esc 입력" => {
            "Arrows / Tab navigate  Enter select  Esc compose"
        }
        "↑↓ 선택  Enter 실행  Esc 닫기" => "↑↓ select  Enter confirm  Esc close",
        "스킬 · ↑↓ 선택 · Enter 추가" => "Skills · ↑↓ select · Enter add",
        "명령 · ↑↓ 선택 · Enter 실행" => "Commands · ↑↓ select · Enter execute",
        "명령 안내" => "Commands",
        "이전 대화" => "History",
        "모델 선택" => "Model",
        "선택한 모델" => "Selected model",
        "스킬 선택" => "Skills",
        "변경 검토" => "Changes",
        "승인 요청" => "Approval",
        "권한 관리" => "Permissions",
        "다음 메시지에 사용할 모델" => "Model for the next message",
        "설치된 스킬 선택" => "Choose an installed skill",
        "이 세션의 모델 변경" => "Change this session's model",
        "조작과 명령 안내" => "Controls and commands",
        "파일 변경 검토" => "Review file changes",
        "승인 요청 검토" => "Review pending approvals",
        "에이전트와 새 터미널" => "Agents and terminal windows",
        "새 대화 시작" => "Start a new conversation",
        "이전 대화 목록" => "Open conversation history",
        "최근 대화 재개" => "Resume latest conversation",
        "화면 종료" => "Close the UI",
        "UI 언어·악센트 색상·작업 패널" => "Language, accent color and task panel",
        "이 세션의 권한과 승인 정책" => {
            "Permission and approval controls for this session"
        }
        "준비됨" => "Ready",
        "완료" => "Done",
        "실행 중" => "Running",
        "실타래를 살피는 중" => "Following the threads",
        "설계를 엮는 중" => "Weaving the plan",
        "코드의 결을 읽는 중" => "Reading the weave",
        "코드를 짜는 중" => "Weaving the code",
        "답변을 엮는 중" => "Weaving the response",
        "한 줄씩 직조하는 중" => "Weaving line by line",
        "코드의 매듭을 확인하는 중" => "Checking the knots",
        "실행 결과를 살피는 중" => "Inspecting the weave",
        "작업을 맞춰보는 중" => "Fitting the pieces",
        "연결된 runtime 활동" => "Connected runtime activities",
        "토큰·할당량" => "Tokens and quota",
        "할당량 새로고침" => "Refresh quota",
        "지원 effort 정보 없음" => "Effort support unavailable",
        "중단됨" => "Interrupted",
        "실패" => "Failed",
        "아직 하위 에이전트가 없습니다" => "No child agents yet",
        "현재 세션의 에이전트" => "Agents in this session",
        "아직 생성된 하위 에이전트가 없습니다." => "No child agents yet.",
        "새 터미널 열기" => "Open in a terminal",
        "파일 변경" => "File changes",
        "대기 중인 승인 요청이 없습니다." => "No pending approvals.",
        "이전 대화를 불러오는 중…" => "Loading history…",
        "이 작업 폴더의 저장된 대화가 없습니다" => {
            "No saved conversations in this folder."
        }
        "이 대화까지 새 분기" => "Branch through this exchange",
        "새 분기 · 이 대화까지 포함 · 원본 유지" => {
            "Branch through this exchange · keep original"
        }
        "무엇을 만들까요? 아래에 지시를 입력하세요." => {
            "Enter a message to start."
        }
        "실행과 입력은 메인 창에서" => "Use the main window to send messages.",
        "다음 메시지부터 선택한 모델을 사용합니다" => {
            "The next message will use the selected model."
        }
        _ => text,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn model_memory_survives_disk_round_trip_without_crossing_engines() {
        let raw = serde_json::json!({"model_selections": {
            "v5": {"model": "ollama/spark", "efforts": {"ollama/spark": "none", "auto": null}},
            "codex": {"model": "other", "efforts": {"other": "high"}}
        }});
        let preferences: Preferences = serde_json::from_value(raw.clone()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        save(dir.path(), &preferences).unwrap();
        let restored = serde_json::to_value(load(dir.path()).unwrap()).unwrap();
        assert_eq!(restored["model_selections"], raw["model_selections"]);
    }
    #[test]
    fn preferences_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let value = Preferences {
            language: Language::English,
            panel_open: false,
            accent: Accent::Hacker,
            ..Preferences::default()
        };
        save(dir.path(), &value).unwrap();
        let loaded = load(dir.path()).unwrap();
        assert_eq!(loaded.language, Language::English);
        assert!(!loaded.panel_open);
        assert_eq!(loaded.accent, Accent::Hacker);
    }
    #[test]
    fn older_settings_keep_default_palette() {
        let value: Preferences =
            serde_json::from_str(r#"{"language":"english","panel_open":true}"#).unwrap();
        assert_eq!(value.accent, Accent::Cyan);
        assert_eq!(value.language, Language::English);
        assert!(value.panel_open);
    }
    #[test]
    fn all_options_round_trip_and_invalid_values_preserve_settings() {
        let dir = tempfile::tempdir().unwrap();
        let mut preferences = Preferences::default();
        for (key, value) in [
            ("density", "compact"),
            ("tool_details", "expanded"),
            ("progress_labels", "minimal"),
            ("timestamps", "true"),
            ("enter_mode", "immediate"),
            ("paste_confirm", "never"),
            ("input_history", "clear"),
            ("notify_completed", "false"),
            ("notify_approval", "false"),
            ("notify_errors", "false"),
            ("terminal_bell", "true"),
            ("startup_screen", "last"),
            ("restore_session", "false"),
            ("reopen_workspace", "true"),
            ("show_tokens", "false"),
            ("core_steps", "160"),
            ("core_seconds", "1200"),
            ("experimental_explore", "false"),
            ("experimental_rewind", "false"),
        ] {
            assert!(preferences.set(key, value));
        }
        let data = serde_json::to_value(&preferences).unwrap();
        assert!(!preferences.set("density", "unknown"));
        assert!(!preferences.set("restore_session", "maybe"));
        assert!(!preferences.set("core_steps", "many"));
        assert_eq!(preferences.core_steps, 160);
        assert_eq!(preferences.core_seconds, 1200);
        assert_eq!(data, serde_json::to_value(&preferences).unwrap());
        save(dir.path(), &preferences).unwrap();
        assert_eq!(
            data,
            serde_json::to_value(load(dir.path()).unwrap()).unwrap()
        );
    }
}
