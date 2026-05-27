#!/usr/bin/env bash
# ==============================================================================
# Demo — manage the demo server and the Heron service (port 3000)
# ==============================================================================
# The demo server runs:
#   - cliproxyapi (LLM proxy) on port 8317
#   - Heron (capture + API + embedded console) on port 3000
#   - traffic-gen.py (simulated LLM traffic)
#
# This router owns:
#   - SSH/bootstrap/setup for the server itself
#   - Build-on-server deploy: git pull → console bun build → cargo release with
#     --features console, launched in a named tmux session `demo`
#   - Legacy cross-compile path (cmd_build/cmd_upload) kept for dev convenience
#   - Service start/stop/restart, logs, traffic, env
set -euo pipefail

source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/../../../scripts/lib/demo-common.sh"

DEMO_BRANCH="${DEMO_BRANCH:-main}"
GIT_REMOTE_URL="${GIT_REMOTE_URL:-git@github.com:Netis/TokenScope.git}"
CROSS_TARGET="x86_64-unknown-linux-gnu"
BINARY_NAME="heron"
TMUX_SESSION="demo"
LOCAL_BINARY="server/target/${CROSS_TARGET}/release/${BINARY_NAME}"
LOCAL_TRAFFIC_GEN="scripts/traffic-gen.py"
# Legacy cross-compile upload layout
REMOTE_BIN_DIR="${DEMO_REMOTE_DIR}/bin"
REMOTE_SCRIPTS_DIR="${DEMO_REMOTE_DIR}/scripts"
# Build-on-server layout (repo cloned into DEMO_REMOTE_DIR)
# REMOTE_CONFIG points at demo.toml (server-side live config, gitignored,
# seeded from default.toml on first deploy — see _build_on_server).
REMOTE_BINARY="${DEMO_REMOTE_DIR}/server/target/release/${BINARY_NAME}"
REMOTE_CONFIG="${DEMO_REMOTE_DIR}/server/config/demo.toml"

show_help() {
    echo ""
    echo "  Demo — ${DEMO_USER}@${DEMO_HOST} (Heron on port 3000)"
    echo ""
    echo "  Connection:"
    echo "   just demo ping         Test SSH connectivity"
    echo "   just demo ssh          Interactive SSH session"
    echo "   just demo ssh root     SSH as root (same key as ${DEMO_USER})"
    echo ""
    echo "  Server:"
    echo "   just demo host         System info (CPU, disk, mem, uptime)"
    echo "   just demo ps           Heron + cliproxyapi + traffic processes"
    echo ""
    echo "  Setup (first-time):"
    echo "   just demo bootstrap    Copy & show bootstrap instructions"
    echo "   just demo setup        Install tools (just, bun, rust, node, tokei)"
    echo ""
    echo "  Root ops (SSH as root with ${DEMO_USER}'s key):"
    echo "   just demo grant-setcap  One-time: allow ${DEMO_USER} to setcap w/o password"
    echo "   just demo kill-root     Kill lingering ${BINARY_NAME} owned by another user"
    echo ""
    echo "  Build & Deploy (on-server — preferred):"
    echo "   just demo deploy       Pull ${DEMO_BRANCH} + build + restart (tmux) + open URL"
    echo "   just demo deploy --no-open    Deploy without opening the browser"
    echo "   just demo preflight    Verify build env + git repo on server"
    echo ""
    echo "  Legacy cross-compile (local build):"
    echo "   just demo build        Cross-compile for linux x86_64"
    echo "   just demo upload       Upload binary + config + traffic-gen.py"
    echo ""
    echo "  Service (tmux session: ${TMUX_SESSION}):"
    echo "   just demo start        Start Heron in tmux"
    echo "   just demo stop         Kill tmux session"
    echo "   just demo restart      Restart (stop + start)"
    echo "   just demo status       Show Heron tmux + process status"
    echo "   just demo log          Show last 100 lines of Heron log"
    echo "   just demo log -f       Follow the log (Ctrl-C to exit)"
    echo "   just demo config       Show deploy config"
    echo ""
    echo "  Traffic:"
    echo "   just demo traffic start|stop|status|log"
    echo ""
    echo "  Env (.env.local):"
    echo "   just demo env check|apply"
    echo ""
    if [[ -n "$DEMO_JUMP_HOST" ]]; then
        echo -e "  ${CYAN}Jump: ${DEMO_JUMP_USER}@${DEMO_JUMP_HOST}${NC}"
    fi
    echo -e "  ${CYAN}Branch: ${DEMO_BRANCH}  Target: ${CROSS_TARGET}${NC}"
    echo -e "  ${CYAN}URL: ${DEMO_URL}${NC}"
    echo ""
}

# ---------------------------------------------------------------------------
# Connection
# ---------------------------------------------------------------------------

cmd_ping() {
    echo -e "${BLUE}Pinging ${DEMO_USER}@${DEMO_HOST}...${NC}"
    if run_ssh "echo 'connected'; hostname; whoami; uname -sr" 2>&1; then
        echo -e "${GREEN}OK${NC}"
    else
        echo -e "${RED}Failed — have you run 'just demo bootstrap'?${NC}"
        exit 1
    fi
}

cmd_ssh() {
    local user="${1:-}"
    local login="${DEMO_USER}"
    [[ "$user" == "root" ]] && login="root"
    _ssh_base
    exec ssh "${SSH_ARGS[@]}" -o IdentitiesOnly=yes -i "$DEMO_SSH_KEY" "${login}@${DEMO_HOST}"
}

_open_url() {
    echo -e "${CYAN}Opening: ${DEMO_URL}${NC}"
    open "$DEMO_URL" 2>/dev/null || xdg-open "$DEMO_URL" 2>/dev/null || echo -e "${CYAN}Open: ${DEMO_URL}${NC}"
}

# ---------------------------------------------------------------------------
# Server
# ---------------------------------------------------------------------------

cmd_host() {
    run_ssh '
        load=$(cat /proc/loadavg 2>/dev/null | awk "{print \$1}" || echo "?")
        cores=$(nproc 2>/dev/null || echo "?")
        disk=$(df -h / 2>/dev/null | awk "NR==2 {for(i=1;i<=NF;i++) if(\$i ~ /%/) {print \$i; exit}}")
        mem=$(free -h 2>/dev/null | awk "/^Mem:/ {print \$3\"/\"\$2}" || echo "?")
        up_sec=$(cat /proc/uptime 2>/dev/null | awk "{print int(\$1)}" || echo "")
        if [ -n "$up_sec" ]; then
            days=$((up_sec / 86400))
            hours=$(( (up_sec % 86400) / 3600 ))
            if [ $days -gt 0 ]; then
                uptime_str="${days}d${hours}h"
            else
                mins=$(( (up_sec % 3600) / 60 ))
                uptime_str="${hours}h${mins}m"
            fi
        else
            uptime_str="?"
        fi
        echo "CPU ${load} (${cores} cores)  Disk ${disk}  Mem ${mem}  Up ${uptime_str}"
        echo "Host: $(hostname)"
    '
}

cmd_ps() {
    run_ssh '
        echo "=== Heron ==="
        pgrep -a heron 2>/dev/null || echo "(not running)"
        echo ""
        echo "=== cliproxyapi ==="
        pgrep -a cli-proxy-api 2>/dev/null || echo "(not running)"
        echo ""
        echo "=== traffic-gen ==="
        pgrep -af traffic-gen 2>/dev/null || echo "(not running)"
        echo ""
        echo "=== Ports ==="
        ss -tlnp 2>/dev/null | grep -E ":(3000|8317) " || echo "(no relevant ports)"
    '
}

# ---------------------------------------------------------------------------
# Setup
# ---------------------------------------------------------------------------

cmd_bootstrap() {
    echo -e "${BLUE}Bootstrap: create 'ts' user on demo server${NC}"
    echo ""
    echo "This requires interactive root access (password: rootroot)."
    echo ""

    if [[ -n "$DEMO_JUMP_HOST" ]]; then
        echo "Step 1: SSH to demo server as root (via jump host):"
        echo -e "  ${CYAN}ssh -J ${DEMO_JUMP_USER}@${DEMO_JUMP_HOST} root@${DEMO_HOST}${NC}"
    else
        echo "Step 1: SSH to demo server as root:"
        echo -e "  ${CYAN}ssh root@${DEMO_HOST}${NC}"
    fi
    echo "  Password: rootroot"
    echo ""
    echo "Step 2: Run the bootstrap script on the server:"
    echo "  Paste and run:"
    echo -e "  ${CYAN}cat > /tmp/setup.sh << 'SCRIPT'"
    cat "$PROJECT_ROOT/scripts/demo-server-setup.sh"
    echo -e "SCRIPT"
    echo -e "  bash /tmp/setup.sh${NC}"
    echo ""
    echo "Step 3: Verify from local:"
    echo -e "  ${CYAN}just demo ping${NC}"
}

cmd_grant_setcap() {
    echo -e "${BLUE}Installing passwordless setcap sudoers rule for '${DEMO_USER}'...${NC}"
    run_ssh_root "set -e; echo '${DEMO_USER} ALL=(root) NOPASSWD: /sbin/setcap' | tee /etc/sudoers.d/heron-setcap >/dev/null && chmod 440 /etc/sudoers.d/heron-setcap && echo 'Installed: /etc/sudoers.d/heron-setcap'"
    echo -e "${GREEN}Done. Re-run: just demo deploy${NC}"
}

cmd_kill_root() {
    echo -e "${BLUE}Killing lingering ${BINARY_NAME} as root on ${DEMO_HOST}...${NC}"
    run_ssh_root "
        if pgrep -x ${BINARY_NAME} >/dev/null 2>&1; then
            pgrep -x ${BINARY_NAME} | while read pid; do
                owner=\$(ps -p \$pid -o user= 2>/dev/null | xargs)
                echo \"killing pid=\$pid owner=\$owner\"
            done
            pkill -x ${BINARY_NAME} && echo 'killed' || echo 'pkill failed'
        else
            echo 'no ${BINARY_NAME} process running'
        fi
    "
}

cmd_setup() {
    echo -e "${BLUE}Installing tools on demo server...${NC}"
    run_ssh '
        echo "Checking tools..."

        if command -v just &>/dev/null; then
            echo "just: $(just --version)"
        else
            echo "Installing just..."
            curl -sSf https://just.systems/install.sh | bash -s -- --to ~/bin
            echo "just installed to ~/bin"
        fi

        if command -v node &>/dev/null; then
            echo "node: $(node --version)"
        else
            echo "Installing Node.js..."
            curl -fsSL https://deb.nodesource.com/setup_lts.x | sudo -E bash - 2>/dev/null
            sudo apt-get install -y nodejs 2>/dev/null || echo "Failed — install Node.js manually"
        fi

        if command -v bun &>/dev/null; then
            echo "bun: $(bun --version)"
        else
            echo "Installing bun..."
            curl -fsSL https://bun.sh/install | bash
            echo "bun installed"
        fi

        if command -v cargo &>/dev/null; then
            echo "cargo: $(cargo --version)"
        else
            echo "Installing Rust..."
            curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
            echo "Rust installed"
        fi

        if command -v claude &>/dev/null; then
            echo "claude: $(claude --version 2>/dev/null || echo installed)"
        else
            echo "Installing Claude Code..."
            npm install -g @anthropic-ai/claude-code 2>/dev/null || echo "Install Claude manually: npm i -g @anthropic-ai/claude-code"
        fi

        if command -v codex &>/dev/null; then
            echo "codex: installed"
        else
            echo "Installing Codex..."
            npm install -g @openai/codex 2>/dev/null || echo "Install Codex manually: npm i -g @openai/codex"
        fi

        if command -v tokei &>/dev/null; then
            echo "tokei: $(tokei --version)"
        else
            echo "Installing tokei..."
            cargo install tokei 2>/dev/null || echo "Install tokei manually: cargo install tokei"
        fi

        echo ""
        echo "Done."
    '
}

# ---------------------------------------------------------------------------
# Build & Deploy
# ---------------------------------------------------------------------------

cmd_config() {
    echo -e "${BLUE}Service config:${NC}"
    echo "  Branch (deploy):  ${DEMO_BRANCH}"
    echo "  Git remote:       ${GIT_REMOTE_URL}"
    echo "  Remote repo:      ${DEMO_REMOTE_DIR}"
    echo "  Remote binary:    ${REMOTE_BINARY}"
    echo "  Remote config:    ${REMOTE_CONFIG}"
    echo "  tmux session:     ${TMUX_SESSION}"
    echo "  Cross target:     ${CROSS_TARGET}  (legacy)"
    echo "  Local binary:     ${LOCAL_BINARY}  (legacy)"
    echo "  Server:           ${DEMO_USER}@${DEMO_HOST}:3000"
}

# ---------------------------------------------------------------------------
# On-server deploy (build-on-server path)
# ---------------------------------------------------------------------------

_preflight_server() {
    echo -e "${BLUE}Pre-flight: verifying server build env...${NC}"
    local check
    check=$(run_ssh "
        source ~/.bashrc 2>/dev/null || true
        missing=''
        for tool in git cargo bun tmux rustc; do
            command -v \$tool >/dev/null 2>&1 || missing=\"\$missing \$tool\"
        done
        echo \"MISSING:\$missing\"
        if [ -d '${DEMO_REMOTE_DIR}/.git' ]; then
            echo 'REPO:ok'
        else
            echo 'REPO:missing'
        fi
        # Detect any ${BINARY_NAME} owned by a user other than us — we won't be
        # able to kill or replace it, so the deploy will fail on restart.
        foreign=''
        for pid in \$(pgrep -x ${BINARY_NAME} 2>/dev/null); do
            owner=\$(ps -p \$pid -o user= 2>/dev/null | xargs)
            if [ -n \"\$owner\" ] && [ \"\$owner\" != \"\$(whoami)\" ]; then
                foreign=\"\$foreign \$pid(\$owner)\"
            fi
        done
        echo \"FOREIGN:\$foreign\"
    ")
    local missing
    missing=$(echo "$check" | grep '^MISSING:' | sed 's/^MISSING://' | xargs)
    if [[ -n "$missing" ]]; then
        echo -e "${RED}Missing tools on server:${NC} $missing"
        echo "Install with: just demo setup"
        exit 1
    fi
    if ! echo "$check" | grep -q '^REPO:ok'; then
        echo -e "${YELLOW}No git repo at ${DEMO_REMOTE_DIR} on server — cloning...${NC}"
        if ! run_ssh "
            set -e
            if [ -d '${DEMO_REMOTE_DIR}' ] && [ -n \"\$(ls -A '${DEMO_REMOTE_DIR}' 2>/dev/null)\" ]; then
                ts=\$(date +%Y%m%d-%H%M%S)
                mv '${DEMO_REMOTE_DIR}' '${DEMO_REMOTE_DIR}.legacy-\$ts'
                echo \"Moved existing dir to ${DEMO_REMOTE_DIR}.legacy-\$ts\"
            fi
            git clone ${GIT_REMOTE_URL} '${DEMO_REMOTE_DIR}'
        "; then
            echo -e "${RED}Clone failed. SSH in and clone manually:${NC}"
            echo "  just demo ssh"
            echo "  git clone ${GIT_REMOTE_URL} ${DEMO_REMOTE_DIR}"
            exit 1
        fi
    fi
    local foreign
    foreign=$(echo "$check" | grep '^FOREIGN:' | sed 's/^FOREIGN://' | xargs)
    if [[ -n "$foreign" ]]; then
        echo -e "${RED}A ${BINARY_NAME} process is running under another user:${NC} $foreign"
        echo "  ${DEMO_USER} cannot kill it, and it's probably holding port 3000."
        echo "  Kill it once as root, then re-run deploy:"
        echo -e "    ${CYAN}ssh root@${DEMO_HOST} \"pkill -x ${BINARY_NAME}\"${NC}"
        exit 1
    fi
    echo -e "${GREEN}Pre-flight OK${NC}"
}

cmd_preflight() {
    _preflight_server
}

_build_on_server() {
    echo -e "${BLUE}Checking out ${DEMO_BRANCH} on server + pulling latest...${NC}"
    run_ssh "
        set -e
        source ~/.bashrc 2>/dev/null || true
        cd '${DEMO_REMOTE_DIR}'
        if ! git diff --quiet || ! git diff --cached --quiet; then
            echo 'Stashing local changes on server...'
            git stash push -m 'demo: auto-stash before deploy'
        fi
        git fetch --all --prune
        current=\$(git branch --show-current)
        if [ \"\$current\" != '${DEMO_BRANCH}' ]; then
            echo \"Switching \$current -> ${DEMO_BRANCH}\"
            git checkout '${DEMO_BRANCH}'
        fi
        git pull --rebase origin '${DEMO_BRANCH}'
        echo \"HEAD: \$(git rev-parse --short HEAD) (\$(git log -1 --format=%s))\"
    "

    # Seed server-side demo.toml from default.toml on first deploy. demo.toml
    # is gitignored — it holds environment-specific overrides and is preserved
    # across subsequent deploys.
    echo -e "${BLUE}Ensuring server-side demo.toml exists (seed from default.toml if missing)...${NC}"
    run_ssh "
        cd '${DEMO_REMOTE_DIR}'
        if [ ! -f server/config/demo.toml ]; then
            cp server/config/default.toml server/config/demo.toml
            echo 'seeded server/config/demo.toml from default.toml — review before production use'
        else
            echo 'demo.toml present, leaving untouched'
        fi
    "

    echo -e "${BLUE}Building console on server (bun + vite)...${NC}"
    run_ssh "
        set -e
        source ~/.bashrc 2>/dev/null || true
        cd '${DEMO_REMOTE_DIR}/console'
        bun install
        bun run build
    "

    echo -e "${BLUE}Building server on server (cargo release, --features console)...${NC}"
    run_ssh "
        set -e
        source ~/.bashrc 2>/dev/null || true
        cd '${DEMO_REMOTE_DIR}/server'
        cargo build --release --features console
    "

    if ! run_ssh "[ -x '${REMOTE_BINARY}' ]"; then
        echo -e "${RED}Build failed — binary missing at ${REMOTE_BINARY}${NC}"
        exit 1
    fi
    local size
    size=$(run_ssh "ls -lh '${REMOTE_BINARY}' | awk '{print \$5}'")
    echo -e "${GREEN}Build complete: ${REMOTE_BINARY} (${size})${NC}"

    _apply_pcap_caps
}

# Grant cap_net_raw + cap_net_admin to the freshly built binary. pcap needs
# these to open a live capture as a non-root user. Capabilities live on the
# inode, so every new build wipes them — we must reapply each deploy.
#
# We try `sudo -n` first (no password). If that fails, we check whether the
# caps happen to already be set (e.g. user re-ran a build on an unchanged
# binary), and fall back to printing a one-time root instruction.
_apply_pcap_caps() {
    echo -e "${BLUE}Applying pcap capabilities to binary...${NC}"
    local result
    result=$(run_ssh "
        if sudo -n setcap cap_net_raw,cap_net_admin=eip '${REMOTE_BINARY}' 2>/dev/null; then
            echo 'CAPS:ok'
        elif getcap '${REMOTE_BINARY}' 2>/dev/null | grep -q cap_net_raw; then
            echo 'CAPS:already'
        else
            echo 'CAPS:denied'
        fi
    " || echo 'CAPS:error')
    case "$result" in
        *CAPS:ok*)
            echo -e "${GREEN}Capabilities set (via sudo -n)${NC}"
            ;;
        *CAPS:already*)
            echo -e "${GREEN}Capabilities already present${NC}"
            ;;
        *)
            echo -e "${YELLOW}Could not set pcap capabilities — heron will fail with 'Operation not permitted'.${NC}"
            echo "  Run this once on the server as root:"
            echo -e "    ${CYAN}ssh root@${DEMO_HOST} \"setcap cap_net_raw,cap_net_admin=eip ${REMOTE_BINARY}\"${NC}"
            echo "  Or enable passwordless setcap for ${DEMO_USER}:"
            echo -e "    ${CYAN}echo '${DEMO_USER} ALL=(root) NOPASSWD: /sbin/setcap' | ssh root@${DEMO_HOST} 'tee /etc/sudoers.d/heron-setcap'${NC}"
            ;;
    esac
}

_checkout_main_on_server() {
    if [[ "${DEMO_BRANCH}" == "main" ]]; then
        return 0
    fi
    echo -e "${BLUE}Switching server repo back to main...${NC}"
    run_ssh "
        cd '${DEMO_REMOTE_DIR}' && git checkout main 2>&1 || true
    "
}

cmd_build() {
    echo -e "${BLUE}Checking prerequisites...${NC}"
    if ! command -v cross &>/dev/null; then
        echo -e "${RED}cross not found. Install: cargo install cross${NC}"
        exit 1
    fi
    if ! docker info &>/dev/null 2>&1; then
        echo -e "${RED}Docker not running (required by cross)${NC}"
        exit 1
    fi

    local current_branch stashed=false
    current_branch=$(git branch --show-current)
    if [[ "$current_branch" != "$DEMO_BRANCH" ]]; then
        if ! git diff --quiet || ! git diff --cached --quiet; then
            echo -e "${YELLOW}Stashing local changes...${NC}"
            git stash push -m "demo: auto-stash before build"
            stashed=true
        fi
        echo -e "${YELLOW}Switching to branch: ${DEMO_BRANCH}${NC}"
        git checkout "$DEMO_BRANCH"
        git pull --rebase
    fi

    _restore_branch() {
        if [[ "$current_branch" != "$DEMO_BRANCH" ]]; then
            echo -e "${YELLOW}Switching back to: ${current_branch}${NC}"
            git checkout "$current_branch" 2>/dev/null || true
            if [[ "$stashed" == true ]]; then
                echo -e "${YELLOW}Restoring stashed changes...${NC}"
                git stash pop 2>/dev/null || true
            fi
        fi
    }
    trap _restore_branch EXIT

    echo -e "${BLUE}Building console (bun)...${NC}"
    (cd console && bun install && bun run build)

    echo -e "${BLUE}Cross-compiling for ${CROSS_TARGET}...${NC}"
    (cd server && cross build --release --features console --target "$CROSS_TARGET")

    trap - EXIT

    if [[ -f "$LOCAL_BINARY" ]]; then
        local size
        size=$(ls -lh "$LOCAL_BINARY" | awk '{print $5}')
        echo -e "${GREEN}Build complete: ${LOCAL_BINARY} (${size})${NC}"
    else
        _restore_branch
        echo -e "${RED}Build failed — binary not found${NC}"
        exit 1
    fi

    _restore_branch
}

cmd_upload() {
    if [[ ! -f "$LOCAL_BINARY" ]]; then
        echo -e "${RED}Binary not found: ${LOCAL_BINARY}${NC}"
        echo "Run: just demo build"
        exit 1
    fi

    echo -e "${CYAN}(legacy upload: config NOT pushed — server-side demo.toml is authoritative)${NC}"

    local size
    size=$(ls -lh "$LOCAL_BINARY" | awk '{print $5}')
    echo -e "${BLUE}Uploading binary (${size}) + traffic-gen...${NC}"

    run_ssh "mkdir -p ${REMOTE_BIN_DIR} ${REMOTE_SCRIPTS_DIR}"
    run_scp_to "$LOCAL_BINARY" "${REMOTE_BIN_DIR}/${BINARY_NAME}"
    run_ssh "chmod +x ${REMOTE_BIN_DIR}/${BINARY_NAME}"
    if [[ -f "$LOCAL_TRAFFIC_GEN" ]]; then
        run_scp_to "$LOCAL_TRAFFIC_GEN" "${REMOTE_SCRIPTS_DIR}/traffic-gen.py"
    fi

    echo -e "${GREEN}Uploaded: ${REMOTE_BIN_DIR}/${BINARY_NAME} (+traffic-gen)${NC}"
}

cmd_deploy() {
    local open_after=true
    for arg in "$@"; do
        case "$arg" in
            --no-open) open_after=false ;;
        esac
    done

    # Always try to restore server to main on exit, even on failure
    trap '_checkout_main_on_server 2>/dev/null || true' EXIT

    _preflight_server
    _build_on_server

    echo -e "${BLUE}Restarting service (tmux: ${TMUX_SESSION})...${NC}"
    cmd_restart

    trap - EXIT
    _checkout_main_on_server

    if $open_after; then
        _open_url
    fi
}

# ---------------------------------------------------------------------------
# Service lifecycle (tmux session: ${TMUX_SESSION})
# ---------------------------------------------------------------------------

cmd_start() {
    run_ssh "
        cd '${DEMO_REMOTE_DIR}'
        if tmux has-session -t ${TMUX_SESSION} 2>/dev/null; then
            echo 'Heron already running (tmux session: ${TMUX_SESSION})'
            exit 0
        fi
        if [ ! -x '${REMOTE_BINARY}' ]; then
            echo 'Binary not found: ${REMOTE_BINARY}'
            echo 'Run: just demo deploy'
            exit 1
        fi
        mkdir -p data
        # Redirect stdout/stderr to the log file directly (no '| tee'). Piping
        # through tee means heron's fd1/fd2 are a pipe whose reader (tee)
        # dies when tmux closes the PTY on kill-session. Subsequent tracing
        # writes can then block a tokio worker thread, which starves the
        # signal driver and makes SIGTERM appear to be ignored — the
        # classic 'kill -9 only' symptom just-demo-stop reproduces.
        tmux new-session -d -s ${TMUX_SESSION} '${REMOTE_BINARY} --config ${REMOTE_CONFIG} >> heron.log 2>&1' < /dev/null
        sleep 1
        if tmux has-session -t ${TMUX_SESSION} 2>/dev/null; then
            echo 'Heron started (tmux session: ${TMUX_SESSION})'
            echo '  logs:  just demo log'
        else
            echo 'Heron failed to start — see heron.log'
            tail -30 heron.log 2>/dev/null || true
            exit 1
        fi
    "
}

cmd_stop() {
    # Send SIGTERM directly to heron rather than relying on tmux's
    # SIGHUP propagating through the pane shell — that path is fragile when
    # anything sits between tmux and the binary (see cmd_start comment).
    #
    # Graceful shutdown budget in main.rs is ~14s worst case (capture 3s +
    # pipeline drain 5s + API 3s + retention 3s, each with force-exit on
    # timeout). Poll up to 20s before escalating to SIGKILL.
    #
    # Match by exact process name (-x), not -f: the remote shell running
    # this script has the pattern in its own argv and pkill -f would kill it
    # mid-SSH, aborting the connection with exit 255.
    run_ssh "
        running=0
        pgrep -x ${BINARY_NAME} >/dev/null 2>&1 && running=1
        if [ \$running -eq 1 ]; then
            pkill -TERM -x ${BINARY_NAME} 2>/dev/null || true
            for _ in \$(seq 1 40); do
                pgrep -x ${BINARY_NAME} >/dev/null 2>&1 || break
                sleep 0.5
            done
        fi
        if tmux has-session -t ${TMUX_SESSION} 2>/dev/null; then
            tmux kill-session -t ${TMUX_SESSION} >/dev/null 2>&1 || true
            echo 'tmux session killed: ${TMUX_SESSION}'
        fi
        if pgrep -x ${BINARY_NAME} >/dev/null 2>&1; then
            echo 'WARN: ${BINARY_NAME} did not exit within 20s after SIGTERM; sending SIGKILL'
            pkill -KILL -x ${BINARY_NAME} 2>/dev/null || true
            sleep 0.5
            if pgrep -x ${BINARY_NAME} >/dev/null 2>&1; then
                echo 'Still running after SIGKILL — likely owned by another user:'
                pgrep -x ${BINARY_NAME} | while read pid; do
                    owner=\$(ps -p \$pid -o user= 2>/dev/null | xargs)
                    echo \"  pid=\$pid owner=\$owner\"
                done
                echo 'Kill it as root:  just demo kill-root'
                exit 1
            fi
            echo 'Killed (SIGKILL).'
        elif [ \$running -eq 1 ]; then
            echo 'Heron stopped cleanly.'
        else
            echo 'Heron was not running'
        fi
    "
}

cmd_restart() {
    cmd_stop
    sleep 1
    cmd_start
}

cmd_status() {
    run_ssh "
        if tmux has-session -t ${TMUX_SESSION} 2>/dev/null; then
            echo -e 'Heron: \033[0;32mrunning\033[0m (tmux: ${TMUX_SESSION})'
            pids=\$(pgrep -x ${BINARY_NAME} 2>/dev/null | tr '\n' ',' | sed 's/,\$//')
            [ -n \"\$pids\" ] && ps -p \"\$pids\" -o pid,user,%cpu,%mem,etime,args --no-headers 2>/dev/null || true
        else
            echo -e 'Heron: \033[0;31mstopped\033[0m'
        fi
    "
}

cmd_log() {
    if [[ "${1:-}" == "-f" ]]; then
        _ssh_base
        echo -e "${CYAN}Tailing heron.log — Ctrl-C to exit${NC}"
        exec ssh "${SSH_ARGS[@]}" -t -o IdentitiesOnly=yes -i "$DEMO_SSH_KEY" \
            "${DEMO_USER}@${DEMO_HOST}" \
            "cd '${DEMO_REMOTE_DIR}' && tail -n 100 -F heron.log"
    else
        run_ssh "cd '${DEMO_REMOTE_DIR}' && tail -100 heron.log 2>/dev/null || echo 'No log file yet'"
    fi
}

# ---------------------------------------------------------------------------
# Traffic generator
# ---------------------------------------------------------------------------

cmd_traffic() {
    local subcmd="${1:-status}"
    case "$subcmd" in
        start)
            run_remote "
                if pgrep -af traffic-gen &>/dev/null; then
                    echo 'Traffic generator already running'
                else
                    nohup python3 scripts/traffic-gen.py --interval 30 --providers claude,codex > traffic-gen.log 2>&1 &
                    echo \"Traffic generator started (pid=\$!)\"
                fi
            "
            ;;
        stop)
            run_ssh "pkill -f traffic-gen.py 2>/dev/null && echo 'Stopped' || echo 'Was not running'"
            ;;
        status)
            run_ssh "pgrep -af traffic-gen 2>/dev/null || echo '(not running)'"
            ;;
        log)
            run_remote "tail -50 traffic-gen.log 2>/dev/null || echo 'No log'"
            ;;
        help|--help|-h)
            echo ""
            echo "   just demo traffic start    Start traffic generator"
            echo "   just demo traffic stop     Stop traffic generator"
            echo "   just demo traffic status   Check if running"
            echo "   just demo traffic log      Tail traffic generator log"
            echo ""
            ;;
        *)
            echo -e "${RED}Unknown: traffic $subcmd${NC}"
            exit 1
            ;;
    esac
}

# ---------------------------------------------------------------------------
# Environment file
# ---------------------------------------------------------------------------

cmd_env() {
    local subcmd="${1:-help}"
    local env_file="$PROJECT_ROOT/.env.local"

    case "$subcmd" in
        check)
            if [[ ! -f "$env_file" ]]; then
                echo -e "${RED}No local .env.local${NC}"
                exit 1
            fi
            local server_content
            server_content=$(run_remote "cat .env.local 2>/dev/null" || true)
            if [[ -z "$server_content" ]]; then
                echo -e "${YELLOW}No .env.local on demo server${NC}"
                return 0
            fi
            local local_keys server_keys
            local_keys=$(grep -E '^[A-Za-z_][A-Za-z0-9_]*=' "$env_file" | cut -d= -f1 | sort)
            server_keys=$(echo "$server_content" | grep -E '^[A-Za-z_][A-Za-z0-9_]*=' | cut -d= -f1 | sort)
            local only_server only_local
            only_server=$(comm -23 <(echo "$server_keys") <(echo "$local_keys") || true)
            only_local=$(comm -13 <(echo "$server_keys") <(echo "$local_keys") || true)
            echo "=== .env.local comparison ==="
            [[ -n "$only_server" ]] && echo "Only on server:" && echo "$only_server" | sed 's/^/  /' && echo ""
            [[ -n "$only_local" ]] && echo "Only on local:" && echo "$only_local" | sed 's/^/  /' && echo ""
            local diff_keys="" common
            common=$(comm -12 <(echo "$server_keys") <(echo "$local_keys") || true)
            while IFS= read -r key; do
                [[ -z "$key" ]] && continue
                local sv lv
                sv=$(echo "$server_content" | grep "^${key}=" | head -1 | cut -d= -f2-)
                lv=$(grep "^${key}=" "$env_file" | head -1 | cut -d= -f2-)
                [[ "$sv" != "$lv" ]] && diff_keys="${diff_keys}${key}\n"
            done <<< "$common"
            [[ -n "$diff_keys" ]] && echo "Different values:" && printf "$diff_keys" | sed 's/^/  /' && echo ""
            if [[ -z "$only_server" && -z "$only_local" && -z "$diff_keys" ]]; then
                echo -e "${GREEN}Files are identical${NC}"
            fi
            ;;
        apply)
            if [[ ! -f "$env_file" ]]; then
                echo -e "${RED}No local .env.local${NC}"
                exit 1
            fi
            echo -e "${BLUE}Uploading .env.local to demo server...${NC}"
            run_scp_to "$env_file" "${DEMO_REMOTE_DIR}/.env.local"
            run_remote "chmod 600 .env.local"
            echo -e "${GREEN}.env.local uploaded${NC}"
            ;;
        help|--help|-h)
            echo ""
            echo "   just demo env check    Compare .env.local (local vs server)"
            echo "   just demo env apply    Upload .env.local to server"
            echo ""
            ;;
        *)
            echo -e "${RED}Unknown: env $subcmd${NC}"
            exit 1
            ;;
    esac
}

# ---------------------------------------------------------------------------
# Main router
# ---------------------------------------------------------------------------

case "${1:-help}" in
    # Connection
    ping)         cmd_ping ;;
    ssh)          shift; cmd_ssh "$@" ;;
    # Server
    host)         cmd_host ;;
    ps)           cmd_ps ;;
    # Setup
    bootstrap)    cmd_bootstrap ;;
    setup)        cmd_setup ;;
    grant-setcap) cmd_grant_setcap ;;
    kill-root)    cmd_kill_root ;;
    # Build & Deploy
    preflight)    cmd_preflight ;;
    build)        cmd_build ;;
    upload)       cmd_upload ;;
    deploy)       shift; cmd_deploy "$@" ;;
    config)       cmd_config ;;
    # Service
    start)        cmd_start ;;
    stop)         cmd_stop ;;
    restart)      cmd_restart ;;
    status)       cmd_status ;;
    log)          shift; cmd_log "$@" ;;
    # Nested
    traffic)      shift; cmd_traffic "$@" ;;
    env)          shift; cmd_env "$@" ;;
    help|--help|-h|"")
        show_help ;;
    *)
        echo -e "${RED}Unknown command: $1${NC}"
        show_help
        exit 1
        ;;
esac
