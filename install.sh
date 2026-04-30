#!/bin/sh
# TokenScope installer.
#
# Usage:
#   System install (binary in /usr/local/bin, config in /etc):
#     curl -fsSL https://raw.githubusercontent.com/Netis/TokenScope/main/install.sh | sudo sh
#
#   User install (binary in ~/.local/bin, config in ~/.config):
#     curl -fsSL https://raw.githubusercontent.com/Netis/TokenScope/main/install.sh | INSTALL_DIR="$HOME/.local" sh
#
# Environment overrides:
#   TOKENSCOPE_VERSION  Pin a specific version (default: latest GitHub release)
#   TOKENSCOPE_TARGET   Force a target triple (default: auto-detected)
#   INSTALL_DIR         Install prefix (default: /usr/local for system,
#                       set to "$HOME/.local" for user install)
#
# Layout decisions follow INSTALL_DIR, NOT the running UID. This avoids the
# `sudo` $HOME trap (where ~/.config would resolve to /root/.config). When
# INSTALL_DIR is /usr/local we treat the install as system-wide.
#
# This script intentionally does NOT:
#   - run setcap, sudo escalations, or chown beyond what install paths require
#   - install or enable a systemd unit
#   - overwrite an existing config file
#   - touch the user's data directory beyond creating it

set -eu

GITHUB_REPO="${TOKENSCOPE_REPO:-Netis/TokenScope}"
INSTALL_DIR="${INSTALL_DIR:-/usr/local}"
BIN_DIR="$INSTALL_DIR/bin"

# Decide config and data targets from INSTALL_DIR.
case "$INSTALL_DIR" in
    /usr/local|/usr|/opt/*)
        INSTALL_MODE="system"
        CONFIG_DIR="/etc/tokenscope"
        DATA_DIR="/var/lib/tokenscope"
        ;;
    *)
        INSTALL_MODE="user"
        CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/tokenscope"
        DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/tokenscope"
        ;;
esac
CONFIG_PATH="$CONFIG_DIR/config.toml"

# ---------------------------------------------------------------------------
# Output helpers
# ---------------------------------------------------------------------------
if [ -t 1 ] && [ -z "${NO_COLOR:-}" ]; then
    BOLD=$(printf '\033[1m')
    DIM=$(printf '\033[2m')
    RED=$(printf '\033[31m')
    GREEN=$(printf '\033[32m')
    YELLOW=$(printf '\033[33m')
    CYAN=$(printf '\033[36m')
    RESET=$(printf '\033[0m')
else
    BOLD=""; DIM=""; RED=""; GREEN=""; YELLOW=""; CYAN=""; RESET=""
fi

info()  { printf '%s==>%s %s\n'   "$CYAN"   "$RESET" "$*"; }
ok()    { printf '%s ok%s %s\n'   "$GREEN"  "$RESET" "$*"; }
warn()  { printf '%swarn%s %s\n'  "$YELLOW" "$RESET" "$*" >&2; }
fail()  { printf '%sfail%s %s\n'  "$RED"    "$RESET" "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Preflight
# ---------------------------------------------------------------------------
need() {
    command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}
need curl
need tar
need uname
need sed

if command -v sha256sum >/dev/null 2>&1; then
    SHA256_VERIFY="sha256sum --check --ignore-missing"
elif command -v shasum >/dev/null 2>&1; then
    SHA256_VERIFY="shasum -a 256 --check --ignore-missing"
else
    fail "no sha256 tool found (need sha256sum or shasum)"
fi

# ---------------------------------------------------------------------------
# Detect target triple
# ---------------------------------------------------------------------------
detect_target() {
    if [ -n "${TOKENSCOPE_TARGET:-}" ]; then
        printf '%s' "$TOKENSCOPE_TARGET"
        return
    fi

    _os=$(uname -s)
    _arch=$(uname -m)

    case "$_os" in
        Linux)  _os_part="unknown-linux-musl" ;;
        Darwin) _os_part="apple-darwin" ;;
        *) fail "unsupported OS: $_os (only Linux and macOS are supported)" ;;
    esac

    case "$_arch" in
        x86_64|amd64)   _arch_part="x86_64" ;;
        aarch64|arm64)  _arch_part="aarch64" ;;
        *) fail "unsupported architecture: $_arch (only x86_64 and aarch64/arm64 are supported)" ;;
    esac

    printf '%s-%s' "$_arch_part" "$_os_part"
}

# ---------------------------------------------------------------------------
# Resolve version (via the /releases/latest redirect, no API rate limit)
# ---------------------------------------------------------------------------
resolve_version() {
    if [ -n "${TOKENSCOPE_VERSION:-}" ]; then
        printf '%s' "$TOKENSCOPE_VERSION"
        return
    fi

    _location=$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
        "https://github.com/$GITHUB_REPO/releases/latest")
    _tag=$(printf '%s' "$_location" | sed -n 's|.*/tag/\(v[^/]*\)$|\1|p')

    [ -n "$_tag" ] || fail "could not determine latest version from $_location"
    printf '%s' "$_tag"
}

# ---------------------------------------------------------------------------
# Permission check
# ---------------------------------------------------------------------------
check_writable_dir() {
    # $1 = dir we want to be able to create or write into.
    # Walk up until we find an existing parent; that parent must be writable.
    _d="$1"
    while [ ! -e "$_d" ]; do
        _d=$(dirname "$_d")
        [ "$_d" = "/" ] && break
    done
    [ -w "$_d" ]
}

# ---------------------------------------------------------------------------
# Materialize config: copy the bundled default.toml to $CONFIG_PATH, but
# rewrite the storage path to the absolute $DATA_DIR so the running binary
# does not depend on its current working directory. No-op if a config
# already exists at the destination — never clobber user changes.
# ---------------------------------------------------------------------------
materialize_config() {
    _src_default="$1"   # path to default.toml inside the extracted tarball

    if [ -f "$CONFIG_PATH" ]; then
        info "Config already present at $CONFIG_PATH (not overwriting)"
        return 0
    fi

    mkdir -p "$CONFIG_DIR"
    # Anchor the duckdb path to $DATA_DIR. The shipped default uses
    # `path = "data/tokenscope.duckdb"` (relative, dev-friendly); rewrite
    # to an absolute path so install-mode startup works without cd-ing.
    _new_db_path="$DATA_DIR/tokenscope.duckdb"
    sed "s|^path = \"data/tokenscope.duckdb\"|path = \"$_new_db_path\"|" \
        "$_src_default" > "$CONFIG_PATH"
    ok "Config installed: $CONFIG_PATH (data at $DATA_DIR)"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
main() {
    TARGET=$(detect_target)
    VERSION=$(resolve_version)

    NAME="tokenscope-${VERSION}-${TARGET}"
    TARBALL="${NAME}.tar.gz"
    BASE_URL="https://github.com/$GITHUB_REPO/releases/download/$VERSION"

    info "TokenScope ${BOLD}${VERSION}${RESET} for ${BOLD}${TARGET}${RESET}"
    info "Mode:    ${BOLD}${INSTALL_MODE}${RESET}"
    info "Binary:  $BIN_DIR/tokenscope"
    info "Config:  $CONFIG_PATH"
    info "Data:    $DATA_DIR"

    if ! check_writable_dir "$BIN_DIR" || ! check_writable_dir "$CONFIG_DIR" || ! check_writable_dir "$DATA_DIR"; then
        cat >&2 <<EOF
${RED}fail${RESET} insufficient permissions for the chosen install paths.

If you intended a system install, run with sudo:
    curl -fsSL https://raw.githubusercontent.com/$GITHUB_REPO/main/install.sh | sudo sh

Or install entirely under your home directory:
    curl -fsSL https://raw.githubusercontent.com/$GITHUB_REPO/main/install.sh | INSTALL_DIR="\$HOME/.local" sh
EOF
        exit 1
    fi

    TMPDIR=$(mktemp -d 2>/dev/null || mktemp -d -t tokenscope)
    trap 'rm -rf "$TMPDIR"' EXIT INT TERM

    info "Downloading $TARBALL"
    curl -fL --progress-bar "$BASE_URL/$TARBALL" -o "$TMPDIR/$TARBALL"

    info "Downloading SHA256SUMS"
    curl -fsSL "$BASE_URL/SHA256SUMS" -o "$TMPDIR/SHA256SUMS"

    info "Verifying checksum"
    (cd "$TMPDIR" && $SHA256_VERIFY SHA256SUMS) >/dev/null || fail "checksum verification failed"
    ok "checksum matches"

    info "Extracting"
    tar -xzf "$TMPDIR/$TARBALL" -C "$TMPDIR"

    SRC="$TMPDIR/$NAME"
    [ -f "$SRC/tokenscope" ] || fail "extracted archive does not contain a tokenscope binary"
    [ -f "$SRC/config/default.toml" ] || fail "extracted archive missing config/default.toml"

    info "Installing binary to $BIN_DIR"
    mkdir -p "$BIN_DIR"
    cp "$SRC/tokenscope" "$BIN_DIR/.tokenscope.new"
    chmod 0755 "$BIN_DIR/.tokenscope.new"
    mv -f "$BIN_DIR/.tokenscope.new" "$BIN_DIR/tokenscope"
    ok "installed: $BIN_DIR/tokenscope"

    # Drop the default config (skips if user already has one) and pre-create
    # the data directory.
    materialize_config "$SRC/config/default.toml"
    mkdir -p "$DATA_DIR"

    # When invoked with `sudo`, root owns everything we just created — but
    # the user expects to run `tokenscope` later under their own UID
    # (typically with setcap, not sudo). Hand the data dir back so writes
    # succeed without further permission tweaks. The config file stays
    # root-owned in /etc/tokenscope/ so accidental edits require sudo,
    # matching system-config conventions.
    if [ "$INSTALL_MODE" = "system" ] && [ -n "${SUDO_USER:-}" ] && [ "${SUDO_USER}" != "root" ]; then
        if command -v chown >/dev/null 2>&1; then
            chown -R "$SUDO_USER" "$DATA_DIR" 2>/dev/null || \
                warn "could not chown $DATA_DIR to $SUDO_USER (run manually if needed)"
        fi
    fi

    # Best-effort sanity check.
    if "$BIN_DIR/tokenscope" --version >/dev/null 2>&1; then
        _ver=$("$BIN_DIR/tokenscope" --version 2>/dev/null || true)
        ok "$_ver"
    else
        warn "binary installed but '--version' failed; check that $BIN_DIR is in your PATH"
    fi

    print_next_steps
}

# ---------------------------------------------------------------------------
# Final guidance — show only what the user still has to do.
# ---------------------------------------------------------------------------
print_next_steps() {
    cat <<EOF

${BOLD}Next steps${RESET}

  ${DIM}# 1. Grant capture privileges (Linux). One-time:${RESET}
  ${CYAN}sudo setcap cap_net_raw,cap_net_admin=eip $BIN_DIR/tokenscope${RESET}
     ${DIM}— or run with sudo each time, or use the systemd recipe in docs/install.md${RESET}

  ${DIM}# 2. Run against a live interface (Linux: eth0; macOS: en0)${RESET}
  ${CYAN}tokenscope -i eth0${RESET}
     ${DIM}— config auto-discovered at $CONFIG_PATH${RESET}

  ${DIM}# 3. Open the console${RESET}
  ${CYAN}http://localhost:3000${RESET}

${BOLD}Customize${RESET}
  Edit ${CYAN}$CONFIG_PATH${RESET} to change BPF filters, retention, etc.

${BOLD}Documentation${RESET}
  Install:    https://github.com/$GITHUB_REPO/blob/main/docs/install.md
  Configure:  https://github.com/$GITHUB_REPO/blob/main/docs/configure.md
EOF
}

main "$@"
