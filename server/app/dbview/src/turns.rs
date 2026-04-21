//! agent_turns list + detail views.

use duckdb::Connection;
use ts_llm::agents::build_default_registry;
use ts_llm::model::{ApiType, LlmCall};
use ts_llm::wire_apis as wa;

use crate::ui::{
    clear_screen, fmt_duration_ms, fmt_opt, fmt_tokens, pretty_json, read_line, truncate,
};

struct TurnSummary {
    row_num: usize,
    turn_id: String,
    session_id: String,
    start_time: String,
    wire_api: String,
    agent_kind: String,
    call_count: u32,
    duration_ms: u64,
    input_tokens: u64,
    output_tokens: u64,
    status: String,
}

fn list(conn: &Connection, limit: usize) -> Vec<TurnSummary> {
    let sql = format!(
        "SELECT turn_id, session_id, \
         strftime(start_time, '%m-%d %H:%M:%S'), \
         wire_api, agent_kind, call_count, duration_ms, \
         total_input_tokens, total_output_tokens, status \
         FROM agent_turns ORDER BY start_time DESC LIMIT {limit}"
    );
    let mut stmt = conn.prepare(&sql).expect("failed to prepare turns query");
    let rows = stmt
        .query_map([], |row| {
            Ok(TurnSummary {
                row_num: 0,
                turn_id: row.get(0)?,
                session_id: row.get(1)?,
                start_time: row.get(2)?,
                wire_api: row.get(3)?,
                agent_kind: row.get(4)?,
                call_count: row.get(5)?,
                duration_ms: row.get(6)?,
                input_tokens: row.get(7)?,
                output_tokens: row.get(8)?,
                status: row.get(9)?,
            })
        })
        .expect("failed to query agent_turns");

    rows.enumerate()
        .filter_map(|(i, r)| match r {
            Ok(mut t) => {
                t.row_num = i + 1;
                Some(t)
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

fn print_list(turns: &[TurnSummary]) {
    if turns.is_empty() {
        println!("No agent_turns records found.");
        return;
    }
    println!(
        "{:>4}  {:<14}  {:<22}  {:<22}  {:<10}  {:<12}  {:>5}  {:>8}  {:>6}  {:>7}  {}",
        "#",
        "START",
        "TURN_ID",
        "SESSION_ID",
        "WIRE_API",
        "CLIENT",
        "CALLS",
        "DURATION",
        "IN",
        "OUT",
        "STATUS"
    );
    for t in turns {
        println!(
            "{:>4}  {:<14}  {:<22}  {:<22}  {:<10}  {:<12}  {:>5}  {:>8}  {:>6}  {:>7}  {}",
            t.row_num,
            t.start_time,
            truncate(&t.turn_id, 22),
            truncate(&t.session_id, 22),
            truncate(&t.wire_api, 10),
            truncate(&t.agent_kind, 12),
            t.call_count,
            fmt_duration_ms(t.duration_ms),
            fmt_tokens(t.input_tokens),
            fmt_tokens(t.output_tokens),
            t.status,
        );
    }
    println!();
}

struct TurnDetail {
    turn_id: String,
    session_id: String,
    tenant_id: Option<String>,
    wire_api: String,
    agent_kind: String,
    start_time: String,
    end_time: String,
    duration_ms: u64,
    call_count: u32,
    models_used: Option<String>,
    subagents_used: Option<String>,
    total_input_tokens: u64,
    total_output_tokens: u64,
    total_cache_read_input_tokens: u64,
    total_cache_creation_input_tokens: u64,
    total_cost_usd: Option<f64>,
    status: String,
    final_finish_reason: Option<String>,
    user_input_preview: Option<String>,
    user_call_id: Option<String>,
    final_answer_preview: Option<String>,
    final_call_id: Option<String>,
    metadata: Option<String>,
}

fn load_detail(conn: &Connection, turn_id: &str) -> Option<TurnDetail> {
    let sql = "SELECT turn_id, session_id, tenant_id, wire_api, agent_kind, \
               CAST(start_time AS VARCHAR), CAST(end_time AS VARCHAR), \
               duration_ms, call_count, models_used, subagents_used, \
               total_input_tokens, total_output_tokens, total_cache_read_input_tokens, \
               total_cache_creation_input_tokens, \
               total_cost_usd, status, final_finish_reason, \
               user_input_preview, user_call_id, \
               final_answer_preview, final_call_id, metadata \
               FROM agent_turns WHERE turn_id = ?";
    let mut stmt = conn
        .prepare(sql)
        .expect("failed to prepare turn detail query");
    stmt.query_row([turn_id], |row| {
        Ok(TurnDetail {
            turn_id: row.get(0)?,
            session_id: row.get(1)?,
            tenant_id: row.get(2)?,
            wire_api: row.get(3)?,
            agent_kind: row.get(4)?,
            start_time: row.get(5)?,
            end_time: row.get(6)?,
            duration_ms: row.get(7)?,
            call_count: row.get(8)?,
            models_used: row.get(9)?,
            subagents_used: row.get(10)?,
            total_input_tokens: row.get(11)?,
            total_output_tokens: row.get(12)?,
            total_cache_read_input_tokens: row.get(13)?,
            total_cache_creation_input_tokens: row.get(14)?,
            total_cost_usd: row.get(15)?,
            status: row.get(16)?,
            final_finish_reason: row.get(17)?,
            user_input_preview: row.get(18)?,
            user_call_id: row.get(19)?,
            final_answer_preview: row.get(20)?,
            final_call_id: row.get(21)?,
            metadata: row.get(22)?,
        })
    })
    .ok()
}

struct ChildCall {
    request_time: String,
    model: String,
    input_tokens: Option<u32>,
    output_tokens: Option<u32>,
    finish_reason: Option<String>,
    status_code: Option<u16>,
}

fn load_child_calls(conn: &Connection, turn_id: &str) -> Vec<ChildCall> {
    // Resolve call_ids for this turn from agent_turns, then join to llm_calls.
    // call_ids is a JSON array of strings stored on agent_turns.
    let sql = "SELECT strftime(c.request_time, '%m-%d %H:%M:%S'), c.model, \
               c.input_tokens, c.output_tokens, c.finish_reason, c.status_code \
               FROM llm_calls c \
               JOIN (SELECT UNNEST(json_extract_string(call_ids, '$[*]')) AS cid \
                     FROM agent_turns WHERE turn_id = ?) ids ON c.id = ids.cid \
               ORDER BY c.request_time";
    let Ok(mut stmt) = conn.prepare(sql) else {
        return Vec::new();
    };
    let rows = stmt
        .query_map([turn_id], |row| {
            Ok(ChildCall {
                request_time: row.get(0)?,
                model: row.get(1)?,
                input_tokens: row.get(2)?,
                output_tokens: row.get(3)?,
                finish_reason: row.get(4)?,
                status_code: row.get(5)?,
            })
        })
        .ok();
    match rows {
        Some(iter) => iter.filter_map(|r| r.ok()).collect(),
        None => Vec::new(),
    }
}

struct CallBodies {
    request_body: Option<String>,
    response_body: Option<String>,
}

fn load_call_bodies(conn: &Connection, call_id: &str) -> Option<CallBodies> {
    let sql = "SELECT request_body, response_body FROM llm_calls WHERE id = ?";
    let mut stmt = conn.prepare(sql).ok()?;
    stmt.query_row([call_id], |row| {
        Ok(CallBodies {
            request_body: row.get(0)?,
            response_body: row.get(1)?,
        })
    })
    .ok()
}

enum ExtractKind {
    User,
    Assistant,
}

fn extract_with_profile(
    agent_kind: &str,
    request_body: Option<String>,
    response_body: Option<String>,
    kind: ExtractKind,
) -> Option<String> {
    let registry = build_default_registry();
    let profile = registry.find_by_name(agent_kind)?;
    let call = LlmCall {
        stream_id: String::new(),
        id: String::new(),
        wire_api: wa::ANTHROPIC,
        model: String::new(),
        api_type: ApiType::Chat,
        tenant_id: None,
        request_time: 0,
        response_time: None,
        complete_time: None,
        request_path: String::new(),
        is_stream: false,
        request_body,
        status_code: None,
        finish_reason: None,
        response_body,
        input_tokens: None,
        output_tokens: None,
        total_tokens: None,
        cache_read_input_tokens: None,
        cache_creation_input_tokens: None,
        ttfb_ms: None,
        e2e_latency_ms: None,
        client_ip: "0.0.0.0".parse().unwrap(),
        client_port: 0,
        server_ip: "0.0.0.0".parse().unwrap(),
        server_port: 0,
        response_id: None,
        request_headers: Vec::new(),
        response_headers: Vec::new(),
    };
    match kind {
        ExtractKind::User => profile.extract_user_input(&call),
        ExtractKind::Assistant => profile.extract_assistant_text(&call),
    }
}

fn print_detail(conn: &Connection, d: &TurnDetail, calls: &[ChildCall]) {
    println!("{}", "=".repeat(80));
    println!("  LLM Turn Detail");
    println!("{}", "=".repeat(80));
    println!("  Turn ID:        {}", d.turn_id);
    println!("  Session ID:     {}", d.session_id);
    println!("  Tenant:         {}", fmt_opt(d.tenant_id.as_deref()));
    println!("  Wire API:       {}", d.wire_api);
    println!("  Client Kind:    {}", d.agent_kind);
    println!("  Start:          {}", d.start_time);
    println!("  End:            {}", d.end_time);
    println!(
        "  Duration:       {} ({} ms)",
        fmt_duration_ms(d.duration_ms),
        d.duration_ms
    );
    println!("{}", "-".repeat(80));
    println!("  Status:         {}", d.status);
    println!(
        "  Final Finish:   {}",
        fmt_opt(d.final_finish_reason.as_deref())
    );
    println!("  Call Count:     {}", d.call_count);
    println!("  Input Tokens:   {}", fmt_tokens(d.total_input_tokens));
    println!("  Output Tokens:  {}", fmt_tokens(d.total_output_tokens));
    println!(
        "  Cache Read:     {}",
        fmt_tokens(d.total_cache_read_input_tokens)
    );
    println!(
        "  Cache Create:   {}",
        fmt_tokens(d.total_cache_creation_input_tokens)
    );
    println!(
        "  Cost (USD):     {}",
        d.total_cost_usd
            .map(|c| format!("{c:.4}"))
            .unwrap_or_else(|| "-".into())
    );
    println!("{}", "-".repeat(80));

    let full_user_input = match d.user_call_id.as_deref() {
        Some(cid) => load_call_bodies(conn, cid).and_then(|b| {
            extract_with_profile(&d.agent_kind, b.request_body, None, ExtractKind::User)
        }),
        None => None,
    };
    let display_user = full_user_input
        .as_deref()
        .or(d.user_input_preview.as_deref());
    if let Some(s) = display_user {
        let header = match (&full_user_input, d.user_call_id.as_deref()) {
            (Some(_), Some(id)) => format!("  User Input (full, call={id}):"),
            _ => "  User Input (preview):".to_string(),
        };
        println!("{header}");
        for line in s.lines() {
            println!("    {line}");
        }
        println!();
    }
    let full_final_answer = match d.final_call_id.as_deref() {
        Some(cid) => load_call_bodies(conn, cid).and_then(|b| {
            extract_with_profile(&d.agent_kind, None, b.response_body, ExtractKind::Assistant)
        }),
        None => None,
    };
    let display_final = full_final_answer
        .as_deref()
        .or(d.final_answer_preview.as_deref());
    if let Some(s) = display_final {
        let header = match (&full_final_answer, d.final_call_id.as_deref()) {
            (Some(_), Some(id)) => format!("  Final Answer (full, call={id}):"),
            _ => "  Final Answer (preview):".to_string(),
        };
        println!("{header}");
        for line in s.lines() {
            println!("    {line}");
        }
        println!();
    } else if let Some(ref id) = d.final_call_id {
        println!("  Final Call: {id}");
    }

    if let Some(ref s) = d.models_used {
        println!("  Models Used:");
        println!("{}", pretty_json(s));
    }
    if let Some(ref s) = d.subagents_used {
        println!("  Subagents Used:");
        println!("{}", pretty_json(s));
    }
    if let Some(ref s) = d.metadata {
        println!("  Metadata:");
        println!("{}", pretty_json(s));
    }
    println!("{}", "-".repeat(80));

    println!("  Calls in this turn ({}):", calls.len());
    if calls.is_empty() {
        println!("    (none found — call_ids on agent_turns may be empty or calls not persisted)");
    } else {
        println!(
            "    {:<14}  {:<24}  {:>6}  {:>6}  {:>5}  {}",
            "TIME", "MODEL", "INPUT", "OUTPUT", "CODE", "FINISH"
        );
        for c in calls {
            println!(
                "    {:<14}  {:<24}  {:>6}  {:>6}  {:>5}  {}",
                c.request_time,
                truncate(&c.model, 24),
                fmt_opt(c.input_tokens),
                fmt_opt(c.output_tokens),
                fmt_opt(c.status_code),
                fmt_opt(c.finish_reason.as_deref()),
            );
        }
    }
    println!("{}", "=".repeat(80));
}

pub fn run(conn: &Connection, limit: usize) {
    loop {
        clear_screen();
        println!("─── agent_turns (latest {limit}) ───");
        let turns = list(conn, limit);
        print_list(&turns);

        if turns.is_empty() {
            read_line("Press Enter to go back...");
            return;
        }

        let input = read_line("Enter row # for detail (q to go back): ");
        if input.eq_ignore_ascii_case("q") || input.is_empty() {
            return;
        }

        match input.parse::<usize>() {
            Ok(n) if n >= 1 && n <= turns.len() => {
                clear_screen();
                let id = &turns[n - 1].turn_id;
                if let Some(detail) = load_detail(conn, id) {
                    let children = load_child_calls(conn, id);
                    print_detail(conn, &detail, &children);
                } else {
                    eprintln!("  Turn not found.");
                }
                read_line("Press Enter to go back to list...");
            }
            _ => {
                println!(
                    "Invalid selection. Enter a number between 1 and {}.",
                    turns.len()
                );
                read_line("Press Enter to continue...");
            }
        }
    }
}
