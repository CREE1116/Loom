//! Native LOCAL execution around a transitional remote worker backend.
use super::repository::RepositoryExplorer;
use crate::agent::*;
use anyhow::{Result, bail};
use std::{
    collections::{HashMap, VecDeque},
    path::Path,
    sync::{Arc, mpsc},
};

pub struct CoreBackend {
    remote: Box<dyn AgentBackend>,
    repository: Arc<RepositoryExplorer>,
    events: VecDeque<AgentEvent>,
    results_tx: mpsc::Sender<(String, Result<String, String>)>,
    results_rx: mpsc::Receiver<(String, Result<String, String>)>,
    tasks: HashMap<String, Option<String>>,
    session: Option<String>,
    may_read_workspace: bool,
    serial: u64,
}

impl CoreBackend {
    pub fn new(
        remote: Box<dyn AgentBackend>,
        root: &Path,
        may_read_workspace: bool,
    ) -> Result<Self> {
        let (results_tx, results_rx) = mpsc::channel();
        Ok(Self {
            remote,
            repository: Arc::new(RepositoryExplorer::new(root)?),
            events: VecDeque::new(),
            results_tx,
            results_rx,
            tasks: HashMap::new(),
            session: None,
            may_read_workspace,
            serial: 0,
        })
    }

    /// Every future worker receives a clone of this same handle.
    pub fn repository(&self) -> Arc<RepositoryExplorer> {
        Arc::clone(&self.repository)
    }

    fn emit(&mut self, session_id: Option<String>, update: AgentUpdate) {
        self.events.push_back(AgentEvent { session_id, update });
    }
}

impl AgentBackend for CoreBackend {
    fn command(&mut self, command: AgentCommand) -> Result<()> {
        let AgentCommand::Explore(query) = command else {
            return self.remote.command(command);
        };
        if !self.may_read_workspace {
            bail!("폴더 실행 모드를 선택한 뒤 탐색하세요.");
        }
        if self.tasks.len() >= 4 {
            bail!("LOCAL 탐색 네 개가 실행 중입니다. 완료 후 다시 요청하세요.");
        }
        self.serial += 1;
        let id = format!("local-explore-{}", self.serial);
        let scope = self.session.clone();
        self.tasks.insert(id.clone(), scope.clone());
        self.emit(scope.clone(), AgentUpdate::TaskStarted { id: id.clone() });
        self.emit(
            scope,
            AgentUpdate::EntryUpdated(Entry {
                id: id.clone(),
                kind: Kind::Tool,
                title: format!("LOCAL · 코드 탐색: {query}"),
                body: String::new(),
                status: "inProgress".into(),
                expanded: false,
            }),
        );
        let repository = Arc::clone(&self.repository);
        let tx = self.results_tx.clone();
        std::thread::spawn(move || {
            let result = repository
                .explore(&query, 4096)
                .map(|r| {
                    let (builds, searches) = repository.counts();
                    format!(
                        "{}\nAPI 호출 0 · index builds {builds} · unique searches {searches}",
                        r.compact()
                    )
                })
                .map_err(|error| error.to_string());
            let _ = tx.send((id, result));
        });
        Ok(())
    }

    fn poll(&mut self) -> Result<Option<AgentEvent>> {
        if let Some(event) = self.events.pop_front() {
            return Ok(Some(event));
        }
        if let Ok((id, result)) = self.results_rx.try_recv() {
            if let Some(scope) = self.tasks.remove(&id) {
                if scope != self.session {
                    self.emit(scope, AgentUpdate::TaskCompleted { id });
                    return Ok(self.events.pop_front());
                }
                let (body, status) = match result {
                    Ok(body) => (body, "completed"),
                    Err(error) => (error, "failed"),
                };
                self.emit(
                    scope.clone(),
                    AgentUpdate::EntryUpdated(Entry {
                        id: id.clone(),
                        kind: Kind::Tool,
                        title: "LOCAL · 공유 코드 탐색".into(),
                        body,
                        status: status.into(),
                        expanded: true,
                    }),
                );
                self.emit(scope, AgentUpdate::TaskCompleted { id });
            }
            return Ok(self.events.pop_front());
        }
        let event = self.remote.poll()?;
        if let Some(event) = &event {
            match &event.update {
                AgentUpdate::SessionLoaded { snapshot, .. } => {
                    self.session = Some(snapshot.id.clone())
                }
                AgentUpdate::TrustResolved(_) => self.may_read_workspace = true,
                AgentUpdate::TrustRequired(_) => self.may_read_workspace = false,
                _ => {}
            }
        }
        Ok(event)
    }
}
