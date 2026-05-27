#!/bin/sh
# Heron installer.
#
# Usage:
#   System install (binary in /usr/local/bin, config in /etc):
#     curl -fsSL https://raw.githubusercontent.com/Netis/TokenScope/main/install.sh | sudo sh
#
#   User install (binary in ~/.local/bin, config in ~/.config):
#     curl -fsSL https://raw.githubusercontent.com/Netis/TokenScope/main/install.sh | INSTALL_DIR="$HOME/.local" sh
#
# Environment overrides:
#   HERON_VERSION  Pin a specific version (default: latest GitHub release)
#   HERON_TARGET   Force a target triple (default: auto-detected)
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

usage() {
    cat <<'EOF'
Heron installer.

Usage:
  curl -fsSL https://raw.githubusercontent.com/Netis/TokenScope/main/install.sh | sudo sh
  curl -fsSL https://raw.githubusercontent.com/Netis/TokenScope/main/install.sh | INSTALL_DIR="$HOME/.local" sh

Flags:
  -h, --help   Show this help and exit.
  --dry-run    Resolve target/version/paths and verify writability without
               downloading or installing. Prints one JSON object to stdout
               (status lines stay on stderr). Exits 0 when all install paths
               are writable, 1 otherwise. Intended for scripting/agent use.
               Note: paths containing control characters (newline, tab, etc.)
               produce undefined JSON output and are not supported.

Environment overrides:
  HERON_VERSION  Pin a specific version (default: latest GitHub release).
                      A leading "v" is added automatically if missing.
  HERON_TARGET   Force a target triple (default: auto-detected).
  HERON_REPO     Override the GitHub repo (default: Netis/TokenScope).
  INSTALL_DIR         Binary install prefix (default: /usr/local).
                      Known system prefixes (/usr/local, /usr, /opt/*) also
                      trigger a system-wide layout: config in /etc/heron,
                      data in /var/lib/heron.
                      Any other prefix only redirects the binary location;
                      config and data still go to XDG paths
                      ($XDG_CONFIG_HOME / $XDG_DATA_HOME, falling back to
                      ~/.config and ~/.local/share).
  NO_COLOR=1          Disable colored output.
EOF
}

# ---------------------------------------------------------------------------
# Argument parsing. Recognized flags only; reject anything else loudly rather
# than silently ignoring a typo'd flag mid-install.
# ---------------------------------------------------------------------------
if [ "$#" -gt 1 ]; then
    usage >&2
    printf '\nfail unexpected extra arguments: %s\n' "$*" >&2
    exit 1
fi
DRY_RUN=0
case "${1:-}" in
    "")        ;;
    -h|--help) usage; exit 0 ;;
    --dry-run) DRY_RUN=1 ;;
    *)         usage >&2; printf '\nfail unknown argument: %s\n' "$1" >&2; exit 1 ;;
esac

# ---------------------------------------------------------------------------
# Output helpers (defined before any logic so EUID guards / preflight checks
# can use fail()/warn() consistently).
# ---------------------------------------------------------------------------
if [ -t 2 ] && [ -z "${NO_COLOR:-}" ]; then
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

# All status output goes to stderr so stdout stays reserved for structured
# data (currently just the --dry-run JSON object).
info()  { printf '%s==>%s %s\n'   "$CYAN"   "$RESET" "$*" >&2; }
ok()    { printf '%s ok%s %s\n'   "$GREEN"  "$RESET" "$*" >&2; }
warn()  { printf '%swarn%s %s\n'  "$YELLOW" "$RESET" "$*" >&2; }
fail()  { printf '%sfail%s %s\n'  "$RED"    "$RESET" "$*" >&2; exit 1; }

# ---------------------------------------------------------------------------
# Preflight — every external command we rely on must exist.
# ---------------------------------------------------------------------------
need() {
    command -v "$1" >/dev/null 2>&1 || fail "required command not found: $1"
}
need curl
need tar
need uname
need sed
need id

# ---------------------------------------------------------------------------
# Resolve install layout from INSTALL_DIR.
# ---------------------------------------------------------------------------
GITHUB_REPO="${HERON_REPO:-Netis/TokenScope}"
INSTALL_DIR="${INSTALL_DIR:-/usr/local}"
BIN_DIR="$INSTALL_DIR/bin"

case "$INSTALL_DIR" in
    /usr/local|/usr|/opt/*)
        INSTALL_MODE="system"
        CONFIG_DIR="/etc/heron"
        DATA_DIR="/var/lib/heron"
        ;;
    *)
        INSTALL_MODE="user"
        CONFIG_DIR="${XDG_CONFIG_HOME:-$HOME/.config}/heron"
        DATA_DIR="${XDG_DATA_HOME:-$HOME/.local/share}/heron"
        ;;
esac
CONFIG_PATH="$CONFIG_DIR/config.toml"

# Guard against the `sudo` $HOME trap: running as root but with a non-system
# INSTALL_DIR resolves $HOME to /root, silently landing config/data there.
# Force the caller to pick an explicit layout.
if [ "$INSTALL_MODE" = "user" ] && [ "$(id -u)" = "0" ]; then
    fail "running as root with non-system INSTALL_DIR=$INSTALL_DIR
  - For a system install, use INSTALL_DIR=/usr/local (default), /usr, or /opt/<name>
  - For a user install, drop sudo and re-run"
fi

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
    if [ -n "${HERON_TARGET:-}" ]; then
        printf '%s' "$HERON_TARGET"
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
    if [ -n "${HERON_VERSION:-}" ]; then
        _tag="$HERON_VERSION"
    else
        _location=$(curl -fsSLI -o /dev/null -w '%{url_effective}' \
            "https://github.com/$GITHUB_REPO/releases/latest")
        _tag=$(printf '%s' "$_location" | sed -n 's|.*/tag/\(v[^/]*\)$|\1|p')
        [ -n "$_tag" ] || fail "could not determine latest version from $_location"
    fi

    # Release tags are always `vX.Y.Z`. Be forgiving if the user passes the
    # bare semver — auto-prepend the `v` so the download URL still resolves.
    case "$_tag" in
        v*) ;;
        *)  _tag="v$_tag" ;;
    esac
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
    # `path = "data/heron.duckdb"` (relative, dev-friendly); rewrite
    # to an absolute path so install-mode startup works without cd-ing.
    _new_db_path="$DATA_DIR/heron.duckdb"
    sed "s|^path = \"data/heron.duckdb\"|path = \"$_new_db_path\"|" \
        "$_src_default" > "$CONFIG_PATH"
    # Verify the rewrite landed. If default.toml's storage line ever drifts
    # (different quoting, comment, key alias), sed silently no-ops and we'd
    # ship a relative path that breaks at runtime. Fail loud here instead.
    grep -qF "path = \"$_new_db_path\"" "$CONFIG_PATH" || \
        fail "could not rewrite duckdb path in default config (pattern drift in default.toml?)"
    ok "Config installed: $CONFIG_PATH (data at $DATA_DIR)"
}

# ---------------------------------------------------------------------------
# Minimal JSON-string escape: backslash first (so the backslashes we just
# inserted don't get re-quoted), then double-quote. Control chars (incl.
# newlines) in filesystem paths are vanishingly rare and not handled.
# ---------------------------------------------------------------------------
json_escape() {
    printf '%s' "$1" | sed -e 's/\\/\\\\/g' -e 's/"/\\"/g'
}

# ---------------------------------------------------------------------------
# Emit one JSON object describing the resolved install plan to stdout.
# Reads globals set by main(): TARGET, VERSION, INSTALL_MODE, BIN_DIR,
# CONFIG_PATH, DATA_DIR, UNWRITABLE. All user-controlled string fields are
# routed through json_escape so the output stays valid even when env-driven
# paths contain " or \.
# ---------------------------------------------------------------------------
emit_dry_run_json() {
    _writable="true"
    _unwritable_arr=""
    if [ -n "$UNWRITABLE" ]; then
        _writable="false"
        _first=1
        for _name in $UNWRITABLE; do
            if [ "$_first" = "1" ]; then
                _unwritable_arr="\"$_name\""
                _first=0
            else
                _unwritable_arr="$_unwritable_arr,\"$_name\""
            fi
        done
    fi
    _target=$(json_escape "$TARGET")
    _version=$(json_escape "$VERSION")
    _bin=$(json_escape "$BIN_DIR")
    _cfg=$(json_escape "$CONFIG_PATH")
    _data=$(json_escape "$DATA_DIR")
    # INSTALL_MODE is one of the two literals "system"/"user"; no escape.
    printf '{"target":"%s","version":"%s","mode":"%s","bin_dir":"%s","config_path":"%s","data_dir":"%s","writable":%s,"unwritable":[%s]}\n' \
        "$_target" "$_version" "$INSTALL_MODE" \
        "$_bin" "$_cfg" "$_data" \
        "$_writable" "$_unwritable_arr"
}

# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------
main() {
    TARGET=$(detect_target)
    VERSION=$(resolve_version)

    NAME="heron-${VERSION}-${TARGET}"
    TARBALL="${NAME}.tar.gz"
    BASE_URL="https://github.com/$GITHUB_REPO/releases/download/$VERSION"

    info "Heron ${BOLD}${VERSION}${RESET} for ${BOLD}${TARGET}${RESET}"
    info "Mode:    ${BOLD}${INSTALL_MODE}${RESET}"
    info "Binary:  $BIN_DIR/heron"
    info "Config:  $CONFIG_PATH"
    info "Data:    $DATA_DIR"

    # Per-dir writability check. Tracked individually so --dry-run can report
    # which dir(s) failed; normal install path just needs the boolean.
    UNWRITABLE=""
    check_writable_dir "$BIN_DIR"    || UNWRITABLE="$UNWRITABLE bin_dir"
    check_writable_dir "$CONFIG_DIR" || UNWRITABLE="$UNWRITABLE config_dir"
    check_writable_dir "$DATA_DIR"   || UNWRITABLE="$UNWRITABLE data_dir"

    if [ "$DRY_RUN" = "1" ]; then
        emit_dry_run_json
        [ -z "$UNWRITABLE" ] && exit 0 || exit 1
    fi

    if [ -n "$UNWRITABLE" ]; then
        cat >&2 <<EOF
${RED}fail${RESET} insufficient permissions for the chosen install paths.

If you intended a system install, run with sudo:
    curl -fsSL https://raw.githubusercontent.com/$GITHUB_REPO/main/install.sh | sudo sh

Or install entirely under your home directory:
    curl -fsSL https://raw.githubusercontent.com/$GITHUB_REPO/main/install.sh | INSTALL_DIR="\$HOME/.local" sh
EOF
        exit 1
    fi

    TMPDIR=$(mktemp -d 2>/dev/null || mktemp -d -t heron)
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
    [ -f "$SRC/heron" ] || fail "extracted archive does not contain a heron binary"
    [ -f "$SRC/config/default.toml" ] || fail "extracted archive missing config/default.toml"

    info "Installing binary to $BIN_DIR"
    mkdir -p "$BIN_DIR"
    cp "$SRC/heron" "$BIN_DIR/.heron.new"
    chmod 0755 "$BIN_DIR/.heron.new"
    mv -f "$BIN_DIR/.heron.new" "$BIN_DIR/heron"
    ok "installed: $BIN_DIR/heron"

    # Drop the default config (skips if user already has one) and pre-create
    # the data directory.
    materialize_config "$SRC/config/default.toml"
    mkdir -p "$DATA_DIR"

    # When invoked with `sudo`, root owns everything we just created — but
    # the user expects to run `heron` later under their own UID
    # (typically with setcap, not sudo). Hand the data dir back so writes
    # succeed without further permission tweaks. The config file stays
    # root-owned in /etc/heron/ so accidental edits require sudo,
    # matching system-config conventions.
    if [ "$INSTALL_MODE" = "system" ] && [ -n "${SUDO_USER:-}" ] && [ "${SUDO_USER}" != "root" ]; then
        if command -v chown >/dev/null 2>&1; then
            chown -R "$SUDO_USER" "$DATA_DIR" 2>/dev/null || \
                warn "could not chown $DATA_DIR to $SUDO_USER (run manually if needed)"
        fi
    fi

    # Best-effort sanity check.
    if "$BIN_DIR/heron" --version >/dev/null 2>&1; then
        _ver=$("$BIN_DIR/heron" --version 2>/dev/null || true)
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
    printf '\n%sNext steps%s\n\n' "$BOLD" "$RESET"

    # Branch the privilege step on the target OS so macOS users don't see a
    # setcap line that doesn't exist on their system.
    case "$TARGET" in
        *-linux-*)
            cat <<EOF
  ${DIM}# 1. Grant capture privileges (one-time, no sudo at runtime):${RESET}
  ${CYAN}sudo setcap cap_net_raw,cap_net_admin=eip $BIN_DIR/heron${RESET}
     ${DIM}— or run with sudo each time, or use the systemd recipe in docs/install.md${RESET}

  ${DIM}# 2. Run against a live interface${RESET}
  ${CYAN}heron -i eth0${RESET}
EOF
            ;;
        *-apple-darwin)
            cat <<EOF
  ${DIM}# 1. Grant BPF access. Either run with sudo:${RESET}
  ${CYAN}sudo heron -i en0${RESET}
     ${DIM}— or install the ChmodBPF helper bundled with Wireshark for${RESET}
     ${DIM}  unprivileged access (see docs/install.md, "macOS notes").${RESET}

  ${DIM}# 2. Or replay a pcap file (no privileges needed)${RESET}
  ${CYAN}heron --pcap-file capture.pcap --no-retention${RESET}
EOF
            ;;
        *)
            printf '  %sSee docs/install.md for permission setup on this platform.%s\n' \
                "$DIM" "$RESET"
            ;;
    esac

    cat <<EOF
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
