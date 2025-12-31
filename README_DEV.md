# Docker Development Environment

This directory contains the configuration for a Docker-based Linux development environment.

## 1. Prerequisites

- Docker Desktop installed and running.
- (Optional) VSCode with "Remote - SSH" extension installed.

## 2. Start the Environment

Run the following command in the project root:

```bash
docker-compose up -d --build
```

This will build the image and start the container `veloq-dev`.

## 3. Connecting via SSH

You can connect to the container using SSH:

```bash
ssh root@localhost -p 2222
# Password: root
```

Alternatively, add the following to your `~/.ssh/config` file for easier access:

```ssh
Host veloq-dev
    HostName localhost
    User root
    Port 2222
    ForwardAgent yes
```

Then you can simply run: `ssh veloq-dev`

## 4. Connecting via VSCode (Remote - SSH)

1. Open VSCode.
2. Press `F1` (or `Ctrl+Shift+P`) and run **Remote-SSH: Connect to Host...**.
3. Enter: `ssh root@localhost -p 2222`.
4. Enter password `root` when prompted.
5. Once connected, open the `/root/workspace` folder.

## 5. Notes

- **Source Code**: The current directory is mounted to `/root/workspace` in the container. Changes propagate instantly.
- **Port Mapping**:
    - `2222` -> `22` (SSH)
    - `8080`, `8081`, `9000` are mapped for your application availability.
- **Tools**: Installed tools include `rust`, `cargo`, `gdb`, `lldb`, `iproute2`, `tcpdump`.
