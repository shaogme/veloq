#!/usr/bin/env bash
set -e

# ==========================================
# NixOS Development Container Entrypoint
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

# 3. System Configuration (Unlock Read-Only Files & Fix Authentication)
# Decouple system files from Nix store to allow modification
for file in /etc/passwd /etc/shadow /etc/group; do
    if [ -L "$file" ]; then
        cp "$file" "${file}.tmp" && rm -f "$file" && mv "${file}.tmp" "$file"
    fi
done
chmod 644 /etc/passwd /etc/group
chmod 600 /etc/shadow

# Configure NSS (Fixes user lookup)
cat > /etc/nsswitch.conf <<EOF
passwd:    files
group:     files
shadow:    files
hosts:     files dns
EOF

# Configure PAM (Allow unconditional access for dev container)
mkdir -p /etc/pam.d
cat > /etc/pam.d/sshd <<EOF
auth       sufficient   pam_permit.so
account    sufficient   pam_permit.so
password   sufficient   pam_permit.so
session    sufficient   pam_permit.so
EOF
cp /etc/pam.d/sshd /etc/pam.d/other

# 4. Setup Root Access (Passwordless)
echo "PermitEmptyPasswords yes" >> /etc/ssh/sshd_config
sed -i "s|^root:[^:]*|root:|" /etc/shadow

# 5. Setup Authorized Keys
# Installs the host's public key for seamless SSH access
if [ -f "/tmp/id_ed25519.pub" ]; then
    mkdir -p /root/.ssh
    cp /tmp/id_ed25519.pub /root/.ssh/authorized_keys
    chmod 700 /root/.ssh
    chmod 600 /root/.ssh/authorized_keys
fi

# 6. Export Environment for SSH Sessions
# CRITICAL: Captures current environment (Nix paths) and saves it to ~/.ssh/environment.
# This allows 'PermitUserEnvironment yes' in sshd_config to load these vars for SSH sessions.
# This fixes 'scp', 'sftp', and VS Code Remote connection issues.
env | grep -E "^(PATH|NIX_|CARGO_|RUST_|PKG_CONFIG|LD_)" > /root/.ssh/environment || true

# 7. Prepare Runtime Directories
mkdir -p /run/sshd

# 8. Execute Command
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
