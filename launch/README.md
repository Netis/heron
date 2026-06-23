# Launch collateral — v0.7.0

Paste-ready launch material, corrected to match the shipped product. Source was
the marketing `Heron_Launch_Package.zip`; this folder holds the reconciled copy.

## What's here

- [`PH_Submission_Details.md`](PH_Submission_Details.md) — Product Hunt fields + Maker's first comment
- [`HN_and_Twitter_Posts.md`](HN_and_Twitter_Posts.md) — Show HN post + Twitter/X thread

## Corrections applied vs the marketing draft

| Item | Draft | Corrected |
| --- | --- | --- |
| Install command | `npm i -g heron-ai` | `curl … install.sh` (no npm package; console is a private workspace pkg) |
| eBPF maturity | "production-gated" | "experimental / opt-in" (cargo feature, Linux, `CAP_BPF`) |
| eBPF invocation | `heron --ebpf` | removed — no such runtime flag; eBPF is enabled via the `ebpf` cargo feature + TOML `type = "ebpf"` |
| API endpoint examples | `/api/agent-turns` | `/api/traces` (canonical after the OTel rename; old route still works as a deprecated alias) |
| Version | v0.6.0 | v0.7.0 |

## Owned by marketing (not in this branch)

- **eBPF segment final wording + hero GIF.** The hero GIFs in the zip hardcode
  `heron --ebpf` and show `Heron v0.6.0`; marketing will re-cut them (drop the
  `--ebpf` command, bump the version). The repo's `docs/images/hero.gif` is
  currently the valid `hero-pcap.gif` as a placeholder until the re-cut lands.
- **Landing page** `https://heron-ai.pages.dev` — not in this repo (`site/` only
  has a favicon); stand it up separately before launch.

## Asset upload (Product Hunt — do not commit binaries to the repo)

Upload directly to Product Hunt from the zip:
`video/heron-demo-narrated.mp4` (main demo) + `producthunt-images/01…05`.

## GitHub repo settings (do on launch day, via repo Settings)

**Topics:**
```
ebpf, llm-tracing, agent-observability, passive-monitoring, llm-monitoring,
ai-agent, network-probe, sft-training-data, wireshark, openai, anthropic,
vllm, sglang
```

**Description:**
```
The Wireshark for AI Agents — passive observability that reconstructs agent
turns from network traffic. Zero SDK, zero proxy, zero code changes. Export SFT
training data from production traffic.
```

**Social preview:** upload a 1280×640 brand image (to be produced).

## Release

`v0.7.0` version files are already bumped (VERSION SSOT + Cargo.toml +
package.json + CHANGELOG). Tag/publish (`git tag v0.7.0` → `release.yml`) is the
explicit release step and gated on a passing `staging-soaked` status — do it on
the soaked commit when ready, not as part of this docs branch.
