//! Shared scripted aglake search mock.
//!
//! aglake's read face is a plain HTTP `POST /api/v1/search` carrying an SPL
//! string. With no user catalog configured, the client presents no session
//! (`AuthState::token` returns `None`), so a test can point `AglakeBackend` at
//! a loopback mock and drive the real read paths without an aglaked daemon —
//! the same trick `h-storage-clickhouse/src/test_mock.rs` uses for its
//! RowBinary protocol.
//!
//! The mock classifies each query by its SPL and answers:
//!
//!   * `| stats count as n | table n` → `{n: 0}` (the page-count companion),
//!   * a `stats … by` aggregate → one synthetic row with the fields the decoder reads (so the
//!     row-mapping code runs, not just the builder),
//!   * anything else → an empty result set.
//!
//! Every request's SPL is recorded, so tests can assert what was issued.
#![cfg(test)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use h_common::config::AglakeConfig;

use crate::AglakeBackend;

pub(crate) struct MockAglake {
    addr: SocketAddr,
    queries: Arc<Mutex<Vec<String>>>,
}

impl MockAglake {
    pub(crate) fn start() -> MockAglake {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock aglake");
        let addr = listener.local_addr().unwrap();
        let queries = Arc::new(Mutex::new(Vec::new()));
        let bg = Arc::clone(&queries);
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let bg = Arc::clone(&bg);
                thread::spawn(move || handle_conn(stream, bg));
            }
        });
        MockAglake { addr, queries }
    }

    pub(crate) fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// True when any issued SPL contains `needle`.
    pub(crate) fn saw(&self, needle: &str) -> bool {
        self.queries
            .lock()
            .unwrap()
            .iter()
            .any(|q| q.contains(needle))
    }

    pub(crate) fn queries(&self) -> Vec<String> {
        self.queries.lock().unwrap().clone()
    }
}

/// Build an `AglakeBackend` pointed at the mock in open (no-auth) mode.
pub(crate) fn test_backend(mock: &MockAglake) -> AglakeBackend {
    let config = AglakeConfig {
        url: mock.url(),
        username: String::new(),
        password: String::new(),
        session_token: String::new(),
        ..Default::default()
    };
    AglakeBackend::new(&config).expect("build aglake backend")
}

fn handle_conn(mut stream: TcpStream, queries: Arc<Mutex<Vec<String>>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    loop {
        let mut req_line = String::new();
        if reader.read_line(&mut req_line).unwrap_or(0) == 0 {
            return;
        }
        let req_line = req_line.trim_end().to_string();
        if req_line.is_empty() {
            continue;
        }

        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                content_length = v.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 && reader.read_exact(&mut body).is_err() {
            return;
        }
        let body = String::from_utf8_lossy(&body).to_string();

        let path = req_line
            .split_whitespace()
            .nth(1)
            .unwrap_or("/")
            .to_string();

        if path.starts_with("/api/v1/search") {
            let q = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("q").and_then(|q| q.as_str()).map(str::to_string))
                .unwrap_or_default();
            queries.lock().unwrap().push(q.clone());
            let payload = classify(&q);
            respond_json(&mut stream, 200, &payload);
        } else if path.starts_with("/api/v1/indexes") {
            respond_json(&mut stream, 200, "{}");
        } else {
            respond_json(&mut stream, 200, "{}");
        }
    }
}

/// Decide the response body from the SPL. Order matters: the aggregate shapes
/// below all contain `+str(`, so they are checked before the distinct shape.
fn classify(q: &str) -> String {
    if q.contains("stats count as n") {
        return r#"{"mode":"results","total":1,"rows":[{"n":0}]}"#.to_string();
    }
    // Agent summary: `stats … by agent_kind` with token sums + avg duration.
    if q.contains("avg_duration_ms") && q.contains("by agent_kind") {
        return r#"{"mode":"results","total":1,"rows":[{"agent_kind":"claude-cli","turn_count":3,"total_input_tokens":10,"total_output_tokens":5,"avg_duration_ms":100.0,"last_us":1700000000000000}]}"#.to_string();
    }
    // Agent activity: bucketed on `_time`.
    if q.contains("by _time, agent_kind") {
        return r#"{"mode":"results","total":1,"rows":[{"_time":1700000000,"agent_kind":"claude-cli","turn_count":2}]}"#.to_string();
    }
    // Finish-reason distincts need both fields.
    if q.contains("by wire_api, finish_reason") {
        return r#"{"mode":"results","total":1,"rows":[{"wire_api":"openai-chat","finish_reason":"stop"}]}"#.to_string();
    }
    // Generic `stats count by <field>` distincts: echo one value for the field.
    if let Some(field) = field_after(q, "+str(") {
        return format!(r#"{{"mode":"results","total":1,"rows":[{{"{field}":"alpha"}}]}}"#);
    }
    r#"{"mode":"results","total":0,"rows":[]}"#.to_string()
}

/// Extract `field` from a `+str(field)` marker.
fn field_after(q: &str, marker: &str) -> Option<String> {
    let start = q.find(marker)? + marker.len();
    let rest = &q[start..];
    let end = rest.find(')')?;
    let field = rest[..end].trim();
    if field.is_empty() {
        None
    } else {
        Some(field.to_string())
    }
}

fn respond_json(stream: &mut TcpStream, code: u16, body: &str) {
    let reason = if code == 200 {
        "200 OK"
    } else {
        "500 Internal Server Error"
    };
    let header = format!(
        "HTTP/1.1 {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: \
         keep-alive\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body.as_bytes());
    let _ = stream.flush();
}
