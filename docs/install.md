# Installing Heron

Heron ships as a single statically linked binary with the web console
embedded inside it. There is no installer, no package manager footprint,
and (on Linux) no runtime dependency on libpcap or any C++ runtime.

## Supported platforms

| OS | Architecture | Tarball suffix |
|---|---|---|
| Linux (any glibc / musl distro) | x86_64 | `x86_64-unknown-linux-musl` |
| Linux (any glibc / musl distro) | aarch64 (ARM64) | `aarch64-unknown-linux-musl` |
| macOS 13+ | Intel | `x86_64-apple-darwin` |
| macOS 13+ | Apple Silicon | `aarch64-apple-darwin` |

The Linux builds are statically linked against musl libc, so they run on
any modern Linux distribution — Ubuntu, Debian, RHEL/Rocky/Alma, Alpine,
Amazon Linux 2/2023, Arch — without installing libpcap or a matching glibc.

## Install with the one-line installer (recommended)

```bash
# System-wide (binary in /usr/local/bin, config in /etc):
curl -fsSL https://raw.githubusercontent.com/Netis/heron/main/install.sh | sudo sh

# User-local (binary in ~/.local/bin, config in ~/.config; no sudo):
curl -fsSL https://raw.githubusercontent.com/Netis/heron/main/install.sh | INSTALL_DIR="$HOME/.local" sh
```

After install, grant capture privileges and run:

```bash
sudo setcap cap_net_raw,cap_net_admin=eip "$(which heron)"
heron -i eth0       # Linux; on macOS use -i en0
```

Open <http://localhost:3000> for the console.

The installer chooses paths from `INSTALL_DIR`:

| `INSTALL_DIR` | Binary | Config | Data |
|---|---|---|---|
| `/usr/local` (default with `sudo`) | `/usr/local/bin/heron` | `/etc/heron/config.toml` | `/var/lib/heron/` |
| `$HOME/.local` (user install) | `~/.local/bin/heron` | `~/.config/heron/config.toml` | `~/.local/share/heron/` |

Other supported environment variables: `HERON_VERSION` (pin a release),
`HERON_TARGET` (force a target triple), `NO_COLOR=1`.

## Manual install

If you would rather not pipe a script to your shell:

```bash
VERSION=v0.1.0
TARGET=x86_64-unknown-linux-musl
curl -fL "https://github.com/Netis/heron/releases/download/${VERSION}/heron-${VERSION}-${TARGET}.tar.gz" \
  | tar -xz
cd "heron-${VERSION}-${TARGET}"
sudo setcap cap_net_raw,cap_net_admin=eip ./heron
./heron -i eth0
```

In this mode Heron reads `./config/default.toml` from the extracted
directory — `cd` into it, or pass `-c <path>` from elsewhere.

For other targets, swap `TARGET` to the value from the table above.

## Verify the download

Each release ships a `SHA256SUMS` file. Verify before running:

```bash
curl -fLO "https://github.com/Netis/heron/releases/download/${VERSION}/SHA256SUMS"
sha256sum -c SHA256SUMS --ignore-missing
```

## Permissions for live capture

Live packet capture requires the kernel to grant the process `CAP_NET_RAW`
(and `CAP_NET_ADMIN` for setting BPF filters and promiscuous mode). Three
deployment patterns, in order of preference:

### 1. File capabilities — recommended

Grant the binary itself the needed capabilities. The process then runs as
an unprivileged user:

```bash
sudo setcap cap_net_raw,cap_net_admin=eip ./heron
./heron -i eth0
```

Notes:
- Capabilities are stored in the file's extended attributes; copying or
  rebuilding the binary erases them. Re-run `setcap` after each upgrade.
- Some filesystems (NFS, certain overlay setups) do not preserve
  capabilities. Install onto a local ext4/xfs/btrfs filesystem.

### 2. systemd `AmbientCapabilities` — for production

Run as a dedicated user via systemd, with capabilities granted by the
service manager. No `setcap` needed. Example unit
(`/etc/systemd/system/heron.service`):

```ini
[Unit]
Description=Heron LLM API observability
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=heron
Group=heron
WorkingDirectory=/opt/heron
ExecStart=/opt/heron/heron -c /etc/heron/config.toml
Restart=on-failure
RestartSec=5

# Network capture privileges, no root.
AmbientCapabilities=CAP_NET_RAW CAP_NET_ADMIN
CapabilityBoundingSet=CAP_NET_RAW CAP_NET_ADMIN
NoNewPrivileges=true

# Hardening
PrivateTmp=true
ProtectSystem=strict
ProtectHome=true
ReadWritePaths=/var/lib/heron
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true

[Install]
WantedBy=multi-user.target
```

```bash
sudo useradd --system --no-create-home --shell /usr/sbin/nologin heron
sudo mkdir -p /opt/heron /etc/heron /var/lib/heron
sudo cp heron /opt/heron/
sudo cp -r config /etc/heron/
sudo chown -R heron:heron /var/lib/heron
sudo systemctl daemon-reload
sudo systemctl enable --now heron
sudo journalctl -u heron -f
```

### 3. `sudo` — quick test only

```bash
sudo ./heron -i eth0
```

Works, but the data files (DuckDB, dumps) end up owned by root. Fine for
a one-off, not for a long-running install.

## macOS notes

macOS ships libpcap as part of the OS; the binary uses the system one.
Live capture requires either:

- Running as root: `sudo ./heron -i en0`
- Or installing the [ChmodBPF helper](https://www.wireshark.org/docs/wsug_html_chunked/ChIntroPlatformSpecificMacos.html)
  bundled with Wireshark, which grants your user access to `/dev/bpf*`
  devices.

For pcap-file replay (`--pcap-file`), no special permissions are needed
on either platform.

## Reading from a pcap file (no privileges needed)

Useful for offline analysis, regression testing, or trying Heron
without touching a live interface:

```bash
./heron --pcap-file capture.pcap --no-retention
```

`--no-retention` disables the retention sweeper for this run. Without
it, a pcap whose packets carry timestamps older than the retention
window (default 7 days) is pruned by the next sweep — leaving the UI
empty shortly after import.

The pipeline runs at file-read speed (much faster than realtime). Once
the file is fully consumed the process **keeps the API/console up** so
you can inspect results in the UI — press Ctrl+C to exit. Pass
`--exit-after-drain` to restore the old "exit immediately when done"
behavior for batch/CI use.

## Verify the install

```bash
./heron --version
./heron --help
```

After starting with `-i <iface>` or `--pcap-file`, hit the health endpoint:

```bash
curl http://localhost:3000/api/health
```

## Uninstall

Heron writes to three places: the binary, a config directory, and
a data directory (DuckDB file plus optional pcap dumps). The installer
never touches anything else, so removing those three paths is a clean
uninstall.

### One-line installer — system mode (`sudo`)

```bash
sudo rm /usr/local/bin/heron
sudo rm -rf /etc/heron /var/lib/heron
```

### One-line installer — user mode (`INSTALL_DIR=$HOME/.local`)

```bash
rm ~/.local/bin/heron
rm -rf ~/.config/heron ~/.local/share/heron
```

### systemd deployment (from the example unit above)

```bash
sudo systemctl disable --now heron
sudo rm /etc/systemd/system/heron.service
sudo rm -rf /opt/heron /etc/heron /var/lib/heron
sudo userdel heron
```

> The DuckDB file holds all captured telemetry. Back it up first if you
> want to keep historical metrics across reinstalls.

## Spinning up a demo on a remote host

There is no built-in deploy tooling — a demo is just the binary running on
a box. If you drive an AI coding agent (Claude Code, etc.), hand it a
prompt like the one below and let it do the SSH/setup. Fill in your own
host and credentials; never commit them.

```text
Set up a Heron demo on the host I give you over SSH:

1. SSH in (I'll provide host + credentials separately — do not hard-code
   them in any file or commit them).
2. Install Heron with the one-line installer from docs/install.md
   (user-local install is fine).
3. Grant capture caps: sudo setcap cap_net_raw,cap_net_admin=eip on the
   binary.
4. Start it on the primary interface, console on port 3000, running under
   tmux so it survives the SSH session.
5. Generate some LLM traffic through it (point an OpenAI/Anthropic-style
   client at a local proxy the host can capture), then confirm
   /api/health is green and the console shows turns.

Report back the console URL. Keep it minimal — this is a throwaway demo.
```

That is the whole "demo" story: install, capture, look at the console.
Nothing about it needs to live in this repo.

## Next steps

- [Configuration reference](configure.md) — pipelines, sources, storage,
  retention, API.
- [Architecture overview](design/01-architecture.md) — what the pipeline
  actually does with the captured packets.
