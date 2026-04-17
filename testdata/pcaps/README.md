# PCAP Fixtures

Local test pcaps for TokenScope development (capture playback + turn grouping).
The `.pcap` files themselves are **not** checked in (see `.gitignore`). This
README documents what each fixture contains so tests and local runs can assert
against known ground truth.

All captures are loopback (`127.0.0.1:8317`) plaintext HTTP — matching
TokenScope's post-TLS server-side deployment model.

## Fixtures

| File | Provider / Endpoint | Client | Size | Turns (Complete/Incomplete) | Notes |
|---|---|---|---|---|---|
| `claude-cli-messages.pcap` | Anthropic `/v1/messages?beta=true` | claude-cli | 3.8 MB | **1** (1/0) | Single connection, single tool-calling turn |
| `claude-cli-messages-multi.pcap` | Anthropic `/v1/messages?beta=true` | claude-cli | 5.4 MB | **3** (≥1 Complete) | Long multi-turn session, single `session_id`; auto title-gen (empty `tools`) filtered as auxiliary, Task sub-agent calls attach to parent turn |
| `codex-cli-messages-multi.pcap` | OpenAI `/v1/responses` | codex-cli | 18 MB | **2** (1/1) | Multi-turn session, single `session_id` (see note); 2nd turn cut off mid-roundtrip by EOF |

Turn counts are ground truth verified against the current implementation and
are intended as assertions for turn-grouping tests
(`server/ts-turn/tests/integration.rs`). `Incomplete` turns reflect streams
that did not close cleanly within the capture window — the grouping is still
deterministic across runs and shard counts.

> **Note on `codex-cli-messages-multi.pcap`:** Codex's `X-Codex-Turn-Metadata`
> header reuses one `turn_id` across a whole `codex` invocation. The
> implementation honors the protocol, so a capture spanning multiple
> user-interactive sessions can still report a small number of turns.

## Usage

Run TokenScope against a fixture via the pcap-file capture backend:

```bash
# example — adjust flags to match current CLI/config
cargo run -p tokenscope -- --pcap testdata/pcaps/claude-cli-messages-multi.pcap
```

## Obtaining the files

These pcaps are not in git. Ask @timmy.yuan for a copy or capture your own
loopback traffic against `127.0.0.1:8317` while using claude-cli / codex-cli.
