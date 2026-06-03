//! Serving-software classification moved to the backend-neutral
//! `h_storage::classify` module so DuckDB and ClickHouse share one copy.
//! Re-exported here to keep existing `crate::apps::*` call sites stable.

pub(crate) use h_storage::classify::{classify_app, extract_server_header};
