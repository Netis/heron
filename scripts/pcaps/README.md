# pcap corpus tooling

Build and maintain the committed regression corpus under `testdata/pcaps/corpus/`
that `server/h-turn/tests/corpus_golden.rs` replays against committed goldens.

## Why scrubbing is mandatory

Captures are loopback **plaintext HTTP**, so they carry real `Authorization`
tokens, provider keys, prompts/responses, and sometimes private IPs / home
paths. `scripts/lint/check-leakage.sh` skips binary files, so a raw pcap would
sail past it. `scrub_pcap.py` is the secret gate; `check-pcap-corpus.sh` is the
CI backstop that scans the committed bytes.

## scrub_pcap.py — what it does

Length-preserving byte substitution over the raw pcap: each secret/PII run is
replaced with an equal-length filler, so every TCP segment keeps its size — no
Content-Length / sequence-number / checksum rewrite needed, and the file still
replays to the **same** `LlmCall`/`AgentTurn` ground truth (Heron reassembles by
5-tuple+seq and does not validate TCP/IP checksums).

- **Preserved** (so detection + extraction still fire): JSON structure, key
  names, `type` discriminators, tool names + tool-call structure, `usage`
  numbers (= ground truth), `finish_reason`/`stop_reason`, model strings, and
  agent-detection headers (`User-Agent`, `X-Claude-Code-Session-Id`,
  `x-session-affinity`, …). Session/turn-id VALUES inside those headers are
  replaced with a *deterministic same-length fake* so linking still resolves.
- **Redacted** (value bytes only): `Authorization`/`x-api-key`/`Bearer` tokens,
  `sk-*`/`sk-ant-*` keys, JWTs, PEM private keys, RFC1918 IPs,
  `/Users|/home/<user>` paths, e-mail addresses.

It refuses to emit if any forbidden pattern survives.

## Add a corpus cell

```bash
# 1. obtain a raw capture (see recipes below) → testdata/pcaps/raw/<id>.raw.pcap
# 2. scrub it into the committed corpus + write the audit sidecar:
scripts/pcaps/scrub_pcap.sh testdata/pcaps/raw/<id>.raw.pcap --id <id>
#    (optional: --trim '<tshark display filter>' to keep only relevant streams)
# 3. flip the entry to status="active" in testdata/pcaps/corpus.toml
# 4. generate + eyeball the golden, then run it:
just corpus bless
just corpus test
just corpus lint
# 5. commit: the .pcap lands in git-LFS (.gitattributes), plus corpus.toml,
#    golden/<id>.json and <id>.scrub.json.
```

## Capture recipes

All captures are loopback `127.0.0.1:8317` plaintext HTTP (Heron's post-TLS
model). Never commit `testdata/pcaps/raw/` — only the scrubbed `corpus/` output.

**Fresh capture** (fills a backend-quirk / new-agent cell):

```bash
# run backend X serving an OpenAI/Anthropic/Gemini-compatible API on :8317,
# then in another shell:
sudo tcpdump -i lo -w testdata/pcaps/raw/<id>.raw.pcap 'tcp port 8317'   # Linux
# (macOS: -i lo0). Drive agent Y to produce the target scenario, then Ctrl-C.
scripts/pcaps/scrub_pcap.sh testdata/pcaps/raw/<id>.raw.pcap --id <id>
```

Backends to stand up for the quirk cells: vllm, sglang, ollama, litellm,
cliproxy. The high-value **cliproxy mixed-format** cell: point an agent at a
cliproxy that returns Anthropic-shaped `usage` on an OpenAI-Chat request
(exercises the `openai/chat.rs` usage fallback, issue #96).

**Trim from an existing large capture**:

```bash
# isolate the relevant TCP streams, then scrub (verify gates it):
tshark -r big.pcap -Y 'tcp.port==8317 && http' -w testdata/pcaps/raw/<id>.raw.pcap
scripts/pcaps/scrub_pcap.sh testdata/pcaps/raw/<id>.raw.pcap --id <id>
```

## Notes / limitations

- The scrubber redacts ASCII secrets in payloads; it does not rewrite binary
  L2/L3 addresses (loopback captures are already `127.0.0.1`). The corpus lint
  scans for residual RFC1918/keys/paths as the backstop.
- If a redaction pattern would change length it is rejected — extend the rules
  in `scrub_pcap.py` rather than breaking length-preservation.
