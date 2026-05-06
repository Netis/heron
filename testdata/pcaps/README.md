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
| `openclaw-openai.pcap` | OpenAI `/v1/chat/completions` | OpenClaw (OpenAI/JS SDK + GLM) | 1.4 MB | **4** (4/0) | Two distinct user sessions on `openclaw`; client echoes `assistant.tool_calls[].id` without the underscore (`calld9c1...`) — exercises `canonicalize_tool_id`. Without it the 4 turns would shatter into many single-call turns |
| `openclaw-anthropic.pcap` | Anthropic `/v1/messages` | OpenClaw (Anthropic/JS SDK + GLM-5) | 1.0 MB | **4** (4/0) | One main session on `openclaw` (compaction-summarizer calls filtered as auxiliary by `OpenClawProfile::is_auxiliary` before turn assembly — pre-profile they appeared as two extra `gen-*` synth-id sessions because their first-user/first-assistant boilerplate hashed identically). GLM-5 emits parallel `tool_use` blocks where every `content_block_start` arrives before any `input_json_delta` — exercises the index-keyed SSE accumulator. Without per-index tracking, parallel `tool_use.input` collapses to `""` or attaches to the wrong block |
| `hermes-openai.pcap` | OpenAI `/v1/chat/completions` | Hermes Agent (Nous Research) via `OpenAI/Python` SDK + GLM-5 | 553 KB | **2** (2/0) | One user-facing 4-call conversation classified as `hermes` by body fingerprint (`HermesProfile` matches ≥2 of `skill_view`/`skill_manage`/`skills_list`/`delegate_task`/`session_search`/`cronjob` in `tools[]`), plus a 1-call chat-title-generation one-shot. The title-gen call has no `tools` and no Hermes markers, so `HermesProfile` does not match it and it falls through to `generic` — by design, since on the wire it *is* an independent turn (own session, own user-start, own `finish_reason=stop`). |

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
