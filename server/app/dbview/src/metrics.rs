//! llm_metrics list view. Metrics are aggregated rows; no per-row detail.

use duckdb::Connection;

use crate::ui::{clear_screen, fmt_f64, fmt_tokens, read_line, truncate};

struct MetricRow {
    row_num: usize,
    timestamp: String,
    granularity: String,
    wire_api: String,
    model: String,
    server_ip: String,
    request_count: u64,
    error_count: u64,
    total_input_tokens: u64,
    total_output_tokens: u64,
    ttft_p95: Option<f64>,
    e2e_p95: Option<f64>,
    tpot_avg: Option<f64>,
}

fn list(conn: &Connection, limit: usize) -> Vec<MetricRow> {
    let sql = format!(
        "SELECT strftime(timestamp, '%m-%d %H:%M:%S'), granularity, \
         wire_api, model, server_ip, \
         request_count, error_count, \
         total_input_tokens, total_output_tokens, \
         ttft_p95, e2e_p95, tpot_avg \
         FROM llm_metrics ORDER BY timestamp DESC LIMIT {limit}"
    );
    let mut stmt = conn.prepare(&sql).expect("failed to prepare metrics query");
    let rows = stmt
        .query_map([], |row| {
            Ok(MetricRow {
                row_num: 0,
                timestamp: row.get(0)?,
                granularity: row.get(1)?,
                wire_api: row.get(2)?,
                model: row.get(3)?,
                server_ip: row.get(4)?,
                request_count: row.get(5)?,
                error_count: row.get(6)?,
                total_input_tokens: row.get(7)?,
                total_output_tokens: row.get(8)?,
                ttft_p95: row.get(9)?,
                e2e_p95: row.get(10)?,
                tpot_avg: row.get(11)?,
            })
        })
        .expect("failed to query llm_metrics");

    rows.enumerate()
        .filter_map(|(i, r)| match r {
            Ok(mut m) => {
                m.row_num = i + 1;
                Some(m)
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

fn print_list(rows: &[MetricRow]) {
    if rows.is_empty() {
        println!("No llm_metrics records found.");
        return;
    }
    println!(
        "{:>4}  {:<14}  {:<4}  {:<10}  {:<20}  {:<15}  {:>5}  {:>5}  {:>6}  {:>6}  {:>9}  {:>9}  {:>10}",
        "#", "TIMESTAMP", "GR", "WIRE_API", "MODEL", "SERVER_IP",
        "REQ", "ERR", "IN", "OUT", "TTFT_P95", "E2E_P95", "TPOT_AVG"
    );
    for m in rows {
        println!(
            "{:>4}  {:<14}  {:<4}  {:<10}  {:<20}  {:<15}  {:>5}  {:>5}  {:>6}  {:>6}  {:>9}  {:>9}  {:>10}",
            m.row_num,
            m.timestamp,
            truncate(&m.granularity, 4),
            truncate(&m.wire_api, 10),
            truncate(&m.model, 20),
            truncate(&m.server_ip, 15),
            m.request_count,
            m.error_count,
            fmt_tokens(m.total_input_tokens),
            fmt_tokens(m.total_output_tokens),
            fmt_f64(m.ttft_p95),
            fmt_f64(m.e2e_p95),
            fmt_f64(m.tpot_avg),
        );
    }
    println!();
    println!("  GR = granularity (1m/5m/...); WIRE_API='*' and MODEL='*' are rollup rows.");
    println!();
}

pub fn run(conn: &Connection, limit: usize) {
    clear_screen();
    println!("─── llm_metrics (latest {limit}) ───");
    let rows = list(conn, limit);
    print_list(&rows);
    read_line("Press Enter to go back...");
}
