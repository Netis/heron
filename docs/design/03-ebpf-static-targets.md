# eBPF capture for static-binary TLS (Bun / Claude Code) — Phase 3

The dynamic-libssl eBPF source (Phase 1) attaches uprobes to `SSL_read` /
`SSL_write` **by exported symbol**. That covers Python, curl, and anything that
dynamically links OpenSSL/BoringSSL. It does **not** cover runtimes that
statically link and strip their TLS stack — most importantly **Claude Code**,
distributed as a single ~100 MB **Bun** binary with a vendored BoringSSL.

This note records what we verified about that target and the mechanism that
handles it.

## De-risking findings (measured, not assumed)

Two questions gate the whole approach. Both were verified empirically on a
Linux 6.8 host with Bun 1.3.13.

### 1. Does Bun's `fetch()` use HTTP/2? → **No, it offers only HTTP/1.1.**

Heron's parser is HTTP/1.x only; an h2 client would decrypt to HPACK/binary
frames we can't reconstruct. So this is existential for Phase 3.

A dual-protocol Node server (`http2.createSecureServer({ allowHTTP1: true })`)
logging each client's offered ALPN via `ALPNCallback`:

| client | offered ALPN | negotiated |
|---|---|---|
| **Bun `fetch()`** | `[http/1.1]` | **HTTP/1.1** |
| Node `fetch()` | `[http/1.1]` | HTTP/1.1 |
| curl `--http1.1` (control) | `[http/1.1]` | HTTP/1.1 |
| curl (default, control) | `[h2, http/1.1]` | HTTP/2 |

Bun's fetch offers **only** `http/1.1` — it never proposes h2, so the server
(however h2-capable, e.g. `api.anthropic.com`) can only pick HTTP/1.1. The
captured plaintext is therefore parseable by Heron unchanged. (Note this is a
property of the *client's* ALPN offer; a direct `node:tls` probe that explicitly
offers `['h2','http/1.1']` does negotiate h2 with anthropic — but Bun's fetch
doesn't make that offer.)

### 2. Can we attach by symbol? → **No, Bun strips all `SSL_*` symbols.**

```
$ nm -D bun | grep -c SSL_      # dynamic symbols
0
$ nm  bun | grep -c SSL_        # .symtab (985 syms total, none SSL_*)
0
$ ldd bun | grep -i ssl         # dynamic libssl?
(none — statically linked)
```

The binary embeds BoringSSL from `vendor/boringssl/ssl/*.cc` (the source paths
survive as assert strings), but every TLS symbol is stripped. The only handle
left is the **machine code** of `SSL_read` / `SSL_write` — located by byte
signature, attached by **file offset**.

## Mechanism: byte-signature → file offset → offset uprobe

- `h-capture/src/ebpf/sigscan.rs` — pure, cross-platform, unit-tested. Parses an
  ELF, scans its executable `PT_LOAD` segments for a masked byte signature
  (`??` = wildcard, for build-varying displacements/immediates), and returns the
  **file offsets** of matches — exactly the value the kernel uprobe API takes.
- The loader (`source.rs`) reads the target binary, resolves a **unique** offset
  per function (`resolve_single_offset` — zero matches = wrong/stale signature,
  many = too-loose signature; both are refused rather than mis-attached), and
  attaches the already-loaded BPF programs by offset (`attach_offset`).
- `[[sources.targets]]` config carries `binary`, `flavor`, and optional
  `write_sig` / `read_sig` patterns. **Signatures are version-specific data, not
  code**: a BoringSSL prologue pins one Bun/Claude Code build, so it lives in
  config and an operator can update it for a new release without a rebuild.

The mechanism is verified end-to-end against real SSL functions in the system
libssl (which has symbols, giving ground truth):

- The scanner maps a 48-byte signature of `SSL_write` / `SSL_read` to exactly
  the symbol's file offset (16-byte prologue → 5 matches, 32 → 3, 48 → **1**;
  the shared CET+frame+canary preamble is why short signatures are ambiguous).
- Running the smoke in **offset-attach mode** (sentinel `ssl_libs` so only the
  byte-offset path is active) captured real HTTP/1.1 plaintext — `curl … GET /
  HTTP/1.1`, attributed to its process — proving signature → scan → offset →
  attach → capture → attribution end-to-end.

## Deriving a signature for a Bun / Claude Code release

Because the signature is build-specific, deriving it is a per-release step. Two
config paths exist:

- `write_offset` / `read_offset` — explicit file offsets, bypass scanning. The
  fast path once the offset is known (and the validation path while deriving a
  signature). The loader attaches directly.
- `write_sig` / `read_sig` — byte-signature patterns, resolved to a unique
  offset at attach time. The resilient path: a signature survives ASLR and
  matches any process mapping the binary.

To derive either, you must first **locate `SSL_write` / `SSL_read` in the
stripped binary**. What we learned probing Bun 1.3.13 (see below) makes the
recipe concrete:

1. Find the TLS write call chain dynamically — `bpftrace` ustack on the write
   syscall from the runtime:
   ```
   bpftrace -e 'tracepoint:syscalls:sys_enter_write /comm=="bun"/ { @[ustack(perf,12)] = count(); }'
   ```
   For a non-PIE EXEC (Bun is `ET_EXEC`), the frame addresses are link-time
   vaddrs; convert to file offsets via the executable `PT_LOAD` delta
   (`file_off = vaddr - (p_vaddr - p_offset)`; 0x318000 for this Bun).
2. The plaintext-bearing functions (`SSL_write` / `ssl_write_internal`, arg1 =
   `buf` in RSI) are the **outer** frames; the inner cluster is the record /
   encryption layer (ciphertext). Validate a candidate **function entry** by
   attaching there and checking RSI:
   ```
   HERON_EBPF_TARGET_BIN=/path/to/bun HERON_EBPF_WRITE_OFFSET=0x… \
     ./target/debug/examples/ebpf_smoke   # prints captured HTTP/1.1 if RSI is plaintext
   ```
3. Read the first ~48 bytes at the validated entry, wildcard (`??`) the 4-byte
   RIP-relative displacements (e.g. the `e8 ?? ?? ?? ??` call operands), and
   confirm uniqueness:
   ```
   cargo run -p h-capture --example sigscan_probe -- /path/to/bun "55 48 89 e5 …"
   ```
4. Put the validated offset (or signature) in config:
   ```toml
   [[sources.targets]]
   binary = "/path/to/claude/bun"
   flavor = "boringssl"
   write_sig = "…"   # or write_offset = 0x…
   read_sig  = "…"   # or read_offset  = 0x…
   ```

No built-in BoringSSL signature ships (`flavor_signatures` returns none): a
guessed-wrong signature is worse than none (it could attach to the wrong
function), so the unique-match requirement plus operator-supplied, validated
patterns/offsets is the safe default.

## What we measured on Bun 1.3.13 (the hard part: entry identification)

The mechanism is proven; the open work is **reliably finding the function
entry** in this specific binary shape. Recorded so a follow-up doesn't re-walk it:

- Bun's BoringSSL is compiled **without CET** — functions do **not** start with
  `endbr64`, so the easy entry marker the system libssl has is absent here.
  Prologues are plain (`55 48 89 e5 …` push-rbp, or frame-pointer-omitted).
- `bpftrace` located the exact write call chain: an inner cluster (`~0x3c2xxxx`,
  the record/encrypt layer that calls `write@plt`) and outer frames
  (`~0x42xxxxx`) on the plaintext side.
- Offset-attach **registers correctly** against the EXEC binary
  (`bpftool link list` shows `uprobe /…/bun+0x…` at the computed file offset;
  the r-xp mapping `…r-xp 02808000 …/bun` confirms the delta), so aya + the
  loader are sound on a non-PIE target.
- The remaining blocker: int3-padding / prologue heuristics yield **basic-block
  boundaries**, not guaranteed function entries on the executed path, in this
  jump-dense optimized code. Pinning the true entry needs proper
  disassembly-based function-boundary analysis (or a symbol-matched BoringSSL
  build — out of reach without the matching toolchain). `bpftrace`'s numeric
  uprobe (`uprobe:bin:0xADDR`) can't help validate here: it resolves addresses
  via the symbol table, which is empty for BoringSSL, so use the
  `HERON_EBPF_WRITE_OFFSET` path above instead.

## Status

- Mechanism (sigscan + offset/sig attach + config + `sigscan_probe`): **done,
  verified end-to-end** on a real SSL function (system libssl).
- De-risking: **done** — Bun fetch is HTTP/1.1; Bun is stripped static BoringSSL,
  non-CET.
- A validated Bun/Claude Code offset or signature: **open** — needs
  disassembly-based entry identification (above), then it is per-release data.
