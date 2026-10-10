//! User-owned direct provider connections. Configuration never contains API keys.
use super::{
    Endpoint, Protocol, connect,
    router::{Candidate, CostClass},
};
use anyhow::{Context, Result, ensure};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
};

mod catalog;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Account {
    pub id: String,
    pub provider: String,
    pub base_url: String,
    pub protocol: Protocol,
    #[serde(default)]
    pub key_env: Option<String>,
    #[serde(default)]
    pub models: Vec<String>,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default = "yes")]
    pub automatic: bool,
    #[serde(default = "paid")]
    pub cost: CostClass,
}
fn yes() -> bool {
    true
}
fn paid() -> CostClass {
    CostClass::Paid
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub accounts: Vec<Account>,
    #[serde(default = "legacy_ceiling")]
    pub max_cost: CostClass,
    #[serde(default)]
    pub inference_backend: Option<String>,
}
fn legacy_ceiling() -> CostClass {
    super::COST_CEILING
}
impl Default for Config {
    fn default() -> Self {
        Self {
            accounts: vec![],
            max_cost: legacy_ceiling(),
            inference_backend: None,
        }
    }
}
#[derive(Clone, Debug)]
pub struct Summary {
    pub account: Account,
    pub authenticated: bool,
}
pub struct Registry {
    root: PathBuf,
}
pub const TEMPLATES: &[(&str, &str, Protocol)] = &[
    ("openai", "https://api.openai.com/v1", Protocol::Responses),
    (
        "anthropic",
        "https://api.anthropic.com/v1",
        Protocol::Anthropic,
    ),
    (
        "gemini",
        "https://generativelanguage.googleapis.com/v1beta/openai",
        Protocol::Chat,
    ),
    ("groq", "https://api.groq.com/openai/v1", Protocol::Chat),
    ("openrouter", "https://openrouter.ai/api/v1", Protocol::Chat),
    ("deepseek", "https://api.deepseek.com/v1", Protocol::Chat),
    ("compatible", "http://127.0.0.1:8080/v1", Protocol::Chat),
];

impl Account {
    pub fn template(provider: &str, id: &str) -> Result<Self> {
        if provider == "chatgpt" {
            return Ok(Self {
                id: id.into(),
                provider: provider.into(),
                base_url: "https://api.openai.com/v1".into(),
                protocol: Protocol::Responses,
                key_env: None,
                models: vec![],
                enabled: true,
                automatic: true,
                cost: CostClass::Subscription,
            });
        }
        let (_, url, protocol) = TEMPLATES.iter().find(|(name, _, _)| *name == provider)
            .context("unknown provider; use openai, anthropic, gemini, groq, openrouter, deepseek or compatible")?;
        Ok(Self {
            id: id.into(),
            provider: provider.into(),
            base_url: (*url).into(),
            protocol: *protocol,
            key_env: None,
            models: vec![],
            enabled: true,
            automatic: true,
            cost: paid(),
        })
    }
    pub fn validate(&self) -> Result<()> {
        if self.provider == "chatgpt" {
            ensure!(
                self.base_url == "https://api.openai.com/v1"
                    && self.protocol == Protocol::Responses
                    && self.key_env.is_none()
                    && self.cost == CostClass::Subscription,
                "ChatGPT connections use the official OAuth API and subscription cost class"
            );
        }
        ensure!(
            !self.id.is_empty()
                && self.id.len() <= 64
                && self
                    .id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
            "connection name must contain 1–64 letters, numbers, - or _"
        );
        ensure!(
            ![
                "auto",
                "loom",
                "ollama",
                "lmstudio",
                "llama-server",
                "mlx",
                "opencodex"
            ]
            .contains(&self.id.as_str()),
            "connection name is reserved; choose another name"
        );
        ensure!(
            (self.base_url.starts_with("https://") || self.base_url.starts_with("http://"))
                && !self.base_url.chars().any(char::is_whitespace)
                && !self.base_url.contains('@')
                && !self.base_url.contains('?')
                && !self.base_url.contains('#'),
            "invalid provider base URL"
        );
        let parsed = url::Url::parse(&self.base_url).context("invalid provider base URL")?;
        let host = parsed
            .host_str()
            .context("provider base URL needs a host")?;
        if parsed.scheme() == "http" {
            ensure!(
                ["127.0.0.1", "localhost", "[::1]"].contains(&host),
                "remote provider URLs must use HTTPS"
            );
        }
        ensure!(
            self.models
                .iter()
                .all(|m| !m.trim().is_empty() && !m.chars().any(char::is_control)),
            "invalid model id"
        );
        if let Some(env) = &self.key_env {
            ensure!(
                !env.is_empty() && env.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'),
                "invalid key environment name"
            );
        }
        Ok(())
    }
}

impl Registry {
    pub fn open() -> Result<Self> {
        Ok(Self::at(
            super::state_dir().context("model state directory unavailable")?,
        ))
    }
    pub fn at(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn config(&self) -> Result<Config> {
        let path = self.root.join("providers.toml");
        if !path.exists() {
            return Ok(Config::default());
        }
        check_file(&path)?;
        let config: Config =
            toml::from_str(&fs::read_to_string(path)?).context("invalid providers.toml")?;
        let mut seen = std::collections::BTreeSet::new();
        for account in &config.accounts {
            account.validate()?;
            ensure!(seen.insert(&account.id), "duplicate connection name");
        }
        Ok(config)
    }
    fn secrets(&self) -> Result<BTreeMap<String, String>> {
        let path = self.root.join("provider-credentials.json");
        if !path.exists() {
            return Ok(BTreeMap::new());
        }
        check_file(&path)?;
        serde_json::from_slice(&fs::read(path)?).context("invalid provider credential store")
    }
    pub fn summaries(&self) -> Result<Vec<Summary>> {
        let config = self.config()?;
        let secrets = self.secrets()?;
        Ok(config
            .accounts
            .into_iter()
            .map(|account| {
                let authenticated = account
                    .key_env
                    .as_ref()
                    .is_some_and(|env| std::env::var(env).ok().is_some_and(|s| !s.is_empty()))
                    || secrets.contains_key(&account.id)
                    || (account.provider == "chatgpt"
                        && super::chatgpt::is_connected(&self.root, &account.id));
                Summary {
                    account,
                    authenticated,
                }
            })
            .collect())
    }
    pub fn put(&self, account: Account, key: Option<&str>) -> Result<()> {
        account.validate()?;
        ensure!(
            account.provider != "chatgpt" || key.is_none(),
            "ChatGPT uses browser login, not an API key"
        );
        ensure!(
            key.is_none() || account.key_env.is_none(),
            "choose a stored key or an environment variable, not both"
        );
        if let Some(key) = key {
            ensure!(
                !key.trim().is_empty() && !key.chars().any(char::is_whitespace),
                "API key must be nonempty and contain no whitespace"
            );
        }
        self.mutate(|config, secrets| {
            if let Some(key) = key {
                secrets.insert(account.id.clone(), key.into());
            } else {
                secrets.remove(&account.id);
            }
            if account.key_env.is_some() {
                secrets.remove(&account.id);
            }
            config.max_cost = config.max_cost.max(account.cost);
            config.accounts.retain(|a| a.id != account.id);
            config.accounts.push(account);
            Ok(())
        })
    }
    /// Reauthentication owns credentials only; it must not reset routing settings.
    pub fn prepare_chatgpt_login(&self, id: &str) -> Result<bool> {
        Account::template("chatgpt", id)?.validate()?;
        match self
            .config()?
            .accounts
            .into_iter()
            .find(|account| account.id == id)
        {
            Some(account) => {
                ensure!(
                    account.provider == "chatgpt",
                    "connection name belongs to an API account; choose another name"
                );
                Ok(false)
            }
            None => Ok(true),
        }
    }
    pub fn finish_chatgpt_login(&self, id: &str, new_connection: bool) -> Result<()> {
        let account = Account::template("chatgpt", id)?;
        account.validate()?;
        self.mutate(|config, _| {
            if let Some(existing) = config.accounts.iter().find(|existing| existing.id == id) {
                ensure!(
                    existing.provider == "chatgpt",
                    "connection name now belongs to an API account; choose another name"
                );
            } else {
                ensure!(
                    new_connection,
                    "connection was removed while signing in; add a new connection explicitly"
                );
                config.accounts.push(account);
            }
            config.max_cost = config.max_cost.max(CostClass::Subscription);
            Ok(())
        })
    }
    pub fn remove(&self, id: &str) -> Result<()> {
        let chatgpt = self
            .config()?
            .accounts
            .iter()
            .any(|account| account.id == id && account.provider == "chatgpt");
        self.mutate(|config, secrets| {
            ensure!(
                config.accounts.iter().any(|a| a.id == id),
                "connection not found"
            );
            config.accounts.retain(|a| a.id != id);
            secrets.remove(id);
            Ok(())
        })?;
        if chatgpt {
            super::chatgpt::logout(&self.root, id)?;
        }
        Ok(())
    }
    pub fn enable(&self, id: &str, enabled: bool) -> Result<()> {
        self.mutate(|config, _| {
            config
                .accounts
                .iter_mut()
                .find(|a| a.id == id)
                .context("connection not found")?
                .enabled = enabled;
            Ok(())
        })
    }
    pub fn set_max_cost(&self, cost: CostClass) -> Result<()> {
        self.mutate(|config, _| {
            config.max_cost = cost;
            Ok(())
        })
    }
    pub fn set_automatic(&self, id: &str, automatic: bool) -> Result<()> {
        self.mutate(|config, _| {
            config.accounts.iter_mut().find(|a| a.id == id).context("connection not found")?.automatic = automatic;
            Ok(())
        })
    }
    pub fn set_inference_backend(&self, value: &str) -> Result<()> {
        let backend = super::native_runtime::Backend::parse(value).context("use auto, cpu, mps or cuda")?;
        ensure!(backend != super::native_runtime::Backend::Mps || cfg!(target_os = "macos"), "Metal requires macOS");
        ensure!(backend != super::native_runtime::Backend::Cuda || cfg!(feature = "cuda"), "CUDA support is not enabled in this build");
        self.mutate(|config, _| { config.inference_backend = Some(backend.label().into()); Ok(()) })
    }
    fn mutate(
        &self,
        change: impl FnOnce(&mut Config, &mut BTreeMap<String, String>) -> Result<()>,
    ) -> Result<()> {
        fs::create_dir_all(&self.root)?;
        let lock_path = self.root.join("providers.lock");
        if lock_path.exists() {
            check_file(&lock_path)?;
        }
        let lock = private_open(&lock_path)?;
        lock.lock_exclusive()?;
        let mut config = self.config()?;
        let mut secrets = self.secrets()?;
        change(&mut config, &mut secrets)?;
        write_private(
            &self.root.join("provider-credentials.json"),
            &serde_json::to_vec(&secrets)?,
        )?;
        write_private(
            &self.root.join("providers.toml"),
            toml::to_string_pretty(&config)?.as_bytes(),
        )?;
        Ok(())
    }
    pub fn endpoint(&self, account: &Account) -> Result<Endpoint> {
        account.validate()?;
        let key = match &account.key_env {
            Some(env) => Some(
                std::env::var(env)
                    .ok()
                    .filter(|key| !key.is_empty())
                    .context("API key environment variable is not set")?,
            ),
            None => self.secrets()?.get(&account.id).cloned(),
        };
        Ok(Endpoint {
            name: account.id.clone(),
            protocol: account.protocol,
            base_url: account.base_url.clone(),
            api_key: key,
            headers: vec![],
        })
    }
    pub fn candidates(
        &self,
        account: &Account,
    ) -> Result<(Box<dyn super::Provider>, Vec<Candidate>)> {
        if account.provider == "openrouter" {
            // The catalog and inference provider must use the same captured account key.
            let endpoint = self.endpoint(account)?;
            let models = catalog::openrouter(account, &endpoint)?;
            let key = endpoint.api_key.clone();
            let provider = Box::new(RegisteredProvider {
                inner: connect(endpoint),
                key,
            });
            return Ok((provider, models));
        }
        let provider = self.provider(account)?;
        let names = if account.models.is_empty() {
            provider.list_models().map_err(|error| {
                anyhow::anyhow!("provider model discovery failed: {:?}", error.kind)
            })?
        } else {
            account.models.clone()
        };
        let models = names
            .into_iter()
            .filter(|m| super::is_chat_model(m))
            .map(|model| Candidate {
                endpoint: account.id.clone(),
                tier: super::discover::guess_tier(&model),
                model,
                cost: account.cost,
                price: if account.cost <= CostClass::Subscription {
                    Some((0.0, 0.0))
                } else {
                    None
                },
                tools: true,
                context: None,
            })
            .collect();
        Ok((provider, models))
    }
    fn provider(&self, account: &Account) -> Result<Box<dyn super::Provider>> {
        if account.provider == "chatgpt" {
            return Ok(super::chatgpt::provider(
                self.root.clone(),
                account.id.clone(),
            ));
        }
        let endpoint = self.endpoint(account)?;
        let key = endpoint.api_key.clone();
        Ok(Box::new(RegisteredProvider {
            inner: connect(endpoint),
            key,
        }))
    }
    pub fn probe(&self, id: &str) -> Result<Vec<String>> {
        let account = self
            .config()?
            .accounts
            .into_iter()
            .find(|a| a.id == id)
            .context("connection not found")?;
        if account.provider == "openrouter" {
            return Ok(self
                .candidates(&account)?
                .1
                .into_iter()
                .map(|model| model.model)
                .collect());
        }
        self.provider(&account)?
            .list_models()
            .map_err(|error| anyhow::anyhow!("connection check failed: {:?}", error.kind))
    }
}
// Servers sometimes echo authentication material in errors. Such messages must
// never become a conversation entry, retry notice, or usage ledger record.
struct RegisteredProvider {
    inner: Box<dyn super::Provider>,
    key: Option<String>,
}
impl RegisteredProvider {
    fn redact(&self, mut error: super::ProviderError) -> super::ProviderError {
        if let Some(key) = self.key.as_deref().filter(|key| !key.is_empty()) {
            error.message = error.message.replace(key, "[redacted]");
            if let Some(code) = &mut error.code {
                *code = code.replace(key, "[redacted]");
            }
        }
        error
    }
}
impl super::Provider for RegisteredProvider {
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn complete(
        &self,
        request: &super::Request,
    ) -> std::result::Result<super::Response, super::ProviderError> {
        self.inner
            .complete(request)
            .map_err(|error| self.redact(error))
    }
    fn complete_stream(
        &self,
        request: &super::Request,
        observe: &mut dyn FnMut(super::StreamEvent),
    ) -> std::result::Result<super::Response, super::ProviderError> {
        self.inner
            .complete_stream(request, observe)
            .map_err(|error| self.redact(error))
    }
    fn list_models(&self) -> std::result::Result<Vec<String>, super::ProviderError> {
        self.inner.list_models().map_err(|error| self.redact(error))
    }
    fn reports_route(&self) -> bool {
        self.inner.reports_route()
    }
    fn reasoning_profile(&self, model: &str) -> super::ReasoningProfile {
        self.inner.reasoning_profile(model)
    }
}

fn private_open(path: &Path) -> Result<std::fs::File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    Ok(options.open(path)?)
}
fn check_file(path: &Path) -> Result<()> {
    let meta = fs::symlink_metadata(path)?;
    ensure!(
        meta.is_file() && !meta.file_type().is_symlink(),
        "provider state must be a regular file"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        ensure!(
            meta.permissions().mode() & 0o077 == 0,
            "provider state permissions must be 600"
        );
    }
    Ok(())
}
fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    if path.exists() {
        check_file(path)?;
    }
    let mut file = tempfile::NamedTempFile::new_in(
        path.parent()
            .context("provider state needs a parent directory")?,
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
    }
    use std::io::Write;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn openrouter_account_catalog_uses_registered_base_and_bearer_with_real_costs() -> Result<()> {
        let root = tempfile::tempdir()?;
        let registry = Registry::at(root.path());
        let (base, rx) = super::super::mock::serve(200, &json!({"data":[
            {"id":"vendor/paid","pricing":{"prompt":"0.000002","completion":"0.000004"},"supported_parameters":["tools"],"context_length":64000},
            {"id":"vendor/free","pricing":{"prompt":"0","completion":"0","request":"0","image":"0","web_search":"0"},"supported_parameters":["tools"]},
            {"id":"vendor/unknown","supported_parameters":["tools"]},
            {"id":"vendor/chat","pricing":{"prompt":"0","completion":"0"},"supported_parameters":["temperature"]}
        ]}).to_string());
        let mut account = Account::template("openrouter", "personal")?;
        account.base_url = format!("{base}/regional/api/v1/");
        // A configured free ceiling must not relabel the model's actual billing.
        account.cost = CostClass::Free;
        registry.put(account.clone(), Some("catalog-key-fixture"))?;
        let (provider, candidates) = registry.candidates(&account)?;
        assert_eq!(provider.name(), "personal");
        assert_eq!(candidates.len(), 4);
        assert_eq!(candidates[0].cost, CostClass::Paid);
        assert_eq!(candidates[0].price, Some((2.0, 4.0)));
        assert_eq!(candidates[0].context, Some(64000));
        assert_eq!(candidates[1].cost, CostClass::Free);
        assert_eq!(candidates[2].cost, CostClass::Unknown);
        assert_eq!(candidates[2].price, None);
        assert!(!candidates[3].tools);
        let request = rx.recv_timeout(std::time::Duration::from_secs(5))?;
        assert_eq!(request.path, "/regional/api/v1/models/user");
        assert!(
            request
                .headers
                .iter()
                .any(|(name, value)| name == "authorization"
                    && value == "Bearer catalog-key-fixture")
        );
        Ok(())
    }

    #[test]
    fn openrouter_probe_intersects_explicit_ids_with_account_metadata() -> Result<()> {
        let root = tempfile::tempdir()?;
        let registry = Registry::at(root.path());
        let (base, rx) = super::super::mock::serve(200, &json!({"data":[
            {"id":"vendor/free","pricing":{"prompt":"0","completion":"0"},"supported_parameters":["tools"]},
            {"id":"vendor/chat","pricing":{"prompt":"0.000002","completion":"0.000004"},"supported_parameters":[]},
            {"id":"vendor/other","pricing":{"prompt":"0","completion":"0"}}
        ]}).to_string());
        let mut account = Account::template("openrouter", "work")?;
        account.base_url = format!("{base}/api/v1");
        account.models = vec![
            "excluded-by-account".into(),
            "vendor/chat".into(),
            "vendor/free".into(),
        ];
        registry.put(account, Some("work-key-fixture"))?;
        assert_eq!(registry.probe("work")?, vec!["vendor/chat", "vendor/free"]);
        let request = rx.recv_timeout(std::time::Duration::from_secs(5))?;
        assert_eq!(request.path, "/api/v1/models/user");
        assert!(
            request
                .headers
                .iter()
                .any(|(name, value)| name == "authorization" && value == "Bearer work-key-fixture")
        );
        Ok(())
    }

    #[test]
    fn openrouter_catalog_fails_closed_without_key_or_on_admission_failure() -> Result<()> {
        let root = tempfile::tempdir()?;
        let registry = Registry::at(root.path());
        let mut account = Account::template("openrouter", "work")?;
        // No key means no HTTP request, even when explicit IDs were configured.
        account.base_url = "http://127.0.0.1:1/api/v1".into();
        account.models = vec!["would-bypass-guardrails".into()];
        let error = registry.candidates(&account).err().unwrap();
        assert!(error.to_string().contains("requires an API key"));
        let (base, rx) =
            super::super::mock::serve(403, r#"{"error":{"message":"denied catalog-key-fixture"}}"#);
        account.base_url = format!("{base}/api/v1");
        registry.put(account.clone(), Some("catalog-key-fixture"))?;
        let error = registry.candidates(&account).err().unwrap();
        assert!(error.to_string().contains("account catalog failed"));
        assert!(!format!("{error:?}").contains("catalog-key-fixture"));
        assert_eq!(
            rx.recv_timeout(std::time::Duration::from_secs(5))?.path,
            "/api/v1/models/user"
        );
        Ok(())
    }

    #[test]
    fn automatic_free_tool_need_routes_only_to_catalog_verified_free_tool_model() -> Result<()> {
        use super::super::{
            ErrorKind, Need, Part, Provider, ProviderError, Request, Response, Stop, Usage,
            router::Router,
        };
        struct OnlyFree;
        impl Provider for OnlyFree {
            fn name(&self) -> &str {
                "personal"
            }
            fn list_models(&self) -> std::result::Result<Vec<String>, ProviderError> {
                unreachable!()
            }
            fn complete(&self, request: &Request) -> std::result::Result<Response, ProviderError> {
                assert_eq!(
                    request.model, "vendor/free-tools",
                    "never invoke a paid, unknown or non-tool model"
                );
                Ok(Response {
                    parts: vec![Part::Text("mock reply".into())],
                    usage: Usage::default(),
                    stop: Stop::EndTurn,
                    model: request.model.clone(),
                    latency: std::time::Duration::ZERO,
                    route: None,
                    gateway_connection: None,
                    gateway_upstream_ms: None,
                })
            }
        }
        let root = tempfile::tempdir()?;
        let registry = Registry::at(root.path());
        let (base, rx) = super::super::mock::serve(200, &json!({"data":[
            {"id":"vendor/free-chat","pricing":{"prompt":"0","completion":"0"},"supported_parameters":[]},
            {"id":"vendor/paid-tools","pricing":{"prompt":"0.000001","completion":"0.000001"},"supported_parameters":["tools"]},
            {"id":"vendor/extra-charge","pricing":{"prompt":"0","completion":"0","web_search":"0.005"},"supported_parameters":["tools"]},
            {"id":"vendor/unknown-tools","supported_parameters":["tools"]},
            {"id":"vendor/free-tools","pricing":{"prompt":"0","completion":"0","request":"0"},"supported_parameters":["tools"]}
        ]}).to_string());
        let mut account = Account::template("openrouter", "personal")?;
        account.base_url = format!("{base}/api/v1");
        assert_eq!(account.cost, CostClass::Paid);
        registry.put(account.clone(), Some("mock-catalog-key"))?;
        let (_, candidates) = registry.candidates(&account)?;
        let _ = rx.recv_timeout(std::time::Duration::from_secs(5))?;
        let mut router = Router::new(None);
        router.add(Box::new(OnlyFree), candidates.clone());
        let need = Need {
            max_cost: CostClass::Free,
            tools: true,
            ..Need::default()
        };
        let served = router.complete(need, &Request::default())?;
        assert_eq!(served.candidate.model, "vendor/free-tools");
        assert_eq!(served.candidate.cost, CostClass::Free);
        let mut unavailable = Router::new(None);
        unavailable.add(
            Box::new(OnlyFree),
            candidates
                .into_iter()
                .filter(|c| c.model != "vendor/free-tools")
                .collect(),
        );
        // No candidate meets the free/tool constraint; no provider reported quota.
        let error = unavailable.complete(need, &Request::default()).unwrap_err();
        assert_eq!(error.kind, ErrorKind::BadRequest);
        assert!(error.message.contains("no tool support"), "{error}");
        Ok(())
    }

    #[test]
    fn chatgpt_reauthentication_preserves_settings_and_rejects_api_collisions() -> Result<()> {
        let root = tempfile::tempdir()?;
        let registry = Registry::at(root.path());
        let new = registry.prepare_chatgpt_login("personal")?;
        registry.finish_chatgpt_login("personal", new)?;
        let mut account = registry.config()?.accounts.remove(0);
        account.enabled = false;
        account.automatic = false;
        account.models = vec!["explicit-model".into()];
        registry.put(account, None)?;
        let new = registry.prepare_chatgpt_login("personal")?;
        assert!(!new);
        registry.finish_chatgpt_login("personal", new)?;
        let account = registry.config()?.accounts.remove(0);
        assert!(!account.enabled && !account.automatic);
        assert_eq!(account.models, ["explicit-model"]);
        registry.remove("personal")?;
        assert!(registry.finish_chatgpt_login("personal", false).is_err());
        registry.put(Account::template("openai", "work")?, Some("keep-api-key"))?;
        assert!(registry.prepare_chatgpt_login("work").is_err());
        assert!(registry.finish_chatgpt_login("work", true).is_err());
        assert_eq!(registry.secrets()?["work"], "keep-api-key");
        Ok(())
    }

    #[test]
    fn replacing_a_connection_without_auth_clears_the_old_key() -> Result<()> {
        let root = tempfile::tempdir()?;
        let registry = Registry::at(root.path());
        let mut account = Account::template("compatible", "work")?;
        registry.put(account.clone(), Some("old-server-key"))?;
        account.base_url = "http://localhost:8081/v1".into();
        registry.put(account.clone(), None)?;
        assert!(registry.endpoint(&account)?.api_key.is_none());
        assert!(!registry.summaries()?[0].authenticated);
        Ok(())
    }

    #[test]
    fn provider_errors_and_endpoint_debug_never_reveal_keys() {
        struct Bad;
        impl super::super::Provider for Bad {
            fn name(&self) -> &str {
                "fixture"
            }
            fn complete(
                &self,
                _: &super::super::Request,
            ) -> std::result::Result<super::super::Response, super::super::ProviderError>
            {
                let mut error = super::super::ProviderError::new(
                    super::super::ErrorKind::Auth,
                    "bad key secret-fixture",
                );
                error.code = Some("secret-fixture".into());
                Err(error)
            }
            fn list_models(&self) -> std::result::Result<Vec<String>, super::super::ProviderError> {
                Ok(vec![])
            }
        }
        use super::super::Provider;
        let registered = RegisteredProvider {
            inner: Box::new(Bad),
            key: Some("secret-fixture".into()),
        };
        let error = registered.complete(&Default::default()).unwrap_err();
        assert!(!format!("{error:?}").contains("secret-fixture"));
        let endpoint = Endpoint {
            name: "fixture".into(),
            protocol: Protocol::Chat,
            base_url: "http://localhost/v1".into(),
            api_key: Some("secret-fixture".into()),
            headers: vec![("Authorization".into(), "secret-header".into())],
        };
        let debug = format!("{endpoint:?}");
        assert!(!debug.contains("secret-fixture") && !debug.contains("secret-header"));
    }

    #[test]
    fn accounts_keep_keys_out_of_config_and_remove_credentials() -> Result<()> {
        let root = tempfile::tempdir()?;
        let registry = Registry::at(root.path());
        for id in ["work", "personal"] {
            registry.put(Account::template("openai", id)?, Some("secret-fixture"))?;
        }
        assert!(
            !fs::read_to_string(root.path().join("providers.toml"))?.contains("secret-fixture")
        );
        assert_eq!(registry.summaries()?.len(), 2);
        assert!(registry.summaries()?.iter().all(|s| s.authenticated));
        registry.enable("work", false)?;
        assert!(!registry.config()?.accounts[0].enabled);
        registry.remove("work")?;
        assert!(!registry.secrets()?.contains_key("work"));
        assert_eq!(registry.config()?.max_cost, CostClass::Paid);
        Ok(())
    }
    #[test]
    fn key_storage_refuses_symlinks_and_public_files() -> Result<()> {
        let root = tempfile::tempdir()?;
        let registry = Registry::at(root.path());
        #[cfg(unix)]
        {
            use std::os::unix::fs::{PermissionsExt, symlink};
            let outside = root.path().join("outside");
            fs::write(&outside, "keep")?;
            symlink(&outside, root.path().join("provider-credentials.json"))?;
            assert!(
                registry
                    .put(Account::template("openai", "test")?, Some("secret"))
                    .is_err()
            );
            assert_eq!(fs::read_to_string(&outside)?, "keep");
            fs::remove_file(root.path().join("provider-credentials.json"))?;
            registry.put(Account::template("openai", "test")?, Some("secret"))?;
            let path = root.path().join("provider-credentials.json");
            assert_eq!(fs::metadata(&path)?.permissions().mode() & 0o777, 0o600);
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644))?;
            assert!(registry.summaries().is_err());
        }
        Ok(())
    }
    #[test]
    fn custom_connections_validate_names_urls_and_preserve_explicit_models() -> Result<()> {
        let root = tempfile::tempdir()?;
        let registry = Registry::at(root.path());
        let mut account = Account::template("compatible", "custom")?;
        account.models = vec!["fixture".into()];
        account.cost = CostClass::Local;
        registry.put(account.clone(), None)?;
        let (_, models) = registry.candidates(&account)?;
        assert_eq!(models[0].endpoint, "custom");
        account.base_url = "http://untrusted.example/v1".into();
        assert!(account.validate().is_err());
        account.base_url = "https://secret@provider.example/v1".into();
        assert!(account.validate().is_err());
        account.base_url = "http://[2001:db8::1]/v1".into();
        assert!(account.validate().is_err());
        account.base_url = "http://[::1]:8080/v1".into();
        assert!(account.validate().is_ok());
        account.base_url = "https://:443/v1".into();
        assert!(account.validate().is_err());
        Ok(())
    }
}
