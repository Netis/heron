#!/usr/bin/env python3
"""
traffic-gen.py — LLM API traffic generator for Heron demos.

Continuously sends requests via Claude Code CLI and Codex CLI through a local
proxy so that Heron can capture plaintext HTTP traffic.

Usage:
    python3 scripts/traffic-gen.py
    python3 scripts/traffic-gen.py --interval 30 --providers claude
    python3 scripts/traffic-gen.py --max-requests 20 --dry-run
"""

import argparse
import json
import logging
import os
import random
import shutil
import subprocess
import sys
import tempfile
import time
import uuid

# ---------------------------------------------------------------------------
# Prompt pools
# ---------------------------------------------------------------------------

SIMPLE_PROMPTS = [
    # Code generation
    "Write a Python function that implements quicksort with type hints.",
    "Write a Rust function that reads a CSV file and returns a Vec of structs.",
    "Create a TypeScript utility that deeply merges two objects.",
    "Write a Go HTTP middleware that adds request ID headers.",
    "Implement a Python LRU cache from scratch without using functools.",
    # Code explanation
    "Explain what this regex does: ^(?:[a-z0-9]+\\.)*[a-z0-9]+$",
    "Explain the difference between tokio::spawn and std::thread::spawn in Rust.",
    "What are the trade-offs between REST and GraphQL for a dashboard API?",
    "Explain how TCP flow control works with sliding windows.",
    "What is the CAP theorem and how does it apply to distributed databases?",
    # Bug fixing
    "Fix the off-by-one error in this binary search: def search(arr, t):\\n  lo, hi = 0, len(arr)\\n  while lo < hi:\\n    mid = (lo + hi) // 2\\n    if arr[mid] < t: lo = mid\\n    else: hi = mid\\n  return lo",
    # Data & SQL
    "Write a SQL query to find the top 10 customers by total revenue in the last 30 days, including their order count.",
    "Write a Python script that connects to PostgreSQL and exports a table to Parquet using polars.",
    # DevOps
    "Write a Dockerfile for a multi-stage Rust build that produces a minimal runtime image.",
    "Write a GitHub Actions workflow that runs cargo test and cargo clippy on PRs.",
    # Short prompts (low token count)
    "What is a monad?",
    "Reverse a linked list in Python.",
    "Explain async/await in JavaScript.",
    # Long prompts (high token count)
    "Design a rate limiter for an API gateway that supports per-user and per-endpoint limits, sliding window counters, distributed state via Redis, and graceful degradation when Redis is unavailable. Include the data structures, algorithms, and a Python implementation.",
]

# Multi-turn scenarios: each is a list of sequential prompts in one session
MULTI_TURN_SCENARIOS = [
    {
        "name": "cli-tool-development",
        "prompts": [
            "Create a Python CLI tool using argparse that converts JSON to YAML and YAML to JSON. Support reading from stdin or a file path argument.",
            "Add error handling for malformed input and unsupported file extensions. Also add a --pretty flag for formatted output.",
            "Write pytest tests covering: valid JSON to YAML, valid YAML to JSON, malformed input, and the --pretty flag.",
        ],
    },
    {
        "name": "debug-and-fix",
        "prompts": [
            "This Python web scraper is silently returning empty results. Debug it:\n\nimport requests\nfrom bs4 import BeautifulSoup\n\ndef scrape(url):\n    r = requests.get(url)\n    soup = BeautifulSoup(r.text)\n    items = soup.find_all('div', class_='item')\n    return [i.text for i in items]",
            "Explain the root cause of the issue and any other potential problems with this code.",
            "Rewrite it with proper error handling, retry logic, and respect for robots.txt.",
        ],
    },
    {
        "name": "api-design-review",
        "prompts": [
            "Review this REST API design for a task management system:\n\nPOST /tasks - create task\nGET /tasks - list all tasks\nGET /tasks/:id - get task\nPUT /tasks/:id - update task\nDELETE /tasks/:id - delete task\nPOST /tasks/:id/assign - assign to user\nPOST /tasks/:id/complete - mark complete\n\nWhat improvements would you suggest?",
            "Now design the request/response schemas for each endpoint using TypeScript interfaces.",
            "Write an OpenAPI 3.0 spec for the improved API design.",
        ],
    },
    {
        "name": "refactor-legacy",
        "prompts": [
            "Refactor this function that has grown too complex:\n\ndef process_order(order, user, inventory, config):\n    if order['type'] == 'standard':\n        if user['tier'] == 'premium':\n            discount = 0.2\n        elif user['tier'] == 'gold':\n            discount = 0.1\n        else:\n            discount = 0\n        for item in order['items']:\n            if inventory.get(item['sku'], 0) < item['qty']:\n                return {'error': 'out of stock', 'sku': item['sku']}\n            inventory[item['sku']] -= item['qty']\n        total = sum(i['price'] * i['qty'] for i in order['items'])\n        total *= (1 - discount)\n        if config.get('tax_enabled'):\n            total *= 1.08\n        return {'status': 'ok', 'total': total}\n    elif order['type'] == 'subscription':\n        # ... more nested logic\n        pass",
            "Now add type hints and create dataclasses for Order, User, and the result types.",
        ],
    },
]

# ---------------------------------------------------------------------------
# CLI wrappers
# ---------------------------------------------------------------------------

log = logging.getLogger("traffic-gen")


def run_claude(
    prompt: str,
    session_id: str | None = None,
    resume: bool = False,
) -> dict:
    """Run claude -p and return {exit_code, duration_s}.

    - First turn of a session: pass session_id + resume=False (creates session)
    - Subsequent turns: pass session_id + resume=True (continues session)
    """
    cmd = ["claude", "-p", prompt, "--output-format", "json"]
    if session_id:
        if resume:
            cmd += ["--resume", session_id]
        else:
            cmd += ["--session-id", session_id]
    return _run_cli("claude", cmd)


def run_codex(prompt: str) -> dict:
    """Run codex exec and return {exit_code, duration_s}."""
    cmd = ["codex", "exec", "--skip-git-repo-check", prompt]
    return _run_cli("codex", cmd)


def _run_cli(provider: str, cmd: list[str]) -> dict:
    """Execute a CLI command, log output, return metadata."""
    log.info("[%s] Running: %s", provider, " ".join(cmd[:4]) + " ...")
    start = time.monotonic()
    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=300,  # 5 min timeout per request
            cwd=tempfile.gettempdir(),
            stdin=subprocess.DEVNULL,
        )
        duration = time.monotonic() - start
        log.info(
            "[%s] Completed in %.1fs (exit=%d, stdout=%d bytes)",
            provider,
            duration,
            result.returncode,
            len(result.stdout),
        )
        if result.returncode != 0 and result.stderr:
            log.warning("[%s] stderr: %s", provider, result.stderr[:200])
        return {"exit_code": result.returncode, "duration_s": round(duration, 1)}
    except subprocess.TimeoutExpired:
        duration = time.monotonic() - start
        log.warning("[%s] Timed out after %.1fs", provider, duration)
        return {"exit_code": -1, "duration_s": round(duration, 1)}
    except FileNotFoundError:
        log.error("[%s] CLI not found in PATH", provider)
        return {"exit_code": -1, "duration_s": 0}


# ---------------------------------------------------------------------------
# Traffic generation logic
# ---------------------------------------------------------------------------


def pick_action(
    providers: list[str], scenario_ratio: float
) -> tuple[str, str | list[dict]]:
    """Pick a provider and either a simple prompt or a multi-turn scenario.

    Returns (provider, prompt_or_scenario) where prompt_or_scenario is:
      - str for a simple prompt
      - list[dict] with keys {prompt} for a multi-turn scenario
    """
    provider = random.choice(providers)

    if random.random() < scenario_ratio and MULTI_TURN_SCENARIOS:
        scenario = random.choice(MULTI_TURN_SCENARIOS)
        log.info("Selected multi-turn scenario: %s", scenario["name"])
        return provider, scenario
    else:
        prompt = random.choice(SIMPLE_PROMPTS)
        return provider, prompt


def execute_simple(provider: str, prompt: str, dry_run: bool) -> int:
    """Execute a single simple prompt. Returns number of requests made (1)."""
    log.info("--- Simple prompt [%s] ---", provider)
    log.info("Prompt: %.80s...", prompt)
    if dry_run:
        log.info("[DRY RUN] Would run %s -p '%s'", provider, prompt[:60])
        return 1

    if provider == "claude":
        run_claude(prompt)
    elif provider == "codex":
        run_codex(prompt)
    return 1


def execute_scenario(
    provider: str, scenario: dict, dry_run: bool, turn_interval: float
) -> int:
    """Execute a multi-turn scenario. Returns number of requests made."""
    name = scenario["name"]
    prompts = scenario["prompts"]
    log.info("--- Multi-turn scenario [%s] '%s' (%d turns) ---", provider, name, len(prompts))

    if provider == "claude":
        session_id = str(uuid.uuid4())
        for i, prompt in enumerate(prompts):
            log.info("Turn %d/%d: %.80s...", i + 1, len(prompts), prompt)
            if dry_run:
                log.info("[DRY RUN] Would run claude -p (session=%s)", session_id[:8])
            else:
                run_claude(prompt, session_id=session_id, resume=(i > 0))
            if i < len(prompts) - 1:
                wait = turn_interval * (0.8 + random.random() * 0.4)
                log.info("Waiting %.1fs before next turn...", wait)
                time.sleep(wait)
    elif provider == "codex":
        # Codex exec doesn't support session continuity, run as separate prompts
        for i, prompt in enumerate(prompts):
            log.info("Turn %d/%d: %.80s...", i + 1, len(prompts), prompt)
            if dry_run:
                log.info("[DRY RUN] Would run codex exec")
            else:
                run_codex(prompt)
            if i < len(prompts) - 1:
                wait = turn_interval * (0.8 + random.random() * 0.4)
                log.info("Waiting %.1fs before next turn...", wait)
                time.sleep(wait)

    return len(prompts)


def main():
    parser = argparse.ArgumentParser(
        description="LLM API traffic generator for Heron demos"
    )
    parser.add_argument(
        "--interval",
        type=float,
        default=45,
        help="Seconds between requests (default: 45)",
    )
    parser.add_argument(
        "--turn-interval",
        type=float,
        default=10,
        help="Seconds between turns in multi-turn scenarios (default: 10)",
    )
    parser.add_argument(
        "--providers",
        type=str,
        default="claude,codex",
        help="Comma-separated list of providers to use (default: claude,codex)",
    )
    parser.add_argument(
        "--scenario-ratio",
        type=float,
        default=0.2,
        help="Fraction of requests that are multi-turn scenarios (default: 0.2)",
    )
    parser.add_argument(
        "--max-requests",
        type=int,
        default=0,
        help="Max requests to send, 0 for unlimited (default: 0)",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print commands without executing",
    )
    parser.add_argument(
        "--log-level",
        type=str,
        default="INFO",
        choices=["DEBUG", "INFO", "WARNING", "ERROR"],
        help="Logging level (default: INFO)",
    )

    args = parser.parse_args()

    logging.basicConfig(
        level=getattr(logging, args.log_level),
        format="%(asctime)s [%(levelname)s] %(message)s",
        datefmt="%H:%M:%S",
    )

    providers = [p.strip() for p in args.providers.split(",") if p.strip()]
    valid_providers = []
    for p in providers:
        if p == "claude" and shutil.which("claude"):
            valid_providers.append(p)
        elif p == "codex" and shutil.which("codex"):
            valid_providers.append(p)
        else:
            log.warning("Provider '%s' not found in PATH, skipping", p)

    if not valid_providers:
        log.error("No valid providers available. Install claude and/or codex CLI.")
        sys.exit(1)

    log.info("=== Heron Traffic Generator ===")
    log.info("Providers: %s", ", ".join(valid_providers))
    log.info("Interval: %.0fs (±20%% jitter)", args.interval)
    log.info("Scenario ratio: %.0f%%", args.scenario_ratio * 100)
    log.info("Max requests: %s", args.max_requests or "unlimited")
    log.info("Dry run: %s", args.dry_run)
    log.info("====================================")

    total_requests = 0
    try:
        while True:
            if args.max_requests > 0 and total_requests >= args.max_requests:
                log.info("Reached max requests (%d). Stopping.", args.max_requests)
                break

            provider, action = pick_action(valid_providers, args.scenario_ratio)

            if isinstance(action, dict):
                # Multi-turn scenario
                count = execute_scenario(
                    provider, action, args.dry_run, args.turn_interval
                )
            else:
                # Simple prompt
                count = execute_simple(provider, action, args.dry_run)

            total_requests += count
            log.info("Total requests so far: %d", total_requests)

            if args.max_requests > 0 and total_requests >= args.max_requests:
                continue  # will break at top of loop

            # Wait with jitter
            jitter = args.interval * (0.8 + random.random() * 0.4)
            log.info("Sleeping %.1fs until next request...", jitter)
            time.sleep(jitter)

    except KeyboardInterrupt:
        log.info("\nInterrupted. Total requests sent: %d", total_requests)

    log.info("Done.")


if __name__ == "__main__":
    main()
