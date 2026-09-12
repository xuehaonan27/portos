//! The two axes a plugin sits on: **what it is**, and **how it runs**.
//!
//! *What it is* — an executable in the CAS, named by a content address, or
//! (the escape hatch) a path on this host.
//!
//! *How it runs* — a bare child process, or that child inside a cgroup. The
//! kernel implements exactly those two, because they are as far as it can go
//! without learning a domain; containers and VMs belong to drivers.
//!
//! The point of separating them is that neither should leak into the other,
//! and neither should leak into the plugin.
//!
//! What the cgroup form buys is measurable rather than theoretical, and
//! these tests measure it: a grandchild that leaves the process group with
//! `setsid` survives every signal a process group can send, and does not
//! survive `cgroup.kill`. The second thing it buys has no equivalent at all
//! — a cgroup is a *named directory*, so a runtime that was killed without
//! running teardown leaves something the next one can find.
//!
//! Skips, with a reason, where there is no writable cgroup v2.

use portos_kernel::Kernel;
use portos_kernel::cgroup::CgroupRoot;
use portos_kernel::host::{Form, Host, LaunchSpec};
use portos_proto::ids::PluginName;
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const ECHO_BIN: &str = env!("CARGO_BIN_EXE_portos-echo");

fn setup(tag: &str) -> (Arc<Kernel>, Host, PathBuf) {
    let root = std::env::temp_dir().join(format!("portos-form-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let kernel = Arc::new(Kernel::open(&root).unwrap());
    let host = Host::new(kernel.clone(), &root.join("sock")).unwrap();
    (kernel, host, root)
}

/// An echo driver whose grandchild deliberately leaves the process group.
fn spawn_with_escaping_grandchild(
    host: &Host,
    family: &str,
    pidfile: &Path,
    form: Form,
) -> PluginName {
    host.spawn_spec(&LaunchSpec {
        env: [
            ("PORTOS_ECHO_FAMILY".to_string(), family.to_string()),
            (
                "PORTOS_ECHO_GRANDCHILD".to_string(),
                pidfile.to_string_lossy().into_owned(),
            ),
            (
                "PORTOS_ECHO_GRANDCHILD_ESCAPES".to_string(),
                "1".to_string(),
            ),
        ]
        .into_iter()
        .collect(),
        form,
        ..LaunchSpec::from_path(ECHO_BIN)
    })
    .unwrap()
}

fn await_pid(path: &Path) -> i32 {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(pid) = text.trim().parse() {
                return pid;
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the grandchild never recorded itself");
}

fn alive(pid: i32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

fn gone_within(pid: i32, d: Duration) -> bool {
    let deadline = Instant::now() + d;
    while alive(pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
    }
    !alive(pid)
}

fn cgroups_or_skip() -> Option<CgroupRoot> {
    match CgroupRoot::detect() {
        Some(r) => Some(r),
        None => {
            eprintln!("skipping: no writable cgroup v2 on this machine");
            None
        }
    }
}

/// A plugin named by **what it is** rather than **where it is**.
///
/// A path is a claim about a file that may have changed since; a content
/// address is a claim anyone can check, means the same thing on every
/// machine, and can be carried in a spec without carrying any bytes. That
/// last part is why it matters here specifically: a `kernel::spawn` spec is
/// written by an agent and travels through the model's context, so it must
/// name things rather than contain them.
#[test]
fn a_plugin_can_be_named_by_what_it_is_instead_of_where_it_is() {
    let (kernel, host, root) = setup("artifact");

    // The same thing `portos put` does.
    let mut file = std::fs::File::open(ECHO_BIN).unwrap();
    let meta = kernel
        .cas
        .put_stream(
            &mut file,
            "application/x-executable",
            portos_proto::Label::default(),
            "test",
        )
        .unwrap();

    let plugin = host
        .spawn_spec(&LaunchSpec::from_artifact(meta.id.clone()))
        .expect("a plugin is startable from the CAS");
    let made: serde_json::Value = host
        .call(
            &plugin,
            &portos_proto::ids::Verb::parse("echo::make_ref").unwrap(),
            portos_proto::wire::Payload::of(&serde_json::json!([])).unwrap(),
        )
        .unwrap()
        .parse()
        .unwrap();
    assert!(made["ref"].is_string(), "and it answers: {made}");

    // Materialised once, byte for byte. Content addressing is what makes
    // that cache correct without an invalidation rule.
    let copy = root.join("exec").join(meta.id.replace(':', "-"));
    assert!(copy.exists(), "the executable was materialised at {copy:?}");
    assert_eq!(
        std::fs::read(&copy).unwrap(),
        std::fs::read(ECHO_BIN).unwrap(),
        "what ran is what was stored"
    );
    let before = std::fs::metadata(&copy).unwrap().modified().unwrap();
    let second = host
        .spawn_spec(&LaunchSpec {
            env: [("PORTOS_ECHO_FAMILY".to_string(), "echotwo".to_string())]
                .into_iter()
                .collect(),
            ..LaunchSpec::from_artifact(meta.id.clone())
        })
        .unwrap();
    assert_eq!(
        std::fs::metadata(&copy).unwrap().modified().unwrap(),
        before,
        "a second spawn reuses it rather than writing it again"
    );

    // Exactly one of the two, and the refusal says which mistake was made.
    let both = host.spawn_spec(&LaunchSpec {
        bin: Some(ECHO_BIN.to_string()),
        ..LaunchSpec::from_artifact(meta.id.clone())
    });
    assert!(
        both.unwrap_err().to_string().contains("exactly one"),
        "naming a plugin twice is a mistake worth a message"
    );
    let neither = host.spawn_spec(&LaunchSpec::default());
    assert!(
        neither
            .unwrap_err()
            .to_string()
            .contains("neither `artifact` nor `bin`"),
        "and so is naming it not at all"
    );

    host.shutdown(&second);
    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

/// The property a plugin author is entitled to rely on: **the same artifact
/// behaves the same under every form the kernel implements.**
///
/// A driver writes `serve(plugin, on_call, on_event)` and is never told which
/// form it got — not by an argument, not by an environment variable, not by
/// anything it can observe. This runs the identical binary under both and
/// insists the results match, across all three things the ABI offers: a verb
/// call, the chunked data plane in both directions, and the event bus. When a
/// container form arrives it gains a case here rather than a new argument.
#[test]
fn the_same_plugin_behaves_identically_under_every_form() {
    if cgroups_or_skip().is_none() {
        return;
    }
    // Separate kernels so both can use the same plugin name and family, and
    // the results are comparable down to the artifact ids.
    let (_kb, bare_host, bare_root) = setup("same-bare");
    let (_kc, cg_host, cg_root) = setup("same-cgroup");

    let bare = exercise(&bare_host, Form::Bare);
    let cgroup = exercise(&cg_host, Form::Cgroup);
    assert_eq!(
        bare, cgroup,
        "the form is the runtime's business, not the plugin's"
    );

    bare_host.shutdown_all();
    cg_host.shutdown_all();
    let _ = std::fs::remove_dir_all(&bare_root);
    let _ = std::fs::remove_dir_all(&cg_root);
}

/// Everything a plugin can do through the ABI, in one value: a verb call, an
/// artifact streamed in and read back out, and an event published.
fn exercise(host: &Host, form: Form) -> serde_json::Value {
    let plugin = host
        .spawn_spec(&LaunchSpec {
            form,
            ..LaunchSpec::from_path(ECHO_BIN)
        })
        .unwrap();
    let call = |verb: &str, args: serde_json::Value| -> serde_json::Value {
        host.call(
            &plugin,
            &portos_proto::ids::Verb::parse(verb).unwrap(),
            portos_proto::wire::Payload::of(&args).unwrap(),
        )
        .unwrap()
        .parse()
        .unwrap()
    };

    let made = call("echo::make_ref", serde_json::json!([]));
    let put = call("echo::put_pattern", serde_json::json!([4096]));
    let id = put["meta"]["id"].as_str().unwrap().to_string();
    let digest = call("echo::digest", serde_json::json!([id.clone()]));
    let published = call(
        "echo::publish",
        serde_json::json!(["probe::hello", {"n": 1}]),
    );

    serde_json::json!({
        "name": plugin.as_str(),
        "ref": made["ref"],
        // Content-addressed, so an identical stream is an identical id —
        // which also proves the data plane went through unchanged.
        "artifact": id,
        "size": put["meta"]["size"],
        "origin": put["meta"]["origin"],
        "digest": digest,
        "published": published,
    })
}

/// The claim, and its control. A process group cannot reach a child that
/// called `setsid`; a cgroup can. Both halves are asserted, because the
/// second is only interesting given the first.
#[test]
fn a_cgroup_collects_what_a_process_group_cannot() {
    if cgroups_or_skip().is_none() {
        return;
    }
    let (_k, host, root) = setup("escape");

    // Control: in bare form the escapee outlives teardown.
    let bare_pidfile = root.join("bare.pid");
    let bare = spawn_with_escaping_grandchild(&host, "echobare", &bare_pidfile, Form::Bare);
    let escaped = await_pid(&bare_pidfile);
    host.shutdown(&bare);
    assert!(
        alive(escaped),
        "if a process group could collect this, the cgroup form would have no job"
    );
    // Not the kernel's to clean up — that is the point — so the test does it.
    unsafe { libc_kill(escaped) };

    // The same plugin, the same escaping grandchild, in cgroup form.
    let cg_pidfile = root.join("cgroup.pid");
    let plugin = spawn_with_escaping_grandchild(&host, "echocg", &cg_pidfile, Form::Cgroup);
    let collected = await_pid(&cg_pidfile);
    assert!(alive(collected));
    host.shutdown(&plugin);
    assert!(
        gone_within(collected, Duration::from_secs(5)),
        "a cgroup has no gap for setsid to go through: pid {collected} survived"
    );

    host.shutdown_all();
    let _ = std::fs::remove_dir_all(&root);
}

/// Removing the cgroup *is* the check, and this is why: the kernel refuses
/// `rmdir` on a cgroup that still holds anything. So a plugin whose cgroup
/// went away is a plugin whose whole tree went with it, and teardown does
/// not have to take anyone's word for it.
#[test]
fn rmdir_is_the_proof_because_a_busy_cgroup_refuses_it() {
    let Some(root) = cgroups_or_skip() else {
        return;
    };
    let cg = root.create("rmdirproof").expect("a cgroup");
    let mut stray = std::process::Command::new("sleep")
        .arg("300")
        .spawn()
        .unwrap();
    std::fs::write(cg.path().join("cgroup.procs"), stray.id().to_string()).unwrap();

    assert!(!cg.is_empty(), "something is in it");
    assert!(
        !cg.remove(),
        "a cgroup that still holds a process cannot be removed — which is \
         exactly what makes removal a proof"
    );

    assert!(cg.kill(), "the kernel accepted cgroup.kill");
    assert!(cg.remove(), "and once emptied it goes");
    assert!(!cg.path().exists());

    let status = reaped_status(&mut stray).expect("the process in it stopped");
    assert_eq!(status.signal(), Some(9), "it was killed: {status:?}");
}

/// The half a process group cannot do at all. `kill -9` on the runtime runs
/// no teardown, and a process group leaves nothing behind to come back to; a
/// cgroup leaves a directory named after the run that made it.
#[test]
fn a_cgroup_left_by_a_dead_run_is_collected_by_the_next_one() {
    let Some(shared) = cgroups_or_skip() else {
        return;
    };
    // Its own root, because every `Host::new` reaps orphans and the other
    // tests in this binary run alongside this one: without isolation the
    // thing under test gets collected by a bystander, and the assertion
    // below would be measuring nothing.
    let Some(cgroups) = CgroupRoot::at_dir(shared.path().join("orphan-test")) else {
        eprintln!("skipping: could not make a private cgroup root");
        return;
    };
    let dead = a_pid_that_does_not_exist();

    // Exactly what a killed runtime leaves: a cgroup named after it, with
    // something still in it.
    let orphan = cgroups
        .path()
        .join(format!("portos-{dead}-{}", std::process::id()));
    std::fs::create_dir_all(&orphan).unwrap();
    let mut stray = std::process::Command::new("sleep")
        .arg("300")
        .spawn()
        .unwrap();
    std::fs::write(orphan.join("cgroup.procs"), stray.id().to_string()).unwrap();

    // A cgroup belonging to a *living* runtime must be left strictly alone:
    // two of these may share a machine.
    let live = cgroups
        .path()
        .join(format!("portos-{}-livecheck", std::process::id()));
    std::fs::create_dir_all(&live).unwrap();

    let reaped = cgroups.reap_orphans();

    assert!(reaped >= 1, "the orphan was collected");
    assert!(!orphan.exists(), "and its directory is gone");
    assert!(
        live.exists(),
        "a living runtime's cgroup is not ours to take"
    );

    // Killed, not merely forgotten — asked by exit status rather than by
    // looking in `/proc`, because this one is *our* child and a zombie keeps
    // its `/proc` entry until someone waits on it.
    let status = reaped_status(&mut stray).expect("the stray stopped");
    assert_eq!(
        status.signal(),
        Some(9),
        "what was still in the cgroup was killed with it: {status:?}"
    );
    let _ = std::fs::remove_dir(&live);
    let _ = std::fs::remove_dir(cgroups.path());
}

/// Wait briefly for a child to stop, rather than forever if it did not.
fn reaped_status(child: &mut std::process::Child) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(Some(status)) = child.try_wait() {
            return Some(status);
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    None
}

/// A pid nothing holds. Scanning down from the maximum finds one far from
/// whatever the allocator is handing out, so this cannot collide with a
/// process that appears mid-test.
fn a_pid_that_does_not_exist() -> u32 {
    let max: u32 = std::fs::read_to_string("/proc/sys/kernel/pid_max")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(4_194_304);
    (1..1000)
        .map(|n| max - n)
        .find(|pid| !Path::new(&format!("/proc/{pid}")).exists())
        .expect("some pid is free")
}

/// `kill -9` without a libc dependency: the test owns this process only
/// because the bare form deliberately does not.
unsafe fn libc_kill(pid: i32) {
    let _ = std::process::Command::new("kill")
        .arg("-9")
        .arg(pid.to_string())
        .status();
}
