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
use std::time::{Duration, Instant};

const DEFAULT_TIMEOUT_MS: u64 = 120_000;
/// How long the group gets between SIGTERM and SIGKILL.
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
    let out = drain(child.stdout.take());
    let err = drain(child.stderr.take());

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
    // Only now: a backgrounded grandchild can hold a pipe open long after its
    // parent exits, so joining before the group is dealt with would hang.
    let stdout = out.join().unwrap_or_default();
    let stderr = err.join().unwrap_or_default();

    let label = Label::with_integ(&format!("shell:{}", truncate(&a.cmd, LABEL_CMD_CHARS)));
    Ok(Payload::of(&RunReply {
        status: status.code(),
        signal: status.signal(),
        stdout: sink.deliver(client, &stdout, "text/plain", Some(label.clone()))?,
        stderr: sink.deliver(client, &stderr, "text/plain", Some(label))?,
        timed_out,
        duration_ms: began.elapsed().as_millis() as u64,
    })?)
}

/// Read a pipe to the end on its own thread. Output is decoded lossily: a
/// build that prints one stray byte is still a build whose log we want.
fn drain<R: Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<String> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        if let Some(mut p) = pipe {
            let _ = p.read_to_end(&mut buf);
        }
        String::from_utf8_lossy(&buf).into_owned()
    })
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
