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

Because the signature is build-specific, deriving it is a per-release step:

1. Find a symbolized reference of the **same BoringSSL** (or disassemble the
   target around the function located by xref to a vendored source-path string),
   and read the first ~48 bytes of the `SSL_write` / `SSL_read` prologue.
2. Wildcard (`??`) the 4-byte RIP-relative displacements (e.g. the `e8 ?? ?? ??
   ??` call near the end) so the signature survives PIE/ASLR-independent relocs
   across rebuilds of the same source.
3. Validate uniqueness against the actual target binary:
   ```
   cargo run -p h-capture --example sigscan_probe -- /path/to/bun "55 41 57 ?? …"
   ```
   A good signature reports exactly **one** match (`OK: unique offset`).
4. Put the validated patterns in config:
   ```toml
   [[sources.targets]]
   binary = "/path/to/claude/bun"
   flavor = "boringssl"
   write_sig = "…"   # validated SSL_write prologue
   read_sig  = "…"   # validated SSL_read prologue
   ```

No built-in BoringSSL signature ships (`flavor_signatures` returns none): a
guessed-wrong signature is worse than none (it could attach to the wrong
function), so the unique-match requirement plus operator-supplied, validated
patterns is the safe default.

## Status

- Mechanism (sigscan + offset-attach + config + discovery tool): **done,
  verified end-to-end** on a real SSL function.
- A validated Bun/Claude Code signature: **per-release data**, derived with the
  procedure above. Wildcarding the displacement bytes (step 2) is what makes a
  signature resilient across patch rebuilds of one Bun line.
