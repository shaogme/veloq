{ pkgs }:
let
  # 1. Define libraries for System Compatibility (nix-ld)
  # These are commonly required by VS Code Server, Copilot, and other unpatched binaries
  runtimeLibs = with pkgs; [
    stdenv.cc.cc.lib
    zlib
    openssl
    icu
    libsecret
    glib
    libkrb5
    util-linux
  ];

  # 2. Development Tools
  devTools = with pkgs; [
    # Standard Linux Utilities
    glibc     # Standard C Library
    coreutils # ls, cp, mv, mkdir, etc.
    findutils # find, xargs
    gnugrep   # grep
    gnused    # sed
    gawk      # awk
    gnutar    # tar
    gzip      # gzip
    wget
    which
    xz
    cacert

    # Shell & Utilities
    bashInteractive
    curl
    git
    gdb
    lldb
    iproute2
    net-tools
    procps # pgrep, ps
    openssh
    shadow
    tcpdump
    vim

    # Rust Dependencies
    cargo
    rustc
    rust-analyzer
    clippy
    rustfmt
    pkg-config
    openssl.dev

    # Nix Utilities
    nix
    nix-ld
    direnv
    nix-direnv
  ];
in
{
  inherit runtimeLibs devTools;
  all = devTools ++ runtimeLibs;
}
