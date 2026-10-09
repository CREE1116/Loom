//! Core descriptions of observed actions; reporting does not require another model call.
use serde_json::Value;

pub(super) fn short(text: &str, limit: usize) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(limit)
        .collect()
}

pub(super) fn describe(tool: &str, args: &Value) -> String {
    let path = args["path"].as_str().or(args["id"].as_str());
    if tool == "finish" {
        return "Deliver result".into();
    }
    if tool == "map" {
        return match path.filter(|path| !path.is_empty() && *path != ".") {
            Some(path) => format!("Repository structure · {}", short(path, 140)),
            None => "Repository structure".into(),
        };
    }
    let target = match tool {
        "check" => "Build and tests".to_owned(),
        "read" => {
            let path = path.unwrap_or("file");
            match (args["start"].as_u64(), args["end"].as_u64()) {
                (Some(start), Some(end)) => format!("{path}:{start}..{end}"),
                _ => path.to_owned(),
            }
        }
        "find" | "ast_search" => format!(
            "{} · {}",
            args["pattern"].as_str().unwrap_or(""),
            path.unwrap_or("repository")
        ),
        "symbols" => format!(
            "AST symbol search · {}{}",
            args["query"].as_str().unwrap_or(""),
            args["path"]
                .as_str()
                .filter(|path| !path.is_empty())
                .map(|path| format!(" · {path}"))
                .unwrap_or_default()
        ),
        "symbol_context" => format!("AST context · {}", args["id"].as_str().unwrap_or("symbol")),
        "inspect_symbol" => format!(
            "AST inspect · {}",
            args["query"].as_str().unwrap_or("symbol")
        ),
        "file_context" => format!("File graph · {}", path.unwrap_or("file")),
        "symbol_impact" => format!("AST impact · {}", args["id"].as_str().unwrap_or("symbol")),
        "read_many" => format!(
            "Batch code read · {} items",
            args["items"].as_array().map_or(0, Vec::len)
        ),
        "refs" => format!(
            "AST references · {}",
            args["id"].as_str().unwrap_or("symbol")
        ),
        "git_status" => "Git status".to_owned(),
        "git_diff" => {
            let path = path.unwrap_or("workspace");
            if args["staged"].as_bool().unwrap_or(false) {
                format!("Git staged diff · {path}")
            } else {
                format!("Git diff · {path}")
            }
        }
        "git_log" => format!("Git history · {}", path.unwrap_or("repository")),
        "git_show" => format!("Git show · {}", args["rev"].as_str().unwrap_or("HEAD")),
        "git_blame" => format!("Git blame · {}", path.unwrap_or("file")),
        "diagnostics" => format!("Fast diagnostics · {}", path.unwrap_or("project")),
        "exec_command" => args["command"].as_str().unwrap_or("command").to_owned(),
        "web_fetch" => args["url"].as_str().unwrap_or("page").to_owned(),
        "web_search" => args["query"].as_str().unwrap_or("search").to_owned(),
        _ => path.unwrap_or(tool).to_owned(),
    };
    format!("{tool} · {}", short(&target, 140))
}

pub(super) fn result_summary(tool: &str, text: &str, error: bool, cached: bool) -> String {
    if cached {
        return "Reused previous evidence".into();
    }
    if !error {
        if tool == "finish" {
            return "Result: delivered".into();
        }
        let header = text.lines().next().unwrap_or("");
        if tool == "map" && header.starts_with("map of ") {
            let mut parts = header.split(" · ").skip(1);
            let files = parts
                .next()
                .and_then(|part| part.split_once(" files"))
                .and_then(|(count, _)| count.parse::<u64>().ok());
            let symbols = parts
                .next()
                .and_then(|part| part.strip_suffix(" symbols"))
                .and_then(|count| count.parse::<u64>().ok());
            if let (Some(files), Some(symbols)) = (files, symbols) {
                return format!("{} files · {} symbols", number(files), number(symbols));
            }
        }
        if tool == "read"
            && let Some((_, range)) = header.rsplit_once(" lines ")
            && let Some((range, _)) = range.split_once(" of ")
            && let Some((start, end)) = range.split_once('-')
            && let (Ok(start), Ok(end)) = (start.parse::<u64>(), end.parse::<u64>())
            && let Some(count) = end
                .checked_sub(start)
                .and_then(|count| count.checked_add(1))
        {
            return format!("Result: read {} lines", number(count));
        }
        if tool == "edit" && header.starts_with("Edited ") {
            return "Result: edit complete".into();
        }
        if tool == "write" && header.starts_with("Created ") {
            return "Result: file created".into();
        }
        if tool == "find" && header.starts_with("no matches for ") {
            return "Result: no matching code".into();
        }
    }
    if tool == "check"
        && let Ok(value) = serde_json::from_str::<Value>(text)
    {
        if value["status"] == "deferred_to_core" {
            return "Validation deferred to Core after merge · not yet validated".into();
        }
        if value["unchanged"] == true {
            return "No changes · previous validation retained".into();
        }
        if value["status"] == "timed_out"
            || value["commands"].as_array().is_some_and(|commands| {
                commands
                    .iter()
                    .any(|command| command["status"] == "TimedOut")
            })
        {
            return "Validation timed out".into();
        }
        if value["commands"].as_array().is_some_and(|commands| {
            commands
                .iter()
                .any(|command| command["status"] == "Cancelled")
        }) {
            return "Validation cancelled".into();
        }
        return if error {
            "Build/tests failed".into()
        } else {
            "Build/tests passed".into()
        };
    }
    if matches!(tool, "exec_command" | "write_stdin")
        && let Ok(value) = serde_json::from_str::<Value>(text)
    {
        let output = short(
            value["output"]
                .as_str()
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or(""),
            100,
        );
        let state = if let Some(exit) = value["exit_code"].as_i64() {
            format!(
                "{} · exit code {exit}",
                if error {
                    "Command failed"
                } else {
                    "Command completed"
                }
            )
        } else if let Some(session) = value["session_id"].as_u64() {
            format!("Running · session {session}")
        } else {
            "Command result".into()
        };
        return if output.is_empty() {
            state
        } else {
            format!("{state} · {output}")
        };
    }
    let detail = short(text.lines().next().unwrap_or("no output"), 140);
    format!("{}: {detail}", if error { "Failed" } else { "Result" })
}

fn number(count: u64) -> String {
    let digits = count.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn map_progress_is_a_short_description_instead_of_its_internal_header() {
        let raw = "map of the repository · 9919 files (javascript 6, python 208, rust 5232, typescript 761) · 130217 symbols · symbols ranked by use from other files (×n)\nrest of map";
        assert_eq!(
            describe("map", &json!({"path":"."})),
            "Repository structure"
        );
        assert_eq!(
            result_summary("map", raw, false, false),
            "9,919 files · 130,217 symbols"
        );
        assert_eq!(
            describe("map", &json!({"path":"src"})),
            "Repository structure · src"
        );
        assert_eq!(
            result_summary("map", "map unavailable", true, false),
            "Failed: map unavailable"
        );
        assert_eq!(
            result_summary("map", raw, false, true),
            "Reused previous evidence"
        );
    }

    #[test]
    fn read_and_edit_progress_describe_the_result_without_copying_raw_headers() {
        assert_eq!(
            result_summary(
                "read",
                "src/main.rs lines 2-12 of 100\n    2| fn main() {}",
                false,
                false
            ),
            "Result: read 11 lines"
        );
        assert_eq!(
            result_summary(
                "edit",
                "Edited src/main.rs\n{\"matched\":\"exactly\"}",
                false,
                false
            ),
            "Result: edit complete"
        );
        assert_eq!(
            result_summary("find", "no matches for `average`", false, false),
            "Result: no matching code"
        );
    }
}
