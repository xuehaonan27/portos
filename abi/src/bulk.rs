//! The shape a verb answers with when its result might be large.
//!
//! At the boundary rather than in the SDK because a *caller* parses it too:
//! `shell::run` answers `{stdout: Bulk, …}`, and the driver interface that
//! says so cannot depend on plugin plumbing. The SDK keeps the mechanism —
//! deciding, storing, previewing — and this is only the shape.
//!
//! It is a contract, not a convenience. `modeld` promises the model a shape
//! in the `artifact::read` tool description — *"large tool results arrive as
//! {handle, preview}: pass the handle here to read the full content"* — so
//! the shape is something the model has been told about, and three drivers
//! each inventing their own version of it would be a cost the model pays,
//! in guessing. The JS twin is `deliver()` in `plugins/browser/src/plugin.js`.
//!
//! The rule it encodes is the health metric of the whole architecture: bytes
//! the model will actually read go into the context; bytes it merely *might*
//! read go into the CAS and are named by a handle. A driver never has to
//! decide this twice.

use serde::{Deserialize, Serialize};

/// A verb result that is either small enough to read or big enough to store.
///
/// Untagged on the wire, so each variant is exactly the object a caller sees
/// — a model that learned one shape did not learn two.
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

/// Where the line between context and data is drawn. Constants rather than
/// configuration: a test that wants the stored shape produces a bigger result.
pub const INLINE_MAX: usize = 16 * 1024;
pub const PREVIEW_CHARS: usize = 2048;

/// Cut on a character boundary: a preview sliced mid-UTF-8 would panic, and
/// slicing by bytes is the obvious way to write this wrong.
pub fn take_chars(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((end, _)) => s[..end].to_string(),
        None => s.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two wire shapes the model has already been told about.
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
