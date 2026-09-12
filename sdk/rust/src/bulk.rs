//! The shape a verb answers with when its result might be large.
//!
//! Part of the SDK rather than a crate of its own, because it is not a
//! family: it has no verbs and no domain, it is simply how *any* verb
//! replies when the answer might not belong in a conversation. A plugin
//! author should not have to find a second library to learn that.
//!
//! This is a contract, not a convenience. `modeld` already promises the model
//! a shape in the `artifact::read` tool description — *"large tool results
//! arrive as {handle, preview}: pass the handle here to read the full
//! content"* — so the shape is something the model has been told about, and
//! three drivers each inventing their own version of it would be a cost the
//! model pays, in guessing. The JS twin of this file is
//! `plugins/browser/src/sink.js`, which established the shape.
//!
//! The rule it encodes is the health metric of the whole architecture: bytes
//! the model will actually read go into the context; bytes it merely *might*
//! read go into the CAS and are named by a handle. A driver never has to
//! decide this twice.

use crate::{KernelClient, PluginError};
use portos_abi::Label;
use serde::{Deserialize, Serialize};

/// A verb result that is either small enough to read or big enough to store.
///
/// Untagged on the wire, so each variant is exactly the object the browser
/// driver already returns — a model that learned one shape did not learn two.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Bulk {
    Stored {
        handle: String,
        size: u64,
        /// The leading bytes, so the model can tell whether it wants the rest.
        preview: String,
    },
    Inline {
        text: String,
    },
}

/// Where the line between context and data is drawn.
///
/// Constants rather than configuration: the one knob of this kind that
/// exists (`WORKSHOP_SINK_INLINE_MAX` in the browser driver) has only ever
/// been turned by a test, and a test can just produce a bigger result.
#[derive(Clone, Copy, Debug)]
pub struct Sink {
    pub inline_max: usize,
    pub preview_chars: usize,
}

pub const INLINE_MAX: usize = 16 * 1024;
pub const PREVIEW_CHARS: usize = 2048;

impl Default for Sink {
    fn default() -> Sink {
        Sink {
            inline_max: INLINE_MAX,
            preview_chars: PREVIEW_CHARS,
        }
    }
}

impl Sink {
    /// Deliver `text` to the model: inline if small, otherwise streamed into
    /// the CAS with a bounded preview left behind.
    ///
    /// `labels` carries provenance the way the browser driver's `web:<origin>`
    /// does — an artifact should still say where it came from once it has
    /// outlived the call that made it.
    pub fn deliver(
        &self,
        client: &KernelClient,
        text: &str,
        content_type: &str,
        labels: Option<Label>,
    ) -> Result<Bulk, PluginError> {
        if text.len() <= self.inline_max {
            return Ok(Bulk::Inline {
                text: text.to_string(),
            });
        }
        let meta = client.put(text.as_bytes(), content_type, labels)?;
        Ok(Bulk::Stored {
            handle: meta.id.clone(),
            size: meta.size,
            preview: take_chars(text, self.preview_chars),
        })
    }
}

/// Cut on a character boundary: a preview sliced mid-UTF-8 would panic, and
/// slicing by bytes is the obvious way to write this wrong.
fn take_chars(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((end, _)) => s[..end].to_string(),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two wire shapes, which the browser driver already emits and the
    /// model has already been told about.
    #[test]
    fn the_two_shapes_are_what_the_model_was_promised() {
        let inline = Bulk::Inline { text: "hi".into() };
        assert_eq!(serde_json::to_string(&inline).unwrap(), r#"{"text":"hi"}"#);
        let stored = Bulk::Stored {
            handle: "blake3:ab".into(),
            size: 99,
            preview: "hi".into(),
        };
        assert_eq!(
            serde_json::to_string(&stored).unwrap(),
            r#"{"handle":"blake3:ab","size":99,"preview":"hi"}"#
        );
        // Untagged parsing must not confuse them.
        assert_eq!(
            serde_json::from_str::<Bulk>(r#"{"text":"hi"}"#).unwrap(),
            inline
        );
        assert_eq!(
            serde_json::from_str::<Bulk>(r#"{"handle":"blake3:ab","size":99,"preview":"hi"}"#)
                .unwrap(),
            stored
        );
    }

    #[test]
    fn a_preview_never_splits_a_character() {
        let s = "日本語テキスト";
        assert_eq!(take_chars(s, 3), "日本語");
        assert_eq!(take_chars(s, 100), s);
    }
}
