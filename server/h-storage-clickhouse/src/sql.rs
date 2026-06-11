//! Shared SQL fragment builders for ClickHouse reads. The dimension-filter
//! `'*'` wildcard logic lives in the backend-neutral `h_storage::dialect`;
//! these helpers cover ClickHouse-specific concerns (DateTime64 time ranges,
//! literal escaping).

/// Escape a string for embedding inside a single-quoted ClickHouse literal.
///
/// ClickHouse processes C-style backslash escapes inside `'...'` (it treats
/// `\'` as an escaped quote in addition to the SQL-standard `''`), so quoting
/// must escape backslashes as well as quotes — otherwise a value ending in `\`
/// consumes the closing quote and breaks out of the literal (SQL injection).
/// Delegates to the backend-neutral `escape_clickhouse` so the rule lives in
/// one place. Used for id / turn_id literals and `LIKE` substrings; `%` / `_`
/// are intentionally NOT escaped so `LIKE '%x%'` keeps substring semantics.
pub(crate) fn escape_str(s: &str) -> String {
    h_storage::dialect::escape_clickhouse(s)
}

/// ClickHouse IN-list builder. Mirrors `h_storage::dialect::sql_in_list` but
/// uses ClickHouse's backslash-aware escaping. ClickHouse call sites MUST use
/// this instead of the backend-neutral `sql_in_list`, whose quote-only
/// escaping is correct for DuckDB/Postgres but injectable on ClickHouse.
pub(crate) fn sql_in_list(values: &[String]) -> String {
    h_storage::dialect::sql_in_list_with(values, h_storage::dialect::escape_clickhouse)
}

/// Half-open time-range predicate on a `DateTime64(6)` column, comparing against
/// microsecond bounds via `fromUnixTimestamp64Micro` so the MergeTree
/// primary-key index on the timestamp column stays usable. `start_us`/`end_us`
/// are values we control, so interpolation is injection-safe.
pub(crate) fn time_where(col: &str, start_us: i64, end_us: i64) -> String {
    format!(
        "{col} >= fromUnixTimestamp64Micro({start_us}) \
         AND {col} < fromUnixTimestamp64Micro({end_us})"
    )
}
