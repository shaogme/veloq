#!/usr/bin/env bash
set -e

# ==========================================
# Veloq Development Container Entrypoint
# ==========================================

# 1. Load Nix Environment
# This sources the environment variables generated during build time.
if [ -f /etc/profile.d/nix-env.sh ]; then
    source /etc/profile.d/nix-env.sh
fi

# 2. Initialize SSH Host Keys
# Only generate if they don't exist (prevents regeneration on container restart if volume persisted)
if [ ! -f /etc/ssh/ssh_host_rsa_key ]; then
    ssh-keygen -f /etc/ssh/ssh_host_rsa_key -N '' -t rsa >/dev/null 2>&1
fi
if [ ! -f /etc/ssh/ssh_host_ed25519_key ]; then
    ssh-keygen -f /etc/ssh/ssh_host_ed25519_key -N '' -t ed25519 >/dev/null 2>&1
fi

# 3. Setup Root Password
# Uses environment variable $ROOT_PASSWORD or defaults to 'root'
if [ -n "$ROOT_PASSWORD" ]; then
    PASS_HASH=$(openssl passwd -6 "$ROOT_PASSWORD")
else
    PASS_HASH=$(openssl passwd -6 "root")
fi
sed -i "s|^root:[^:]*|root:$PASS_HASH|" /etc/shadow

# 4. Setup Authorized Keys
# Installs the host's public key for seamless SSH access
if [ -f "/tmp/id_ed25519.pub" ]; then
    mkdir -p /root/.ssh
    cp /tmp/id_ed25519.pub /root/.ssh/authorized_keys
    chmod 700 /root/.ssh
    chmod 600 /root/.ssh/authorized_keys
fi

# 5. Export Environment for SSH Sessions
# CRITICAL: Captures current environment (Nix paths, Rust vars) and saves it to ~/.ssh/environment.
# This allows 'PermitUserEnvironment yes' in sshd_config to load these vars for SSH sessions.
# This fixes 'scp', 'sftp', and VS Code Remote connection issues.
env | grep -E "^(PATH|NIX_|CARGO_|RUST_|PKG_CONFIG|LD_)" > /root/.ssh/environment || true

# 6. Prepare Runtime Directories
mkdir -p /run/sshd

# 7. Execute Command
# If arguments are provided, execute them.
# Smart execution: if the first argument is a valid command, exec it directly.
# Otherwise, treat the arguments as a shell command (supporting loops, pipes, etc.).
if [ $# -gt 0 ]; then
    if command -v "$1" >/dev/null 2>&1; then
        exec "$@"
    else
        exec bash -c "$*"
    fi
fi

# Default: Start SSH Server
echo "Starting SSH server..."
exec $(which sshd) -D -e
