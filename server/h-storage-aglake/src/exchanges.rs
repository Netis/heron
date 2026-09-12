//! `http_exchanges` — write path and by-id lookup.
//!
//! Same metadata/body split as spans, and the same ordering guarantee: the
//! metadata event precedes its body event in the batch, so a partial ingest
//! can lose a body but never orphan one.
//!
//! # Timestamp units: matching DuckDB, not ClickHouse
//!
//! The two SQL backends disagree about this entity. `HttpExchangeDetail`'s doc
//! comment says microseconds; DuckDB emits **milliseconds** (`epoch_ms`) and
//! ClickHouse emits **microseconds** (`toUnixTimestamp64Micro`), so the same
//! API endpoint returns values 1000× apart depending on which backend is
//! configured. For the list type they agree on milliseconds, despite its doc
//! comment also saying microseconds.
//!
//! This backend follows DuckDB — milliseconds everywhere — because DuckDB is
//! the default and what the console renders against; emitting microseconds
//! would date every request to the year 58000 in the UI. That makes aglake
//! self-consistent and consistent with the default backend, and leaves
//! ClickHouse's detail read as the outlier to fix separately.

use h_common::error::Result;
use h_protocol::HttpExchange;
use h_storage::query::{
    HttpExchangeDetail, HttpExchangeListItem, HttpExchangesPage, HttpExchangesQuery,
};

use crate::calls::ms;
use crate::read::Sort;
use crate::rows::{http_events, Envelope, HttpBodyEvent, HttpEvent, ST_HTTP, ST_HTTP_BODY};
use crate::spl::{self, match_term, Search};
use crate::AglakeBackend;

/// `duration_ms` is not stored — it is `done_us - ts_us`, computed by an
/// `| eval` stage before the sort so it can be sorted on.
const EXCHANGE_SORT: &[(&str, &str)] = &[
    ("request_time", "num(ts_us)"),
    ("status", "num(status)"),
    ("duration_ms", "num(dur_ms)"),
];

impl AglakeBackend {
    pub(crate) async fn write_exchanges(&self, exchanges: Vec<HttpExchange>) -> Result<()> {
        if exchanges.is_empty() {
            return Ok(());
        }
        let mut events = Vec::with_capacity(exchanges.len() * 2);
        for x in &exchanges {
            let (meta, body) = http_events(x, self.store_bodies);
            let ts = x.request.timestamp_us;
            let host = x.request.flow_key.source_id.clone();

            match Envelope::new(ts, &host, ST_HTTP, &self.ix.http, meta).encode() {
                Ok(s) => events.push(s),
                Err(e) => {
                    tracing::error!(
                        target: "aglake::write", id = %x.id, error = %e,
                        "aglake: failed to encode http exchange; skipping it"
                    );
                    continue;
                }
            }
            if let Some(body) = body {
                match crate::rows::raw_envelope(
                    ts,
                    &host,
                    ST_HTTP_BODY,
                    &self.ix.http_bodies,
                    &body,
                ) {
                    Ok(s) => events.push(s),
                    Err(e) => tracing::error!(
                        target: "aglake::write", id = %x.id, error = %e,
                        "aglake: failed to encode http body; metadata still written"
                    ),
                }
            }
        }
        self.hec.send(events).await
    }

    pub(crate) async fn query_http_exchanges(
        &self,
        query: &HttpExchangesQuery,
    ) -> Result<HttpExchangesPage> {
        let sort = Sort::new(
            &query.sort_by,
            &query.sort_order,
            EXCHANGE_SORT,
            &["num(ts_us)", "str(id)"],
        )?;

        let mut s = Search::new(&self.ix.http, ST_HTTP);
        s.any_of("server_ip", &query.server_ips);
        s.any_of("client_ip", &query.client_ips);
        s.any_of("method", &query.methods);
        s.any_of_nums("status", &query.status_codes);
        if let Some(sub) = query
            .uri_contains
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
        {
            s.contains("uri", sub);
        }
        if let Some(sse) = query.is_sse {
            s.eq_num("sse", u8::from(sse));
        }
        s.eval("dur_ms", "(done_us - ts_us) / 1000");

        let (events, total) = self
            .fetch_page::<HttpEvent>(
                "query_http_exchanges",
                &s,
                &sort,
                query.page,
                query.page_size,
                &query.time_range,
            )
            .await?;

        Ok(HttpExchangesPage {
            total,
            items: events.into_iter().map(exchange_list_item).collect(),
        })
    }

    pub(crate) async fn query_http_exchange_by_id(
        &self,
        id: &str,
    ) -> Result<Option<HttpExchangeDetail>> {
        let ix = &self.ix.http;
        let Some(term) = match_term("id", id) else {
            return Ok(None);
        };
        let search = format!("search index={ix} sourcetype={ST_HTTP} {term}");
        let Some(e) = self
            .fetch_raw_by_id::<HttpEvent>("query_http_exchange_by_id", &search, 1, id)
            .await?
            .into_iter()
            .next()
        else {
            return Ok(None);
        };

        let body = if e.has_body {
            let bix = &self.ix.http_bodies;
            // Raw term, not a field predicate: the body indexes carry no
            // extracted fields at all, so `span_id="…"` matches nothing here.
            // See `spl::body_term`.
            let Some(bterm) = spl::body_term(id) else {
                return Ok(None);
            };
            let bsearch = format!("search index={bix} sourcetype={ST_HTTP_BODY} {bterm}");
            self.fetch_raw_by_id::<HttpBodyEvent>("query_http_exchange_body", &bsearch, 1, id)
                .await?
                .into_iter()
                .find(|b| b.span_id == id)
        } else {
            None
        };
        // Headers are non-optional on the detail type. An exchange written
        // with bodies disabled has none stored, and an empty JSON array is the
        // honest answer — the same shape the parser produces for a request
        // that genuinely carried no headers.
        let body = body.unwrap_or(HttpBodyEvent {
            span_id: e.id.clone(),
            request_headers: "[]".into(),
            response_headers: "[]".into(),
            request_body: None,
            response_body: None,
        });

        Ok(Some(HttpExchangeDetail {
            id: e.id,
            source_id: e.source_id,
            client_ip: e.client_ip,
            client_port: e.client_port,
            server_ip: e.server_ip,
            server_port: e.server_port,
            method: e.method,
            uri: e.uri,
            request_headers: body.request_headers,
            request_body: body.request_body,
            status: e.status,
            response_headers: body.response_headers,
            response_body: body.response_body,
            is_sse: e.is_sse,
            sse_event_count: e.sse_event_count,
            sse_data_bytes: e.sse_data_bytes,
            request_time: ms(e.ts_us),
            response_first_byte_time: e.first_byte_us.map(ms),
            response_complete_time: e.done_us.map(ms),
        }))
    }
}

fn exchange_list_item(e: HttpEvent) -> HttpExchangeListItem {
    HttpExchangeListItem {
        id: e.id,
        source_id: e.source_id,
        request_time: ms(e.ts_us),
        method: e.method,
        uri: e.uri,
        client_ip: e.client_ip,
        server_ip: e.server_ip,
        server_port: e.server_port,
        status: e.status,
        is_sse: e.is_sse,
        // `None` for an exchange with no completion, matching the SQL
        // backends' NULL for the same case.
        duration_ms: e.done_us.map(|d| (d - e.ts_us) as f64 / 1000.0),
    }
}
