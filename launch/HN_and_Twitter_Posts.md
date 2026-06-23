# Hacker News & Twitter 发帖指南(修正版)

> 修正基线:对齐已发布产品。相对 marketing 原稿的改动——
> - 安装命令 `npm i -g heron-ai` → `curl … install.sh`
> - eBPF "production-gated" → "experimental / opt-in"
> - 版本表述 → v0.7.0(v0.5.6 的过滤作为历史时间线保留)
> - 不出现运行时 `heron --ebpf` 命令
>
> eBPF 段最终取舍与 GIF 由 marketing 负责;本稿提供准确基线。

## 1. Hacker News (Show HN)

**标题:**
`Show HN: Heron – passive agent observability that reconstructs agent turns from traffic`

**正文:**
```text
We run Heron (https://github.com/Netis/heron) — a passive analyzer that reconstructs agent turns from network traffic — on our inference fleet.

When we looked at the production data, we found that ~73% of Claude Code's opus-model calls in our capture weren't actual coding work. They were a hidden "security monitor" sidecar: a /v1/messages POST with a system prompt "You are a security monitor for autonomous AI coding agents", feeding the running transcript and returning a <block>yes/no verdict.

Because it embeds the same transcript, the turn tracker initially merged it into the real turn and overwrote the answer with "<block>no" — masking real working sessions. v0.5.6 added filtering by system-prompt signature.

Heron itself is a Rust single-binary, Apache-2.0 licensed, passive analyzer. It captures LLM traffic off the wire and stitches multi-call interactions into agent turns. On Linux it can additionally read TLS-encrypted traffic on-host via experimental, opt-in eBPF SSL_read/SSL_write uprobes — with per-process attribution. Current release: v0.7.0.

https://github.com/Netis/heron
```

## 2. Twitter / X Thread

**Tweet 1:**
We just open-sourced the Wireshark for AI Agents. 🦩

Passive. Reconstructs agent turns from traffic. On Linux, experimental eBPF reads TLS-encrypted calls on-host and tells you which process made each one.

We found that ~73% of Claude Code's Opus turns in production are hidden security monitors. 🤯

Zero SDK. Zero proxy. Zero restarts. 🧵👇
[Attach hero GIF OR link https://youtu.be/yZzEBb-wK58]

**Tweet 2:**
Your agent was stuck in a 47-second loop. Your SDK noticed nothing.
But Heron saw every call on the wire — and reconstructed the whole turn so you could see the loop.

Heron passively captures traffic and assembles multi-call interactions into agent turns automatically.

**Tweet 3:**
You're throwing away thousands of real training samples every day.
Turn on Heron. One click to convert production traffic into SFT trajectories.

Open-source and free:
⭐ GitHub: https://github.com/Netis/heron
💻 Install: curl -fsSL https://raw.githubusercontent.com/Netis/heron/main/install.sh | INSTALL_DIR="$HOME/.local" sh
🚀 PH: [Link to your PH Launch]
```
