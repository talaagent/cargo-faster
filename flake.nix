{
  description = "cargo-faster: find out why your Rust build is slow and how to make it faster";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
  };

  outputs =
    {
      self,
      nixpkgs,
    }:
    let
      inherit (nixpkgs) lib;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "x86_64-darwin"
        "aarch64-darwin"
      ];
      forAll = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAll (pkgs: rec {
        cargo-faster = pkgs.rustPlatform.buildRustPackage {
          pname = "cargo-faster";
          version = (lib.importTOML ./Cargo.toml).workspace.package.version;
          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./crates
            ];
          };
          cargoLock.lockFile = ./Cargo.lock;
          # The tests drive cargo and rustc; run them in the dev shell.
          doCheck = false;
          postInstall = ''
            mkdir -p $out/share/man/man1
            $out/bin/cargo-faster --man > $out/share/man/man1/cargo-faster.1
          '';
          meta = {
            description = "Find out why your Rust build is slow and how to make it faster";
            homepage = "https://github.com/talaagent/cargo-faster";
            license = lib.licenses.asl20;
            mainProgram = "cargo-faster";
          };
        };
        default = cargo-faster;
      });

      devShells = forAll (pkgs: {
        default = pkgs.mkShell {
          packages = [
            pkgs.cargo
            pkgs.rustc
            pkgs.clippy
            pkgs.rustfmt
            pkgs.rust-analyzer
            pkgs.cargo-nextest
            pkgs.cargo-deny
            # summarize, crox and flamegraph for self-profiles.
            pkgs.measureme
          ];
          RUST_SRC_PATH = "${pkgs.rustPlatform.rustLibSrc}";
        };
      });

      formatter = forAll (pkgs: pkgs.nixfmt);
    };
}
