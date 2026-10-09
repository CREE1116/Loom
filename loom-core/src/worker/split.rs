//! Core-owned dispatch. No model calls are spent polling or merging workers.
use super::{
    Budget, WorkMemory, WorkerResult, WorkerStatus, WorkerTask, agents::Agents, budget::SharedModel,
};
use crate::{
    base::{ArtifactStore, Cost, Phase},
    models::ModelPort,
    world::{World, split::Group},
};
use anyhow::{Result, ensure};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicBool, Ordering},
    time::{Duration, Instant},
};

pub fn run_split(
    task: &mut WorkerTask,
    groups: &[Group],
    model: &dyn ModelPort,
    cancel: &AtomicBool,
    mut observe: impl FnMut(Value),
) -> Result<WorkerResult> {
    ensure!(
        (2..=4).contains(&groups.len()),
        "automatic dispatch requires 2–4 independent groups"
    );
    let started = Instant::now();
    let world = World::open(task.overlay.dir())?;
    let shared = SharedModel::new(model, task.budget);
    let store = crate::base::FileArtifactStore::open(task.overlay.workspace())?;
    let mut trace = Vec::new();
    std::thread::scope(|scope| {
        let mut record = |event: Value| {
            trace.extend_from_slice(event.to_string().as_bytes());
            trace.push(b'\n');
            observe(event);
        };
        record(
            json!({"progress":{"id":"graph-dispatch","phase":"dispatch","message":format!("그래프에서 독립된 작업 {}개를 나눠 실행합니다. 각 작업의 수정 범위를 제한하고, 결과를 병합한 뒤 Core가 검증합니다.",groups.len())}}),
        );
        let mut agents = Agents::new(scope, &shared, task, cancel, started);
        agents.automatic(Budget {
            steps: task.budget.steps / groups.len(),
            tokens: task.budget.tokens / groups.len() as u64,
            seconds: task.budget.seconds,
        });
        let mut ids = vec![];
        let mut failure = None;
        for group in groups {
            match agents.spawn_group(group, &world, &task.goal) {
                Ok(id) => ids.push(id),
                Err(error) => {
                    failure = Some(error.to_string());
                    break;
                }
            }
        }
        for event in agents.events() {
            record(event);
        }
        while failure.is_none() && agents.results().len() < ids.len() {
            if cancel.load(Ordering::SeqCst)
                || started.elapsed() >= Duration::from_secs(task.budget.seconds)
            {
                failure = Some(
                    if cancel.load(Ordering::SeqCst) {
                        "cancelled"
                    } else {
                        "agent execution window ended"
                    }
                    .into(),
                );
                break;
            }
            for id in &ids {
                let report = agents.call(
                    "wait_agent",
                    &json!({"id":id,"timeout_ms":20}),
                    &mut task.overlay,
                );
                match report {
                    Err(error) => {
                        failure = Some(error.to_string());
                        break;
                    }
                    Ok(report) => {
                        let report: Value = serde_json::from_str(&report)?;
                        if report["running"] == false && report["status"].is_null() {
                            failure = Some(format!("graph worker {id} ended without a result"));
                            break;
                        }
                    }
                }
                for event in agents.events() {
                    record(event);
                }
            }
        }
        if failure.is_none()
            && agents
                .results()
                .iter()
                .any(|result| result.status != WorkerStatus::Proposed)
        {
            failure =
                Some("one or more graph workers could not complete their scoped proposal".into());
        }
        if failure.is_none() {
            for id in &ids {
                if let Err(error) = agents.call("merge_agent", &json!({"id":id}), &mut task.overlay)
                {
                    failure = Some(error.to_string());
                    break;
                }
                for event in agents.events() {
                    record(event);
                }
            }
        }
        if let Err(error) = agents.finish() {
            failure.get_or_insert_with(|| error.to_string());
        }
        for event in agents.events() {
            record(event);
        }
        if cancel.load(Ordering::SeqCst) {
            failure = Some("cancelled".into());
        }
        let children = agents.results();
        let mut cost_by_phase: BTreeMap<Phase, Cost> = BTreeMap::new();
        let mut phase_bytes = BTreeMap::new();
        let mut models = BTreeMap::new();
        for child in &children {
            for (phase, cost) in &child.cost_by_phase {
                cost_by_phase.entry(*phase).or_default().accumulate(cost);
            }
            for (phase, bytes) in &child.phase_bytes {
                *phase_bytes.entry(phase.clone()).or_insert(0u64) += bytes;
            }
            for (model, count) in &child.models {
                *models.entry(model.clone()).or_insert(0u32) += count;
            }
        }
        let summary = failure.clone().unwrap_or_else(|| {
            children
                .iter()
                .map(|child| child.summary.as_str())
                .collect::<Vec<_>>()
                .join("\n")
        });
        let reports = children
            .iter()
            .enumerate()
            .map(|(i, child)| {
                format!(
                    "작업 {}: {}\n{}",
                    i + 1,
                    child.summary,
                    child.activity_report
                )
            })
            .collect::<Vec<_>>()
            .join("\n");
        let patch = task.overlay.patch()?;
        let touched = patch.files.iter().map(|file| file.path.clone()).collect();
        Ok(WorkerResult {
            status: if failure.is_none() {
                WorkerStatus::Proposed
            } else {
                WorkerStatus::Unresolved
            },
            finished: failure.is_none() && children.iter().all(|child| child.finished),
            answered: failure.is_none()
                && patch.files.is_empty()
                && children.iter().all(|child| child.answered),
            patch,
            touched,
            summary,
            plan: task.plan.clone(),
            rereads: children.iter().map(|child| child.rereads).sum(),
            cost_by_phase,
            trace: store.put(&trace)?,
            artifacts: agents.artifacts(),
            work_memory: WorkMemory::default(),
            activity_report: format!(
                "그래프 분배: {}개 · 모델 요청 {}회\n{reports}",
                groups.len(),
                shared.calls()
            ),
            last_model_error: children
                .iter()
                .find_map(|child| child.last_model_error.clone()),
            steps: shared.calls(),
            input_tokens: children.iter().map(|child| child.input_tokens).sum(),
            remote_input_tokens: children.iter().try_fold(0u64, |sum, child| {
                child
                    .remote_input_tokens
                    .map(|tokens| sum.saturating_add(tokens))
            }),
            remote_calls: children.iter().map(|child| child.remote_calls).sum(),
            model_wait_ms: children.iter().map(|child| child.model_wait_ms).sum(),
            gateway_overhead_ms: children.iter().map(|child| child.gateway_overhead_ms).sum(),
            cached_input_tokens: children.iter().map(|child| child.cached_input_tokens).sum(),
            output_tokens: children.iter().map(|child| child.output_tokens).sum(),
            phase_bytes,
            models,
            seconds: started.elapsed().as_secs(),
        })
    })
}
