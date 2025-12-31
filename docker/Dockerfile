FROM rust:latest

# Install basic development tools, SSH server, and networking tools
RUN apt-get update && apt-get install -y \
    openssh-server \
    gdb \
    lldb \
    build-essential \
    pkg-config \
    iproute2 \
    curl \
    git \
    vim \
    net-tools \
    tcpdump \
    && rm -rf /var/lib/apt/lists/*

# Configure SSH
RUN mkdir -p /var/run/sshd
# Set root password to 'root' (Usage: ssh root@localhost -p 2222)
RUN echo 'root:root' | chpasswd
# Allow root login
RUN sed -i 's/#PermitRootLogin prohibit-password/PermitRootLogin yes/' /etc/ssh/sshd_config
# SSH login fix: Otherwise user is kicked off after login
RUN sed -i 's@session\s*required\s*pam_loginuid.so@session optional pam_loginuid.so@g' /etc/pam.d/sshd

# Environment variables setup
ENV NOTVISIBLE="in users profile"
RUN echo "export RUSTUP_HOME=/usr/local/rustup" >> /etc/profile
RUN echo "export CARGO_HOME=/usr/local/cargo" >> /etc/profile
RUN echo "export PATH=\"/usr/local/cargo/bin:\$PATH\"" >> /etc/profile

# Install useful cargo subcommands (optional, can be commented out to speed up build)
# RUN cargo install cargo-expand cargo-edit

WORKDIR /root/workspace

COPY docker/entrypoint.sh /entrypoint.sh
RUN chmod +x /entrypoint.sh

EXPOSE 22

ENTRYPOINT ["/entrypoint.sh"]
