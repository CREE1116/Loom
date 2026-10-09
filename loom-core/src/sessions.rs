//! Durable conversation, goals and plans. Code checkpoints are exact raw revisions.
use crate::{
    base::{ArtifactRef, Revision, TaskId},
    control::{Task, TaskStatus},
};
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Turn {
    pub task: TaskId,
    pub goal: String,
    pub summary: String,
    pub status: TaskStatus,
    pub before: Revision,
    pub after: Option<Revision>,
    pub evidence: Vec<ArtifactRef>,
    #[serde(default)]
    pub tokens: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Goal {
    pub objective: String,
    pub status: String,
    pub tokens_used: u64,
    pub token_budget: Option<u64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PlanStep {
    pub step: String,
    pub status: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    pub turns: Vec<Turn>,
    pub summary: String,
    #[serde(default)]
    pub compacted: usize,
    #[serde(default)]
    pub archive: Option<ArtifactRef>,
    pub goal: Option<Goal>,
    pub plan: Vec<PlanStep>,
    pub updated_at: u64,
}
pub struct Sessions {
    root: PathBuf,
}
impl Sessions {
    pub fn open(root: &Path) -> Result<Self> {
        let root = root.join(".loom/sessions");
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }
    fn path(&self, id: &str) -> Result<PathBuf> {
        if id.is_empty()
            || id.len() > 100
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
        {
            bail!("invalid session id");
        }
        Ok(self.root.join(format!("{id}.json")))
    }
    fn lock(&self) -> Result<fs::File> {
        let lock = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join("store.lock"))?;
        lock.lock_exclusive()?;
        Ok(lock)
    }
    pub fn get(&self, id: &str) -> Result<Session> {
        serde_json::from_slice(&fs::read(self.path(id)?).context("unknown Native session")?)
            .map_err(Into::into)
    }
    fn write(&self, session: &Session) -> Result<()> {
        let mut file = tempfile::NamedTempFile::new_in(&self.root)?;
        file.write_all(&serde_json::to_vec(session)?)?;
        file.as_file().sync_all()?;
        file.persist(self.path(&session.id)?).map_err(|e| e.error)?;
        fs::File::open(&self.root)?.sync_all()?;
        Ok(())
    }
    pub fn ensure(&self, id: &str) -> Result<Session> {
        let _lock = self.lock()?;
        if self.path(id)?.exists() {
            return self.get(id);
        }
        let s = Session {
            id: id.into(),
            title: "Native Core".into(),
            turns: vec![],
            summary: String::new(),
            compacted: 0,
            archive: None,
            goal: None,
            plan: vec![],
            updated_at: now(),
        };
        self.write(&s)?;
        Ok(s)
    }
    pub fn create(&self) -> Result<Session> {
        self.ensure(&format!(
            "session-{:x}-{}",
            SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos(),
            std::process::id()
        ))
    }
    pub fn update(
        &self,
        id: &str,
        change: impl FnOnce(&mut Session) -> Result<()>,
    ) -> Result<Session> {
        self.ensure(id)?;
        let _lock = self.lock()?;
        let mut s = self.get(id)?;
        change(&mut s)?;
        s.updated_at = now();
        self.write(&s)?;
        Ok(s)
    }
    /// Remove one saved session. Revisions and artifacts it referenced are kept.
    pub fn delete(&self, id: &str) -> Result<()> {
        let path = self.path(id)?;
        let _lock = self.lock()?;
        fs::remove_file(path).context("unknown Native session")?;
        Ok(())
    }
    pub fn list(&self) -> Result<Vec<Session>> {
        let mut sessions = vec![];
        for entry in fs::read_dir(&self.root)? {
            let path = entry?.path();
            if path.extension().is_some_and(|s| s == "json") {
                sessions.push(serde_json::from_slice::<Session>(&fs::read(path)?)?);
            }
        }
        sessions.sort_by_key(|s| std::cmp::Reverse(s.updated_at));
        Ok(sessions)
    }
    pub fn record(&self, id: &str, task: &Task, after: Option<Revision>) -> Result<Session> {
        self.update(id, |s| {
            let previous = s.turns.iter().position(|t| t.task == task.id);
            let old_tokens = previous.map_or(0, |index| s.turns[index].tokens);
            let tokens = task
                .attempts
                .iter()
                .map(|a| a.input_tokens + a.output_tokens)
                .sum::<u64>();
            if s.turns.is_empty() {
                s.title = task.goal.chars().take(70).collect();
            }
            let evidence = task
                .attempts
                .iter()
                .map(|a| a.result.clone())
                .chain(task.patch.clone())
                .collect();
            let turn = Turn {
                task: task.id.clone(),
                goal: task.goal.clone(),
                summary: task.summary.clone(),
                status: task.status,
                before: task.base.clone(),
                after,
                evidence,
                tokens,
            };
            if let Some(index) = previous {
                s.turns[index] = turn;
            } else {
                s.turns.push(turn);
            }
            if let Some(goal) = &mut s.goal {
                goal.tokens_used = goal
                    .tokens_used
                    .saturating_add(tokens.saturating_sub(old_tokens));
                if goal
                    .token_budget
                    .is_some_and(|limit| goal.tokens_used >= limit)
                {
                    goal.status = "budget_limited".into();
                }
            }
            Ok(())
        })
    }
    pub fn context(&self, id: &str) -> Result<(String, Vec<ArtifactRef>)> {
        let session = self.ensure(id)?;
        let mut out = format!(
            "Conversation session {}:\n{}\n",
            session.id, session.summary
        );
        if let Some(goal) = session.goal.filter(|g| g.status == "active") {
            out.push_str(&format!("Persistent goal: {}\n", goal.objective));
        }
        if !session.plan.is_empty() {
            out.push_str(&format!(
                "Plan: {}\n",
                serde_json::to_string(&session.plan)?
            ));
        }
        let mut refs = vec![];
        refs.extend(session.archive);
        for t in session.turns.into_iter().skip(session.compacted) {
            out.push_str(&format!(
                "\nUser: {}\nAssistant: {}\nTask {}: {:?}; evidence: {}\n",
                t.goal,
                t.summary,
                t.task,
                t.status,
                serde_json::to_string(&t.evidence)?
            ));
            refs.extend(t.evidence);
        }
        Ok((out, refs))
    }
    pub fn compact(&self, id: &str, keep: usize) -> Result<Session> {
        self.update(id, |s| {
            let count = s.turns.len().saturating_sub(keep);
            if count <= s.compacted {
                return Ok(());
            }
            use crate::base::ArtifactStore;
            let store = crate::base::FileArtifactStore::open(
                self.root.parent().unwrap().parent().unwrap(),
            )?;
            s.archive = Some(store.put(&serde_json::to_vec(&s.turns[..count])?)?);
            s.summary = String::from("Earlier conversation summary (full details in archive):\n");
            for t in &s.turns[..count] {
                s.summary.push_str(&format!(
                    "Task {} ({:?}): {} => {}.\n",
                    t.task,
                    t.status,
                    t.goal.chars().take(200).collect::<String>(),
                    t.summary.chars().take(400).collect::<String>()
                ));
            }
            if s.summary.len() > 16_000 {
                let mut start = s.summary.len() - 16_000;
                while !s.summary.is_char_boundary(start) {
                    start += 1;
                }
                s.summary = s.summary[start..].to_owned();
            }
            s.summary.push_str(&format!(
                "\nConversation archive: {}\n",
                serde_json::to_string(&s.archive)?
            ));
            s.compacted = count;
            Ok(())
        })
    }
    pub fn fork(&self, id: &str, turn: &str) -> Result<Session> {
        let original = self.get(id)?;
        let index = original
            .turns
            .iter()
            .position(|t| t.task.0 == turn)
            .context("unknown fork turn")?;
        let mut s = self.create()?;
        s.title = format!("분기 · {}", original.title);
        s.turns = original.turns[..=index].to_vec();
        s.summary = original.summary;
        s.compacted = original.compacted.min(s.turns.len());
        s.archive = original.archive;
        s.goal = original.goal;
        s.plan = original.plan;
        let _lock = self.lock()?;
        self.write(&s)?;
        Ok(s)
    }
    pub fn rewind_target(&self, id: &str, turn: &str) -> Result<(Revision, Revision)> {
        let s = self.get(id)?;
        let selected = s
            .turns
            .iter()
            .find(|t| t.task.0 == turn)
            .context("unknown rewind turn")?;
        let target = selected
            .after
            .clone()
            .context("turn has no accepted code checkpoint")?;
        let current = s
            .turns
            .iter()
            .rev()
            .find_map(|t| t.after.clone())
            .context("session has no code checkpoint")?;
        Ok((target, current))
    }
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
