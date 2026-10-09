//! Shared provider management commands for the installed Loom executable.
use super::{
    CostClass, Protocol,
    registry::{Account, Registry, TEMPLATES},
};
use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::io::Read;

#[derive(Parser)]
#[command(
    name = "loom provider",
    about = "Manage direct model provider connections (no gateway required)"
)]
struct Args {
    #[command(subcommand)]
    command: Command,
}
#[derive(Subcommand)]
enum Command {
    List,
    Templates,
    /// Connect a ChatGPT account through its own browser authorization.
    Login {
        #[arg(value_parser = ["chatgpt"])]
        provider: String,
        #[arg(long, default_value = "chatgpt")]
        name: String,
    },
    Logout {
        name: String,
    },
    Add {
        provider: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        base_url: Option<String>,
        #[arg(long, value_parser = ["chat", "responses", "anthropic", "ollama"])]
        protocol: Option<String>,
        #[arg(long, conflicts_with = "key_stdin")]
        key_env: Option<String>,
        /// Read the API key from standard input; never put a key in arguments.
        #[arg(long)]
        key_stdin: bool,
        #[arg(long = "model")]
        models: Vec<String>,
        #[arg(long)]
        manual: bool,
        #[arg(long, default_value = "paid", value_parser = ["local", "free", "subscription", "paid", "unknown"])]
        cost: String,
    },
    Remove {
        name: String,
    },
    Enable {
        name: String,
    },
    Disable {
        name: String,
    },
    Test {
        name: String,
    },
    Policy {
        #[arg(value_parser = ["local", "free", "subscription", "paid", "unknown"])]
        max_cost: String,
    },
}
pub fn execute(args: impl IntoIterator<Item = String>) -> Result<()> {
    let args = match Args::try_parse_from(std::iter::once("loom provider".into()).chain(args)) {
        Ok(args) => args,
        Err(error)
            if matches!(
                error.kind(),
                clap::error::ErrorKind::DisplayHelp | clap::error::ErrorKind::DisplayVersion
            ) =>
        {
            error.print()?;
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    let registry = Registry::open()?;
    match args.command {
        Command::List => {
            let summaries = registry.summaries()?;
            if summaries.is_empty() {
                println!("No connections. Add one with `loom provider add` or /providers in Loom.");
            }
            for summary in summaries {
                let a = summary.account;
                println!(
                    "{} · {} · {} · {} · {:?} · {} · {}",
                    a.id,
                    a.provider,
                    if a.enabled { "enabled" } else { "disabled" },
                    if a.automatic { "automatic" } else { "manual" },
                    a.cost,
                    if summary.authenticated {
                        "credential configured"
                    } else {
                        "no credential"
                    },
                    a.base_url
                );
            }
        }
        Command::Login { provider, name } => {
            let _ = provider;
            let new = registry.prepare_chatgpt_login(&name)?;
            super::chatgpt::login(registry.root(), &name, &mut |url| {
                println!("Continue with ChatGPT in your browser:\n{url}");
                open_browser(url)
            })?;
            registry.finish_chatgpt_login(&name, new)?;
            println!(
                "Connected {name} · ChatGPT plan usage. Select {name}/MODEL or automatic routing."
            );
        }
        Command::Logout { name } => {
            let account = registry
                .config()?
                .accounts
                .into_iter()
                .find(|account| account.id == name)
                .context("connection not found")?;
            anyhow::ensure!(
                account.provider == "chatgpt",
                "API key connections use remove to clear credentials"
            );
            super::chatgpt::logout(registry.root(), &name)?;
            registry.enable(&name, false)?;
            println!("Logged out {name}; the saved connection is disabled.");
        }
        Command::Templates => {
            for (name, url, _) in TEMPLATES {
                println!("{name} · {url}");
            }
        }
        Command::Add {
            provider,
            name,
            base_url,
            protocol,
            key_env,
            key_stdin,
            models,
            manual,
            cost,
        } => {
            let mut account = Account::template(&provider, name.as_deref().unwrap_or(&provider))?;
            if let Some(url) = base_url {
                account.base_url = url;
            }
            if let Some(protocol) = protocol {
                account.protocol = match protocol.as_str() {
                    "responses" => Protocol::Responses,
                    "anthropic" => Protocol::Anthropic,
                    "ollama" => Protocol::Ollama,
                    _ => Protocol::Chat,
                };
            }
            account.key_env = key_env;
            account.models = models;
            account.automatic = !manual;
            account.cost = cost_class(&cost)?;
            let key = if key_stdin {
                let mut key = String::new();
                std::io::stdin().take(16_385).read_to_string(&mut key)?;
                anyhow::ensure!(key.len() <= 16_384, "API key input too large");
                Some(key.trim().to_owned())
            } else {
                None
            };
            let id = account.id.clone();
            registry.put(account, key.as_deref())?;
            println!("Saved {id}. Discovering available models…");
            match registry.probe(&id) {
                Ok(models) => {
                    println!("{} available models. Select one in /model (type a name to search).", models.len());
                    for model in models { println!("  {id}/{model}"); }
                }
                Err(error) => eprintln!("Saved connection; model discovery failed: {error}. Retry with loom provider test {id}."),
            }
        }
        Command::Remove { name } => {
            registry.remove(&name)?;
            println!("Removed {name} and its stored credential.");
        }
        Command::Enable { name } => {
            registry.enable(&name, true)?;
            println!("Enabled {name}.");
        }
        Command::Disable { name } => {
            registry.enable(&name, false)?;
            println!("Disabled {name}.");
        }
        Command::Test { name } => {
            let models = registry.probe(&name)?;
            println!("{name}: connected · {} models", models.len());
            for model in models {
                println!("  {model}");
            }
        }
        Command::Policy { max_cost } => {
            registry.set_max_cost(cost_class(&max_cost)?)?;
            println!("Maximum routing cost: {max_cost}");
        }
    }
    Ok(())
}
pub fn cost_class(text: &str) -> Result<CostClass> {
    match text {
        "local" => Some(CostClass::Local),
        "free" => Some(CostClass::Free),
        "subscription" => Some(CostClass::Subscription),
        "paid" => Some(CostClass::Paid),
        "unknown" => Some(CostClass::Unknown),
        _ => None,
    }
    .context("invalid cost class")
}

fn open_browser(url: &str) -> Result<()> {
    let mut command;
    #[cfg(target_os = "macos")]
    {
        command = std::process::Command::new("open");
        command.arg(url);
    }
    #[cfg(target_os = "windows")]
    {
        command = std::process::Command::new("rundll32");
        command.args(["url.dll,FileProtocolHandler", url]);
    }
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    {
        command = std::process::Command::new("xdg-open");
        command.arg(url);
    }
    // A missing browser launcher leaves the printed URL available for manual use.
    let _ = command.spawn();
    Ok(())
}
