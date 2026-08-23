{
  description = "Automagically generate commit messages";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs = {
    self,
    nixpkgs,
  }: let
    systems = ["x86_64-linux" "aarch64-linux" "x86_64-darwin" "aarch64-darwin"];
    forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
  in {
    packages = forAllSystems (pkgs: rec {
      auto-commit = pkgs.callPackage ./nix/package.nix {};
      default = auto-commit;
    });

    overlays.default = final: _prev: {
      auto-commit = final.callPackage ./nix/package.nix {};
    };

    devShells = forAllSystems (pkgs: {
      default = pkgs.mkShell {
        packages = with pkgs; [
          cargo
          rustc
          clippy
          rustfmt
          rust-analyzer
          git
          pkg-config
          openssl
        ];
        RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
      };
    });

    checks = forAllSystems (pkgs: {
      build = self.packages.${pkgs.system}.auto-commit;

      clippy = self.packages.${pkgs.system}.auto-commit.overrideAttrs (old: {
        pname = "auto-commit-clippy";
        nativeBuildInputs = old.nativeBuildInputs ++ [pkgs.clippy];
        buildPhase = "cargo clippy --all-targets --all-features -- --deny warnings";
        installPhase = "touch $out";
        postInstall = "";
        doCheck = false;
      });

      fmt =
        pkgs.runCommand "auto-commit-fmt" {
          nativeBuildInputs = [pkgs.rustfmt pkgs.cargo];
        } ''
          cd ${self}
          cargo fmt --check
          touch $out
        '';
    });

    nixosModules.default = import ./nix/module.nix self;

    formatter = forAllSystems (pkgs: pkgs.alejandra);
  };
}
