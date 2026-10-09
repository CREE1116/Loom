//! DAG scheduling is deterministic; the main model supplies semantic goals.
use crate::agent::{
    events::{Event, Stage},
    worker::{self, Shared},
};
use anyhow::{Result, ensure};
use crate::{base::Revision, effects::Overlay};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, PathBuf},
};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Unit {
    pub id: String,
    pub goal: String,
    pub write_files: Vec<PathBuf>,
    pub depends_on: Vec<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub units: Vec<Unit>,
}
impl Plan {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            !self.units.is_empty() && self.units.len() <= 16,
            "DAG requires 1-16 units"
        );
        let ids: BTreeSet<_> = self.units.iter().map(|u| u.id.as_str()).collect();
        ensure!(ids.len() == self.units.len(), "duplicate unit id");
        for u in &self.units {
            ensure!(
                !u.id.is_empty()
                    && u.id != "main"
                    && u.id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "invalid unit id"
            );
            ensure!(!u.goal.trim().is_empty(), "unit goal required");
            ensure!(
                u.write_files.iter().all(|p| !p.as_os_str().is_empty()
                    && p.components().all(|c| matches!(c, Component::Normal(_)))),
                "invalid write scope"
            );
            ensure!(
                u.depends_on
                    .iter()
                    .all(|d| d != &u.id && ids.contains(d.as_str())),
                "invalid dependency"
            );
        }
        let mut done = BTreeSet::new();
        while done.len() < self.units.len() {
            let ready: Vec<_> = self
                .units
                .iter()
                .filter(|u| !done.contains(&u.id) && u.depends_on.iter().all(|d| done.contains(d)))
                .map(|u| u.id.clone())
                .collect();
            ensure!(!ready.is_empty(), "cyclic DAG");
            done.extend(ready);
        }
        Ok(())
    }
}

pub fn execute(shared: &Shared<'_>, plan: &Plan, parent: &mut Overlay) -> Result<Value> {
    plan.validate()?;
    let mut done = BTreeSet::new();
    let result = execute_inner(shared, plan, parent, &mut done);
    if let Err(error) = &result {
        for unit in plan.units.iter().filter(|unit| !done.contains(&unit.id)) {
            let mut event = Event::new(
                &unit.id,
                Stage::AgentFinished,
                &unit.goal,
                json!({"reason":error.to_string(),"depends_on":unit.depends_on}),
            );
            event.failed = true;
            shared.emit(event);
        }
    }
    result
}
fn execute_inner(
    shared: &Shared<'_>,
    plan: &Plan,
    parent: &mut Overlay,
    done: &mut BTreeSet<String>,
) -> Result<Value> {
    shared.emit(Event::new(
        "main",
        Stage::Orchestrating,
        "DAG",
        json!({"units":plan.units}),
    ));
    let mut receipts = BTreeMap::new();
    while done.len() < plan.units.len() {
        ensure!(!shared.stopped(), "DAG cancelled or deadline reached");
        let view = shared.hub.view_cancellable(parent.dir(), &shared.cancel)?;
        let mut writes = BTreeSet::new();
        let mut reads = BTreeSet::new();
        let mut wave = Vec::new();
        // Choose a deterministic wave, conservatively serialize name-resolved read/write conflicts.
        for unit in plan
            .units
            .iter()
            .filter(|u| !done.contains(&u.id) && u.depends_on.iter().all(|d| done.contains(d)))
        {
            let own: BTreeSet<_> = unit.write_files.iter().cloned().collect();
            let inferred: BTreeSet<_> = unit
                .write_files
                .iter()
                .flat_map(|p| {
                    view.world
                        .graph
                        .in_file(&p.to_string_lossy())
                        .collect::<Vec<_>>()
                })
                .flat_map(|s| view.world.graph.resolve(&s.id))
                .map(|s| PathBuf::from(&s.file))
                .collect();
            if !own.is_disjoint(&writes)
                || !own.is_disjoint(&reads)
                || !inferred.is_disjoint(&writes)
            {
                continue;
            }
            writes.extend(own);
            reads.extend(inferred);
            wave.push(unit);
            if wave.len() == shared.options.parallelism.clamp(1, 4) {
                break;
            }
        }
        ensure!(!wave.is_empty(), "no runnable DAG unit");
        let base = Revision::capture_cancellable(parent.dir(), &shared.cancel)?;
        std::thread::scope(|scope| {
            let mut joins = Vec::new();
            for unit in &wave {
                let mut overlay = Overlay::create_cancellable(shared.root, &base, &shared.cancel)?;
                shared.emit(Event::new(
                    &unit.id,
                    Stage::AgentStarted,
                    &unit.goal,
                    json!({"write_files":unit.write_files,"depends_on":unit.depends_on,"base":base}),
                ));
                joins.push((
                    unit,
                    scope.spawn(move || {
                        worker::run(
                            shared,
                            &unit.id,
                            &unit.goal,
                            &mut overlay,
                            Some(unit.write_files.iter().cloned().collect()),
                            false,
                        )
                    }),
                ));
            }
            let mut published = BTreeSet::new();
            // Publish each accepted proposal while the other branches keep running.
            // Joining in plan order keeps merge outcomes reproducible.
            for (unit, join) in joins {
                let mut proposal = join
                    .join()
                    .map_err(|_| anyhow::anyhow!("child worker panicked"))??;
                ensure!(!shared.stopped(), "DAG cancelled before merge");
                // Dynamic reads absent from the approximate graph must not silently consume stale siblings.
                if !proposal.reads.is_disjoint(&published) {
                    shared.emit(Event::new(
                        &unit.id,
                        Stage::Planning,
                        "refresh stale branch",
                        json!({"changed":published}),
                    ));
                    let current = Revision::capture_cancellable(parent.dir(), &shared.cancel)?;
                    let mut overlay = Overlay::create_cancellable(shared.root, &current, &shared.cancel)?;
                    proposal = worker::run(
                        shared,
                        &unit.id,
                        &unit.goal,
                        &mut overlay,
                        Some(unit.write_files.iter().cloned().collect()),
                        false,
                    )?;
                }
                ensure!(
                    proposal.complete,
                    "agent {} unresolved: {}",
                    unit.id,
                    proposal.summary
                );
                let actual: BTreeSet<_> = proposal
                    .patch
                    .files
                    .iter()
                    .map(|f| f.path.clone())
                    .collect();
                ensure!(
                    actual.is_subset(&unit.write_files.iter().cloned().collect()),
                    "agent changed files outside lease"
                );
                proposal.patch.merge_into(parent)?;
                published.extend(actual);
                let revision = Revision::capture_cancellable(parent.dir(), &shared.cancel)?;
                shared.emit(Event::new(
                &unit.id,
                Stage::Merge,
                &unit.goal,
                json!({"revision":revision,"files":proposal.patch.files,"depends_on":unit.depends_on}),
            ));
                shared.emit(Event::new(
                    &unit.id,
                    Stage::AgentFinished,
                    &unit.goal,
                    json!({"summary":proposal.summary,"depends_on":unit.depends_on}),
                ));
                done.insert(unit.id.clone());
                receipts.insert(unit.id.clone(), proposal.summary);
            }
            Ok::<_, anyhow::Error>(())
        })?;
    }
    Ok(json!({"completed":receipts,"revision":Revision::capture_cancellable(parent.dir(), &shared.cancel)?}))
}
