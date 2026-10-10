//! Deterministic model for worker/control tests; no credentials, HTTP, or fallback.
use super::*;
use std::{collections::VecDeque, sync::Mutex};

pub struct ScriptedModel {
    candidate: router::Candidate,
    replies: Mutex<VecDeque<Result<Response, ProviderError>>>,
    requests: Mutex<Vec<Request>>,
}

impl ScriptedModel {
    pub fn new(replies: impl IntoIterator<Item = Response>) -> Self {
        Self::with_candidate(
            router::Candidate {
                endpoint: "scripted".into(),
                model: "scripted".into(),
                tier: Tier::Local,
                cost: CostClass::Local,
                price: Some((0.0, 0.0)),
                tools: true,
                context: None,
            },
            replies.into_iter().map(Ok),
        )
    }
    pub fn with_candidate(
        candidate: router::Candidate,
        replies: impl IntoIterator<Item = Result<Response, ProviderError>>,
    ) -> Self {
        Self {
            candidate,
            replies: Mutex::new(replies.into_iter().collect()),
            requests: Mutex::new(Vec::new()),
        }
    }
    /// A scripted model under `name` that reports a context window of `tokens`.
    pub fn with_context(name: &str, tokens: u64, replies: impl IntoIterator<Item = Response>) -> Self {
        let mut model = Self::new(replies);
        model.candidate.model = name.into();
        model.candidate.context = Some(tokens);
        model
    }
    pub fn requests(&self) -> Vec<Request> {
        self.requests.lock().unwrap().clone()
    }
    pub fn remaining(&self) -> usize {
        self.replies.lock().unwrap().len()
    }
}

impl ModelPort for ScriptedModel {
    fn complete(&self, need: Need, request: &Request) -> Result<Served, ProviderError> {
        if self.candidate.cost > need.max_cost.min(cost_ceiling())
            || self.candidate.tier < need.min_tier
            || (need.tools && !self.candidate.tools)
        {
            return Err(ProviderError::new(
                ErrorKind::BadRequest,
                "scripted candidate exceeds worker constraints",
            ));
        }
        self.requests.lock().unwrap().push(request.clone());
        let response = self.replies.lock().unwrap().pop_front().ok_or_else(|| {
            ProviderError::new(ErrorKind::BadRequest, "scripted model exhausted")
        })??;
        if response.route.as_deref().is_some_and(blocked_route) {
            return Err(ProviderError::new(
                ErrorKind::BadRequest,
                "blocked subscription OAuth route",
            ));
        }
        Ok(Served {
            response,
            candidate: self.candidate.clone(),
            skipped: Vec::new(),
        })
    }

    fn limits(&self) -> Vec<super::ModelLimits> {
        vec![super::ModelLimits {
            endpoint: self.candidate.endpoint.clone(),
            model: self.candidate.model.clone(),
            context: self.candidate.context,
            tools: self.candidate.tools,
            disabled: None,
            cooldown_until: None,
        }]
    }
}
