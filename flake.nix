{
  description = "FreeFlow - Linux/Wayland dictation daemon (see Sources/ for the macOS app)";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      # The Swift app in Sources/ is macOS-only and is built by the Makefile.
      # This flake covers the Rust reimplementation under linux/ and therefore
      # deliberately exposes Linux outputs only.
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};

      # Runtime programs the daemon shells out to. Kept in one place so the
      # package wrapper and the devShell cannot drift apart.
      runtimeDeps = with pkgs; [
        wtype
        wl-clipboard
      ];
    in
    {
      packages.${system} = {
        default = self.packages.${system}.freeflow;

        freeflow = pkgs.rustPlatform.buildRustPackage {
          pname = "freeflow";
          version = "0.1.0";
          src = ./linux;
          cargoLock.lockFile = ./linux/Cargo.lock;

          nativeBuildInputs = [ pkgs.makeWrapper ];

          postInstall = ''
            for bin in "$out"/bin/*; do
              wrapProgram "$bin" \
                --prefix PATH : ${pkgs.lib.makeBinPath runtimeDeps}
            done
          '';

          meta = {
            description = "Linux/Wayland dictation daemon";
            license = pkgs.lib.licenses.mit;
            platforms = pkgs.lib.platforms.linux;
          };
        };
      };

      devShells.${system}.default = pkgs.mkShell {
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
          ]
          ++ runtimeDeps;

        # cargo runs from the workspace root, not the repo root.
        shellHook = ''
          echo "freeflow linux dev shell - cd linux && cargo check"
        '';
      };

      formatter.${system} = pkgs.nixfmt-rfc-style;
    };
}
