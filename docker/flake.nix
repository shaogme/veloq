{
  description = "Veloq Development Environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils, ... }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          config.allowUnfree = true;
        };

        # 1. Setup Rust Toolchain using standard Nixpkgs
        # We use standard packages instead of overlay to utilize cache.nixos.org
        rustToolchain = with pkgs; [
            cargo
            rustc
            rust-analyzer
            clippy
            rustfmt
        ];

        # 2. Define libraries for System Compatibility (nix-ld)
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

        # 3. Development Tools
        devTools = with pkgs; [
          # Rust Dependencies
          pkg-config
          openssl.dev

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

          # Nix Utilities
          nix-ld
          direnv
          nix-direnv
        ] ++ rustToolchain;
      in
      {
        packages.default = pkgs.buildEnv {
          name = "veloq-dev-env";
          paths = devTools ++ runtimeLibs;
        };

        devShells.default = pkgs.mkShell {
          buildInputs = devTools ++ runtimeLibs;

          # --- Environment Variables ---
          
          # 1. Rust Source Path (Critical for rust-analyzer)
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";

          # 2. PKG_CONFIG_PATH for C dependencies
          PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";

          # 3. NIX_LD_LIBRARY_PATH (For nix-ld compatibility layer)
          NIX_LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibs;

          # 4. Hints for dynamic linker
          NIX_LD = pkgs.lib.fileContents "${pkgs.stdenv.cc}/nix-support/dynamic-linker";
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath runtimeLibs;
        };
      }
    );
}
