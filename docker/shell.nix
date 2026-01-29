{ sources ? import ./npins
, system ? builtins.currentSystem
, pkgs ? import sources.nixpkgs { inherit system; config.allowUnfree = true; }
}:
let
  deps = import ./deps.nix { inherit pkgs; };
in
pkgs.mkShell {
  buildInputs = deps.all;

  # --- Environment Variables ---
  
  # 1. Rust Source Path (Critical for rust-analyzer)
  RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";

  # 2. PKG_CONFIG_PATH for C dependencies
  PKG_CONFIG_PATH = "${pkgs.openssl.dev}/lib/pkgconfig";

  # 3. NIX_LD_LIBRARY_PATH (For nix-ld compatibility layer)
  NIX_LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath deps.runtimeLibs;

  # 4. Hints for dynamic linker
  NIX_LD = pkgs.lib.fileContents "${pkgs.stdenv.cc}/nix-support/dynamic-linker";
  LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath deps.runtimeLibs;
}
