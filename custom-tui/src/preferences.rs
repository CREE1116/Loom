use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[default]
    Korean,
    English,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Preferences {
    pub language: Language,
    pub panel_open: bool,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            language: Language::Korean,
            panel_open: true,
        }
    }
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
        "diff" => "Diff",
        "승인" => "Approval",
        "권한" => "Permissions",
        "스킬" => "Skills",
        "모델" => "Model",
        "기록" => "History",
        "세션 ▾" => "Sessions ▾",
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
        "↑↓ 선택  Enter 실행  Esc 닫기" => "↑↓ select  Enter confirm  Esc close",
        "스킬 · ↑↓ 선택 · Enter 추가" => "Skills · ↑↓ select · Enter add",
        "명령 · ↑↓ 선택 · Enter 실행" => "Commands · ↑↓ select · Enter execute",
        "명령 안내" => "Commands",
        "이전 대화" => "History",
        "모델 선택" => "Model",
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
        "UI 언어와 작업 패널" => "Language and task panel",
        "이 세션의 권한과 승인 정책" => {
            "Permission and approval controls for this session"
        }
        "준비됨" => "Ready",
        "완료" => "Done",
        "실행 중" => "Running",
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
    fn preferences_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let value = Preferences {
            language: Language::English,
            panel_open: false,
        };
        save(dir.path(), &value).unwrap();
        let loaded = load(dir.path()).unwrap();
        assert_eq!(loaded.language, Language::English);
        assert!(!loaded.panel_open);
    }
}
