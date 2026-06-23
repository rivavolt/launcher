//! Hyprland IPC: monitor info, clients, dispatch, event subscription

use serde::Deserialize;
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::UnixStream;
use std::process::Command;
use std::{env, thread};

#[derive(Deserialize)]
pub struct Monitor {
    pub width: f64,
    pub height: f64,
    pub scale: f64,
}

#[derive(Deserialize)]
pub struct Workspace {
    pub id: i32,
    pub name: String,
}

#[derive(Deserialize)]
pub struct Client {
    pub address: String,
    pub title: String,
    pub class: String,
    pub workspace: Workspace,
    #[serde(rename = "focusHistoryID")]
    pub focus_history_id: i32,
    #[serde(default)]
    pub pinned: bool,
}

/// Send a request to Hyprland's IPC request socket (`.socket.sock`) and return
/// the raw response. This is exactly what `hyprctl` does internally, minus the
/// process fork+exec — a direct socket round-trip is ~1 ms versus ~10 ms to spawn
/// `hyprctl`, which matters because the monitor and client queries sit on the
/// launcher's show/toggle critical path. `cmd` is the wire form, e.g.
/// `j/monitors` or `j/clients` (the `j/` prefix asks for JSON).
fn request(cmd: &str) -> Option<String> {
    let sig = env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    let runtime = env::var("XDG_RUNTIME_DIR").unwrap_or("/tmp".into());
    let path = format!("{}/hypr/{}/.socket.sock", runtime, sig);
    let mut stream = UnixStream::connect(path).ok()?;
    stream.write_all(cmd.as_bytes()).ok()?;
    let mut buf = String::new();
    stream.read_to_string(&mut buf).ok()?;
    Some(buf)
}

pub fn monitor() -> Option<Monitor> {
    let resp = request("j/monitors")?;
    let monitors: Vec<Monitor> = serde_json::from_str(&resp).ok()?;
    monitors.into_iter().next()
}

/// Calculate eframe viewport-builder size for given width/height ratios.
/// Divides by 2.0 because eframe applies internal 2x HiDPI scaling on Wayland.
/// Note: egui's screen_rect/cursor coords match hyprland logical pixels (no /2).
pub fn window_size(w_ratio: f32, h_ratio: f32, fallback: (f32, f32)) -> (f32, f32) {
    monitor()
        .map(|m| {
            let w = m.width / m.scale * w_ratio as f64 / 2.0;
            let h = m.height / m.scale * h_ratio as f64 / 2.0;
            (w as f32, h as f32)
        })
        .unwrap_or(fallback)
}

/// Get the active monitor's logical (post-scale) dimensions, matching the
/// coordinate space used by `hl.dsp.window.move/resize`. Falls back to (0, 0).
pub fn monitor_logical_size() -> (f32, f32) {
    monitor()
        .map(|m| ((m.width / m.scale) as f32, (m.height / m.scale) as f32))
        .unwrap_or((0.0, 0.0))
}

/// Resize a quake-style overlay and pin its top edge to a fixed Y, so the
/// input field stays put as the result list grows/shrinks. Horizontal
/// position stays centered. Runs as one hyprctl batch so resize+move land
/// atomically (no flicker / no transient recentering).
pub fn resize_anchored(class: &str, width: i32, height: i32, monitor_w: f32, monitor_h: f32, y_ratio: f32) {
    let x = ((monitor_w - width as f32) * 0.5).round() as i32;
    let y = (monitor_h * y_ratio).round() as i32;
    dispatch_batch_async(&[
        format!(r#"hl.dsp.window.resize({{ x = {width}, y = {height}, window = "class:{class}" }})"#),
        format!(r#"hl.dsp.window.move({{ x = {x}, y = {y}, window = "class:{class}" }})"#),
    ]);
}

/// Get sorted list of Hyprland clients (by focus history)
pub fn clients() -> Vec<Client> {
    let Some(resp) = request("j/clients") else {
        return vec![];
    };
    let mut clients: Vec<Client> = serde_json::from_str(&resp).unwrap_or_default();
    clients.sort_by_key(|c| c.focus_history_id);
    clients
}

/// Run a Lua dispatch expression via hyprctl (blocking).
/// `expr` is a Lua dispatcher value, e.g. `hl.dsp.window.float({})`.
pub fn dispatch(expr: &str) {
    let _ = Command::new("hyprctl").args(["dispatch", expr]).output();
}

/// Run a Lua dispatch expression via hyprctl (non-blocking).
pub fn dispatch_async(expr: &str) {
    let expr = expr.to_owned();
    thread::spawn(move || {
        let _ = Command::new("hyprctl").args(["dispatch", &expr]).output();
    });
}

/// Run multiple Lua dispatch expressions atomically (non-blocking).
pub fn dispatch_batch_async(exprs: &[String]) {
    let batch = exprs.iter()
        .map(|e| format!("dispatch {e}"))
        .collect::<Vec<_>>()
        .join(" ; ");
    thread::spawn(move || {
        let _ = Command::new("hyprctl").args(["--batch", &batch]).output();
    });
}

/// Subscribe to Hyprland IPC event socket.
/// Calls `callback` for each event line. Reconnects on disconnect.
pub fn subscribe_events(mut callback: impl FnMut(&str) + Send + 'static) -> Option<thread::JoinHandle<()>> {
    let sig = env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    let runtime = env::var("XDG_RUNTIME_DIR").unwrap_or("/tmp".into());
    let path = format!("{}/hypr/{}/.socket2.sock", runtime, sig);

    Some(thread::spawn(move || {
        loop {
            let Ok(stream) = UnixStream::connect(&path) else {
                thread::sleep(std::time::Duration::from_secs(1));
                continue;
            };
            let reader = BufReader::new(stream);
            for line in reader.lines() {
                let Ok(line) = line else { break };
                callback(&line);
            }
            thread::sleep(std::time::Duration::from_millis(100));
        }
    }))
}
