//! A model driver that answers with what it was told.
//!
//! The second implementation of the `model::*` driver, and it exists to keep
//! the first one honest: whoever drives a model driver addresses it by verb
//! and cannot tell whose implementation answered, so `portos run` is tested
//! against this one in `cli/tests/run.rs` exactly as it runs against
//! `modeld`. It is also the way to watch the whole runtime work with no
//! provider, no key and no network — list it in `portos.json` and type.
//!
//! A turn is accepted, not awaited, whoever runs it: `send` returns at once
//! and the text comes back on the session topic as one delta and a `done`.

use portos_abi::wire::Payload;
use portos_model_api as model;
use portos_sdk::Plugin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

fn main() -> std::io::Result<()> {
    let next = Arc::new(AtomicU64::new(1));
    portos_sdk::serve(
        Plugin::new("portos-model-echo")
            .verb(model::START.as_str(), move |_args, _client| {
                let session = model::SessionId::new(next.fetch_add(1, Ordering::SeqCst));
                Ok(Payload::of(&model::StartReply { session })?)
            })
            .verb(model::SEND.as_str(), |args, client| {
                let send: model::SendArgs = args.parse()?;
                let client = client.clone();
                std::thread::spawn(move || {
                    let topic = send.session.topic();
                    for event in [
                        model::SessionEvent::Delta {
                            text: send.text.clone(),
                        },
                        model::SessionEvent::Done { text: send.text },
                    ] {
                        if let Ok(data) = Payload::of(&event) {
                            let _ = client.emit(&topic, data);
                        }
                    }
                });
                Ok(Payload::of(&model::SendReply {})?)
            })
            .verb(model::SESSIONS.as_str(), |_args, _client| {
                Ok(Payload::of(&model::SessionsReply::default())?)
            })
            .verb(model::CANCEL.as_str(), |_args, _client| {
                Ok(Payload::of(&model::CancelReply { cancelled: false })?)
            })
            .verb(model::END.as_str(), |_args, _client| {
                Ok(Payload::of(&model::EndReply { ended: true })?)
            }),
        |_topic, _data| {},
    )
}
