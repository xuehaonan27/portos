//! portos-shell: run a command, and do not drown the model in its output.
//!
//! One verb. The interesting parts are all about what happens around the
//! command rather than the command itself:
//!
//! **Output is a data-plane problem.** `cargo test --workspace` in a real
//! repository prints tens of kilobytes; that belongs in the CAS with a
//! preview, not in the model's context. `portos_bulk_api::Sink` makes the
//! same call the browser and fs drivers make.
//!
//! **The command is a whole string handed to `sh -c`, and that is
//! deliberate.** Pipes are context discipline: `cargo test 2>&1 | tail -40`
//! has the shell shrink the output before it ever reaches us, which is
//! cheaper than paging through an artifact afterwards. It also means this
//! verb is arbitrary execution, which is the same thing `kernel::spawn`
//! already grants.
//!
//! **A timeout collects the process group, not the process.** `sh -c` is
//! rarely the thing doing the work; it has children, and they have children.
//! The kernel learned this about plugins and it is the same lesson here, so
//! the command leads its own group and the escalation is the same:
//! SIGTERM to the group, then SIGKILL.
//!
//! Known shape, stated: this call blocks until the command ends. A turn
//! cancelled mid-command is only noticed once the command returns, because
//! cancellation is checked between tool calls. Making it asynchronous is the
//! road `model::send` already took — accepted rather than awaited, with
//! exactly one terminal event — and it is worth taking when somebody is
//! actually waiting on a five-minute build.
//!
//! Config (environment):
//!   PORTOS_SHELL_CWD   default working directory (the process's own if unset)

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use portos_bulk_api::{Bulk, Sink};
use portos_proto::Label;
use portos_proto::wire::{Payload, ToolMeta};
use portos_sdk::{CallError, CallResult, KernelClient, Plugin};
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// How long anything gets between SIGTERM and SIGKILL — the leader, and then
/// the rest of its group.
const GRACE_MS: u64 = 500;
const POLL_MS: u64 = 20;
/// Provenance labels carry the command; a whole script would not be a label.
const LABEL_CMD_CHARS: usize = 80;

fn main() -> std::io::Result<()> {
    let cwd = match std::env::var_os("PORTOS_SHELL_CWD") {
        Some(p) => std::fs::canonicalize(p)?,
        None => std::env::current_dir()?,
    };
    eprintln!("[shell] cwd {}", cwd.display());
    let sink = Sink::default();

    portos_sdk::serve(
        Plugin::new("portos-shell", &["shell::run"]).with_tools(tools()),
        move |verb, args, client| match verb.short() {
            "run" => run(&cwd, &sink, client, args.parse()?),
            other => Err(CallError::from(format!("unknown verb: {other}"))),
        },
        |_topic, _data| {},
    )
}

#[derive(Deserialize)]
struct RunArgs {
    cmd: String,
    /// Relative to the driver's default working directory.
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Serialize)]
struct RunReply {
    /// Exit code, or `null` when a signal ended it (including our own).
    status: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    signal: Option<i32>,
    stdout: Bulk,
    stderr: Bulk,
    timed_out: bool,
    /// Output was still arriving when we stopped waiting for it. Only
    /// reachable if something in the group survived a SIGKILL, but a caller
    /// that is told "this is all of it" deserves to know when it is not.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    output_truncated: bool,
    duration_ms: u64,
}

fn run(base: &PathBuf, sink: &Sink, client: &KernelClient, a: RunArgs) -> CallResult {
    let cwd = match &a.cwd {
        Some(rel) => base.join(rel),
        None => base.clone(),
    };
    let timeout = Duration::from_millis(a.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));

    let mut child = Command::new("sh")
        .arg("-c")
        .arg(&a.cmd)
        .current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Lead a group, so a timeout can collect whatever the command started.
        .process_group(0)
        .spawn()
        .map_err(|e| CallError::from(format!("spawn: {e}")))?;

    let pgid = child.id();
    // Drain both pipes on their own threads. A command that fills one pipe
    // while we wait on the other deadlocks, and a large build fills both.
    let mut out = drain(child.stdout.take());
    let mut err = drain(child.stderr.take());

    let began = Instant::now();
    let mut timed_out = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(s)) => break s,
            Err(e) => return Err(CallError::from(format!("wait: {e}"))),
            Ok(None) => {}
        }
        if began.elapsed() >= timeout {
            timed_out = true;
            // Same escalation the kernel uses on a plugin, for the same
            // reason: the leader is rarely the only thing running.
            signal_group(pgid, Signal::SIGTERM);
            if let Some(s) = wait_briefly(&mut child, GRACE_MS) {
                break s;
            }
            signal_group(pgid, Signal::SIGKILL);
            break child
                .wait()
                .map_err(|e| CallError::from(format!("wait after kill: {e}")))?;
        }
        std::thread::sleep(Duration::from_millis(POLL_MS));
    };

    // The leader has been reaped, which says nothing about its group: a
    // backgrounded grandchild outlives it, is no longer anything we can wait
    // on, and holds the output pipe open. Two separate things follow from
    // that, and conflating them was the first version's mistake.
    //
    // The group gets the *same* escalation the leader got, not a bare
    // SIGKILL. These are the processes actually doing work — the ones with a
    // buffer to flush or a lock file to remove — so being polite to `sh` and
    // brutal to them had the courtesy exactly backwards.
    //
    // And the wait is bounded, because liveness must not depend on a pipe
    // somebody else decides when to close. Killing writers to obtain EOF is
    // using resource reclamation to fix a liveness bug; the deadline is what
    // actually fixes it, and the signals are then only about reclamation.
    //
    // The consequence callers should know: `shell::run` leaves nothing
    // running. `some-daemon &` does not survive the call. Something meant to
    // keep running is a plugin, not a background job.
    //
    // A process group is the weakest form of this boundary — it can be left
    // with `setsid`, and a reaped leader's pgid can in principle be reused.
    // A cgroup has neither hole, which is why the runtime-form work exists.
    signal_group(pgid, Signal::SIGTERM);
    let mut settled = wait_for_eof(&mut out, &mut err, GRACE_MS);
    if !settled {
        signal_group(pgid, Signal::SIGKILL);
        settled = wait_for_eof(&mut out, &mut err, GRACE_MS);
    }
    let stdout = out.take();
    let stderr = err.take();
    let output_truncated = !settled;

    let label = Label::with_integ(&format!("shell:{}", truncate(&a.cmd, LABEL_CMD_CHARS)));
    Ok(Payload::of(&RunReply {
        status: status.code(),
        signal: status.signal(),
        stdout: sink.deliver(client, &stdout, "text/plain", Some(label.clone()))?,
        stderr: sink.deliver(client, &stderr, "text/plain", Some(label))?,
        timed_out,
        output_truncated,
        duration_ms: began.elapsed().as_millis() as u64,
    })?)
}

/// Wait for both pipes to reach EOF, or give up. Returns whether they did.
fn wait_for_eof(out: &mut Drain, err: &mut Drain, ms: u64) -> bool {
    let deadline = Instant::now() + Duration::from_millis(ms);
    // Both, not short-circuited: the second one still has until the deadline.
    let a = out.settled_by(deadline);
    let b = err.settled_by(deadline);
    a && b
}

/// A pipe being read on its own thread, with the bytes readable *before* the
/// read finishes. That is the whole point: whoever is waiting can stop
/// waiting and still have what arrived.
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

    /// Output decoded lossily: a build that prints one stray byte is still a
    /// build whose log we want.
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

fn wait_briefly(child: &mut std::process::Child, ms: u64) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + Duration::from_millis(ms);
    while Instant::now() < deadline {
        if let Ok(Some(s)) = child.try_wait() {
            return Some(s);
        }
        std::thread::sleep(Duration::from_millis(POLL_MS));
    }
    None
}

fn signal_group(pgid: u32, sig: Signal) {
    let _ = killpg(Pid::from_raw(pgid as i32), sig);
}

fn truncate(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((end, _)) => format!("{}…", &s[..end]),
        None => s.to_string(),
    }
}

fn tools() -> BTreeMap<&'static str, ToolMeta> {
    let mut t = BTreeMap::new();
    t.insert(
        "shell::run",
        ToolMeta {
            description: "Run a shell command and return its exit status and output. \
                          The command is passed to `sh -c`, so pipes and redirection \
                          work — use them: `… 2>&1 | tail -40` keeps a long log out \
                          of the conversation. Output over ~16KB comes back as \
                          {handle, size, preview}; read the rest with artifact::read. \
                          The call blocks until the command ends or times out."
                .to_string(),
            schema: Payload::of(&json!({
                "type": "object",
                "properties": {
                    "cmd": {"type": "string", "description": "passed to sh -c"},
                    "cwd": {"type": "string", "description": "relative to the driver's working directory"},
                    "timeout_ms": {"type": "integer", "description": "default 120000"},
                },
                "required": ["cmd"],
            }))
            .ok(),
        },
    );
    t
}
