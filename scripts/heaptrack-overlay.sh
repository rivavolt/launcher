#!/usr/bin/env bash
#
# heaptrack-overlay.sh — memory-profiling harness for the launcher/clipboard
# wlr-layer-shell overlay daemons.
#
# WHY THIS EXISTS
# ---------------
# The overlay daemon (src/launcher.rs / src/clipboard.rs via src/layer.rs) idles
# until SIGUSR1, maps a layer surface, renders with egui-on-wgpu, and tears the
# surface down on dismiss. It once leaked unbounded — ~8 GB over ~2 days of real
# use — which swap-thrashed a 16 GB laptop into a halt (that is why the systemd
# units now carry MemoryMax). Static analysis showed the icon cache is bounded,
# so the suspect is the GPU/wgpu/egui-texture lifecycle that only runs *while the
# surface is mapped and rendering*. That code path needs a real compositor and
# many show/hide cycles to surface the leak — exactly what this harness creates.
#
# The trick: the overlays are Hyprland-coupled. They shell out to `hyprctl`
# (client list, dispatch, monitor size) and read HYPRLAND_INSTANCE_SIGNATURE, so
# they will NOT run under a plain cage/sway nest. This harness therefore spins a
# *nested, headless* Hyprland (wlroots headless backend) on its own
# WAYLAND_DISPLAY and its own XDG_RUNTIME_DIR, runs the target daemon inside it
# under heaptrack, and drives it through N show/hide cycles while sampling RSS.
# Nothing lands on your real screen and your live Hyprland is never touched.
#
# USAGE
# -----
#   ./scripts/heaptrack-overlay.sh [launcher|clipboard] [cycles] [binary-path]
#
#   $1  target binary name: launcher (default) or clipboard
#   $2  number of show/hide cycles (default 200)
#   $3  optional explicit path to the binary (skips `nix build`)
#
# Examples:
#   ./scripts/heaptrack-overlay.sh                      # launcher, 200 cycles
#   ./scripts/heaptrack-overlay.sh clipboard 500        # clipboard, 500 cycles
#   ./scripts/heaptrack-overlay.sh launcher 50 ./result/bin/launcher
#
# OUTPUT
# ------
# A heaptrack.<binary>.<pid>.zst file (path printed at the end) plus a live RSS
# log so you can watch growth before heaptrack is even analyzed. To read it:
#   heaptrack_print --print-leaks heaptrack.launcher.NNNN.zst | less
#   heaptrack_gui  heaptrack.launcher.NNNN.zst              # flamegraph UI
#
# See scripts/README.md for prerequisites, what it measures, and the fragile
# knobs to tweak on the first real run.

set -euo pipefail

# ----------------------------------------------------------------------------
# Config knobs (env-overridable; the timing ones are the most likely to need a
# tweak on first run — see README "Fragile bits").
# ----------------------------------------------------------------------------
TARGET="${1:-launcher}"          # launcher | clipboard
CYCLES="${2:-200}"               # how many show/hide iterations
BIN_ARG="${3:-}"                 # optional explicit binary path

NESTED_WD="${NESTED_WD:-wayland-99}"   # nested compositor's WAYLAND_DISPLAY name
SHOW_SLEEP="${SHOW_SLEEP:-0.30}"       # dwell after SIGUSR1 so the surface maps + renders a few frames
HIDE_SLEEP="${HIDE_SLEEP:-0.20}"       # dwell after Escape so the surface tears down
RSS_EVERY="${RSS_EVERY:-10}"           # sample VmRSS every K cycles
HYPR_TIMEOUT="${HYPR_TIMEOUT:-30}"     # seconds to wait for nested Hyprland to come up
PROJECT_DIR="${PROJECT_DIR:-$HOME/dev/launcher}"

# nixpkgs attrs are VERIFIED to exist: heaptrack-1.5.0, hyprland-0.54.2, wtype-0.4.
NIX_TOOLS=(nixpkgs#heaptrack nixpkgs#hyprland nixpkgs#wtype)

case "$TARGET" in
  launcher|clipboard) ;;
  *) echo "FATAL: target must be 'launcher' or 'clipboard', got '$TARGET'" >&2; exit 2 ;;
esac

log() { printf '[heaptrack-overlay] %s\n' "$*" >&2; }
die() { printf '[heaptrack-overlay] FATAL: %s\n' "$*" >&2; exit 1; }

# ----------------------------------------------------------------------------
# Re-exec inside `nix shell` so heaptrack / Hyprland / wtype are on PATH. The
# guard env var stops an infinite re-exec loop. Everything below the guard runs
# with the tools provisioned.
# ----------------------------------------------------------------------------
if [[ -z "${_HEAPTRACK_OVERLAY_PROVISIONED:-}" ]]; then
  log "provisioning tools via nix shell: ${NIX_TOOLS[*]}"
  export _HEAPTRACK_OVERLAY_PROVISIONED=1
  exec nix shell "${NIX_TOOLS[@]}" -c "$0" "$@"
fi

# Sanity: tools really are present now.
for t in heaptrack Hyprland wtype hyprctl; do
  command -v "$t" >/dev/null 2>&1 || die "'$t' not on PATH after nix shell (attr name drift?)"
done

# ----------------------------------------------------------------------------
# Guard: capture the CALLER's live Hyprland signature up front. We will later
# assert the nested daemon's signature differs, so we can never, ever signal the
# user's real launcher/clipboard.
# ----------------------------------------------------------------------------
CALLER_SIG="${HYPRLAND_INSTANCE_SIGNATURE:-<none>}"
log "caller's live Hyprland signature: $CALLER_SIG"

# ----------------------------------------------------------------------------
# Resolve the target binary. Prefer an explicit path arg; else build with nix
# and use ./result/bin/<target>.
# ----------------------------------------------------------------------------
if [[ -n "$BIN_ARG" ]]; then
  BIN="$BIN_ARG"
  [[ -x "$BIN" ]] || die "binary '$BIN' is not executable"
else
  log "building target via: nix build $PROJECT_DIR (--no-link keeps the tree clean)"
  OUT_PATH="$(nix build "$PROJECT_DIR" --no-link --print-out-paths)" \
    || die "nix build failed"
  [[ -n "$OUT_PATH" ]] || die "nix build produced no out path"
  BIN="$OUT_PATH/bin/$TARGET"
  [[ -x "$BIN" ]] || die "built store path has no executable bin/$TARGET ($BIN)"
fi
log "target binary: $BIN"

# ----------------------------------------------------------------------------
# Isolated runtime for the nested compositor. Its OWN XDG_RUNTIME_DIR means
# (a) a fresh, collision-free Wayland socket and (b) exactly one hypr/<sig>
# directory to discover, so signature detection is unambiguous and we never
# read the user's live socket tree.
# ----------------------------------------------------------------------------
NESTED_RUNTIME="$(mktemp -d "${TMPDIR:-/tmp}/heaptrack-overlay.XXXXXX")"
chmod 700 "$NESTED_RUNTIME"
HEAP_OUT_DIR="$NESTED_RUNTIME/heaptrack"
mkdir -p "$HEAP_OUT_DIR"

# Minimal Hyprland config: no bars, no autostart, no animations. Just a single
# headless monitor sized like a typical laptop so the overlay's hyprctl monitor
# query returns sane dimensions, plus an Escape bind that we will ALSO drive via
# wtype (belt and suspenders). `misc:disable_hyprland_logo`/`disable_splash`
# keep the surface clean; `debug:disable_logs=false` keeps the log for triage.
HYPR_CONF="$NESTED_RUNTIME/hyprland.conf"
cat >"$HYPR_CONF" <<'EOF'
# Auto-generated by heaptrack-overlay.sh — minimal nested headless instance.
# A fake headless monitor so `hyprctl monitors -j` reports real dimensions to
# the overlay (it divides by scale to place the surface). WLR_BACKENDS=headless
# (set in the env) makes wlroots create no real outputs; this monitor keyword
# names/sizes the headless one.
monitor = HEADLESS-1, 1920x1080@60, 0x0, 1

# Strip everything that would autostart a bar / wallpaper / portal / IME — we
# host exactly one layer-shell client and nothing else.
exec-once =

# No animations: deterministic timing, less GPU churn unrelated to the leak.
animations {
    enabled = false
}

misc {
    disable_hyprland_logo = true
    disable_splash_rendering = true
    force_default_wallpaper = 0
    # Don't let an idle/unfocused headless session do anything surprising.
    vfr = true
}

# Escape dismisses (the overlay also self-dismisses on Escape via its own key
# handling; this bind is a fallback path that asks Hyprland to nudge focus).
bind = , ESCAPE, killactive
EOF

# ----------------------------------------------------------------------------
# State for the trap. Populated as we go; the trap tolerates unset/dead pids.
# ----------------------------------------------------------------------------
HYPR_PID=""
HEAPTRACK_PID=""
DAEMON_PID=""
NESTED_SIG=""

_CLEANED=""
cleanup() {
  local rc=$?
  # One-shot: INT/TERM run cleanup then fall through to EXIT, which would run it
  # again. Guard so teardown + the "kept for inspection" notice happen once.
  [[ -n "$_CLEANED" ]] && return
  _CLEANED=1
  set +e
  log "cleaning up (exit code $rc)…"

  # Kill the heaptracked daemon first so heaptrack flushes its .zst. Scope every
  # kill to pids WE spawned inside the nested runtime — never a pkill by name.
  if [[ -n "$DAEMON_PID" ]] && kill -0 "$DAEMON_PID" 2>/dev/null; then
    log "stopping daemon pid $DAEMON_PID"
    kill -TERM "$DAEMON_PID" 2>/dev/null
  fi
  if [[ -n "$HEAPTRACK_PID" ]] && kill -0 "$HEAPTRACK_PID" 2>/dev/null; then
    # Give heaptrack a moment to write out the trace before it dies.
    for _ in 1 2 3 4 5 6 7 8 9 10; do
      kill -0 "$HEAPTRACK_PID" 2>/dev/null || break
      sleep 0.3
    done
    kill -TERM "$HEAPTRACK_PID" 2>/dev/null
  fi

  # Tear down the nested compositor.
  if [[ -n "$NESTED_SIG" ]]; then
    hyprctl --instance "$NESTED_SIG" dispatch exit >/dev/null 2>&1
  fi
  if [[ -n "$HYPR_PID" ]] && kill -0 "$HYPR_PID" 2>/dev/null; then
    kill -TERM "$HYPR_PID" 2>/dev/null
    for _ in 1 2 3 4 5; do
      kill -0 "$HYPR_PID" 2>/dev/null || break
      sleep 0.3
    done
    kill -KILL "$HYPR_PID" 2>/dev/null
  fi

  # Surface the trace path even on failure, then leave the runtime dir (it holds
  # the .zst + the Hyprland log) for post-mortem. Print where it is.
  if [[ -d "$HEAP_OUT_DIR" ]]; then
    local zsts=( "$HEAP_OUT_DIR"/heaptrack.*.zst )
    if [[ -e "${zsts[0]}" ]]; then
      log "heaptrack trace: ${zsts[0]}"
    fi
  fi
  log "nested runtime kept for inspection: $NESTED_RUNTIME"
  log "  (Hyprland log: $NESTED_RUNTIME/hypr.log)"
}
trap cleanup EXIT INT TERM

# ----------------------------------------------------------------------------
# 1) Launch the nested headless Hyprland.
#    WLR_BACKENDS=headless          → wlroots/Aquamarine creates no real output.
#    WLR_LIBINPUT_NO_DEVICES=1      → don't require real input devices.
#    XDG_RUNTIME_DIR=<isolated>     → fresh socket tree, unambiguous signature.
#    WAYLAND_DISPLAY=<nested>       → the socket name the daemon will connect to.
#    Unset HYPRLAND_INSTANCE_SIGNATURE so Hyprland mints a fresh one rather than
#    inheriting the caller's.
# ----------------------------------------------------------------------------
log "starting nested headless Hyprland (display=$NESTED_WD, runtime=$NESTED_RUNTIME)…"
# `--socket "$NESTED_WD"` pins the Wayland socket name explicitly (verified flag)
# so the daemon's connect_to_env() finds exactly $XDG_RUNTIME_DIR/$NESTED_WD,
# regardless of whether this wlroots build would otherwise honor the preset
# WAYLAND_DISPLAY or auto-pick wayland-1.
env -u HYPRLAND_INSTANCE_SIGNATURE \
    WLR_BACKENDS=headless \
    WLR_LIBINPUT_NO_DEVICES=1 \
    XDG_RUNTIME_DIR="$NESTED_RUNTIME" \
    WAYLAND_DISPLAY="$NESTED_WD" \
    Hyprland --config "$HYPR_CONF" --socket "$NESTED_WD" >"$NESTED_RUNTIME/hypr.log" 2>&1 &
HYPR_PID=$!

# ----------------------------------------------------------------------------
# Wait for it to come up and discover its instance signature. With the isolated
# runtime there is exactly one hypr/<sig> dir; the .socket.sock inside it means
# the IPC is live. Fail loudly on timeout.
# ----------------------------------------------------------------------------
log "waiting up to ${HYPR_TIMEOUT}s for nested Hyprland IPC…"
deadline=$(( SECONDS + HYPR_TIMEOUT ))
while :; do
  if ! kill -0 "$HYPR_PID" 2>/dev/null; then
    log "----- nested Hyprland log tail -----"
    tail -n 40 "$NESTED_RUNTIME/hypr.log" >&2 || true
    die "nested Hyprland exited before coming up (see log above)"
  fi
  # The signature dir appears under the nested runtime once Hyprland boots.
  for d in "$NESTED_RUNTIME"/hypr/*/; do
    [[ -e "$d/.socket.sock" ]] || continue
    NESTED_SIG="$(basename "$d")"
    break
  done
  if [[ -n "$NESTED_SIG" ]]; then
    # Confirm IPC actually answers, not just that the socket file exists.
    if XDG_RUNTIME_DIR="$NESTED_RUNTIME" hyprctl --instance "$NESTED_SIG" monitors -j >/dev/null 2>&1; then
      break
    fi
  fi
  (( SECONDS >= deadline )) && {
    log "----- nested Hyprland log tail -----"
    tail -n 40 "$NESTED_RUNTIME/hypr.log" >&2 || true
    die "timed out waiting for nested Hyprland IPC after ${HYPR_TIMEOUT}s"
  }
  sleep 0.25
done
log "nested Hyprland up, signature: $NESTED_SIG"

# ----------------------------------------------------------------------------
# HARD SAFETY ASSERTION: the nested signature MUST differ from the caller's live
# one. If for any reason they match (isolation failed), abort before we ever
# send a signal — signalling the user's real daemon is the one outcome we forbid.
# ----------------------------------------------------------------------------
if [[ "$NESTED_SIG" == "$CALLER_SIG" ]]; then
  die "nested signature equals caller's live signature ($CALLER_SIG) — isolation failed, refusing to run"
fi

# ----------------------------------------------------------------------------
# 2) Launch the target daemon INSIDE the nested instance, under heaptrack.
#    The daemon reads HYPRLAND_INSTANCE_SIGNATURE + XDG_RUNTIME_DIR to find the
#    hypr socket, and WAYLAND_DISPLAY (via Connection::connect_to_env) to bind
#    its layer surface — all pointed at the nested instance. We spawn heaptrack
#    directly (not via `hyprctl dispatch exec`) so heaptrack is the daemon's
#    parent and we own the pid for RSS sampling + scoped teardown.
#    `-o` fixes the trace into our runtime dir; heaptrack appends .<pid>.zst.
# ----------------------------------------------------------------------------
log "launching '$TARGET' under heaptrack inside nested instance…"
env XDG_RUNTIME_DIR="$NESTED_RUNTIME" \
    WAYLAND_DISPLAY="$NESTED_WD" \
    HYPRLAND_INSTANCE_SIGNATURE="$NESTED_SIG" \
    heaptrack -o "$HEAP_OUT_DIR/heaptrack.$TARGET" "$BIN" \
    >"$NESTED_RUNTIME/daemon.log" 2>&1 &
HEAPTRACK_PID=$!

# Read a pid's parent pid from /proc/<pid>/stat. The comm sits in parens as
# field 2 and may itself contain spaces/parens, so split on the LAST ')' rather
# than awk-ing $4. Prints the ppid (or nothing on failure).
ppid_of() {
  local stat_rest
  stat_rest="$(cat "/proc/$1/stat" 2>/dev/null)" || return 1
  stat_rest="${stat_rest##*) }"                 # drop "PID (comm) " → "state ppid …"
  stat_rest="${stat_rest#* }"                   # drop state → "ppid …"
  printf '%s' "${stat_rest%% *}"                # first remaining token = ppid
}

# Is $1 a descendant of HEAPTRACK_PID (walking the ppid chain)?
is_heaptrack_descendant() {
  local p="$1" hop=0 par
  while [[ -n "$p" && "$p" != "0" && "$p" != "1" && "$hop" -lt 32 ]]; do
    [[ "$p" == "$HEAPTRACK_PID" ]] && return 0
    par="$(ppid_of "$p")" || return 1
    [[ "$par" =~ ^[0-9]+$ ]] || return 1
    p="$par"; hop=$(( hop + 1 ))
  done
  return 1
}

# Find the actual daemon pid. The nix-built binary is a makeWrapper bash script
# that `exec -a "$0" …/.<name>-wrapped`s the real ELF, so the kernel comm is
# `.<name>-wrapped` (truncated to 15 chars) — NOT `<name>`. Matching comm is
# therefore fragile; instead enumerate every pid that descends from our heaptrack
# pid and pick the one whose cmdline argv[0] basename mentions the target. The
# descendant filter guarantees we never pick the user's same-named live daemon.
log "resolving daemon pid (descendant of heaptrack $HEAPTRACK_PID)…"
DAEMON_PID=""
for _ in $(seq 1 40); do
  if ! kill -0 "$HEAPTRACK_PID" 2>/dev/null; then
    log "----- heaptrack/daemon log tail -----"
    tail -n 40 "$NESTED_RUNTIME/daemon.log" >&2 || true
    die "heaptrack exited before the daemon started (see log above)"
  fi
  for pid_dir in /proc/[0-9]*; do
    cand="${pid_dir#/proc/}"
    [[ "$cand" == "$HEAPTRACK_PID" ]] && continue
    is_heaptrack_descendant "$cand" || continue
    # argv[0] basename should reference the target (wrapper keeps argv0 = the
    # path heaptrack was given, e.g. …/bin/launcher; the wrapped ELF inherits it
    # via `exec -a "$0"`). Fall back to a comm prefix match for safety.
    argv0="$(tr '\0' '\n' < "$pid_dir/cmdline" 2>/dev/null | head -n1)"
    base="${argv0##*/}"
    comm="$(cat "$pid_dir/comm" 2>/dev/null || true)"
    if [[ "$base" == *"$TARGET"* || "$comm" == *"$TARGET"* ]]; then
      DAEMON_PID="$cand"; break
    fi
  done
  [[ -n "$DAEMON_PID" ]] && break
  sleep 0.25
done
[[ -n "$DAEMON_PID" ]] || die "could not resolve the heaptracked daemon pid (check $NESTED_RUNTIME/daemon.log)"
log "daemon pid: $DAEMON_PID  (argv0=$(tr '\0' ' ' < "/proc/$DAEMON_PID/cmdline" 2>/dev/null | cut -c1-80))"

# Defence in depth: verify the daemon's own env actually carries the nested
# signature (proc/environ is NUL-separated). This catches a daemon that somehow
# inherited the caller's signature despite our env override.
DAEMON_SIG="$(tr '\0' '\n' < "/proc/$DAEMON_PID/environ" 2>/dev/null \
              | sed -n 's/^HYPRLAND_INSTANCE_SIGNATURE=//p' | head -n1)"
if [[ -z "$DAEMON_SIG" ]]; then
  log "WARNING: could not read daemon's HYPRLAND_INSTANCE_SIGNATURE from /proc (continuing; pid scoping still applies)"
elif [[ "$DAEMON_SIG" != "$NESTED_SIG" ]]; then
  die "daemon's signature ($DAEMON_SIG) is not the nested one ($NESTED_SIG) — refusing to drive it"
elif [[ "$DAEMON_SIG" == "$CALLER_SIG" ]]; then
  die "daemon is bound to the caller's live signature — refusing to drive it"
else
  log "verified daemon is bound to nested signature $DAEMON_SIG"
fi

# Give the daemon a beat to install its SIGUSR1 handler and finish first-run init.
sleep 1.0

# ----------------------------------------------------------------------------
# RSS sampler. Reads VmRSS straight from /proc/<pid>/status (kB).
# ----------------------------------------------------------------------------
rss_kb() {
  awk '/^VmRSS:/{print $2}' "/proc/$DAEMON_PID/status" 2>/dev/null || echo "?"
}
RSS_LOG="$NESTED_RUNTIME/rss.log"
: >"$RSS_LOG"
sample_rss() {
  local cyc="$1" kb ; kb="$(rss_kb)"
  printf 'cycle=%-6s VmRSS=%s kB (%s MB)\n' "$cyc" "$kb" \
    "$(awk -v k="$kb" 'BEGIN{ if (k ~ /^[0-9]+$/) printf "%.1f", k/1024; else print "?" }')" \
    | tee -a "$RSS_LOG" >&2
}

log "baseline RSS before any show:"; sample_rss 0

# ----------------------------------------------------------------------------
# 3) Drive N show/hide cycles. SIGUSR1 (scoped to OUR daemon pid) shows; Escape
#    (wtype into the nested instance) dismisses. Sample RSS every K cycles.
# ----------------------------------------------------------------------------
log "driving $CYCLES show/hide cycles (show_sleep=$SHOW_SLEEP hide_sleep=$HIDE_SLEEP)…"
for (( c=1; c<=CYCLES; c++ )); do
  # Bail out early if the daemon died (e.g. OOM-killed by its own MemoryMax,
  # which is itself a useful signal).
  if ! kill -0 "$DAEMON_PID" 2>/dev/null; then
    log "daemon pid $DAEMON_PID vanished at cycle $c — stopping (check daemon.log / RSS trend)"
    break
  fi

  # SHOW: signal strictly by pid (never `pkill -USR1 launcher`, which could hit
  # the user's live daemon). This is the same trigger the real show keybind uses.
  kill -USR1 "$DAEMON_PID" 2>/dev/null || true
  sleep "$SHOW_SLEEP"

  # DISMISS: send Escape into the nested compositor. wtype talks to the Wayland
  # display named by WAYLAND_DISPLAY/XDG_RUNTIME_DIR, so point it at the nest.
  env XDG_RUNTIME_DIR="$NESTED_RUNTIME" WAYLAND_DISPLAY="$NESTED_WD" \
    wtype -k Escape 2>/dev/null || true
  sleep "$HIDE_SLEEP"

  if (( c % RSS_EVERY == 0 )); then
    sample_rss "$c"
  fi
done

log "drive loop done; final RSS:"; sample_rss "$CYCLES"

# ----------------------------------------------------------------------------
# 4) Stop the daemon so heaptrack flushes, then report. The EXIT trap does the
#    actual teardown + path printing; here we just summarise next steps.
# ----------------------------------------------------------------------------
log "stopping daemon to flush heaptrack trace…"
kill -TERM "$DAEMON_PID" 2>/dev/null || true
# Wait for heaptrack to finish writing.
for _ in $(seq 1 40); do
  kill -0 "$HEAPTRACK_PID" 2>/dev/null || break
  sleep 0.25
done

ZST=""
_zsts=( "$HEAP_OUT_DIR"/heaptrack.*.zst )
[[ -e "${_zsts[0]}" ]] && ZST="${_zsts[0]}"

echo >&2
log "================ RESULTS ================"
if [[ -n "$ZST" ]]; then
  log "heaptrack trace : $ZST"
  log "RSS sample log  : $RSS_LOG"
  echo >&2
  log "Analyze the leak with (heaptrack is on PATH only inside the nix shell):"
  log "  nix shell nixpkgs#heaptrack -c heaptrack_print --print-leaks '$ZST' | less"
  log "  nix shell nixpkgs#heaptrack -c heaptrack_print --print-massif-stats '$ZST'"
  log "  nix shell nixpkgs#heaptrack -c heaptrack_gui '$ZST'   # flamegraph + time-line UI"
  echo >&2
  log "Look for allocations whose 'leaked' bytes scale with the cycle count —"
  log "that is the per-show accumulation. Cross-check against the RSS trend in"
  log "the sample log: a rising VmRSS with flat icon-cache usage points at the"
  log "wgpu/egui-texture lifecycle in src/layer.rs."
else
  log "WARNING: no heaptrack.*.zst found in $HEAP_OUT_DIR"
  log "Check $NESTED_RUNTIME/daemon.log and $NESTED_RUNTIME/hypr.log"
fi
log "========================================"

# Let the trap handle final teardown.
exit 0
