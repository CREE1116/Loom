use serde_json::Value;
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
};
#[derive(Clone, Debug)]
pub struct AgentDirective {
    pub id: String,
    pub text: Option<String>,
}
#[derive(Clone, Debug, Default)]
pub struct WorkerControl {
    agents: Arc<Mutex<VecDeque<AgentDirective>>>,
    answers: Arc<Mutex<BTreeMap<String, Value>>>,
}
impl WorkerControl {
    pub(crate) fn child(&self) -> Self {
        Self {
            agents: Default::default(),
            answers: self.answers.clone(),
        }
    }
    pub fn direct_agent(&self, id: String, text: Option<String>) {
        self.agents
            .lock()
            .unwrap()
            .push_back(AgentDirective { id, text });
    }
    pub(crate) fn directives(&self) -> Vec<AgentDirective> {
        self.agents.lock().unwrap().drain(..).collect()
    }
    pub fn answer(&self, id: String, answer: Value) {
        self.answers.lock().unwrap().insert(id, answer);
    }
    pub(crate) fn take_answer(&self, id: &str) -> Option<Value> {
        self.answers.lock().unwrap().remove(id)
    }
}
