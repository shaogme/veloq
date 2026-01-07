#!/usr/bin/env bash
set -e

# Load Nix environment variables (generated during build)
if [ -f /etc/profile.d/nix-env.sh ]; then
    source /etc/profile.d/nix-env.sh
fi

# Generate SSH host keys if they don't exist
if [ ! -f /etc/ssh/ssh_host_rsa_key ]; then
    echo "Generating RSA host key..."
    ssh-keygen -f /etc/ssh/ssh_host_rsa_key -N '' -t rsa
fi
if [ ! -f /etc/ssh/ssh_host_ed25519_key ]; then
    echo "Generating ED25519 host key..."
    ssh-keygen -f /etc/ssh/ssh_host_ed25519_key -N '' -t ed25519
fi

# Setup root password
if [ -n "$ROOT_PASSWORD" ]; then
    echo "Setting root password from environment variable..."
    echo "root:$ROOT_PASSWORD" | chpasswd
else
    echo "Warning: ROOT_PASSWORD not set. Defaulting to 'root'"
    echo "root:root" | chpasswd
fi

# Setup SSH authorized_keys if public key is mounted
if [ -f "/tmp/id_ed25519.pub" ]; then
    mkdir -p /root/.ssh
    cp -f /tmp/id_ed25519.pub /root/.ssh/authorized_keys
    chmod 700 /root/.ssh
    chmod 600 /root/.ssh/authorized_keys
    echo "SSH public key installed/updated."
fi

# If arguments are provided, execute them
if [ $# -gt 0 ]; then
    exec "$@"
fi

# Start SSH daemon
# -D: Do not detach and does not become a daemon
echo "Starting SSH server..."
# Ensure /var/run/sshd exists
mkdir -p /var/run/sshd

# Find sshd in path
SSHD_BIN=$(which sshd)
if [ -z "$SSHD_BIN" ]; then
    echo "Error: sshd not found in PATH"
    exit 1
fi

exec "$SSHD_BIN" -D -e
