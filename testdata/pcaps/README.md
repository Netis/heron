# PCAP Fixtures

Local test pcaps for Heron development (capture playback + turn grouping).
The `.pcap` files themselves are **not** checked in (see `.gitignore`). This
README documents what each fixture contains so tests and local runs can assert
against known ground truth.

All captures are loopback (`127.0.0.1:8317`) plaintext HTTP — matching
Heron's post-TLS server-side deployment model.

## Fixtures

| File | Provider / Endpoint | Client | Size | Turns (Complete/Incomplete) | Notes |
|---|---|---|---|---|---|
| `claude-cli-messages.pcap` | Anthropic `/v1/messages?beta=true` | claude-cli | 3.8 MB | **1** (1/0) | Single connection, single tool-calling turn |
| `claude-cli-messages-multi.pcap` | Anthropic `/v1/messages?beta=true` | claude-cli | 5.4 MB | **3** (≥1 Complete) | Long multi-turn session, single `session_id`; auto title-gen (empty `tools`) filtered as auxiliary, Task sub-agent calls attach to parent turn |
| `codex-cli-messages-multi.pcap` | OpenAI `/v1/responses` | codex-cli | 18 MB | **2** (1/1) | Multi-turn session, single `session_id` (see note); 2nd turn cut off mid-roundtrip by EOF |
| `openclaw-openai.pcap` | OpenAI `/v1/chat/completions` | OpenClaw (OpenAI/JS SDK + GLM) | 1.4 MB | **4** (4/0) | Two distinct user sessions on `openclaw`; client echoes `assistant.tool_calls[].id` without the underscore (`calld9c1...`) — exercises `canonicalize_tool_id`. Without it the 4 turns would shatter into many single-call turns |
| `openclaw-anthropic.pcap` | Anthropic `/v1/messages` | OpenClaw (Anthropic/JS SDK + GLM-5) | 1.0 MB | **4** (4/0) | One main session on `openclaw` (compaction-summarizer calls filtered as auxiliary by `OpenClawProfile::is_auxiliary` before turn assembly — pre-profile they appeared as two extra `gen-*` synth-id sessions because their first-user/first-assistant boilerplate hashed identically). GLM-5 emits parallel `tool_use` blocks where every `content_block_start` arrives before any `input_json_delta` — exercises the index-keyed SSE accumulator. Without per-index tracking, parallel `tool_use.input` collapses to `""` or attaches to the wrong block |
| `hermes-openai.pcap` | OpenAI `/v1/chat/completions` | Hermes Agent (Nous Research) via `OpenAI/Python` SDK + GLM-5 | 553 KB | **2** (2/0) | One user-facing 4-call conversation classified as `hermes` by body fingerprint (`HermesProfile` matches ≥2 of `skill_view`/`skill_manage`/`skills_list`/`delegate_task`/`session_search`/`cronjob` in `tools[]`), plus a 1-call chat-title-generation one-shot. The title-gen call has no `tools` and no Hermes markers, so `HermesProfile` does not match it and it falls through to `generic` — by design, since on the wire it *is* an independent turn (own session, own user-start, own `finish_reason=stop`). |
| `gemini-cli-apikey.pcap` | Gemini AI Studio `/v1beta/models/{m}:streamGenerateContent?alt=sse` | Gemini CLI (API-key mode) via `@google/genai` SDK | 1.3 MB | **2** (2/0) | 7 LlmCalls split 4+3 across one shared `session_id` (form `tu-<16hex>` — Gemini has no protocol-level tool ids, so `first_assistant_sig_*` synthesizes a stable opaque id by FNV-1a hashing the canonical model-turn sig string). Turn A (4 calls): initial prompt + 3 tool roundtrips, closes on call-4 pure-text response (no `functionCall` ⇒ wire `STOP` not rewritten to synthetic `TOOL_USE` ⇒ `is_turn_terminal` true). Turn B (3 calls): user follow-up prompt at call 5 re-arms `is_user_turn_start`. No dedicated `gemini-cli` profile yet → all calls land on `generic`. Exercises (a) `GenericProfile::matches()` covering `gemini-aistudio`, and (b) `first_assistant_sig_*` returning `ToolId(_)` (not `Text(_)`) for tools-bearing model turns so `generic`'s helper-shape one-shot gate doesn't spuriously fire on call 1 (which historically split call 1 into its own session). |

Turn counts are ground truth verified against the current implementation and
are intended as assertions for turn-grouping tests
(`server/h-turn/tests/integration.rs`). `Incomplete` turns reflect streams
that did not close cleanly within the capture window — the grouping is still
deterministic across runs and shard counts.

> **Note on `codex-cli-messages-multi.pcap`:** Codex's `X-Codex-Turn-Metadata`
> header reuses one `turn_id` across a whole `codex` invocation. The
> implementation honors the protocol, so a capture spanning multiple
> user-interactive sessions can still report a small number of turns.

## Usage

Run Heron against a fixture via the pcap-file capture backend:

```bash
# example — adjust flags to match current CLI/config
cargo run -p heron -- --pcap testdata/pcaps/claude-cli-messages-multi.pcap
```

## Obtaining the files

These (legacy, hand-distributed) pcaps are not in git. Ask @timmy.yuan for a
copy or capture your own loopback traffic against `127.0.0.1:8317`. They feed
the turn-grouping tests in `server/h-turn/tests/integration.rs`, which skip
when absent.

---

# Committed regression corpus (`corpus/`)

Separate from the legacy fixtures above: `testdata/pcaps/corpus/` holds a
**curated, secret-scrubbed, git-LFS-committed** corpus that runs as a golden
regression gate in CI (`server/h-turn/tests/corpus_golden.rs`,
`cargo test --workspace`).

- **`corpus.toml`** — the single source of truth for the
  (backend × agent × wire_api × scenario) matrix. Each `[[fixture]]` is either
  `status = "active"` (a scrubbed `corpus/<file>.pcap` + `golden/<id>.json` are
  committed) or `status = "pending"` (a target cell whose capture isn't obtained
  yet — listed for visibility, skipped by the test).
- **`corpus/*.pcap`** — scrubbed fixtures, stored via git-LFS (`.gitattributes`).
  Run `git lfs pull` to materialize them; CI checks out with `lfs: true`. An
  unsmudged LFS pointer is treated as "absent" and skipped.
- **`golden/<id>.json`** — the deterministic extracted projection (no uuids /
  timing). Regenerate with `just corpus bless` and review the diff.
- **`<id>.scrub.json`** — per-fixture redaction audit (which rules fired, size),
  so reviewers can confirm scrubbing happened without seeing secrets.

### Why "backend" is a label, not a detection output

vLLM / SGLang / Ollama / LiteLLM / CLIproxy all speak `openai-chat` /
`openai-responses` on the wire — Heron does not distinguish them per-deployment.
Their value as fixtures is the **parsing quirk** each one locks in (SSE framing,
usage-block field names, finish_reason vocab, the CLIproxy mixed-format usage
fallback). `backend_label` + `[fixture.expect.quirks]` document that; the hard
detection assertion is `wire_apis`.

### Adding / refreshing a cell

See [`scripts/pcaps/README.md`](../../scripts/pcaps/README.md) for the
capture → scrub → bless → commit workflow and the capture recipes. Quick:

```bash
scripts/pcaps/scrub_pcap.sh testdata/pcaps/raw/<id>.raw.pcap --id <id>
# flip status="active" in corpus.toml, then:
just corpus bless && just corpus test && just corpus lint
```

Never commit raw captures — `testdata/pcaps/raw/` is gitignored.
