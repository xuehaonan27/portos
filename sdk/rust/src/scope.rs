//! Running a child process you can be sure of collecting.
//!
//! A plugin that shells out has to answer four questions that have nothing to
//! do with its business: how to read both pipes without deadlocking, how long
//! to wait, how to stop something that will not stop, and how to be sure
//! nothing was left behind. The shell driver answered all four by hand, got
//! the last one wrong twice, and any second plugin that ran a command would
//! have had to answer them again.
//!
//! They are answered here instead, and the answer is the one the kernel
//! already uses on plugins: a cgroup, named in `PORTOS_PLUGIN_CGROUP`. Where
//! there is none, this falls back to a process group — the same escalation,
//! with the same hole (a child can leave a process group with `setsid`), so
//! that a plugin behaves sensibly on a machine without cgroup v2 rather than
//! refusing to work.
//!
//! What stays the caller's business: what command to run, in what directory,
//! with what environment, and what to make of the output.

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use portos_abi::boundary::CgroupRoot;
use std::io::Read;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long anything gets between being asked to stop and being made to.
const GRACE: Duration = Duration::from_millis(500);
const POLL: Duration = Duration::from_millis(20);

/// What a finished child left behind.
pub struct Completed {
    /// Exit code, or `None` when a signal ended it — including ours.
    pub status: Option<i32>,
    pub signal: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
    /// Output was still arriving when we stopped waiting for it. Only
    /// reachable if something survived a kill, but a caller told "this is all
    /// of it" deserves to know when it is not.
    pub output_truncated: bool,
    pub duration: Duration,
}

/// A place to run children that can be emptied afterwards.
pub struct Scope {
    cgroup: Option<PathBuf>,
}

impl Scope {
    /// `tag` distinguishes this scope's boundary from another in the same
    /// plugin, so two concurrent commands do not collect each other.
    pub fn new(tag: &str) -> Scope {
        let cgroup = std::env::var_os("PORTOS_PLUGIN_CGROUP")
            .map(PathBuf::from)
            .and_then(|own| CgroupRoot::at_dir(own))
            .and_then(|root| root.create(tag))
            .map(|cg| cg.path().to_path_buf());
        Scope { cgroup }
    }

    /// Run `cmd` to completion, or stop it after `timeout`.
    ///
    /// Both pipes are drained on their own threads, because a command that
    /// fills one while we wait on the other deadlocks and a real build fills
    /// both. The wait is bounded rather than trusting the pipes to close:
    /// liveness must not depend on when somebody else decides to let go of a
    /// file descriptor.
    pub fn run(&self, mut cmd: Command, timeout: Duration) -> std::io::Result<Completed> {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        if let Some(dir) = &self.cgroup {
            let file = std::fs::File::options()
                .write(true)
                .open(dir.join("cgroup.procs"))?;
            // Between fork and exec, so everything the child forks is inside
            // too. Joining afterwards leaves its existing children out, and
            // then emptying the cgroup misses exactly the processes that
            // needed collecting.
            unsafe {
                cmd.pre_exec(move || {
                    use std::io::Write;
                    (&file).write_all(b"0")
                });
            }
        }

        let began = Instant::now();
        let mut child = cmd.spawn()?;
        let pgid = child.id();
        let mut out = drain(child.stdout.take());
        let mut err = drain(child.stderr.take());

        let mut timed_out = false;
        let status = loop {
            if let Some(s) = child.try_wait()? {
                break s;
            }
            if began.elapsed() >= timeout {
                timed_out = true;
                self.stop(pgid);
                break child.wait()?;
            }
            std::thread::sleep(POLL);
        };

        // The leader is gone, which says nothing about its group. Ask the
        // rest politely, then insist — the survivors are the processes doing
        // real work, and being brutal to them while being patient with the
        // shell that started them has the courtesy exactly backwards.
        signal_group(pgid, Signal::SIGTERM);
        let mut settled = wait_for_eof(&mut out, &mut err, GRACE);
        if !settled {
            self.stop(pgid);
            settled = wait_for_eof(&mut out, &mut err, GRACE);
        }
        self.empty();

        Ok(Completed {
            status: status.code(),
            signal: status.signal(),
            stdout: out.take(),
            stderr: err.take(),
            timed_out,
            output_truncated: !settled,
            duration: began.elapsed(),
        })
    }

    /// The unanswerable one. A cgroup has no gap for `setsid` to go through;
    /// a process group does, and saying so is better than pretending.
    fn stop(&self, pgid: u32) {
        signal_group(pgid, Signal::SIGKILL);
        if let Some(dir) = &self.cgroup {
            let _ = std::fs::write(dir.join("cgroup.kill"), b"1");
        }
    }

    fn empty(&self) {
        if let Some(dir) = &self.cgroup {
            let _ = std::fs::write(dir.join("cgroup.kill"), b"1");
        }
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        self.empty();
        if let Some(dir) = &self.cgroup {
            let _ = std::fs::remove_dir(dir);
        }
    }
}

fn signal_group(pgid: u32, sig: Signal) {
    let _ = killpg(Pid::from_raw(pgid as i32), sig);
}

fn wait_for_eof(out: &mut Drain, err: &mut Drain, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    // Both, not short-circuited: the second still has until the deadline.
    let a = out.settled_by(deadline);
    let b = err.settled_by(deadline);
    a && b
}

/// A pipe read on its own thread, with the bytes readable *before* the read
/// finishes — so whoever is waiting can stop waiting and still have what
/// arrived.
struct Drain {
    buf: Arc<Mutex<Vec<u8>>>,
    done: Receiver<()>,
    settled: bool,
}

impl Drain {
    fn settled_by(&mut self, deadline: Instant) -> bool {
        if !self.settled {
            let left = deadline.saturating_duration_since(Instant::now());
            self.settled = self.done.recv_timeout(left).is_ok();
        }
        self.settled
    }

    /// Decoded lossily: a build that prints one stray byte is still a build
    /// whose log we want.
    fn take(&self) -> String {
        String::from_utf8_lossy(&self.buf.lock().unwrap()).into_owned()
    }
}

fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> Drain {
    let buf: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
    let (tx, done) = std::sync::mpsc::channel();
    let sink = buf.clone();
    std::thread::spawn(move || {
        if let Some(mut p) = pipe {
            let mut chunk = [0u8; 8192];
            while let Ok(n) = p.read(&mut chunk) {
                if n == 0 {
                    break;
                }
                sink.lock().unwrap().extend_from_slice(&chunk[..n]);
            }
        }
        let _ = tx.send(());
    });
    Drain {
        buf,
        done,
        settled: false,
    }
}
