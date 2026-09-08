//! Built-in target capture and cleanup. A failed observation is never absence.
use super::*;
use std::os::unix::fs::MetadataExt;

fn boot() -> std::io::Result<String> {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id").map(|s| s.trim().to_owned())
}
fn process_stat(pid: u32) -> std::io::Result<(u64, bool)> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let fields = stat
        .rsplit_once(')')
        .ok_or_else(|| std::io::Error::other("malformed process stat"))?
        .1
        .split_whitespace()
        .collect::<Vec<_>>();
    let start = fields
        .get(19)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| std::io::Error::other("missing process start time"))?;
    Ok((start, matches!(fields.first(), Some(&"Z" | &"X"))))
}
pub(crate) fn proc_start_time(pid: u32) -> Option<u64> {
    process_stat(pid).ok().map(|s| s.0)
}
pub(crate) fn capture_process(pid: u32) -> Result<ProcessWitness, KernelError> {
    let (start, _) = process_stat(pid)?;
    let witness = ProcessWitness::new(pid, start, boot()?.into()).map_err(map_err)?;
    if !process_present(&witness)? {
        return Err(KernelError::Denied("process already exited".into()));
    }
    Ok(witness)
}

// A zombie group leader may still have running threads. Only pidfd readiness
// establishes that the whole thread group exited; /proc's state letter cannot.
fn pin_process(w: &ProcessWitness) -> std::io::Result<Option<std::os::fd::OwnedFd>> {
    use nix::libc;
    use std::os::fd::FromRawFd;
    if boot()? != w.boot().as_str() {
        return Ok(None);
    }
    // SAFETY: pid was checked on construction; pidfd_open takes no pointers.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, w.pid() as libc::pid_t, 0u32) };
    if fd < 0 {
        let e = std::io::Error::last_os_error();
        return if e.raw_os_error() == Some(libc::ESRCH) {
            Ok(None)
        } else {
            Err(e)
        };
    }
    // SAFETY: a successful pidfd_open returns a newly owned descriptor.
    let fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd as i32) };
    match process_stat(w.pid()) {
        Ok((start, _)) if start != w.start_ticks() => return Ok(None),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e),
        _ => {}
    }
    if exited(&fd, 0)? {
        Ok(None)
    } else {
        Ok(Some(fd))
    }
}
fn exited(fd: &std::os::fd::OwnedFd, timeout: i32) -> std::io::Result<bool> {
    use nix::libc;
    use std::os::fd::AsRawFd;
    let mut p = libc::pollfd {
        fd: fd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: p points to one initialized pollfd, kept live by OwnedFd.
    let ready = unsafe { libc::poll(&mut p, 1, timeout) };
    if ready < 0 {
        return Err(std::io::Error::last_os_error());
    }
    if p.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
        return Err(std::io::Error::other("invalid pidfd poll result"));
    }
    Ok(ready > 0 && p.revents & (libc::POLLIN | libc::POLLHUP) != 0)
}
pub(super) fn process_present(w: &ProcessWitness) -> std::io::Result<bool> {
    pin_process(w).map(|p| p.is_some())
}
#[cfg(test)]
pub(super) fn proc_alive(pid: u32, start: u64) -> bool {
    boot()
        .ok()
        .and_then(|b| ProcessWitness::new(pid, start, b.into()).ok())
        .is_some_and(|w| process_present(&w).unwrap_or(false))
}

/// Pin, recheck the incarnation, then signal through that same descriptor.
/// Successful signal delivery is not sufficient to discharge occupancy.
pub(crate) fn cleanup_process(w: &ProcessWitness) -> CleanupOutcome {
    use nix::libc;
    use std::os::fd::AsRawFd;
    let fd = match pin_process(w) {
        Ok(Some(fd)) => fd,
        Ok(None) => return CleanupOutcome::AlreadyAbsent,
        Err(e) => return CleanupOutcome::Unknown(format!("process observation: {e}")),
    };
    // SAFETY: descriptor is live; null siginfo requests an ordinary signal.
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd.as_raw_fd(),
            libc::SIGKILL,
            std::ptr::null::<libc::siginfo_t>(),
            0u32,
        )
    };
    if result < 0 {
        let e = std::io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::ESRCH) && exited(&fd, 0).unwrap_or(false) {
            return CleanupOutcome::AlreadyAbsent;
        }
        return CleanupOutcome::Retryable(format!("pidfd_send_signal: {e}"));
    }
    match exited(&fd, 100) {
        Ok(true) => CleanupOutcome::Confirmed,
        Ok(false) => {
            CleanupOutcome::Retryable("termination requested; exit not yet confirmed".into())
        }
        Err(e) => CleanupOutcome::Unknown(format!("exit observation: {e}")),
    }
}

pub(super) fn capture(resource: &ResourceKey, value: &Value) -> Result<CleanupTarget, KernelError> {
    if !value.is_object() {
        return Err(KernelError::Denied("substrate must be an object".into()));
    }
    match resource.class().as_str() {
        CLASS_PROCESS => {
            let pid = value["pid"]
                .as_u64()
                .and_then(|n| u32::try_from(n).ok())
                .filter(|p| *p > 0 && *p <= i32::MAX as u32)
                .ok_or_else(|| KernelError::Denied("invalid process pid".into()))?;
            let w = capture_process(pid)?;
            if value.get("start").is_some() && value["start"].as_u64() != Some(w.start_ticks()) {
                return Err(KernelError::Denied(
                    "process incarnation changed during registration".into(),
                ));
            }
            Ok(CleanupTarget::Process(w))
        }
        CLASS_FILE_LOCK => {
            let path = std::path::Path::new(resource.instance().as_str());
            let path = if path.is_absolute() {
                path.to_owned()
            } else {
                std::env::current_dir()?.join(path)
            };
            let metadata = std::fs::symlink_metadata(&path)?;
            if !metadata.is_file() {
                return Err(KernelError::Denied(
                    "lock must be a regular file, not a symlink".into(),
                ));
            }
            let owner = value
                .get("owner_pid")
                .map(|raw| {
                    let pid = raw
                        .as_u64()
                        .and_then(|p| u32::try_from(p).ok())
                        .ok_or_else(|| KernelError::Denied("invalid lock owner".into()))?;
                    let w = capture_process(pid)?;
                    if value.get("owner_start").is_some()
                        && value["owner_start"].as_u64() != Some(w.start_ticks())
                    {
                        return Err(KernelError::Denied("lock owner incarnation changed".into()));
                    }
                    Ok::<_, KernelError>(w)
                })
                .transpose()?;
            Ok(CleanupTarget::FileLock(
                FileLockWitness::new(path, metadata.dev(), metadata.ino(), owner)
                    .map_err(map_err)?,
            ))
        }
        CLASS_PORT => {
            let (transport, port) =
                resource
                    .instance()
                    .as_str()
                    .split_once(':')
                    .ok_or_else(|| {
                        KernelError::Denied("port instance must be tcp:<port> or udp:<port>".into())
                    })?;
            let transport = match transport {
                "tcp" => Transport::Tcp,
                "udp" => Transport::Udp,
                _ => return Err(KernelError::Denied("unsupported port transport".into())),
            };
            let port = port
                .parse::<u16>()
                .map_err(|_| KernelError::Denied("invalid port number".into()))?;
            Ok(CleanupTarget::Port(
                PortWitness::new(transport, port).map_err(map_err)?,
            ))
        }
        _ => Err(KernelError::Denied(
            "class has no substrate registration adapter".into(),
        )),
    }
}

fn cleanup_lock(w: &FileLockWitness) -> CleanupOutcome {
    let metadata = match std::fs::symlink_metadata(w.path()) {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return CleanupOutcome::AlreadyAbsent,
        Err(e) => return CleanupOutcome::Unknown(e.to_string()),
    };
    if metadata.dev() != w.device() || metadata.ino() != w.inode() || !metadata.is_file() {
        return CleanupOutcome::Blocked(
            "lock path now denotes a different file; replacement was left untouched".into(),
        );
    }
    // Retirement revokes this exact lock marker even if its owner is still
    // alive. Waiting for that owner would deadlock a process waiting for its
    // child lock to retire. Legacy locks without inode evidence stay blocked.
    // Lock files are managed in their owner's namespace. Unix has no conditional
    // unlink-by-inode operation: foreign concurrent path replacement is outside
    // that contract. The identity check rejects an already replaced path.
    match std::fs::remove_file(w.path()) {
        Ok(()) => CleanupOutcome::Confirmed,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => CleanupOutcome::AlreadyAbsent,
        Err(e) => CleanupOutcome::Retryable(e.to_string()),
    }
}
pub(crate) fn execute_target(work: &CleanupWork) -> CleanupOutcome {
    if let Some(result) = crate::host::cleanup_known_host(work) {
        return result;
    }
    match work.target() {
        CleanupTarget::AccountingOnly => CleanupOutcome::Confirmed,
        CleanupTarget::Process(w) => cleanup_process(w),
        CleanupTarget::FileLock(w) => cleanup_lock(w),
        CleanupTarget::Port(w) => {
            let result = match w.transport() {
                Transport::Tcp => std::net::TcpListener::bind(("0.0.0.0", w.port())).map(|_| ()),
                Transport::Udp => std::net::UdpSocket::bind(("0.0.0.0", w.port())).map(|_| ()),
            };
            match result {
                Ok(()) => CleanupOutcome::AlreadyAbsent,
                Err(e) => CleanupOutcome::Retryable(format!("port absence not confirmed: {e}")),
            }
        }
        CleanupTarget::Subscription { host, .. } if crate::host::local_host_gone(host) => {
            CleanupOutcome::AlreadyAbsent
        }
        CleanupTarget::Subscription { host, .. } => match process_present(host.process()) {
            Ok(false) => CleanupOutcome::AlreadyAbsent,
            Ok(true) => CleanupOutcome::Blocked("subscription requires its owning host".into()),
            Err(e) => CleanupOutcome::Unknown(e.to_string()),
        },
        CleanupTarget::Plugin { process, .. } => cleanup_process(process),
        CleanupTarget::Provider => {
            CleanupOutcome::Blocked("cleanup provider is unavailable".into())
        }
        CleanupTarget::Unresolved { reason, .. } => CleanupOutcome::Blocked(reason.clone()),
    }
}
pub(super) struct BootstrapWorld;
impl CleanupExecutor for BootstrapWorld {
    fn execute(&mut self, work: &CleanupWork) -> CleanupOutcome {
        execute_target(work)
    }
}

pub(super) fn legacy_process_absent(pid: u32, start: u64) -> std::io::Result<bool> {
    match process_stat(pid) {
        Ok((s, _)) if s != start => Ok(true),
        Ok(_) => {
            let w = ProcessWitness::new(pid, start, boot()?.into())
                .map_err(|_| std::io::Error::other("invalid legacy process"))?;
            process_present(&w).map(|p| !p)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(e) => Err(e),
    }
}
