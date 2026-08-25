//! Anthropic Messages API backend — one implementation behind the neutral
//! [`Backend`] seam, never the driver itself.
//!
//! All traffic goes through the egress gateway (the broker injects
//! `x-api-key`; this process never holds the key). Responses stream via SSE:
//! broker chunks arrive at arbitrary byte boundaries, so parsing is
//! incremental. Assistant content blocks are accumulated **verbatim**
//! (thinking blocks and signatures included) into the provider-opaque `raw`
//! payload so multi-turn replay is faithful; the neutral parts are derived
//! from the same accumulator.

use crate::core::{
    Backend, EgressStream, Gateway, Msg, Part, StopKind, ToolCall, TurnRequest, TurnResult,
    TurnSink, mangle, unmangle,
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

pub struct Anthropic {
    base: String,
    model: String,
    max_tokens: u64,
    version: String,
}

impl Anthropic {
    pub fn from_config(cfg: &Value) -> Anthropic {
        Anthropic {
            base: cfg["base_url"]
                .as_str()
                .unwrap_or("https://api.anthropic.com")
                .trim_end_matches('/')
                .to_string(),
            model: cfg["model"].as_str().unwrap_or("claude-opus-5").to_string(),
            max_tokens: cfg["max_tokens"].as_u64().unwrap_or(64000),
            version: cfg["api_version"].as_str().unwrap_or("2023-06-01").to_string(),
        }
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
                                "name": mangle(&c.verb), "input": c.args,
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

impl Backend for Anthropic {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    fn complete(
        &self,
        gw: &dyn Gateway,
        req: &TurnRequest,
        sink: &mut dyn TurnSink,
    ) -> Result<TurnResult, String> {
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
                            "name": mangle(&t.verb),
                            "description": t.description,
                            "input_schema": t.schema,
                        })
                    })
                    .collect(),
            );
        }

        let stream = gw.http_stream(json!({
            "method": "POST",
            "url": format!("{}/v1/messages", self.base),
            "headers": {
                "content-type": "application/json",
                "anthropic-version": self.version,
                "accept": "text/event-stream",
            },
            "body": body.to_string(),
        }))?;

        let status = stream.head["status"].as_u64().unwrap_or(0);
        if status != 200 {
            let body = drain_body(&stream);
            return Err(format!("anthropic api status {status}: {body}"));
        }

        let mut sse = SseParser::default();
        let mut acc = MsgAcc::default();
        loop {
            let ev = stream
                .rx
                .recv_timeout(std::time::Duration::from_secs(360))
                .map_err(|_| "egress stream stalled".to_string())?;
            if let Some(chunk) = ev["chunk"].as_str() {
                for (event, data) in sse.feed(chunk) {
                    acc.handle(&event, &data, sink)?;
                }
            } else if ev["done"].as_bool() == Some(true) {
                break;
            } else if let Some(e) = ev["error"].as_str() {
                return Err(format!("egress stream error: {e}"));
            }
        }
        acc.finish()
    }
}

fn drain_body(stream: &EgressStream) -> String {
    let mut out = String::new();
    while let Ok(ev) = stream
        .rx
        .recv_timeout(std::time::Duration::from_secs(10))
    {
        if let Some(c) = ev["chunk"].as_str() {
            out.push_str(c);
        } else {
            break;
        }
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
    fn handle(&mut self, event: &str, data: &str, sink: &mut dyn TurnSink) -> Result<(), String> {
        if event == "ping" {
            return Ok(());
        }
        if event == "error" {
            return Err(format!("anthropic stream error: {data}"));
        }
        let v: Value =
            serde_json::from_str(data).map_err(|e| format!("sse data json ({event}): {e}"))?;
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
                            .map_err(|e| format!("tool input json: {e}"))?
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

    fn finish(self) -> Result<TurnResult, String> {
        let mut parts = Vec::new();
        for block in &self.raw_blocks {
            match block["type"].as_str() {
                Some("text") => {
                    parts.push(Part::Text(block["text"].as_str().unwrap_or("").to_string()));
                }
                Some("tool_use") => {
                    parts.push(Part::ToolCall(ToolCall {
                        id: block["id"].as_str().unwrap_or("").to_string(),
                        verb: unmangle(block["name"].as_str().unwrap_or("")),
                        args: block["input"].clone(),
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
                assert_eq!(c.verb, "echo::emit");
                assert_eq!(c.args, json!({"x": 1}));
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
        acc.handle("content_block_start", r#"{"index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#, &mut sink).unwrap();
        acc.handle("content_block_delta", r#"{"index":0,"delta":{"type":"thinking_delta","thinking":"hmm"}}"#, &mut sink).unwrap();
        acc.handle("content_block_delta", r#"{"index":0,"delta":{"type":"signature_delta","signature":"sig123"}}"#, &mut sink).unwrap();
        acc.handle("content_block_start", r#"{"index":1,"content_block":{"type":"text","text":""}}"#, &mut sink).unwrap();
        acc.handle("content_block_delta", r#"{"index":1,"delta":{"type":"text_delta","text":"ok"}}"#, &mut sink).unwrap();
        acc.handle("message_delta", r#"{"delta":{"stop_reason":"end_turn"}}"#, &mut sink).unwrap();
        let out = acc.finish().unwrap();
        assert_eq!(out.parts.len(), 1, "thinking is not a neutral part");
        assert_eq!(out.raw[0]["thinking"], "hmm");
        assert_eq!(out.raw[0]["signature"], "sig123");
        assert_eq!(sink.0, "ok", "thinking deltas are not text deltas");
    }
}
