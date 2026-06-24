//! Event-sourced frecency tracking with dual-scale exponential decay.
//!
//! Raw focus events are the source of truth. Scores are derived by
//! reducing over the event log with time-decay weighting.
//! Dual half-lives (short + long) make scoring responsive to recent
//! activity while preserving long-term patterns.
//! The log is capped (max events + max age) and pruned on save.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Short half-life (1 hour) — responsive to recent session activity
const HALF_LIFE_SHORT: f64 = 3600.0;
/// Long half-life (3 days) — stable long-term patterns
const HALF_LIFE_LONG: f64 = 3.0 * 24.0 * 3600.0;
/// Max events to retain
const MAX_EVENTS: usize = 5000;
/// Max age in seconds (30 days)
const MAX_AGE: f64 = 30.0 * 24.0 * 3600.0;
/// Fixed weight for app launches in seconds (equivalent to 5 min focus)
const LAUNCH_WEIGHT: f64 = 300.0;

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn decay(dt: f64, half_life: f64) -> f64 {
    (-std::f64::consts::LN_2 * dt / half_life).exp()
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Event {
    /// Normalized app class (lowercase)
    pub class: String,
    /// Unix timestamp
    pub timestamp: f64,
    /// Focus duration in seconds (0 for launches)
    pub duration_secs: f64,
}

/// Query→selection association for contextual frecency.
/// Records what the user chose when they typed a given query.
#[derive(Serialize, Deserialize, Clone)]
pub struct Selection {
    /// The query typed (lowercase)
    pub query: String,
    /// The entry chosen (lowercase class/name)
    pub class: String,
    /// Unix timestamp
    pub timestamp: f64,
}

#[derive(Serialize, Deserialize)]
pub struct UsageLog {
    events: Vec<Event>,
    #[serde(default)]
    selections: Vec<Selection>,
    #[serde(skip)]
    path: PathBuf,
    #[serde(skip)]
    dirty: bool,
}

impl Default for UsageLog {
    fn default() -> Self {
        Self {
            events: Vec::new(),
            selections: Vec::new(),
            path: PathBuf::new(),
            dirty: false,
        }
    }
}

impl UsageLog {
    pub fn load() -> Self {
        let path = dirs::data_dir()
            .unwrap_or_else(|| PathBuf::from("/tmp"))
            .join("launcher")
            .join("usage.json");

        let mut log: UsageLog = fs::read_to_string(&path)
            .ok()
            .and_then(|data| serde_json::from_str(&data).ok())
            .unwrap_or_default();
        log.path = path;
        log
    }

    /// Record a focus duration event
    pub fn record_focus(&mut self, class: &str, duration_secs: f64) {
        if duration_secs < 1.0 {
            return; // ignore sub-second focus blips
        }
        self.events.push(Event {
            class: class.to_lowercase(),
            timestamp: now(),
            duration_secs,
        });
        self.dirty = true;
    }

    /// Record an app launch (fixed weight, no duration)
    pub fn record_launch(&mut self, class: &str) {
        self.events.push(Event {
            class: class.to_lowercase(),
            timestamp: now(),
            duration_secs: 0.0,
        });
        self.dirty = true;
    }

    /// Frecency score for every class, in a SINGLE pass over the event log
    /// (`weight(e) * (0.5*decay_short + 0.5*decay_long)`, summed per class).
    /// Replaces a per-class `O(events)` scan with one `O(events)` fold — the
    /// caller looks up each entry in `O(1)` instead of rescanning the whole log
    /// per entry per keystroke. Keys are the already-lowercased event classes;
    /// an absent class scores 0.
    pub fn class_scores(&self) -> HashMap<String, f64> {
        let now = now();
        let mut map: HashMap<String, f64> = HashMap::new();
        for e in &self.events {
            let weight = if e.duration_secs > 0.0 {
                e.duration_secs / 60.0
            } else {
                LAUNCH_WEIGHT / 60.0
            };
            let dt = (now - e.timestamp).max(0.0);
            *map.entry(e.class.clone()).or_insert(0.0) +=
                weight * (0.5 * decay(dt, HALF_LIFE_SHORT) + 0.5 * decay(dt, HALF_LIFE_LONG));
        }
        map
    }

    /// Record a query→selection association for contextual learning
    pub fn record_selection(&mut self, query: &str, class: &str) {
        let query = query.to_lowercase();
        if query.is_empty() {
            return;
        }
        self.selections.push(Selection {
            query,
            class: class.to_lowercase(),
            timestamp: now(),
        });
        self.dirty = true;
    }

    /// Contextual frecency per class for the current `query`, in a SINGLE pass
    /// over the selection log: how often was each class chosen for a query that
    /// is a prefix of (or is prefixed by) `query`. Same per-class output as the
    /// old per-class scan, folded once; keys are lowercased classes.
    pub fn query_class_scores(&self, query: &str) -> HashMap<String, f64> {
        let query = query.to_lowercase();
        let now = now();
        let mut map: HashMap<String, f64> = HashMap::new();
        for s in &self.selections {
            if query.starts_with(&s.query) || s.query.starts_with(&query) {
                let dt = (now - s.timestamp).max(0.0);
                *map.entry(s.class.clone()).or_insert(0.0) +=
                    0.5 * decay(dt, HALF_LIFE_SHORT) + 0.5 * decay(dt, HALF_LIFE_LONG);
            }
        }
        map
    }

    /// Prune old events and cap size, then persist if dirty
    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        let cutoff = now() - MAX_AGE;
        self.events.retain(|e| e.timestamp > cutoff);
        if self.events.len() > MAX_EVENTS {
            let drop = self.events.len() - MAX_EVENTS;
            self.events.drain(..drop);
        }
        self.selections.retain(|s| s.timestamp > cutoff);
        if self.selections.len() > MAX_EVENTS {
            let drop = self.selections.len() - MAX_EVENTS;
            self.selections.drain(..drop);
        }
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(data) = serde_json::to_string(&self) {
            let _ = fs::write(&self.path, data);
        }
        self.dirty = false;
    }
}
