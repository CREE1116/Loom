//! Resolve native executables and npm's Windows Codex shim without a shell.
use anyhow::{Context, Result, bail};
use std::{
    path::{Path, PathBuf},
    process::Command,
};

fn candidates(binary: &str, windows: bool) -> Vec<String> {
    if windows && Path::new(binary).extension().is_none() {
        vec![
            format!("{binary}.exe"),
            format!("{binary}.cmd"),
            format!("{binary}.bat"),
            format!("{binary}.com"),
            binary.into(),
        ]
    } else {
        vec![binary.into()]
    }
}
fn resolve(binary: &str, paths: &[PathBuf], windows: bool) -> Result<PathBuf> {
    let names = candidates(binary, windows);
    let path = Path::new(binary);
    let roots = if path.components().count() > 1 {
        vec![PathBuf::new()]
    } else {
        paths.to_vec()
    };
    let path = roots.iter().flat_map(|root| names.iter().map(move |name| root.join(name)))
        .find(|path| path.is_file())
        .with_context(|| format!("Cannot find {binary}. Install Codex (npm install -g @openai/codex), reopen the terminal, or pass --codex-bin PATH"))?
        ;
    dunce::canonicalize(path).context("Cannot resolve executable")
}
fn npm_entry(shim: &Path) -> Option<PathBuf> {
    let parent = shim.parent()?;
    [
        parent.join("node_modules/@openai/codex/bin/codex.js"),
        parent.parent()?.join("@openai/codex/bin/codex.js"),
    ]
    .into_iter()
    .find(|path| path.is_file())
}

pub(super) struct Executable {
    pub path: PathBuf,
    node: Option<PathBuf>,
}
impl Executable {
    pub fn resolve(binary: &str) -> Result<Self> {
        let paths: Vec<_> = std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).collect())
            .unwrap_or_default();
        let mut path = resolve(binary, &paths, cfg!(windows))?;
        let mut node = None;
        if cfg!(windows) {
            let extension = path
                .extension()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if matches!(extension.as_str(), "cmd" | "bat" | "ps1") {
                path = npm_entry(&path).with_context(|| format!("{} is not a supported Codex npm launcher. Pass --codex-bin pointing to codex.exe or @openai/codex/bin/codex.js", path.display()))?;
            }
            if path.extension().is_some_and(|s| s == "js") {
                let local_node = path
                    .ancestors()
                    .filter_map(|parent| {
                        let candidate = parent.join("node.exe");
                        candidate.is_file().then_some(candidate)
                    })
                    .next();
                node = Some(match local_node {
                    Some(node) => node,
                    None => resolve("node", &paths, true)?,
                });
            } else if !path
                .extension()
                .is_some_and(|s| s.eq_ignore_ascii_case("exe") || s.eq_ignore_ascii_case("com"))
            {
                bail!(
                    "Unsupported Windows executable: {}. Use --codex-bin PATH_TO_CODEX_EXE",
                    path.display()
                );
            }
        }
        Ok(Self { path, node })
    }
    pub fn command(&self) -> Command {
        if let Some(node) = &self.node {
            let mut command = Command::new(node);
            command.arg(&self.path);
            command
        } else {
            Command::new(&self.path)
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn windows_path_search_resolves_native_exe_and_global_npm_shim() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("codex.cmd"), "not executed").unwrap();
        let shim = resolve("codex", &[dir.path().into()], true).unwrap();
        assert_eq!(shim.file_name().unwrap(), "codex.cmd");
        #[cfg(windows)]
        assert!(
            !shim.to_string_lossy().starts_with(r"\\?\"),
            "Node/npm requires a normal Windows path"
        );
        let entry = dir.path().join("node_modules/@openai/codex/bin/codex.js");
        std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
        std::fs::write(&entry, "not executed").unwrap();
        assert_eq!(
            npm_entry(&shim).unwrap().canonicalize().unwrap(),
            entry.canonicalize().unwrap()
        );
        std::fs::write(dir.path().join("codex.exe"), "not executed").unwrap();
        assert_eq!(
            resolve("codex", &[dir.path().into()], true)
                .unwrap()
                .file_name()
                .unwrap(),
            "codex.exe"
        );
        assert!(resolve("missing", &[dir.path().into()], true).is_err());
    }
    #[test]
    fn npm_entry_supports_project_bin_shim_and_command_uses_literal_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let entry = dir.path().join("node_modules/@openai/codex/bin/codex.js");
        std::fs::create_dir_all(entry.parent().unwrap()).unwrap();
        std::fs::write(&entry, "").unwrap();
        assert_eq!(
            npm_entry(&dir.path().join("node_modules/.bin/codex.cmd")),
            Some(entry.clone())
        );
        let executable = Executable {
            path: entry.clone(),
            node: Some(PathBuf::from("node.exe")),
        };
        let mut command = executable.command();
        command.args(["app-server", "--listen", "ws://127.0.0.1:12345"]);
        assert_eq!(command.get_program(), "node.exe");
        assert_eq!(command.get_args().next().unwrap(), entry.as_os_str());
    }
}
