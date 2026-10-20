{
  description = "runboard: run metrics, queue history and device telemetry as HTML";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs/nixos-unstable";
    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };
    devshell = {
      url = "github:numtide/devshell";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    crane.url = "github:ipetkov/crane";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    inputs:
    inputs.flake-parts.lib.mkFlake { inherit inputs; } {
      imports = [ inputs.devshell.flakeModule ];
      flake.nixosModules.default = import ./nix/module.nix inputs.self;
      systems = [
        "x86_64-linux"
        "aarch64-linux"
        "aarch64-darwin"
      ];

      perSystem =
        { pkgs, system, ... }:
        let
          toolchain = pkgs.rust-bin.stable.latest.default.override {
            extensions = [
              "rust-src"
              "rustfmt"
              "clippy"
            ];
          };
          craneLib = (inputs.crane.mkLib pkgs).overrideToolchain toolchain;
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./src
              ./examples
              ./rustfmt.toml
            ];
          };
          mdformat = pkgs.python313.withPackages (ps: [
            ps.mdformat
            ps.mdformat-gfm
            ps.mdformat-footnote
            ps.mdformat-frontmatter
          ]);
          runboard = craneLib.buildPackage {
            inherit src;
            RUNBOARD_PUEUE = pkgs.lib.getExe' pkgs.pueue "pueue";
            RUNBOARD_NATS_SERVER = pkgs.lib.getExe pkgs.nats-server;
            nativeCheckInputs = [ pkgs.nats-server ];
            cargoArtifacts = null;
            cargoTestExtraArgs = "--all-targets";
            meta = {
              mainProgram = "runboard";
              license = pkgs.lib.licenses.mit;
            };
          };
        in
        {
          _module.args.pkgs = import inputs.nixpkgs {
            inherit system;
            overlays = [ inputs.rust-overlay.overlays.default ];
          };

          packages.default = runboard;
          checks = {
            inherit runboard;
            clippy = craneLib.cargoClippy {
              inherit src;
              cargoArtifacts = null;
              cargoClippyExtraArgs = "--all-targets -- -D warnings";
            };
            rustfmt = craneLib.cargoFmt {
              inherit src;
              cargoExtraArgs = "--all";
            };
          };
          formatter = pkgs.nixfmt;
          devshells.default = {
            name = "runboard";
            packages = [
              toolchain
              pkgs.stdenv.cc
              pkgs.pueue
              pkgs.nats-server
              pkgs.natscli
              pkgs.nixfmt
              pkgs.taplo
              mdformat
            ];
          };
        };
    };
}
