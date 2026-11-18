{
  description = "cargo-faster: find out why your Rust build is slow and how to make it faster";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixpkgs-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };
  };

  outputs =
    inputs@{ flake-parts, ... }:
    flake-parts.lib.mkFlake { inherit inputs; } {
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];

      perSystem =
        {
          system,
          lib,
          self',
          ...
        }:
        let
          pkgs = import inputs.nixpkgs {
            inherit system;
            overlays = [ (import inputs.rust-overlay) ];
          };
          rustToolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          rustPlatform = pkgs.makeRustPlatform {
            cargo = rustToolchain;
            rustc = rustToolchain;
          };
        in
        {
          _module.args.pkgs = pkgs;
          packages.cargo-faster = rustPlatform.buildRustPackage {
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
          packages.default = self'.packages.cargo-faster;

          devShells.default = pkgs.mkShell {
            packages = [
              rustToolchain
              pkgs.cargo-nextest
              pkgs.cargo-deny
              # summarize, crox and flamegraph for self-profiles.
              pkgs.measureme
            ];
            RUST_SRC_PATH = "${rustToolchain}/lib/rustlib/src/rust/library";
          };

          formatter = pkgs.nixfmt;
        };
    };
}
