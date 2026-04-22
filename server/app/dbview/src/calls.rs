//! llm_calls list + detail views.

use duckdb::Connection;

use crate::ui::{clear_screen, fmt_f64, fmt_opt, pretty_json, read_line, truncate};

struct CallSummary {
    row_num: usize,
    id: String,
    timestamp: String,
    client_ip: String,
    server: String,
    wire_api: String,
    model: String,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    ttft_ms: Option<f64>,
    status: String,
}

fn list(conn: &Connection, limit: usize) -> Vec<CallSummary> {
    let sql = format!(
        "SELECT id, strftime(request_time, '%m-%d %H:%M:%S'), \
         client_ip, server_ip || ':' || server_port, wire_api, model, \
         input_tokens, output_tokens, ttft_ms, status_code, finish_reason, is_stream \
         FROM llm_calls ORDER BY request_time DESC LIMIT {limit}"
    );
    let mut stmt = conn.prepare(&sql).expect("failed to prepare query");
    let rows = stmt
        .query_map([], |row| {
            let status_code = row.get::<_, Option<u16>>(9)?;
            let finish_reason = row.get::<_, Option<String>>(10)?;
            let is_stream = row.get::<_, bool>(11)?;
            let status = match (status_code, finish_reason.as_deref(), is_stream) {
                (Some(code), _, _) if code >= 400 => format!("{code} ERR"),
                (_, Some(fr), _) => fr.to_uppercase(),
                (_, _, true) => "STREAM".into(),
                _ => "DONE".into(),
            };
            Ok(CallSummary {
                row_num: 0,
                id: row.get(0)?,
                timestamp: row.get(1)?,
                client_ip: row.get(2)?,
                server: row.get(3)?,
                wire_api: row.get(4)?,
                model: row.get(5)?,
                input_tokens: row.get(6)?,
                output_tokens: row.get(7)?,
                ttft_ms: row.get(8)?,
                status,
            })
        })
        .expect("failed to query llm_calls");

    rows.enumerate()
        .filter_map(|(i, r)| match r {
            Ok(mut c) => {
                c.row_num = i + 1;
                Some(c)
            }
            Err(e) => {
                if i == 0 {
                    eprintln!("Row read error: {e}");
                }
                None
            }
        })
        .collect()
}

fn print_list(calls: &[CallSummary]) {
    if calls.is_empty() {
        println!("No llm_calls records found.");
        return;
    }
    println!(
        "{:>4}  {:<14}  {:<15}  {:<21}  {:<10}  {:<16}  {:>5}  {:>6}  {:>9}  {}",
        "#",
        "TIMESTAMP",
        "IP",
        "SERVER",
        "WIRE_API",
        "MODEL",
        "INPUT",
        "OUTPUT",
        "TTFT(ms)",
        "STATUS"
    );
    for c in calls {
        println!(
            "{:>4}  {:<14}  {:<15}  {:<21}  {:<10}  {:<16}  {:>5}  {:>6}  {:>9}  {}",
            c.row_num,
            c.timestamp,
            c.client_ip,
            c.server,
            c.wire_api,
            truncate(&c.model, 16),
            c.input_tokens.map(|t| t.to_string()).unwrap_or("0".into()),
            c.output_tokens.map(|t| t.to_string()).unwrap_or("0".into()),
            fmt_f64(c.ttft_ms),
            c.status,
        );
    }
    println!();
}

struct CallDetail {
    id: String,
    tenant_id: Option<String>,
    client_ip: String,
    client_port: u16,
    server_ip: String,
    server_port: u16,
    request_time: String,
    response_time: Option<String>,
    complete_time: Option<String>,
    wire_api: String,
    model: String,
    api_type: String,
    is_stream: bool,
    request_path: String,
    status_code: Option<u16>,
    finish_reason: Option<String>,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    total_tokens: Option<u32>,
    ttft_ms: Option<f64>,
    e2e_latency_ms: Option<f64>,
    request_body: Option<String>,
    response_body: Option<String>,
    response_id: Option<String>,
    request_headers: Option<String>,
    response_headers: Option<String>,
}

fn load_detail(conn: &Connection, id: &str) -> Option<CallDetail> {
    let sql = "SELECT id, tenant_id, client_ip, client_port, server_ip, server_port, \
               CAST(request_time AS VARCHAR), CAST(response_time AS VARCHAR), \
               CAST(complete_time AS VARCHAR), \
               wire_api, model, api_type, is_stream, request_path, \
               status_code, finish_reason, \
               input_tokens, output_tokens, total_tokens, \
               ttft_ms, e2e_latency_ms, \
               request_body, response_body, \
               response_id, request_headers, response_headers \
               FROM llm_calls WHERE id = ?";
    let mut stmt = conn.prepare(sql).expect("failed to prepare detail query");
    stmt.query_row([id], |row| {
        Ok(CallDetail {
            id: row.get(0)?,
            tenant_id: row.get(1)?,
            client_ip: row.get(2)?,
            client_port: row.get(3)?,
            server_ip: row.get(4)?,
            server_port: row.get(5)?,
            request_time: row.get(6)?,
            response_time: row.get(7)?,
            complete_time: row.get(8)?,
            wire_api: row.get(9)?,
            model: row.get(10)?,
            api_type: row.get(11)?,
            is_stream: row.get(12)?,
            request_path: row.get(13)?,
            status_code: row.get(14)?,
            finish_reason: row.get(15)?,
            input_tokens: row.get(16)?,
            output_tokens: row.get(17)?,
            total_tokens: row.get(18)?,
            ttft_ms: row.get(19)?,
            e2e_latency_ms: row.get(20)?,
            request_body: row.get(21)?,
            response_body: row.get(22)?,
            response_id: row.get(23)?,
            request_headers: row.get(24)?,
            response_headers: row.get(25)?,
        })
    })
    .ok()
}

fn print_detail(d: &CallDetail) {
    println!("{}", "=".repeat(80));
    println!("  LLM Call Detail");
    println!("{}", "=".repeat(80));
    println!("  ID:             {}", d.id);
    println!("  Response ID:    {}", fmt_opt(d.response_id.as_deref()));
    println!("  Tenant:         {}", fmt_opt(d.tenant_id.as_deref()));
    println!("  Client:         {}:{}", d.client_ip, d.client_port);
    println!("  Server:         {}:{}", d.server_ip, d.server_port);
    println!("  Request Time:   {}", d.request_time);
    println!("  Response Time:  {}", fmt_opt(d.response_time.as_deref()));
    println!("  Complete Time:  {}", fmt_opt(d.complete_time.as_deref()));
    println!("{}", "-".repeat(80));
    println!("  Wire API:       {}", d.wire_api);
    println!("  Model:          {}", d.model);
    println!("  API Type:       {}", d.api_type);
    println!(
        "  Stream:         {}",
        if d.is_stream { "yes" } else { "no" }
    );
    println!("  Request Path:   {}", d.request_path);
    println!("  Status Code:    {}", fmt_opt(d.status_code));
    println!("  Finish Reason:  {}", fmt_opt(d.finish_reason.as_deref()));
    println!("{}", "-".repeat(80));
    println!("  Input Tokens:   {}", fmt_opt(d.input_tokens));
    println!("  Output Tokens:  {}", fmt_opt(d.output_tokens));
    println!("  Total Tokens:   {}", fmt_opt(d.total_tokens));
    println!("  TTFT (ms):      {}", fmt_f64(d.ttft_ms));
    println!("  E2E (ms):       {}", fmt_f64(d.e2e_latency_ms));
    println!("{}", "-".repeat(80));

    if let Some(ref headers) = d.request_headers {
        println!("  Request Headers:");
        println!("{}", pretty_json(headers));
    }
    if let Some(ref body) = d.request_body {
        println!("  Request Body:");
        println!("{}", pretty_json(body));
    }
    if let Some(ref headers) = d.response_headers {
        println!("  Response Headers:");
        println!("{}", pretty_json(headers));
    }
    if let Some(ref body) = d.response_body {
        println!("  Response Body:");
        println!("{}", pretty_json(body));
    }
    println!("{}", "=".repeat(80));
}

/// List → pick row → detail → back. Returns when user presses q/Enter.
pub fn run(conn: &Connection, limit: usize) {
    loop {
        clear_screen();
        println!("─── llm_calls (latest {limit}) ───");
        let calls = list(conn, limit);
        print_list(&calls);

        if calls.is_empty() {
            read_line("Press Enter to go back...");
            return;
        }

        let input = read_line("Enter row # for detail (q to go back): ");
        if input.eq_ignore_ascii_case("q") || input.is_empty() {
            return;
        }

        match input.parse::<usize>() {
            Ok(n) if n >= 1 && n <= calls.len() => {
                clear_screen();
                if let Some(detail) = load_detail(conn, &calls[n - 1].id) {
                    print_detail(&detail);
                } else {
                    eprintln!("  Record not found.");
                }
                read_line("Press Enter to go back to list...");
            }
            _ => {
                println!(
                    "Invalid selection. Enter a number between 1 and {}.",
                    calls.len()
                );
                read_line("Press Enter to continue...");
            }
        }
    }
}
