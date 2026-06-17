//! Shared wlr-layer-shell harness for the launcher and clipboard overlays.
//!
//! Both surfaces are persistent daemons that idle invisibly and pop up on
//! demand. Under the previous design they were ordinary eframe windows forced
//! onto a per-app Hyprland special workspace by externally-injected window
//! rules, shown/hidden by toggling that workspace and resized via `hyprctl`.
//! Here they are real `wlr_layer_shell` surfaces instead: an overlay-layer
//! surface, anchored to the top edge and horizontally centered, with an
//! exclusive keyboard grab so it focuses on map like a proper launcher. The
//! gold border and rounded corners that the Hyprland rules used to paint are
//! now drawn by the app (see `common::popup_border`), and dynamic height is a
//! plain `set_size` + commit on the layer surface — the top anchor keeps the
//! input row pinned as the list grows, replacing `hyprland::resize_anchored`.
//!
//! Show/hide is surface map/unmap, not a workspace toggle: the surface is
//! created when the daemon is asked to appear (SIGUSR1) and destroyed when it
//! dismisses (Escape, activation, or focus loss). With no special workspace in
//! the picture the old toggle keybind no longer applies — the trigger becomes
//! `pkill -USR1 <binary>`.
//!
//! The egui `Context` is rebuilt with each surface, so any `TextureHandle`s an
//! app caches belong to one pop-up only; apps clear those caches in
//! `on_hidden`. Within a single pop-up the caches still dedup the many reloads
//! a visible session triggers, which is what bounded GPU memory in the first
//! place.

use crate::common;
use crate::hyprland;
use egui::Context;
use smithay_client_toolkit::reexports::client::Proxy;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer};
use std::sync::atomic::{AtomicBool, Ordering};
use wayapp::{Application, EguiAppData, EguiGpu, EguiSurfaceState, WaylandEvent};

/// Vertical position of the surface's top edge, as a fraction of the monitor
/// height. Matches the old `move = monitor_h*0.236` window rule and
/// `common::Y_ANCHOR_RATIO`, so the input row lands in the same place.
const TOP_MARGIN_RATIO: f32 = common::Y_ANCHOR_RATIO;

/// What an overlay app (launcher/clipboard) provides to the harness.
///
/// `update_ui` runs each egui frame: it draws the surface and returns the pixel
/// height the content wants. The harness diffs that against the current surface
/// height and issues a `set_size` when it changes, reproducing the per-keystroke
/// auto-grow the apps used to drive through `resize_anchored`. Setting the
/// returned `should_hide` (or losing keyboard focus) dismisses the surface.
pub trait LayerApp {
    /// Setup that needs an egui context (fonts, styles). Runs on the first
    /// frame of each pop-up, since the context is rebuilt per surface.
    fn on_frame_init(&mut self, _ctx: &Context) {}

    /// Called once per pop-up when the surface gains keyboard focus, mirroring
    /// the eframe "focus gain" reload hook.
    fn on_show(&mut self, _ctx: &Context) {}

    /// Draw a frame. Returns `(desired_height, should_hide)`: the wanted total
    /// surface height in logical pixels, and whether to dismiss now.
    fn update_ui(&mut self, ctx: &Context) -> (f32, bool);

    /// Reset transient UI state (query, selection) and drop any
    /// context-bound texture caches, the way `hide_and_reset` used to.
    fn on_hidden(&mut self) {}

    /// Fixed surface width in logical pixels.
    fn width(&self) -> u32;

    /// Initial surface height in logical pixels before the first auto-resize.
    fn init_height(&self) -> u32;
}

/// `EguiAppData` shim bridging the wayapp renderer's `ui()` callback to a
/// `LayerApp`. Carries the per-frame results back out to the event loop and
/// fires the one-shot init/show hooks when their flags are set.
struct UiBridge<'a, A: LayerApp> {
    app: &'a mut A,
    desired_height: f32,
    should_hide: bool,
    /// Whether `on_frame_init` still needs to run (carried in and out so the
    /// hook fires exactly once on the first frame that actually renders, not on
    /// earlier no-op event passes before the surface is configured).
    needs_frame_init: bool,
    /// Whether `on_show` still needs to run (focus was gained but the show hook
    /// hasn't fired yet).
    needs_show: bool,
    /// Set true when `ui()` actually ran this pass, so the loop only advances
    /// the one-shot flags on a real render.
    rendered: bool,
}

impl<A: LayerApp> EguiAppData for UiBridge<'_, A> {
    fn ui(&mut self, ctx: &Context) {
        self.rendered = true;
        if self.needs_frame_init {
            self.needs_frame_init = false;
            self.app.on_frame_init(ctx);
        }
        if self.needs_show {
            self.needs_show = false;
            self.app.on_show(ctx);
        }
        let (h, hide) = self.app.update_ui(ctx);
        self.desired_height = h;
        self.should_hide = hide;
    }
}

static SHOW_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sigusr1(_sig: i32) {
    SHOW_REQUESTED.store(true, Ordering::SeqCst);
}

/// Install the SIGUSR1 -> "show" handler. The daemon idles between pop-ups;
/// SIGUSR1 (from the show keybind, `pkill -USR1 <binary>`) requests it to
/// appear. The handler only does an atomic store, which is async-signal-safe.
fn install_show_signal() {
    SHOW_REQUESTED.store(false, Ordering::SeqCst);
    // SAFETY: registering a handler that performs only an atomic store.
    unsafe {
        signal(SIGUSR1, handle_sigusr1);
    }
}

// Minimal libc bindings. The only libc needs here are `signal` and `poll`;
// pulling in the whole `libc` crate for two FFI calls would be its sole use.
unsafe extern "C" {
    fn signal(signum: i32, handler: extern "C" fn(i32)) -> usize;
    fn poll(fds: *mut PollFd, nfds: u64, timeout: i32) -> i32;
}
const SIGUSR1: i32 = 10;

#[repr(C)]
struct PollFd {
    fd: i32,
    events: i16,
    revents: i16,
}
const POLLIN: i16 = 0x001;

/// Run the overlay daemon event loop forever.
///
/// Idles until SIGUSR1, then maps the layer surface, pumps Wayland + egui until
/// the app dismisses or keyboard focus is lost, tears the surface down, and
/// returns to idle. The process never exits on its own — it is a
/// `Restart=on-failure` systemd unit, same as before.
pub fn run<A: LayerApp>(namespace: &str, mut app: A) -> ! {
    let mut wl = Application::new();
    install_show_signal();

    // Persistent GPU state, built once on the first pop-up and reused for every
    // later one. Recreating the wgpu instance/adapter/device on each show is the
    // dominant toggle cost; persisting it here is what keeps the device alive
    // between pop-ups the way the old single persistent window did.
    let mut gpu: Option<EguiGpu> = None;

    loop {
        // Idle: poll the Wayland fd with a 100 ms cap so the show flag is seen
        // promptly without a busy loop. Keeps the connection serviced so the
        // compositor doesn't consider us unresponsive.
        while !SHOW_REQUESTED.load(Ordering::SeqCst) {
            dispatch_with_timeout(&mut wl, 100);
        }
        SHOW_REQUESTED.store(false, Ordering::SeqCst);

        show_once(&mut wl, &mut gpu, namespace, &mut app);
        app.on_hidden();
    }
}

/// Map the overlay, run it until dismissed, then destroy it.
fn show_once<A: LayerApp>(
    wl: &mut Application,
    gpu: &mut Option<EguiGpu>,
    namespace: &str,
    app: &mut A,
) {
    let (_mon_w, mon_h) = hyprland::monitor_logical_size();
    let width = app.width();
    let init_h = app.init_height();
    let top_margin = (mon_h * TOP_MARGIN_RATIO).round() as i32;

    let surface = wl.compositor_state.create_surface(&wl.qh);
    let layer = wl.layer_shell.create_layer_surface(
        &wl.qh,
        surface,
        Layer::Overlay,
        Some(namespace),
        None,
    );
    // Anchor TOP only: the surface stays horizontally centered on the free axis
    // and its top edge is pinned, so height changes grow downward — the
    // quake-style behavior the old `resize_anchored` produced.
    layer.set_anchor(Anchor::TOP);
    layer.set_margin(top_margin, 0, 0, 0);
    layer.set_size(width, init_h);
    // Grab the keyboard so typing lands here and Escape works, like a real
    // launcher. No exclusive zone: the overlay floats over windows and must not
    // reserve screen space.
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    layer.set_exclusive_zone(0);
    layer.commit();

    // Build the persistent GPU state on the first pop-up (it needs a live
    // surface to select the adapter against), then reuse it for every later
    // pop-up so only the cheap swapchain surface and egui context are recreated.
    let gpu_ref = gpu.get_or_insert_with(|| EguiGpu::new(wl, layer.wl_surface()));
    let mut egui_surface = EguiSurfaceState::new_with_gpu(wl, gpu_ref, &layer, width, init_h);

    let mut has_focus = false;
    let mut ever_focused = false;
    // One-shot hooks, advanced only when a frame actually renders.
    let mut frame_init_pending = true;
    let mut show_pending = true;
    let mut last_height = 0u32;

    loop {
        dispatch_with_timeout(wl, 16);
        let events = wl.take_wayland_events();

        for ev in &events {
            match ev {
                WaylandEvent::KeyboardEnter(s, _, _) if s.id() == layer.wl_surface().id() => {
                    ever_focused = true;
                    has_focus = true;
                }
                WaylandEvent::KeyboardLeave(s) if s.id() == layer.wl_surface().id() => {
                    has_focus = false;
                }
                _ => {}
            }
        }

        let mut bridge = UiBridge {
            app,
            desired_height: last_height as f32,
            should_hide: false,
            needs_frame_init: frame_init_pending,
            // Fire `on_show` on the first render after focus is gained.
            needs_show: show_pending && ever_focused,
            rendered: false,
        };
        egui_surface.handle_events(wl, &events, &mut bridge);

        // Advance the one-shots only if `ui()` ran this pass.
        if bridge.rendered {
            frame_init_pending = bridge.needs_frame_init;
            if ever_focused {
                show_pending = bridge.needs_show;
            }
        }

        let desired = bridge.desired_height.round().max(1.0) as u32;
        // Auto-resize: ask the compositor for the new height when it changes by
        // more than a pixel — the layer-shell analogue of `resize_anchored`. The
        // resulting configure event re-renders at the new size.
        if desired.abs_diff(last_height) > 1 {
            last_height = desired;
            layer.set_size(width, desired);
            layer.commit();
        }

        if bridge.should_hide {
            break;
        }
        // Dismiss on focus loss (click-outside), but only after the surface has
        // actually been focused once — it may render a frame before the enter.
        if ever_focused && !has_focus {
            break;
        }

        // Request the next frame callback only when egui is actually animating
        // (key repeat, scroll momentum, the delete fade) or a widget asked to
        // be repainted. The compositor then drives the next render at its own
        // vsync rate via WaylandEvent::Frame. Previously this fired every loop
        // iteration unconditionally, committing a fresh frame callback ~60+
        // times a second even at rest; that flood of commits is what made
        // input and rendering lag. Input/configure events still wake the loop
        // and render on their own, so static frames need no polling.
        if egui_surface.wants_repaint() {
            egui_surface.request_frame();
        }
    }

    // Destroying the LayerSurface unmaps the overlay. The egui Context inside
    // `egui_surface` is dropped with it; the app clears its caches in on_hidden.
    drop(egui_surface);
    drop(layer);
    let _ = wl.conn.flush();
}

/// Block on Wayland traffic for at most `timeout_ms`, returning early when the
/// socket is readable. Avoids a busy loop while bounding event latency.
fn dispatch_with_timeout(wl: &mut Application, timeout_ms: u64) {
    use std::os::fd::AsRawFd;

    let _ = wl.conn.flush();
    let Some(mut queue) = wl.event_queue.take() else {
        return;
    };

    let _ = queue.dispatch_pending(wl);

    if let Some(guard) = queue.prepare_read() {
        let fd = guard.connection_fd().as_raw_fd();
        let mut pfd = PollFd { fd, events: POLLIN, revents: 0 };
        // SAFETY: poll over one valid pollfd; timeout in ms. A non-positive
        // return (timeout, or EINTR from our own SIGUSR1) drops the guard
        // unread — no events are lost, the next pass re-prepares.
        let ready = unsafe { poll(&mut pfd as *mut PollFd, 1, timeout_ms as i32) };
        if ready > 0 {
            let _ = guard.read();
            let _ = queue.dispatch_pending(wl);
        }
    }

    wl.event_queue = Some(queue);
}
