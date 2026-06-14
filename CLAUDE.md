# Launcher

Wayland app launcher + clipboard manager for Hyprland, built with Rust/egui.

## Architecture

```
src/
  launcher.rs    # App launcher binary (special workspace overlay)
  clipboard.rs   # Clipboard UI binary (special workspace overlay)
  clipd.rs       # Clipboard daemon binary (watcher + DB)
  common.rs      # Shared UI: input panel, virtual list, colors, highlighting
  hyprland.rs    # Hyprland IPC: monitors, clients, dispatch, events
  desktop.rs     # XDG desktop entry parsing
  scroll.rs      # Scroll momentum physics
  clip/          # Clipboard backend module (used by clipd + clipboard UI)
    mod.rs        # Public API: ClipboardDb
    db.rs         # SQLite schema, migrations, queries
    watcher.rs    # Wayland clipboard watcher (wlr-data-control via wl-clipboard-rs)
    mime.rs       # MIME detection from magic bytes
  lib.rs         # Library root (pub mod common, desktop, hyprland, scroll, clip)
```

Three binaries:
- `launcher` — app launcher UI
- `clipboard` — clipboard manager UI (reads from clipd's SQLite DB)
- `clipd` — clipboard daemon (watches Wayland clipboard, writes to SQLite DB)

## Clipboard Backend (`src/clip/`)

See `src/clip/CLAUDE.md` for full design doc. Replaces cliphist with native Rust daemon + SQLite.

## UI conventions

- Golden ratio (1.618) proportions throughout
- Shared input panel with `>` prompt, Ctrl+U clear, ghost text completion
- Row dimming: selected = full brightness, unselected = 0.5 opacity (TEXT_SECONDARY)
- Match highlighting: underline + TEXT_PRIMARY color on matched chars
- Virtual list for performance, custom painter-based row rendering
- Hyprland special workspaces for overlay behavior
- Dynamic window resize via `resizewindowpixel` dispatch
