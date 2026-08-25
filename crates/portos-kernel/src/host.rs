//! Plugin host: spawn a plugin as a plain child process.
//!
//! ## Documentation
//! TODO:
//! M0 downgrade: sandbox yet. 
//! Hand it a private UDS, speak length-prefixed JSON frames,
//! pass artifact fds with SCM_RIGHTS. Zero capabilities at start.

use crate::KernelError;
use portos_proto::{fdpass, frame};
use serde_json::{Value, json};
use std::os::fd::RawFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

pub struct PluginConn {
    pub name: String,
    child: Child,
    stream: UnixStream,
    sock_path: PathBuf,
}

impl PluginConn {
    /// Spawn `bin` and wait for it to connect and say hello.
    pub fn spawn(bin: &Path, sock_dir: &Path) -> Result<PluginConn, KernelError> {
        std::fs::create_dir_all(sock_dir)?;
        let sock_path = sock_dir.join(format!("plugin-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&sock_path);
        let listener = UnixListener::bind(&sock_path)?;

        let child = Command::new(bin)
            .env("PORTOS_PLUGIN_SOCK", &sock_path)
            .spawn()?;

        let (stream, _addr) = listener.accept()?;
        let mut s = stream.try_clone()?;
        let hello =
            frame::read_frame(&mut s).map_err(|e| KernelError::Corrupt(format!("hello: {e}")))?;
        let name = hello["hello"]["name"].as_str().unwrap_or("?").to_string();

        Ok(PluginConn {
            name,
            child,
            stream,
            sock_path,
        })
    }

    /// Call a verb; if `fd` is given, it is passed as an SCM_RIGHTS
    /// attachment immediately after the frame (frame carries "fds": 1).
    pub fn call(
        &mut self,
        verb: &str,
        args: Value,
        fd: Option<RawFd>,
    ) -> Result<Value, KernelError> {
        let msg = json!({
            "op": "call",
            "verb": verb,
            "args": args,
            "fds": fd.map(|_| 1).unwrap_or(0),
        });
        frame::write_frame(&mut self.stream, &msg)
            .map_err(|e| KernelError::Corrupt(format!("call write: {e}")))?;
        if let Some(fd) = fd {
            fdpass::send_fds(&self.stream, &[fd])
                .map_err(|e| KernelError::Corrupt(format!("fd send: {e}")))?;
        }
        let resp = frame::read_frame(&mut self.stream)
            .map_err(|e| KernelError::Corrupt(format!("call read: {e}")))?;
        if let Some(err) = resp.get("err").and_then(|e| e.as_str()) {
            return Err(KernelError::Denied(format!("plugin error: {err}")));
        }
        Ok(resp.get("ok").cloned().unwrap_or(Value::Null))
    }

    pub fn shutdown(mut self) {
        let _ = frame::write_frame(&mut self.stream, &json!({"op": "shutdown"}));
        let _ = self.child.wait();
        let _ = std::fs::remove_file(&self.sock_path);
    }
}

impl Drop for PluginConn {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = std::fs::remove_file(&self.sock_path);
    }
}
