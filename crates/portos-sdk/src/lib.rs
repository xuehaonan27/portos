//! portos-sdk: the plugin side of the kernel IPC. A plugin is a plain binary
//! that connects to $PORTOS_PLUGIN_SOCK, says hello, and serves verb calls.
//! Plugins start with ZERO capabilities; everything they can do is what the
//! kernel routes to them.

use portos_proto::{fdpass, frame};
use serde_json::{json, Value};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;

pub struct CallCtx {
    /// fds attached to this call (e.g. a read-only artifact descriptor).
    pub fds: Vec<OwnedFd>,
}

/// Serve verbs until the kernel says shutdown. `handler` returns Ok(json)
/// or Err(message).
pub fn serve<F>(name: &str, mut handler: F) -> std::io::Result<()>
where
    F: FnMut(&str, &Value, CallCtx) -> Result<Value, String>,
{
    let path = std::env::var("PORTOS_PLUGIN_SOCK")
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::NotFound, "PORTOS_PLUGIN_SOCK unset"))?;
    let stream = UnixStream::connect(path)?;
    let mut rd = stream.try_clone()?;
    let mut wr = stream.try_clone()?;

    frame::write_frame(&mut wr, &json!({"hello": {"name": name, "abi": "0.1"}}))
        .map_err(io_err)?;

    loop {
        let msg = match frame::read_frame(&mut rd) {
            Ok(m) => m,
            Err(_) => return Ok(()), // kernel went away; exit quietly
        };
        match msg["op"].as_str() {
            Some("shutdown") | None => return Ok(()),
            Some("call") => {
                let verb = msg["verb"].as_str().unwrap_or("");
                let nfds = msg["fds"].as_u64().unwrap_or(0) as usize;
                let fds = if nfds > 0 {
                    fdpass::recv_fds(&stream, nfds).map_err(|e| {
                        std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
                    })?
                } else {
                    Vec::new()
                };
                let resp = match handler(verb, &msg["args"], CallCtx { fds }) {
                    Ok(v) => json!({"ok": v}),
                    Err(e) => json!({"err": e}),
                };
                frame::write_frame(&mut wr, &resp).map_err(io_err)?;
            }
            Some(other) => {
                frame::write_frame(&mut wr, &json!({"err": format!("unknown op {other}")}))
                    .map_err(io_err)?;
            }
        }
    }
}

fn io_err(e: frame::FrameError) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::Other, e.to_string())
}
