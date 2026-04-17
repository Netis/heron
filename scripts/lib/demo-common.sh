#!/usr/bin/env bash
# ==============================================================================
# Shared helpers for demo-* scripts (SSH, SCP, env loading)
# ==============================================================================
# Source this from demo.sh

RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
CYAN='\033[0;36m'
NC='\033[0m'

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[1]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../../.." && pwd)"
cd "$PROJECT_ROOT"

# Load .env.local
if [[ -f "$PROJECT_ROOT/.env.local" ]]; then
    set -a
    source "$PROJECT_ROOT/.env.local"
    set +a
fi

DEMO_HOST="${DEMO_HOST:-10.40.7.104}"
DEMO_USER="${DEMO_USER:-ts}"
DEMO_SSH_KEY="${DEMO_SSH_KEY:-$HOME/.ssh/id_ed25519_tokenscope}"
DEMO_URL="${DEMO_URL:-http://10.40.7.104:3000/}"
DEMO_JUMP_HOST="${DEMO_JUMP_HOST:-}"
DEMO_JUMP_USER="${DEMO_JUMP_USER:-william}"
DEMO_JUMP_KEY="${DEMO_JUMP_KEY:-$HOME/.ssh/id_ed25519_macbook_m5}"
DEMO_REMOTE_DIR="/home/${DEMO_USER}/TokenScope"

# ---------------------------------------------------------------------------
# SSH helpers
# ---------------------------------------------------------------------------

_ssh_base() {
    SSH_ARGS=(-o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new)
    if [[ -n "$DEMO_JUMP_HOST" ]]; then
        SSH_ARGS+=(-o "ProxyCommand=ssh -o IdentitiesOnly=yes -i ${DEMO_JUMP_KEY} -W %h:%p ${DEMO_JUMP_USER}@${DEMO_JUMP_HOST}")
    fi
}

run_ssh() {
    _ssh_base
    ssh "${SSH_ARGS[@]}" -o IdentitiesOnly=yes -i "$DEMO_SSH_KEY" "${DEMO_USER}@${DEMO_HOST}" "$@"
}

run_ssh_root() {
    _ssh_base
    ssh "${SSH_ARGS[@]}" -o IdentitiesOnly=yes -i "$DEMO_SSH_KEY" "root@${DEMO_HOST}" "$@"
}

run_remote() {
    run_ssh "cd ${DEMO_REMOTE_DIR} || { echo 'Error: ${DEMO_REMOTE_DIR} not found — run: just demo deploy'; exit 1; } && $*"
}

run_scp_to() {
    local src="$1" dst="$2"
    _ssh_base
    scp "${SSH_ARGS[@]}" -o IdentitiesOnly=yes -i "$DEMO_SSH_KEY" \
        "$src" "${DEMO_USER}@${DEMO_HOST}:${dst}"
}

run_rsync_to() {
    local src="$1" dst="$2"
    local ssh_cmd="ssh -o ConnectTimeout=10 -o StrictHostKeyChecking=accept-new"
    if [[ -n "$DEMO_JUMP_HOST" ]]; then
        ssh_cmd+=" -o 'ProxyCommand=ssh -o IdentitiesOnly=yes -i ${DEMO_JUMP_KEY} -W %h:%p ${DEMO_JUMP_USER}@${DEMO_JUMP_HOST}'"
    fi
    ssh_cmd+=" -o IdentitiesOnly=yes -i ${DEMO_SSH_KEY}"
    rsync -az --delete -e "$ssh_cmd" "$src" "${DEMO_USER}@${DEMO_HOST}:${dst}"
}
