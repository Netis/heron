#!/usr/bin/env python3
"""Unit test for run_triage.sh's comment composition — stdlib only, so CI can
run it without installing anything:

    python3 scripts/agent-bot/tests/test_triage.py

run_triage.sh's live flow needs `gh`, `claude`, and the network. The part worth
testing in isolation is `compose_comment_body` — which exact markdown gets
posted for each verdict. The script supports `TRIAGE_LIB_ONLY=1`: sourced that
way it loads the helpers and returns BEFORE the live flow, so we can drive the
pure function directly.

What's under test (the behavior the maintainer asked for):
  - the warm, agent-authored `reply` is posted verbatim, NOT a robotic
    "Triage: <verdict>" template, and with no gate-number checklist;
  - a non-`do` verdict still gets the reply, plus collapsed maintainer controls;
  - a `do`→`needs_info` safety downgrade drops the (now-misleading) `do` reply
    for an honest fallback;
  - the hidden breadcrumb marker matches the verdict.

Exit 0 = all pass, 1 = a failure.
"""
import os
import subprocess
import sys

HERE = os.path.dirname(os.path.abspath(__file__))
TRIAGE = os.path.join(os.path.dirname(HERE), "run_triage.sh")


def compose(reply, verdict, downgraded="0"):
    """Source run_triage.sh in lib-only mode and call compose_comment_body."""
    script = (
        f'TRIAGE_LIB_ONLY=1 source "{TRIAGE}"; '
        'compose_comment_body "$1" "$2" "$3"'
    )
    r = subprocess.run(
        ["bash", "-c", script, "bash", reply, verdict, downgraded],
        capture_output=True,
        text=True,
        timeout=30,
    )
    assert r.returncode == 0, f"compose exited {r.returncode}: {r.stderr}"
    return r.stdout


# ---- tests ----------------------------------------------------------------

def test_do_posts_reply_verbatim_no_robotic_header():
    reply = "Thanks for the great report! I reproduced it — the SSE parser drops the final chunk. I'm on it. 🙏"
    out = compose(reply, "do")
    assert reply in out, out
    # human voice, not the old template
    assert "Triage:" not in out, out
    assert "scope:" not in out, out
    assert "Gate" not in out and "gate" not in out, out
    # `do` is auto-queued → no maintainer-controls block
    assert "Maintainer controls" not in out, out
    assert "<!-- heron-triage:do -->" in out, out


def test_needs_info_keeps_reply_and_adds_collapsed_controls():
    reply = "Thanks for filing this! I dug in but couldn't reproduce it yet. Could you share the model + endpoint you hit?"
    out = compose(reply, "needs_info")
    assert reply in out, out
    assert "<details><summary>Maintainer controls</summary>" in out, out
    assert "`agent:try`" in out and "`agent:skip`" in out, out
    assert "<!-- heron-triage:needs_info -->" in out, out
    # still no robotic framing
    assert "Triage:" not in out, out


def test_skip_is_not_a_brushoff_reply_preserved():
    reply = "Love this idea! It spans a few crates + a migration, so it's better as a human-guided change. Happy to help scope it."
    out = compose(reply, "skip")
    assert reply in out, out
    assert "Maintainer controls" in out, out
    assert "<!-- heron-triage:skip -->" in out, out


def test_downgrade_drops_do_reply_for_honest_fallback():
    # The agent wrote a do-flavored reply, but a gate failed → downgraded.
    do_reply = "Reproduced it, it's queued and I'm on it now!"
    out = compose(do_reply, "needs_info", downgraded="1")
    assert do_reply not in out, "misleading do-reply must be dropped on downgrade\n" + out
    assert "acceptance criteria" in out, out
    assert "Maintainer controls" in out, out
    assert "<!-- heron-triage:needs_info -->" in out, out


def test_empty_reply_falls_back():
    out = compose("", "needs_info")
    assert "Thanks so much" in out, out
    assert "acceptance criteria" in out, out


def test_marker_tracks_verdict():
    for v in ("do", "needs_info", "skip"):
        out = compose("hi", v)
        assert f"<!-- heron-triage:{v} -->" in out, (v, out)


def main():
    tests = [v for k, v in sorted(globals().items()) if k.startswith("test_") and callable(v)]
    fails = 0
    for t in tests:
        try:
            t()
            print(f"ok   {t.__name__}")
        except AssertionError as e:
            fails += 1
            print(f"FAIL {t.__name__}: {e}")
        except Exception as e:  # noqa: BLE001
            fails += 1
            print(f"ERR  {t.__name__}: {type(e).__name__}: {e}")
    print(f"\n{len(tests) - fails}/{len(tests)} passed")
    sys.exit(1 if fails else 0)


if __name__ == "__main__":
    main()
