#!/usr/bin/env python3
"""Replay the pcap corpus through two storage backends and diff the API.

A backend test that builds its own fixtures can only check the backend against
the author's idea of what the pipeline produces. This runs the actual pipeline
— capture, parse, LLM extraction, turn tracking, aggregation — into DuckDB and
then into aglake, and compares what the REST API says about the same packets.
That is the claim the pluggable-backend design makes, stated as something that
can fail.

It has already earned its keep: the first run found a 500 on the sessions page,
two aggregates reading the wrong column, a topology graph missing a node its
own edges pointed at, a millisecond truncation in DuckDB, and paginated lists
in both SQL backends that returned 26 rows as 18 distinct ones.

    AGLAKE_AGLAKED_BIN=/path/to/aglaked scripts/storage/backend-differential.py

Without `AGLAKE_AGLAKED_BIN` there is nothing to compare against and the run stops
after the DuckDB half. `HERON_BIN` defaults to the release build in-tree.

Two things are deliberately not compared:

* **Minted ids.** A span id is a UUIDv7 created when the pipeline first sees
  the record, so two runs over identical packets legitimately differ. Keys
  ending in `_id`/`_ids` are dropped before comparison.
* **Float tails.** The backends sum in different orders — one in SQL, one in
  SPL with the division in Rust — so equality is to a relative 1e-9. Asserting
  on the last ulp would only teach us to ignore the result.

And one divergence is accepted rather than ignored: services/topology may
disagree on `app` and `server_header`, because aglake classifies every span at
write time while the SQL backends sample a few bodies at read time. The check
for that is a real check — aglake may name an app where the others found
nothing, but it must never contradict them, and nothing else may move.
"""
import json, os, shutil, signal, subprocess, sys, tempfile, time, urllib.error, urllib.request

ROOT = os.environ.get(
    "HERON_ROOT",
    os.path.dirname(os.path.dirname(os.path.dirname(os.path.abspath(__file__)))),
)
HERON = os.environ.get("HERON_BIN", f"{ROOT}/server/target/release/heron")
# aglaked is not part of this repo; point at a build of it. Without this the
# aglake half is skipped and the run only checks that DuckDB is self-consistent.
# `SGLOGD_BIN` is the pre-rename name, still read so an existing shell profile
# or note keeps working.
AGLAKED = os.environ.get("AGLAKE_AGLAKED_BIN") or os.environ.get("SGLOGD_BIN", "")
# Vendored Splunk frontend assets. No longer needed for the management API —
# the native admin face is always mounted as of aglake 0.3 — so this is only
# here for a daemon being run with the compatibility UI. Harmless to leave unset.
ASSETS = os.environ.get("AGLAKE_SPLUNK_WEB_DIR") or os.environ.get("SGLOGD_WEB_DIR", "")
CORPUS = os.environ.get("HERON_CORPUS", f"{ROOT}/testdata/pcaps/corpus")
WORK = os.environ.get("HERON_WORK", tempfile.mkdtemp(prefix="heron-differential-"))

PAGING = {}

API_PORT = 18080
SG_PORT = 15980


def wait_http(url, timeout=90, headers=None):
    end = time.time() + timeout
    while time.time() < end:
        try:
            req = urllib.request.Request(url, headers=headers or {})
            with urllib.request.urlopen(req, timeout=2) as r:
                if r.status < 500:
                    return True
        except urllib.error.HTTPError:
            return True
        except Exception:
            time.sleep(0.3)
    return False


def get(path):
    with urllib.request.urlopen(f"http://127.0.0.1:{API_PORT}{path}", timeout=180) as r:
        return json.loads(r.read())


# --- what the console actually asks for, one entry per page -----------------
# `start`/`end` bracket the corpus: the pcaps carry their original capture
# timestamps, so the window has to be wide enough to contain them all.
T0, T1 = 0, 4_102_444_800  # 1970 .. 2100

ENDPOINTS = [
    ("calls", f"/api/spans?start={T0}&end={T1}&page=1&page_size=200"),
    ("calls_filtered", f"/api/spans?start={T0}&end={T1}&page=1&page_size=200&wire_api=anthropic"),
    ("traces", f"/api/traces?start={T0}&end={T1}&page=1&page_size=200"),
    ("sessions", f"/api/agent-sessions?start={T0}&end={T1}&page_size=100"),
    ("http", f"/api/http-exchanges?start={T0}&end={T1}&page=1&page_size=200"),
    ("summary", f"/api/metrics/summary?start={T0}&end={T1}"),
    ("timeseries", f"/api/metrics/timeseries?start={T0}&end={T1}&granularity=1h&fields=call_count,error_count,ttft_avg,e2e_avg"),
    ("timeseries_grouped", f"/api/metrics/timeseries?start={T0}&end={T1}&granularity=1h&fields=call_count&group_by=model"),
    ("models", f"/api/metrics/models?start={T0}&end={T1}&sort_by=call_count&sort_order=DESC&limit=50"),
    ("finish_reasons", f"/api/metrics/finish-reasons?start={T0}&end={T1}&granularity=1h"),
    ("services", f"/api/services?start={T0}&end={T1}"),
    ("topology", f"/api/services/topology?start={T0}&end={T1}"),
    ("agent_summary", f"/api/traces/summary?start={T0}&end={T1}"),
    ("agent_activity", f"/api/traces/activity?start={T0}&end={T1}&bucket_seconds=3600"),
    ("filter_wire_apis", "/api/filters/wire-apis"),
    ("filter_models", "/api/filters/models"),
    ("filter_server_ips", "/api/filters/server-ips"),
    ("filter_finish_reasons", "/api/filters/finish-reasons"),
    ("filter_agent_kinds", f"/api/filters/agent-kinds?start={T0}&end={T1}"),
]

# Fields that legitimately differ run to run (minted ids, and anything derived
# from them). Dropped before comparison, everywhere they appear.
VOLATILE_SUFFIXES = ("_id", "_ids")
VOLATILE_EXACT = {"id"}
# Ids that are NOT minted per run and so must still be compared.
STABLE_IDS = {"request_id", "response_id"}


def is_volatile(key):
    if key in STABLE_IDS:
        return False
    return key in VOLATILE_EXACT or key.endswith(VOLATILE_SUFFIXES)


def scrub(node):
    """Drop minted ids recursively; order lists by a key that cannot itself differ.

    Sorting by the whole serialized row was the obvious choice and the wrong
    one: the rows being compared differ in their float tails, so the sort keys
    differ too, and the comparison ends up pairing row 1 against row 2 and
    calling a match a mismatch. Rounding floats out of the *sort key only*
    keeps the ordering stable while leaving the values themselves intact for
    the tolerance check.
    """
    if isinstance(node, dict):
        return {k: scrub(v) for k, v in sorted(node.items()) if not is_volatile(k)}
    if isinstance(node, list):
        return sorted((scrub(v) for v in node), key=sort_key)
    return node


def sort_key(node):
    def coarse(n):
        if isinstance(n, float):
            return round(n, 6)
        if isinstance(n, dict):
            return {k: coarse(v) for k, v in n.items()}
        if isinstance(n, list):
            return [coarse(v) for v in n]
        return n
    return json.dumps(coarse(node), sort_keys=True)


def snapshot():
    out = {}
    for name, path in ENDPOINTS:
        try:
            out[name] = scrub(get(path))
        except urllib.error.HTTPError as e:
            out[name] = {"__http_error__": e.code, "body": e.read().decode()[:400]}
        except Exception as e:
            out[name] = {"__error__": str(e)}
    return out


def write_config(path, backend, extra):
    pcaps = sorted(f for f in os.listdir(CORPUS) if f.endswith(".pcap"))
    src = "\n".join(
        f'[[pipeline.sources]]\ntype = "pcap-file"\npath = "{CORPUS}/{f}"\nrealtime = false\n'
        for f in pcaps
    )
    with open(path, "w") as fh:
        fh.write(f"""[[pipeline]]
name = "e2e"
dispatcher_count = 1
flow_shard_count = 2

{src}

[storage]
backend = "{backend}"

[storage.duckdb]
path = "{WORK}/{backend}/heron.duckdb"

[storage.retention]
enabled = false

[storage.sink]
batch_size = 100
flush_interval_ms = 100

[api]
listen = "127.0.0.1"
port = {API_PORT}

{extra}
""")
    return len(pcaps)


def run_backend(backend, extra=""):
    d = f"{WORK}/{backend}"
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d, exist_ok=True)
    cfg = f"{d}/config.toml"
    n = write_config(cfg, backend, extra)
    log = open(f"{d}/heron.log", "wb")
    p = subprocess.Popen([HERON, "--config", cfg, "--no-retention"],
                         stdout=log, stderr=log, cwd=d)
    if not wait_http(f"http://127.0.0.1:{API_PORT}/api/filters/models"):
        p.kill(); p.wait()
        print(open(f"{d}/heron.log").read()[-3000:])
        raise SystemExit(f"{backend}: API never came up")

    # Wait for the pipeline to drain: poll until the call count stops moving.
    stable, last = 0, -1
    for _ in range(300):
        try:
            n_calls = get(f"/api/spans?start={T0}&end={T1}&page=1&page_size=1")["data"]["total"]
        except Exception:
            n_calls = -1
        if n_calls == last and n_calls > 0:
            stable += 1
            if stable >= 5:
                break
        else:
            stable = 0
        last = n_calls
        time.sleep(1)
    print(f"  {backend}: {n} pcaps -> {last} spans")
    snap = snapshot()
    PAGING[backend] = paging_is_lossless(
        lambda page, size: get(
            f"/api/spans?start={T0}&end={T1}&page={page}&page_size={size}"
        )["data"]["items"],
        5, last)
    p.send_signal(signal.SIGTERM)
    try:
        p.wait(timeout=30)
    except subprocess.TimeoutExpired:
        p.kill(); p.wait()
    return snap, last


def start_aglaked():
    d = f"{WORK}/aglaked"
    shutil.rmtree(d, ignore_errors=True)
    os.makedirs(d, exist_ok=True)
    log = open(f"{d}/aglaked.log", "wb")
    p = subprocess.Popen(
        [AGLAKED, "--data-dir", d, "--listen", f"127.0.0.1:{SG_PORT}",
         "--hec-token", "heron-e2e", "--no-self-trace", "--max-hot-raw-mib", "2048"]
        + (["--splunk-web-dir", ASSETS] if ASSETS else []),
        stdout=log, stderr=log)
    if not wait_http(f"http://127.0.0.1:{SG_PORT}/api/v1/indexes"):
        p.kill(); raise SystemExit("aglaked never came up")
    return p


TOL = 1e-9


def approx(a, b):
    """Equality, but floats only have to agree to a relative 1e-9.

    The two backends compute the same averages in a different order — one sums
    in SQL, the other sums in SPL and divides in Rust — so the last ulp is not
    a contract either can keep, and asserting on it would only teach us to
    ignore the comparison.
    """
    if isinstance(a, float) or isinstance(b, float):
        try:
            fa, fb = float(a), float(b)
        except (TypeError, ValueError):
            return a == b
        return fa == fb or abs(fa - fb) <= TOL * max(abs(fa), abs(fb), 1.0)
    if isinstance(a, dict) and isinstance(b, dict):
        return a.keys() == b.keys() and all(approx(a[k], b[k]) for k in a)
    if isinstance(a, list) and isinstance(b, list):
        return len(a) == len(b) and all(approx(x, y) for x, y in zip(a, b))
    return a == b


# Divergences the design accepts, each with the check that proves the
# difference is only of the expected kind. Listing an endpoint here without a
# check would just be muting it.
def only_classification_differs(a, b):
    """Services/topology may disagree on `app` and `server_header`, nowhere else.

    The SQL backends sample a handful of bodies per endpoint at read time;
    aglake classifies every span at write time and takes the majority. Same
    classifier, more input — so aglake may name an app where the others found
    nothing, but it must never contradict them, and nothing else may move.
    """
    rows_a = a["data"].get("services") or a["data"].get("nodes") or []
    rows_b = b["data"].get("services") or b["data"].get("nodes") or []
    if len(rows_a) != len(rows_b):
        return False, f"row count {len(rows_a)} vs {len(rows_b)}"
    key = lambda r: (r.get("server_ip"), r.get("server_port"))
    for ra, rb in zip(sorted(rows_a, key=key), sorted(rows_b, key=key)):
        for k in set(ra) | set(rb):
            if approx(ra.get(k), rb.get(k)):
                continue
            if k not in ("app", "server_header"):
                return False, f"{key(ra)} differs on {k}: {ra.get(k)!r} vs {rb.get(k)!r}"
            if ra.get(k) is not None and ra.get(k) != rb.get(k):
                return False, (f"{key(ra)} {k}: aglake contradicts rather than "
                               f"extends: {ra.get(k)!r} vs {rb.get(k)!r}")
    return True, "app/server_header only"


EXPECTED = {
    "services": only_classification_differs,
    "topology": only_classification_differs,
}


def diff(a, b):
    hard, soft = [], []
    for n, _ in ENDPOINTS:
        if approx(a.get(n), b.get(n)):
            continue
        check = EXPECTED.get(n)
        if check is None:
            hard.append((n, "unexpected difference"))
            continue
        try:
            ok, why = check(a[n], b[n])
        except Exception as e:
            ok, why = False, f"check raised {e!r}"
        (soft if ok else hard).append((n, why))
    return hard, soft


def paging_is_lossless(get_all, page_size, total):
    """Every row appears exactly once across the pages, and nothing is lost.

    Page N's *contents* are not comparable across backends when rows share a
    sort key — 24 of these 26 spans carry the same request_time, so which five
    land on page 2 is a tie-break, and the backends break ties differently by
    design. What must hold either way is that walking the pages visits every
    row exactly once.
    """
    seen = []
    page = 1
    while len(seen) < total and page < 100:
        rows = get_all(page, page_size)
        if not rows:
            break
        seen.extend(rows)
        page += 1
    keys = [json.dumps(r, sort_keys=True) for r in seen]
    return len(keys) == total and len(set(keys)) == total, len(keys), len(set(keys))


def main():
    os.makedirs(WORK, exist_ok=True)
    print("== duckdb ==")
    duck, n_duck = run_backend("duckdb")

    if not AGLAKED:
        print("\nAGLAKE_AGLAKED_BIN unset — nothing to compare against. Point it "
              "at an aglaked build to run the differential.")
        return 0

    print("== aglake ==")
    sg = start_aglaked()
    try:
        aglake, n_sg = run_backend("aglake", extra=f"""[storage.aglake]
url = "http://127.0.0.1:{SG_PORT}"
hec_token = "heron-e2e"
index_prefix = "e2e"
""")
    finally:
        sg.send_signal(signal.SIGTERM)
        try:
            sg.wait(timeout=30)
        except subprocess.TimeoutExpired:
            sg.kill(); sg.wait()

    with open(f"{WORK}/duckdb.json", "w") as f:
        json.dump(duck, f, indent=1, sort_keys=True)
    with open(f"{WORK}/aglake.json", "w") as f:
        json.dump(aglake, f, indent=1, sort_keys=True)

    print(f"\nspans: duckdb={n_duck} aglake={n_sg}")
    if n_duck != n_sg:
        print("FAIL: the two runs did not even ingest the same number of spans")
        return 1

    hard, soft = diff(duck, aglake)
    soft_names = {n for n, _ in soft}
    hard_names = {n for n, _ in hard}
    print(f"\n{'endpoint':22s} {'result':>10s}")
    print("-" * 34)
    for name, _ in ENDPOINTS:
        verdict = "MISMATCH" if name in hard_names else (
            "expected" if name in soft_names else "ok")
        print(f"{name:22s} {verdict:>10s}")
    for name, why in soft:
        print(f"\n  {name}: accepted divergence — {why}")

    print(f"\npaging (page_size=5, {n_duck} spans):")
    ok_pages = True
    for label, snap in (("duckdb", duck), ("aglake", aglake)):
        got = PAGING.get(label)
        if got is None:
            continue
        good, n, uniq = got
        print(f"  {label:8s} visited {n} rows, {uniq} distinct "
              + ("ok" if good else "LOSSY"))
        ok_pages &= good

    if hard or not ok_pages:
        print(f"\n{len(hard)} unexpected difference(s); snapshots in {WORK}/")
        import difflib
        for name, why in hard[:4]:
            print(f"\n--- {name}: {why} ---")
            da = json.dumps(duck.get(name), indent=1, sort_keys=True).splitlines()
            db = json.dumps(aglake.get(name), indent=1, sort_keys=True).splitlines()
            for line in list(difflib.unified_diff(da, db, "duckdb", "aglake", n=1))[:50]:
                print(line.rstrip())
        return 1
    print("\nRESULT: both backends agree on every endpoint, apart from the "
          "app-classification divergence the design documents")
    return 0


if __name__ == "__main__":
    sys.exit(main())
