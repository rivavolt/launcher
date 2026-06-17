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
      systemOutputs = flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (system:
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
              for bin in launcher clipboard picker clipd; do
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
          # The launcher and clipboard are now wlr-layer-shell overlays (see
          # src/layer.rs), so they need none of the old per-class Hyprland
          # window rules. The overlay layer, top anchor + centering, gold
          # border and rounded corners are all properties of the layer surface
          # the app creates — there is no managed window to force onto a special
          # workspace, float, border, round, or move. That retires the entire
          # `hyprctl eval` rule-injection dance (and its UWSM-signature
          # resolution and the configreloaded re-injection watcher) that the
          # eframe-window approach depended on.
          #
          # Show/hide is now surface map/unmap driven by SIGUSR1: the daemons
          # idle until signalled, pop up, and dismiss on Escape / activation /
          # focus loss. The show keybind must therefore send the signal
          # (`pkill -USR1 launcher` / `pkill -USR1 clipboard`) instead of
          # toggling a special workspace — that binding lives in the Hyprland
          # config (nixos-config), out of this flake.
        in {
          options.services.launcher = {
            enable = lib.mkEnableOption "launcher service";
          };

          config = lib.mkIf cfg.enable {
            environment.systemPackages = [ launcherPkg ];

            systemd.user.services.launcher = {
              description = "Launcher (wlr-layer-shell overlay)";
              wantedBy = [ "hyprland-session.target" ];
              partOf = [ "hyprland-session.target" ];
              # User services are not restarted by `nixos-rebuild switch` the way
              # system services are, so a rebuilt binary would otherwise keep
              # running the previous store path until the next logout. Keying a
              # restartTrigger on the package makes the switch restart the daemon
              # whenever the binary changes, so the running process always matches
              # the deployed build. The daemon idles between pop-ups, so the
              # restart is invisible unless an overlay happens to be mapped.
              restartTriggers = [ launcherPkg ];
              path = lib.mkForce [];
              environment = {
                GIO_EXTRA_MODULES = "${pkgs.dconf.lib}/lib/gio/modules:${pkgs.glib-networking}/lib/gio/modules";
              };
              serviceConfig = {
                # No ExecStartPre rule injection: a layer surface needs no
                # window rules. The daemon idles until SIGUSR1 (the show
                # keybind), then maps its overlay. hyprctl (for the client
                # list / focus dispatch / event subscription) comes from the
                # inherited PATH below.
                ExecStart = "${launcherPkg}/bin/launcher";
                Restart = "on-failure";
                RestartSec = 2;
                PassEnvironment = "PATH HYPRLAND_INSTANCE_SIGNATURE XDG_RUNTIME_DIR WAYLAND_DISPLAY TERMINAL XDG_DATA_DIRS DBUS_SESSION_BUS_ADDRESS HOME";
              };
            };

            systemd.user.services.clipboard = {
              description = "Clipboard (wlr-layer-shell overlay)";
              wantedBy = [ "hyprland-session.target" ];
              partOf = [ "hyprland-session.target" ];
              restartTriggers = [ launcherPkg ];
              path = [ pkgs.hyprland pkgs.wl-clipboard ];
              serviceConfig = {
                ExecStart = "${launcherPkg}/bin/clipboard";
                Restart = "on-failure";
                RestartSec = 2;
                PassEnvironment = "HYPRLAND_INSTANCE_SIGNATURE XDG_RUNTIME_DIR WAYLAND_DISPLAY XDG_CACHE_HOME HOME";
              };
            };

            systemd.user.services.clipd = {
              description = "Clipboard daemon";
              wantedBy = [ "hyprland-session.target" ];
              partOf = [ "hyprland-session.target" ];
              restartTriggers = [ launcherPkg ];
              # clipd shells out to wl-copy to persist each new entry onto the
              # live selection so it outlives the app that copied it.
              path = [ pkgs.wl-clipboard ];
              serviceConfig = {
                ExecStart = "${launcherPkg}/bin/clipd";
                Restart = "on-failure";
                RestartSec = 2;
                PassEnvironment = "HYPRLAND_INSTANCE_SIGNATURE XDG_RUNTIME_DIR WAYLAND_DISPLAY XDG_CACHE_HOME HOME";
              };
            };
          };
        };
    };
}
