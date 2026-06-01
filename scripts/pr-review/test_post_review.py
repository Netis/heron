#!/usr/bin/env python3
"""Unit tests for post_review.py verdict parser."""

import os
import sys
from pathlib import Path

# Mock PR_NUMBER before importing the module
os.environ["PR_NUMBER"] = "999"

# Import the module under test
sys.path.insert(0, str(Path(__file__).parent))
import post_review


def test_section_nonempty_level3_heading():
    """Original format: ### Blocking with content."""
    body = """### Summary
APPROVE

### Blocking
- Test failure

### Verified
- All tests pass"""
    assert post_review.section_nonempty(body, "Blocking") is True
    assert post_review.section_nonempty(body, "Verified") is True


def test_section_nonempty_level2_heading():
    """New format: ## Summary with content."""
    body = """## Summary
APPROVE

## Verified
- All tests pass"""
    assert post_review.section_nonempty(body, "Verified") is True


def test_section_nonempty_missing_section():
    """Missing section returns False."""
    body = """### Summary
APPROVE"""
    assert post_review.section_nonempty(body, "Blocking") is False


def test_pick_event_explicit_approve_level3():
    """Explicit APPROVE in ### Summary."""
    body = """### Summary
**APPROVE** — ready to merge.

### Verified
- Tests pass"""
    assert post_review.pick_event(body) == "APPROVE"


def test_pick_event_explicit_approve_level2():
    """Explicit APPROVE in ## Summary (the bug case)."""
    body = """## Summary
**APPROVE** — ready to merge.

## Verified
- Tests pass"""
    assert post_review.pick_event(body) == "APPROVE"


def test_pick_event_explicit_approve_markdown_bold():
    """Explicit APPROVE in bold markdown."""
    body = """### Summary
**APPROVE**

### Verified
- All good"""
    assert post_review.pick_event(body) == "APPROVE"


def test_pick_event_request_changes_from_blocking():
    """REQUEST_CHANGES inferred from Blocking section."""
    body = """### Summary
Needs fixes.

### Blocking
- Test failures

### Verified"""
    assert post_review.pick_event(body) == "REQUEST_CHANGES"


def test_pick_event_request_changes_from_blocking_level2():
    """REQUEST_CHANGES inferred from ## Blocking section."""
    body = """## Summary
Needs fixes.

## Blocking
- Test failures"""
    assert post_review.pick_event(body) == "REQUEST_CHANGES"


def test_pick_event_comment_from_suggestions():
    """COMMENT inferred from Suggestions section."""
    body = """### Summary
Minor nits.

### Suggestions
- Consider renaming"""
    assert post_review.pick_event(body) == "COMMENT"


def test_pick_event_approve_default():
    """APPROVE when only Verified present, no explicit token."""
    body = """### Summary
Looks good.

### Verified
- All tests pass"""
    assert post_review.pick_event(body) == "APPROVE"


def test_pick_event_approve_verified_only_level2():
    """APPROVE with ## Verified only (no Summary token)."""
    body = """## Summary
Clean implementation.

## Verified
- Tests pass
- Lint clean"""
    assert post_review.pick_event(body) == "APPROVE"


def test_pick_event_no_headings():
    """No markdown headings → COMMENT."""
    body = "Just a plain text comment"
    assert post_review.pick_event(body) == "COMMENT"


def test_pick_event_error_prefix():
    """ERROR prefix → COMMENT."""
    body = "ERROR: something went wrong"
    assert post_review.pick_event(body) == "COMMENT"


def test_pick_event_explicit_overrides_sections():
    """Explicit APPROVE token overrides Blocking section."""
    body = """### Summary
**APPROVE**

### Blocking
- This would normally block"""
    assert post_review.pick_event(body) == "APPROVE"


def test_pick_event_request_changes_explicit():
    """Explicit REQUEST_CHANGES token."""
    body = """### Summary
**REQUEST_CHANGES**

### Verified
- Tests pass"""
    assert post_review.pick_event(body) == "REQUEST_CHANGES"


def test_pick_event_comment_explicit():
    """Explicit COMMENT token."""
    body = """### Summary
**COMMENT**

### Suggestions
- Minor nits"""
    assert post_review.pick_event(body) == "COMMENT"


if __name__ == "__main__":
    import traceback
    failed = 0
    for name, fn in list(globals().items()):
        if name.startswith("test_") and callable(fn):
            try:
                fn()
                print(f"✓ {name}")
            except AssertionError:
                failed += 1
                print(f"✗ {name}")
                traceback.print_exc()
    print(f"\n{failed} failed")
    sys.exit(0 if failed == 0 else 1)