{
  description = "FreeFlow - Linux/Wayland dictation daemon (see Sources/ for the macOS app)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      # The Swift app in Sources/ is macOS-only and is built by the Makefile.
      # This flake covers the Rust reimplementation under linux/ and therefore
      # deliberately exposes Linux outputs only.
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});

      # Runtime programs the daemon shells out to. Kept in one place so the
      # package wrapper and the devShell cannot drift apart.
      runtimeDeps =
        pkgs: with pkgs; [
          wtype
          wl-clipboard
        ];
    in
    {
      packages = forAllSystems (pkgs: {
        default = self.packages.${pkgs.stdenv.hostPlatform.system}.freeflow;

        freeflow = pkgs.rustPlatform.buildRustPackage {
          pname = "freeflow";
          version = "0.1.0";
          src = ./linux;
          cargoLock.lockFile = ./linux/Cargo.lock;

          nativeBuildInputs = [
            pkgs.makeWrapper
            pkgs.pkg-config
            # Without this the GTK overlay fails at runtime on missing icon
            # themes and settings schemas, which no unit test would catch.
            pkgs.wrapGAppsHook4
          ];
          buildInputs = [
            pkgs.alsa-lib
            pkgs.gtk4
            pkgs.gtk4-layer-shell
          ];

          # Build only the shipped binaries. `freeflow-mock-harness` is a test
          # fixture for the worker pool and has no business in a user profile.
          cargoBuildFlags = [
            "--bin=freeflowd"
            "--bin=freeflow"
            "--bin=freeflow-overlay"
          ];

          # Exercised by the module test and the Rust suite; running them again
          # inside the sandbox would need a compositor and a microphone.
          doCheck = false;

          # wrapGAppsHook4 wraps the binaries itself, so contribute the
          # runtime PATH through its hook rather than wrapping twice.
          preFixup = ''
            gappsWrapperArgs+=(--prefix PATH : ${pkgs.lib.makeBinPath (runtimeDeps pkgs)})
          '';

          meta = {
            description = "Linux/Wayland dictation daemon";
            license = pkgs.lib.licenses.mit;
            platforms = pkgs.lib.platforms.linux;
          };
        };
      });

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages =
            with pkgs;
            [
              cargo
              rustc
              rustfmt
              clippy
              rust-analyzer
              pkg-config
              alsa-lib
              gtk4
              gtk4-layer-shell
            ]
            ++ runtimeDeps pkgs;

          # cargo runs from the workspace root, not the repo root.
          shellHook = ''
            echo "freeflow linux dev shell - cd linux && cargo check"
          '';
        };
      });

      checks = forAllSystems (pkgs: {
        # Catches a broken Home Manager module here rather than during a
        # rebuild on someone's machine.
        home-manager-module = import ./linux/nix/module-test.nix { inherit pkgs self; };
      });

      # Import into a Home Manager configuration to get the daemon, the local
      # whisper-server, and the Hyprland bindings wired up together.
      homeManagerModules = {
        default = import ./linux/nix/home-manager-module.nix { inherit self; };
        freeflow = self.homeManagerModules.default;
      };

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
