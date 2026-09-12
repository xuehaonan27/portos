//! Delivering a result that might be large: inline if small, otherwise into
//! the CAS with a preview left behind.
//!
//! The *shape* is `portos_abi::bulk::Bulk`, at the boundary, because callers
//! parse it too. What lives here is the decision — and the reason it lives in
//! the SDK is that every driver that answers with text faces it, and each
//! re-invented it before it moved. A plugin author should not have to find a
//! second library to learn that.

use crate::{KernelClient, PluginError};
use portos_abi::Label;
use portos_abi::bulk::take_chars;
pub use portos_abi::bulk::{Bulk, INLINE_MAX, PREVIEW_CHARS};

/// Where the line between context and data is drawn.
///
/// Constants rather than configuration: the one knob of this kind that
/// ever existed was only turned by a test, and a test can just produce a
/// bigger result.
#[derive(Clone, Copy, Debug)]
pub struct Sink {
    pub inline_max: usize,
    pub preview_chars: usize,
}

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
