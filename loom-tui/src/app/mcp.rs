//! MCP server status and OAuth sign-in (`.loom/config.toml` of the workspace).
use super::*;

#[derive(Clone, Debug)]
pub struct McpServer {
    pub name: String,
    pub target: String,
    pub oauth: bool,
    pub signed_in: bool,
    /// Tool count, or the discovery failure reason.
    pub status: std::result::Result<usize, String>,
}

/// Blocking: reads config, runs MCP discovery. Call from a worker thread.
pub fn load_servers(root: &std::path::Path) -> anyhow::Result<Vec<McpServer>> {
    let configured = loom_core::extensions::configured_servers(root)?;
    let status = loom_core::extensions::Extensions::open(root)?.server_status();
    Ok(configured
        .into_iter()
        .map(|(name, target, oauth)| {
            let status = status
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, s)| s.clone())
                .unwrap_or(Ok(0));
            let signed_in = oauth && loom_core::mcp_oauth::signed_in(&target);
            McpServer {
                name,
                target,
                oauth,
                signed_in,
                status,
            }
        })
        .collect())
}

impl App {
    pub(super) fn mcp_command(&mut self, argument: &str) -> Option<Intent> {
        self.panel_open = true;
        self.switch_view(View::Mcp);
        let words: Vec<_> = argument.split_whitespace().collect();
        match words.as_slice() {
            [] | ["refresh"] => {}
            ["login" | "logout" | "add", ..] if self.readonly => {
                self.error("열람 창에서는 MCP 연결을 변경할 수 없습니다.");
                return None;
            }
            ["login" | "logout", _] | ["add", _] | ["add", _, _] => {}
            _ => {
                self.error("사용법: /mcp [refresh|add NAME [URL]|login NAME|logout NAME]");
                return None;
            }
        }
        self.mcp_loading = true;
        Some(Intent::Mcp(argument.into()))
    }

    pub fn set_mcp_servers(&mut self, result: std::result::Result<Vec<McpServer>, String>) {
        self.mcp_loading = false;
        match result {
            Ok(servers) => self.mcp_servers = servers,
            Err(error) => self.error(error),
        }
    }

    pub(super) fn mcp_rows(&self, width: usize) -> Vec<(String, Tone, Option<Action>)> {
        let mut rows = vec![
            ("MCP 서버".into(), Tone::Accent, None),
            (
                "~/.loom/config.toml + 작업 폴더 .loom/config.toml".into(),
                Tone::Muted,
                None,
            ),
            (String::new(), Tone::Normal, None),
            (
                if self.mcp_loading {
                    "… 확인 중"
                } else {
                    "↻ 새로고침"
                }
                .into(),
                Tone::Muted,
                (!self.mcp_loading).then(|| Action::Command("/mcp refresh".into())),
            ),
            (String::new(), Tone::Normal, None),
        ];
        if self.mcp_servers.is_empty() && !self.mcp_loading {
            rows.push(("설정된 MCP 서버가 없습니다.".into(), Tone::Muted, None));
        }
        for server in &self.mcp_servers {
            let (mark, tone, detail) = match &server.status {
                _ if server.oauth && !server.signed_in => {
                    ("○", Tone::Warning, "로그인 필요".to_owned())
                }
                Ok(n) => ("●", Tone::Accent, format!("도구 {n}개")),
                Err(reason) => ("✕", Tone::Warning, reason.clone()),
            };
            rows.push((format!("{mark} {} · {detail}", server.name), tone, None));
            rows.push((
                format!("  {}", truncate(&server.target, width.saturating_sub(2))),
                Tone::Muted,
                None,
            ));
            if server.oauth && !self.readonly {
                let (label, command) = if server.signed_in {
                    ("  로그아웃", format!("/mcp logout {}", server.name))
                } else {
                    ("  브라우저로 로그인", format!("/mcp login {}", server.name))
                };
                rows.push((label.into(), Tone::Accent, Some(Action::Command(command))));
            }
        }
        let missing: Vec<_> = loom_core::extensions::PRESETS
            .iter()
            .filter(|(name, ..)| !self.mcp_servers.iter().any(|s| s.name == *name))
            .collect();
        if !missing.is_empty() && !self.readonly && !self.mcp_loading {
            rows.push((String::new(), Tone::Normal, None));
            rows.push(("기본 제공 서버".into(), Tone::Muted, None));
            for (name, url, ..) in missing {
                rows.push((
                    format!("+ {name} 추가"),
                    Tone::Accent,
                    Some(Action::Command(format!("/mcp add {name}"))),
                ));
                rows.push((
                    format!("  {}", truncate(url, width.saturating_sub(2))),
                    Tone::Muted,
                    None,
                ));
            }
        }
        rows
    }
}

fn truncate(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('…');
    out
}
