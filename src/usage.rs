//! Event-sourced frecency tracking with dual-scale exponential decay.
//!
//! Raw focus events are the source of truth. Scores are derived by
//! reducing over the event log with time-decay weighting.
//! Dual half-lives (short + long) make scoring responsive to recent
//! activity while preserving long-term patterns.
//! The log is capped (max events + max age) and pruned on save.

use serde::{Deserialize, Serialize};
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

    /// Compute frecency score by reducing over events with dual-scale decay.
    /// score = Σ weight(e) * (0.5 * decay_short + 0.5 * decay_long)
    /// Short decay makes recent activity dominant; long decay preserves habits.
    pub fn score(&self, class: &str) -> f64 {
        let class = class.to_lowercase();
        let now = now();
        self.events
            .iter()
            .filter(|e| e.class == class)
            .map(|e| {
                let weight = if e.duration_secs > 0.0 {
                    e.duration_secs / 60.0
                } else {
                    LAUNCH_WEIGHT / 60.0
                };
                let dt = (now - e.timestamp).max(0.0);
                weight * (0.5 * decay(dt, HALF_LIFE_SHORT) + 0.5 * decay(dt, HALF_LIFE_LONG))
            })
            .sum()
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

    /// Compute contextual frecency: how often was this class selected
    /// for queries that are prefixes of (or match) the current query?
    pub fn query_score(&self, query: &str, class: &str) -> f64 {
        let query = query.to_lowercase();
        let class = class.to_lowercase();
        let now = now();
        self.selections
            .iter()
            .filter(|s| {
                s.class == class
                    && (query.starts_with(&s.query) || s.query.starts_with(&query))
            })
            .map(|s| {
                let dt = (now - s.timestamp).max(0.0);
                0.5 * decay(dt, HALF_LIFE_SHORT) + 0.5 * decay(dt, HALF_LIFE_LONG)
            })
            .sum()
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
