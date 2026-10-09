//! Find usable model endpoints: local runtimes, gateways, and API keys in the environment.
//!
//! Keys come from environment variables or the private connection registry. A source that is
//! not reachable or not configured is reported with the reason, not silently dropped.
use super::router::{Candidate, CostClass, Router, Tier};
use super::*;
use serde_json::Value;
use std::path::Path;

#[derive(Clone, Debug)]
pub struct Source {
    pub name: String,
    pub status: Result<usize, String>,
}

/// Size-based tier guess from a model id; config can override later.
pub fn guess_tier(model: &str) -> Tier {
    let m = model.to_ascii_lowercase();
    let billions = m
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '.')
        .filter_map(|t| {
            t.strip_suffix('b')
                .and_then(|n| n.trim_start_matches(['e', 'a']).parse::<f64>().ok())
        })
        .fold(None, |acc: Option<f64>, n| {
            Some(acc.map_or(n, |a| a.max(n)))
        });
    if let Some(b) = billions {
        return match b {
            b if b < 10.0 => Tier::Small,
            b if b < 100.0 => Tier::Medium,
            _ => Tier::Flagship,
        };
    }
    let has = |keys: &[&str]| keys.iter().any(|k| m.contains(k));
    if has(&[
        "opus",
        "gpt-5-pro",
        "o3-pro",
        "ultra",
        "-pro",
        "gpt-6",
        "grok-4",
    ]) {
        Tier::Flagship
    } else if has(&[
        "mini",
        "nano",
        "lite",
        "haiku",
        "flash-lite",
        "small",
        "tiny",
        "instant",
    ]) {
        Tier::Small
    } else {
        Tier::Medium
    }
}

fn reachable_models(base: &str, headers: &[(String, String)]) -> Result<Vec<String>, String> {
    let value = get_json(&url(base, "models"), headers).map_err(|e| e.to_string())?;
    Ok(value["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| m["id"].as_str().map(str::to_owned))
        .filter(|id| !id.contains("embed"))
        .collect())
}

/// Loom runs Ollama's downloaded weights itself as `loom/ollama/<name>`; listing the
/// same weights again through the Ollama server only showed one model twice.
fn served_natively(endpoint: &str, model: &str, native: &[String]) -> bool {
    endpoint == "ollama" && native.iter().any(|id| id.strip_prefix("ollama/") == Some(model))
}

fn local(router: &mut Router, sources: &mut Vec<Source>, name: &str, base: &str, native: &[String]) {
    let protocol = if name == "ollama" {
        Protocol::Ollama
    } else {
        Protocol::Chat
    };
    match reachable_models(base, &[]) {
        Ok(models) => {
            let candidates = models
                .iter()
                .filter(|m| !served_natively(name, m, native))
                .map(|m| Candidate {
                    endpoint: name.into(),
                    model: m.clone(),
                    tier: Tier::Local.max(guess_tier(m).min(Tier::Small)),
                    cost: CostClass::Local,
                    price: Some((0.0, 0.0)),
                    tools: true,
                    context: None,
                })
                .collect::<Vec<_>>();
            sources.push(Source {
                name: name.into(),
                status: Ok(candidates.len()),
            });
            router.add(
                connect(Endpoint {
                    name: name.into(),
                    protocol,
                    base_url: base.into(),
                    api_key: None,
                    headers: vec![],
                }),
                candidates,
            );
        }
        Err(e) => sources.push(Source {
            name: name.into(),
            status: Err(format!("not running ({e})")),
        }),
    }
}

/// MLX checkpoints in `~/.loom/models/mlx`, run by Loom's MLX engine.
#[cfg(loom_mlx)]
fn mlx(router: &mut Router, sources: &mut Vec<Source>) {
    let provider = super::mlx_runtime::MlxProvider::discover();
    let candidates: Vec<_> = provider
        .models()
        .iter()
        .map(|model| Candidate {
            endpoint: "mlx".into(),
            model: model.id.clone(),
            tier: Tier::Small,
            cost: CostClass::Local,
            price: Some((0.0, 0.0)),
            tools: true,
            context: None,
        })
        .collect();
    sources.push(Source { name: "loom-mlx".into(), status: Ok(candidates.len()) });
    router.add(Box::new(provider), candidates);
}

/// Native model ids, so other sources can leave out weights Loom already serves.
fn loom_native(router: &mut Router, sources: &mut Vec<Source>) -> Vec<String> {
    let backend = match super::native_runtime::Backend::configured() {
        Ok(backend) => backend,
        Err(error) => {
            sources.push(Source {
                name: "loom-native".into(),
                status: Err(error.to_string()),
            });
            return Vec::new();
        }
    };
    let provider = super::native_runtime::NativeProvider::discover(backend);
    // A GGUF already converted to LMX is listed once, as the LMX.
    let converted = super::lmx_runtime::converted_sources();
    let candidates = provider
        .models()
        .iter()
        .filter(|model| !converted.contains(&std::fs::canonicalize(&model.path).unwrap_or_else(|_| model.path.clone())))
        .map(|model| Candidate {
            endpoint: "loom".into(),
            model: model.id.clone(),
            tier: Tier::Small,
            cost: CostClass::Local,
            price: Some((0.0, 0.0)),
            tools: true,
            context: None,
        })
        .collect::<Vec<_>>();
    sources.push(Source {
        name: format!("loom-native ({})", backend.label()),
        status: Ok(candidates.len()),
    });
    let ids = candidates.iter().map(|candidate| candidate.model.clone()).collect();
    router.add(Box::new(provider), candidates);
    ids
}

fn opencodex(router: &mut Router, sources: &mut Vec<Source>, home: &Path) {
    let config = home.join(".opencodex/config.json");
    let Ok(text) = std::fs::read_to_string(&config) else {
        sources.push(Source {
            name: "opencodex".into(),
            status: Err("not installed".into()),
        });
        return;
    };
    let port = serde_json::from_str::<Value>(&text)
        .ok()
        .and_then(|v| v["port"].as_u64())
        .unwrap_or(10100);
    let base = format!("http://127.0.0.1:{port}/v1");
    match reachable_models(&base, &[]) {
        Ok(models) => {
            let candidates: Vec<Candidate> = models
                .iter()
                .map(|m| Candidate {
                    endpoint: "opencodex".into(),
                    model: m.clone(),
                    tier: guess_tier(m),
                    cost: CostClass::Subscription,
                    price: None,
                    tools: true,
                    context: None,
                })
                .collect();
            sources.push(Source {
                name: "opencodex".into(),
                status: Ok(candidates.len()),
            });
            router.add(
                connect(Endpoint {
                    name: "opencodex".into(),
                    protocol: Protocol::Responses,
                    base_url: base,
                    api_key: None,
                    headers: vec![],
                }),
                candidates,
            );
        }
        Err(e) => sources.push(Source {
            name: "opencodex".into(),
            status: Err(format!("installed, not running on {port} ({e})")),
        }),
    }
}

/// OpenRouter lists prices without a key; free models (`price 0`) need a key to call.
fn openrouter(router: &mut Router, sources: &mut Vec<Source>, key: Option<String>) {
    if registered_name("openrouter") { return; }
    let Some(key) = key else {
        sources.push(Source {
            name: "openrouter".into(),
            status: Err("OPENROUTER_API_KEY not set".into()),
        });
        return;
    };
    let base = "https://openrouter.ai/api/v1";
    match get_json(&url(base, "models"), &[]) {
        Ok(value) => {
            let candidates: Vec<Candidate> = value["data"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|m| {
                    let id = m["id"].as_str()?.to_owned();
                    let price =
                        |k: &str| m["pricing"][k].as_str().and_then(|p| p.parse::<f64>().ok());
                    let (input, output) = (price("prompt")?, price("completion")?);
                    let tools = m["supported_parameters"]
                        .as_array()
                        .is_some_and(|p| p.iter().any(|x| x == "tools"));
                    let free = input == 0.0 && output == 0.0;
                    Some(Candidate {
                        endpoint: "openrouter".into(),
                        tier: guess_tier(&id),
                        model: id,
                        cost: if free {
                            CostClass::Free
                        } else {
                            CostClass::Paid
                        },
                        price: Some((input * 1e6, output * 1e6)),
                        tools,
                        context: m["context_length"].as_u64(),
                    })
                })
                .collect();
            let free = candidates
                .iter()
                .filter(|c| c.cost == CostClass::Free)
                .count();
            sources.push(Source {
                name: format!("openrouter ({free} free)"),
                status: Ok(candidates.len()),
            });
            router.add(
                connect(Endpoint {
                    name: "openrouter".into(),
                    protocol: Protocol::Chat,
                    base_url: base.into(),
                    api_key: Some(key),
                    headers: vec![],
                }),
                candidates,
            );
        }
        Err(e) => sources.push(Source {
            name: "openrouter".into(),
            status: Err(e.to_string()),
        }),
    }
}

/// An API key from the environment, or from `~/.loom/keys.toml` (`NAME = "key"`).
/// The file must be private to the user (mode 600); otherwise it is ignored.
pub fn api_key(name: &str) -> Option<String> {
    if let Some(key) = std::env::var(name).ok().filter(|k| !k.is_empty()) {
        return Some(key);
    }
    let path = super::state_dir()?.join("keys.toml");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if std::fs::metadata(&path).ok()?.permissions().mode() & 0o077 != 0 {
            eprintln!(
                "loom: ignoring {} (permissions must be 600)",
                path.display()
            );
            return None;
        }
    }
    let table: toml::Table = toml::from_str(&std::fs::read_to_string(path).ok()?).ok()?;
    table
        .get(name)?
        .as_str()
        .map(str::to_owned)
        .filter(|k| !k.is_empty())
}

/// Keyed provider whose model list is fetched from its own `/models`.
fn keyed(
    router: &mut Router,
    sources: &mut Vec<Source>,
    name: &str,
    env: &str,
    protocol: Protocol,
    base: &str,
    cost: CostClass,
) {
    if registered_name(name) { return; }
    let Some(key) = api_key(env) else {
        sources.push(Source {
            name: name.into(),
            status: Err(format!("{env} not set")),
        });
        return;
    };
    let endpoint = Endpoint {
        name: name.into(),
        protocol,
        base_url: base.into(),
        api_key: Some(key),
        headers: vec![],
    };
    let provider = connect(endpoint);
    match provider.list_models() {
        Ok(models) => {
            let candidates: Vec<Candidate> = models
                .into_iter()
                .filter(|m| {
                    !m.contains("embed")
                        && !m.contains("tts")
                        && !m.contains("image")
                        && !m.contains("audio")
                })
                .map(|m| Candidate {
                    endpoint: name.into(),
                    tier: guess_tier(&m),
                    model: m,
                    cost,
                    price: None,
                    tools: true,
                    context: None,
                })
                .collect();
            sources.push(Source {
                name: name.into(),
                status: Ok(candidates.len()),
            });
            router.add(provider, candidates);
        }
        Err(e) => sources.push(Source {
            name: name.into(),
            status: Err(e.to_string()),
        }),
    }
}

/// Build a router over everything reachable now. `log` receives every call record.
pub fn discover(log: Option<std::path::PathBuf>) -> (Router, Vec<Source>) {
    let mut router = Router::new(log);
    let mut sources = Vec::new();
    match super::registry::Registry::open().and_then(|registry| registry.config()) {
        Ok(_) => {}
        Err(error) => {
            sources.push(Source { name: "connections".into(), status: Err(error.to_string()) });
            return (router, sources);
        }
    }

    let native = loom_native(&mut router, &mut sources);
    match super::lmx_runtime::LmxProvider::discover() {
        Ok(provider) => {
            let candidates = provider
                .available_models()
                .into_iter()
                .map(|model| model.display_name())
                .chain(provider.native_names())
                .map(|model| Candidate {
                    endpoint: "lmx".into(),
                    model,
                    tier: Tier::Small,
                    cost: CostClass::Local,
                    price: Some((0.0, 0.0)),
                    tools: true,
                    context: None,
                })
                .collect::<Vec<_>>();
            sources.push(Source { name: "loom-lmx".into(), status: Ok(candidates.len()) });
            router.add(Box::new(provider), candidates);
        }
        Err(error) => sources.push(Source { name: "loom-lmx".into(), status: Err(error.to_string()) }),
    }
    #[cfg(loom_mlx)]
    mlx(&mut router, &mut sources);
    local(
        &mut router,
        &mut sources,
        "ollama",
        "http://127.0.0.1:11434/v1",
        &native,
    );
    local(
        &mut router,
        &mut sources,
        "lmstudio",
        "http://127.0.0.1:1234/v1",
        &[],
    );
    local(
        &mut router,
        &mut sources,
        "llama-server",
        "http://127.0.0.1:8080/v1",
        &[],
    );
    if let Some(home) = std::env::var_os("HOME") {
        opencodex(&mut router, &mut sources, Path::new(&home));
    }
    openrouter(
        &mut router,
        &mut sources,
        api_key("OPENROUTER_API_KEY"),
    );
    // A key does not prove free billing. Unverified keyed providers stay Unknown.
    keyed(
        &mut router,
        &mut sources,
        "gemini",
        "GEMINI_API_KEY",
        Protocol::Chat,
        "https://generativelanguage.googleapis.com/v1beta/openai",
        CostClass::Unknown,
    );
    keyed(
        &mut router,
        &mut sources,
        "groq",
        "GROQ_API_KEY",
        Protocol::Chat,
        "https://api.groq.com/openai/v1",
        CostClass::Unknown,
    );
    keyed(
        &mut router,
        &mut sources,
        "openai",
        "OPENAI_API_KEY",
        Protocol::Responses,
        "https://api.openai.com/v1",
        CostClass::Unknown,
    );
    keyed(
        &mut router,
        &mut sources,
        "anthropic",
        "ANTHROPIC_API_KEY",
        Protocol::Anthropic,
        "https://api.anthropic.com/v1",
        CostClass::Unknown,
    );
    registered(&mut router, &mut sources);
    (router, sources)
}

// Named connections own their enablement, cost and credentials, including when disabled.
fn registered_name(name: &str) -> bool {
    super::registry::Registry::open().and_then(|registry| registry.config()).ok()
        .is_some_and(|config| config.accounts.iter().any(|account| account.id == name))
}

fn registered(router: &mut Router, sources: &mut Vec<Source>) {
    let registry = match super::registry::Registry::open() {
        Ok(registry) => registry,
        Err(error) => { sources.push(Source { name: "connections".into(), status: Err(error.to_string()) }); return; }
    };
    let config = match registry.config() {
        Ok(config) => config,
        Err(error) => { sources.push(Source { name: "connections".into(), status: Err(error.to_string()) }); return; }
    };
    for account in config.accounts {
        if !account.enabled { sources.push(Source { name: account.id, status: Err("disabled".into()) }); continue; }
        match registry.candidates(&account) {
            Ok((provider, candidates)) => {
                sources.push(Source { name: account.id.clone(), status: Ok(candidates.len()) });
                if account.automatic { router.add(provider, candidates); }
                else { router.add(provider, vec![]); router.add_selectable(candidates); }
            }
            Err(error) => sources.push(Source { name: account.id, status: Err(error.to_string()) }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ollama_models_loom_serves_itself_are_listed_once() {
        let native = vec!["ollama/SparkLLM/Spark-X2.5-4B:latest".to_owned(), "owner/repo/a.gguf".to_owned()];
        assert!(served_natively("ollama", "SparkLLM/Spark-X2.5-4B:latest", &native));
        assert!(!served_natively("ollama", "qwen3:8b", &native));
        assert!(!served_natively("lmstudio", "SparkLLM/Spark-X2.5-4B:latest", &native));
    }

    #[test]
    fn tiers_follow_size_then_name() {
        assert_eq!(guess_tier("qwen2.5-coder:7b"), Tier::Small);
        assert_eq!(guess_tier("Gemma-4-E4B-Q4_K_M"), Tier::Small);
        assert_eq!(guess_tier("llama-3.3-70b-versatile"), Tier::Medium);
        assert_eq!(guess_tier("deepseek/deepseek-r1:671b"), Tier::Flagship);
        assert_eq!(guess_tier("gemini-2.5-flash-lite"), Tier::Small);
        assert_eq!(guess_tier("claude-opus-4"), Tier::Flagship);
        assert_eq!(guess_tier("gpt-5.1"), Tier::Medium);
    }
}
