# syntax=docker/dockerfile:1
FROM nixos/nix:latest

# --- Phase 1: Nix Configuration ---
# Enable flakes and optimize for parallel builds
RUN echo "experimental-features = nix-command flakes" >> /etc/nix/nix.conf && \
    echo "max-jobs = auto" >> /etc/nix/nix.conf && \
    echo "http-connections = 128" >> /etc/nix/nix.conf && \
    echo "min-free = 1073741824" >> /etc/nix/nix.conf

WORKDIR /root

# --- Phase 2: Dependency Installation ---
COPY docker/flake.nix docker/flake.lock /root/

# Install dependencies into the Nix profile with caching
RUN --mount=type=tmpfs,target=/tmp \
    --mount=type=cache,target=/root/.cache/nix \
    --mount=type=cache,target=/nix-store-cache \
    nix \
    --option extra-substituters "file:///nix-store-cache" \
    --option require-sigs false \
    profile add .#default --priority 4 && \
    # Populate cache
    nix copy --to "file:///nix-store-cache?compression=none" .#default && \
    # Cleanup to reduce layer size
    nix-collect-garbage -d

# --- Phase 3: Environment & System Setup ---
# Generate environment loader
RUN mkdir -p /etc/profile.d && \
    nix print-dev-env .#default > /etc/profile.d/nix-env.sh

# Set standard paths
ENV CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:/root/.nix-profile/bin:/usr/bin:/bin:$PATH

RUN mkdir -p $CARGO_HOME && \
    # Create necessary SSH and system directories
    mkdir -p /var/run/sshd /etc/ssh /root/.ssh /var/empty/sshd && \
    # Setup users and groups
    echo "root:x:0:0:System Administrator:/root:/bin/bash" > /etc/passwd && \
    echo "sshd:x:74:74:Privilege-separated SSH:/var/empty/sshd:/sbin/nologin" >> /etc/passwd && \
    echo "root:x:0:" > /etc/group && \
    echo "sshd:x:74:" >> /etc/group && \
    echo "root:*:19733:0:99999:7:::" > /etc/shadow && \
    echo "sshd:*:19733:0:99999:7:::" >> /etc/shadow && \
    # Configure SSHD (Consolidated)
    echo "PermitRootLogin yes" > /etc/ssh/sshd_config && \
    echo "PasswordAuthentication yes" >> /etc/ssh/sshd_config && \
    echo "PubkeyAuthentication yes" >> /etc/ssh/sshd_config && \
    echo "Port 22" >> /etc/ssh/sshd_config && \
    echo "HostKey /etc/ssh/ssh_host_rsa_key" >> /etc/ssh/sshd_config && \
    echo "HostKey /etc/ssh/ssh_host_ed25519_key" >> /etc/ssh/sshd_config && \
    echo "Subsystem sftp internal-sftp" >> /etc/ssh/sshd_config && \
    echo "PermitUserEnvironment yes" >> /etc/ssh/sshd_config

# Symlink standard tools
RUN ln -sf $(which bash) /bin/bash && \
    ln -sf $(which pgrep) /usr/bin/pgrep && \
    ln -sf $(which ps) /usr/bin/ps

# --- Phase 4: Compatibility (FHS) ---
RUN source /etc/profile.d/nix-env.sh && \
    mkdir -p /lib64 && \
    ln -sf $(which nix-ld) /lib64/ld-linux-x86-64.so.2 && \
    mkdir -p /usr/lib64 /usr/lib && \
    LIBSTDCPP=$(find /root/.nix-profile/lib -name "libstdc++.so.6" -print -quit) && \
    if [ -n "$LIBSTDCPP" ]; then \
    ln -sf "$LIBSTDCPP" /usr/lib/libstdc++.so.6 && \
    ln -sf "$LIBSTDCPP" /usr/lib64/libstdc++.so.6; \
    fi

WORKDIR /root/workspace
COPY . .

# Setup Entrypoint
COPY docker/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

EXPOSE 22
ENTRYPOINT ["/entrypoint.sh"]
