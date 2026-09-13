//! portos-tty: a front end at a terminal.
//!
//! A chat is not a mode of the launcher; it is a model driver plus something
//! that talks to it, and this is the something for a terminal: lines in
//! from stdin to `model::send`, the session's events out to stdout. It is a
//! plugin like any renderer, with zero verbs of its own — listed in
//! `portos.json` next to the model driver it needs, and replaceable by a
//! TUI, a browser presenter, or anything else that speaks the same
//! interface. It finds the driver by verb and cannot tell whose it is.
//!
//! It owns the terminal when it has one: it takes the foreground process
//! group, so keystrokes and Ctrl-C come here — Ctrl-C cancels the running
//! turn, and quits when idle. Quitting means asking the process that
//! started everything to stop, with `SIGTERM` to the launcher: a front end
//! cannot bring the runtime down through a verb, and should not be able to
//! through anything less deliberate than a signal to the one process that
//! is not a plugin.
//!
//! Config (from the launch spec), all optional:
//!   `{"resume": "latest" | "<session id>", "system": "…"}`

use nix::sys::signal::{self, SigHandler, SigSet, Signal};
use nix::unistd::{getpgid, getpgrp, getppid, isatty, tcsetpgrp};
use portos_abi::ids::Topic;
use portos_abi::wire::Payload;
use portos_kernel_api as kernel;
use portos_model_api as model;
use portos_sdk::{KernelClient, Plugin, PluginError};
use serde::Deserialize;
use std::collections::VecDeque;
use std::io::{BufRead, Write};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

#[derive(Default, Deserialize)]
#[serde(default)]
struct Config {
    /// `latest`, or a session id. Absent opens a new conversation.
    resume: Option<String>,
    /// Overrides the driver's system prompt for this session.
    system: Option<String>,
}

/// Everything the front end waits on, in one channel so waiting is one call.
enum Msg {
    Line(String),
    /// stdin closed.
    Eof,
    /// A session event. Every session's arrive, because the subscription is
    /// declared before this plugin knows which session is its own; the
    /// front end keeps the ones on its session's topic.
    Event(Topic, Payload),
    Interrupt,
    /// The launcher says everything it listed is running.
    Up,
}

/// How long to wait for the launcher before opening a session anyway — a
/// front end started some other way has no launcher to wait for.
const UP_WAIT: Duration = Duration::from_secs(10);

fn main() -> std::io::Result<()> {
    let cfg: Config = portos_sdk::config::config().map_err(std::io::Error::other)?;
    // Blocked before any thread exists so every thread inherits the mask and
    // only the waiter below ever sees an interrupt — which may then do real
    // work, a verb call, that is not safe inside an actual signal handler.
    let mut interrupts = SigSet::empty();
    interrupts.add(Signal::SIGINT);
    interrupts.thread_block()?;

    let (tx, rx) = channel::<Msg>();
    let events_tx = tx.clone();
    portos_sdk::serve(
        Plugin::new("portos-tty")
            // Said here because this plugin is the only thing that knows it.
            .needs(&model::START)
            // The launcher publishes this the moment its list is up.
            .subscribes(&kernel::UP)
            // Its own session's events, once it has one: a subscription is
            // declared, and the session id is not known until later.
            .subscribes(&model::ALL_SESSIONS)
            .on_ready(move |_registrar, client| {
                let client = client.clone();
                std::thread::spawn(move || {
                    if let Err(e) = front_end(&client, &cfg, tx, rx, interrupts) {
                        eprintln!("[tty] {e}");
                    }
                    quit();
                });
                Ok(())
            }),
        move |topic, data| {
            let msg = if topic == &*kernel::UP {
                Msg::Up
            } else {
                Msg::Event(topic.clone(), data.clone())
            };
            let _ = events_tx.send(msg);
        },
    )
}

fn front_end(
    client: &Arc<KernelClient>,
    cfg: &Config,
    tx: Sender<Msg>,
    rx: Receiver<Msg>,
    interrupts: SigSet,
) -> Result<(), PluginError> {
    let own_terminal = take_terminal();
    {
        let tx = tx.clone();
        std::thread::spawn(move || {
            while interrupts.wait().is_ok() {
                if tx.send(Msg::Interrupt).is_err() {
                    return;
                }
            }
        });
    }
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            match line {
                Ok(l) => {
                    if tx.send(Msg::Line(l)).is_err() {
                        return;
                    }
                }
                Err(_) => break,
            }
        }
        let _ = tx.send(Msg::Eof);
    });

    // The whole set first — the tools, the other renderers — because a turn
    // started before them would run without them. `needs` cannot say this:
    // it names a driver, and only the launcher knows the list.
    let mut pending: VecDeque<Msg> = VecDeque::new();
    let deadline = Instant::now() + UP_WAIT;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left) {
            Ok(Msg::Up) => break,
            Ok(other) => pending.push_back(other),
            Err(_) => break,
        }
    }

    let sid = open_session(client, cfg)?;
    let own = sid.topic();
    println!("[tty] session {sid} — type a message; /cancel stops a turn, /exit quits\n");

    // While a turn runs, what is typed waits for it — `/exit` included,
    // because quitting mid-turn would take the whole runtime down under a
    // call still in flight. Only `/cancel` and an interrupt act at once.
    let mut busy = false;
    let mut interrupted = false;
    loop {
        let deferred = if busy { None } else { pending.pop_front() };
        let msg = match deferred {
            Some(m) => m,
            None => rx
                .recv()
                .map_err(|_| PluginError::Refused("the front end lost its input".into()))?,
        };
        match msg {
            Msg::Line(line) if busy => {
                if line.trim() == "/cancel" {
                    cancel(client, &sid);
                } else {
                    pending.push_back(Msg::Line(line));
                }
            }
            Msg::Eof if busy => pending.push_back(Msg::Eof),
            Msg::Event(topic, data) => {
                if topic == own && render(&data) {
                    busy = false;
                    interrupted = false;
                }
            }
            // A reload finished; nothing to do mid-session.
            Msg::Up => {}
            // A turn in flight is cancelled, not abandoned: an interrupt
            // almost always means "stop this", not "lose the session".
            // Interrupting again, or when idle, quits.
            Msg::Interrupt if busy && !interrupted => {
                interrupted = true;
                eprintln!("\n[tty] cancelling — interrupt again to quit");
                cancel(client, &sid);
            }
            Msg::Interrupt | Msg::Eof => break,
            Msg::Line(line) => {
                let text = line.trim();
                match text {
                    "" => {}
                    "/exit" | "/quit" => break,
                    "/cancel" => println!("[tty] nothing is running"),
                    text => {
                        let args = Payload::of(&model::SendArgs {
                            session: sid.clone(),
                            text: text.to_string(),
                        })?;
                        // `send` returns as soon as the turn is accepted; the
                        // turn is over when a terminal event arrives.
                        match client.invoke(&model::SEND, args) {
                            Ok(_) => busy = true,
                            Err(e) => println!("[tty] error: {e}"),
                        }
                    }
                }
            }
        }
    }
    let _ = client.invoke(&model::END, Payload::of(&model::EndArgs { session: sid })?);
    if own_terminal {
        give_terminal_back();
    }
    Ok(())
}

/// Open the conversation the config asks for.
///
/// Once, not retried: by the time the launcher has said its list is up, a
/// model driver that does not answer is a configuration error to report,
/// not a race to wait through.
fn open_session(client: &Arc<KernelClient>, cfg: &Config) -> Result<model::SessionId, PluginError> {
    let resume = match cfg.resume.as_deref() {
        None => None,
        Some("latest") => {
            let listed: model::SessionsReply = client
                .invoke(&model::SESSIONS, Payload::of(&model::SessionsArgs {})?)?
                .parse()?;
            match listed.sessions.first() {
                Some(entry) => {
                    println!("[tty] resuming {} ({} turns)", entry.id, entry.record.turns);
                    Some(entry.id.clone())
                }
                None => {
                    println!("[tty] nothing stored to resume — starting a new session");
                    None
                }
            }
        }
        Some(id) => {
            let id =
                model::SessionId::parse(id).map_err(|e| PluginError::Refused(e.to_string()))?;
            println!("[tty] resuming {id}");
            Some(id)
        }
    };
    let started: model::StartReply = client
        .invoke(
            &model::START,
            Payload::of(&model::StartArgs {
                system: cfg.system.clone(),
                resume,
            })?,
        )?
        .parse()?;
    Ok(started.session)
}

fn cancel(client: &KernelClient, sid: &model::SessionId) {
    if let Ok(args) = Payload::of(&model::CancelArgs {
        session: sid.clone(),
    }) {
        let _ = client.invoke(&model::CANCEL, args);
    }
}

/// One of the session's events, as it arrives. Returns whether it ended the
/// turn: exactly one terminal event does, and that is what lets the prompt
/// come back.
fn render(data: &Payload) -> bool {
    let Ok(event) = data.parse::<model::SessionEvent>() else {
        return false;
    };
    let mut out = std::io::stdout();
    match event {
        model::SessionEvent::Delta { text } => {
            print!("{text}");
            let _ = out.flush();
            false
        }
        model::SessionEvent::ToolCall { verb } => {
            println!("\n[tool→] {verb}");
            false
        }
        model::SessionEvent::ToolResult { verb, ok } => {
            println!("[tool{}] {verb}", if ok { "✓" } else { "✗" });
            false
        }
        model::SessionEvent::Done { .. } => {
            println!();
            true
        }
        model::SessionEvent::Cancelled => {
            println!("\n[tty] cancelled");
            true
        }
        model::SessionEvent::Failed { error } => {
            println!("\n[tty] failed: {error}");
            true
        }
        model::SessionEvent::Unknown => false,
    }
}

/// Become the terminal's foreground process group, so keystrokes and Ctrl-C
/// come here rather than to the launcher. Only when stdin is a terminal:
/// under a test it is a pipe, and there is nothing to take. A background
/// process that moves itself to the foreground is stopped with `SIGTTOU`
/// unless it ignores that signal first, so it does.
fn take_terminal() -> bool {
    if !isatty(0).unwrap_or(false) {
        return false;
    }
    // Safety: replacing the disposition of one signal with "ignore" before
    // any handler for it exists; nothing else in this process touches it.
    unsafe {
        let _ = signal::signal(Signal::SIGTTOU, SigHandler::SigIgn);
    }
    tcsetpgrp(std::io::stdin(), getpgrp()).is_ok()
}

/// Hand the terminal back to the launcher's group before asking it to stop.
fn give_terminal_back() {
    if let Ok(group) = getpgid(Some(getppid())) {
        let _ = tcsetpgrp(std::io::stdin(), group);
    }
}

/// Ask the launcher to bring the runtime down. It is the one thing that is
/// not a plugin, so there is no verb for this; a signal to the process that
/// started us is the honest way to say it.
fn quit() {
    let _ = signal::kill(getppid(), Signal::SIGTERM);
}
