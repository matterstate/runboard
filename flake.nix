{
  description = "runboard: run metrics, queue history and device telemetry as HTML";

  inputs = {
    nixpkgs.url = "github:nixos/nixpkgs?ref=nixos-unstable";
    flake-parts = {
      url = "github:hercules-ci/flake-parts";
      inputs.nixpkgs-lib.follows = "nixpkgs";
    };
    devshell = {
      url = "github:numtide/devshell";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    pyproject-nix = {
      url = "github:pyproject-nix/pyproject.nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    uv2nix = {
      url = "github:pyproject-nix/uv2nix";
      inputs.pyproject-nix.follows = "pyproject-nix";
      inputs.nixpkgs.follows = "nixpkgs";
    };
    pyproject-build-systems = {
      url = "github:pyproject-nix/build-system-pkgs";
      inputs.pyproject-nix.follows = "pyproject-nix";
      inputs.uv2nix.follows = "uv2nix";
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
          workspace = inputs.uv2nix.lib.workspace.loadWorkspace { workspaceRoot = ./.; };
          pythonSet =
            (pkgs.callPackage inputs.pyproject-nix.build.packages {
              python = pkgs.python313;
            }).overrideScope
              (
                pkgs.lib.composeManyExtensions [
                  inputs.pyproject-build-systems.overlays.wheel
                  (workspace.mkPyprojectOverlay { sourcePreference = "wheel"; })
                  (final: prev: {
                    runboard = prev.runboard.overrideAttrs (old: {
                      nativeBuildInputs =
                        (old.nativeBuildInputs or [ ]) ++ final.resolveBuildSystem { setuptools = [ ]; };
                    });
                  })
                ]
              );
          pythonEnvironment = pythonSet.mkVirtualEnv "runboard-python" workspace.deps.default;
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              ./src
              ./python/runboard
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
            nativeCheckInputs = [
              pkgs.nats-server
              pkgs.python313
            ];
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
          packages.python = pythonEnvironment;
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
          }
          // pkgs.lib.optionalAttrs (system == "x86_64-linux") {
            deployment = import ./nix/deployment-test.nix {
              inherit pkgs;
              module = inputs.self.nixosModules.default;
              package = runboard;
            };
          };
          formatter = pkgs.nixfmt;
          devshells.default = {
            name = "runboard";
            env = [
              {
                name = "UV_PYTHON";
                value = "${pythonEnvironment}/bin/python";
              }
              {
                name = "UV_PYTHON_DOWNLOADS";
                value = "never";
              }
            ]
            ++ pkgs.lib.optionals pkgs.stdenv.hostPlatform.isLinux [
              {
                name = "LIBC_PATH";
                value = "${pkgs.lib.getLib pkgs.stdenv.cc.libc}/lib/libc.so.6";
              }
            ];
            packages = [
              toolchain
              pkgs.stdenv.cc
              pkgs.llvmPackages.clang-unwrapped
              pkgs.pueue
              pkgs.nats-server
              pkgs.natscli
              pkgs.nixfmt
              pkgs.leptosfmt
              pythonEnvironment
              pkgs.uv
              pkgs.ruff
              pkgs.ast-grep
              pkgs.taplo
              pkgs.yamlfmt
              pkgs.actionlint
              pkgs.cargo-audit
              pkgs.gitleaks
              (pkgs.lib.lowPrio mdformat)
            ];
          };
        };
    };
}
