//! The Anthropic Messages API **as a protocol**, not as a vendor.
//!
//! This backend speaks the Messages wire format to whatever endpoint it is
//! pointed at: Anthropic's own, a gateway, a proxy, a local server, or any
//! of the providers that expose an Anthropic-compatible surface. Nothing
//! here names a host, and there is no default that quietly picks one —
//! `base_url` and `model` are required, because a wrong guess about which
//! provider you meant is worse than an error at startup.
//!
//! What is configurable, and why each one has to be:
//!
//! | key | default | why it varies |
//! |---|---|---|
//! | `base_url` | **required** | the endpoint |
//! | `model` | **required** | no cross-provider default exists |
//! | `path` | `/v1/messages` | gateways mount the API under a prefix |
//! | `headers` | `{"anthropic-version": …}` | compatible endpoints differ on which protocol headers they want; an empty object sends none |
//! | `max_tokens` | 64000 | tuning, not identity |
//!
//! **Credentials are not in that table and cannot be.** Authentication is
//! the broker's job: its per-host `inject` rule decides the header name, so
//! `x-api-key` and `Authorization: Bearer` are both a matter of broker
//! config, and the key exists in no other process. A `headers` entry that
//! collides with an injected one is refused there by design.
//!
//! All traffic goes through the egress gateway. Responses stream via SSE:
//! broker chunks arrive at arbitrary byte boundaries, so parsing is
//! incremental. Assistant content blocks are accumulated **verbatim**
//! (thinking blocks and signatures included) into the provider-opaque `raw`
//! payload so multi-turn replay is faithful; the neutral parts are derived
//! from the same accumulator.

use crate::core::{
    Backend, EgressStream, Gateway, ModelError, Msg, Part, StopKind, ToolCall, TurnRequest,
    TurnResult, TurnSink,
};
use portos_abi::ids::Verb;
use portos_abi::wire::Payload;
use portos_egress_api::{EgressRequest, StreamEvent};
use serde_json::{Value, json};
use std::collections::BTreeMap;

/// The protocol header every Anthropic-compatible endpoint has historically
/// wanted. It is a default rather than a constant precisely because some
/// compatible endpoints do not.
const DEFAULT_API_VERSION: &str = "2023-06-01";
const DEFAULT_PATH: &str = "/v1/messages";
const DEFAULT_MAX_TOKENS: u64 = 64000;

pub struct AnthropicCompatible {
    base: String,
    path: String,
    model: String,
    max_tokens: u64,
    headers: BTreeMap<String, String>,
}

impl AnthropicCompatible {
    pub fn from_config(cfg: &Value) -> Result<AnthropicCompatible, String> {
        let base = cfg["base_url"].as_str().ok_or(
            "modeld config: `base_url` is required — this backend speaks the \
             Anthropic Messages API to whichever endpoint you name",
        )?;
        let model = cfg["model"]
            .as_str()
            .ok_or("modeld config: `model` is required — providers do not share model names")?;
        // An explicit `{}` sends no protocol headers at all, which is the
        // escape hatch for an endpoint that rejects the ones Anthropic wants.
        let headers = match cfg.get("headers").and_then(Value::as_object) {
            Some(m) => m
                .iter()
                .filter_map(|(k, v)| Some((k.to_ascii_lowercase(), v.as_str()?.to_string())))
                .collect(),
            None => BTreeMap::from([(
                "anthropic-version".to_string(),
                DEFAULT_API_VERSION.to_string(),
            )]),
        };
        Ok(AnthropicCompatible {
            base: base.trim_end_matches('/').to_string(),
            path: cfg["path"].as_str().unwrap_or(DEFAULT_PATH).to_string(),
            model: model.to_string(),
            max_tokens: cfg["max_tokens"].as_u64().unwrap_or(DEFAULT_MAX_TOKENS),
            headers,
        })
    }

    fn endpoint(&self) -> String {
        format!("{}{}", self.base, self.path)
    }

    fn map_messages(&self, messages: &[Msg]) -> Vec<Value> {
        messages
            .iter()
            .map(|m| match m {
                Msg::User(text) => json!({"role": "user", "content": text}),
                Msg::Assistant { parts, raw } => {
                    // Our own raw payload replays verbatim; anything else is
                    // reconstructed from the neutral parts.
                    if let Some((tag, raw)) = raw {
                        if tag == "anthropic" {
                            return json!({"role": "assistant", "content": raw});
                        }
                    }
                    let content: Vec<Value> = parts
                        .iter()
                        .map(|p| match p {
                            Part::Text(t) => json!({"type": "text", "text": t}),
                            Part::ToolCall(c) => json!({
                                "type": "tool_use", "id": c.id,
                                "name": c.verb.tool_name(), "input": c.args,
                            }),
                        })
                        .collect();
                    json!({"role": "assistant", "content": content})
                }
                Msg::ToolResults(results) => {
                    let content: Vec<Value> = results
                        .iter()
                        .map(|r| {
                            let mut v = json!({
                                "type": "tool_result",
                                "tool_use_id": r.call_id,
                                "content": r.content,
                            });
                            if r.is_error {
                                v["is_error"] = json!(true);
                            }
                            v
                        })
                        .collect();
                    json!({"role": "user", "content": content})
                }
            })
            .collect()
    }
}

impl Backend for AnthropicCompatible {
    /// The tag stored beside a transcript's verbatim `raw` payload, so a
    /// later turn knows it may replay that content as-is. It names the **wire
    /// format**, not this backend's config key — renaming the backend must
    /// not invalidate transcripts recorded by it.
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn complete(
        &self,
        gw: &dyn Gateway,
        req: &TurnRequest,
        sink: &mut dyn TurnSink,
    ) -> Result<TurnResult, ModelError> {
        let mut body = json!({
            "model": self.model,
            "max_tokens": self.max_tokens,
            "stream": true,
            "messages": self.map_messages(req.messages),
        });
        if !req.system.is_empty() {
            body["system"] = json!(req.system);
        }
        if !req.tools.is_empty() {
            body["tools"] = Value::Array(
                req.tools
                    .iter()
                    .map(|t| {
                        json!({
                            "name": t.verb.tool_name(),
                            "description": t.description,
                            "input_schema": t.schema,
                        })
                    })
                    .collect(),
            );
        }

        let mut req = EgressRequest::post(self.endpoint(), body.to_string())
            .header("content-type", "application/json")
            .header("accept", "text/event-stream");
        // Configured headers go on last, so an endpoint that wants something
        // different from the protocol defaults gets it.
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let stream = gw.http_stream(req)?;

        if stream.status != 200 {
            let body = drain_body(&stream);
            return Err(ModelError::Provider(format!(
                "model endpoint status {}: {body}",
                stream.status
            )));
        }

        let mut sse = SseParser::default();
        let mut acc = MsgAcc::default();
        let started = std::time::Instant::now();
        loop {
            // Short waits rather than one long one: a cancel arriving while
            // the provider is quiet must still be noticed promptly.
            let ev = match stream
                .rx
                .recv_timeout(std::time::Duration::from_millis(200))
            {
                Ok(ev) => ev,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if sink.cancelled() {
                        return Err(ModelError::Cancelled);
                    }
                    if started.elapsed() > std::time::Duration::from_secs(360) {
                        return Err(ModelError::Gateway("egress stream stalled".into()));
                    }
                    continue;
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(ModelError::Gateway("egress stream closed".into()));
                }
            };
            if sink.cancelled() {
                return Err(ModelError::Cancelled);
            }
            match ev {
                StreamEvent::Chunk { chunk } => {
                    for (event, data) in sse.feed(&chunk) {
                        acc.handle(&event, &data, sink)?;
                    }
                }
                StreamEvent::Done { .. } => break,
                StreamEvent::Error { error } => {
                    return Err(ModelError::Gateway(format!("egress stream error: {error}")));
                }
            }
        }
        acc.finish()
    }
}

fn drain_body(stream: &EgressStream) -> String {
    let mut out = String::new();
    while let Ok(StreamEvent::Chunk { chunk }) =
        stream.rx.recv_timeout(std::time::Duration::from_secs(10))
    {
        out.push_str(&chunk);
    }
    out
}

/// Incremental SSE parser: buffers arbitrary chunk boundaries, yields
/// (event, data) pairs at each blank-line boundary.
#[derive(Default)]
pub struct SseParser {
    buf: String,
}

impl SseParser {
    pub fn feed(&mut self, s: &str) -> Vec<(String, String)> {
        self.buf.push_str(s);
        let mut out = Vec::new();
        while let Some(pos) = self.buf.find("\n\n") {
            let raw: String = self.buf[..pos].to_string();
            self.buf.drain(..pos + 2);
            let mut event = String::new();
            let mut data_lines = Vec::new();
            for line in raw.lines() {
                let line = line.trim_end_matches('\r');
                if let Some(v) = line.strip_prefix("event:") {
                    event = v.trim().to_string();
                } else if let Some(v) = line.strip_prefix("data:") {
                    data_lines.push(v.trim_start().to_string());
                }
            }
            if !event.is_empty() || !data_lines.is_empty() {
                out.push((event, data_lines.join("\n")));
            }
        }
        out
    }
}

/// Accumulates one assistant message from the event stream — the raw content
/// blocks verbatim (for replay) with partial tool-input JSON tracked until
/// each block closes.
#[derive(Default)]
struct MsgAcc {
    raw_blocks: Vec<Value>,
    partial_json: BTreeMap<usize, String>,
    stop_reason: Option<String>,
}

impl MsgAcc {
    fn handle(
        &mut self,
        event: &str,
        data: &str,
        sink: &mut dyn TurnSink,
    ) -> Result<(), ModelError> {
        if event == "ping" {
            return Ok(());
        }
        if event == "error" {
            return Err(ModelError::Provider(format!(
                "anthropic stream error: {data}"
            )));
        }
        let v: Value = serde_json::from_str(data)
            .map_err(|e| ModelError::Provider(format!("sse data json ({event}): {e}")))?;
        match event {
            "content_block_start" => {
                let idx = v["index"].as_u64().unwrap_or(0) as usize;
                while self.raw_blocks.len() <= idx {
                    self.raw_blocks.push(Value::Null);
                }
                self.raw_blocks[idx] = v["content_block"].clone();
            }
            "content_block_delta" => {
                let idx = v["index"].as_u64().unwrap_or(0) as usize;
                let delta = &v["delta"];
                let Some(block) = self.raw_blocks.get_mut(idx) else {
                    return Ok(()); // tolerate deltas for unknown blocks
                };
                match delta["type"].as_str() {
                    Some("text_delta") => {
                        let s = delta["text"].as_str().unwrap_or("");
                        if let Some(t) = block["text"].as_str() {
                            block["text"] = json!(format!("{t}{s}"));
                        }
                        sink.text_delta(s);
                    }
                    Some("input_json_delta") => {
                        self.partial_json
                            .entry(idx)
                            .or_default()
                            .push_str(delta["partial_json"].as_str().unwrap_or(""));
                    }
                    Some("thinking_delta") => {
                        let s = delta["thinking"].as_str().unwrap_or("");
                        if let Some(t) = block["thinking"].as_str() {
                            block["thinking"] = json!(format!("{t}{s}"));
                        }
                    }
                    Some("signature_delta") => {
                        block["signature"] = delta["signature"].clone();
                    }
                    _ => {}
                }
            }
            "content_block_stop" => {
                let idx = v["index"].as_u64().unwrap_or(0) as usize;
                if let Some(partial) = self.partial_json.remove(&idx) {
                    let input: Value = if partial.trim().is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(&partial)
                            .map_err(|e| ModelError::Provider(format!("tool input json: {e}")))?
                    };
                    if let Some(block) = self.raw_blocks.get_mut(idx) {
                        block["input"] = input;
                    }
                }
            }
            "message_delta" => {
                if let Some(s) = v["delta"]["stop_reason"].as_str() {
                    self.stop_reason = Some(s.to_string());
                }
            }
            _ => {} // message_start, message_stop, unknown future events
        }
        Ok(())
    }

    fn finish(self) -> Result<TurnResult, ModelError> {
        let mut parts = Vec::new();
        for block in &self.raw_blocks {
            match block["type"].as_str() {
                Some("text") => {
                    parts.push(Part::Text(block["text"].as_str().unwrap_or("").to_string()));
                }
                Some("tool_use") => {
                    // A provider that names a tool we never offered is a
                    // protocol error, not something to paper over with a
                    // half-formed verb.
                    let name = block["name"].as_str().unwrap_or_default();
                    parts.push(Part::ToolCall(ToolCall {
                        id: block["id"].as_str().unwrap_or("").to_string(),
                        verb: Verb::from_tool_name(name)?,
                        args: Payload::of(&block["input"])?,
                    }));
                }
                _ => {} // thinking etc. ride in raw only
            }
        }
        let stop = match self.stop_reason.as_deref() {
            Some("tool_use") => StopKind::ToolUse,
            Some("end_turn") | None => StopKind::EndTurn,
            Some(other) => StopKind::Other(other.to_string()),
        };
        Ok(TurnResult {
            parts,
            raw: Value::Array(self.raw_blocks),
            stop,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NullSink(String);
    impl TurnSink for NullSink {
        fn text_delta(&mut self, s: &str) {
            self.0.push_str(s);
        }
    }

    /// Captures the request instead of making it, so the tests below can ask
    /// what this backend would actually have sent.
    #[derive(Default)]
    struct CapturingGateway(std::cell::RefCell<Option<EgressRequest>>);

    impl Gateway for CapturingGateway {
        fn http_stream(&self, req: EgressRequest) -> Result<EgressStream, ModelError> {
            *self.0.borrow_mut() = Some(req);
            Err(ModelError::Gateway("captured".into()))
        }
    }

    fn sent(cfg: serde_json::Value) -> EgressRequest {
        let backend = AnthropicCompatible::from_config(&cfg).expect("config");
        let gw = CapturingGateway::default();
        let _ = backend.complete(
            &gw,
            &TurnRequest {
                system: "",
                messages: &[Msg::User("hi".into())],
                tools: &[],
            },
            &mut NullSink(String::new()),
        );
        gw.0.into_inner().expect("a request was built")
    }

    /// The whole point of this backend being a protocol rather than a vendor:
    /// nothing it sends is decided here.
    #[test]
    fn every_part_of_the_endpoint_comes_from_config() {
        let req = sent(json!({
            "base_url": "https://gateway.internal:8443/llm/",
            "path": "/anthropic/v1/messages",
            "model": "some-other-model",
            "max_tokens": 4096,
            "headers": {"x-tenant": "acme"},
        }));
        assert_eq!(
            req.url,
            "https://gateway.internal:8443/llm/anthropic/v1/messages"
        );
        assert_eq!(
            req.headers.get("x-tenant").map(String::as_str),
            Some("acme")
        );
        assert_eq!(
            req.headers.get("anthropic-version"),
            None,
            "an explicit header set replaces the defaults rather than adding to them"
        );
        let body: Value = serde_json::from_str(req.body.as_deref().unwrap()).unwrap();
        assert_eq!(body["model"], "some-other-model");
        assert_eq!(body["max_tokens"], 4096);
    }

    #[test]
    fn the_protocol_header_is_a_default_not_a_constant() {
        let req = sent(json!({"base_url": "https://x.test", "model": "m"}));
        assert_eq!(req.url, "https://x.test/v1/messages");
        assert_eq!(
            req.headers.get("anthropic-version").map(String::as_str),
            Some(DEFAULT_API_VERSION)
        );
        // An endpoint that rejects it needs a way to say so.
        let bare = sent(json!({"base_url": "https://x.test", "model": "m", "headers": {}}));
        assert_eq!(bare.headers.get("anthropic-version"), None);
    }

    /// Guessing which provider was meant is worse than refusing to start.
    #[test]
    fn identity_has_no_default() {
        assert!(AnthropicCompatible::from_config(&json!({"model": "m"})).is_err());
        assert!(AnthropicCompatible::from_config(&json!({"base_url": "https://x.test"})).is_err());
    }

    /// Feed a full Anthropic SSE exchange split at hostile byte boundaries;
    /// the accumulator must reassemble text, tool input, and stop reason.
    #[test]
    fn sse_reassembles_across_arbitrary_chunk_boundaries() {
        let wire = concat!(
            "event: message_start\ndata: {\"type\":\"message_start\"}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"Hel\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"lo\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"echo__emit\",\"input\":{}}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"x\\\":\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"1}\"}}\n\n",
            "event: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        );
        // Split every 7 bytes — guaranteed to cut mid-line and mid-JSON.
        let mut sse = SseParser::default();
        let mut acc = MsgAcc::default();
        let mut sink = NullSink(String::new());
        let bytes = wire.as_bytes();
        let mut i = 0;
        while i < bytes.len() {
            let end = (i + 7).min(bytes.len());
            for (event, data) in sse.feed(std::str::from_utf8(&bytes[i..end]).unwrap()) {
                acc.handle(&event, &data, &mut sink).unwrap();
            }
            i = end;
        }
        let out = acc.finish().unwrap();
        assert_eq!(sink.0, "Hello");
        assert_eq!(out.stop, StopKind::ToolUse);
        assert_eq!(out.parts.len(), 2);
        match &out.parts[1] {
            Part::ToolCall(c) => {
                assert_eq!(c.verb.as_str(), "echo::emit");
                assert_eq!(c.args.parse::<Value>().unwrap(), json!({"x": 1}));
            }
            other => panic!("expected tool call, got {other:?}"),
        }
        // Raw payload preserves the provider shape for replay.
        assert_eq!(out.raw[1]["type"], "tool_use");
        assert_eq!(out.raw[1]["input"], json!({"x": 1}));
    }

    #[test]
    fn thinking_blocks_ride_in_raw_only() {
        let mut acc = MsgAcc::default();
        let mut sink = NullSink(String::new());
        acc.handle(
            "content_block_start",
            r#"{"index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            &mut sink,
        )
        .unwrap();
        acc.handle(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#,
            &mut sink,
        )
        .unwrap();
        acc.handle(
            "content_block_delta",
            r#"{"index":0,"delta":{"type":"signature_delta","signature":"sig123"}}"#,
            &mut sink,
        )
        .unwrap();
        acc.handle(
            "content_block_start",
            r#"{"index":1,"content_block":{"type":"text","text":""}}"#,
            &mut sink,
        )
        .unwrap();
        acc.handle(
            "content_block_delta",
            r#"{"index":1,"delta":{"type":"text_delta","text":"ok"}}"#,
            &mut sink,
        )
        .unwrap();
        acc.handle(
            "message_delta",
            r#"{"delta":{"stop_reason":"end_turn"}}"#,
            &mut sink,
        )
        .unwrap();
        let out = acc.finish().unwrap();
        assert_eq!(out.parts.len(), 1, "thinking is not a neutral part");
        assert_eq!(out.raw[0]["thinking"], "hmm");
        assert_eq!(out.raw[0]["signature"], "sig123");
        assert_eq!(sink.0, "ok", "thinking deltas are not text deltas");
    }
}
