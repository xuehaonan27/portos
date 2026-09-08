//! Storage DTOs stay here; the domain has no JSON dependency.
use super::*;
use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProcessDto {
    pid: u32,
    start: u64,
    boot: String,
}
impl From<&ProcessWitness> for ProcessDto {
    fn from(w: &ProcessWitness) -> Self {
        Self {
            pid: w.pid(),
            start: w.start_ticks(),
            boot: w.boot().to_string(),
        }
    }
}
impl ProcessDto {
    fn checked(self) -> Result<ProcessWitness, KernelError> {
        ProcessWitness::new(self.pid, self.start, self.boot.into()).map_err(map_err)
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HostDto {
    process: ProcessDto,
    session: String,
}
impl From<&HostWitness> for HostDto {
    fn from(w: &HostWitness) -> Self {
        Self {
            process: w.process().into(),
            session: w.session().to_string(),
        }
    }
}
impl HostDto {
    fn checked(self) -> Result<HostWitness, KernelError> {
        HostWitness::new(self.process.checked()?, self.session.into()).map_err(map_err)
    }
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum TargetDto {
    AccountingOnly,
    Process {
        process: ProcessDto,
    },
    FileLock {
        path: std::path::PathBuf,
        device: u64,
        inode: u64,
        owner: Option<ProcessDto>,
    },
    Port {
        transport: String,
        port: u16,
    },
    Subscription {
        host: HostDto,
        subscription: u64,
    },
    Plugin {
        host: HostDto,
        process: ProcessDto,
    },
    Provider,
    Unresolved {
        expected: String,
        reason: String,
    },
}
pub(super) fn kind_name(k: CleanupKind) -> &'static str {
    match k {
        CleanupKind::Process => "process",
        CleanupKind::FileLock => "file_lock",
        CleanupKind::Port => "port",
        CleanupKind::Subscription => "subscription",
        CleanupKind::Plugin => "plugin",
        CleanupKind::Provider => "provider",
    }
}
fn kind(s: &str) -> Result<CleanupKind, KernelError> {
    match s {
        "process" => Ok(CleanupKind::Process),
        "file_lock" => Ok(CleanupKind::FileLock),
        "port" => Ok(CleanupKind::Port),
        "subscription" => Ok(CleanupKind::Subscription),
        "plugin" => Ok(CleanupKind::Plugin),
        "provider" => Ok(CleanupKind::Provider),
        _ => Err(corrupt(format!("unknown cleanup kind {s}"))),
    }
}
pub(super) fn policy_name(p: CleanupPolicy) -> &'static str {
    match p {
        CleanupPolicy::AccountingOnly => "accounting_only",
        CleanupPolicy::Managed(k) => kind_name(k),
    }
}
pub(super) fn policy(s: &str) -> Result<CleanupPolicy, KernelError> {
    if s == "accounting_only" {
        Ok(CleanupPolicy::AccountingOnly)
    } else {
        kind(s).map(CleanupPolicy::Managed)
    }
}
pub(super) fn target_json(target: &CleanupTarget) -> Result<String, KernelError> {
    let dto = match target {
        CleanupTarget::AccountingOnly => TargetDto::AccountingOnly,
        CleanupTarget::Process(w) => TargetDto::Process { process: w.into() },
        CleanupTarget::FileLock(w) => TargetDto::FileLock {
            path: w.path().to_owned(),
            device: w.device(),
            inode: w.inode(),
            owner: w.owner().map(Into::into),
        },
        CleanupTarget::Port(w) => TargetDto::Port {
            transport: match w.transport() {
                Transport::Tcp => "tcp",
                Transport::Udp => "udp",
            }
            .into(),
            port: w.port(),
        },
        CleanupTarget::Subscription { host, subscription } => TargetDto::Subscription {
            host: host.into(),
            subscription: *subscription,
        },
        CleanupTarget::Plugin { host, process } => TargetDto::Plugin {
            host: host.into(),
            process: process.into(),
        },
        CleanupTarget::Provider => TargetDto::Provider,
        CleanupTarget::Unresolved { kind, reason } => TargetDto::Unresolved {
            expected: kind_name(*kind).into(),
            reason: reason.clone(),
        },
    };
    serde_json::to_string(&dto).map_err(|e| corrupt(e.to_string()))
}
pub(super) fn target(s: &str) -> Result<CleanupTarget, KernelError> {
    Ok(
        match serde_json::from_str::<TargetDto>(s)
            .map_err(|e| corrupt(format!("cleanup target: {e}")))?
        {
            TargetDto::AccountingOnly => CleanupTarget::AccountingOnly,
            TargetDto::Process { process } => CleanupTarget::Process(process.checked()?),
            TargetDto::FileLock {
                path,
                device,
                inode,
                owner,
            } => CleanupTarget::FileLock(
                FileLockWitness::new(
                    path,
                    device,
                    inode,
                    owner.map(ProcessDto::checked).transpose()?,
                )
                .map_err(map_err)?,
            ),
            TargetDto::Port { transport, port } => CleanupTarget::Port(
                PortWitness::new(
                    match transport.as_str() {
                        "tcp" => Transport::Tcp,
                        "udp" => Transport::Udp,
                        _ => return Err(corrupt("unknown port transport")),
                    },
                    port,
                )
                .map_err(map_err)?,
            ),
            TargetDto::Subscription { host, subscription } => CleanupTarget::Subscription {
                host: host.checked()?,
                subscription,
            },
            TargetDto::Plugin { host, process } => CleanupTarget::Plugin {
                host: host.checked()?,
                process: process.checked()?,
            },
            TargetDto::Provider => CleanupTarget::Provider,
            TargetDto::Unresolved { expected, reason } => CleanupTarget::Unresolved {
                kind: kind(&expected)?,
                reason,
            },
        },
    )
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StateDto {
    Pending,
    Running { worker: HostDto },
    Retryable { reason: String },
    Unknown { reason: String },
    Blocked { reason: String },
    Confirmed,
    AlreadyAbsent,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskDto {
    id: i64,
    key: String,
    holding: i64,
    generation: String,
    state: StateDto,
    attempts: u64,
    requested_at: i64,
    updated_at: i64,
}
pub(super) fn task_json(t: &CleanupRecord) -> Result<String, KernelError> {
    let state = match &t.state {
        CleanupState::Pending => StateDto::Pending,
        CleanupState::Running { worker } => StateDto::Running {
            worker: worker.into(),
        },
        CleanupState::Retryable(s) => StateDto::Retryable { reason: s.clone() },
        CleanupState::Unknown(s) => StateDto::Unknown { reason: s.clone() },
        CleanupState::Blocked(s) => StateDto::Blocked { reason: s.clone() },
        CleanupState::Done(Completion::Confirmed) => StateDto::Confirmed,
        CleanupState::Done(Completion::AlreadyAbsent) => StateDto::AlreadyAbsent,
    };
    serde_json::to_string(&TaskDto {
        id: t.id.to_sql(),
        key: t.key.as_str().into(),
        holding: t.holding.id().to_sql(),
        generation: t.holding.generation().to_string(),
        state,
        attempts: t.attempts,
        requested_at: t.requested_at.to_sql(),
        updated_at: t.updated_at.to_sql(),
    })
    .map_err(|e| corrupt(e.to_string()))
}
pub(super) fn task(s: &str) -> Result<CleanupRecord, KernelError> {
    let t: TaskDto = serde_json::from_str(s).map_err(|e| corrupt(format!("cleanup task: {e}")))?;
    Ok(CleanupRecord {
        id: CleanupId::try_from(t.id).map_err(map_err)?,
        key: CleanupKey::new(t.key).map_err(map_err)?,
        holding: HoldingHandle::new(
            HoldingId::try_from(t.holding).map_err(map_err)?,
            t.generation.into(),
        ),
        attempts: t.attempts,
        requested_at: Timestamp::try_from(t.requested_at).map_err(map_err)?,
        updated_at: Timestamp::try_from(t.updated_at).map_err(map_err)?,
        state: match t.state {
            StateDto::Pending => CleanupState::Pending,
            StateDto::Running { worker } => CleanupState::Running {
                worker: worker.checked()?,
            },
            StateDto::Retryable { reason } => CleanupState::Retryable(reason),
            StateDto::Unknown { reason } => CleanupState::Unknown(reason),
            StateDto::Blocked { reason } => CleanupState::Blocked(reason),
            StateDto::Confirmed => CleanupState::Done(Completion::Confirmed),
            StateDto::AlreadyAbsent => CleanupState::Done(Completion::AlreadyAbsent),
        },
    })
}
