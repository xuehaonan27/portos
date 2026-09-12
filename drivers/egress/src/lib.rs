//! The `egress::*` family interface.
//!
//! A driver family interface is the contract between whoever implements a
//! family and whoever calls it — here, the trusted egress broker and any
//! plugin that needs the outside world. It lives outside the kernel because
//! the kernel must not know what "egress" means; it lives outside both
//! implementations because a wire contract copied into two crates is a
//! contract that drifts.
//!
//! Credentials are not part of this interface by construction: a caller
//! names a URL and headers, and the implementation attaches whatever secret
//! its rules say. There is no field here through which a key could travel.

use portos_abi::ids::{Topic, Verb};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::LazyLock;

/// Buffered request/response. The whole body comes back in the reply.
/// The directory an egress broker is given under the runtime root, and the
/// variable it arrives in. The allowlist and the secrets live there.
pub const DIR: &str = "broker";
pub const DIR_ENV: &str = "PORTOS_BROKER_DIR";

pub static HTTP: LazyLock<Verb> =
    LazyLock::new(|| Verb::parse("egress::http").expect("constant verb"));

/// Streaming request: the reply carries the response head, and the body is
/// published to the caller's topic as [`StreamEvent`]s. The caller must
/// subscribe before invoking.
pub static HTTP_STREAM: LazyLock<Verb> =
    LazyLock::new(|| Verb::parse("egress::http_stream").expect("constant verb"));

/// Where the implementation publishes its outbound accounting.
pub static LOG: LazyLock<Topic> =
    LazyLock::new(|| Topic::parse("egress::log").expect("constant topic"));

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct EgressRequest {
    pub url: String,
    #[serde(default)]
    pub method: Method,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    /// `http_stream` only: where the body chunks are published.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<Topic>,
}

impl EgressRequest {
    pub fn post(url: impl Into<String>, body: impl Into<String>) -> EgressRequest {
        EgressRequest {
            url: url.into(),
            method: Method::Post,
            body: Some(body.into()),
            ..Default::default()
        }
    }

    pub fn header(mut self, name: &str, value: &str) -> EgressRequest {
        self.headers.insert(name.to_string(), value.to_string());
        self
    }

    pub fn streaming_to(mut self, topic: Topic) -> EgressRequest {
        self.topic = Some(topic);
        self
    }
}

/// The methods this family forwards. Accepted in any case, since callers
/// write both `POST` and `post`; recorded in one.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum Method {
    #[default]
    Get,
    Post,
    Put,
    Delete,
    Patch,
    Head,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Method::Get => "GET",
            Method::Post => "POST",
            Method::Put => "PUT",
            Method::Delete => "DELETE",
            Method::Patch => "PATCH",
            Method::Head => "HEAD",
        }
    }
}

impl<'de> Deserialize<'de> for Method {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Method, D::Error> {
        let s = String::deserialize(d)?;
        match s.to_ascii_uppercase().as_str() {
            "GET" => Ok(Method::Get),
            "POST" => Ok(Method::Post),
            "PUT" => Ok(Method::Put),
            "DELETE" => Ok(Method::Delete),
            "PATCH" => Ok(Method::Patch),
            "HEAD" => Ok(Method::Head),
            other => Err(serde::de::Error::custom(format!(
                "method not allowed: {other}"
            ))),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HttpReply {
    pub status: u16,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub body: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StreamHead {
    pub status: u16,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
}

/// One frame of a streamed response body. Distinguished by which key is
/// present rather than by a tag, which is the shape already on the wire.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum StreamEvent {
    Chunk { chunk: String },
    Done { done: bool, bytes: u64 },
    Error { error: String },
}

/// One line of outbound accounting. Header *names* are recorded, never
/// values.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct EgressLog {
    pub verb: String,
    pub method: Method,
    pub host: String,
    pub status: u16,
    #[serde(default)]
    pub injected: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_events_keep_their_established_shapes() {
        let cases = [
            (
                StreamEvent::Chunk { chunk: "hi".into() },
                r#"{"chunk":"hi"}"#,
            ),
            (
                StreamEvent::Done {
                    done: true,
                    bytes: 5,
                },
                r#"{"done":true,"bytes":5}"#,
            ),
            (
                StreamEvent::Error {
                    error: "boom".into(),
                },
                r#"{"error":"boom"}"#,
            ),
        ];
        for (ev, text) in cases {
            assert_eq!(serde_json::to_string(&ev).unwrap(), text);
            assert_eq!(serde_json::from_str::<StreamEvent>(text).unwrap(), ev);
        }
    }

    #[test]
    fn method_accepts_any_case_and_rejects_the_rest() {
        assert_eq!(
            serde_json::from_str::<Method>(r#""post""#).unwrap(),
            Method::Post
        );
        assert_eq!(
            serde_json::from_str::<Method>(r#""POST""#).unwrap(),
            Method::Post
        );
        assert!(serde_json::from_str::<Method>(r#""TRACE""#).is_err());
    }
}
