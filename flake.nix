{
  inputs = {
    flake-utils.url = "github:numtide/flake-utils";
    naersk.url = "github:nix-community/naersk";
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    nixpkgs-mozilla = {
      url = "github:mozilla/nixpkgs-mozilla";
      flake = false;
    };
  };

  outputs =
    {
      self,
      flake-utils,
      naersk,
      nixpkgs,
      nixpkgs-mozilla,
    }:
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = (import nixpkgs) {
          inherit system;

          overlays = [
            (import nixpkgs-mozilla)
          ];
        };

        toolchain =
          (pkgs.rustChannelOf {
            rustToolchain = ./rust-toolchain.toml;
            sha256 = "sha256-AJ6LX/Q/Er9kS15bn9iflkUwcgYqRQxiOIL2ToVAXaU=";
          }).rust;
        naersk' = pkgs.callPackage naersk {
          cargo = toolchain;
          clippy = toolchain;
          rustc = toolchain;
        };
        # The rust package
        bacon-ls = naersk'.buildPackage {
          src = ./.;
        };

      in
      rec {
        # For `nix build` & `nix run`:
        defaultPackage = bacon-ls;

        # For `nix develop` (optional, can be skipped):
        devShell = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [
            cargo-audit
            cargo-nextest
            grcov
            llvmPackages_19.libllvm
            rust-analyzer
            toolchain
          ];
        };

        # Overlay for package usage in other Nix configurations
        overlay = final: prev: {
          inherit bacon-ls;
        };
      }
    );
}
