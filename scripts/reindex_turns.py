#!/usr/bin/env python3
"""Re-derive agent_turns rows from llm_calls using the helper-shape
fallback session_id rule introduced in ts-llm.

Mirrors the Rust algorithm in agents/{generic,session_id}.rs:

    extract_session_id(call):
        let user_text   = first_user_text(req)?
        let sig_in_req  = first_assistant_sig_from_request(req)
        let sig_in_resp = if sig_in_req.is_none() { sig_from_resp } else { None }
        let system_text = first_system_text(req)

        if sig_in_req is None and sig_in_resp is Text and system_text is non-empty:
            -> "gen-" + synth_helper_session_id(system_text, request_time)
        let sig = sig_in_req or sig_in_resp                 # else None -> skip
        match sig:
            ToolId(id) -> canonicalize_tool_id(id)
            Text(t)    -> "gen-" + synth_text_hash(user_text, t)

Only the OpenAI Chat wire api is implemented here — `SELECT DISTINCT wire_api
FROM llm_calls` shows every row in this database is openai-chat. Add Anthropic
/ Responses extractors if that ever changes.

Usage:

    # Stop heron first (it holds an exclusive db lock):
    pkill -f 'target/release/heron' && sleep 2
    python3 reindex_turns.py /home/vader/.local/share/heron/data/heron.duckdb
"""

import json
import sys
from pathlib import Path
from datetime import timedelta

import duckdb


HELPER_BUCKET_US = 60_000_000
TOOL_ID_PREFIXES = ("call", "toolu", "fc", "chatcmpl")


def fnv1a64(data: bytes) -> int:
    h = 0xcbf29ce484222325
    for b in data:
        h ^= b
        h = (h * 0x100000001b3) & 0xFFFFFFFFFFFFFFFF
    return h


def synth_text_hash(user_text: str, assistant_text: str) -> str:
    data = user_text.encode("utf-8") + b"\n" + assistant_text.encode("utf-8")
    return f"{fnv1a64(data):016x}"


def synth_helper_session_id(system_text: str, request_time_us: int) -> str:
    bucket = request_time_us // HELPER_BUCKET_US
    payload = system_text.encode("utf-8") + bucket.to_bytes(8, "little", signed=True)
    return f"{fnv1a64(payload):016x}"


def canonicalize_tool_id(tid: str) -> str:
    for p in TOOL_ID_PREFIXES:
        if tid.startswith(p):
            after = tid[len(p):]
            if after and not after.startswith("_"):
                return f"{p}_{after}"
    return tid


def user_content_to_text(content) -> str | None:
    if isinstance(content, str):
        s = content.strip()
        return content if s else None
    if isinstance(content, list):
        parts = []
        for b in content:
            if not isinstance(b, dict):
                continue
            t = b.get("type")
            if t in ("text", "input_text"):
                txt = b.get("text")
                if isinstance(txt, str):
                    parts.append(txt)
        if not parts:
            return None
        return "\n".join(parts)
    return None


def first_system_text(req: dict) -> str | None:
    for m in req.get("messages") or []:
        if m.get("role") == "system":
            return user_content_to_text(m.get("content"))
    return None


def first_user_text(req: dict) -> str | None:
    for m in req.get("messages") or []:
        if m.get("role") == "user":
            t = user_content_to_text(m.get("content"))
            if t is not None:
                return t
    return None


def first_assistant_sig(messages: list) -> tuple[str, str] | None:
    """Return ('toolid', id) or ('text', txt). None if no assistant turn."""
    for m in messages or []:
        if m.get("role") != "assistant":
            continue
        tcs = m.get("tool_calls")
        if isinstance(tcs, list) and tcs:
            tc = tcs[0]
            if isinstance(tc, dict):
                tid = tc.get("id")
                if isinstance(tid, str) and tid:
                    return ("toolid", tid)
        c = m.get("content")
        if isinstance(c, str) and c.strip():
            return ("text", c)
    return None


def first_assistant_sig_from_response(resp: dict) -> tuple[str, str] | None:
    choices = resp.get("choices") or []
    if not choices:
        return None
    msg = choices[0].get("message") or {}
    tcs = msg.get("tool_calls")
    if isinstance(tcs, list) and tcs:
        tc = tcs[0]
        if isinstance(tc, dict):
            tid = tc.get("id")
            if isinstance(tid, str) and tid:
                return ("toolid", tid)
    c = msg.get("content")
    if isinstance(c, str) and c.strip():
        return ("text", c)
    return None


def assistant_text_from_response(resp: dict) -> str | None:
    choices = resp.get("choices") or []
    if not choices:
        return None
    msg = choices[0].get("message") or {}
    c = msg.get("content")
    return c if isinstance(c, str) and c.strip() else None


def derive_session_id(req_str: str | None, resp_str: str | None, request_time_us: int) -> str | None:
    if not req_str:
        return None
    try:
        req = json.loads(req_str)
    except Exception:
        return None
    user_text = first_user_text(req)
    if user_text is None:
        return None
    sig_in_req = first_assistant_sig(req.get("messages") or [])
    sig_in_resp = None
    if sig_in_req is None and resp_str:
        try:
            resp = json.loads(resp_str)
            sig_in_resp = first_assistant_sig_from_response(resp)
        except Exception:
            pass
    sys_text = first_system_text(req)

    # Helper-shape: no asst in req + text-only resp + has system → bucket hash.
    if sig_in_req is None and sig_in_resp and sig_in_resp[0] == "text" and sys_text:
        return f"gen-{synth_helper_session_id(sys_text, request_time_us)}"

    sig = sig_in_req or sig_in_resp
    if sig is None:
        return None
    kind, payload = sig
    if kind == "toolid":
        return canonicalize_tool_id(payload)
    return f"gen-{synth_text_hash(user_text, payload)}"


def main() -> None:
    if len(sys.argv) < 2:
        print(__doc__, file=sys.stderr)
        sys.exit(2)
    db_path = Path(sys.argv[1])
    if not db_path.exists():
        sys.exit(f"db not found: {db_path}")

    backup = db_path.with_suffix(db_path.suffix + ".pre_reindex_backup")
    if not backup.exists():
        print(f"backing up {db_path} -> {backup}")
        backup.write_bytes(db_path.read_bytes())
    else:
        print(f"backup already exists: {backup} (not overwriting)")

    con = duckdb.connect(str(db_path))

    print("loading llm_calls …")
    rows = con.execute(
        """
        SELECT id, source_id, wire_api, model, client_ip, server_ip,
               request_time, response_time, complete_time,
               input_tokens, output_tokens, finish_reason,
               cache_read_input_tokens, cache_creation_input_tokens,
               CAST(request_body AS VARCHAR), CAST(response_body AS VARCHAR)
        FROM llm_calls
        ORDER BY request_time
        """
    ).fetchall()
    print(f"  {len(rows)} calls")

    # Bucket calls into (session_id, source_id, wire_api, agent_kind, client_ip, server_ip).
    groups: dict[tuple, list] = {}
    skipped_no_session_id = 0
    helper_bucket_hits = 0
    for r in rows:
        (
            cid,
            source_id,
            wire_api,
            model,
            client_ip,
            server_ip,
            req_time,
            resp_time,
            complete_time,
            in_tok,
            out_tok,
            finish,
            cache_read,
            cache_create,
            req_body,
            resp_body,
        ) = r
        # request_time is a timestamp; convert to micros since epoch.
        req_time_us = int(req_time.timestamp() * 1_000_000) if req_time else 0
        sid = derive_session_id(req_body, resp_body, req_time_us)
        if sid is None:
            skipped_no_session_id += 1
            continue
        # Detect helper-bucket hits for stats.
        if sid.startswith("gen-") and req_body:
            try:
                req = json.loads(req_body)
                msgs = req.get("messages") or []
                has_asst = any(m.get("role") == "assistant" for m in msgs)
                if not has_asst and first_system_text(req):
                    helper_bucket_hits += 1
            except Exception:
                pass

        key = (sid, source_id or "", wire_api or "", "generic", client_ip or "", server_ip or "")
        groups.setdefault(key, []).append(
            {
                "id": cid,
                "model": model,
                "request_time": req_time,
                "response_time": resp_time,
                "complete_time": complete_time,
                "input_tokens": in_tok or 0,
                "output_tokens": out_tok or 0,
                "cache_read": cache_read or 0,
                "cache_create": cache_create or 0,
                "finish": finish,
                "req_body": req_body,
                "resp_body": resp_body,
            }
        )

    print(f"  groups: {len(groups)} (helper-bucket hits encountered: {helper_bucket_hits})")
    print(f"  skipped (no session_id derivable): {skipped_no_session_id}")

    # Build agent_turns rows.
    new_turns = []
    import uuid
    from datetime import datetime
    for key, calls in groups.items():
        sid, source_id, wire_api, agent_kind, cip, sip = key
        calls.sort(key=lambda c: c["request_time"])
        first = calls[0]
        last = calls[-1]
        models = sorted({c["model"] for c in calls if c["model"]})
        end_time = last["complete_time"] or last["response_time"] or last["request_time"]
        duration_ms = int(((end_time - first["request_time"]).total_seconds()) * 1000) if end_time and first["request_time"] else 0
        # user_input_preview = first user text from first call's request
        user_preview = ""
        try:
            ut = first_user_text(json.loads(first["req_body"] or "{}"))
            if ut:
                user_preview = ut[:512]
        except Exception:
            pass
        # final_answer_preview from last call's response
        final_preview = ""
        try:
            at = assistant_text_from_response(json.loads(last["resp_body"] or "{}"))
            if at:
                final_preview = at[:512]
        except Exception:
            pass
        new_turns.append(
            {
                "turn_id": str(uuid.uuid4()),
                "source_id": source_id,
                "session_id": sid,
                "wire_api": wire_api,
                "agent_kind": agent_kind,
                "client_ip": cip,
                "server_ip": sip,
                "start_time": first["request_time"],
                "end_time": end_time,
                "duration_ms": duration_ms,
                "call_count": len(calls),
                "models_used": json.dumps(models),
                "subagents_used": json.dumps([]),
                "total_input_tokens": sum(c["input_tokens"] for c in calls),
                "total_output_tokens": sum(c["output_tokens"] for c in calls),
                "total_cache_read_input_tokens": sum(c["cache_read"] for c in calls),
                "total_cache_creation_input_tokens": sum(c["cache_create"] for c in calls),
                "total_cost_usd": 0.0,
                "status": "complete",
                "final_finish_reason": last["finish"] or "",
                "user_input_preview": user_preview,
                "user_call_id": first["id"],
                "final_answer_preview": final_preview,
                "final_call_id": last["id"],
                "call_ids": json.dumps([c["id"] for c in calls]),
                "metadata": "{}",
            }
        )

    print(f"  new agent_turns: {len(new_turns)}")
    distrib = {}
    for t in new_turns:
        distrib.setdefault(t["call_count"], 0)
        distrib[t["call_count"]] += 1
    print("  call_count distribution:")
    for n in sorted(distrib.keys()):
        print(f"    {n} calls -> {distrib[n]} turns")

    print("rewriting agent_turns table …")
    con.execute("BEGIN")
    try:
        con.execute("DELETE FROM agent_turns")
        for t in new_turns:
            cols = list(t.keys())
            placeholders = ", ".join(["?"] * len(cols))
            con.execute(
                f"INSERT INTO agent_turns ({', '.join(cols)}) VALUES ({placeholders})",
                [t[c] for c in cols],
            )
        con.execute("COMMIT")
        con.execute("CHECKPOINT")
    except Exception:
        con.execute("ROLLBACK")
        raise

    n_after = con.execute("SELECT COUNT(*) FROM agent_turns").fetchone()[0]
    print(f"  agent_turns row count after rewrite: {n_after}")
    con.close()
    print("done.")


if __name__ == "__main__":
    main()
