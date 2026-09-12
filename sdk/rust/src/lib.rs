//! portos-sdk: the plugin side of the kernel IPC — ABI v2.
//!
//! A plugin is a plain process that connects to `$PORTOS_PLUGIN_SOCK`
//! **twice**, authenticating each connection with `$PORTOS_PLUGIN_TOKEN`:
//! a `serve` connection on which it declares its verbs and answers kernel
//! calls, and a `client` connection through which it reaches the kernel —
//! `invoke` (call another plugin's verb, capability-checked kernel-side),
//! `emit`/`subscribe` (event bus), and `put`/`read` (artifact dereference as
//! chunked byte streams; payloads never ride inside JSON frames). A plugin
//! may also open an `events` connection so subscribed events keep arriving
//! while one of its own calls is in flight.
//!
//! Plugins start with ZERO capabilities; `invoke` succeeds only for verbs
//! the kernel has been told to grant this plugin.
//!
//! Arguments and results are [`Payload`] — opaque on the wire, typed the
//! moment a plugin parses one into a shape it owns. The kernel never does.

pub mod bulk;
pub mod config;
pub mod scope;

use portos_abi::ids::{IdError, PluginName, SubId, Topic, Verb};
use portos_abi::wire::{
    self, ChannelRole, ClientOp, EmitReply, Grant, GrantsReply, Hello, HelloFrame, LocateReply,
    Payload, PutReply, ReadReply, Reply, ServeMsg, SubscribeReply, ToolMeta, UnsubscribeReply,
};
use portos_abi::{ABI_VERSION, ArtifactMeta, Label, chunk, frame};
use portos_router::Router as _;
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};

/// What can go wrong talking to the kernel. Callers can tell a refusal
/// (no capability, no route) from a broken channel or a malformed payload,
/// which a single `String` never allowed.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("kernel refused: {0}")]
    Refused(String),
    #[error(transparent)]
    Frame(#[from] frame::FrameError),
    #[error("payload: {0}")]
    Payload(#[from] serde_json::Error),
    #[error(transparent)]
    Chunk(#[from] chunk::ChunkError),
    #[error(transparent)]
    Id(#[from] IdError),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

/// The error a verb handler reports. It becomes the `err` string of one
/// reply frame, so it is a message by construction; `?` still works from
/// anything that displays.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct CallError(String);

impl From<&str> for CallError {
    fn from(s: &str) -> CallError {
        CallError(s.to_string())
    }
}
impl From<String> for CallError {
    fn from(s: String) -> CallError {
        CallError(s)
    }
}
impl From<serde_json::Error> for CallError {
    fn from(e: serde_json::Error) -> CallError {
        CallError(e.to_string())
    }
}
impl From<PluginError> for CallError {
    fn from(e: PluginError) -> CallError {
        CallError(e.to_string())
    }
}
impl From<IdError> for CallError {
    fn from(e: IdError) -> CallError {
        CallError(e.to_string())
    }
}

pub type CallResult = Result<Payload, CallError>;

/// The plugin's connection to the kernel (the client channel). Safe to share
/// across threads; each operation holds the channel for one request/response
/// (chunk streams included), so requests never interleave.
pub struct KernelClient {
    stream: Mutex<UnixStream>,
}

impl KernelClient {
    fn request<T: DeserializeOwned>(&self, op: &ClientOp) -> Result<T, PluginError> {
        let mut s = self.stream.lock().unwrap();
        frame::write_frame(&mut *s, op)?;
        let reply: Reply<T> = frame::read_frame(&mut *s)?;
        reply.into_result().map_err(PluginError::Refused)
    }

    /// Call another plugin's verb through the kernel. The kernel checks this
    /// plugin's capabilities, audits, and routes.
    pub fn invoke(&self, verb: &Verb, args: Payload) -> Result<Payload, PluginError> {
        self.request(&ClientOp::Invoke(wire::Invoke {
            verb: verb.clone(),
            args,
        }))
    }

    /// Publish an event. Returns the number of subscribers it reached.
    pub fn emit(&self, topic: &Topic, data: Payload) -> Result<u64, PluginError> {
        let r: EmitReply = self.request(&ClientOp::Emit(wire::Emit {
            topic: topic.clone(),
            data,
        }))?;
        Ok(r.delivered)
    }

    /// Subscribe to a topic pattern. Matching events later arrive on the
    /// events channel and are handed to the plugin's event handler.
    pub fn subscribe(&self, pattern: &Topic) -> Result<SubId, PluginError> {
        let r: SubscribeReply = self.request(&ClientOp::Subscribe(wire::Subscribe {
            topic: pattern.clone(),
        }))?;
        Ok(r.sub)
    }

    /// Drop one of this plugin's subscriptions.
    pub fn unsubscribe(&self, sub: SubId) -> Result<bool, PluginError> {
        let r: UnsubscribeReply =
            self.request(&ClientOp::Unsubscribe(wire::Unsubscribe { sub }))?;
        Ok(r.removed)
    }

    /// What this plugin may invoke right now: live grants joined with the
    /// target verbs' advertised metadata, ready to become tool definitions.
    pub fn grants(&self) -> Result<Vec<Grant>, PluginError> {
        let r: GrantsReply = self.request(&ClientOp::Grants)?;
        Ok(r.grants)
    }

    /// Where an artifact's bytes are on disk, for handing to something that
    /// only speaks in paths.
    ///
    /// The file is read-only, so the path can be passed on without the
    /// artifact's immutability becoming a promise someone has to keep. Use
    /// this rather than [`read_bytes`] whenever the consumer is a program —
    /// moving a hundred megabytes through a socket to give `grep` a file is
    /// the mistake the whole data plane exists to avoid.
    ///
    /// [`read_bytes`]: KernelClient::read_bytes
    pub fn locate(&self, id: &str) -> Result<std::path::PathBuf, PluginError> {
        let r: LocateReply =
            self.request(&ClientOp::Locate(wire::Locate { id: id.to_string() }))?;
        Ok(std::path::PathBuf::from(r.path))
    }

    /// Take on a verb after connecting, so the kernel starts routing it.
    pub fn claim(&self, verb: &Verb, meta: ToolMeta) -> Result<(), PluginError> {
        let _: wire::ClaimReply = self.request(&ClientOp::Claim(wire::Claim {
            tools: BTreeMap::from([(verb.clone(), meta)]),
        }))?;
        Ok(())
    }

    /// Ingest a payload into the kernel CAS, streaming (never buffered whole,
    /// never inside a JSON frame).
    pub fn put<R: Read>(
        &self,
        mut r: R,
        content_type: &str,
        labels: Option<Label>,
    ) -> Result<ArtifactMeta, PluginError> {
        let mut s = self.stream.lock().unwrap();
        let op = ClientOp::Put(wire::Put {
            content_type: content_type.to_string(),
            labels,
        });
        frame::write_frame(&mut *s, &op)?;
        chunk::copy_into_chunks(&mut r, &mut *s)?;
        let reply: Reply<PutReply> = frame::read_frame(&mut *s)?;
        Ok(reply.into_result().map_err(PluginError::Refused)?.meta)
    }

    /// Dereference (a range of) an artifact into `w`. Returns bytes moved.
    pub fn read_to<W: Write>(
        &self,
        id: &str,
        offset: u64,
        len: Option<u64>,
        w: &mut W,
    ) -> Result<u64, PluginError> {
        let mut s = self.stream.lock().unwrap();
        let op = ClientOp::Read(wire::Read {
            id: id.to_string(),
            offset,
            len,
        });
        frame::write_frame(&mut *s, &op)?;
        let reply: Reply<ReadReply> = frame::read_frame(&mut *s)?;
        reply.into_result().map_err(PluginError::Refused)?;
        Ok(chunk::copy_from_chunks(&mut *s, w)?)
    }

    /// Convenience: dereference a whole artifact into memory. Only for
    /// payloads the caller knows are small; streaming is the norm.
    pub fn read_bytes(&self, id: &str) -> Result<Vec<u8>, PluginError> {
        let mut out = Vec::new();
        self.read_to(id, 0, None, &mut out)?;
        Ok(out)
    }
}

/// What a plugin declares about itself at startup: its name, and one entry
/// per verb holding **everything** about that verb.
///
/// One entry, because before this there were two tables. A driver wrote a
/// `match verb.short()` mapping verbs to functions, and separately a
/// `tools()` mapping the same verbs to descriptions and schemas, and kept
/// them aligned by hand. Missing from the first is a verb the model can see
/// and cannot call; missing from the second is one it can call and does not
/// know about. Neither mistake is visible at compile time, and both were
/// possible in every driver.
pub struct Plugin<'a> {
    pub name: &'a str,
    /// Shared with the [`Registrar`] handed to `on_ready`, because a plugin
    /// that mirrors somebody else only learns what it answers once it is
    /// running — and the serve loop is dispatching out of this at the time.
    ///
    /// Behind a lock rather than a cell because learning can take a while
    /// and must not hold up serving: a driver whose peer is down waits on
    /// its own thread, answering nothing, and takes its verbs on when the
    /// peer appears. That is why handlers must be `Send`.
    inner: Arc<Mutex<Served<'a>>>,
    needs: Vec<Verb>,
    on_ready: Option<ReadyHook<'a>>,
}

#[derive(Default)]
struct Served<'a> {
    table: VerbTable,
    handlers: Vec<Handler<'a>>,
}

/// Taking on verbs after the plugin is already running.
///
/// Handed to `on_ready`, which is the only moment where a plugin is
/// connected (so it can ask the world what it should answer) and not yet
/// answering (so nothing is mid-call while the table grows).
#[derive(Clone)]
pub struct Registrar<'a> {
    inner: Arc<Mutex<Served<'a>>>,
    client: Arc<KernelClient>,
}

impl<'a> Registrar<'a> {
    /// Answer this verb from now on, and tell the kernel so it routes it.
    ///
    /// The local table and the kernel's are changed together; a name in one
    /// and not the other is a plugin that is asked for something it cannot
    /// answer, or answers something nobody sends it.
    pub fn tool(
        &self,
        verb: &Verb,
        description: &str,
        schema: serde_json::Value,
        handler: impl FnMut(&Payload, &Arc<KernelClient>) -> CallResult + Send + 'a,
    ) -> Result<(), PluginError> {
        let meta = ToolMeta {
            description: description.to_string(),
            schema: Payload::of(&schema).ok(),
        };
        {
            let mut inner = self.inner.lock().unwrap();
            let id = HandlerId(inner.handlers.len());
            inner
                .table
                .add(verb.clone(), id, meta.clone())
                .map_err(|c| PluginError::Refused(c.to_string()))?;
            inner.handlers.push(Box::new(handler));
        }
        self.client.claim(verb, meta)
    }
}

type Handler<'a> = Box<dyn FnMut(&Payload, &Arc<KernelClient>) -> CallResult + Send + 'a>;

/// Run once, after every channel is up and before the first call is served.
type ReadyHook<'a> =
    Box<dyn FnOnce(&Registrar<'a>, &Arc<KernelClient>) -> Result<(), PluginError> + 'a>;

/// Which handler answers, by name rather than by being it — the split the
/// `router` driver asks for, and the reason the table can be a plain map
/// while the handlers stay mutable behind it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HandlerId(usize);

struct Entry {
    handler: HandlerId,
    meta: ToolMeta,
}

/// This plugin's implementation of the `router` driver: the verbs it answers.
///
/// It satisfies the one-answerer-per-family law by construction — every verb
/// here is answered by this plugin — so nothing enforces it. What it does
/// enforce is the law that was quietly broken before: a name claimed twice
/// used to be found by `find`, which silently took the first and left the
/// second unreachable with no complaint from anyone.
#[derive(Default)]
pub struct VerbTable {
    routes: BTreeMap<Verb, Entry>,
}

impl portos_router::Router for VerbTable {
    type Target = HandlerId;

    fn resolve(&self, verb: &Verb) -> Option<portos_router::Resolved<'_, HandlerId>> {
        self.routes.get(verb).map(|e| portos_router::Resolved {
            target: &e.handler,
            // A plugin's verbs are its own; nothing here crosses a boundary
            // that would call them something else.
            name: verb.clone(),
        })
    }

    fn add(
        &mut self,
        verb: Verb,
        target: HandlerId,
        meta: ToolMeta,
    ) -> Result<(), portos_router::Conflict> {
        if self.routes.contains_key(&verb) {
            return Err(portos_router::Conflict::Verb(verb));
        }
        self.routes.insert(
            verb,
            Entry {
                handler: target,
                meta,
            },
        );
        Ok(())
    }

    fn meta(&self, verb: &Verb) -> Option<&ToolMeta> {
        self.routes.get(verb).map(|e| &e.meta)
    }

    fn remove_where(&mut self, f: &dyn Fn(&Verb, &HandlerId) -> bool) -> usize {
        let before = self.routes.len();
        self.routes.retain(|v, e| !f(v, &e.handler));
        before - self.routes.len()
    }

    fn verbs(&self) -> Vec<Verb> {
        self.routes.keys().cloned().collect()
    }
}

impl<'a> Plugin<'a> {
    pub fn new(name: &'a str) -> Plugin<'a> {
        Plugin {
            name,
            inner: Arc::new(Mutex::new(Served::default())),
            needs: Vec::new(),
            on_ready: None,
        }
    }

    /// A verb this plugin answers, described so the model can use it.
    ///
    /// The description and schema are what a caller holding the capability
    /// is handed by `grants` introspection — so this one call is the whole
    /// of what the verb is: how to run it, and what to tell someone about
    /// it.
    pub fn tool(
        self,
        verb: &str,
        description: &str,
        schema: serde_json::Value,
        handler: impl FnMut(&Payload, &Arc<KernelClient>) -> CallResult + Send + 'a,
    ) -> Plugin<'a> {
        self.declare(
            verb,
            ToolMeta {
                description: description.to_string(),
                schema: Payload::of(&schema).ok(),
            },
            handler,
        )
    }

    /// A verb with nothing to say for itself: reachable, but not something a
    /// model is meant to discover. Test fixtures and internal plumbing.
    pub fn verb(
        self,
        verb: &str,
        handler: impl FnMut(&Payload, &Arc<KernelClient>) -> CallResult + Send + 'a,
    ) -> Plugin<'a> {
        self.declare(verb, ToolMeta::default(), handler)
    }

    fn declare(
        self,
        verb: &str,
        meta: ToolMeta,
        handler: impl FnMut(&Payload, &Arc<KernelClient>) -> CallResult + Send + 'a,
    ) -> Plugin<'a> {
        // Both failures here are programming mistakes, caught before the
        // plugin ever connects: a malformed verb would otherwise surface as a
        // hello the kernel refuses for reasons the author has to look up, and
        // a name claimed twice used to leave the second handler silently
        // unreachable.
        let verb = Verb::parse(verb).unwrap_or_else(|e| panic!("plugin {}: {e}", self.name));
        let mut inner = self.inner.lock().unwrap();
        let id = HandlerId(inner.handlers.len());
        if let Err(conflict) = inner.table.add(verb, id, meta) {
            panic!("plugin {}: {conflict}", self.name);
        }
        inner.handlers.push(Box::new(handler));
        drop(inner);
        self
    }

    /// A verb this plugin cannot work without.
    ///
    /// It takes a `&Verb` rather than a string, and the verb you pass should
    /// be a driver's own constant — `egress::HTTP`, not `"egress::http"`.
    /// That is the whole of the restriction and it is deliberate: a
    /// dependency is a statement in some driver's vocabulary, and a plugin
    /// that cannot name the driver it depends on is depending on a rumour.
    ///
    /// Needing something is not being allowed to use it. The operator still
    /// grants that, and a plugin that needs a verb it was never granted
    /// starts, becomes usable, and then fails the call with a capability
    /// error — which is the right error, from the right place.
    pub fn needs(mut self, verb: &Verb) -> Plugin<'a> {
        self.needs.push(verb.clone());
        self
    }

    /// Work to start once the channels are up and before the first call is
    /// served: subscribing, listening, pumping a link. A plugin whose job
    /// begins on its own has no call to hang it off, and deferring it to the
    /// first call means a plugin nobody calls never starts.
    pub fn on_ready(
        mut self,
        f: impl FnOnce(&Registrar<'a>, &Arc<KernelClient>) -> Result<(), PluginError> + 'a,
    ) -> Plugin<'a> {
        self.on_ready = Some(Box::new(f));
        self
    }

    fn hello(&self, role: ChannelRole, token: &str) -> Result<HelloFrame, IdError> {
        let serving = role == ChannelRole::Serve;
        let inner = self.inner.lock().unwrap();
        let tools: BTreeMap<Verb, ToolMeta> = inner
            .table
            .verbs()
            .into_iter()
            .filter_map(|v| {
                let meta = inner.table.meta(&v)?;
                (meta != &ToolMeta::default()).then(|| (v, meta.clone()))
            })
            .collect();
        Ok(HelloFrame {
            hello: Hello {
                name: PluginName::parse(self.name)?,
                abi: ABI_VERSION.to_string(),
                role,
                token: token.to_string(),
                verbs: if serving {
                    inner.table.verbs()
                } else {
                    Vec::new()
                },
                channels: serving.then(|| vec![ChannelRole::Client, ChannelRole::Events]),
                tools: if serving && !tools.is_empty() {
                    Some(tools)
                } else {
                    None
                },
                needs: if serving {
                    self.needs.clone()
                } else {
                    Vec::new()
                },
            },
        })
    }

    fn dispatch(&mut self, verb: &Verb, args: &Payload, client: &Arc<KernelClient>) -> CallResult {
        // Resolve, then reach — the handler is found by the name the table
        // gave back, not by the one that came in.
        let Some(id) = self
            .inner
            .lock()
            .unwrap()
            .table
            .resolve(verb)
            .map(|r| *r.target)
        else {
            // Unreachable through the kernel, which only routes what the
            // hello declared — so this is the kernel and the plugin
            // disagreeing, and saying so beats a default.
            return Err(CallError::from(format!(
                "not a verb of this plugin: {verb}"
            )));
        };
        let mut inner = self.inner.lock().unwrap();
        (inner.handlers[id.0])(args, client)
    }
}

/// Connect all channels, declare the plugin, and serve until the kernel says
/// shutdown (or goes away).
///
/// `on_call` answers kernel calls and may use the [`KernelClient`] it is
/// handed — shared as an `Arc` so a handler can move a clone into a
/// background thread. `on_event` receives subscribed events **on a dedicated
/// thread** fed by the events channel, so events keep flowing while a call
/// handler is blocked, which is what lets a handler await an event stream
/// mid-call.
pub fn serve<G>(mut plugin: Plugin<'_>, mut on_event: G) -> std::io::Result<()>
where
    G: FnMut(&Topic, &Payload) + Send + 'static,
{
    let sock = std::env::var("PORTOS_PLUGIN_SOCK").map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::NotFound, "PORTOS_PLUGIN_SOCK unset")
    })?;
    let token = std::env::var("PORTOS_PLUGIN_TOKEN").unwrap_or_default();
    let hello_for = |role| plugin.hello(role, &token).map_err(std::io::Error::other);

    let serve_stream = UnixStream::connect(&sock)?;
    let mut rd = serve_stream.try_clone()?;
    let mut wr = serve_stream.try_clone()?;
    handshake(&mut wr, &mut rd, &hello_for(ChannelRole::Serve)?)?;

    let client_stream = UnixStream::connect(&sock)?;
    {
        let mut crd = client_stream.try_clone()?;
        let mut cwr = client_stream.try_clone()?;
        handshake(&mut cwr, &mut crd, &hello_for(ChannelRole::Client)?)?;
    }
    let client = Arc::new(KernelClient {
        stream: Mutex::new(client_stream),
    });

    let events_stream = UnixStream::connect(&sock)?;
    {
        let mut erd = events_stream.try_clone()?;
        let mut ewr = events_stream.try_clone()?;
        handshake(&mut ewr, &mut erd, &hello_for(ChannelRole::Events)?)?;
    }
    std::thread::spawn(move || {
        let mut erd = events_stream;
        loop {
            let bytes = match frame::read_bytes(&mut erd) {
                Ok(b) => b,
                Err(_) => return, // kernel went away
            };
            if let Ok(ServeMsg::Event(ev)) = ServeMsg::from_slice(&bytes) {
                on_event(&ev.topic, &ev.data);
            }
        }
    });

    if let Some(ready) = plugin.on_ready.take() {
        let registrar = Registrar {
            inner: plugin.inner.clone(),
            client: client.clone(),
        };
        ready(&registrar, &client).map_err(std::io::Error::other)?;
    }

    loop {
        let bytes = match frame::read_bytes(&mut rd) {
            Ok(b) => b,
            Err(_) => return Ok(()), // kernel went away; exit quietly
        };
        match ServeMsg::from_slice(&bytes) {
            Ok(ServeMsg::Shutdown) | Err(_) => return Ok(()),
            Ok(ServeMsg::Call(call)) => {
                let reply = match plugin.dispatch(&call.verb, &call.args, &client) {
                    Ok(v) => Reply::Ok(v),
                    Err(e) => Reply::Err(e.to_string()),
                };
                frame::write_frame(&mut wr, &reply).map_err(std::io::Error::other)?;
            }
            // Events ride their own channel here; tolerate strays.
            Ok(ServeMsg::Event(_)) => {}
        }
    }
}

fn handshake<W: Write, R: Read>(wr: &mut W, rd: &mut R, h: &HelloFrame) -> std::io::Result<()> {
    frame::write_frame(wr, h).map_err(std::io::Error::other)?;
    let ack: Reply<Payload> = frame::read_frame(rd).map_err(std::io::Error::other)?;
    ack.into_result()
        .map_err(|e| std::io::Error::other(format!("hello rejected: {e}")))?;
    Ok(())
}
