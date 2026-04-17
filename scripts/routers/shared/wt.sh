#!/usr/bin/env bash
set -euo pipefail

PROJECT_ROOT="$(git rev-parse --show-toplevel 2>/dev/null)"
if [ -z "$PROJECT_ROOT" ]; then
    SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
    PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
fi
cd "$PROJECT_ROOT"

show_help() {
    echo ""
    echo "Worktrees"
    echo "   just wt add <name>    Create worktree + branch"
    echo "   just wt list          Show all worktrees"
    echo "   just wt remove <name> Remove worktree after merge"
    echo "   just wt merge <name>  Cherry-pick commits to main"
    echo "   just wt dev <name> [--yolo] [--cleanup] [--model <id>]  Open claude in worktree (auto-creates if missing)"
}

cmd_dev() {
    local name=""
    local do_cleanup=false
    local forward=()
    while [ $# -gt 0 ]; do
        case "$1" in
            --yolo) forward+=("--dangerously-skip-permissions"); shift ;;
            --cleanup) do_cleanup=true; shift ;;
            --model)
                shift
                if [ -n "${1:-}" ] && [ "${1:-}" != "default" ]; then
                    forward+=("--model" "$1")
                fi
                shift ;;
            --) shift; while [ $# -gt 0 ]; do forward+=("$1"); shift; done ;;
            --*) forward+=("$1"); shift ;;
            *)
                if [ -n "$name" ]; then
                    echo "warning: multiple names given ('$name', '$1') — using '$1'" >&2
                fi
                name="$1"; shift ;;
        esac
    done

    if [ -z "$name" ]; then
        echo "Usage: just wt dev <name> [--yolo] [--cleanup] [--model <id>] [-- <claude-args>...]" >&2
        exit 1
    fi

    if [ "$do_cleanup" = true ] && [ -d ".worktrees/$name" ]; then
        echo "Removing existing worktree: $name"
        git worktree remove ".worktrees/$name"
    fi

    if [ ! -d ".worktrees/$name" ]; then
        echo "Worktree not found, creating: $name"
        cmd_add "$name"
    fi

    cd ".worktrees/$name" && exec claude "${forward[@]}"
}

cmd_add() {
    local name="${1:-}"
    if [ -z "$name" ]; then
        echo "Usage: just wt add <name>" >&2; exit 1
    fi
    local base_commit
    base_commit=$(git rev-parse HEAD)
    git worktree add ".worktrees/$name" -b "feature/$name"
    # Record the base commit so merge knows the correct cherry-pick range
    echo "$base_commit" > ".worktrees/$name/.wt_base"
    echo ""
    echo "Worktree created: .worktrees/$name"
    echo "Branch: feature/$name"
    echo "Base: $(git rev-parse --abbrev-ref HEAD) ($base_commit)"
    echo ""
    echo "To start working, open a NEW terminal:"
    echo "  cd $(pwd)/.worktrees/$name && claude"
}

cmd_list() {
    git worktree list
}

cmd_remove() {
    local name="${1:-}"
    if [ -z "$name" ]; then
        echo "Usage: just wt remove <name>" >&2; exit 1
    fi
    # .wt_base is a marker file created by cmd_add; it is always untracked and
    # would otherwise make `git worktree remove` refuse to delete the worktree.
    rm -f ".worktrees/$name/.wt_base"
    git worktree remove ".worktrees/$name"
    echo "Removed: .worktrees/$name"
}

cmd_merge() {
    local name="${1:-}"
    if [ -z "$name" ]; then
        local worktrees
        worktrees=$(git worktree list --porcelain | grep "^worktree " | grep -v "$(pwd)$" | wc -l | tr -d ' ')
        if [ "$worktrees" -ne 1 ]; then
            echo "Usage: just wt merge <name>  (multiple worktrees found, specify name)" >&2
            git worktree list
            exit 1
        fi
        name=$(git worktree list --porcelain | grep "^worktree " | grep -v "$(pwd)$" | awk '{print $2}' | xargs basename)
    fi
    # Determine the fork point.
    #
    # Use `git merge-base` against the destination branch (parent repo HEAD) as
    # the source of truth — it self-corrects under rebase/reset. Honor
    # `.wt_base` only if it is still an ancestor of worktree HEAD; otherwise
    # the worktree was rebased onto newer mainline and the recorded snapshot
    # would pull in upstream commits.
    local dest_branch
    dest_branch=$(git rev-parse --abbrev-ref HEAD)
    local merge_base
    merge_base=$(git -C ".worktrees/$name" merge-base HEAD "$dest_branch") || {
        echo "error: no common ancestor between .worktrees/$name and $dest_branch" >&2
        exit 1
    }

    local base_ref="$merge_base"
    if [ -f ".worktrees/$name/.wt_base" ]; then
        local recorded
        recorded=$(cat ".worktrees/$name/.wt_base")
        if git -C ".worktrees/$name" merge-base --is-ancestor "$recorded" HEAD 2>/dev/null; then
            # Recorded base is still on the branch. Pick whichever is newer
            # (closer to HEAD) — that is the true fork point.
            if git -C ".worktrees/$name" merge-base --is-ancestor "$recorded" "$merge_base" 2>/dev/null; then
                base_ref="$merge_base"
            else
                base_ref="$recorded"
            fi
        else
            echo "warning: .wt_base ($recorded) is no longer an ancestor of worktree HEAD — likely rebased." >&2
            echo "         Falling back to git merge-base against $dest_branch: $merge_base" >&2
        fi
    fi
    local commits
    commits=$(git -C ".worktrees/$name" log --oneline "$base_ref..HEAD")
    if [ -z "$commits" ]; then
        echo "No commits to merge in .worktrees/$name"
        exit 0
    fi
    echo "Cherry-picking from $name → $dest_branch (active branch of main repo)"
    echo ""
    echo "Commits:"
    echo "$commits"
    echo ""
    git -C ".worktrees/$name" log --format="%H" "$base_ref..HEAD" | tac | while read -r sha; do
        git cherry-pick "$sha"
    done
    echo ""
    echo "Merge complete. Review commits, then: just wt remove $name"
}

ACTION="${1:-help}"
shift 2>/dev/null || true

case "$ACTION" in
    add|create|new) cmd_add "$@" ;;
    list|ls|status) cmd_list ;;
    remove|rm|delete|cleanup) cmd_remove "$@" ;;
    merge|integrate) cmd_merge "$@" ;;
    dev|claude|open) cmd_dev "$@" ;;
    help|--help|-h) show_help ;;
    *) echo "Unknown: $ACTION. Run 'just wt' for help."; exit 1 ;;
esac
