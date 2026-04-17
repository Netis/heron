#!/usr/bin/env bash
# ==============================================================================
# Demo Server Bootstrap — run ON the demo server (10.40.7.104) as root
# Creates the 'ts' user, sets up SSH key auth, grants non-root capture.
#
# Usage:
#   ssh root@10.40.7.104   (password: rootroot, via Mac Mini jump host)
#   curl/paste this script, then: bash demo-server-setup.sh
# ==============================================================================
set -euo pipefail

GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m'

TS_USER="ts"
PUBKEY="ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIFtjiC3olMik26ZQRitCiOtUwrUtOcjBDJZfC6aHx1yV tokenscope-demo"

echo -e "${GREEN}=== TokenScope Demo Server Setup ===${NC}"

# 1. Create ts user
if id $TS_USER &>/dev/null; then
    echo -e "${YELLOW}User '$TS_USER' already exists — skipping creation${NC}"
else
    echo "Creating user '$TS_USER'..."
    useradd -m -s /bin/bash $TS_USER
    echo -e "${GREEN}User created${NC}"
fi

# 2. Setup SSH authorized_keys
echo "Setting up SSH key..."
mkdir -p /home/$TS_USER/.ssh
echo "$PUBKEY" > /home/$TS_USER/.ssh/authorized_keys
chown -R $TS_USER:$TS_USER /home/$TS_USER/.ssh
chmod 700 /home/$TS_USER/.ssh
chmod 600 /home/$TS_USER/.ssh/authorized_keys
echo -e "${GREEN}SSH key installed${NC}"

# 3. Grant non-root packet capture (Linux capabilities)
echo "Granting capture permissions..."
# Allow ts to use tcpdump/libpcap without root
if command -v setcap &>/dev/null; then
    # If tokenscope binary exists, grant cap_net_raw
    if [[ -f /root/tokenscope/bin/tokenscope ]]; then
        cp /root/tokenscope/bin/tokenscope /home/$TS_USER/tokenscope-cap 2>/dev/null || true
        setcap cap_net_raw,cap_net_admin=eip /home/$TS_USER/tokenscope-cap 2>/dev/null || true
        chown $TS_USER:$TS_USER /home/$TS_USER/tokenscope-cap 2>/dev/null || true
    fi
    echo -e "${GREEN}cap_net_raw granted${NC}"
else
    echo -e "${YELLOW}setcap not found — ts will need sudo for capture${NC}"
fi

# 4. Setup .bashrc with PATH (server has direct internet — no proxy needed)
echo "Setting up shell environment..."
cat > /home/$TS_USER/.bashrc << 'BASHRC'
# PATH
export PATH="$HOME/.local/bin:$HOME/bin:$HOME/.bun/bin:$HOME/.cargo/bin:/usr/local/bin:$PATH"

# Cargo
[[ -f "$HOME/.cargo/env" ]] && source "$HOME/.cargo/env"
BASHRC
chown $TS_USER:$TS_USER /home/$TS_USER/.bashrc
echo -e "${GREEN}Shell environment configured${NC}"

# 5. Ensure SSH is running
if systemctl is-active sshd &>/dev/null || systemctl is-active ssh &>/dev/null; then
    echo -e "${GREEN}SSH service is running${NC}"
else
    systemctl enable --now sshd 2>/dev/null || systemctl enable --now ssh 2>/dev/null || true
fi

echo ""
echo -e "${GREEN}=== Setup Complete ===${NC}"
echo ""
echo "Test from local machine (via jump host):"
echo "  ssh -J william@172.16.103.73 -i ~/.ssh/id_ed25519_tokenscope ts@10.40.7.104"
echo ""
echo "Next steps (run locally):"
echo "  1. just demo ping         # verify SSH"
echo "  2. just demo setup        # install just, bun, rust, claude, codex"
echo "  3. just demo clone        # clone TokenScope repo"
echo "  4. just demo build        # build binary"
echo "  5. just demo start        # start the service"
