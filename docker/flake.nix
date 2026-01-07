{
  description = "Veloq Development Environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs }:
    let
      supportedSystems = [ "x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin" ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
      nixpkgsFor = system: import nixpkgs { inherit system; config.allowUnfree = true; };

      commonPackages = pkgs: with pkgs; [
        bashInteractive
        busybox
        curl
        gcc
        gdb
        git
        iproute2
        lldb
        net-tools
        openssh
        openssl
        pkg-config
        # Use official packages to hit cache.nixos.org
        cargo
        rustc
        rust-analyzer
        clippy
        shadow
        tcpdump
        vim
      ];
    in
    {
      packages = forAllSystems (system:
        let pkgs = nixpkgsFor system; in
        {
          default = pkgs.buildEnv {
            name = "veloq-dev-env";
            paths = commonPackages pkgs;
          };
        });

      devShells = forAllSystems (system:
        let pkgs = nixpkgsFor system; in
        {
          default = pkgs.mkShell {
            packages = commonPackages pkgs;
          };
        });
    };
}
