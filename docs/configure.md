# Configuring Heron

This is a reference for every section in the configuration file. The
shipped `config/default.toml` is heavily commented and is the best place
to copy a starting point from.

## Where Heron looks for config

When you start `heron` without `-c <path>`, it searches these
locations **in order** and uses the first file that exists:

1. `./config/default.toml` — current working directory (dev mode and
   the layout inside an extracted release tarball).
2. `$XDG_CONFIG_HOME/heron/config.toml` — XDG-aware user override.
3. `~/.config/heron/config.toml` — XDG default user override.
4. `/etc/heron/config.toml` — system-wide install (dropped by
   `install.sh` when invoked with `sudo`).

The startup log line `heron vX.Y.Z starting, config=<path>` tells
you which one was picked. macOS uses `~/.config/` too — Heron does
not split between Linux and macOS here.

## Override precedence

```bash
# Auto-discovered (the cascade above)
heron -i eth0

# Explicit path — bypasses discovery
heron -c /opt/heron/custom.toml -i eth0
```

CLI flags **override the config file** entirely for the source pipeline:

| CLI flag | Effect |
|---|---|
| `-i <iface>` | Replaces all `[[pipeline]]` blocks with a single live-capture pipeline named `cli` |
| `--pcap-file <path>` | Same, but reading from a file instead of an interface. Typically combined with `--no-retention` (see below) |
| `--bpf-filter "<expr>"` | Adds a BPF filter to the CLI pipeline (requires `-i`) |
| `--snaplen <n>` | Snapshot length for the CLI pipeline (default `262144`) |
| `--exit-after-drain` | Exit when capture sources finish and the pipeline drains. Default keeps the API/console available so you can browse results after a `--pcap-file` replay; press Ctrl+C to exit. Use this flag for batch/CI runs. |
| `--no-retention` | Disables the retention sweeper for this run (overrides `[storage.retention] enabled`). Use with `--pcap-file` when the pcap's event timestamps are older than the retention window — without it, freshly imported data is pruned by the next sweep because cutoffs are anchored at wall-clock `now`. |

Storage, API, and retention settings are always read from the config
file — CLI does not override these.

## `[[pipeline]]` — capture and processing pipelines

Each `[[pipeline]]` is an independent processing pipeline with its own
worker pool. Multiple pipelines provide resource isolation: for example,
keep low-priority pcap-file replay separate from high-priority live
capture.

```toml
[[pipeline]]
name = "local"               # required, must be unique
dispatcher_count = 1         # parsed-packet → flow workers
flow_shard_count = 4         # parallel flow workers (sharded by flow key)

[pipeline.turn]
idle_timeout_secs = 600      # close a turn after this much idleness
sweep_interval_secs = 10     # how often the sweeper scans for idle turns
grace_ms = 1000              # buffer-finalize fan-in jitter window
shard_count = 1              # parallel turn-tracker workers

[pipeline.metrics]
shard_count = 1              # parallel metrics aggregators
```

### Source types

A pipeline must have at least one `[[pipeline.sources]]` block. Four
source types are supported — three packet taps (`pcap`, `pcap-file`,
`cloud-probe`) and one in-process TLS reader (`ebpf`, Linux-only and
experimental):

#### `pcap` — live interface capture

```toml
[[pipeline.sources]]
type = "pcap"
interface = "eth0"
bpf_filter = "tcp port 8000"   # see "BPF filters" below
snaplen = 262144               # max bytes per packet (256 KiB default)
```

#### `pcap-file` — replay from a file

```toml
[[pipeline.sources]]
type = "pcap-file"
path = "/data/captures/llm-traffic.pcap"
realtime = false               # false = read as fast as possible (default)
                               # true  = honor original packet timestamps
```

#### `cloud-probe` — receive from a remote [cloud-probe](https://github.com/Netis/cloud-probe)

```toml
[[pipeline.sources]]
type = "cloud-probe"
endpoint = "tcp://0.0.0.0:5555"
recv_hwm = 1000                # ZMQ receive high-water mark
```

Use this when the LLM provider workload runs on hosts you cannot install
Heron on directly. Cloud-probe runs there, captures locally, and
forwards packets over ZMQ.

#### `ebpf` — on-host TLS capture (Linux, experimental)

Reads plaintext at the in-process TLS boundary by attaching uprobes to the
target's `SSL_read` / `SSL_write`, instead of tapping ciphertext off the wire.
This lets Heron observe **TLS-encrypted** calls on the host that makes them — no
proxy, no TLS terminator — and stamp each call with its **owning process**
(pid · comm · executable). See [eBPF capture](design/02-capture.md#ebpf-ssl-uprobe-capture-linux-experimental)
for the design.

```toml
[[pipeline.sources]]
type = "ebpf"
# source_id = "ebpf"           # optional; defaults to "ebpf"
ssl_libs = []                  # empty = auto-discover libssl.so via ldconfig.
                               # Or pin explicit paths, e.g.
                               #   ["/usr/lib/x86_64-linux-gnu/libssl.so.3"]
pid_allowlist = []             # empty = all processes; or restrict, e.g. [4123, 4124]
segment_size = 16384           # max plaintext bytes per synthetic TCP segment

# Static, symbol-stripped TLS binaries (no dynamic libssl) — e.g. Claude
# Code's Bun runtime. The built-in "bun" flavor ships signatures, so no
# manual offset derivation is needed for stock Bun / Claude Code:
[[pipeline.sources.targets]]
binary = "/home/user/.bun/bin/bun"
flavor = "bun"                 # built-ins: "bun" | "boringssl-bun" | "claude-code"
# For a generic stripped BoringSSL build, supply your own (see the eBPF
# static-targets doc for how to derive these):
#   write_sig = "55 48 89 e5 ..."   # or write_offset = 0x4a64a40
#   read_sig  = "55 48 89 e5 ..."   # or read_offset  = 0x4a63db0
```

**Requirements.** Linux only, and the binary must be **built from source** with
the non-default `ebpf` cargo feature on `h-capture` (prebuilt release binaries
omit it). The host needs `CAP_BPF` + `CAP_PERFMON` (kernel ≥ 5.8) or root, plus
kernel BTF at `/sys/kernel/btf/vmlinux`. Run `heron doctor` to check the
`capture.ebpf` prerequisites. Like every source it reconstructs **HTTP/1.x
only** (an h2-over-ALPN client is not reconstructed).

### Optional: persist captured packets

Useful for offline replay, debugging a specific incident, or shipping a
trace to support:

```toml
[pipeline.pcap_dump]
enabled = false                # off by default
dir = "data/dumps/local"
compression = "none"           # "none" | "snappy"
```

Files land at `<dir>/<sanitized_source_id>/<minute>.pcap[.snappy]` —
one subdirectory per capture source, rotated on the wall-clock minute
boundary. Files are sparse, so an idle minute costs no disk. With
`compression = "snappy"` the per-minute file is written using
snappy-framed compression and gets a `.snappy` suffix; decompress with
`snzip -d` before opening in Wireshark.

### Optional: tune internal queue depths

Bounded channels between pipeline stages, all default to 4096:

```toml
[pipeline.queues]
raw_pkts           = 4096
parsed_pkts        = 4096
http_parse_events  = 4096
http_joiner_events = 4096
agent_calls        = 4096
llm_events         = 4096
storage_calls      = 4096
storage_turns      = 4096
storage_metrics    = 4096
storage_exchanges  = 4096
```

Keys mirror the `pipeline-health` page's queue cell labels (drop the
`q_` prefix). `storage_*` are shared sinks (fan-in across every pipeline);
the others are per-pipeline. Bump the queue named by the first cell that
reddens; lower them if memory is tight.

## `[storage]` — backend selection

```toml
[storage]
backend = "duckdb"             # "duckdb" (default) or "clickhouse"
```

`duckdb` (embedded, single-node / edge) and `clickhouse` (dedicated server for
large-scale, high-throughput analytics) are both shipped. PostgreSQL is designed
but not yet wired up; see `docs/design/06-storage.md`.

### DuckDB-specific

```toml
[storage.duckdb]
path = "data/heron.duckdb"
```

The path is relative to the working directory. Parent directories are
created automatically on first run.

### ClickHouse-specific

```toml
[storage.clickhouse]
url               = "http://localhost:8123"   # HTTP interface (default port 8123)
database          = "heron"                    # created on startup if absent
user              = "default"
password          = ""
optimize_on_sweep = false                      # OPTIMIZE TABLE ... FINAL after retention
```

The backend talks to ClickHouse over its HTTP interface and creates the schema
(MergeTree fact tables + a ReplacingMergeTree `traces`) on first run.
Retention runs through the same `[storage.retention]` schedule as DuckDB, using
lightweight `DELETE`. `optimize_on_sweep` eagerly reclaims space after each
sweep at the cost of a full-table merge; off by default (TTL-style background
merges reclaim lazily).

### Sink batching

How many records to buffer before flushing a batch to the database:

```toml
[storage.sink]
batch_size = 1000
flush_interval_ms = 1000       # flush after this many ms even if batch < size
```

Larger batches are more efficient but increase write latency; defaults
are tuned for ~1k req/s sustained ingestion.

### Retention (enabled by default)

Retention runs by default with conservative TTLs so the DuckDB file
stays bounded out of the box. The block below shows the **active
defaults** — you only need to add it to override:

```toml
[storage.retention]
enabled = true                 # set to false to opt out entirely
check_interval_secs = 3600     # how often to run the cleanup sweep
spans = 30                     # keep spans (per-call detail) for N days; caps `traces`
traces = 30                    # keep traces (agent turns) for N days; must be <= spans
http_exchanges = 7             # keep http_exchanges (bulkiest table) for N days

# Per-granularity retention for llm_metrics. Missing keys fall back to
# the defaults below — overriding "10s" does NOT drop the others.
[storage.retention.metrics]
"10s" = 1                      # keep 10-second buckets for N days
"1m"  = 7                      # keep 1-minute buckets for N days
"5m"  = 30
"1h"  = 365
```

Behavior:

- `enabled = false` skips the retention loop entirely.
- Any per-table field (or any `metrics` granularity) set to `0` means
  **never expire that table**. Combine with `enabled = true` to keep
  some tables forever and let others rotate.
- Unknown granularity labels under `[storage.retention.metrics]`
  (anything not in `10s` / `1m` / `5m` / `1h`) are logged at warn and
  ignored — useful to catch typos like `"10sec"`.
- `http_exchanges` stores raw HTTP transport records (headers + bodies)
  per request/response and is by far the bulkiest table; keep its TTL
  short unless you specifically need a longer forensics window.
- `traces` must not outlive `spans`. `traces.span_ids` references
  rows in `spans`, and the no-JOIN trace-detail read returns
  empty/partial span lists once those references go stale. `heron
  config validate` enforces `traces <= spans` (with `spans = 0` treated
  as infinite, which satisfies any finite `traces`).
- The pre-rename keys `calls` / `turns` are still accepted as deprecated
  aliases for `spans` / `traces`, so existing config files keep working.

## `[api]` — REST server

```toml
[api]
listen = "0.0.0.0"
port = 3000
```

The API also serves the embedded web console at `/`. There is no
authentication built in — bind to a private interface or front it with a
reverse proxy if exposed.

## `[internal_metrics]` — pipeline self-monitoring

```toml
[internal_metrics]
enabled = true
interval_secs = 10
```

Emits stage-by-stage throughput, queue depths, and drop counters to the
log and to the API. Useful for verifying that workers aren't backed up.
See `docs/design/08-internal-metrics.md`.

## BPF filters per scenario

The BPF expression filters packets at the kernel level before they reach
Heron. Use it to scope capture to the LLM API path and avoid
processing unrelated traffic.

> **Heron sees plaintext HTTP.** It runs *post-TLS* — either on the
> inference host where TLS is already terminated, or fed by cloud-probe
> from a TLS-terminating LB. Filters target the *internal* port, not 443.

| Setup | BPF filter |
|---|---|
| vLLM (default port) | `tcp port 8000` |
| Ollama | `tcp port 11434` |
| Generic OpenAI-compatible inference proxy | `tcp port 8080` |
| TLS-terminating LB → backend pool on multiple ports | `tcp portrange 8000-8010` |
| Specific upstream pool | `tcp port 8000 and host 10.0.0.5` |
| Multiple proxies | `(tcp port 8000) or (tcp port 8001)` |

Test a filter without Heron first:

```bash
sudo tcpdump -i eth0 -n 'tcp port 8000'
```

If `tcpdump` shows the LLM traffic you expect, the same filter will work
in Heron's `bpf_filter`.

## Multi-pipeline example

Two pipelines on one node — local high-priority capture isolated from
slower cloud-probe ingestion that may bursty:

```toml
# Local capture: small queues, fewer shards, low memory.
[[pipeline]]
name = "local"
dispatcher_count = 1
flow_shard_count = 4
[pipeline.turn]
shard_count = 1
[pipeline.metrics]
shard_count = 1
[[pipeline.sources]]
type = "pcap"
interface = "eth0"
bpf_filter = "tcp port 8000"

# Remote ingestion: more shards, larger queues, isolated from local.
[[pipeline]]
name = "remote"
dispatcher_count = 2
flow_shard_count = 8
[pipeline.turn]
shard_count = 2
[pipeline.metrics]
shard_count = 2
[pipeline.queues]
raw_pkts = 16384
parsed_pkts = 16384
[[pipeline.sources]]
type = "cloud-probe"
endpoint = "tcp://0.0.0.0:5555"
recv_hwm = 5000
```

## Sizing guidance

These are starting points; tune based on `internal_metrics` output.

| Traffic shape | `dispatcher_count` | `flow_shard_count` | Notes |
|---|---|---|---|
| < 500 req/s, single host | 1 | 4 | Default config is fine |
| 500–5,000 req/s | 1–2 | 8 | Increase `metrics.shard_count` to 2 |
| 5,000–20,000 req/s | 2–4 | 16 | Watch CPU; shard counts ≤ physical cores |
| > 20,000 req/s | 4+ | 32+ | Consider multiple pipelines or scaling out |

Same connection's packets always land on the same flow worker (sharded
by flow key), so increasing `flow_shard_count` helps only when many
distinct connections are active.

## Minimal config (smallest valid file)

```toml
[[pipeline]]
name = "live"
[[pipeline.sources]]
type = "pcap"
interface = "eth0"

[storage]
backend = "duckdb"
[storage.duckdb]
path = "data/heron.duckdb"

[api]
listen = "127.0.0.1"
port = 3000
```

Everything else uses defaults.
