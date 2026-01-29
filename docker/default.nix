{ sources ? import ./npins
, system ? builtins.currentSystem
, pkgs ? import sources.nixpkgs { inherit system; config.allowUnfree = true; }
}:
let
  deps = import ./deps.nix { inherit pkgs; };
in
pkgs.buildEnv {
  name = "nixos-dev-env";
  paths = deps.all;
}
