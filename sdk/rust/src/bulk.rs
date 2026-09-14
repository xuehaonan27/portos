//! A result that might be large: stored when it is, with a preview left
//! behind.
//!
//! The *shape* is `portos_abi::bulk::Bulk`, at the boundary, because callers
//! parse it too. The *decision* is here, made once for every verb whose
//! driver says its reply — or which fields of it — may be bulky: a handler
//! returns its text as `Bulk::Inline` or its document as itself, and never
//! sees the line. Every driver that answered with text faced this and each
//! re-invented it before it moved; now none can forget it, because the
//! document says where the line applies and the SDK applies it.

use crate::{CallError, KernelClient};
use portos_abi::Label;
use portos_abi::bulk::take_chars;
use portos_abi::driver::BulkSpec;
use portos_abi::ids::Verb;
use portos_abi::wire::Payload;
use std::collections::BTreeMap;

pub use portos_abi::bulk::{Bulk, INLINE_MAX, PREVIEW_CHARS};

/// How much of the arguments a stored result remembers as provenance.
const ARGS_LABEL_CHARS: usize = 256;

/// Apply a verb's bulk declaration to its reply.
pub(crate) fn spill(
    client: &KernelClient,
    spec: &BulkSpec,
    verb: &Verb,
    args: &Payload,
    reply: Payload,
) -> Result<Payload, CallError> {
    // An artifact should still say where it came from once it has outlived
    // the call that made it: the verb, and the arguments it was given.
    let provenance = || {
        let mut label = Label::default();
        label.integ.insert(verb.as_str().to_string());
        label.integ.insert(format!(
            "args:{}",
            take_chars(args.as_raw(), ARGS_LABEL_CHARS)
        ));
        label
    };
    if spec.fields.is_empty() {
        return spill_one(client, spec, &provenance, reply);
    }
    let mut fields: BTreeMap<String, Payload> = reply.parse().map_err(|e| {
        CallError::from(format!(
            "{verb}: reply is not an object with fields {:?}: {e}",
            spec.fields
        ))
    })?;
    for field in &spec.fields {
        if let Some(value) = fields.remove(field) {
            fields.insert(field.clone(), spill_one(client, spec, &provenance, value)?);
        }
    }
    Payload::of(&fields).map_err(|e| CallError::from(e.to_string()))
}

fn spill_one(
    client: &KernelClient,
    spec: &BulkSpec,
    provenance: &dyn Fn() -> Label,
    value: Payload,
) -> Result<Payload, CallError> {
    if matches!(value.parse::<Bulk>(), Ok(Bulk::Stored { .. })) {
        return Ok(value);
    }
    // Text is carried inline as `{text}` and its bytes are the text — what
    // `artifact::read` and a path handed to `grep` must see. Anything else
    // is a document, and its bytes are its JSON.
    let owned;
    let bytes: &str = match inline_text(&value) {
        Some(text) => {
            owned = text;
            &owned
        }
        None => value.as_raw(),
    };
    if bytes.len() <= INLINE_MAX {
        return Ok(value);
    }
    let meta = client.put(bytes.as_bytes(), &spec.r#type, Some(provenance()))?;
    Payload::of(&Bulk::Stored {
        handle: meta.id,
        size: meta.size,
        preview: take_chars(bytes, PREVIEW_CHARS),
    })
    .map_err(|e| CallError::from(e.to_string()))
}

/// The text of a value that is exactly `{text}`.
fn inline_text(value: &Payload) -> Option<String> {
    let fields: BTreeMap<String, Payload> = value.parse().ok()?;
    if fields.len() != 1 {
        return None;
    }
    fields.get("text")?.parse().ok()
}
