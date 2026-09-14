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

use portos_model_api as model;
use portos_sdk::{Job, Plugin};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

fn main() -> std::io::Result<()> {
    let next = Arc::new(AtomicU64::new(1));
    portos_sdk::serve(
        Plugin::new("portos-model-echo")
            .implement(
                &model::DRIVER,
                &model::START,
                move |_: model::StartArgs, _client| {
                    let session = model::SessionId::new(next.fetch_add(1, Ordering::SeqCst));
                    Ok(model::StartReply { session })
                },
            )
            .implement_accepted(
                &model::DRIVER,
                &model::SEND,
                |send: model::SendArgs, _client| {
                    let job: Job = Box::new(move |accepted| {
                        for event in [
                            model::SessionEvent::Delta {
                                text: send.text.clone(),
                            },
                            model::SessionEvent::Done { text: send.text },
                        ] {
                            let _ = accepted.emit(&event);
                        }
                    });
                    Ok((model::SendReply {}, job))
                },
            )
            .implement(
                &model::DRIVER,
                &model::SESSIONS,
                |_: model::SessionsArgs, _client| Ok(model::SessionsReply::default()),
            )
            .implement(
                &model::DRIVER,
                &model::CANCEL,
                |_: model::CancelArgs, _client| Ok(model::CancelReply { cancelled: false }),
            )
            .implement(&model::DRIVER, &model::END, |_: model::EndArgs, _client| {
                Ok(model::EndReply { ended: true })
            }),
        |_topic, _data| {},
    )
}
