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
use portos_abi::ids::Verb;
use portos_abi::wire::Payload;
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
    /// The turn is over, one way or another.
    TurnOver,
    Interrupt,
}

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
        move |_topic, data| render(data, &events_tx),
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

    let sid = open_session(client, cfg)?;
    client.subscribe(&sid.topic())?;
    println!("[tty] session {sid} — type a message; /cancel stops a turn, /exit quits\n");

    // While a turn runs, what is typed waits for it — `/exit` included,
    // because quitting mid-turn would take the whole runtime down under a
    // call still in flight. Only `/cancel` and an interrupt act at once.
    let mut busy = false;
    let mut interrupted = false;
    let mut pending: VecDeque<Msg> = VecDeque::new();
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
            Msg::TurnOver => {
                busy = false;
                interrupted = false;
            }
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
/// The model driver may be up and not yet usable — waiting on its own grant,
/// or on its gateway — and a front end that gave up at the first "no route"
/// would be racing the launcher that started them both. So this waits,
/// saying why once.
fn open_session(client: &Arc<KernelClient>, cfg: &Config) -> Result<model::SessionId, PluginError> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let told = std::cell::Cell::new(false);
    let call = |verb: &Verb, args: Payload| -> Result<Payload, PluginError> {
        loop {
            match client.invoke(verb, args.clone()) {
                Ok(reply) => return Ok(reply),
                Err(e) if Instant::now() < deadline => {
                    if !told.replace(true) {
                        eprintln!("[tty] waiting for a model driver ({e})");
                    }
                    std::thread::sleep(Duration::from_millis(200));
                }
                Err(e) => return Err(e),
            }
        }
    };
    let resume = match cfg.resume.as_deref() {
        None => None,
        Some("latest") => {
            let listed: model::SessionsReply =
                call(&model::SESSIONS, Payload::of(&model::SessionsArgs {})?)?.parse()?;
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
    let started: model::StartReply = call(
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

/// The session's events, as they arrive. Exactly one terminal event ends a
/// turn, and that is what lets the prompt come back.
fn render(data: &Payload, tx: &Sender<Msg>) {
    let Ok(event) = data.parse::<model::SessionEvent>() else {
        return;
    };
    let mut out = std::io::stdout();
    match event {
        model::SessionEvent::Delta { text } => {
            print!("{text}");
            let _ = out.flush();
        }
        model::SessionEvent::ToolCall { verb } => println!("\n[tool→] {verb}"),
        model::SessionEvent::ToolResult { verb, ok } => {
            println!("[tool{}] {verb}", if ok { "✓" } else { "✗" });
        }
        model::SessionEvent::Done { .. } => {
            println!();
            let _ = tx.send(Msg::TurnOver);
        }
        model::SessionEvent::Cancelled => {
            println!("\n[tty] cancelled");
            let _ = tx.send(Msg::TurnOver);
        }
        model::SessionEvent::Failed { error } => {
            println!("\n[tty] failed: {error}");
            let _ = tx.send(Msg::TurnOver);
        }
        model::SessionEvent::Unknown => {}
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
