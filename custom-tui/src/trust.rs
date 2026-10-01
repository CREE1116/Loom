//! Inspect Codex's own per-project trust record. This file is read-only;
//! persistent trust decisions go through the app-server config API.
use std::{fs, path::Path};

pub fn project_trust(cwd: &Path, config_file: &Path) -> Option<bool> {
    let config = fs::read_to_string(config_file).ok()?;
    let value: toml::Value = toml::from_str(&config).ok()?;
    let key = cwd.to_str()?;
    let project = value.get("projects")?.get(key)?;
    match project.get("trust_level")?.as_str()? {
        "trusted" => Some(true),
        "untrusted" => Some(false),
        _ => None,
    }
}

pub fn local_trust(cwd: &Path) -> Option<bool> {
    let config = std::env::var_os("CODEX_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(|home| std::path::PathBuf::from(home).join(".codex"))
        })?
        .join("config.toml");
    project_trust(cwd, &config)
}

/// Quote each TOML path segment, because directory names may contain dots.
pub fn trust_key_path(cwd: &Path) -> Option<String> {
    let path = cwd.to_str()?;
    let quoted = path.replace('\\', "\\\\").replace('"', "\\\"");
    Some(format!("projects.\"{quoted}\".trust_level"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reads_only_exact_codex_trust_and_quotes_project_path() {
        let dir = tempfile::tempdir().unwrap();
        let config = dir.path().join("config.toml");
        fs::write(&config, "[projects.\"/tmp/example.project\"]\ntrust_level = 'trusted'\n[projects.\"/tmp/no\"]\ntrust_level = 'untrusted'\n").unwrap();
        assert_eq!(
            project_trust(Path::new("/tmp/example.project"), &config),
            Some(true)
        );
        assert_eq!(project_trust(Path::new("/tmp/no"), &config), Some(false));
        assert_eq!(project_trust(Path::new("/tmp/missing"), &config), None);
        assert_eq!(
            trust_key_path(Path::new("/tmp/example.project")).unwrap(),
            "projects.\"/tmp/example.project\".trust_level"
        );
    }
}
