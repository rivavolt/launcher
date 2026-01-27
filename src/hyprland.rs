//! Hyprland IPC: monitor info, clients, dispatch, event subscription

use serde::Deserialize;
use std::io::{BufRead, BufReader};
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
}

pub fn monitor() -> Option<Monitor> {
    let out = Command::new("hyprctl").args(["monitors", "-j"]).output().ok()?;
    let monitors: Vec<Monitor> = serde_json::from_slice(&out.stdout).ok()?;
    monitors.into_iter().next()
}

/// Calculate eframe window size for given width/height ratios.
/// Accounts for eframe's 2x HiDPI scaling.
pub fn window_size(w_ratio: f32, h_ratio: f32, fallback: (f32, f32)) -> (f32, f32) {
    monitor()
        .map(|m| {
            let w = m.width / m.scale * w_ratio as f64 / 2.0;
            let h = m.height / m.scale * h_ratio as f64 / 2.0;
            (w as f32, h as f32)
        })
        .unwrap_or(fallback)
}

/// Get sorted list of Hyprland clients (by focus history)
pub fn clients() -> Vec<Client> {
    let Some(out) = Command::new("hyprctl").args(["clients", "-j"]).output().ok() else {
        return vec![];
    };
    if !out.status.success() { return vec![]; }
    let mut clients: Vec<Client> = serde_json::from_slice(&out.stdout).unwrap_or_default();
    clients.sort_by_key(|c| c.focus_history_id);
    clients
}

/// Run hyprctl dispatch (blocking)
pub fn dispatch(cmd: &str, arg: &str) {
    let _ = Command::new("hyprctl").args(["dispatch", cmd, arg]).output();
}

/// Run hyprctl dispatch (non-blocking)
pub fn dispatch_async(cmd: &str, arg: &str) {
    let _ = Command::new("hyprctl").args(["dispatch", cmd, arg]).spawn();
}

/// Subscribe to Hyprland IPC event socket.
/// Calls `callback` for each event line. Reconnects on disconnect.
pub fn subscribe_events(callback: impl Fn(&str) + Send + 'static) -> Option<thread::JoinHandle<()>> {
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
