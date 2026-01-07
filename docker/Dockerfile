# syntax=docker/dockerfile:1
FROM nixos/nix:latest

# Enable flakes and configure nix
RUN echo "experimental-features = nix-command flakes" >> /etc/nix/nix.conf && \
    # 自动设置最大构建并行数（通常等于 CPU 核心数）
    echo "max-jobs = auto" >> /etc/nix/nix.conf && \
    # 关键：增加对二进制缓存服务器的 HTTP 连接数（默认是 25，改为 50 或 100）
    echo "http-connections = 128" >> /etc/nix/nix.conf && \
    # 设置下载重试次数，防止网络抖动导致失败
    echo "download-attempts = 3" >> /etc/nix/nix.conf && \
    # 避免在磁盘空间不足时频繁触发 GC，这会拖慢构建
    echo "min-free = 1073741824" >> /etc/nix/nix.conf

WORKDIR /root

# Copy flake definition and lockfile
COPY docker/flake.nix docker/flake.lock /root/

# Install dependencies via flake and clean up to reduce image size
# Utilizes tmpfs for speed
RUN --mount=type=tmpfs,target=/tmp \
    --mount=type=cache,target=/root/.cache/nix \
    nix profile add .#default --priority 4 && \
    nix-collect-garbage -d

# Generate environment script from devShell to automatically set compiler flags (PKG_CONFIG, etc.)
RUN mkdir -p /etc/profile.d && nix print-dev-env .#default > /etc/profile.d/nix-env.sh

ENV CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:/root/.nix-profile/bin:/usr/bin:/bin:$PATH

# Setup Cargo Home for caching (rust toolchain is now in nix profile)
RUN mkdir -p $CARGO_HOME

# Configure SSH and User
# 1. Create necessary directories
RUN mkdir -p /var/run/sshd /etc/ssh /root/.ssh && \
    # 2. Force recreate auth files to ensure they exist and are valid
    echo "root:x:0:0:System Administrator:/root:/bin/bash" > /etc/passwd && \
    echo "sshd:x:74:74:Privilege-separated SSH:/var/empty/sshd:/sbin/nologin" >> /etc/passwd && \
    echo "root:x:0:" > /etc/group && \
    echo "sshd:x:74:" >> /etc/group && \
    # 3. Generate password hash for 'root' (password: root) and write to shadow
    # We use openssl which is installed via the flake
    echo "root:*:19733:0:99999:7:::" > /etc/shadow && \
    echo "sshd:*:19733:0:99999:7:::" >> /etc/shadow && \
    # Create privilege separation directory
    mkdir -p /var/empty/sshd && \
    # 4. Configure SSH Daemon
    echo "PermitRootLogin yes" > /etc/ssh/sshd_config && \
    echo "PasswordAuthentication yes" >> /etc/ssh/sshd_config && \
    echo "PubkeyAuthentication yes" >> /etc/ssh/sshd_config && \
    echo "Port 22" >> /etc/ssh/sshd_config && \
    echo "HostKey /etc/ssh/ssh_host_rsa_key" >> /etc/ssh/sshd_config && \
    echo "HostKey /etc/ssh/ssh_host_ed25519_key" >> /etc/ssh/sshd_config

# Ensure bash is available at standard location if needed by scripts
RUN ln -sf $(which bash) /bin/bash

WORKDIR /root/workspace

# Copy source code for standalone mode
COPY . .

COPY docker/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

EXPOSE 22

ENTRYPOINT ["/entrypoint.sh"]
