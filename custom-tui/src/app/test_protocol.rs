//! Historical JSON fixtures exercise the same adapter and reducer as live use.
use super::*;
use crate::{
    agent::{AgentEvent, AgentUpdate},
    backend::codec,
};
use serde_json::Value;

impl SessionSummary {
    pub fn from_thread(thread: &Value) -> Option<Self> {
        codec::summary(thread)
    }
}
impl App {
    pub fn track(&mut self, id: u64, method: &str) {
        let operation = match method {
            "thread/list/more" => Operation::MoreSessions,
            "turn/interrupt" => Operation::Interrupt,
            _ if method.starts_with("switch/") => Operation::SwitchSession,
            _ => Operation::OpenSession,
        };
        self.apply_event(AgentEvent::global(AgentUpdate::OperationStarted {
            id,
            operation,
        }));
    }
    pub fn read_permission_settings(&mut self, settings: &Value) {
        self.apply_event(AgentEvent::global(AgentUpdate::PermissionsUpdated {
            permissions: codec::permissions(settings, false),
            defaults_only: false,
        }));
    }
    pub fn read_config_settings(&mut self, settings: &Value) {
        self.apply_event(AgentEvent::global(AgentUpdate::PermissionsUpdated {
            permissions: codec::permissions(settings, true),
            defaults_only: true,
        }));
    }
    pub fn set_thread(&mut self, thread: &Value) {
        if let Some(snapshot) = codec::snapshot(thread) {
            self.apply_event(AgentEvent::global(AgentUpdate::SessionLoaded {
                snapshot,
                switching: false,
                model: None,
                permissions: Default::default(),
            }));
        }
    }
    pub fn upsert(&mut self, item: &Value) {
        for update in codec::item_events(item) {
            self.apply_event(AgentEvent::global(update));
        }
    }
    pub fn notification(&mut self, method: &str, params: &Value) {
        for event in codec::notification(method, params) {
            self.apply_event(event);
        }
    }
    pub fn server_request(&mut self, id: Value, method: &str, params: &Value) -> bool {
        if self.readonly || params["threadId"].as_str() != self.thread.as_deref() {
            return false;
        }
        let Some((approval, _)) = codec::approval(&id, method, params) else {
            return false;
        };
        self.apply_event(AgentEvent::global(AgentUpdate::ApprovalRequested(approval)));
        true
    }
    pub fn load_skills(&mut self, result: &Value) {
        let (skills, errors) = codec::skills(result);
        self.apply_event(AgentEvent::global(AgentUpdate::SkillsLoaded {
            skills,
            errors,
        }));
    }
    pub fn turn_input(&self, text: &str) -> Value {
        codec::turn_input(text, &self.submitted_skills)
    }
}
