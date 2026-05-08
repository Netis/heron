//! Session-scoped queries. Sessions are a view over `agent_turns`
//! grouped by `(source_id, session_id)` — no schema of their own.

use ts_common::error::{AppError, Result};
use ts_storage::query::*;

use crate::util::{
    extract_full_text_batch, parse_json_string_list, us_to_timestamp, ExtractKind,
};
use crate::DuckDbBackend;

impl DuckDbBackend {
    pub(crate) async fn query_sessions(&self, query: &SessionListQuery) -> Result<SessionsPage> {
        let conn = self.read_pool.acquire().await?;
        let query = query.clone();

        tokio::task::spawn_blocking(move || {
            let start_ts = us_to_timestamp(query.time_range.start_us);
            let end_ts = us_to_timestamp(query.time_range.end_us);
            let page_size = query.page_size.max(1);

            // Step 1 WHERE: time window + optional source/agent_kind. Both
            // optional fields are session-stable (same session -> same value),
            // so pushing them into WHERE does not truncate the lifetime
            // aggregates computed in Step 2.
            let mut where_parts: Vec<String> = vec![
                "end_time >= ?".to_string(),
                "end_time < ?".to_string(),
            ];
            if let Some(sid) = &query.source_id {
                where_parts.push(format!("source_id = '{}'", sid.replace('\'', "''")));
            }
            if let Some(ak) = &query.agent_kind {
                where_parts.push(format!("agent_kind = '{}'", ak.replace('\'', "''")));
            }
            let where_sql = where_parts.join(" AND ");

            // Cursor HAVING clause. Tuple comparison lets us sort by
            // (MAX(end_time), source_id, session_id) DESC uniformly.
            let (having_sql, cursor_ts) = if let Some(c) = &query.cursor {
                let ts = us_to_timestamp(c.last_turn_at_ms.saturating_mul(1000));
                let sid = c.source_id.replace('\'', "''");
                let sess = c.session_id.replace('\'', "''");
                (
                    format!(
                        " HAVING (MAX(end_time), source_id, session_id) < (CAST(? AS TIMESTAMP), '{sid}', '{sess}')"
                    ),
                    Some(ts),
                )
            } else {
                (String::new(), None)
            };

            // Fetch one extra row to detect the next page without a count query.
            let limit = (page_size as u64) + 1;

            let step1_sql = format!(
                "SELECT source_id, session_id, epoch_ms(MAX(end_time)) AS last_ms \
                 FROM agent_turns \
                 WHERE {where_sql} \
                 GROUP BY source_id, session_id{having_sql} \
                 ORDER BY MAX(end_time) DESC, source_id DESC, session_id DESC \
                 LIMIT {limit}"
            );

            let mut stmt = conn.prepare(&step1_sql).map_err(|e| {
                AppError::Storage(format!("failed to prepare sessions step1: {e}"))
            })?;

            let mut key_rows: Vec<(String, String, i64)> = Vec::new();
            {
                let mut rows = match &cursor_ts {
                    Some(cts) => stmt.query(duckdb::params![start_ts, end_ts, cts]),
                    None => stmt.query(duckdb::params![start_ts, end_ts]),
                }
                .map_err(|e| {
                    AppError::Storage(format!("failed to execute sessions step1: {e}"))
                })?;

                while let Some(row) = rows
                    .next()
                    .map_err(|e| AppError::Storage(format!("row error: {e}")))?
                {
                    let src: String = row
                        .get(0)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                    let sess: String = row
                        .get(1)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                    let ms: i64 = row
                        .get(2)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                    key_rows.push((src, sess, ms));
                }
            }

            let has_more = key_rows.len() > page_size as usize;
            if has_more {
                key_rows.truncate(page_size as usize);
            }
            if key_rows.is_empty() {
                return Ok(SessionsPage {
                    items: vec![],
                    next_cursor: None,
                });
            }

            // Step 2: full-lifetime aggregate + first-turn preview via
            // ROW_NUMBER(). Pair list is inlined because DuckDB's `IN ((?, ?))`
            // with positional params gets awkward and the ids are trusted
            // internal strings already vetted by Step 1.
            let pairs_sql = key_rows
                .iter()
                .map(|(s, k, _)| {
                    format!(
                        "('{}', '{}')",
                        s.replace('\'', "''"),
                        k.replace('\'', "''")
                    )
                })
                .collect::<Vec<_>>()
                .join(", ");

            let step2_sql = format!(
                "SELECT source_id, session_id, \
                        epoch_ms(MIN(start_time)) AS first_ms, \
                        epoch_ms(MAX(end_time))   AS last_ms, \
                        COUNT(*) AS turn_count, \
                        SUM(call_count) AS call_count, \
                        SUM(total_input_tokens) AS total_in, \
                        SUM(total_output_tokens) AS total_out, \
                        SUM(total_cache_read_input_tokens) AS total_cr, \
                        SUM(total_cache_creation_input_tokens) AS total_cc, \
                        SUM(total_cost_usd) AS total_cost, \
                        MIN(agent_kind) AS agent_kind, \
                        MIN(CASE WHEN rn = 1 THEN user_input_preview END) AS first_input, \
                        MIN(CASE WHEN rn = 1 THEN user_call_id      END) AS first_call_id \
                 FROM ( \
                    SELECT source_id, session_id, start_time, end_time, call_count, \
                           total_input_tokens, total_output_tokens, \
                           total_cache_read_input_tokens, total_cache_creation_input_tokens, \
                           total_cost_usd, agent_kind, user_input_preview, user_call_id, \
                           ROW_NUMBER() OVER (PARTITION BY source_id, session_id ORDER BY start_time) AS rn \
                    FROM agent_turns \
                    WHERE (source_id, session_id) IN ({pairs_sql}) \
                 ) t \
                 GROUP BY source_id, session_id"
            );

            let mut stmt2 = conn.prepare(&step2_sql).map_err(|e| {
                AppError::Storage(format!("failed to prepare sessions step2: {e}"))
            })?;

            use std::collections::HashMap;
            let mut agg: HashMap<(String, String), SessionListItem> = HashMap::new();
            {
                let mut rows = stmt2.query([]).map_err(|e| {
                    AppError::Storage(format!("failed to execute sessions step2: {e}"))
                })?;
                while let Some(row) = rows
                    .next()
                    .map_err(|e| AppError::Storage(format!("row error: {e}")))?
                {
                    let src: String = row
                        .get(0)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                    let sess: String = row
                        .get(1)
                        .map_err(|e| AppError::Storage(format!("read error: {e}")))?;
                    let item = SessionListItem {
                        source_id: src.clone(),
                        session_id: sess.clone(),
                        last_turn_at_in_window: 0,
                        first_turn_at: row
                            .get(2)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        last_turn_at: row
                            .get(3)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        turn_count: row
                            .get(4)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        call_count: row
                            .get(5)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        total_input_tokens: row
                            .get(6)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        total_output_tokens: row
                            .get(7)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        total_cache_read_input_tokens: row
                            .get(8)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        total_cache_creation_input_tokens: row
                            .get(9)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        total_cost_usd: row
                            .get::<_, Option<f64>>(10)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        agent_kind: row
                            .get(11)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        first_user_input_preview: row
                            .get::<_, Option<String>>(12)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        first_user_call_id: row
                            .get::<_, Option<String>>(13)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    };
                    agg.insert((src, sess), item);
                }
            }

            // Preserve Step 1's ordering and inject last_turn_at_in_window.
            let mut items: Vec<SessionListItem> = Vec::with_capacity(key_rows.len());
            for (src, sess, in_window_ms) in &key_rows {
                if let Some(mut it) = agg.remove(&(src.clone(), sess.clone())) {
                    it.last_turn_at_in_window = *in_window_ms;
                    items.push(it);
                }
            }

            let next_cursor = if has_more {
                items.last().map(|it| {
                    encode_session_cursor(&SessionListCursor {
                        last_turn_at_ms: it.last_turn_at_in_window,
                        source_id: it.source_id.clone(),
                        session_id: it.session_id.clone(),
                    })
                })
            } else {
                None
            };

            Ok(SessionsPage { items, next_cursor })
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    pub(crate) async fn query_session_by_id(
        &self,
        source_id: &str,
        session_id: &str,
    ) -> Result<Option<SessionDetail>> {
        let conn = self.read_pool.acquire().await?;
        let source_id = source_id.to_string();
        let session_id = session_id.to_string();

        tokio::task::spawn_blocking(move || {
            let sql = "SELECT source_id, session_id, \
                              epoch_ms(MIN(start_time)) AS first_ms, \
                              epoch_ms(MAX(end_time))   AS last_ms, \
                              COUNT(*) AS turn_count, \
                              SUM(call_count) AS call_count, \
                              SUM(total_input_tokens) AS total_in, \
                              SUM(total_output_tokens) AS total_out, \
                              SUM(total_cache_read_input_tokens) AS total_cr, \
                              SUM(total_cache_creation_input_tokens) AS total_cc, \
                              SUM(total_cost_usd) AS total_cost, \
                              MIN(agent_kind) AS agent_kind, \
                              MIN(CASE WHEN rn = 1 THEN user_input_preview END) AS first_input, \
                              MIN(CASE WHEN rn = 1 THEN user_call_id      END) AS first_call_id \
                       FROM ( \
                          SELECT source_id, session_id, start_time, end_time, call_count, \
                                 total_input_tokens, total_output_tokens, \
                                 total_cache_read_input_tokens, total_cache_creation_input_tokens, \
                                 total_cost_usd, agent_kind, user_input_preview, user_call_id, \
                                 ROW_NUMBER() OVER (PARTITION BY source_id, session_id ORDER BY start_time) AS rn \
                          FROM agent_turns \
                          WHERE source_id = ? AND session_id = ? \
                       ) t \
                       GROUP BY source_id, session_id";

            let mut stmt = conn.prepare(sql).map_err(|e| {
                AppError::Storage(format!("failed to prepare session_by_id: {e}"))
            })?;
            let mut rows = stmt
                .query(duckdb::params![source_id, session_id])
                .map_err(|e| {
                    AppError::Storage(format!("failed to execute session_by_id: {e}"))
                })?;

            let Some(row) = rows
                .next()
                .map_err(|e| AppError::Storage(format!("row error: {e}")))?
            else {
                return Ok(None);
            };
            // GROUP BY always emits a row when the subquery has at least one
            // match; when the session has zero turns the subquery is empty and
            // the outer aggregate emits nothing -> handled above.
            Ok(Some(SessionDetail {
                source_id: row
                    .get(0)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                session_id: row
                    .get(1)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                first_turn_at: row
                    .get(2)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                last_turn_at: row
                    .get(3)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                turn_count: row
                    .get(4)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                call_count: row
                    .get(5)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                total_input_tokens: row
                    .get(6)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                total_output_tokens: row
                    .get(7)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                total_cache_read_input_tokens: row
                    .get(8)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                total_cache_creation_input_tokens: row
                    .get(9)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                total_cost_usd: row
                    .get::<_, Option<f64>>(10)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                agent_kind: row
                    .get(11)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                first_user_input_preview: row
                    .get::<_, Option<String>>(12)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                first_user_call_id: row
                    .get::<_, Option<String>>(13)
                    .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
            }))
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }

    pub(crate) async fn query_session_turns(&self, query: &SessionTurnsQuery) -> Result<SessionTurnsPage> {
        let conn = self.read_pool.acquire().await?;
        let query = query.clone();

        tokio::task::spawn_blocking(move || {
            let page_size = query.page_size.max(1);
            let limit = (page_size as u64) + 1;

            // Cursor filter (tuple comparison). ORDER BY start_time DESC, turn_id DESC.
            let (cursor_sql, cursor_values) = if let Some(c) = &query.cursor {
                let ts = us_to_timestamp(c.start_time_us);
                (
                    " AND (start_time, turn_id) < (CAST(? AS TIMESTAMP), ?)".to_string(),
                    Some((ts, c.turn_id.clone())),
                )
            } else {
                (String::new(), None)
            };

            // Paging query. SELECT returns SessionTurnItem columns + preview +
            // call_id for each side so we know whether to run full-text
            // extraction below.
            let sql = format!(
                "SELECT turn_id, source_id, session_id, \
                        epoch_ms(start_time)   AS start_ms, \
                        epoch_ms(end_time)     AS end_ms, \
                        duration_ms, wire_api, agent_kind, \
                        models_used, call_count, \
                        total_input_tokens, total_output_tokens, \
                        status, final_finish_reason, \
                        user_input_preview, user_call_id, \
                        final_answer_preview, final_call_id \
                 FROM agent_turns \
                 WHERE source_id = ? AND session_id = ?{cursor_sql} \
                 ORDER BY start_time DESC, turn_id DESC \
                 LIMIT {limit}"
            );

            let mut stmt = conn
                .prepare(&sql)
                .map_err(|e| AppError::Storage(format!("failed to prepare session_turns: {e}")))?;

            #[allow(clippy::type_complexity)]
            let mut fetched: Vec<(
                String,
                String,
                String,
                i64,
                i64,
                u64,
                String,
                String,
                Option<String>,
                u32,
                u64,
                u64,
                String,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
                Option<String>,
            )> = Vec::new();

            {
                let mut rows = match &cursor_values {
                    Some((ts, sid)) => {
                        stmt.query(duckdb::params![query.source_id, query.session_id, ts, sid])
                    }
                    None => stmt.query(duckdb::params![query.source_id, query.session_id]),
                }
                .map_err(|e| AppError::Storage(format!("failed to execute session_turns: {e}")))?;

                while let Some(row) = rows
                    .next()
                    .map_err(|e| AppError::Storage(format!("row error: {e}")))?
                {
                    let tuple = (
                        row.get(0)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get(1)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get(2)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get(3)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get(4)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get(5)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get(6)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get(7)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get::<_, Option<String>>(8)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get(9)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get(10)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get(11)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get(12)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get::<_, Option<String>>(13)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get::<_, Option<String>>(14)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get::<_, Option<String>>(15)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get::<_, Option<String>>(16)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                        row.get::<_, Option<String>>(17)
                            .map_err(|e| AppError::Storage(format!("read error: {e}")))?,
                    );
                    fetched.push(tuple);
                }
            }

            // Fetch+1 pattern: if we got page_size + 1 rows, there's a next page.
            let has_more = fetched.len() as u64 > page_size as u64;
            if has_more {
                fetched.truncate(page_size as usize);
            }

            // Gather call-ids that need full-text extraction (preview ended with `…`).
            let mut need_user: Vec<(String, String)> = Vec::new(); // (agent_kind, call_id)
            let mut need_assistant: Vec<(String, String)> = Vec::new();
            for t in &fetched {
                let agent_kind = t.7.clone();
                let user_preview = &t.14;
                let user_call_id = &t.15;
                let final_preview = &t.16;
                let final_call_id = &t.17;
                if let (Some(p), Some(cid)) = (user_preview, user_call_id) {
                    if p.ends_with('…') {
                        need_user.push((agent_kind.clone(), cid.clone()));
                    }
                }
                if let (Some(p), Some(cid)) = (final_preview, final_call_id) {
                    if p.ends_with('…') {
                        need_assistant.push((agent_kind, cid.clone()));
                    }
                }
            }
            let user_map = extract_full_text_batch(&conn, ExtractKind::User, &need_user);
            let asst_map = extract_full_text_batch(&conn, ExtractKind::Assistant, &need_assistant);

            let mut items: Vec<SessionTurnItem> = Vec::with_capacity(fetched.len());
            for t in fetched {
                let (
                    turn_id,
                    source_id,
                    session_id,
                    start_ms,
                    end_ms,
                    duration_ms,
                    wire_api,
                    agent_kind,
                    models_used_raw,
                    call_count,
                    total_input_tokens,
                    total_output_tokens,
                    status,
                    final_finish_reason,
                    user_preview,
                    user_call_id,
                    final_preview,
                    final_call_id,
                ) = t;

                let user_input = match (user_preview.as_deref(), user_call_id.as_deref()) {
                    (Some(p), _) if !p.ends_with('…') => Some(p.to_string()),
                    (_, Some(cid)) => user_map.get(cid).cloned().or_else(|| user_preview.clone()),
                    _ => user_preview.clone(),
                };
                let final_answer = match (final_preview.as_deref(), final_call_id.as_deref()) {
                    (Some(p), _) if !p.ends_with('…') => Some(p.to_string()),
                    (_, Some(cid)) => asst_map.get(cid).cloned().or_else(|| final_preview.clone()),
                    _ => final_preview.clone(),
                };

                let models_used = parse_json_string_list(models_used_raw.as_deref());
                let primary_model = models_used.first().cloned();

                items.push(SessionTurnItem {
                    turn_id,
                    source_id,
                    session_id,
                    start_time: start_ms,
                    end_time: end_ms,
                    duration_ms,
                    wire_api,
                    agent_kind,
                    primary_model,
                    models_used,
                    call_count,
                    total_input_tokens,
                    total_output_tokens,
                    status,
                    final_finish_reason,
                    user_input,
                    final_answer,
                });
            }

            let next_cursor = if has_more {
                items.last().map(|last| {
                    encode_session_turns_cursor(&SessionTurnsCursor {
                        start_time_us: last.start_time.saturating_mul(1000),
                        turn_id: last.turn_id.clone(),
                    })
                })
            } else {
                None
            };

            Ok(SessionTurnsPage { items, next_cursor })
        })
        .await
        .map_err(|e| AppError::Storage(format!("spawn_blocking failed: {e}")))?
    }
}
