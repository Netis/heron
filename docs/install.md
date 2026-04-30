# Installing TokenScope

TokenScope ships as a single statically linked binary with the web console
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
curl -fsSL https://raw.githubusercontent.com/Netis/TokenScope/main/install.sh | sudo sh

# User-local (binary in ~/.local/bin, config in ~/.config; no sudo):
curl -fsSL https://raw.githubusercontent.com/Netis/TokenScope/main/install.sh | INSTALL_DIR="$HOME/.local" sh
```

After install, grant capture privileges and run:

```bash
sudo setcap cap_net_raw,cap_net_admin=eip "$(which tokenscope)"
tokenscope -i eth0       # Linux; on macOS use -i en0
```

Open <http://localhost:3000> for the console.

The installer chooses paths from `INSTALL_DIR`:

| `INSTALL_DIR` | Binary | Config | Data |
|---|---|---|---|
| `/usr/local` (default with `sudo`) | `/usr/local/bin/tokenscope` | `/etc/tokenscope/config.toml` | `/var/lib/tokenscope/` |
| `$HOME/.local` (user install) | `~/.local/bin/tokenscope` | `~/.config/tokenscope/config.toml` | `~/.local/share/tokenscope/` |

Other supported environment variables: `TOKENSCOPE_VERSION` (pin a release),
`TOKENSCOPE_TARGET` (force a target triple), `NO_COLOR=1`.

## Manual install

If you would rather not pipe a script to your shell:

```bash
VERSION=v0.1.0
TARGET=x86_64-unknown-linux-musl
curl -fL "https://github.com/Netis/TokenScope/releases/download/${VERSION}/tokenscope-${VERSION}-${TARGET}.tar.gz" \
  | tar -xz
cd "tokenscope-${VERSION}-${TARGET}"
sudo setcap cap_net_raw,cap_net_admin=eip ./tokenscope
./tokenscope -i eth0
```

In this mode TokenScope reads `./config/default.toml` from the extracted
directory — `cd` into it, or pass `-c <path>` from elsewhere.

For other targets, swap `TARGET` to the value from the table above.

## Verify the download

Each release ships a `SHA256SUMS` file. Verify before running:

```bash
curl -fLO "https://github.com/Netis/TokenScope/releases/download/${VERSION}/SHA256SUMS"
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
sudo setcap cap_net_raw,cap_net_admin=eip ./tokenscope
./tokenscope -i eth0
```

Notes:
- Capabilities are stored in the file's extended attributes; copying or
  rebuilding the binary erases them. Re-run `setcap` after each upgrade.
- Some filesystems (NFS, certain overlay setups) do not preserve
  capabilities. Install onto a local ext4/xfs/btrfs filesystem.

### 2. systemd `AmbientCapabilities` — for production

Run as a dedicated user via systemd, with capabilities granted by the
service manager. No `setcap` needed. Example unit
(`/etc/systemd/system/tokenscope.service`):

```ini
[Unit]
Description=TokenScope LLM API observability
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
User=tokenscope
Group=tokenscope
WorkingDirectory=/opt/tokenscope
ExecStart=/opt/tokenscope/tokenscope -c /etc/tokenscope/config.toml
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
ReadWritePaths=/var/lib/tokenscope
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true

[Install]
WantedBy=multi-user.target
```

```bash
sudo useradd --system --no-create-home --shell /usr/sbin/nologin tokenscope
sudo mkdir -p /opt/tokenscope /etc/tokenscope /var/lib/tokenscope
sudo cp tokenscope /opt/tokenscope/
sudo cp -r config /etc/tokenscope/
sudo chown -R tokenscope:tokenscope /var/lib/tokenscope
sudo systemctl daemon-reload
sudo systemctl enable --now tokenscope
sudo journalctl -u tokenscope -f
```

### 3. `sudo` — quick test only

```bash
sudo ./tokenscope -i eth0
```

Works, but the data files (DuckDB, dumps) end up owned by root. Fine for
a one-off, not for a long-running install.

## macOS notes

macOS ships libpcap as part of the OS; the binary uses the system one.
Live capture requires either:

- Running as root: `sudo ./tokenscope -i en0`
- Or installing the [ChmodBPF helper](https://www.wireshark.org/docs/wsug_html_chunked/ChIntroPlatformSpecificMacos.html)
  bundled with Wireshark, which grants your user access to `/dev/bpf*`
  devices.

For pcap-file replay (`--pcap-file`), no special permissions are needed
on either platform.

## Reading from a pcap file (no privileges needed)

Useful for offline analysis, regression testing, or trying TokenScope
without touching a live interface:

```bash
./tokenscope --pcap-file capture.pcap
```

The pipeline runs at file-read speed (much faster than realtime) and
exits when the file is fully consumed.

## Verify the install

```bash
./tokenscope --version
./tokenscope --help
```

After starting with `-i <iface>` or `--pcap-file`, hit the health endpoint:

```bash
curl http://localhost:3000/api/health
```

## Uninstall

There are no system files to clean — TokenScope only writes to its
configured data directory (default `data/` next to the binary, or
`/var/lib/tokenscope` in the systemd example). Remove the binary, the
data directory, the config directory, and (if used) the systemd unit:

```bash
sudo systemctl disable --now tokenscope
sudo rm /etc/systemd/system/tokenscope.service
sudo rm -rf /opt/tokenscope /etc/tokenscope /var/lib/tokenscope
sudo userdel tokenscope
```

## Next steps

- [Configuration reference](configure.md) — pipelines, sources, storage,
  retention, API.
- [Architecture overview](design/01-architecture.md) — what the pipeline
  actually does with the captured packets.
