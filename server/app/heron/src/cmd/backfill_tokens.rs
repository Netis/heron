//! `heron backfill-tokens` — fill in missing per-call token counts on
//! historical rows by re-tokenizing their request/response bodies via the
//! same wire-api walkers + tiktoken cl100k estimator the live processor
//! uses for fresh traffic.
//!
//! Affects rows where:
//!   * status_code is 2xx (or NULL — we still try, status filtering handled
//!     defensively per row)
//!   * input_tokens IS NULL OR input_tokens = 0
//!   * AND output_tokens IS NULL OR output_tokens = 0
//!   * response_body IS NOT NULL AND length > 0
//!
//! Idempotent — a re-run finds no zero-token rows and exits with a no-op
//! summary. Refuses to run if the live heron daemon holds the DB lock.
//!
//! Exit codes:
//!   0 — success (rows updated, or zero rows to update)
//!   1 — db open / IO / SQL error
//!   2 — invalid arguments
//!
//! Logging:
//!   INFO  — start banner, end summary
//!   DEBUG (`-v`) — per-row "estimated call_id=… in=N out=M" line

use std::path::{Path, PathBuf};
use std::sync::Arc;

use clap::Args;
use duckdb::Connection;
use serde_json::Value;

use h_llm::token_estimator::{CL100kEstimator, TokenEstimator};
use h_llm::wire_apis as wa;

#[derive(Debug, Args)]
pub struct BackfillTokensArgs {
    /// DuckDB file to scan. If omitted, falls back to `XDG_DATA_HOME` /
    /// `~/.local/share/heron/data/heron.duckdb`.
    #[arg(long)]
    pub db_path: Option<PathBuf>,

    /// Don't write — just log what would be updated.
    #[arg(long)]
    pub dry_run: bool,

    /// Cap the number of rows touched (after filtering). Useful for
    /// progressively rolling out on a large database. 0 means no cap.
    #[arg(long, default_value_t = 0)]
    pub limit: u64,

    /// Skip the safety backup (`heron.duckdb.pre_backfill_tokens_backup`).
    /// Default: take a one-time backup if not already present.
    #[arg(long)]
    pub skip_backup: bool,
}

pub fn run(args: &BackfillTokensArgs) -> i32 {
    let db_path = match resolve_db_path(args.db_path.as_deref()) {
        Ok(p) => p,
        Err(e) => {
            tracing::error!("could not resolve db path: {e}");
            return 2;
        }
    };
    if !db_path.exists() {
        tracing::error!("db file does not exist: {}", db_path.display());
        return 1;
    }

    if !args.dry_run && !args.skip_backup {
        if let Err(e) = ensure_backup(&db_path) {
            tracing::error!("backup failed: {e}");
            return 1;
        }
    }

    let conn = match Connection::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(
                "failed to open {}: {e}\n\
                 Hint: stop the live heron daemon first — DuckDB \
                 takes an exclusive lock while the writer is open.",
                db_path.display()
            );
            return 1;
        }
    };

    let estimator: Arc<dyn TokenEstimator> = Arc::new(CL100kEstimator::new());

    let limit_clause = if args.limit > 0 {
        format!(" LIMIT {}", args.limit)
    } else {
        String::new()
    };
    let select_sql = format!(
        "SELECT id, wire_api, status_code, request_body, response_body \
         FROM spans \
         WHERE COALESCE(input_tokens, 0) = 0 \
           AND COALESCE(output_tokens, 0) = 0 \
           AND response_body IS NOT NULL \
           AND length(response_body) > 0\
         ORDER BY request_time ASC{limit_clause}"
    );
    tracing::info!(
        "backfill-tokens: scanning {}{}",
        db_path.display(),
        if args.dry_run { " (dry-run)" } else { "" }
    );

    let mut stmt = match conn.prepare(&select_sql) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to prepare select: {e}");
            return 1;
        }
    };

    let rows = match stmt.query_map([], |row| {
        let id: String = row.get(0)?;
        let wire_api: String = row.get(1)?;
        let status: Option<u16> = row.get(2)?;
        let req_body: Option<String> = row.get(3)?;
        let resp_body: Option<String> = row.get(4)?;
        Ok((id, wire_api, status, req_body, resp_body))
    }) {
        Ok(it) => it,
        Err(e) => {
            tracing::error!("failed to execute select: {e}");
            return 1;
        }
    };

    let mut scanned: u64 = 0;
    let mut updated: u64 = 0;
    let mut skipped_status: u64 = 0;
    let mut skipped_no_body: u64 = 0;
    let mut skipped_estimate_zero: u64 = 0;
    let mut skipped_unknown_wire: u64 = 0;

    let mut update_stmt = match conn.prepare(
        "UPDATE spans SET input_tokens = ?, output_tokens = ?, total_tokens = ? WHERE id = ?",
    ) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("failed to prepare update: {e}");
            return 1;
        }
    };

    let row_iter: Vec<_> = rows.filter_map(|r| r.ok()).collect();
    for (id, wire_api, status, req_body, resp_body) in row_iter {
        scanned += 1;
        // Defensive status filter: live processor only estimates on 2xx,
        // mirror that here so a 5xx error body's noise doesn't get a count.
        if let Some(s) = status {
            if !(200..300).contains(&s) {
                skipped_status += 1;
                continue;
            }
        }
        let resp_body_str = match resp_body {
            Some(s) if !s.is_empty() => s,
            _ => {
                skipped_no_body += 1;
                continue;
            }
        };
        let resp_json: Value = match serde_json::from_str(&resp_body_str) {
            Ok(v) => v,
            Err(_) => {
                skipped_no_body += 1;
                continue;
            }
        };
        let req_json: Value = req_body
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(Value::Null);

        let (est_in, est_out) = match wire_api.as_str() {
            wa::OPENAI_CHAT => (
                wa::openai::chat::estimate_input_tokens(&req_json, estimator.as_ref()),
                wa::openai::chat::estimate_output_tokens(&resp_json, estimator.as_ref()),
            ),
            wa::OPENAI_RESPONSES => (
                wa::openai::responses::estimate_input_tokens(&req_json, estimator.as_ref()),
                wa::openai::responses::estimate_output_tokens(&resp_json, estimator.as_ref()),
            ),
            wa::ANTHROPIC => (
                wa::anthropic::estimate_input_tokens(&req_json, estimator.as_ref()),
                wa::anthropic::estimate_output_tokens(&resp_json, estimator.as_ref()),
            ),
            other => {
                tracing::debug!("unknown wire_api={other} on call_id={id}; skipping");
                skipped_unknown_wire += 1;
                continue;
            }
        };

        if est_in == 0 && est_out == 0 {
            skipped_estimate_zero += 1;
            continue;
        }

        tracing::debug!(
            call_id = %id,
            wire_api = %wire_api,
            in_tokens = est_in,
            out_tokens = est_out,
            "would update tokens"
        );

        if !args.dry_run {
            let total = est_in.saturating_add(est_out);
            if let Err(e) = update_stmt.execute(duckdb::params![est_in, est_out, total, id]) {
                tracing::error!(call_id = %id, "update failed: {e}");
                continue;
            }
        }
        updated += 1;
    }

    drop(update_stmt);
    drop(stmt);
    if !args.dry_run {
        if let Err(e) = conn.execute("CHECKPOINT", []) {
            tracing::warn!("CHECKPOINT failed (changes still committed via WAL): {e}");
        }
    }

    tracing::info!(
        scanned,
        updated,
        skipped_status,
        skipped_no_body,
        skipped_estimate_zero,
        skipped_unknown_wire,
        dry_run = args.dry_run,
        "backfill-tokens done"
    );
    0
}

fn resolve_db_path(arg: Option<&Path>) -> Result<PathBuf, String> {
    if let Some(p) = arg {
        return Ok(p.to_path_buf());
    }
    let home = std::env::var("HOME").map_err(|_| "HOME not set".to_string())?;
    Ok(PathBuf::from(home).join(".local/share/heron/data/heron.duckdb"))
}

fn ensure_backup(db_path: &Path) -> Result<(), String> {
    let mut backup = db_path.to_path_buf();
    let new_ext = match db_path.extension() {
        Some(ext) => format!("{}.pre_backfill_tokens_backup", ext.to_string_lossy()),
        None => "pre_backfill_tokens_backup".to_string(),
    };
    backup.set_extension(new_ext);
    if backup.exists() {
        tracing::info!("backup already present at {} (skipping)", backup.display());
        return Ok(());
    }
    tracing::info!("backing up {} -> {}", db_path.display(), backup.display());
    std::fs::copy(db_path, &backup).map_err(|e| format!("copy failed: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use duckdb::Connection;
    use tempfile::tempdir;

    /// Bootstrap a temp DuckDB matching the production spans schema
    /// (only the columns we actually read/write — minimal but matching
    /// types). Returns (path, conn).
    fn make_temp_db(dir: &std::path::Path) -> (PathBuf, Connection) {
        let db = dir.join("test.duckdb");
        let c = Connection::open(&db).expect("open");
        c.execute_batch(
            "CREATE TABLE spans (
                id VARCHAR PRIMARY KEY,
                source_id VARCHAR DEFAULT '',
                client_ip VARCHAR DEFAULT '',
                client_port USMALLINT DEFAULT 0,
                server_ip VARCHAR DEFAULT '',
                server_port USMALLINT DEFAULT 0,
                request_time TIMESTAMP DEFAULT now(),
                wire_api VARCHAR,
                model VARCHAR DEFAULT '',
                api_type VARCHAR DEFAULT 'chat',
                is_stream BOOLEAN DEFAULT false,
                request_path VARCHAR DEFAULT '',
                status_code USMALLINT,
                input_tokens UINTEGER,
                output_tokens UINTEGER,
                total_tokens UINTEGER,
                request_body VARCHAR,
                response_body VARCHAR
            );",
        )
        .unwrap();
        (db, c)
    }

    fn insert(
        c: &Connection,
        id: &str,
        wire_api: &str,
        status: u16,
        req: &str,
        resp: &str,
        in_t: Option<u32>,
        out_t: Option<u32>,
    ) {
        c.execute(
            "INSERT INTO spans (id, wire_api, status_code, request_body, response_body, input_tokens, output_tokens) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
            duckdb::params![id, wire_api, status, req, resp, in_t, out_t],
        )
        .unwrap();
    }

    fn token_pair(c: &Connection, id: &str) -> (Option<u32>, Option<u32>) {
        c.query_row(
            "SELECT input_tokens, output_tokens FROM spans WHERE id = ?",
            duckdb::params![id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap()
    }

    #[test]
    fn backfill_updates_zero_token_rows_and_leaves_wire_rows_alone() {
        let dir = tempdir().unwrap();
        let (db_path, conn) = make_temp_db(dir.path());

        // 5 zero rows (LiteLLM-shape — no usage in response).
        for i in 0..5 {
            insert(
                &conn,
                &format!("zero-{i}"),
                "openai-chat",
                200,
                &format!(
                    r#"{{"model":"exp_model","messages":[{{"role":"user","content":"q{i}"}}]}}"#
                ),
                &format!(
                    r#"{{"choices":[{{"message":{{"role":"assistant","content":"answer {i} with some longer text so the estimator returns something non-trivial"}}}}]}}"#
                ),
                Some(0),
                Some(0),
            );
        }
        // 5 wire-usage rows that must NOT be modified.
        for i in 0..5 {
            insert(
                &conn,
                &format!("wire-{i}"),
                "openai-chat",
                200,
                &format!(r#"{{"model":"gpt-4","messages":[{{"role":"user","content":"q{i}"}}]}}"#),
                &format!(
                    r#"{{"choices":[{{"message":{{"role":"assistant","content":"answer {i}"}}}}],"usage":{{"prompt_tokens":7,"completion_tokens":4,"total_tokens":11}}}}"#
                ),
                Some(7),
                Some(4),
            );
        }
        drop(conn);

        let args = BackfillTokensArgs {
            db_path: Some(db_path.clone()),
            dry_run: false,
            limit: 0,
            skip_backup: true,
        };
        let code = run(&args);
        assert_eq!(code, 0);

        // Verify zero rows now have tokens > 0.
        let conn = Connection::open(&db_path).unwrap();
        for i in 0..5 {
            let (it, ot) = token_pair(&conn, &format!("zero-{i}"));
            assert!(it.unwrap_or(0) > 0, "zero-{i} input not estimated");
            assert!(ot.unwrap_or(0) > 0, "zero-{i} output not estimated");
        }
        // Wire rows untouched.
        for i in 0..5 {
            let (it, ot) = token_pair(&conn, &format!("wire-{i}"));
            assert_eq!(it, Some(7), "wire-{i} input mutated");
            assert_eq!(ot, Some(4), "wire-{i} output mutated");
        }
        drop(conn);

        // Re-run is idempotent (touches 0 rows).
        let code2 = run(&args);
        assert_eq!(code2, 0);
        let conn = Connection::open(&db_path).unwrap();
        for i in 0..5 {
            let (it1, ot1) = token_pair(&conn, &format!("zero-{i}"));
            assert!(it1.unwrap_or(0) > 0); // still > 0
            assert!(ot1.unwrap_or(0) > 0);
        }
    }

    #[test]
    fn dry_run_makes_no_changes() {
        let dir = tempdir().unwrap();
        let (db_path, conn) = make_temp_db(dir.path());
        insert(
            &conn,
            "x",
            "openai-chat",
            200,
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
            r#"{"choices":[{"message":{"role":"assistant","content":"longer answer here"}}]}"#,
            Some(0),
            Some(0),
        );
        drop(conn);

        let args = BackfillTokensArgs {
            db_path: Some(db_path.clone()),
            dry_run: true,
            limit: 0,
            skip_backup: true,
        };
        let code = run(&args);
        assert_eq!(code, 0);

        let conn = Connection::open(&db_path).unwrap();
        let (it, ot) = token_pair(&conn, "x");
        assert_eq!(it, Some(0));
        assert_eq!(ot, Some(0));
    }

    #[test]
    fn skips_unknown_wire_api() {
        let dir = tempdir().unwrap();
        let (db_path, conn) = make_temp_db(dir.path());
        insert(
            &conn,
            "weird",
            "made-up-wire",
            200,
            r#"{}"#,
            r#"{"some":"body"}"#,
            Some(0),
            Some(0),
        );
        drop(conn);

        let args = BackfillTokensArgs {
            db_path: Some(db_path.clone()),
            dry_run: false,
            limit: 0,
            skip_backup: true,
        };
        let code = run(&args);
        assert_eq!(code, 0);

        let conn = Connection::open(&db_path).unwrap();
        let (it, ot) = token_pair(&conn, "weird");
        assert_eq!(it, Some(0));
        assert_eq!(ot, Some(0));
    }

    #[test]
    fn skips_non_2xx_status() {
        let dir = tempdir().unwrap();
        let (db_path, conn) = make_temp_db(dir.path());
        insert(
            &conn,
            "err",
            "openai-chat",
            500,
            r#"{"model":"m","messages":[{"role":"user","content":"hi"}]}"#,
            r#"{"error":{"message":"oops"}}"#,
            Some(0),
            Some(0),
        );
        drop(conn);

        let args = BackfillTokensArgs {
            db_path: Some(db_path.clone()),
            dry_run: false,
            limit: 0,
            skip_backup: true,
        };
        let code = run(&args);
        assert_eq!(code, 0);

        let conn = Connection::open(&db_path).unwrap();
        let (it, ot) = token_pair(&conn, "err");
        assert_eq!(it, Some(0));
        assert_eq!(ot, Some(0));
    }
}
