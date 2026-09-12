//! A boundary that whatever is inside cannot leave.
//!
//! Part of the ABI because **the boundary a plugin runs in is part of what
//! being a plugin means** — it is handed over the same contract as the
//! socket, in `PORTOS_PLUGIN_CGROUP`. Both ends use it and for the same
//! reason: the kernel puts a plugin in one, a plugin puts its own children
//! in one, and those are the same problem. `shell::run` re-implemented
//! signal escalation only because it had no other way to run a command it
//! could be sure of collecting.
//!
//! `cgroup` is the mechanism, not the concept. If a second one ever appears
//! it belongs behind this module, not beside it.
//!
//! A process group is the cheapest grouping Unix offers, and it is what
//! teardown used until now. It has two holes. A child can walk out of it
//! with `setsid`, and a reaped leader's pgid can in principle be reused by
//! something unrelated before the final signal lands. Measured on this
//! project's dev box: start two children, have one call `setsid`, then
//! `killpg` leaves one alive and `cgroup.kill` leaves none.
//!
//! Nothing here needs root, and nothing here needs a controller to be
//! enabled — `cgroup.kill` works on a bare cgroup. Limits (`memory.max`,
//! `pids.max`, `cpu.max`) do need delegated controllers, and are
//! deliberately not set yet: reclamation is what has actually been biting,
//! and a limit nobody asked for is a number to get wrong.
//!
//! The second thing it gives is the one a process group cannot. `kill -9` on
//! PortOS itself runs no teardown at all, and a process group leaves nothing
//! behind to find. A cgroup leaves a **directory**, so the next run can
//! collect what the last one dropped — which turns "we lost track of it"
//! into "there is a directory".
//!
//! What it does *not* reclaim, stated so the boundary is not mistaken for a
//! bigger one: mounts, network configuration and IPC objects belong to
//! namespaces, files written to disk belong to whoever owns the disk, and
//! nothing at any level undoes a request that already went out.

use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Where plugin cgroups are made. Absent when this machine has no cgroup v2,
/// or nowhere writable in it, in which case plugins fall back to plain
/// process groups and teardown keeps the holes described above.
pub struct CgroupRoot {
    base: PathBuf,
}

impl CgroupRoot {
    /// `PORTOS_CGROUP_ROOT` if set, else a `portos.slice` beside this
    /// process's own cgroup, else one inside it.
    ///
    /// Beside is preferred because it survives the shell session that
    /// happened to start us — which is the whole point of leaving a
    /// tombstone a later run can find.
    pub fn detect() -> Option<CgroupRoot> {
        if let Some(dir) = std::env::var_os("PORTOS_CGROUP_ROOT") {
            return CgroupRoot::at_dir(PathBuf::from(dir));
        }
        let own = own_cgroup()?;
        own.parent()
            .and_then(|p| CgroupRoot::at_dir(p.join("portos.slice")))
            .or_else(|| CgroupRoot::at_dir(own.join("portos.slice")))
    }

    /// A root at an exact path, creating it if need be. The escape hatch
    /// behind `PORTOS_CGROUP_ROOT`, and the way a caller places plugin
    /// cgroups somewhere deliberate rather than wherever it happens to be
    /// running.
    pub fn at_dir(base: PathBuf) -> Option<CgroupRoot> {
        std::fs::create_dir_all(&base).ok()?;
        // A directory under a cgroup2 mount is populated by the kernel; if
        // these are missing we are not looking at a cgroup.
        base.join("cgroup.procs")
            .exists()
            .then_some(CgroupRoot { base })
    }

    pub fn path(&self) -> &Path {
        &self.base
    }

    /// One cgroup for one plugin. The name starts with the pid of the
    /// runtime that made it, so a later run can tell its own leftovers from
    /// a live sibling's. Everything after that is the caller's business, and
    /// has to be unique among *this* process's hosts as well as its plugins:
    /// a test binary runs several hosts at once, and two of them sharing a
    /// cgroup means stopping one plugin kills another's.
    pub fn create(&self, tag: &str) -> Option<Cgroup> {
        let dir = self
            .base
            .join(format!("portos-{}-{tag}", std::process::id()));
        std::fs::create_dir_all(&dir).ok()?;
        dir.join("cgroup.kill").exists().then_some(Cgroup { dir })
    }

    /// Collect what a previous run left when it died without teardown.
    ///
    /// A cgroup names the pid that created it, so "was this ours, and is that
    /// process gone?" is a question the filesystem can answer. Anything
    /// belonging to a living process is left strictly alone — two runtimes
    /// may share a machine.
    pub fn reap_orphans(&self) -> usize {
        let Ok(entries) = std::fs::read_dir(&self.base) else {
            return 0;
        };
        let mut reaped = 0;
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(owner) = name
                .to_str()
                .and_then(|n| n.strip_prefix("portos-"))
                .and_then(|rest| rest.split('-').next())
                .and_then(|pid| pid.parse::<u32>().ok())
            else {
                continue;
            };
            if Path::new(&format!("/proc/{owner}")).exists() {
                continue;
            }
            let cg = Cgroup { dir: entry.path() };
            cg.kill();
            if cg.remove() {
                reaped += 1;
            }
        }
        reaped
    }
}

pub struct Cgroup {
    dir: PathBuf,
}

impl Cgroup {
    /// The handle a forked child writes itself into.
    ///
    /// Opened **before** the fork on purpose, twice over: the child then has
    /// only a `write` to do (no path resolution, no allocation, between fork
    /// and exec), and joining before it forks anything of its own is what
    /// makes the cgroup complete. A process moved in afterwards leaves its
    /// existing children in the old cgroup, and `cgroup.kill` misses them
    /// entirely — which is exactly the shape of the first attempt at this.
    pub fn procs_file(&self) -> std::io::Result<File> {
        File::options()
            .write(true)
            .open(self.dir.join("cgroup.procs"))
    }

    /// Kill everything in it, atomically and with no escape. Returns whether
    /// the kernel accepted the request.
    pub fn kill(&self) -> bool {
        File::options()
            .write(true)
            .open(self.dir.join("cgroup.kill"))
            .and_then(|mut f| f.write_all(b"1"))
            .is_ok()
    }

    /// Whether anything is still in it. `rmdir` only succeeds on an empty
    /// cgroup, which makes removal itself the proof that reclamation worked.
    pub fn is_empty(&self) -> bool {
        std::fs::read_to_string(self.dir.join("cgroup.procs"))
            .map(|s| s.trim().is_empty())
            .unwrap_or(true)
    }

    pub fn remove(&self) -> bool {
        remove(&self.dir)
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }
}

/// `rmdir`, retried briefly: the kernel empties a killed cgroup
/// asynchronously, so the first attempt can lose a race it will win a
/// millisecond later.
fn remove(dir: &Path) -> bool {
    for _ in 0..50 {
        if std::fs::remove_dir(dir).is_ok() {
            return true;
        }
        if !dir.exists() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    false
}

fn own_cgroup() -> Option<PathBuf> {
    // cgroup v2 gives exactly one line, `0::<path>`; a v1 machine has other
    // lines and no `0::`, which is the case this returns None for.
    let text = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = text.lines().find_map(|l| l.strip_prefix("0::"))?;
    Some(Path::new("/sys/fs/cgroup").join(rel.trim().trim_start_matches('/')))
}
