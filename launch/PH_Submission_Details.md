# Product Hunt 提交资料包(修正版)

> 修正基线:对齐已发布产品。相对 marketing 原稿的改动——
> - 安装命令 `npm i -g heron-ai` → `curl … install.sh`(无 npm 包,console 为 private)
> - eBPF "production-gated" → "experimental / opt-in"(仍是 opt-in cargo feature,Linux,需 CAP_BPF)
> - 版本 → v0.7.0
> - 不出现运行时 `heron --ebpf` 命令(eBPF 经 cargo feature + TOML 配置)
>
> eBPF 段的最终取舍与 hero GIF 由 marketing 负责;本稿提供准确基线。

## 基本信息 (Basic Info)

- **Name of product:** Heron
- **Tagline (60 chars max):** The Wireshark for AI Agents
- **Website URL:** https://heron-ai.pages.dev
- **Pricing:** Free / Open Source
- **Topics:** Developer Tools, Open Source, Artificial Intelligence, DevOps
- **GitHub URL:** https://github.com/Netis/heron

## 详细描述 (Description - 260 chars max)

Heron is a passive network analyzer that reconstructs what your AI agents are actually doing. Zero SDKs. Zero proxy. On Linux, an experimental eBPF source reads TLS-encrypted LLM calls on-host and tells you which agent process made them.

## 视频与图片 (Gallery)

按顺序上传 `Heron_Launch_Package/producthunt-images` 和 `video` 中的文件:
1. `heron-demo-narrated.mp4`(主 Demo 视频)
2. `01_hero_preview.png`
3. `02_ebpf_concept.png`
4. `03_agent_turns.png`
5. `04_services_topology.png`
6. `05_http_exchanges.png`

## Maker's First Comment (首发评论)

```markdown
Hey PH! 👋 I'm the team lead behind Heron at Netis.

I built Heron because I got tired of my AI agent loops looking like 200 OK in the logs while the actual agent was stuck replaying the same tool call for 47 seconds straight.

**What Heron does:**
Heron is a passive analyzer that reconstructs what your AI agents are actually doing — from the traffic itself. No SDK, no proxy, nothing in the request path. It captures LLM traffic (OpenAI, Anthropic, Gemini, vLLM, SGLang, Ollama…), parses the wire protocol, and stitches multi-call interactions into agent turns you can actually debug.

**What's new in v0.7.0:**
🔬 On-host eBPF capture (experimental, opt-in) — hook SSL_read/SSL_write to read TLS-encrypted agent traffic as plaintext, with process attribution (which agent process made which call). No proxy, no TLS terminator.

📊 We discovered that ~73% of Claude Code's Opus turns in our production capture were hidden security-monitor sidecars — Heron now filters them automatically so you see real agent work, not housekeeping noise.

🧬 One-click SFT trajectory export — turn your production agent traffic into fine-tuning training data without re-running anything.

Built in Rust, ships as a single binary with the React console embedded. Apache-2.0. Would love your feedback!

🦩 Try it:
  curl -fsSL https://raw.githubusercontent.com/Netis/heron/main/install.sh | INSTALL_DIR="$HOME/.local" sh
  heron --pcap-file capture.pcap --no-retention
⭐ GitHub: https://github.com/Netis/heron
```
