//! Shared SQL fragment builders for ClickHouse reads. The dimension-filter
//! `'*'` wildcard logic lives in the backend-neutral `h_storage::dialect`;
//! these helpers cover ClickHouse-specific concerns (DateTime64 time ranges,
//! literal escaping).

/// Escape a string for embedding inside a single-quoted ClickHouse literal by
/// doubling single quotes (`''`), matching the DuckDB backend's `sql_in_list`
/// convention (ClickHouse accepts both `''` and `\'`). Used for id / turn_id
/// literals and `LIKE` substrings; `%` / `_` are intentionally NOT escaped so
/// `LIKE '%x%'` keeps substring semantics, exactly as the DuckDB backend.
pub(crate) fn escape_str(s: &str) -> String {
    s.replace('\'', "''")
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
