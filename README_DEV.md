# Docker Development Environment

This directory contains the configuration for a Docker-based Linux development environment, powered by **Nix Flakes** for reproducible builds.

## 0. Initial Setup (One-time)

Before building the image for the first time, you must generate the `flake.lock` file to pin your dependencies. We provide a helper container for this:

```bash
# Generate docker/flake.lock
docker-compose run --rm flake-update
```

If you ever need to update your dependencies (e.g., to get a newer Rust version), simply run this command again and rebuild.

## 1. Prerequisites

- Docker Desktop installed and running.
- **Nix Support**: The environment now uses Nix flakes for reproducible builds.
- **Windows Users**: You can run this setup directly from PowerShell or WSL2. No need to install Nix on the host unless you want to use the `flake-update` command natively.


## 2. Start the Environment

### Option A: Development Mode (Recommended)
Use this for active development. Your local source code is mounted into the container, allowing hot-reloading/live-editing.

```bash
docker-compose up -d --build dev
```

### Option B: Standalone Mode
Use this to test the self-contained image. The source code is copied into the image at build time and is isolated from your local file system changes.

```bash
docker-compose up -d --build standalone
```

**Note**: Both modes use the same ports (SSH 2222, App 8080+), so stop one before starting the other.

This will build the image and start the container (`veloq-dev` or `veloq-standalone`).

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
    Port 2222
    User root
    StrictHostKeyChecking no
    UserKnownHostsFile /dev/null
    IdentityFile ~/.ssh/id_ed25519
```

Then you can simply run: `ssh veloq-dev`

## 4. Running Commands Directly

You can execute cargo commands directly inside the container without SSH:

```bash
# Run cargo check
docker-compose run --rm dev cargo check

# Run tests
docker-compose run --rm dev cargo test

# Open a shell
docker-compose run --rm dev bash
```

## 5. Connecting via VSCode (Remote - SSH)

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
