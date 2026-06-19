# scripts/

Developer tooling for the launcher/clipboard overlay daemons. Not part of the
build — these are run by hand.

## `heaptrack-overlay.sh` — memory-profiling harness

Catches the overlay daemon's slow per-show memory accumulation by driving many
real show/hide cycles inside a **nested, headless Hyprland** while recording a
[heaptrack](https://github.com/KDE/heaptrack) allocation trace and sampling RSS.

### Why it has to be this elaborate

The daemon once leaked unbounded — ~8 GB over ~2 days of real use — which is why
the systemd units now carry `MemoryMax`. Static analysis found the icon cache
bounded, so the suspect is the GPU/wgpu/egui-texture lifecycle in `src/layer.rs`,
which only runs **while the surface is mapped and rendering**. Reproducing it
needs (a) a real Wayland compositor and (b) many map → render → unmap cycles.

The overlays are also **Hyprland-coupled**: `src/hyprland.rs` shells out to
`hyprctl` (monitors / clients / dispatch) and reads
`HYPRLAND_INSTANCE_SIGNATURE`, so they will *not* run under a plain `cage`/`sway`
nest. The harness therefore spins a nested Hyprland that gives the daemon its own
instance signature and a working `hyprctl`, but runs it **headless** (wlroots
`WLR_BACKENDS=headless`) on its own `WAYLAND_DISPLAY` and its own
`XDG_RUNTIME_DIR` so nothing lands on your real screen and your live Hyprland
session is never touched.

### Prerequisites

Nothing needs to be installed. The script re-execs itself inside

```
nix shell nixpkgs#heaptrack nixpkgs#hyprland nixpkgs#wtype -c …
```

so `heaptrack`, `Hyprland`, `hyprctl`, and `wtype` are all provisioned on the
fly. (Verified attrs: `heaptrack` 1.5.0, `hyprland` 0.54.2, `wtype` 0.4.) The
analysis tools `heaptrack_print` / `heaptrack_gui` ship in the same `heaptrack`
package.

You do **not** need to be inside a graphical session — the nested compositor is
headless, so this works fine over SSH or from a TTY (it just needs a working DRM
or software-rendering path that wlroots' headless backend can use).

### Usage

```
./scripts/heaptrack-overlay.sh [launcher|clipboard] [cycles] [binary-path]
```

| arg | meaning | default |
| --- | --- | --- |
| `$1` | target binary: `launcher` or `clipboard` | `launcher` |
| `$2` | number of show/hide cycles | `200` |
| `$3` | explicit path to the binary (skips `nix build`) | *(builds with nix)* |

Examples:

```
./scripts/heaptrack-overlay.sh                    # launcher, 200 cycles, nix-built
./scripts/heaptrack-overlay.sh clipboard 500      # clipboard, 500 cycles
./scripts/heaptrack-overlay.sh launcher 50 ./result/bin/launcher   # use an existing build
```

Env knobs (all optional):

| var | meaning | default |
| --- | --- | --- |
| `SHOW_SLEEP` | dwell after SIGUSR1 so the surface maps + renders | `0.30` |
| `HIDE_SLEEP` | dwell after Escape so the surface tears down | `0.20` |
| `RSS_EVERY` | sample VmRSS every K cycles | `10` |
| `HYPR_TIMEOUT` | seconds to wait for nested Hyprland to come up | `30` |
| `NESTED_WD` | nested compositor's `WAYLAND_DISPLAY` name | `wayland-99` |
| `PROJECT_DIR` | path to the launcher checkout (for `nix build`) | `$HOME/dev/launcher` |

### What it does, step by step

1. Re-execs inside `nix shell` to provision the tools.
2. Records the **caller's** live `HYPRLAND_INSTANCE_SIGNATURE` so it can later
   assert the nested one differs (so it can never signal your real daemon).
3. Builds the target (`nix build $PROJECT_DIR`) or uses the path you passed.
4. Creates an isolated `XDG_RUNTIME_DIR` (a `mktemp -d`) and a minimal generated
   `hyprland.conf` (one fake `HEADLESS-1` 1920x1080 monitor, no bars, no
   autostart, no animations).
5. Launches `Hyprland` headless on `wayland-99` in that runtime, waits for its
   IPC socket, and discovers its instance signature from the single
   `hypr/<sig>/` dir under the isolated runtime.
6. **Hard safety assertion**: aborts if the nested signature equals the caller's.
7. Launches the target **under heaptrack**, with `XDG_RUNTIME_DIR` /
   `WAYLAND_DISPLAY` / `HYPRLAND_INSTANCE_SIGNATURE` all pointed at the nest, so
   the daemon connects to the nested compositor. Resolves the daemon's real pid
   as heaptrack's descendant and re-verifies (via `/proc/<pid>/environ`) that it
   is bound to the nested signature.
8. Drives `cycles` iterations of **SIGUSR1 (show, by pid) → `SHOW_SLEEP` →
   `wtype -k Escape` into the nest (dismiss) → `HIDE_SLEEP`**, sampling VmRSS
   every `RSS_EVERY` cycles to a log so growth is visible live.
9. Stops the daemon (so heaptrack flushes), tears down the nested compositor,
   and prints the `.zst` path plus the analysis commands.

### What it measures, and how to read the output

- **Live RSS trend** (`<runtime>/rss.log`, also echoed to the terminal): the
  fastest signal. If `VmRSS` climbs steadily across cycles and does not plateau,
  that *is* the leak reproducing. A flat line means this workload didn't trigger
  it — bump the cycle count, or vary the query (see fragile bits).
- **heaptrack trace** (`heaptrack.<target>.<pid>.zst`): the diagnosis. Analyze:

  ```
  nix shell nixpkgs#heaptrack -c heaptrack_print --print-leaks <trace>.zst | less
  nix shell nixpkgs#heaptrack -c heaptrack_gui   <trace>.zst    # flamegraph + timeline
  ```

  Look for allocation sites whose **leaked** bytes scale with the cycle count —
  those are the per-show accumulation. Because the icon cache is known bounded, a
  leak that tracks cycles and shows up under wgpu/egui/texture frames in
  `src/layer.rs` is the prime suspect (e.g. a wgpu device/surface/texture not
  dropped on unmap, or an egui `Context`/`TextureHandle` that outlives the pop-up
  it was built for).

The script leaves the whole nested runtime dir in place on exit (path printed at
the end) so the `.zst`, the Hyprland log (`hypr.log`), the daemon log
(`daemon.log`), and the RSS log are all available for post-mortem. Delete it
yourself when done.

### Known-fragile bits (tweak on first real run)

- **Headless Hyprland startup.** Whether wlroots' headless backend comes up
  cleanly on a given box (GPU/DRM access, `seatd`/`logind`, software-rendering
  fallback) is the single most likely failure. If the script dies with "timed
  out waiting for nested Hyprland IPC", read the printed `hypr.log`. Common
  fixes: ensure a DRM render node is accessible, or set
  `WLR_RENDERER=pixman` (software renderer) in the environment before running, or
  raise `HYPR_TIMEOUT`. The harness already sets `WLR_LIBINPUT_NO_DEVICES=1` so
  it doesn't demand real input devices.
- **Timing knobs `SHOW_SLEEP` / `HIDE_SLEEP`.** These must be long enough for the
  surface to actually map and render (and tear down) each cycle, but short
  enough that 200+ cycles don't take forever. If RSS stays suspiciously flat, the
  surface may not be fully cycling — raise `SHOW_SLEEP` first. If the run is slow,
  lower both. They are deliberately conservative defaults.
- **Escape delivery.** Dismiss is driven two ways: `wtype -k Escape` into the
  nested display (primary) and an `ESCAPE → killactive` bind in the generated
  config (fallback). The overlay also self-dismisses on focus loss. If a surface
  ever fails to dismiss (it would stay mapped and the next SIGUSR1 is a no-op),
  the RSS log will flatten — check `daemon.log`. `wtype` needs the virtual-
  keyboard protocol, which wlroots/Hyprland provides; if it's rejected, fall back
  to the bind-only path or drive Escape via
  `hyprctl --instance <sig> dispatch sendshortcut`.
- **Monitor size.** The generated config hard-codes a 1920x1080 `HEADLESS-1`
  output. The overlay reads this via `hyprctl monitors -j` to place/size its
  surface; if you want to profile at your real resolution/scale, edit the
  `monitor =` line in the heredoc.
- **Daemon pid resolution.** The script finds the daemon as heaptrack's
  descendant via `/proc/<pid>/stat` ppid walking, explicitly excluding any
  same-named live daemon. If heaptrack's process model changes (e.g. a wrapper
  layer), the pid walk may need adjusting — it currently tolerates intermediate
  processes between heaptrack and the daemon.

### Safety

The harness **never** uses `pkill -USR1 <name>`; every show is `kill -USR1
<pid>` scoped to the daemon it spawned, and it asserts (twice) that this pid is
bound to the nested instance signature and not the caller's. It will refuse to
run if it cannot obtain an isolated signature. Your live launcher/clipboard
daemon is not touched.
