//! L2: isolated file effects and bounded process execution, independent of models.
mod editing;
mod overlay;
mod patch;
mod runner;

pub use editing::find_matches;
pub use overlay::Overlay;
pub use patch::{FilePatch, Patch};
pub use runner::{RunStatus, Runner, TestResult, classify_command};

use crate::base::{ArtifactRef, ArtifactStore, Cost, FileArtifactStore, Phase};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Output {
    pub phase: Phase,
    pub summary: String,
    pub structured: Value,
    pub artifact: ArtifactRef,
    pub cost: Cost,
}

impl Output {
    pub(crate) fn record(
        store: &FileArtifactStore,
        phase: Phase,
        summary: String,
        structured: Value,
        raw: &[u8],
    ) -> Result<Self> {
        Ok(Self {
            phase,
            summary,
            structured,
            artifact: store.put(raw)?,
            cost: Cost {
                usd: Some(0.0),
                ..Cost::default()
            },
        })
    }
}

pub mod sandbox;
pub mod terminal;
pub use sandbox::{ExecutionPolicy, SandboxMode, denied_by_sandbox, loopback_allowed};
pub use terminal::{TerminalManager,TerminalOutput};
