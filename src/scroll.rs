//! Kinetic scroll momentum for touchpad scrolling.
//! Uses exponential velocity decay matching Firefox/iOS (rate=0.998/ms).

use eframe::egui;

const DECAY_PER_MS: f32 = 0.998;
const STOP_VELOCITY: f32 = 10.0; // px/s (Firefox: 0.01 px/ms)

pub struct ScrollMomentum {
    velocity: f32,
    prev_frame: std::time::Instant,
}

impl ScrollMomentum {
    pub fn new() -> Self {
        let now = std::time::Instant::now();
        Self { velocity: 0.0, prev_frame: now }
    }

    /// Call each frame before any ScrollArea is shown.
    pub fn update(&mut self, ctx: &egui::Context) {
        let now = std::time::Instant::now();
        let dt = now.duration_since(self.prev_frame).as_secs_f32().max(0.001);
        self.prev_frame = now;

        let raw = ctx.input(|i| i.raw_scroll_delta.y);

        if raw.abs() > 0.5 {
            self.velocity = self.velocity * 0.5 + (raw / dt) * 0.5;
            return;
        }

        if self.velocity.abs() < STOP_VELOCITY {
            self.velocity = 0.0;
            return;
        }

        // Exponential decay: v *= 0.998^(dt_ms)
        self.velocity *= DECAY_PER_MS.powf(dt * 1000.0);

        if self.velocity.abs() < STOP_VELOCITY {
            self.velocity = 0.0;
        } else {
            ctx.input_mut(|i| i.smooth_scroll_delta.y = self.velocity * dt);
            ctx.request_repaint();
        }
    }
}
