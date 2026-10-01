//! Codex protocol decoding. The renderer never receives JSON payloads.
use crate::agent::*;
use serde_json::{Value, json};
use std::collections::HashMap;

pub fn summary(thread: &Value) -> Option<SessionSummary> {
    let title = thread["name"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| thread["preview"].as_str())
        .unwrap_or("제목 없는 대화");
    Some(SessionSummary {
        id: thread["id"].as_str()?.into(),
        title: title
            .lines()
            .next()
            .unwrap_or(title)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "),
        status: thread["status"]["type"].as_str().unwrap_or("").into(),
        updated_at: thread["updatedAt"].as_u64(),
    })
}

pub fn snapshot(thread: &Value) -> Option<SessionSnapshot> {
    Some(SessionSnapshot {
        id: thread["id"].as_str()?.into(),
        title: thread["name"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .or_else(|| thread["preview"].as_str().filter(|s| !s.trim().is_empty()))
            .map(str::to_owned),
        status: thread["status"]["type"].as_str().map(str::to_owned),
        turns: thread["turns"].as_array().map(|turns| {
            turns
                .iter()
                .map(|turn| TurnSnapshot {
                    id: turn["id"].as_str().map(str::to_owned),
                    status: turn["status"].as_str().unwrap_or("").into(),
                    entries: turn["items"]
                        .as_array()
                        .map(|items| items.iter().filter_map(entry_from_item).collect())
                        .unwrap_or_default(),
                    activities: turn["items"]
                        .as_array()
                        .map(|items| items.iter().flat_map(activities).collect())
                        .unwrap_or_default(),
                })
                .collect()
        }),
    })
}

pub fn permissions(settings: &Value, defaults: bool) -> Permissions {
    if defaults {
        Permissions {
            approval: settings["approval_policy"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| {
                    settings["approval_policy"]
                        .is_object()
                        .then(|| "granular".into())
                }),
            sandbox: settings["sandbox_mode"].as_str().map(|s| {
                match s {
                    "read-only" => "readOnly",
                    "workspace-write" => "workspaceWrite",
                    "danger-full-access" => "dangerFullAccess",
                    other => other,
                }
                .into()
            }),
        }
    } else {
        Permissions {
            approval: (!settings["approvalPolicy"].is_null()).then(|| {
                settings["approvalPolicy"]
                    .as_str()
                    .unwrap_or("granular")
                    .into()
            }),
            sandbox: settings["sandboxPolicy"]["type"]
                .as_str()
                .or_else(|| settings["sandbox"]["type"].as_str())
                .map(str::to_owned),
        }
    }
}

pub fn activities(item: &Value) -> Vec<Agent> {
    if !matches!(
        item["type"].as_str(),
        Some("collabAgentToolCall" | "collabToolCall")
    ) {
        return vec![];
    }
    let mut ids: Vec<String> = item["receiverThreadIds"]
        .as_array()
        .map(|ids| {
            ids.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    for key in ["receiverThreadId", "newThreadId"] {
        if let Some(id) = item[key].as_str() {
            ids.push(id.into());
        }
    }
    ids.into_iter()
        .map(|id| {
            let state = &item["agentsStates"][&id];
            Agent {
                status: state["status"]
                    .as_str()
                    .or_else(|| item["agentStatus"].as_str())
                    .unwrap_or("working")
                    .into(),
                detail: state["message"]
                    .as_str()
                    .or_else(|| item["prompt"].as_str())
                    .unwrap_or("")
                    .into(),
                id,
                name: String::new(),
            }
        })
        .collect()
}

pub fn item_events(item: &Value) -> Vec<AgentUpdate> {
    let mut updates: Vec<_> = entry_from_item(item)
        .map(AgentUpdate::EntryUpdated)
        .into_iter()
        .collect();
    updates.extend(
        activities(item)
            .into_iter()
            .map(AgentUpdate::ActivityUpdated),
    );
    updates
}

pub fn notification(method: &str, params: &Value) -> Vec<AgentEvent> {
    let updates = match method {
        "serverRequest/resolved" => vec![AgentUpdate::ApprovalResolved(
            params["requestId"].to_string(),
        )],
        "turn/started" => vec![AgentUpdate::TurnStarted {
            id: params["turn"]["id"].as_str().map(str::to_owned),
        }],
        "turn/completed" => vec![AgentUpdate::TurnCompleted {
            id: params["turn"]["id"].as_str().map(str::to_owned),
            status: params["turn"]["status"]
                .as_str()
                .unwrap_or("completed")
                .into(),
            error: params["turn"]["error"]["message"]
                .as_str()
                .map(str::to_owned),
        }],
        "item/started" | "item/completed" => item_events(&params["item"]),
        "item/agentMessage/delta" | "item/commandExecution/outputDelta" | "item/plan/delta" => {
            vec![AgentUpdate::EntryDelta {
                id: params["itemId"].as_str().unwrap_or("stream").into(),
                kind: if method.contains("commandExecution") {
                    Kind::Tool
                } else {
                    Kind::Assistant
                },
                text: params["delta"].as_str().unwrap_or("").into(),
            }]
        }
        "turn/diff/updated" => vec![AgentUpdate::DiffUpdated(
            params["diff"].as_str().unwrap_or("").into(),
        )],
        "thread/tokenUsage/updated" => vec![AgentUpdate::UsageUpdated(format!(
            "tokens {} · context {}",
            params["tokenUsage"]["total"]["totalTokens"],
            params["tokenUsage"]["last"]["totalTokens"]
        ))],
        "thread/status/changed" => {
            let status = params["status"]["type"].as_str().unwrap_or("unknown");
            vec![AgentUpdate::StatusUpdated {
                status: status.into(),
                busy: status == "active",
            }]
        }
        "thread/settings/updated" => vec![AgentUpdate::PermissionsUpdated {
            permissions: permissions(&params["threadSettings"], false),
            defaults_only: false,
        }],
        "error" => vec![AgentUpdate::Error(
            params["error"]["message"]
                .as_str()
                .unwrap_or("Runtime error")
                .into(),
        )],
        _ => vec![],
    };
    updates
        .into_iter()
        .map(|update| AgentEvent {
            session_id: params["threadId"].as_str().map(str::to_owned),
            update,
        })
        .collect()
}

/// Reply payloads stay in the adapter; the UI sees only opaque choice tokens.
pub fn approval(
    id: &Value,
    method: &str,
    params: &Value,
) -> Option<(Approval, HashMap<String, Value>)> {
    let mut payloads = HashMap::new();
    let mut choices = Vec::new();
    let mut add = |label: &str, payload: Value| {
        let token = choices.len().to_string();
        payloads.insert(token.clone(), payload);
        choices.push(Choice {
            label: label.into(),
            result: token,
        });
    };
    let title = match method {
        "item/commandExecution/requestApproval" | "item/fileChange/requestApproval" => {
            let available = params["availableDecisions"].as_array();
            for (decision, label) in [
                ("accept", "이번 요청 허용"),
                ("acceptForSession", "이 세션에서 허용"),
                ("decline", "거절"),
                ("cancel", "취소"),
            ] {
                if decision == "acceptForSession" && available.is_none() {
                    continue;
                }
                if available.is_none_or(|list| list.iter().any(|v| v.as_str() == Some(decision))) {
                    add(label, json!({"decision":decision}));
                }
            }
            if method.contains("commandExecution") {
                "명령 실행 승인"
            } else {
                "파일 변경 승인"
            }
        }
        "item/permissions/requestApproval" => {
            add(
                "이번 턴 허용",
                json!({"permissions":params["permissions"],"scope":"turn"}),
            );
            add("권한 거절", json!({"permissions":{},"scope":"turn"}));
            "추가 권한 요청"
        }
        "mcpServer/elicitation/request" => {
            add("거절", json!({"action":"decline","content":null}));
            add("취소", json!({"action":"cancel","content":null}));
            "MCP 입력 요청"
        }
        _ => return None,
    };
    let mut summary = Vec::new();
    for (key, label) in [
        ("command", "명령"),
        ("cwd", "작업 위치"),
        ("grantRoot", "접근 경로"),
        ("reason", "요청 이유"),
    ] {
        if let Some(value) = params[key].as_str().filter(|s| !s.is_empty()) {
            summary.push(format!("{label}: {value}"));
        } else if let Some(values) = params[key].as_array() {
            summary.push(format!(
                "{label}: {}",
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
    }
    if !params["permissions"].is_null() {
        summary.push(format!("추가 권한: {}", params["permissions"]));
    }
    Some((
        Approval {
            id: id.to_string(),
            title: title.into(),
            summary: summary.join("\n"),
            detail: serde_json::to_string_pretty(params).unwrap_or_default(),
            choices,
            answering: false,
            expanded: false,
        },
        payloads,
    ))
}

pub fn turn_input(text: &str, skills: &[Skill]) -> Value {
    let mut input = vec![json!({"type":"text", "text":text})];
    input.extend(
        skills
            .iter()
            .map(|s| json!({"type":"skill", "name":s.name, "path":s.path})),
    );
    Value::Array(input)
}

pub fn session_page(result: &Value, append: bool) -> AgentUpdate {
    match result["data"].as_array() {
        Some(data) => AgentUpdate::SessionsLoaded {
            sessions: data.iter().filter_map(summary).collect(),
            cursor: result["nextCursor"].as_str().map(str::to_owned),
            append,
        },
        None => AgentUpdate::SessionsFailed(
            "세션 목록 응답이 올바르지 않습니다. 이전 목록을 유지합니다.".into(),
        ),
    }
}

pub fn entry_from_item(item: &Value) -> Option<Entry> {
    let id = item["id"].as_str()?.to_owned();
    let typ = item["type"].as_str()?;
    let (kind, title, body) = match typ {
        "userMessage" => (
            Kind::User,
            "› 나".into(),
            item["content"]
                .as_array()
                .map(|content| {
                    content
                        .iter()
                        .map(|v| v["text"].as_str().unwrap_or(""))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default(),
        ),
        "agentMessage" | "plan" => (
            Kind::Assistant,
            "● 에이전트".into(),
            item["text"].as_str().unwrap_or("").into(),
        ),
        "commandExecution" => (
            Kind::Tool,
            item["command"].as_str().unwrap_or("명령 실행").into(),
            match item["cwd"].as_str().filter(|cwd| !cwd.is_empty()) {
                Some(cwd) => format!(
                    "위치: {cwd}\n{}",
                    item["aggregatedOutput"].as_str().unwrap_or("")
                ),
                None => item["aggregatedOutput"].as_str().unwrap_or("").into(),
            },
        ),
        "fileChange" => (
            Kind::Change,
            "파일 변경".into(),
            item["changes"]
                .as_array()
                .map(|changes| {
                    changes
                        .iter()
                        .map(|v| {
                            format!(
                                "+++ {}\n{}",
                                v["path"].as_str().unwrap_or(""),
                                v["diff"].as_str().unwrap_or("")
                            )
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default(),
        ),
        "collabAgentToolCall" | "collabToolCall" => (
            Kind::Tool,
            "에이전트 작업".into(),
            item["prompt"]
                .as_str()
                .unwrap_or("활동 탭에서 에이전트별 작업을 확인하세요.")
                .into(),
        ),
        "reasoning" => return None,
        _ => (
            Kind::Tool,
            typ.into(),
            serde_json::to_string_pretty(item).unwrap_or_default(),
        ),
    };
    Some(Entry {
        id,
        kind,
        title,
        body,
        status: item["status"].as_str().unwrap_or("completed").into(),
        expanded: false,
    })
}

pub fn skills(result: &Value) -> (Vec<Skill>, Vec<String>) {
    let mut skills_found = Vec::<Skill>::new();
    let mut errors_found = Vec::new();
    if let Some(data) = result["data"].as_array() {
        for entry in data {
            if let Some(skills) = entry["skills"].as_array() {
                for skill in skills {
                    if skill["enabled"] == false {
                        continue;
                    }
                    if let (Some(name), Some(path)) =
                        (skill["name"].as_str(), skill["path"].as_str())
                        && !skills_found.iter().any(|s| s.path == path)
                    {
                        skills_found.push(Skill {
                            name: name.into(),
                            path: path.into(),
                            description: skill["interface"]["shortDescription"]
                                .as_str()
                                .or_else(|| skill["shortDescription"].as_str())
                                .or_else(|| skill["description"].as_str())
                                .unwrap_or("")
                                .into(),
                        });
                    }
                }
            }
            if let Some(errors) = entry["errors"].as_array() {
                for error in errors {
                    errors_found.push(
                        error["message"]
                            .as_str()
                            .unwrap_or("스킬을 읽지 못했습니다")
                            .into(),
                    );
                }
            }
        }
    }
    skills_found.sort_by(|a, b| a.name.cmp(&b.name));
    (skills_found, errors_found)
}

/// Decode provider questions into a bounded, provider-independent form.
pub fn input_request(id: &Value, params: &Value) -> anyhow::Result<InputRequest> {
    use anyhow::{Context, ensure};
    let questions = params["questions"]
        .as_array()
        .context("Missing questions")?;
    ensure!(
        !questions.is_empty() && questions.len() <= 16,
        "Expected 1–16 questions"
    );
    let mut seen = std::collections::HashSet::new();
    let mut decoded = Vec::new();
    for question in questions {
        let id = question["id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .context("Missing question ID")?;
        ensure!(seen.insert(id.to_owned()), "Duplicate question ID");
        let options = match &question["options"] {
            Value::Null => vec![],
            Value::Array(options) => {
                ensure!(options.len() <= 32, "Too many options");
                options
                    .iter()
                    .map(|option| {
                        Ok(QuestionOption {
                            label: option["label"]
                                .as_str()
                                .filter(|s| !s.trim().is_empty())
                                .context("Missing option label")?
                                .into(),
                            description: option["description"].as_str().unwrap_or("").into(),
                        })
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?
            }
            _ => anyhow::bail!("Malformed options"),
        };
        decoded.push(Question {
            id: id.into(),
            header: question["header"].as_str().unwrap_or("질문").into(),
            prompt: question["question"]
                .as_str()
                .context("Missing question text")?
                .into(),
            allow_custom: options.is_empty() || question["isOther"].as_bool().unwrap_or(false),
            secret: question["isSecret"].as_bool().unwrap_or(false),
            options,
        });
    }
    Ok(InputRequest {
        id: id.to_string(),
        questions: decoded,
        blocking: params["isBlocking"].as_bool().unwrap_or(true),
    })
}
