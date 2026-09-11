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

use portos_proto::ids::{IdError, PluginName, SubId, Topic, Verb};
use portos_proto::wire::{
    self, ChannelRole, ClientOp, EmitReply, Grant, GrantsReply, Hello, HelloFrame, Payload,
    PutReply, ReadReply, Reply, ServeMsg, SubscribeReply, ToolMeta, UnsubscribeReply,
};
use portos_proto::{ABI_VERSION, ArtifactMeta, Label, chunk, frame};
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

/// What a plugin declares about itself at startup.
pub struct Plugin<'a> {
    pub name: &'a str,
    pub verbs: &'a [&'a str],
    /// Per-verb metadata the kernel joins into `grants` introspection, so a
    /// caller holding the capability gets a ready-made tool definition.
    pub tools: BTreeMap<&'a str, ToolMeta>,
}

impl<'a> Plugin<'a> {
    pub fn new(name: &'a str, verbs: &'a [&'a str]) -> Plugin<'a> {
        Plugin {
            name,
            verbs,
            tools: BTreeMap::new(),
        }
    }

    pub fn with_tools(mut self, tools: BTreeMap<&'a str, ToolMeta>) -> Plugin<'a> {
        self.tools = tools;
        self
    }

    fn hello(&self, role: ChannelRole, token: &str) -> Result<HelloFrame, IdError> {
        let verbs = self
            .verbs
            .iter()
            .map(|v| Verb::parse(v))
            .collect::<Result<Vec<_>, _>>()?;
        let tools = if self.tools.is_empty() {
            None
        } else {
            Some(
                self.tools
                    .iter()
                    .map(|(v, m)| Ok((Verb::parse(v)?, m.clone())))
                    .collect::<Result<BTreeMap<_, _>, IdError>>()?,
            )
        };
        let serving = role == ChannelRole::Serve;
        Ok(HelloFrame {
            hello: Hello {
                name: PluginName::parse(self.name)?,
                abi: ABI_VERSION.to_string(),
                role,
                token: token.to_string(),
                verbs: if serving { verbs } else { Vec::new() },
                channels: serving.then(|| vec![ChannelRole::Client, ChannelRole::Events]),
                tools: if serving { tools } else { None },
            },
        })
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
pub fn serve<F, G>(plugin: Plugin<'_>, mut on_call: F, mut on_event: G) -> std::io::Result<()>
where
    F: FnMut(&Verb, &Payload, &Arc<KernelClient>) -> CallResult,
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

    loop {
        let bytes = match frame::read_bytes(&mut rd) {
            Ok(b) => b,
            Err(_) => return Ok(()), // kernel went away; exit quietly
        };
        match ServeMsg::from_slice(&bytes) {
            Ok(ServeMsg::Shutdown) | Err(_) => return Ok(()),
            Ok(ServeMsg::Call(call)) => {
                let reply = match on_call(&call.verb, &call.args, &client) {
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
