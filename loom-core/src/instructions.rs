//! Project guidance and explicitly selected skills, with path-scoped instructions.
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs,
    path::{Component, Path, PathBuf},
};

const LIMIT: usize = 64_000;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Skill {
    pub name: String,
    pub path: PathBuf,
    pub description: String,
}
pub struct Instructions {
    root: PathBuf,
    seen: BTreeSet<PathBuf>,
}
impl Instructions {
    pub fn open(root: &Path) -> Result<Self> {
        Ok(Self {
            root: root.canonicalize()?,
            seen: BTreeSet::new(),
        })
    }
    pub fn root_guidance(&mut self) -> Result<String> {
        let mut paths = self
            .root
            .ancestors()
            .map(Path::to_owned)
            .collect::<Vec<_>>();
        paths.reverse();
        let mut out = String::new();
        for directory in paths {
            self.append(&directory.join("AGENTS.md"), &mut out)?;
            // Project-local Loom guidance complements the cross-tool AGENTS contract.
            if directory.starts_with(&self.root) {
                self.append_loom(&directory, &mut out)?;
            }
        }
        Ok(out)
    }
    pub fn for_path(&mut self, path: &Path) -> Result<String> {
        if path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir)) {
            bail!("instruction path must be inside the repository");
        }
        let mut out = String::new();
        let mut dir = self.root.clone();
        if let Some(parent) = path.parent() {
            for component in parent.components() {
                if let Component::Normal(part) = component {
                    dir.push(part);
                    self.append(&dir.join("AGENTS.md"), &mut out)?;
                    self.append_loom(&dir, &mut out)?;
                }
            }
        }
        Ok(out)
    }
    pub fn loaded_paths(&self) -> impl Iterator<Item = &PathBuf> {
        self.seen.iter()
    }
    fn append_loom(&mut self, directory: &Path, out: &mut String) -> Result<()> {
        let uppercase = directory.join("LOOM.md");
        self.append(&if uppercase.exists() { uppercase } else { directory.join("loom.md") }, out)
    }
    fn append(&mut self, path: &Path, out: &mut String) -> Result<()> {
        if !path.exists()
            || self
                .seen
                .contains(&path.canonicalize().unwrap_or_else(|_| path.to_owned()))
        {
            return Ok(());
        }
        let absolute = path.canonicalize()?;
        if path.starts_with(&self.root) && !absolute.starts_with(&self.root) {
            bail!("instruction symlink escapes repository");
        }
        let text =
            fs::read_to_string(&absolute).with_context(|| format!("reading {}", path.display()))?;
        if text.len() + out.len() > LIMIT {
            bail!(
                "project instructions exceed {LIMIT} bytes: {}",
                path.display()
            );
        }
        out.push_str(&format!(
            "\nProject instructions ({}, applies below its directory):\n{}\n",
            path.display(),
            text
        ));
        self.seen.insert(absolute);
        Ok(())
    }
}
/// Skills installed for Loom or for other agents: `.agents`, `.codex` and `.claude` skill
/// directories of the project and of the home directory, and the skills that Claude Code
/// and Codex plugins ship. Project skills come first and win a name clash; a skill that
/// cannot be read is skipped rather than hiding every other one.
/// Skills that ship with Loom, installed into `~/.loom/skills` on first use.
const BUNDLED: [(&str, &str); 3] = [
    ("loom-guide", include_str!("../skills/loom-guide/SKILL.md")),
    ("skill-builder", include_str!("../skills/skill-builder/SKILL.md")),
    ("loom-extension", include_str!("../skills/loom-extension/SKILL.md")),
];

/// Writes the bundled skills under `home/.loom/skills`. A copy is replaced only while it
/// is exactly what Loom wrote last time (recorded next to it); an edited copy is the
/// user's and stays.
fn install_bundled(home: &Path) {
    use sha2::{Digest, Sha256};
    let digest = |text: &str| format!("{:x}", Sha256::digest(text.as_bytes()));
    for (name, text) in BUNDLED {
        let dir = home.join(".loom/skills").join(name);
        let path = dir.join("SKILL.md");
        let record = dir.join(".bundled");
        if let Ok(current) = fs::read_to_string(&path) {
            let written = fs::read_to_string(&record).unwrap_or_default();
            if current == text || digest(&current) != written.trim() {
                continue; // up to date, or changed by the user
            }
        }
        let _ = fs::create_dir_all(&dir)
            .and_then(|_| fs::write(&path, text))
            .and_then(|_| fs::write(&record, digest(text)));
    }
}

pub fn discover(root: &Path) -> Result<Vec<Skill>> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    if let Some(home) = &home {
        static INSTALLED: std::sync::Once = std::sync::Once::new();
        INSTALLED.call_once(|| install_bundled(home));
    }
    let mut directories = Vec::new();
    for base in std::iter::once(root.to_path_buf()).chain(home.clone()) {
        for agent in [".loom", ".agents", ".codex", ".claude"] {
            directories.push((base.join(agent).join("skills"), 2));
        }
    }
    if let Some(home) = &home {
        // Plugin caches: <cache>/<plugin>/<version>/skills/<skill>/SKILL.md and similar.
        directories.push((home.join(".claude/plugins/cache"), 6));
        directories.push((home.join(".codex/plugins/cache"), 6));
    }
    let mut skills = Vec::new();
    let mut paths = BTreeSet::new();
    let mut names = BTreeSet::new();
    for (directory, depth) in directories {
        let mut found = Vec::new();
        find_skills(&directory, depth, &mut found);
        found.sort();
        for path in found {
            let Ok(path) = path.canonicalize() else { continue };
            if !paths.insert(path.clone()) {
                continue;
            }
            let Ok(text) = fs::read_to_string(&path) else { continue };
            let value = |key: &str| {
                text.lines()
                    .find_map(|l| l.strip_prefix(key))
                    .map(|s| s.trim().trim_matches(['\'', '"']).to_owned())
            };
            let name = value("name:").unwrap_or_else(|| {
                path.parent()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            });
            if name.is_empty() || !names.insert(name.clone()) {
                continue;
            }
            skills.push(Skill { name, description: value("description:").unwrap_or_default(), path });
        }
    }
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(skills)
}

/// `SKILL.md` files below `directory`, at most `depth` levels down; dependency and VCS
/// directories are not searched.
fn find_skills(directory: &Path, depth: usize, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(directory) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        let is_dir = kind.is_dir() || (kind.is_symlink() && path.is_dir());
        if !is_dir {
            continue;
        }
        let skill = path.join("SKILL.md");
        if skill.is_file() {
            out.push(skill);
            continue;
        }
        let name = entry.file_name();
        if depth > 1 && !matches!(name.to_str(), Some("node_modules" | ".git" | "target" | "dist" | "build")) {
            find_skills(&path, depth - 1, out);
        }
    }
}
pub fn load_skill(path: &Path) -> Result<String> {
    let text =
        fs::read_to_string(path).with_context(|| format!("reading skill {}", path.display()))?;
    if text.len() > LIMIT {
        bail!("skill exceeds {LIMIT} bytes");
    }
    Ok(format!(
        "Skill instructions ({}; relative references resolve against {}):\n{}",
        path.display(),
        path.parent().unwrap_or(Path::new(".")).display(),
        text
    ))
}
pub fn initial(root: &Path, goal: &str, selected: &[PathBuf]) -> Result<String> {
    let mut instructions = Instructions::open(root)?;
    let mut out = instructions.root_guidance()?;
    out.push_str(&skill_guidance(root, goal, selected, "enable_tools skills, then use_skill to load one")?);
    Ok(out)
}

/// The skills part of the system prompt: installed skills by name (project skills with
/// their description; global ones only named, since this goes with every model call), and
/// the full text of skills the user selected or named as `$name` in the request.
pub fn skill_guidance(root: &Path, goal: &str, selected: &[PathBuf], how: &str) -> Result<String> {
    let mut out = String::new();
    let skills = discover(root)?;
    let mut chosen = selected.iter().cloned().collect::<BTreeSet<_>>();
    for skill in &skills {
        if goal.contains(&format!("${}", skill.name)) {
            chosen.insert(skill.path.clone());
        }
    }
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    let (project, global): (Vec<_>, Vec<_>) =
        skills.iter().partition(|s| s.path.starts_with(&root));
    if !project.is_empty() || !global.is_empty() {
        out.push_str(&format!("\nSkills ({how}):\n"));
        for skill in project {
            let mut description: String = skill.description.chars().take(160).collect();
            if description.len() < skill.description.len() {
                description.push('…');
            }
            out.push_str(&format!("{}: {description}\n", skill.name));
        }
        if global.iter().any(|s| s.name == "loom-guide") {
            out.push_str("For a question or problem about Loom itself (its files, models, skills, extensions, settings), load loom-guide first.\n");
        }
        if !global.is_empty() {
            out.push_str(&format!(
                "user skills: {}\n",
                global.iter().map(|s| s.name.as_str()).collect::<Vec<_>>().join(", ")
            ));
        }
    }
    for path in chosen {
        out.push_str(&load_skill(&path)?);
        out.push('\n');
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn skills_of_other_agents_and_nested_groups_are_found() {
        let root = tempfile::tempdir().unwrap();
        let write = |rel: &str, text: &str| {
            let path = root.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write(".claude/skills/review/SKILL.md", "---\nname: review\ndescription: Review code\n---\n");
        write(".claude/skills/synced/humanize/SKILL.md", "---\nname: humanize\n---\n");
        write(".agents/skills/review/SKILL.md", "---\nname: review\ndescription: shadowed\n---\n");
        write(".codex/skills/node_modules/x/SKILL.md", "---\nname: hidden\n---\n");
        let skills = super::discover(root.path()).unwrap();
        let names: Vec<_> = skills.iter().map(|s| s.name.as_str()).collect();
        assert!(names.contains(&"review") && names.contains(&"humanize"), "{names:?}");
        assert!(!names.contains(&"hidden"), "{names:?}");
        assert_eq!(skills.iter().filter(|s| s.name == "review").count(), 1);
    }

    /// Lists what this machine has installed. `cargo test --lib installed_skills -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn installed_skills() {
        for skill in super::discover(&std::env::current_dir().unwrap()).unwrap() {
            eprintln!("{} · {}", skill.name, skill.path.display());
        }
    }

    use super::*;
    #[test]
    fn root_and_scoped_guidance_load_agents_and_loom_without_duplicates() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("src")).unwrap();
        fs::write(root.path().join("AGENTS.md"), "root agents").unwrap();
        fs::write(root.path().join("loom.md"), "root loom").unwrap();
        fs::write(root.path().join("src/LOOM.md"), "scoped loom").unwrap();
        let mut instructions = Instructions::open(root.path()).unwrap();
        let root_text = instructions.root_guidance().unwrap();
        assert!(root_text.contains("root agents") && root_text.contains("root loom"));
        assert_eq!(root_text.matches("root loom").count(), 1);
        let scoped = instructions.for_path(Path::new("src/file.rs")).unwrap();
        assert!(scoped.contains("scoped loom") && !scoped.contains("root loom"));
        assert!(
            instructions
                .for_path(Path::new("src/file.rs"))
                .unwrap()
                .is_empty()
        );
    }
}
