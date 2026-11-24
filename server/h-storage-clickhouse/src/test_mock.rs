//! Shared scripted ClickHouse HTTP mock for in-process tests.
//!
//! ClickHouse has no in-process `:memory:` mode, so the live `it.rs` suite
//! self-skips without a server. This mock speaks enough of the ClickHouse HTTP
//! protocol to let tests exercise the real client paths without one:
//!
//!   * it records every SQL statement the backend issues,
//!   * answers a `SELECT count() …` probe with a RowBinary `u64` (the pagination / retention count
//!     shape), and
//!   * answers every other query with an empty body, which decodes to an empty result set under
//!     `with_validation(false)`.
//!
//! `test_backend` builds a client with compression off (the mock returns plain
//! RowBinary) and validation off (no names/types header). Production keeps LZ4
//! and validation; neither is what these tests are about.
#![cfg(test)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::ClickHouseBackend;

pub(crate) struct Mock {
    addr: SocketAddr,
    log: Arc<Mutex<Vec<String>>>,
    counts: Arc<Mutex<HashMap<String, u64>>>,
    fail_on: Arc<Mutex<Option<String>>>,
}

impl Mock {
    pub(crate) fn start() -> Mock {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let addr = listener.local_addr().unwrap();
        let log = Arc::new(Mutex::new(Vec::new()));
        let counts = Arc::new(Mutex::new(HashMap::new()));
        let fail_on = Arc::new(Mutex::new(None));
        let (l, c, f) = (log.clone(), counts.clone(), fail_on.clone());
        thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let (l, c, f) = (l.clone(), c.clone(), f.clone());
                thread::spawn(move || handle_conn(stream, l, c, f));
            }
        });
        Mock {
            addr,
            log,
            counts,
            fail_on,
        }
    }

    pub(crate) fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    /// Count returned for `SELECT count() … FROM <table>` (default 0).
    pub(crate) fn set_count(&self, table: &str, n: u64) {
        self.counts.lock().unwrap().insert(table.to_string(), n);
    }

    /// Make any statement containing `needle` answer HTTP 500.
    pub(crate) fn fail_on(&self, needle: &str) {
        *self.fail_on.lock().unwrap() = Some(needle.to_string());
    }

    pub(crate) fn statements(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }

    /// All recorded statements joined — convenient for `contains` assertions.
    pub(crate) fn sql(&self) -> String {
        self.statements().join("\n")
    }
}

pub(crate) fn test_backend(mock: &Mock, optimize: bool) -> ClickHouseBackend {
    let client = clickhouse::Client::default()
        .with_url(mock.url())
        .with_database("heron")
        .with_compression(clickhouse::Compression::None)
        .with_validation(false);
    ClickHouseBackend {
        client,
        url: mock.url(),
        user: "default".to_string(),
        password: String::new(),
        database: "heron".to_string(),
        optimize_on_sweep: optimize,
    }
}

fn handle_conn(
    mut stream: TcpStream,
    log: Arc<Mutex<Vec<String>>>,
    counts: Arc<Mutex<HashMap<String, u64>>>,
    fail_on: Arc<Mutex<Option<String>>>,
) {
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
        let mut chunked = false;
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap_or(0) == 0 {
                return;
            }
            if line == "\r\n" || line == "\n" {
                break;
            }
            let low = line.to_ascii_lowercase();
            if let Some(v) = low.strip_prefix("content-length:") {
                content_length = v.trim().parse().unwrap_or(0);
            } else if let Some(v) = low.strip_prefix("transfer-encoding:") {
                chunked = v.trim() == "chunked";
            }
        }

        // A statement arrives in the body for `query()`, but in the URL's
        // `?query=` parameter for streaming `insert()` (whose body is the
        // RowBinary row data). Prefer the URL query when present so an insert's
        // binary body is never recorded or classified as SQL.
        let body = if chunked {
            read_chunked(&mut reader).into_bytes()
        } else if content_length > 0 {
            let mut buf = vec![0u8; content_length];
            if reader.read_exact(&mut buf).is_err() {
                return;
            }
            buf
        } else {
            Vec::new()
        };
        let sql = match query_param(&req_line) {
            Some(q) if !q.trim().is_empty() => q,
            _ => String::from_utf8_lossy(&body).to_string(),
        };

        if req_line.starts_with("GET /ping") {
            respond(&mut stream, 200, b"Ok.\n", "text/plain");
            continue;
        }

        // Only the pagination/retention count probe (`SELECT count() AS n FROM
        // …`) is answered with a row. Aggregate selects that merely *contain*
        // `count()` in the projection use `fetch_all` and must still get an
        // empty body.
        let is_count = sql
            .trim_start()
            .to_ascii_uppercase()
            .starts_with("SELECT COUNT()");
        log.lock().unwrap().push(sql.clone());

        if fail_on
            .lock()
            .unwrap()
            .as_ref()
            .map(|needle| sql.contains(needle.as_str()))
            .unwrap_or(false)
        {
            respond(
                &mut stream,
                500,
                b"Code: 62. DB::Exception: mock failure\n",
                "text/plain",
            );
            continue;
        }

        if is_count {
            let table = table_of(&sql);
            let n = counts.lock().unwrap().get(&table).copied().unwrap_or(0);
            respond(
                &mut stream,
                200,
                &n.to_le_bytes(),
                "application/octet-stream",
            );
        } else {
            respond(&mut stream, 200, b"", "text/plain");
        }
    }
}

fn read_chunked<R: BufRead>(reader: &mut R) -> String {
    let mut out = Vec::new();
    loop {
        let mut size_line = String::new();
        if reader.read_line(&mut size_line).unwrap_or(0) == 0 {
            break;
        }
        let size = usize::from_str_radix(size_line.trim().split(';').next().unwrap_or("0"), 16)
            .unwrap_or(0);
        if size == 0 {
            let mut trailer = String::new();
            let _ = reader.read_line(&mut trailer);
            break;
        }
        let mut buf = vec![0u8; size + 2];
        if reader.read_exact(&mut buf).is_err() {
            break;
        }
        out.extend_from_slice(&buf[..size]);
    }
    String::from_utf8_lossy(&out).to_string()
}

fn respond(stream: &mut TcpStream, code: u16, body: &[u8], ctype: &str) {
    let reason = if code == 200 {
        "200 OK"
    } else {
        "500 Internal Server Error"
    };
    let header = format!(
        "HTTP/1.1 {reason}\r\nContent-Length: {}\r\nContent-Type: {ctype}\r\nConnection: \
         keep-alive\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

fn query_param(req_line: &str) -> Option<String> {
    let path = req_line.split_whitespace().nth(1)?;
    let qs = path.split_once('?')?.1;
    for pair in qs.split('&') {
        if let Some(v) = pair.strip_prefix("query=") {
            return Some(percent_decode(v));
        }
    }
    None
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
                if let Ok(b) = u8::from_str_radix(hex, 16) {
                    out.push(b);
                    i += 3;
                } else {
                    out.push(bytes[i]);
                    i += 1;
                }
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).to_string()
}

fn table_of(sql: &str) -> String {
    let upper = sql.to_ascii_uppercase();
    let Some(pos) = upper.find("FROM ") else {
        return String::new();
    };
    sql[pos + 5..]
        .trim_start()
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches('`')
        .to_string()
}
