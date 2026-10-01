//! One immutable repository index, shared by every worker, with deduplicated queries.
use anyhow::{Context, Result, bail};
use sha2::{Digest, Sha256};
use std::{
    borrow::Cow,
    collections::{BTreeMap, HashMap},
    fs,
    path::{Component, Path, PathBuf},
    process::Command,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    time::SystemTime,
};

const MAX_FILE_BYTES: u64 = 512 * 1024;
const MAX_HOT_BYTES: usize = 64 * 1024 * 1024;
const MAX_RESULTS: usize = 24;
const MAX_CACHE_ENTRIES: usize = 256;

#[derive(Clone, Debug)]
pub struct Evidence {
    pub reference: String,
    pub path: PathBuf,
    pub line: usize,
    pub excerpt: String,
    pub file_digest: String,
}

#[derive(Clone, Debug)]
pub struct ExploreResult {
    pub revision: String,
    pub query: String,
    pub evidence: Vec<Evidence>,
    pub truncated: bool,
    pub skipped_files: usize,
}

impl ExploreResult {
    pub fn compact(&self) -> String {
        let mut output = format!("LOCAL · revision {}\n", &self.revision[..12]);
        for item in &self.evidence {
            output.push_str(&format!(
                "{}:{} {}\nRef: {}\n",
                item.path.display(),
                item.line,
                item.excerpt,
                item.reference
            ));
        }
        if self.evidence.is_empty() {
            output.push_str("일치하는 코드 증거가 없습니다.\n");
        }
        if self.truncated {
            output.push_str("결과가 제한되었습니다. 더 구체적인 탐색이 필요합니다.\n");
        }
        if self.skipped_files > 0 {
            output.push_str(&format!(
                "제외된 파일: {} (크기·바이너리·접근 제한)\n",
                self.skipped_files
            ));
        }
        output
    }
}

#[derive(Clone, PartialEq, Eq)]
struct Stamp {
    size: u64,
    modified: Option<SystemTime>,
}
#[derive(Clone)]
enum StoredText {
    Hot(Arc<str>),
    Cold(Arc<ArchivedText>),
}
struct ArchivedText {
    path: PathBuf,
    _directory: Arc<tempfile::TempDir>,
}
impl StoredText {
    fn read(&self) -> Result<Cow<'_, str>> {
        match self {
            Self::Hot(text) => Ok(Cow::Borrowed(text)),
            Self::Cold(archive) => Ok(Cow::Owned(
                fs::read_to_string(&archive.path)
                    .context("Reading immutable repository archive")?,
            )),
        }
    }
}
struct IndexedFile {
    path: PathBuf,
    digest: String,
    text: StoredText,
}
pub struct RepositorySnapshot {
    root: PathBuf,
    pub revision: String,
    files: Vec<IndexedFile>,
    skipped: usize,
}
type QueryCell = Arc<OnceLock<std::result::Result<Arc<ExploreResult>, String>>>;
type QueryKey = (String, String, usize);
#[derive(Default)]
struct State {
    stamps: BTreeMap<PathBuf, Stamp>,
    snapshot: Option<Arc<RepositorySnapshot>>,
}

pub struct RepositoryExplorer {
    root: PathBuf,
    state: Mutex<State>,
    queries: Mutex<HashMap<QueryKey, QueryCell>>,
    builds: AtomicUsize,
    searches: AtomicUsize,
    hot_budget: usize,
}

impl RepositoryExplorer {
    pub fn new(root: &Path) -> Result<Self> {
        Ok(Self {
            root: root
                .canonicalize()
                .context("Repository root does not exist")?,
            state: Mutex::new(State::default()),
            queries: Mutex::new(HashMap::new()),
            builds: AtomicUsize::new(0),
            searches: AtomicUsize::new(0),
            hot_budget: MAX_HOT_BYTES,
        })
    }

    /// Callers retain this Arc to pin all parallel tasks to the same revision.
    pub fn snapshot(&self) -> Result<Arc<RepositorySnapshot>> {
        self.refresh(false)
    }

    fn refresh(&self, force: bool) -> Result<Arc<RepositorySnapshot>> {
        let paths = self.paths()?;
        let mut stamps = BTreeMap::new();
        for path in paths {
            // Never follow a symlink out of the canonical repository.
            let metadata = match fs::symlink_metadata(self.root.join(&path)) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => {
                    return Err(error).with_context(|| format!("Reading {}", path.display()));
                }
            };
            if metadata.is_file() {
                stamps.insert(
                    path,
                    Stamp {
                        size: metadata.len(),
                        modified: metadata.modified().ok(),
                    },
                );
            }
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Repository index lock failed"))?;
        if !force
            && state.stamps == stamps
            && let Some(snapshot) = &state.snapshot
        {
            return Ok(Arc::clone(snapshot));
        }
        let mut files = Vec::new();
        let mut revision = Sha256::new();
        let mut used = 0;
        let mut archive: Option<Arc<tempfile::TempDir>> = None;
        let mut skipped = 0;
        let previous: HashMap<_, _> = state
            .snapshot
            .as_ref()
            .into_iter()
            .flat_map(|snapshot| &snapshot.files)
            .map(|file| (&file.path, file))
            .collect();
        for (path, stamp) in &stamps {
            revision.update(path.as_os_str().as_encoded_bytes());
            revision.update([0]);
            if stamp.size > MAX_FILE_BYTES {
                skipped += 1;
                revision.update(stamp.size.to_le_bytes());
                revision.update(format!("{:?}", stamp.modified));
                continue;
            }
            // Reuse parsed contents when the file stamp is unchanged. A forced
            // refresh also catches external writes that preserved the stamp.
            let reused = (!force && state.stamps.get(path) == Some(stamp))
                .then(|| previous.get(path).copied())
                .flatten();
            let (mut text, digest) = if let Some(previous) = reused {
                (previous.text.clone(), previous.digest.clone())
            } else {
                let bytes = fs::read(self.root.join(path))
                    .with_context(|| format!("Indexing {}", path.display()))?;
                let digest = format!("{:x}", Sha256::digest(&bytes));
                if bytes.contains(&0) {
                    revision.update(&digest);
                    skipped += 1;
                    continue;
                }
                let Ok(text) = String::from_utf8(bytes) else {
                    revision.update(&digest);
                    skipped += 1;
                    continue;
                };
                let text = if used + text.len() <= self.hot_budget {
                    StoredText::Hot(Arc::from(text))
                } else {
                    let directory = match &archive {
                        Some(directory) => Arc::clone(directory),
                        None => {
                            let directory = Arc::new(
                                tempfile::Builder::new()
                                    .prefix("loom-repository-")
                                    .tempdir()?,
                            );
                            archive = Some(Arc::clone(&directory));
                            directory
                        }
                    };
                    let archive_path = directory.path().join(files.len().to_string());
                    fs::write(&archive_path, text).context("Archiving repository source")?;
                    StoredText::Cold(Arc::new(ArchivedText {
                        path: archive_path,
                        _directory: directory,
                    }))
                };
                (text, digest)
            };
            // Newly inserted paths may precede previously hot files. Spill those
            // reused files too, so refresh cannot grow past the hot budget.
            if let StoredText::Hot(contents) = &text
                && used + contents.len() > self.hot_budget
            {
                let directory = match &archive {
                    Some(directory) => Arc::clone(directory),
                    None => {
                        let directory = Arc::new(
                            tempfile::Builder::new()
                                .prefix("loom-repository-")
                                .tempdir()?,
                        );
                        archive = Some(Arc::clone(&directory));
                        directory
                    }
                };
                let archive_path = directory.path().join(files.len().to_string());
                fs::write(&archive_path, contents.as_bytes())?;
                text = StoredText::Cold(Arc::new(ArchivedText {
                    path: archive_path,
                    _directory: directory,
                }));
            }
            revision.update(&digest);
            if let StoredText::Hot(text) = &text {
                used += text.len();
            }
            files.push(IndexedFile {
                path: path.clone(),
                digest,
                text,
            });
        }
        let snapshot = Arc::new(RepositorySnapshot {
            root: self.root.clone(),
            revision: format!("{:x}", revision.finalize()),
            files,
            skipped,
        });
        state.stamps = stamps;
        state.snapshot = Some(Arc::clone(&snapshot));
        self.builds.fetch_add(1, Ordering::Relaxed);
        Ok(snapshot)
    }

    pub fn explore(&self, query: &str, budget_chars: usize) -> Result<Arc<ExploreResult>> {
        let mut snapshot = self.snapshot()?;
        // A cache hit still validates source bytes for the returned evidence.
        // Immutable snapshots remain usable by running tasks as historical evidence.
        for _ in 0..2 {
            let result = self.explore_snapshot(&snapshot, query, budget_chars)?;
            if result.evidence.iter().all(|item| {
                fs::read(self.root.join(&item.path))
                    .is_ok_and(|b| format!("{:x}", Sha256::digest(b)) == item.file_digest)
            }) {
                return Ok(result);
            }
            snapshot = self.refresh(true)?;
        }
        bail!("Repository changed during exploration; retry on a stable revision")
    }

    pub fn explore_snapshot(
        &self,
        snapshot: &Arc<RepositorySnapshot>,
        query: &str,
        budget_chars: usize,
    ) -> Result<Arc<ExploreResult>> {
        if snapshot.root != self.root {
            bail!("Snapshot belongs to a different repository");
        }
        let query = query.split_whitespace().collect::<Vec<_>>().join(" ");
        if query.is_empty() || query.len() > 512 {
            bail!("탐색어는 1~512바이트여야 합니다.");
        }
        let budget = budget_chars.clamp(256, 8192);
        let key = (snapshot.revision.clone(), query.clone(), budget);
        let cell = {
            let mut queries = self
                .queries
                .lock()
                .map_err(|_| anyhow::anyhow!("Query cache lock failed"))?;
            if queries.len() >= MAX_CACHE_ENTRIES && !queries.contains_key(&key) {
                queries.retain(|_, cell| cell.get().is_none());
            }
            Arc::clone(queries.entry(key).or_default())
        };
        match cell.get_or_init(|| {
            self.searches.fetch_add(1, Ordering::Relaxed);
            search(snapshot, &query, budget)
                .map(Arc::new)
                .map_err(|error| format!("{error:#}"))
        }) {
            Ok(result) => Ok(Arc::clone(result)),
            Err(reason) => bail!("{reason}"),
        }
    }

    pub fn counts(&self) -> (usize, usize) {
        (
            self.builds.load(Ordering::Relaxed),
            self.searches.load(Ordering::Relaxed),
        )
    }

    fn paths(&self) -> Result<Vec<PathBuf>> {
        let output = Command::new("git")
            .arg("-C")
            .arg(&self.root)
            .args([
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
                "--",
                ".",
            ])
            .output()
            .context("Listing repository files")?;
        let bytes = if output.status.success() {
            output.stdout
        } else {
            let output = Command::new("rg")
                .args([
                    "--files",
                    "-0",
                    "--hidden",
                    "-g",
                    "!.git",
                    "-g",
                    "!.custom-tui",
                    "-g",
                    "!target",
                    "-g",
                    "!node_modules",
                ])
                .current_dir(&self.root)
                .output()
                .context("Listing workspace files")?;
            if !output.status.success() && output.status.code() != Some(1) {
                bail!("Workspace file listing failed");
            }
            output.stdout
        };
        let mut paths = Vec::new();
        for raw in bytes.split(|b| *b == 0).filter(|b| !b.is_empty()) {
            let path = PathBuf::from(
                String::from_utf8(raw.to_vec()).context("Workspace contains a non-UTF-8 path")?,
            );
            if !path.is_absolute()
                && !path
                    .components()
                    .any(|c| !matches!(c, Component::Normal(_)))
                && !path.components().any(|c| {
                    matches!(
                        c.as_os_str().to_str(),
                        Some(".git" | ".custom-tui" | "target" | "node_modules")
                    )
                })
            {
                paths.push(path);
            }
        }
        paths.sort();
        paths.dedup();
        Ok(paths)
    }
}

fn search(snapshot: &RepositorySnapshot, query: &str, budget: usize) -> Result<ExploreResult> {
    let terms: Vec<_> = query
        .to_lowercase()
        .split_whitespace()
        .map(str::to_owned)
        .collect();
    let mut candidates = Vec::new();
    for file in &snapshot.files {
        let path = file.path.to_string_lossy().to_lowercase();
        let contents = file.text.read()?;
        for (index, line) in contents.lines().enumerate() {
            let text = line.to_lowercase();
            let matches = terms
                .iter()
                .filter(|term| text.contains(term.as_str()))
                .count();
            let path_matches = terms
                .iter()
                .filter(|term| path.contains(term.as_str()))
                .count();
            if matches == 0 && (path_matches == 0 || index > 2) {
                continue;
            }
            // This is lexical evidence ranking, not a claimed AST/call graph.
            let declaration = [
                "fn ",
                "struct ",
                "enum ",
                "trait ",
                "def ",
                "class ",
                "function ",
                "interface ",
            ]
            .iter()
            .any(|prefix| text.contains(prefix));
            let score = matches * 10 + path_matches * 3 + usize::from(declaration) * 5;
            // Keep only the best candidates; broad searches cannot retain the
            // full archive or an unbounded list of matching source lines.
            if candidates.len() > MAX_RESULTS
                && candidates.last().is_some_and(
                    |candidate: &(usize, &PathBuf, usize, String, &String, usize)| {
                        score < candidate.0
                            || (score == candidate.0
                                && (&file.path, index + 1) >= (candidate.1, candidate.2))
                    },
                )
            {
                continue;
            }
            candidates.push((
                score,
                &file.path,
                index + 1,
                line.trim().chars().take(240).collect::<String>(),
                &file.digest,
                line.trim().chars().count(),
            ));
            candidates.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)).then(a.2.cmp(&b.2)));
            candidates.truncate(MAX_RESULTS + 1);
        }
    }
    candidates.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(b.1)).then(a.2.cmp(&b.2)));
    let mut result = ExploreResult {
        revision: snapshot.revision.clone(),
        query: query.into(),
        evidence: vec![],
        truncated: false,
        skipped_files: snapshot.skipped,
    };
    let mut used = 0;
    for (_, path, line, text, digest, original_chars) in candidates {
        let reference = format!(
            "repo://{}/{}#L{}",
            &snapshot.revision[..12],
            path.display(),
            line
        );
        let overhead = reference.chars().count() + path.to_string_lossy().chars().count() + 32;
        let available = budget.saturating_sub(used + overhead).min(240);
        if available == 0 || result.evidence.len() >= MAX_RESULTS {
            result.truncated = true;
            break;
        }
        let excerpt: String = text.trim().chars().take(available).collect();
        if original_chars > available {
            result.truncated = true;
        }
        used += overhead + excerpt.chars().count();
        result.evidence.push(Evidence {
            reference,
            path: path.clone(),
            line,
            excerpt,
            file_digest: digest.clone(),
        });
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hot_budget_overflow_preserves_all_sources_and_historical_snapshots() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("a.rs"), "struct HotResource;\n").unwrap();
        fs::write(directory.path().join("z.rs"), "struct ColdResource;\n").unwrap();
        let mut explorer = RepositoryExplorer::new(directory.path()).unwrap();
        explorer.hot_budget = 20;
        let old = explorer.snapshot().unwrap();
        assert!(
            old.files
                .iter()
                .any(|file| matches!(file.text, StoredText::Cold(_)))
        );
        let found = explorer
            .explore_snapshot(&old, "ColdResource", 4096)
            .unwrap();
        assert_eq!(found.evidence.len(), 1);
        assert_eq!(found.skipped_files, 0);
        fs::write(directory.path().join("z.rs"), "struct NewResource;\n").unwrap();
        let current = explorer.snapshot().unwrap();
        assert_ne!(current.revision, old.revision);
        assert_eq!(
            explorer
                .explore_snapshot(&current, "NewResource", 4096)
                .unwrap()
                .evidence
                .len(),
            1
        );
        // Old archive remains owned by the pinned snapshot after refresh.
        assert_eq!(
            explorer
                .explore_snapshot(&old, "Resource", 4096)
                .unwrap()
                .evidence
                .len(),
            2
        );
        fs::write(directory.path().join("0.rs"), "struct First;\n").unwrap();
        let expanded = explorer.snapshot().unwrap();
        let hot_bytes: usize = expanded
            .files
            .iter()
            .map(|file| match &file.text {
                StoredText::Hot(text) => text.len(),
                _ => 0,
            })
            .sum();
        assert!(hot_bytes <= explorer.hot_budget);
        drop(explorer);
        assert!(old.files.iter().all(|file| file.text.read().is_ok()));
    }
}
