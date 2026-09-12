# Aglake Storage Backend

The third `StorageBackend`, alongside DuckDB and ClickHouse. Aglake (released
as sglog before its 0.3) is a Splunk-compatible log platform: writes go over its
HTTP Event Collector, reads are SPL over `/api/v1/search`. Selected with
`storage.backend = "aglake"`; the REST API and the console are unchanged.

**Needs the API Aglake added in 0.3.** Its rename was a deliberate breaking
boundary rather than an alias layer — the pre-0.3 REST namespace answers
`410 Gone` — so Heron speaks only the current API. Upstream has since
renumbered that same line to **1.5** (a version-number unification, not a
second break: the 1.5 nightly is a descendant of the 0.3 one), so any current
build qualifies; both were verified against this backend's live suite. Heron's
own config keeps accepting the old `sglake` spelling, since a config file
survives a Heron upgrade and a daemon does not.

**Operational note on receiver ports.** From `0.3.0.2653` onward — 1.5
included — aglaked starts dedicated receivers on fixed `0.0.0.0` ports by
default (HEC 8088, OTLP 4318/4317, syslog 514, S2S 9997, ES-compat 9200) and
**refuses to start if one cannot bind**; 514 is privileged, and 9200 collides
with a real Elasticsearch. Heron uses none of them — it writes HEC over the
`--listen` port — so an aglaked dedicated to Heron should switch off the ones
it does not need. One trap: `--hec-http-enabled false` turns off the HEC
*input*, including the alias on `--listen` that Heron writes through. Move that
listener (`--hec-http`) rather than disabling it. `OwnedAglaked` in `it.rs`
does exactly this, probing `--help` so it still works against a build that
predates the flags.

**Why it exists.** Where the SQL backends give Heron a private database, this
one puts observation data into a log platform an organisation may already run —
the same search, alerting, and retention machinery, and the same knowledge
objects, rather than a second thing to operate. It also does something neither
SQL backend can: **stored bodies are full-text searchable**.

```
search index=heron_bodies "SELECT * FROM users"
```

Because aglake tokenizes every raw byte it stores, that works with no schema
and no configuration — finding a prompt by its contents is a property of the
store, not a feature that had to be built.

## What it is not

It is not a SQL database with a different driver. Four differences shape every
decision below:

* **No DDL, no schema.** An index exists because something wrote to it. Fields
  are extracted at search time. There is nothing to migrate — and nothing that
  will tell you a field went missing.
* **No `UPDATE`, no `DELETE`.** The store is append-only. Data leaves by whole
  buckets aging out, not by row.
* **No JOIN**, which the project already forbids on the read path anyway.
* **`*` is a wildcard, and there is no escape for it.** This one is a hazard
  rather than a limitation; see [Dimension tiers](#dimension-tiers).

## Index layout

Thirteen indexes under a configurable prefix (default `heron`), one sourcetype
each.

| Index | Contents | `_time` | Retention from |
|---|---|---|---|
| `<p>_spans` | LLM call metadata, no bodies | request time | `retention.spans` |
| `<p>_bodies` | one event per span: bodies + headers | same | `aglake.body_retention_days`, else spans |
| `<p>_traces` | agent turns | start time | `retention.traces` |
| `<p>_metrics_{10s,1m,5m,1h}` | pre-aggregated wide rows | window start | `retention.metrics[label]` |
| `<p>_finish_{10s,1m,5m,1h}` | finish-reason counts | window start | same |
| `<p>_http` | HTTP exchange metadata | request time | `retention.http_exchanges` |
| `<p>_http_bodies` | exchange bodies + headers | same | `aglake.body_retention_days`, else http |

Three decisions produced that shape:

**Bodies live apart from metadata.** A body is three orders of magnitude larger
than the columns beside it, and no list or aggregate query needs one. Splitting
them means those queries never decompress body bytes, and `include_bodies =
false` becomes "skip the second query" rather than "project a column away". It
also lets bodies expire on their own schedule. Measured: metadata-only is **2.7%**
of the full footprint, so a deployment that does not want bodies pays almost
nothing.

**Granularity is part of the index name.** aglake's retention is per-index and
Heron's metrics retention is per-granularity, so encoding the label in the name
makes `RetentionPolicy.metrics_before` a direct mapping instead of something to
emulate — and turns the most common metrics filter into index-level pruning.

**One sourcetype per index.** The columnar fast path requires every sourcetype
in a bucket to have indexed the field being read. Two sourcetypes with
different `indexed` sets in one index would knock every query in that bucket
back onto the row path. This is why finish-metric rows get their own index
rather than sharing with the wide metric rows.

## Storage cost

Measured on 124,328 real production spans (19.18 GB of raw JSON, ~216 KB per
span once assembled into HEC events — LLM bodies are themselves JSON, so
embedding them in an event's string field roughly doubles them through escaping):

| File | Size | vs. raw |
|---|---|---|
| `journal.sgj` (zstd) | 2.32 GB | **0.121×** |
| `index.tsidx` (inverted) | 0.18 GB | **0.009×** |
| `time.sgt` + `bloom.sgb` | 0.01 GB | 0.001× |
| `columns.sgv` | 0.02 GB | 0.001× |
| total | 2.53 GB | **0.132×** |

The inverted index is the surprise: a *forty-ninth* of the journal, where an
access-log corpus on the same engine produces an index **3.15× the journal**.
The reason is term repetition — LLM bodies repeat JSON keys, system-prompt
templates and ordinary English endlessly, so the dictionary stays small and the
postings compress hard. This is the opposite of the intuition that prose has
lower term repetition than structured logs, and the projection built on that
intuition was wrong by a factor of twelve.

Write throughput measured at **1,891 spans/s** with 16 concurrent clients, at
which point aglaked was using 8.65 of 384 cores — nowhere near saturation. (A
single-threaded client measures ~100 spans/s and is measuring itself.)

## Read path

### Dimension tiers

The metrics aggregator marks rollup rows with a **literal `'*'`** in the
dimension it rolled up. In SPL, `*` is a wildcard with no escape: a direct
translation of `server_ip = '*'` selects the rollup row **and** every detail row
it already contains, so every metric silently doubles.

So the tier is computed at write time into a `dim_tier` field — one of `wms`,
`wm`, `s`, `all`, `other` — and reads select exactly one tier by exact match.
`dims.rs` holds the selector; its equivalence test parses the **real output** of
the SQL backends' `build_dimension_where` and checks the two select the same
rows across nine filter shapes, plus the property that actually prevents the
doubling: every filter shape selects exactly one tier.

### Write-time precomputation

Aglake cannot push down `<`, `>`, `!=` or `NOT`. Anything a query would compare
is turned into a categorical value at write time instead:

| Field | Replaces |
|---|---|
| `err` (0/1), `err_class` (`ok`/`4xx`/`429`/`5xx`) | `status >= 400` and the four error buckets |
| `strm` (0/1) | a boolean that `sum()` cannot add |
| `dim_tier` | the wildcard tiers above |
| `proxy_hidden` (0/1) | `role NOT IN ('proxy_out','mirror_secondary')` — a negation aglake cannot push down, and which cannot distinguish "role absent" from "role is something else" |
| `tokens_estimated` | a read-time derivation the two SQL backends disagree about |
| `server_header`, `app_hint` | reading headers or bodies during a Services aggregation |
| `first_span_id` | pulling back a `span_ids_json` that runs to tens of KiB |

### Pagination

SPL has no `OFFSET`. Paging is:

```
… | sort <offset+limit> <keys> | streamstats count as _rn
  | where _rn>offset AND _rn<=end | table _raw
```

The obvious `| sort N | tail L` is wrong three ways, all of which a property
test caught at once: `tail` **reverses** the rows, paging past the end returns
the last page forever instead of nothing (so paging never terminates), and a
partial last page comes back as a full overlapping one.

Sort keys always end in the row's id. A total order is not optional under
offset paging — with a repeated sort key, tied rows can come back in a different
order between the query for page 1 and the query for page 2, so a row lands on
both and another on neither. (Both SQL backends were missing this and lost rows
on the corpus; they have it now too.)

`max_page_offset` (default 100,000) refuses a deep page rather than running for
minutes.

### Point lookups

Two steps, no JOIN: fetch `span_ids_json` from the trace, then
`id IN ("a","b",…)` in chunks of 512. Ids are UUIDv7, so a time window can be
derived from the id itself and used to prune buckets — but that derivation
**must be able to miss**: Codex's `turn_id` comes from the provider and is not
a UUIDv7 at all, and during pcap replay the id is minted when the pipeline sees
the record while `_time` comes from the packet, putting them years apart. So it
is "try the window, retry unbounded on empty". `query_trace_spans` does better:
it uses the trace's own `[start_us, end_us]`.

## Encoding

Rules that came out of measurement rather than design:

**aglake re-serializes object events with their keys sorted.** An event posted
as `{"id":…,"source_id":…}` comes back as `{"err":0,"err_class":…,"id":…}`,
nested objects included. Body events are therefore posted as **pre-serialized
strings**, which aglake stores byte-for-byte. Three benefits: `span_id` stays at
the front where an anchored regex can find it without scanning 320 KiB, aglake
skips a parse-and-reserialize of that payload on every write, and schema-on-read
field lookup still works on the string — so nothing is lost.

**`_time` cannot carry a precise timestamp.** It round-trips through `f64`
seconds; 3.3% of rows come back off by 1 µs, and the natural Rust spelling
(`as i64`, which truncates) is the failing one. Every event therefore also
carries an integer `ts_us`, and that is the authoritative value for ordering,
cursors, and anything read back into a struct. `_time` is for bucket pruning
only.

**Extracted fields are not a faithful projection.** The same event's
`total_input_tokens` is a number in `_raw` and a *string* in the extracted
field. Search output also drops null fields entirely and collapses a
single-element multivalue to a scalar, either of which would corrupt an
`Option<T>` or a `Vec<String>`. So reads always take `| table _raw` and
deserialize with serde; extracted fields are for filtering and aggregation only.

**Negative instants are clamped to the epoch.** aglake's time parser rejects
them outright (`bad time "-86400.000000"`), and the reads that widen their
window backwards produce one whenever the caller starts from `0`.

## props.toml and the columnar fast path

There is a path that answers an aggregate entirely from `columns.sgv` and the
postings, decoding only a 256-event sample to authenticate the result. It
requires every filtered and projected field to be `indexed` at write time.

`heron aglake-props` prints the stanzas. They are generated from the event
structs themselves — serde's derive hands the full field list to
`deserialize_struct`, so the list cannot drift from the schema — with `*_json`
blobs and free-text previews excluded by rule. A new scalar field is indexed by
default, which fails toward a slightly larger columnar file rather than toward a
silently missing fast path.

Measured against a no-props control on the same data:

| Query | with props | without |
|---|---|---|
| metrics timeseries (7,200 groups × 8 aggs) | 111 ms | 139 ms |
| services endpoints | **14 ms** | **61 ms** |
| spans list total | **4 ms** | **34 ms** |
| distinct dropdown | **7 ms** | **33 ms** |

Two things to know about that measurement. Without props, `columns.sgv` is never
created and **both** the column-source and column-fallback counters read zero —
so "zero fallbacks" alone proves nothing; the signal is `column_source > 0`. And
the fast path reads *warm* buckets, so a store whose data is still in hot
buckets measures as if it had no fast path at all.

Two limits an operator has to know: `indexed` is read once at daemon startup and
is **never applied retroactively**, so a query spanning a props change is fast on
one side and slow on the other with nothing in the logs to say why; and Heron
never writes this file — it belongs to whoever runs aglaked.

## Retention

Aglake has no `DELETE`. `apply_retention` translates each cutoff into a per-index
`frozen_after_secs` TTL and pushes it through the management API; the daemon
expires whole buckets on its own timer. Two consequences stated plainly:

* **The report is always zero.** There are no deleted rows to count. The sweep
  logs which indexes got which TTL and returns an empty report rather than a
  fabricated number.
* **Deletion is coarser than the cutoff.** A bucket survives until its *newest*
  event ages out, so rows can outlive the policy by up to one bucket's span.

Retention goes through the native `/api/v1/admin/indexes` face, which is always
mounted — unlike the Splunk-compatible one it replaced, which appeared only when
aglaked was started with vendored frontend assets, so an ingest-only deployment
could not be told about retention at all. What remains outside Heron's control
is version and authorization: a 404 means the daemon predates 0.3, a 401 that no
session is configured, a 403 that the account lacks the `admin` role. Each is
reported once, naming its own fix, and the sweep is a no-op until it clears.

## Durability

**At-least-once.** `WriteBuffer` discards a batch whose flush returns `Err` and
cannot retry it, so every retry lives in the HEC client:

* **200** — already fsynced. Never resend.
* **400 with `invalid-event-number: k`** — the prefix committed, event `k` is
  malformed, the rest was never seen. Skip past `k` and continue. This is
  deterministic progress, not a retry, so it does not spend the retry budget.
* **401 / 415 / other 400** — resending cannot help.
* **413** — halve and re-split.
* **5xx / timeout / connection error** — may or may not have landed; ask, then
  resend.

That "ask" is the ack, and it works differently than it looks. Aglake issues the
ack id **in the same response that reports success**, so there is no id to ask
about when the response is what got lost. Every request therefore goes out on a
**freshly minted channel**: the per-channel counter starts at zero, so the only
id that request could be given is `0`, and asking about id 0 asks about this
request. Every degradation path — restarted daemon, evicted channel,
unreachable server — answers "not committed" and resends, which is what would
have happened with acks off.

Acks cannot cover a 500 raised after some indexes in a batch already committed:
no id is issued, so the resend duplicates that prefix. Duplicates are visible
(two rows, one id) and harmless everywhere except metric sums, which is what
`metrics_dedup` is for — off by default, because `dedup` costs a sort and takes
the query off the columnar fast path, and that is not a price to pay
continuously against an event most deployments never see.

A single event larger than `max_event_bytes` (default 8 MiB) is dropped with a
loud log line rather than sent. Past aglake's 16 MiB WAL frame limit an event is
treated as corruption during crash replay and silently discarded, which is the
worst available failure mode; `[body_cap]` normally keeps events three orders of
magnitude below this, and this guard is what stands in when it is disabled.

## Known divergences

* **Proxy pairing does not annotate traces.** `update_trace_metadata` is a
  no-op: emulating an update on an append-only store means appending a revision
  and deduplicating on read, and `dedup` does not imply an order, so every
  traces read would have to pay a full-window `sort 0` first — which defeats
  the top-k pagination the whole read path is built on. The cost is bounded and
  visible: `proxy_role`/`proxy_peer_turn_id` stay unset, the proxy view is
  empty, and topology loses its `proxy` edges (`client` and `inferred` edges
  remain). The other 29 methods are unaffected. `init()` says so at startup.
* **App classification differs, and is better here.** The SQL backends sample a
  few bodies per endpoint at read time; aglake classifies every span at write
  time and takes the majority per endpoint. Same classifier, more input — so
  aglake may name an app where the others found nothing. It must never
  contradict them, which the differential harness checks.
* **`query_services_topology` is bounded** at 10,000 turns in the window,
  returning a truncated graph with a warning rather than issuing a thousand
  chunked queries.
* **Filter-dropdown distincts scan unbounded.** Their trait signature carries
  no time range, so they are the one read that cannot prune buckets. They stay
  cheap because `stats … by <indexed field>` reads postings rather than events,
  but their cost grows with retention.

## Deployment

Three daemon flags Heron cannot set:

* **`--max-hot-raw-mib 2048` or more.** The 64 MiB default seals a bucket every
  few hundred spans, and search cost scales with how many buckets a query must
  open. At 2048 MiB, 124k spans produced 12 warm buckets.
* **`--max-hot-span-hours 6`** (default 2160 h = 90 days). Set this, or the size
  knob above will starve the small indexes. See below — it is the one flag whose
  omission produces a slow console rather than a large one.
* **`--splunk-web-dir <assets>`**, only if you want Heron to manage retention.

### The size knob is global, and Heron's indexes are not the same size

`--max-hot-raw-mib` is per daemon, and it decides when a *hot* bucket seals.
An inverted index (`index.tsidx`), bloom filter and time index are written when
that seal happens — a hot bucket has none of them, only `journal.sgj` and its
WAL. To serve a search over a hot bucket, aglaked builds an index for it in
memory, and Heron appends every `flush_interval_ms`, so that structure is
invalidated about as fast as it is built.

`heron_bodies` carries ~200 KB events and seals every few hours on size alone,
so nearly all of it is warm and indexed. `heron_spans` carries scalars — a
production instance accumulated 135 MB of raw spans in three days, which reaches
2048 MiB in about seven weeks, and 90 days of span is further still. **The
metadata index is the one that never gets an on-disk index**, which is the
opposite of what the bucket-count analysis above would lead you to expect.

Measured against that instance, both indexes hot and unsealed, under live
ingest:

| index | hot `journal.sgj` | point lookup | `stats count` |
|---|---|---|---|
| `heron_traces` | 3.4 MB | 65 ms, no outliers | 82 ms |
| `heron_spans` | 22 MB | 12 ms, **spiking to 2.2 s** | 4.2 s |

Same code path and the same absent tsidx in both rows; the only variable is how
much journal the in-memory build has to cover. At 3.4 MB the rebuild is invisible
and at 22 MB it is the dominant cost of opening an agent turn — polling once a
second, ~40% of lookups paid it. It is invariant in the ways that mislead: the
same for a 1-call turn as an 85-call one, and the same whether the search window
is the turn's own span, ±24 h, or unbounded.

Keeping the hot bucket small is the fix, and only `--max-hot-span-hours` does
that for an index whose events are small. At 6 h, `heron_spans` seals four
buckets a day — 120 over a 30-day retention, against the hundreds of thousands
the bucket-count guidance is about.

And one property to check: `/api/v1/*` is open until aglaked has a user
catalog. With no users — its default — where aglaked listens is the entire
access-control story for every request and response body Heron stores there, so
`config validate` warns when `storage.aglake.url` is not loopback. Configuring
`username`/`password` (exchanged for a session, presented as a bearer token,
re-established once on a 401) puts a real check in front of the data and lifts
that warning. `hec_token` does **not**: it authenticates ingest only.

## Testing

| Layer | What it covers |
|---|---|
| unit | encoding round-trips, SPL quoting and injection, dimension-tier equivalence against the SQL builder's real output, props generation |
| `retry_tests` (mock HTTP) | all six retry branches over a real socket — a live server cannot be asked for a 413, or to accept a request and then never answer. Runs in CI with no server. |
| `auth_tests` (mock HTTP) | session handling and the admin API: expiry-then-retry, a fixed token never refreshed, credentials against a no-auth daemon, one shared login, and the request/response shapes. A session expiring is reached in production by running twelve hours and by nothing a test can ask a daemon for. |
| `it.rs` (live) | 25 tests against a real aglaked, gated on `AGLAKE_TEST_URL`; self-skip without it. Includes a fault-injection test that SIGKILLs the daemon mid-write. |
| `scripts/storage/backend-differential.py` | replays the pcap corpus through DuckDB and aglake and diffs every REST endpoint |

The differential is the one that finds things the others cannot, because it is
the only one where the input comes from the pipeline rather than from a fixture
the author wrote. Its first run found a 500 on the sessions page, two aggregates
reading the wrong column, a topology graph missing a node its own edges pointed
at, a millisecond truncation in DuckDB, and paginated lists in both SQL backends
that returned 26 rows as 18 distinct ones.
