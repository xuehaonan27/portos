//! Run states and the data exchanged by plan services.

use super::RunControl;
use crate::KernelError;
use portos_rm::identity::{SubjectId, VerbId};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

/// What a submission returns: the admitted run's id, the plan's CAS id, the
/// deterministic rendering (WYSIWYS), and the derived budget it renders.
pub struct SubmitOut {
    pub run_id: String,
    pub plan_hash: String,
    pub rendering: String,
    pub budget: BTreeMap<String, u64>,
}

/// Terminal outcome of a run (persisted as JSON).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub enum Outcome {
    Completed,
    FailStop { at: String },
    Truncated { dropped: usize },
    Aborted { expired: bool },
}

impl Outcome {
    pub fn status(&self) -> &'static str {
        match self {
            Outcome::Completed => "Completed",
            Outcome::FailStop { .. } => "FailStop",
            Outcome::Truncated { .. } => "Truncated",
            Outcome::Aborted { .. } => "Aborted",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RunState {
    Admitted,
    Running,
    AwaitingApproval,
    Paused,
    Done,
}

impl RunState {
    pub fn as_str(&self) -> &'static str {
        match self {
            RunState::Admitted => "admitted",
            RunState::Running => "running",
            RunState::AwaitingApproval => "awaiting_approval",
            RunState::Paused => "paused",
            RunState::Done => "done",
        }
    }
}

impl std::fmt::Display for RunState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for RunState {
    type Err = KernelError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "admitted" => Ok(Self::Admitted),
            "running" => Ok(Self::Running),
            "awaiting_approval" => Ok(Self::AwaitingApproval),
            "paused" => Ok(Self::Paused),
            "done" => Ok(Self::Done),
            _ => Err(KernelError::Corrupt(format!(
                "unknown plan run state: {value}"
            ))),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum BufferState {
    Held,
    Inserted,
    Aborted,
}

impl BufferState {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Held => "held",
            Self::Inserted => "inserted",
            Self::Aborted => "aborted",
        }
    }
}

pub(super) struct RunHandle {
    pub(super) plan_hash: String,
    pub(super) fiber: SubjectId,
    pub(super) nonce: String,
    pub(super) original_ttl_at: u64,
    pub(super) state: RunState,
    pub(super) ctrl: Arc<RunControl>,
    pub(super) thread: Option<std::thread::JoinHandle<()>>,
}

pub(super) struct RunRow {
    pub(super) plan_hash: String,
    pub(super) fiber: SubjectId,
    pub(super) nonce: String,
    pub(super) state: RunState,
}

#[derive(Clone)]
pub(super) struct Buffered {
    pub(super) seq: u64,
    pub(super) verb: VerbId,
    pub(super) target: String,
    pub(super) args: Value,
    pub(super) cost: u64,
}

/// A withheld effect shown for approval, without its private execution payload.
pub struct WithheldEffect {
    pub verb: VerbId,
    pub target: String,
    pub cost: u64,
}
