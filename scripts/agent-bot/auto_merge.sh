#!/usr/bin/env bash
# Called from the tail of pr-review.yml AFTER vivi posts her review.
# Auto-merges iff:
#   - PR has label `auto-agent`
#   - PR is not draft (wiwi may have flipped it; or the linked issue
#     author was a team member and we promoted earlier — see below)
#   - vivi's latest review state == APPROVED
#   - the linked issue's author is in TEAM
set -euo pipefail

TEAM='vaderyang william timmy'

PR="${PR_NUMBER:?PR_NUMBER required}"

meta=$(gh pr view "$PR" --json isDraft,labels,body)
labels=$(echo "$meta" | jq -r '.labels[].name')
echo "$labels" | grep -qx auto-agent || { echo "not auto-agent PR; skip"; exit 0; }

# Latest review state.
state=$(gh pr view "$PR" --json reviews --jq '[.reviews[] | select(.author.login=="vivi" or (.body | contains("vivi")))] | last | .state // empty')
[ "$state" = "APPROVED" ] || { echo "vivi verdict=$state; skip"; exit 0; }

# Extract issue number from PR body `Closes #N`.
issue=$(echo "$meta" | jq -r '.body' | grep -oE 'Closes #[0-9]+' | head -1 | tr -dc 0-9)
[ -n "$issue" ] || { echo "no linked issue; skip"; exit 0; }

author=$(gh issue view "$issue" --json author --jq '.author.login')
for m in $TEAM; do
  if [ "$m" = "$author" ]; then
    echo "vivi APPROVED + author=$author ∈ TEAM → admin-merge"
    # Lift draft (if still draft) and merge.
    gh pr ready "$PR" >/dev/null 2>&1 || true
    gh pr merge "$PR" --admin --squash --delete-branch
    exit 0
  fi
done
echo "author=$author not in TEAM; leaving PR for human review"
