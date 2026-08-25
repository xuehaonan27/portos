//! portos-broker: the egress chokepoint (architecture-v0.md §5.7,
//! decisions-v1.md D28), as a kernel-spawned trusted process.
//!
//! The plugin default contract is NO direct network. Anything that must
//! reach the outside world invokes `egress::http` / `egress::http_stream`
//! here (capability-gated by the kernel on the invoke path), and this
//! process is where the three chokepoint duties live:
//!
//!   1. **allowlist** — a request goes out only to a configured host, https
//!      unless the rule opts into insecure http (loopback/dev);
//!   2. **credential injection** — secrets are attached as headers here and
//!      exist only in this process; callers reference them by rule, never by
//!      value. Caller-supplied headers can never collide with injected ones.
//!   3. **outbound accounting** — every request emits an `egress::log` event
//!      (method, host, status, injected header *names* — never values); the
//!      spawner pins that topic to the audit chain via `Host::audit_topic`.
//!
//! Responses are sanitized: `set-cookie` (plus per-rule strip lists) never
//! reaches the caller. Config lives in `$PORTOS_BROKER_DIR/config.json`,
//! secrets in `$PORTOS_BROKER_DIR/secrets.json` (0600 file stub — same
//! discipline as consent.key: the store is a stub, the data flow is real;
//! the macOS Keychain backend comes later). Missing config = empty
//! allowlist = deny everything.
//!
//! Per D31, this process knows nothing of the plan language or interpreter:
//! it faces the Host ABI only.

use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::io::Read;
use std::sync::Arc;
use std::time::Duration;

/// Inline response body cap for `egress::http`; larger bodies must use
/// `egress::http_stream` (or, later, land in the CAS as an artifact).
const BODY_MAX: usize = 4 * 1024 * 1024;
const STREAM_CHUNK: usize = 16 * 1024;

/// Headers the caller may never set: connection mechanics, and anything a
/// rule injects (auth belongs to the injection point, not the caller).
const FORBIDDEN_CALLER_HEADERS: &[&str] =
    &["host", "content-length", "connection", "transfer-encoding"];

struct Rule {
    host: String,
    insecure_http: bool,
    /// header name -> secret name (resolved via the secrets file)
    inject: BTreeMap<String, String>,
    strip_response: Vec<String>,
}

struct Config {
    rules: Vec<Rule>,
    secrets: BTreeMap<String, String>,
}

fn load_config() -> Config {
    let Some(dir) = std::env::var_os("PORTOS_BROKER_DIR") else {
        eprintln!("[broker] PORTOS_BROKER_DIR unset: empty allowlist, denying all egress");
        return Config {
            rules: Vec::new(),
            secrets: BTreeMap::new(),
        };
    };
    let dir = std::path::PathBuf::from(dir);
    let rules = std::fs::read_to_string(dir.join("config.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v["allow"].as_array().cloned())
        .map(|entries| {
            entries
                .iter()
                .filter_map(|e| {
                    Some(Rule {
                        host: e["host"].as_str()?.to_ascii_lowercase(),
                        insecure_http: e["insecure_http"].as_bool().unwrap_or(false),
                        inject: e["inject"]
                            .as_object()
                            .map(|m| {
                                m.iter()
                                    .filter_map(|(k, v)| {
                                        Some((k.to_ascii_lowercase(), v.as_str()?.to_string()))
                                    })
                                    .collect()
                            })
                            .unwrap_or_default(),
                        strip_response: e["strip_response"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .filter_map(|s| Some(s.as_str()?.to_ascii_lowercase()))
                                    .collect()
                            })
                            .unwrap_or_default(),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    let secrets = std::fs::read_to_string(dir.join("secrets.json"))
        .ok()
        .and_then(|s| serde_json::from_str::<BTreeMap<String, String>>(&s).ok())
        .unwrap_or_default();
    Config { rules, secrets }
}

fn find_rule<'a>(cfg: &'a Config, url: &url::Url) -> Result<&'a Rule, String> {
    let host = url
        .host_str()
        .ok_or_else(|| "url has no host".to_string())?
        .to_ascii_lowercase();
    let rule = cfg
        .rules
        .iter()
        .find(|r| r.host == host)
        .ok_or_else(|| format!("egress denied: host not allowed: {host}"))?;
    match url.scheme() {
        "https" => Ok(rule),
        "http" if rule.insecure_http => Ok(rule),
        s => Err(format!("egress denied: scheme {s} not allowed for {host}")),
    }
}

/// Build the outbound request and send it. 4xx/5xx are responses, not
/// errors; only transport failures error out.
struct Sent {
    resp: ureq::Response,
    method: &'static str,
    host: String,
    injected: Vec<String>,
    strip: Vec<String>,
}

fn send(
    agent: &ureq::Agent,
    cfg: &Config,
    args: &Value,
    overall_timeout: Option<Duration>,
) -> Result<Sent, String> {
    let url_s = args["url"].as_str().ok_or("missing url")?;
    let url = url::Url::parse(url_s).map_err(|e| format!("bad url: {e}"))?;
    let rule = find_rule(cfg, &url)?;
    let method = match args["method"].as_str().unwrap_or("GET") {
        m if m.eq_ignore_ascii_case("GET") => "GET",
        m if m.eq_ignore_ascii_case("POST") => "POST",
        m if m.eq_ignore_ascii_case("PUT") => "PUT",
        m if m.eq_ignore_ascii_case("DELETE") => "DELETE",
        m if m.eq_ignore_ascii_case("PATCH") => "PATCH",
        m if m.eq_ignore_ascii_case("HEAD") => "HEAD",
        m => return Err(format!("method not allowed: {m}")),
    };

    let mut req = agent.request(method, url_s);
    if let Some(t) = overall_timeout {
        req = req.timeout(t);
    }
    if let Some(headers) = args["headers"].as_object() {
        for (k, v) in headers {
            let kl = k.to_ascii_lowercase();
            if FORBIDDEN_CALLER_HEADERS.contains(&kl.as_str()) || rule.inject.contains_key(&kl) {
                continue; // injection point owns these
            }
            if let Some(vs) = v.as_str() {
                req = req.set(k, vs);
            }
        }
    }
    let mut injected = Vec::new();
    for (header, secret_name) in &rule.inject {
        let secret = cfg
            .secrets
            .get(secret_name)
            .ok_or_else(|| format!("unknown secret: {secret_name}"))?;
        req = req.set(header, secret);
        injected.push(header.clone());
    }

    let result = match args["body"].as_str() {
        Some(body) => req.send_string(body),
        None => req.call(),
    };
    let resp = match result {
        Ok(r) => r,
        Err(ureq::Error::Status(_, r)) => r,
        Err(e) => return Err(format!("egress transport: {e}")),
    };
    Ok(Sent {
        resp,
        method,
        host: url.host_str().unwrap_or("?").to_string(),
        injected,
        strip: rule.strip_response.clone(),
    })
}

fn sanitize_headers(resp: &ureq::Response, strip: &[String]) -> Value {
    let mut out = serde_json::Map::new();
    for name in resp.headers_names() {
        let lower = name.to_ascii_lowercase();
        if lower == "set-cookie" || strip.contains(&lower) {
            continue;
        }
        if let Some(v) = resp.header(&name) {
            out.insert(lower, json!(v));
        }
    }
    Value::Object(out)
}

fn main() -> std::io::Result<()> {
    let cfg = Arc::new(load_config());
    // Buffered requests get an overall timeout; streaming requests only a
    // connect/idle timeout (an SSE stream is long-lived by design).
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(300))
        .build();

    portos_sdk::serve(
        "portos-broker",
        &["egress::http", "egress::http_stream"],
        move |verb, args, client| match verb {
            "egress::http" => {
                let sent = send(&agent, &cfg, args, Some(Duration::from_secs(60)))?;
                let status = sent.resp.status();
                let headers = sanitize_headers(&sent.resp, &sent.strip);
                let mut body = Vec::new();
                sent.resp
                    .into_reader()
                    .take((BODY_MAX + 1) as u64)
                    .read_to_end(&mut body)
                    .map_err(|e| format!("egress body: {e}"))?;
                if body.len() > BODY_MAX {
                    return Err("response too large: use egress::http_stream".into());
                }
                let _ = client.emit(
                    "egress::log",
                    json!({"verb": "http", "method": sent.method, "host": sent.host,
                           "status": status, "injected": sent.injected}),
                );
                Ok(json!({
                    "status": status,
                    "headers": headers,
                    "body": String::from_utf8_lossy(&body),
                }))
            }
            // Returns {status, headers} immediately; the body streams to the
            // caller-named topic as {"chunk"} events, closed by {"done"} (or
            // {"error"}). The caller subscribes before invoking.
            "egress::http_stream" => {
                let topic = args["topic"]
                    .as_str()
                    .ok_or("missing topic")?
                    .to_string();
                let sent = send(&agent, &cfg, args, None)?;
                let status = sent.resp.status();
                let headers = sanitize_headers(&sent.resp, &sent.strip);
                let _ = client.emit(
                    "egress::log",
                    json!({"verb": "http_stream", "method": sent.method, "host": sent.host,
                           "status": status, "injected": sent.injected}),
                );
                let client = client.clone();
                std::thread::spawn(move || {
                    let mut reader = sent.resp.into_reader();
                    let mut buf = vec![0u8; STREAM_CHUNK];
                    let mut total: u64 = 0;
                    loop {
                        match reader.read(&mut buf) {
                            Ok(0) => {
                                let _ = client.emit(&topic, json!({"done": true, "bytes": total}));
                                return;
                            }
                            Ok(n) => {
                                total += n as u64;
                                let chunk = String::from_utf8_lossy(&buf[..n]).to_string();
                                if client.emit(&topic, json!({"chunk": chunk})).is_err() {
                                    return; // kernel went away
                                }
                            }
                            Err(e) => {
                                let _ = client.emit(&topic, json!({"error": e.to_string()}));
                                return;
                            }
                        }
                    }
                });
                Ok(json!({"status": status, "headers": headers}))
            }
            other => Err(format!("unknown verb: {other}")),
        },
        |_, _| {},
    )
}
