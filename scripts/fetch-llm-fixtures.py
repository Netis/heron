#!/usr/bin/env python3
"""Regenerate ts-llm test fixtures from the official OpenAI + Anthropic OpenAPI specs.

Usage:
    python3 scripts/fetch-llm-fixtures.py

Output: server/h-llm/tests/fixtures/{provider}/request|response-*.{json,sse,txt}

Sources (pinned):
- OpenAI:    https://raw.githubusercontent.com/openai/openai-openapi/2025-03-21/openapi.yaml
- Anthropic: URL from anthropic-sdk-python .stats.yml (Stainless-hosted)

Anthropic examples live on `components.schemas.*.example` (canonical request +
response bodies). OpenAI examples live on `paths.<p>.post.x-oaiMeta.examples[]`
as `{title, request.curl, response}` — the request body is embedded in the curl
one-liner's `-d '...'` argument, the response is a JSON or SSE string.

Anthropic's SSE event schemas have no inline examples; streaming fixtures are
left as a follow-up (capture from a real call, or hand-roll per docs).
"""
import json
import re
import sys
import urllib.request
from pathlib import Path

try:
    import yaml
except ImportError:
    sys.exit("pip install pyyaml")

REPO = Path(__file__).resolve().parent.parent
OUT = REPO / "server" / "ts-llm" / "tests" / "fixtures"

OPENAI_URL = "https://raw.githubusercontent.com/openai/openai-openapi/2025-03-21/openapi.yaml"
ANTHROPIC_STATS = "https://raw.githubusercontent.com/anthropics/anthropic-sdk-python/main/.stats.yml"


def fetch(url: str) -> str:
    with urllib.request.urlopen(url) as r:
        return r.read().decode()


def parse_spec(data: str):
    if data.lstrip().startswith("{"):
        return json.loads(data)
    return yaml.safe_load(data)


def slug(s: str) -> str:
    s = re.sub(r"[^a-z0-9]+", "-", s.lower().strip()).strip("-")
    return s or "unnamed"


def write_json(relpath: str, obj):
    p = OUT / relpath
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(json.dumps(obj, indent=2, ensure_ascii=False) + "\n")
    print(f"  {p.relative_to(REPO)}")


def write_text(relpath: str, text: str):
    p = OUT / relpath
    p.parent.mkdir(parents=True, exist_ok=True)
    p.write_text(text if text.endswith("\n") else text + "\n")
    print(f"  {p.relative_to(REPO)}")


def extract_curl_body(curl: str):
    m = re.search(r"-d\s+'(.*?)'\s*$", curl, re.DOTALL)
    if not m:
        return None
    try:
        return json.loads(m.group(1))
    except json.JSONDecodeError:
        return None


def classify_response(resp):
    if not isinstance(resp, str):
        return ("json", resp)
    stripped = resp.strip()
    if stripped.startswith(("{", "[")):
        try:
            return ("json", json.loads(stripped))
        except json.JSONDecodeError:
            pass
    if "\n" in stripped and (stripped.startswith("event:") or stripped.startswith("data:")):
        return ("sse", resp)
    return ("text", resp)


def harvest_openai(spec, path: str, out_dir: str):
    node = spec["paths"].get(path, {}).get("post", {})
    meta = node.get("x-oaiMeta") or {}
    for ex in meta.get("examples", []) or []:
        title = slug(ex.get("title", "unnamed"))
        curl = (ex.get("request") or {}).get("curl", "")
        body = extract_curl_body(curl) if curl else None
        if body is not None:
            write_json(f"{out_dir}/request-{title}.json", body)
        kind, payload = classify_response(ex.get("response"))
        suffix = {"json": "json", "sse": "sse", "text": "txt"}[kind]
        if kind == "json":
            write_json(f"{out_dir}/response-{title}.json", payload)
        else:
            write_text(f"{out_dir}/response-{title}.{suffix}", payload)


def harvest_anthropic(spec):
    schemas = spec["components"]["schemas"]
    for name, out in [("CreateMessageParams", "request-basic"), ("Message", "response-basic")]:
        s = schemas.get(name, {})
        ex = s.get("example") or (s.get("examples") or [None])[0]
        if ex is not None:
            write_json(f"anthropic/{out}.json", ex)


def main():
    if OUT.exists():
        for p in sorted(OUT.rglob("*"), reverse=True):
            if p.is_file():
                p.unlink()
            elif p.is_dir():
                p.rmdir()
    OUT.mkdir(parents=True, exist_ok=True)

    print("Anthropic:")
    stats = yaml.safe_load(fetch(ANTHROPIC_STATS))
    harvest_anthropic(parse_spec(fetch(stats["openapi_spec_url"])))

    print("OpenAI:")
    o = parse_spec(fetch(OPENAI_URL))
    harvest_openai(o, "/chat/completions", "openai-chat")
    harvest_openai(o, "/responses", "openai-responses")


if __name__ == "__main__":
    main()
