#!/bin/bash
set -e

# Setup SSH authorized_keys if public key is mounted
if [ -f "/tmp/id_ed25519.pub" ]; then
    mkdir -p /root/.ssh
    if [ ! -f "/root/.ssh/authorized_keys" ]; then
        cp /tmp/id_ed25519.pub /root/.ssh/authorized_keys
        chmod 700 /root/.ssh
        chmod 600 /root/.ssh/authorized_keys
        echo "SSH public key installed."
    fi
fi

# Start SSH daemon
# -D: Do not detach and does not become a daemon
echo "Starting SSH server..."
/usr/sbin/sshd -D
