//! One request allowance for a parent and every descendant using its model port.
use super::Budget;
use crate::models::{ErrorKind, ModelPort, Need, ProviderError, Request, Served, StreamEvent};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

pub(super) struct SharedModel<'a> {
    inner: &'a dyn ModelPort,
    limit: Budget,
    deadline: Option<Instant>,
    calls: AtomicUsize,
    tokens: AtomicU64,
}
impl<'a> SharedModel<'a> {
    pub(super) fn new(inner: &'a dyn ModelPort, limit: Budget) -> Self {
        Self {
            inner,
            limit,
            deadline: Instant::now().checked_add(Duration::from_secs(limit.seconds)),
            calls: AtomicUsize::new(0),
            tokens: AtomicU64::new(0),
        }
    }
    pub(super) fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
    fn request(&self, request: &Request) -> Result<Request, ProviderError> {
        let remaining = self
            .deadline
            .map_or(Duration::from_secs(self.limit.seconds), |deadline| {
                deadline.saturating_duration_since(Instant::now())
            });
        if remaining.is_zero() || self.tokens.load(Ordering::SeqCst) >= self.limit.tokens {
            return Err(ProviderError::new(
                ErrorKind::BadRequest,
                "shared parent/agent time or token limit reached",
            ));
        }
        self.calls
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |calls| {
                (calls < self.limit.steps).then_some(calls + 1)
            })
            .map_err(|_| {
                ProviderError::new(
                    ErrorKind::BadRequest,
                    "shared parent/agent model-call limit reached",
                )
            })?;
        let mut request = request.clone();
        request.timeout = Some(
            request
                .timeout
                .map_or(remaining, |timeout| timeout.min(remaining)),
        );
        Ok(request)
    }
    fn served(&self, result: Result<Served, ProviderError>) -> Result<Served, ProviderError> {
        if let Ok(served) = &result {
            self.tokens.fetch_add(
                served
                    .response
                    .usage
                    .input
                    .unwrap_or(0)
                    .saturating_add(served.response.usage.output.unwrap_or(0)),
                Ordering::SeqCst,
            );
        }
        result
    }
}
impl ModelPort for SharedModel<'_> {
    fn model_profiles(&self) -> Vec<crate::models::ModelProfile> {
        self.inner.model_profiles()
    }
    fn limits(&self) -> Vec<crate::models::ModelLimits> {
        self.inner.limits()
    }
    fn records(&self) -> Vec<crate::models::router::CallRecord> {
        self.inner.records()
    }
    fn complete(&self, need: Need, request: &Request) -> Result<Served, ProviderError> {
        let request = self.request(request)?;
        self.served(self.inner.complete(need, &request))
    }
    fn complete_stream(
        &self,
        need: Need,
        request: &Request,
        observe: &mut dyn FnMut(StreamEvent),
    ) -> Result<Served, ProviderError> {
        let request = self.request(request)?;
        self.served(self.inner.complete_stream(need, &request, observe))
    }
}
