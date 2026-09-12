//! The shell driver: exit status, the data-plane discipline, and the part
//! that is easy to get wrong — what a timeout actually collects.

use portos_abi::ids::{PluginName, Verb};
use portos_abi::wire::Payload;
use portos_kernel::Kernel;
use portos_kernel::host::Host;
use serde_json::{Value, json};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const SHELL_BIN: &str = env!("CARGO_BIN_EXE_portos-shell");

struct Fixture {
    kernel: Arc<Kernel>,
    host: Host,
    plugin: PluginName,
    root: PathBuf,
    cwd: PathBuf,
}

impl Fixture {
    fn open(tag: &str) -> Fixture {
        let root = std::env::temp_dir().join(format!("portos-shell-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let cwd = root.join("work");
        std::fs::create_dir_all(&cwd).unwrap();
        let kernel = Arc::new(Kernel::open(&root).unwrap());
        let host = Host::new(kernel.clone(), &root.join("sock")).unwrap();
        let plugin = host
            .spawn(
                Path::new(SHELL_BIN),
                &[],
                &[(
                    "PORTOS_PLUGIN_CONFIG",
                    &format!(
                        "{{\"cwd\":{}}}",
                        serde_json::to_string(cwd.to_str().unwrap()).unwrap()
                    ),
                )],
            )
            .unwrap();
        Fixture {
            kernel,
            host,
            plugin,
            root,
            cwd,
        }
    }

    fn run(&self, args: Value) -> Value {
        self.host
            .call(
                &self.plugin,
                &Verb::parse("shell::run").unwrap(),
                Payload::of(&args).expect("test payload"),
            )
            .map(|p| p.parse().expect("json"))
            .expect("shell::run")
    }

    fn artifact(&self, handle: &str) -> String {
        let mut f = self.kernel.cas.open_read(&handle.to_string()).unwrap();
        let mut s = String::new();
        f.read_to_string(&mut s).unwrap();
        s
    }

    fn close(self) {
        self.host.shutdown_all();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

#[test]
fn reports_status_and_both_streams() {
    let fx = Fixture::open("status");

    let ok = fx.run(json!({"cmd": "echo hello"}));
    assert_eq!(ok["status"], 0);
    assert_eq!(ok["stdout"]["text"], "hello\n");
    assert_eq!(ok["stderr"]["text"], "");
    assert_eq!(ok["timed_out"], false);

    // A failure is a result, not an error: the model needs to see it.
    let bad = fx.run(json!({"cmd": "echo oops >&2; exit 3"}));
    assert_eq!(bad["status"], 3);
    assert_eq!(bad["stderr"]["text"], "oops\n");

    // The shell is a real shell, which is the point: a pipe is how a caller
    // keeps a long log out of the conversation in the first place.
    let piped = fx.run(json!({"cmd": "seq 1 100 | tail -2"}));
    assert_eq!(piped["stdout"]["text"], "99\n100\n");

    // cwd is where the driver was pointed.
    std::fs::write(fx.cwd.join("marker"), "x").unwrap();
    let listed = fx.run(json!({"cmd": "ls"}));
    assert_eq!(listed["stdout"]["text"], "marker\n");

    fx.close();
}

/// The reason this driver exists in the shape it does: a build log is data,
/// not conversation.
#[test]
fn a_long_log_goes_to_the_data_plane() {
    let fx = Fixture::open("bulk");
    let (context_before, _) = fx.host.meter();

    // ~78KB of output, the scale of a real test run.
    let out = fx.run(json!({"cmd": "seq 1 10000"}));

    assert!(out["stdout"]["text"].is_null(), "not inline: {out}");
    let handle = out["stdout"]["handle"].as_str().expect("a handle");
    let size = out["stdout"]["size"].as_u64().expect("a size");
    let body = fx.artifact(handle);
    assert_eq!(body.len() as u64, size);
    assert!(body.starts_with("1\n2\n"), "the log is intact");
    assert!(body.ends_with("10000\n"));
    assert!(
        out["stdout"]["preview"].as_str().unwrap().len() < 4096,
        "the preview is bounded"
    );

    let (context_after, data_after) = fx.host.meter();
    let context = context_after - context_before;
    assert!(data_after >= size, "bytes moved on the data plane");
    assert!(
        context < size / 8,
        "context spent {context}B on a {size}B log"
    );

    fx.close();
}

/// The dead end, closed.
///
/// A big result goes into the store and the caller gets a handle. Until now
/// the only thing anyone could do with that handle was read it back into the
/// conversation — which is exactly what storing it was meant to avoid. Now
/// the handle goes back *in*: the command sees a read-only file and does the
/// work where the bytes already are.
#[test]
fn a_stored_result_can_be_worked_on_without_reading_it_back() {
    let fx = Fixture::open("artifacts");

    // Something too big to be conversation.
    let produced = fx.run(json!({"cmd": "seq 1 20000"}));
    let handle = produced["stdout"]["handle"]
        .as_str()
        .expect("it went to the store")
        .to_string();

    let (context_before, data_before) = fx.host.meter();
    let counted = fx.run(json!({
        "cmd": "wc -l < \"$LOG\"; grep -c '^1[0-9]\\{3\\}$' \"$LOG\"",
        "artifacts": {"LOG": handle},
    }));
    assert_eq!(counted["status"], 0, "{counted}");
    assert_eq!(
        counted["stdout"]["text"], "20000\n1000\n",
        "the command really read all of it: {counted}"
    );

    // The point, measured: 20000 lines were searched and almost nothing
    // crossed the control plane to do it.
    let (context_after, data_after) = fx.host.meter();
    assert!(
        context_after - context_before < 2048,
        "answering cost {}B of context",
        context_after - context_before
    );
    assert_eq!(
        data_after, data_before,
        "and no bytes moved through the data plane either — the command \
         opened the file the store already had"
    );

    // The file it was given is the store's own, and read-only, so handing
    // the path out does not make immutability a promise someone has to keep.
    let located = fx.run(json!({
        "cmd": "stat -c '%a' \"$LOG\"",
        "artifacts": {"LOG": handle},
    }));
    assert_eq!(located["stdout"]["text"], "444\n");

    fx.close();
}

/// A timeout has to collect the process *group*. `sh -c` is almost never the
/// thing doing the work, and killing only the leader leaves the work running
/// with nobody waiting on it — the same lesson the kernel learned about
/// plugins and their grandchildren.
#[test]
fn a_timeout_collects_the_whole_process_group() {
    let fx = Fixture::open("timeout");
    let pidfile = fx.cwd.join("grandchild.pid");

    let began = Instant::now();
    let out = fx.run(json!({
        // A backgrounded grandchild that outlives its parent's own sleep, and
        // which also holds the stdout pipe open — so this fails two ways if
        // the group is not collected: a leaked process, or a call that hangs.
        "cmd": format!("sleep 300 & echo $! > {}; sleep 300", pidfile.display()),
        "timeout_ms": 700,
    }));
    assert_eq!(out["timed_out"], true, "{out}");
    assert!(
        began.elapsed() < Duration::from_secs(10),
        "the call returned rather than waiting on a pipe nobody will close"
    );

    let pid: i32 = std::fs::read_to_string(&pidfile)
        .expect("the grandchild recorded itself")
        .trim()
        .parse()
        .unwrap();
    // Give the signal a moment to land, then insist it is gone.
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !alive(pid),
        "the grandchild outlived the timeout: pid {pid}"
    );

    fx.close();
}

/// The same collection on the ordinary path, where it is easy to forget:
/// the command *succeeded*, so no escalation ran, and a backgrounded child
/// would both leak and hold the output pipe open — which is a hang, not just
/// a leak.
#[test]
fn a_background_child_does_not_survive_a_command_that_exits_normally() {
    let fx = Fixture::open("background");
    let pidfile = fx.cwd.join("bg.pid");

    let began = Instant::now();
    let out = fx.run(json!({
        "cmd": format!("sleep 300 & echo $! > {}; echo done", pidfile.display()),
        "timeout_ms": 5000,
    }));
    assert_eq!(out["status"], 0, "the command itself succeeded: {out}");
    assert_eq!(out["timed_out"], false);
    assert_eq!(out["stdout"]["text"], "done\n");
    assert!(
        began.elapsed() < Duration::from_secs(5),
        "returned rather than waiting on a pipe the background child still holds"
    );

    let pid: i32 = std::fs::read_to_string(&pidfile)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !alive(pid),
        "shell::run leaves nothing running; something meant to keep running is a plugin"
    );

    fx.close();
}

/// The courtesy the first version got backwards. `sh` — the process least
/// likely to have anything to clean up — was given SIGTERM then SIGKILL,
/// while the children actually doing the work were given only SIGKILL. Now
/// the group gets the same escalation, so a child that handles SIGTERM can
/// finish what it was doing.
#[test]
fn a_background_child_gets_a_chance_to_clean_up() {
    let fx = Fixture::open("grace");
    let marker = fx.cwd.join("cleaned");

    let out = fx.run(json!({
        // `sleep & wait` rather than a bare sleep: a POSIX shell runs a trap
        // only once the foreground command returns, so a plain `sleep 300`
        // would swallow the signal until it finished.
        "cmd": format!(
            "(trap 'echo done > {}; exit 0' TERM; sleep 300 & wait) & echo started",
            marker.display()
        ),
        "timeout_ms": 5000,
    }));
    assert_eq!(out["status"], 0);
    assert_eq!(out["stdout"]["text"], "started\n");
    assert!(
        out["output_truncated"].as_bool().unwrap_or(false) == false,
        "the child closed the pipe on its own, so nothing was cut short: {out}"
    );

    let deadline = Instant::now() + Duration::from_secs(5);
    while !marker.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        marker.exists(),
        "the background child was killed outright instead of being asked first"
    );

    fx.close();
}

/// The hole that was left, closed by moving the work into the SDK.
///
/// While this driver collected its own children it could only use a process
/// group, and a child that calls `setsid` walks out of one — anything that
/// daemonises does. `portos_sdk::scope` uses the cgroup the kernel gave this
/// plugin, and nothing walks out of a cgroup.
#[test]
fn a_command_that_daemonises_is_still_collected() {
    let fx = Fixture::open("daemon");
    let pidfile = fx.cwd.join("daemon.pid");

    let out = fx.run(json!({
        "cmd": format!("setsid sleep 300 & echo $! > {}; echo started", pidfile.display()),
        "timeout_ms": 5000,
    }));
    assert_eq!(out["status"], 0, "the command itself succeeded: {out}");

    let pid: i32 = std::fs::read_to_string(&pidfile)
        .expect("the daemon recorded itself")
        .trim()
        .parse()
        .unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(
        !alive(pid),
        "a cgroup has no gap for setsid to go through: pid {pid} survived"
    );

    fx.close();
}

/// `kill -0`: does a process still exist?
fn alive(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}
