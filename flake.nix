{
  description = "mcsapi: an xmonad-like desktop policy for Smithay compositors";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    # The toolchain comes from nixpkgs (rustc there is newer than the
    # workspace's rust-version), so its binary cache covers rustc, clippy and
    # rust-analyzer. crane has no inputs of its own, so it needs no `follows`.
    crane.url = "github:ipetkov/crane";
  };

  outputs =
    {
      self,
      nixpkgs,
      crane,
    }:
    let
      inherit (nixpkgs) lib;
      # Wayland only: Smithay and GPUI's wayland backend are Linux-only.
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      perSystem =
        pkgs:
        let
          craneLib = crane.mkLib pkgs;

          # Cargo sources only; README and bindings/node stay out of the
          # build hash. bindings/node is its own workspace.
          src = lib.fileset.toSource {
            root = ./.;
            fileset = lib.fileset.unions [
              ./Cargo.toml
              ./Cargo.lock
              (lib.fileset.fileFilter (f: f.hasExt "rs" || f.name == "Cargo.toml") ./crates)
              # mcsapi-zed-icons' build.rs embeds these SVGs.
              ./crates/mcsapi-zed-icons/icons
            ];
          };

          # Linked at build time by Smithay (xkbcommon) and GPUI.
          buildLibs = with pkgs; [
            libxkbcommon
            wayland
            fontconfig
            freetype
            openssl
            vulkan-loader
            libxcb
            # mcsapi-hardened-malloc; the cc wrapper's -L and rpath find it.
            graphene-hardened-malloc
          ];

          # dlopen'd at run time by winit, Smithay's EGL renderer and GPUI.
          runtimeLibs = with pkgs; [
            libGL
            libxkbcommon
            wayland
            vulkan-loader
            libx11
            libxcursor
            libxi
            libxrandr
            libxcb
          ];

          commonArgs = {
            inherit src;
            strictDeps = true;
            pname = "mcsapi-workspace";
            version = "0.1.0";
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = buildLibs;
            # Tests that open a window or create a GL context find the libs.
            LD_LIBRARY_PATH = lib.makeLibraryPath runtimeLibs;
          };

          # One build per feature set, like the CI matrix: default and GPUI.
          variants = {
            default = "";
            gpui = "--features mcsapi/gpui";
          };

          depsFor =
            features:
            craneLib.buildDepsOnly (
              commonArgs
              // {
                cargoExtraArgs = "--locked --workspace ${features}";
              }
            );
          deps = lib.mapAttrs (_: depsFor) variants;

          variantChecks = lib.concatMapAttrs (
            name: features:
            let
              args = commonArgs // {
                cargoArtifacts = deps.${name};
                cargoExtraArgs = "--locked --workspace ${features}";
              };
            in
            {
              "clippy-${name}" = craneLib.cargoClippy (
                args
                // {
                  cargoClippyExtraArgs = "--all-targets -- -D warnings";
                }
              );
              # Unit, integration and doc tests, as in CI.
              "test-${name}" = craneLib.cargoTest args;
              "doc-${name}" = craneLib.cargoDoc (
                args
                // {
                  cargoDocExtraArgs = "--no-deps";
                  RUSTDOCFLAGS = "-D warnings";
                }
              );
            }
          ) variants;

          bin =
            pname:
            craneLib.buildPackage (
              commonArgs
              // {
                inherit pname;
                cargoArtifacts = deps.default;
                cargoExtraArgs = "--locked -p ${pname}";
                doCheck = false;
                meta.mainProgram = pname;
              }
            );

          packages = rec {
            mcsapi-mcp = bin "mcsapi-mcp";
            x2mcsapi = bin "x2mcsapi";
            default = pkgs.symlinkJoin {
              name = "mcsapi";
              paths = [
                mcsapi-mcp
                x2mcsapi
              ];
            };
          };
        in
        {
          inherit packages;

          checks =
            variantChecks
            // {
              fmt = craneLib.cargoFmt {
                inherit (commonArgs) src pname version;
                cargoExtraArgs = "--all";
              };
              # The headless example CI runs after the tests.
              example-desktop = craneLib.mkCargoDerivation (
                commonArgs
                // {
                  pname = "mcsapi-example-desktop";
                  cargoArtifacts = deps.default;
                  # --workspace keeps feature unification, and so the deps, shared.
                  buildPhaseCargoCommand = ''
                    cargoWithProfile build --locked --workspace --examples
                    target/release/examples/desktop > desktop.out
                  '';
                  installPhaseCommand = "install -Dm644 desktop.out $out/desktop.out";
                }
              );
            }
            // lib.mapAttrs' (name: lib.nameValuePair "package-${name}") packages;

          devShells.default = craneLib.devShell {
            inherit (commonArgs) LD_LIBRARY_PATH;
            # nixpkgs' rustc has no llvm-tools component, so a cargo-llvm-cov
            # of one's own uses the LLVM rustc was built with instead.
            LLVM_COV = "${pkgs.rustc.llvmPackages.llvm}/bin/llvm-cov";
            LLVM_PROFDATA = "${pkgs.rustc.llvmPackages.llvm}/bin/llvm-profdata";
            inputsFrom = [ deps.gpui ];
            packages = with pkgs; [
              rust-analyzer
              # No cargo-llvm-cov: with nixpkgs' rustc 1.99, a test binary
              # built with -C instrument-coverage crashes (SIGSEGV) as it
              # writes its profile, so the package fails its own tests and
              # took this shell down with it. The Coverage job in CI measures
              # with rustup's toolchain instead.
              cargo-audit
              bacon
              # bindings/node
              nodejs_22
              # Shell and future DRM/udev backend libraries.
              libinput
              udev
              libdrm
              libgbm
              seatd
            ];
          };

          formatter = pkgs.treefmt.withConfig {
            runtimeInputs = [
              pkgs.nixfmt
              pkgs.rustfmt
            ];
            settings = {
              tree-root-file = "flake.nix";
              formatter.nixfmt = {
                command = "nixfmt";
                includes = [ "*.nix" ];
              };
              formatter.rustfmt = {
                command = "rustfmt";
                options = [
                  "--edition"
                  "2024"
                ];
                includes = [ "*.rs" ];
              };
            };
          };
        };

      all = forAllSystems perSystem;
    in
    {
      packages = lib.mapAttrs (_: s: s.packages) all;
      checks = lib.mapAttrs (_: s: s.checks) all;
      devShells = lib.mapAttrs (_: s: s.devShells) all;
      formatter = lib.mapAttrs (_: s: s.formatter) all;
    };
}
