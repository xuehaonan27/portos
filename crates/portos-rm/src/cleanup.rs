//! Durable retirement state and exact cleanup targets. No OS actions live here.
use crate::identity::{Generation, HoldingHandle, HoldingId, ResourceKey};
use crate::ledger::{LedgerError, RevertGrade};
use crate::time::Timestamp;
use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CleanupId(HoldingId);
impl CleanupId {
    pub(crate) fn for_holding(id: HoldingId) -> Self {
        Self(id)
    }
    pub fn get(self) -> u64 {
        self.0.get()
    }
    pub fn to_sql(self) -> i64 {
        self.0.to_sql()
    }
}
impl TryFrom<i64> for CleanupId {
    type Error = LedgerError;
    fn try_from(n: i64) -> Result<Self, Self::Error> {
        HoldingId::try_from(n).map(Self)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct CleanupKey(String);
impl CleanupKey {
    pub fn new(key: String) -> Result<Self, LedgerError> {
        if key.is_empty() {
            return Err(LedgerError::InvalidValue);
        }
        Ok(Self(key))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CleanupKind {
    Process,
    FileLock,
    Port,
    Subscription,
    Plugin,
    Provider,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CleanupPolicy {
    AccountingOnly,
    Managed(CleanupKind),
}

/// An exact Linux process incarnation; zero is never a wildcard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcessWitness {
    pid: u32,
    start_ticks: u64,
    boot: Generation,
}
impl ProcessWitness {
    pub fn new(pid: u32, start_ticks: u64, boot: Generation) -> Result<Self, LedgerError> {
        if pid == 0 || pid > i32::MAX as u32 || start_ticks == 0 || boot.as_str().is_empty() {
            return Err(LedgerError::InvalidValue);
        }
        Ok(Self {
            pid,
            start_ticks,
            boot,
        })
    }
    pub fn pid(&self) -> u32 {
        self.pid
    }
    pub fn start_ticks(&self) -> u64 {
        self.start_ticks
    }
    pub fn boot(&self) -> &Generation {
        &self.boot
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FileLockWitness {
    path: PathBuf,
    device: u64,
    inode: u64,
    owner: Option<ProcessWitness>,
}
impl FileLockWitness {
    pub fn new(
        path: PathBuf,
        device: u64,
        inode: u64,
        owner: Option<ProcessWitness>,
    ) -> Result<Self, LedgerError> {
        if !path.is_absolute() {
            return Err(LedgerError::InvalidValue);
        }
        Ok(Self {
            path,
            device,
            inode,
            owner,
        })
    }
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
    pub fn device(&self) -> u64 {
        self.device
    }
    pub fn inode(&self) -> u64 {
        self.inode
    }
    pub fn owner(&self) -> Option<&ProcessWitness> {
        self.owner.as_ref()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Transport {
    Tcp,
    Udp,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PortWitness {
    transport: Transport,
    port: u16,
}
impl PortWitness {
    pub fn new(transport: Transport, port: u16) -> Result<Self, LedgerError> {
        if port == 0 {
            return Err(LedgerError::InvalidValue);
        }
        Ok(Self { transport, port })
    }
    pub fn transport(self) -> Transport {
        self.transport
    }
    pub fn port(self) -> u16 {
        self.port
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostWitness {
    process: ProcessWitness,
    session: Generation,
}
impl HostWitness {
    pub fn new(process: ProcessWitness, session: Generation) -> Result<Self, LedgerError> {
        if session.as_str().is_empty() {
            return Err(LedgerError::InvalidGeneration);
        }
        Ok(Self { process, session })
    }
    pub fn process(&self) -> &ProcessWitness {
        &self.process
    }
    pub fn session(&self) -> &Generation {
        &self.session
    }
}

/// Targets are fixed at registration. Unresolved is recovery input only: it
/// preserves missing legacy evidence and can never admit a new holding.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CleanupTarget {
    AccountingOnly,
    Process(ProcessWitness),
    FileLock(FileLockWitness),
    Port(PortWitness),
    Subscription {
        host: HostWitness,
        subscription: u64,
    },
    Plugin {
        host: HostWitness,
        process: ProcessWitness,
    },
    Provider,
    Unresolved {
        kind: CleanupKind,
        reason: String,
    },
}
impl CleanupTarget {
    /// Built-in exclusive resources cannot acquire a second ledger identity by
    /// choosing another display name. Provider-defined aliasing belongs to its
    /// registration contract.
    pub(crate) fn conflicts_with(&self, other: &Self) -> bool {
        let process = |t: &Self| match t {
            Self::Process(w) | Self::Plugin { process: w, .. } => Some(w.clone()),
            _ => None,
        };
        if let (Some(a), Some(b)) = (process(self), process(other)) {
            return a == b;
        }
        match (self, other) {
            (Self::FileLock(a), Self::FileLock(b)) => {
                a.path() == b.path() || (a.device() == b.device() && a.inode() == b.inode())
            }
            (Self::Port(a), Self::Port(b)) => a == b,
            (
                Self::Subscription {
                    host: a,
                    subscription: x,
                },
                Self::Subscription {
                    host: b,
                    subscription: y,
                },
            ) => a == b && x == y,
            _ => false,
        }
    }
    pub fn policy(&self) -> CleanupPolicy {
        use CleanupKind as K;
        CleanupPolicy::Managed(match self {
            Self::AccountingOnly => return CleanupPolicy::AccountingOnly,
            Self::Process(_) => K::Process,
            Self::FileLock(_) => K::FileLock,
            Self::Port(_) => K::Port,
            Self::Subscription { .. } => K::Subscription,
            Self::Plugin { .. } => K::Plugin,
            Self::Provider => K::Provider,
            Self::Unresolved { kind, .. } => *kind,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldingState {
    Active,
    Retiring(CleanupId),
    Retired(Timestamp),
}
impl HoldingState {
    pub fn is_active(self) -> bool {
        self == Self::Active
    }
    pub fn occupies(self) -> bool {
        !matches!(self, Self::Retired(_))
    }
    pub fn released_at(self) -> Option<Timestamp> {
        match self {
            Self::Retired(t) => Some(t),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Completion {
    Confirmed,
    AlreadyAbsent,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CleanupOutcome {
    Confirmed,
    AlreadyAbsent,
    Retryable(String),
    Unknown(String),
    Blocked(String),
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CleanupState {
    Pending,
    Running { worker: HostWitness },
    Retryable(String),
    Unknown(String),
    Blocked(String),
    Done(Completion),
}
impl CleanupState {
    pub fn is_done(&self) -> bool {
        matches!(self, Self::Done(_))
    }
}
/// Raw recovery DTO, validated along with holdings by LedgerBuilder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanupRecord {
    pub id: CleanupId,
    pub key: CleanupKey,
    pub holding: HoldingHandle,
    pub state: CleanupState,
    pub attempts: u64,
    pub requested_at: Timestamp,
    pub updated_at: Timestamp,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanupTask {
    pub(crate) record: CleanupRecord,
}
impl std::ops::Deref for CleanupTask {
    type Target = CleanupRecord;
    fn deref(&self) -> &Self::Target {
        &self.record
    }
}
/// A claimed attempt. Completion must match its task, incarnation and attempt.
#[derive(Clone, Debug)]
pub struct CleanupWork {
    pub(crate) task: CleanupRecord,
    pub(crate) target: CleanupTarget,
    pub(crate) resource: ResourceKey,
    pub(crate) grade: RevertGrade,
}
impl CleanupWork {
    pub fn task(&self) -> &CleanupRecord {
        &self.task
    }
    pub fn target(&self) -> &CleanupTarget {
        &self.target
    }
    pub fn resource(&self) -> &ResourceKey {
        &self.resource
    }
    pub fn grade(&self) -> RevertGrade {
        self.grade
    }
}
/// Implementations must be safe to repeat for the same target and key after an
/// interrupted attempt. Unknown/failed results never discharge occupancy.
pub trait CleanupExecutor {
    fn execute(&mut self, work: &CleanupWork) -> CleanupOutcome;
}

/// ```compile_fail
/// use portos_rm::cleanup::{CleanupTask, CleanupState};
/// fn forge(mut task: CleanupTask) { task.state = CleanupState::Pending; }
/// ```
/// ```compile_fail
/// use portos_rm::cleanup::CleanupWork;
/// fn forge(work: &mut CleanupWork) { work.task.attempts = 0; }
/// ```
const _: () = ();
