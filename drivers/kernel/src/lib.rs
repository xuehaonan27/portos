//! The `kernel::*` driver interface: starting, stopping and listing plugins.
//!
//! Stated outside the kernel for the same reason every driver is: the
//! interface is what a driver *is*, not who implements it. The kernel is
//! this one's first implementation, under the instance name `kernel`; a
//! launcher for a form the kernel does not know — a container, a microVM —
//! is its second, and gets its types from here rather than from the kernel.
//! The CLI's launcher builds a `LaunchSpec` from `portos.json` with these
//! same types, so the file and the verb cannot disagree about what starting
//! a plugin means.

use portos_abi::ids::{PluginName, Topic, Verb};
use portos_abi::wire::{Payload, ToolMeta};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

pub static SPAWN: LazyLock<Verb> =
    LazyLock::new(|| Verb::parse("kernel::spawn").expect("constant verb"));
pub static STOP: LazyLock<Verb> =
    LazyLock::new(|| Verb::parse("kernel::stop").expect("constant verb"));
pub static PLUGINS: LazyLock<Verb> =
    LazyLock::new(|| Verb::parse("kernel::plugins").expect("constant verb"));

/// Published by whoever launches a set of plugins, once every one of them
/// is up — at startup and again after each reload.
///
/// A plugin's `needs` say what *it* cannot work without; they cannot say
/// that everything the operator listed has started, because only the
/// launcher knows the list. A front end that wants the whole set — the
/// tools, the other renderers — before it opens a session waits for this
/// rather than racing the launcher that started it.
pub static UP: LazyLock<Topic> =
    LazyLock::new(|| Topic::parse("kernel::up").expect("constant topic"));

/// How to start a plugin, and what it may do once it is up.
///
/// The same shape whether it comes from `portos.json` at boot or from a
/// `kernel::spawn` call mid-session: starting a plugin is one operation with
/// one description, not two code paths that drift.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct LaunchSpec {
    /// An executable in the CAS.
    ///
    /// A plugin named this way is a **thing** rather than a location: the
    /// same bytes get the same id on every machine, the spec carries the
    /// name and never the bytes, and what ran is checkable afterwards. It
    /// is also what lets a spec cross a boundary at all — a path means
    /// nothing inside a container, on another node, or to the next run of
    /// this one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    /// A path on this host: the escape hatch, for things already installed
    /// (`node`, a system tool) and for the bootstrap.
    ///
    /// **Not reproducible and not portable.** The file can change under a
    /// spec that names it, and nothing that names one can be shipped
    /// anywhere. Kept because pretending otherwise would be worse, and
    /// labelled so it does not read as equivalent to the line above.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin: Option<String>,
    /// A tar archive in the CAS: everything the plugin needs beyond one
    /// executable.
    ///
    /// A second axis from `artifact`/`bin`, not an alternative to them. Those
    /// say **what runs**; this says **what files it has**. A JS plugin needs
    /// both and they come from different places — the script and its
    /// dependencies from here, the interpreter from the host — which is why
    /// folding them into one field would not have worked.
    ///
    /// It is unpacked once and becomes the process's working directory, so
    /// relative paths inside it mean what they meant when it was built.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bundle: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// What this plugin should be, in its own vocabulary. Opaque here — the
    /// kernel hands it over and cannot read it, like any other payload.
    ///
    /// It lives in the spec rather than in a file the plugin finds for
    /// itself, so that changing it changes the spec: a reload then re-plugs
    /// that one driver without anyone having to declare which files matter.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<Payload>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// The identifier this instance runs under.
    ///
    /// Two instances of one driver — two browsers — need two, and the
    /// launcher is the one who knows there are two, so it names them; what a
    /// plugin calls itself is only the name it gets when nobody says
    /// otherwise. A plugin that was named and answers to something else is
    /// refused: a launcher that cannot trust the name it gave cannot address
    /// what it started.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Minted once the plugin is up and has declared its name.
    #[serde(default)]
    pub grants: Vec<GrantSpec>,
    /// How it runs. What a plugin *is* and how it runs are two axes; this is
    /// the second one, and the kernel knows only the two it can do without
    /// learning a domain — a child process, optionally inside a cgroup.
    /// Containers and VMs belong to drivers.
    #[serde(default)]
    pub form: Form,
}

impl LaunchSpec {
    pub fn from_path(bin: impl Into<String>) -> LaunchSpec {
        LaunchSpec {
            bin: Some(bin.into()),
            ..Default::default()
        }
    }

    pub fn from_artifact(id: impl Into<String>) -> LaunchSpec {
        LaunchSpec {
            artifact: Some(id.into()),
            ..Default::default()
        }
    }

    /// A bundled plugin: its files from `bundle`, run by `bin` — which is
    /// either a name to find on PATH (`node`) or, if it contains a
    /// separator, a file inside the bundle.
    pub fn from_bundle(bundle: impl Into<String>, bin: impl Into<String>) -> LaunchSpec {
        LaunchSpec {
            bundle: Some(bundle.into()),
            bin: Some(bin.into()),
            ..Default::default()
        }
    }
}

/// The runtime forms the kernel implements itself.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Form {
    /// A child process leading its own process group. Always available, and
    /// the bootstrap that must never depend on anything else being present.
    Bare,
    /// The same child, inside a cgroup of its own — a boundary it cannot
    /// leave with `setsid`, and one that leaves a findable directory if this
    /// runtime dies without teardown. Falls back to [`Form::Bare`] where
    /// cgroup v2 is not available or not writable.
    #[default]
    Cgroup,
}

/// A capability to mint. `subject` defaults to the plugin being started,
/// which is the common case: a driver being granted what it needs.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GrantSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    pub resource: String,
    #[serde(default)]
    pub verbs: BTreeSet<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SpawnReply {
    pub name: PluginName,
    pub verbs: Vec<Verb>,
    /// Present when it started but is waiting on something. An agent that
    /// spawned a plugin and got this back knows to start what it needs
    /// rather than wondering why the new verbs are not there.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmet: Vec<Verb>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StopArgs {
    pub name: PluginName,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StopReply {
    pub stopped: bool,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct PluginsArgs {}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginsReply {
    pub plugins: Vec<PluginInfo>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PluginInfo {
    pub name: PluginName,
    /// What it was started from: exactly one of these, as in `LaunchSpec`.
    /// This is the implementation, reported so an operator can tell two
    /// instances of one driver apart by more than their names. It is never
    /// a routing key: a caller that chose an implementation would be finding
    /// a plugin by name, which is what replaceability forbids.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub artifact: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bin: Option<String>,
    pub verbs: Vec<Verb>,
    /// What it is still waiting for. Empty means it is answering.
    ///
    /// The diagnosis lives here rather than in the route table because the
    /// table has one job — does this name resolve — and a third state in it
    /// would be a special case for every reader of it.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unmet: Vec<Verb>,
}

/// What each verb says about itself: the tool a caller granted it is shown.
/// A driver describes its verbs in its hello; the kernel has no hello, so
/// the description is here — and a second implementation says the same
/// thing by construction.
pub fn tools() -> BTreeMap<Verb, ToolMeta> {
    let tool = |verb: &Verb, description: &str, schema: serde_json::Value| {
        (
            verb.clone(),
            ToolMeta {
                description: description.to_string(),
                schema: Payload::of(&schema).ok(),
            },
        )
    };
    BTreeMap::from([
        tool(
            &SPAWN,
            "Start a new plugin and grant it what it needs. Name it with \
             either `artifact` (an executable in the CAS) or `bin` (a path \
             on this host) — exactly one. The plugin's verbs \
             become available to anyone granted them — including, if the grants \
             say so, you — from your next turn onward. Use this to add a \
             capability the system does not currently have.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "artifact": {"type": "string", "description":
                        "id of an executable stored in the CAS — the portable \
                         way to name a plugin"},
                    "bin": {"type": "string", "description":
                        "path to an executable on this host; use it for things \
                         already installed, such as `node`"},
                    "args": {"type": "array", "items": {"type": "string"}},
                    "env": {"type": "object"},
                    "grants": {
                        "type": "array",
                        "description": "capabilities to mint once it is up; \
                                        `subject` defaults to the new plugin",
                        "items": {
                            "type": "object",
                            "properties": {
                                "subject": {"type": "string"},
                                "resource": {"type": "string"},
                                "verbs": {"type": "array", "items": {"type": "string"}},
                            },
                            "required": ["resource", "verbs"],
                        },
                    },
                },
                // Exactly one of `artifact` and `bin`, which a JSON
                // schema cannot say and the kernel checks instead.
                "required": [],
            }),
        ),
        tool(
            &STOP,
            "Stop a running plugin. Its verbs stop being routed and everything \
             it was granted is revoked; anything it started is collected too.",
            serde_json::json!({
                "type": "object",
                "properties": {"name": {"type": "string"}},
                "required": ["name"],
            }),
        ),
        tool(
            &PLUGINS,
            "List the plugins currently running: what each was started from, \
             the verbs each answers, and what each is still waiting for.",
            serde_json::json!({"type": "object", "properties": {}}),
        ),
    ])
}
