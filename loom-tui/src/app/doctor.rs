use super::*;
use std::net::{SocketAddr, TcpStream};
use std::process::Command;
use std::time::Duration;
use std::time::{SystemTime, UNIX_EPOCH};

pub(super) fn comma(n: u64) -> String {
    let s = n.to_string();
    let mut result = String::new();
    let len = s.len();
    for (i, ch) in s.chars().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            result.push(',');
        }
        result.push(ch);
    }
    result
}

pub(super) fn check_tool(cmd: &str) -> Option<String> {
    Command::new(cmd)
        .arg("--version")
        .output()
        .ok()
        .and_then(|out| {
            if out.status.success() {
                String::from_utf8(out.stdout)
                    .ok()
                    .and_then(|s| s.lines().next().map(|l| l.trim().to_string()))
            } else {
                None
            }
        })
}

pub(super) fn check_tcp_port(port: u16) -> bool {
    if let Ok(addr) = format!("127.0.0.1:{port}").parse::<SocketAddr>() {
        TcpStream::connect_timeout(&addr, Duration::from_millis(30)).is_ok()
    } else {
        false
    }
}

impl App {
    pub(crate) fn run_doctor(&mut self) {
        let mut report = Vec::new();
        report.push("# 🩺 Loom Doctor 진단 보고서".to_string());
        report.push("## 🛠 시스템 및 Git 환경".to_string());
        let cwd = std::env::current_dir()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "알 수 없음".into());
        report.push(format!("- 작업 디렉터리: `{cwd}`"));

        let git_branch = Command::new("git")
            .args(["rev-parse", "--abbrev-ref", "HEAD"])
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    String::from_utf8(o.stdout).ok()
                } else {
                    None
                }
            })
            .map(|s| s.trim().to_string());

        if let Some(branch) = git_branch {
            let dirty_count = Command::new("git")
                .args(["status", "--porcelain"])
                .output()
                .ok()
                .and_then(|o| {
                    if o.status.success() {
                        String::from_utf8(o.stdout).ok()
                    } else {
                        None
                    }
                })
                .map(|s| s.lines().count())
                .unwrap_or(0);

            report.push(format!(
                "- Git 저장소: 브랜치 `{branch}` (수정/미추적 파일: {dirty_count}개)"
            ));
        } else {
            report.push("- Git 저장소: 감지되지 않음 (일반 디렉터리)".into());
        }

        report.push(String::new());
        report.push("## 🧰 개발 도구체인 (Toolchains)".to_string());
        for tool in ["rustc", "cargo", "git", "python3", "node"] {
            if let Some(ver) = check_tool(tool) {
                let first_part = ver.split_whitespace().take(3).collect::<Vec<_>>().join(" ");
                report.push(format!("- `{tool}`: ✓ {first_part}"));
            } else {
                report.push(format!("- `{tool}`: ✗ 미설치 또는 PATH에 없음"));
            }
        }

        report.push(String::new());
        report.push("## 🧠 AI 코어 및 로컬 런타임 엔드포인트".to_string());
        report.push("- 런타임 코어: Loom Core V5".into());
        let model_display = if self.model.is_empty() {
            "자동 선택(Auto)".to_string()
        } else {
            self.model.clone()
        };
        report.push(format!("- 현재 활성 모델: `{model_display}`"));

        let ollama = check_tcp_port(11434);
        let lmstudio = check_tcp_port(1234);

        report.push(format!(
            "- Ollama (127.0.0.1:11434): {}",
            if ollama {
                "✓ 온라인 (연결 가능)"
            } else {
                "○ 오프라인"
            }
        ));
        report.push(format!(
            "- LM Studio (127.0.0.1:1234): {}",
            if lmstudio {
                "✓ 온라인 (연결 가능)"
            } else {
                "○ 오프라인"
            }
        ));

        report.push(String::new());
        report.push("## 🔒 권한 및 보안 상태".to_string());
        report.push(format!(
            "- 샌드박스 모드: {}",
            self.sandbox_mode
                .as_deref()
                .unwrap_or("기본값(workspaceWrite)")
        ));
        report.push(format!(
            "- 승인 정책: {}",
            self.approval_mode.as_deref().unwrap_or("기본값(onRequest)")
        ));
        report.push(format!(
            "- 열람 전용 모드: {}",
            if self.readonly {
                "예 (Readonly)"
            } else {
                "아니오 (Full Access)"
            }
        ));

        let body = report.join("\n");
        let id = format!(
            "doctor-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        );
        self.entries.push(Entry {
            id,
            kind: Kind::Assistant,
            title: "Loom Doctor · 환경 및 도구체인 진단".into(),
            body,
            status: "completed".into(),
            expanded: false,
        });
        self.notify("✓ 환경 및 도구체인 진단 완료 (/doctor)");
    }

    pub(crate) fn run_cost(&mut self) {
        let mut report = Vec::new();
        report.push("## 📊 세션 토큰 사용량 & 비용 경제성".to_string());

        let ctx_used = self
            .tokens
            .context_window
            .or(self.tokens.total)
            .unwrap_or(0);
        let max_ctx = 200_000u64;
        let ctx_pct = if max_ctx > 0 {
            (ctx_used as f64 / max_ctx as f64) * 100.0
        } else {
            0.0
        };
        report.push(format!(
            "- 컨텍스트 윈도우 사용: {} / {} tokens ({ctx_pct:.1}%)",
            comma(ctx_used),
            comma(max_ctx)
        ));

        let input_tokens = self.tokens.input.unwrap_or(0);
        let output_tokens = self.tokens.output.unwrap_or(0);
        let cached_tokens = self.tokens.cached_input.unwrap_or(0);
        let total_input = input_tokens + cached_tokens;
        let cache_rate = if total_input > 0 {
            (cached_tokens as f64 / total_input as f64) * 100.0
        } else {
            0.0
        };

        report.push(format!("- 입력 토큰: {} tokens", comma(input_tokens)));
        report.push(format!(
            "- 캐시 적중 토큰: {} tokens (적중률: {cache_rate:.1}%)",
            comma(cached_tokens)
        ));
        report.push(format!("- 출력 토큰: {} tokens", comma(output_tokens)));

        report.push(String::new());
        report.push("## 💰 비용 및 절감액 (Cost & Savings)".to_string());
        // Estimated standard commercial API pricing: $3.00/1M input, $15.00/1M output, $0.30/1M cached
        let est_cost = (input_tokens as f64 * 3.0 / 1_000_000.0)
            + (cached_tokens as f64 * 0.30 / 1_000_000.0)
            + (output_tokens as f64 * 15.0 / 1_000_000.0);
        let savings_from_cache = cached_tokens as f64 * 2.70 / 1_000_000.0;

        report.push(format!("- 예상 상용 API 비용: ${est_cost:.4}"));
        report.push(format!("- 프롬프트 캐싱 절감액: +${savings_from_cache:.4}"));
        report.push(
            "- 로컬 모델 라우팅 (Ollama/LM Studio): 비용 $0.00 (Zero-Cost Local Execution)".into(),
        );

        let body = report.join("\n");
        let id = format!(
            "cost-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        );
        self.entries.push(Entry {
            id,
            kind: Kind::Assistant,
            title: "비용 및 토큰 경제성 분석 (/cost)".into(),
            body,
            status: "completed".into(),
            expanded: false,
        });
        self.notify("✓ 토큰 및 비용 경제성 분석 완료 (/cost)");
    }

    pub(crate) fn run_context(&mut self) {
        let mut report = Vec::new();
        report.push("## 📂 현재 세션 컨텍스트 및 참조 파일".to_string());

        let session_id = self.thread.as_deref().unwrap_or("새 대화");
        report.push(format!("- 활성 세션: `{session_id}`"));

        if self.has_reported_changes() {
            report.push(format!(
                "- 수정된 파일: {} (확인: `/diff`)",
                self.reported_change_label()
            ));
        } else {
            report.push("- 수정된 파일: 없음 (작업 트리 변경 없음)".into());
        }

        report.push(String::new());
        report.push("## 💡 컨텍스트 조작 바로가기".to_string());
        report.push("- `@경로/파일명`: 프롬프트 작성 시 특정 파일 바로 참조".into());
        report.push("- `/diff`: 변경된 코드 차이점 한눈에 확인".into());
        report.push("- `/compression` 또는 `/compact`: 컨텍스트 압축하여 토큰 절약".into());
        report.push("- `/doctor`: 도구체인 및 로컬 모델 런타임 상태 진단".into());

        let body = report.join("\n");
        let id = format!(
            "context-{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        );
        self.entries.push(Entry {
            id,
            kind: Kind::Assistant,
            title: "세션 컨텍스트 및 파일 현황 (/context)".into(),
            body,
            status: "completed".into(),
            expanded: false,
        });
        self.notify("✓ 세션 컨텍스트 정보 확인 (/context)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doctor_cost_and_context_commands_produce_assistant_entries() {
        let mut app = App::new(false);
        assert_eq!(app.entries.len(), 0);

        app.run_doctor();
        assert_eq!(app.entries.len(), 1);
        assert_eq!(app.entries[0].kind, Kind::Assistant);
        assert!(app.entries[0].body.contains("Loom Core V5"));

        app.run_cost();
        assert_eq!(app.entries.len(), 2);
        assert_eq!(app.entries[1].kind, Kind::Assistant);
        assert!(app.entries[1].body.contains("비용 및 절감액"));

        app.run_context();
        assert_eq!(app.entries.len(), 3);
        assert_eq!(app.entries[2].kind, Kind::Assistant);
        assert!(app.entries[2].body.contains("컨텍스트"));
    }
}
