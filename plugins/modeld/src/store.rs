//! Where a conversation lives when nothing is running.
//!
//! Two halves, deliberately kept apart, for the reason the whole system is
//! built around:
//!
//! - the **transcript** is data. It grows without bound, so it goes into the
//!   CAS and is named by a handle.
//! - the **index** is control. One line per session — the handle, a count, a
//!   timestamp, and enough of the opening message to recognise it.
//!
//! `sessions.json` therefore stays small no matter how long the
//! conversations get, which is the same rule the model's own context obeys.
//! It is also why listing sessions costs nothing: nobody reads a transcript
//! to find out that it exists.
//!
//! Content addressing makes one thing free: a transcript that did not change
//! is the same artifact, so re-saving an untouched session stores nothing
//! new.

use crate::core::Session;
use portos_model_api::{SessionIndex, SessionRecord};
use portos_sdk::{KernelClient, PluginError};
use std::path::{Path, PathBuf};

/// How much of the opening line to keep for a listing.
const TITLE_CHARS: usize = 72;

pub struct Store {
    dir: PathBuf,
}

impl Store {
    /// `None` when the driver has no directory to write to — then sessions
    /// live only as long as the process, which is what the tests that do not
    /// care about persistence get.
    pub fn open(dir: Option<&Path>) -> Option<Store> {
        let dir = dir?;
        std::fs::create_dir_all(dir).ok()?;
        Some(Store {
            dir: dir.to_path_buf(),
        })
    }

    pub fn load(&self) -> SessionIndex {
        SessionIndex::read(&self.dir)
    }

    /// Write the transcript to the CAS and record where it went.
    pub fn save(
        &self,
        client: &KernelClient,
        session_id: &str,
        session: &Session,
    ) -> Result<(), PluginError> {
        let bytes = serde_json::to_vec(session)?;
        let meta = client.put(bytes.as_slice(), "portos/transcript", None)?;
        let mut index = self.load();
        index.sessions.insert(
            session_id.to_string(),
            SessionRecord {
                artifact: meta.id,
                turns: session.turns(),
                updated_at: now_unix(),
                title: session.title(TITLE_CHARS),
            },
        );
        self.write(&index)
    }

    pub fn restore(
        &self,
        client: &KernelClient,
        session_id: &str,
    ) -> Result<Option<Session>, PluginError> {
        let Some(entry) = self.load().sessions.get(session_id).cloned() else {
            return Ok(None);
        };
        let bytes = client.read_bytes(&entry.artifact)?;
        Ok(Some(serde_json::from_slice(&bytes)?))
    }

    /// Replace the index in one step. A half-written index would lose every
    /// conversation at once, which is too much to risk on a crash during a
    /// write that takes microseconds to do safely.
    fn write(&self, index: &SessionIndex) -> Result<(), PluginError> {
        let path = SessionIndex::path_in(&self.dir);
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(index)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
