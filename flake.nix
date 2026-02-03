{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
    flake-utils.url = "github:numtide/flake-utils";
    crane.url = "github:ipetkov/crane";
  };

  outputs = { self, nixpkgs, rust-overlay, flake-utils, crane }:
    let
      # System-specific outputs
      systemOutputs = flake-utils.lib.eachDefaultSystem (system:
        let
          overlays = [ (import rust-overlay) ];
          pkgs = import nixpkgs { inherit system overlays; };
          rustToolchain = pkgs.rust-bin.stable.latest.default;

          craneLib = (crane.mkLib pkgs).overrideToolchain rustToolchain;

          # Common build inputs
          buildInputs = with pkgs; [
            wayland
            libxkbcommon
            vulkan-loader
            fontconfig
            freetype
          ];
          nativeBuildInputs = with pkgs; [ pkg-config makeWrapper mold clang ];

          # Use mold linker for faster builds
          CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER = "clang";
          CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS = "-C link-arg=-fuse-ld=mold";

          # Build deps only (for caching)
          cargoArtifacts = craneLib.buildDepsOnly {
            src = craneLib.cleanCargoSource ./.;
            inherit buildInputs nativeBuildInputs;
            CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER = "clang";
            CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS = "-C link-arg=-fuse-ld=mold";
          };

          # Build the actual package
          launcher = craneLib.buildPackage {
            inherit cargoArtifacts buildInputs nativeBuildInputs;
            src = craneLib.cleanCargoSource ./.;
            doCheck = false; # Skip tests for faster builds
            CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER = "clang";
            CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_RUSTFLAGS = "-C link-arg=-fuse-ld=mold";

            postInstall = ''
              # Wrap both binaries with library paths
              for bin in launcher clipboard; do
                wrapProgram $out/bin/$bin \
                  --prefix LD_LIBRARY_PATH : ${pkgs.lib.makeLibraryPath [
                    pkgs.wayland
                    pkgs.libxkbcommon
                    pkgs.vulkan-loader
                    pkgs.fontconfig
                  ]}
              done
            '';
          };
        in {
          devShells.default = pkgs.mkShell {
            buildInputs = buildInputs ++ (with pkgs; [
              rustToolchain
              pkg-config
              ibm-plex
              bacon
              cargo-watch
              process-compose
            ]);

            LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [
              pkgs.wayland
              pkgs.libxkbcommon
              pkgs.vulkan-loader
              pkgs.fontconfig
            ];
          };

          packages.default = launcher;
        }
      );
    in
    systemOutputs // {
      nixosModules.default = { config, lib, pkgs, ... }:
        let
          cfg = config.services.launcher;
          launcherPkg = self.packages.${pkgs.stdenv.hostPlatform.system}.default;
        in {
          options.services.launcher = {
            enable = lib.mkEnableOption "launcher service";
          };

          config = lib.mkIf cfg.enable {
            environment.systemPackages = [ launcherPkg ];

            systemd.user.services.launcher = {
              description = "Launcher (eframe)";
              wantedBy = [ "hyprland-session.target" ];
              partOf = [ "hyprland-session.target" ];
              after = [ "hyprland-session.target" ];
              path = lib.mkForce [];
              environment = {
                GIO_EXTRA_MODULES = "${pkgs.dconf.lib}/lib/gio/modules:${pkgs.glib-networking}/lib/gio/modules";
              };
              serviceConfig = {
                ExecStart = "${launcherPkg}/bin/launcher";
                Restart = "on-failure";
                RestartSec = 2;
                PassEnvironment = "PATH HYPRLAND_INSTANCE_SIGNATURE XDG_RUNTIME_DIR WAYLAND_DISPLAY TERMINAL XDG_DATA_DIRS DBUS_SESSION_BUS_ADDRESS HOME";
              };
            };

            systemd.user.services.clipboard = {
              description = "Clipboard (eframe)";
              wantedBy = [ "hyprland-session.target" ];
              partOf = [ "hyprland-session.target" ];
              after = [ "hyprland-session.target" ];
              path = [ pkgs.hyprland pkgs.cliphist pkgs.wl-clipboard ];
              serviceConfig = {
                ExecStart = "${launcherPkg}/bin/clipboard";
                Restart = "on-failure";
                RestartSec = 2;
                PassEnvironment = "HYPRLAND_INSTANCE_SIGNATURE XDG_RUNTIME_DIR WAYLAND_DISPLAY XDG_DATA_HOME HOME";
              };
            };
          };
        };
    };
}
