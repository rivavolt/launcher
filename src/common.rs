//! Shared constants and utilities for launcher and clipboard

use eframe::egui::{self, Sense, Ui};

// Layout constants - golden ratio based
pub const GOLDEN: f32 = 1.618;
pub const TEXT_SIZE: f32 = 16.0;
pub const INPUT_SIZE: f32 = TEXT_SIZE * GOLDEN;
pub const INPUT_PADDING: f32 = 8.0 * GOLDEN;
pub const ROW_HEIGHT: f32 = 36.0;
pub const MAX_VISIBLE_ITEMS: usize = 12;

// Key repeat timing
pub const REPEAT_DELAY_MS: u128 = 300;
pub const REPEAT_INTERVAL_MS: u128 = 120;

// Colors
pub mod colors {
    use eframe::egui::Color32;
    pub const BG_SELECTED: Color32 = Color32::from_rgba_premultiplied(60, 100, 160, 50);
    pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(225, 225, 225);
    pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(140, 140, 140);
    pub const TEXT_MUTED: Color32 = Color32::from_rgb(80, 80, 80);
    pub const GHOST_TEXT: Color32 = Color32::from_rgba_premultiplied(120, 120, 120, 140);
    pub const ACCENT: Color32 = Color32::from_rgb(100, 160, 220);
}

/// Render a selectable row with automatic scroll-into-view
/// Returns (row_rect, row_response, was_clicked)
pub fn render_row(
    ui: &mut Ui,
    index: usize,
    selected: usize,
    scroll_to_selected: bool,
    row_height: f32,
    content_width: f32,
) -> (egui::Rect, egui::Response, bool) {
    let is_selected = index == selected;

    // Get current Y position from cursor
    let row_y = ui.cursor().min.y;
    let row_rect = egui::Rect::from_min_size(
        egui::pos2(0.0, row_y),
        egui::vec2(content_width, row_height),
    );

    // Draw selection background
    if is_selected {
        ui.painter().rect_filled(row_rect, 0.0, colors::BG_SELECTED);
        if scroll_to_selected {
            ui.scroll_to_rect(row_rect, Some(egui::Align::Center));
        }
    }

    // Allocate space for the row
    let (_, response) = ui.allocate_exact_size(
        egui::vec2(content_width, row_height),
        Sense::click(),
    );

    let clicked = response.clicked();
    (row_rect, response, clicked)
}

/// Handle navigation keys and return (down, up) flags
/// Also handles key repeat state
pub fn handle_navigation_keys(
    ctx: &egui::Context,
    held_key: &mut Option<(egui::Key, std::time::Instant)>,
) -> (bool, bool) {
    let mut down = false;
    let mut up = false;
    let now = std::time::Instant::now();

    ctx.input(|i| {
        // Check for key releases
        for event in &i.events {
            if let egui::Event::Key { key, pressed: false, .. } = event {
                match key {
                    egui::Key::ArrowDown | egui::Key::ArrowUp |
                    egui::Key::J | egui::Key::K | egui::Key::N | egui::Key::P => {
                        *held_key = None;
                    }
                    _ => {}
                }
            }
        }

        // Check for key presses
        for event in &i.events {
            if let egui::Event::Key { key, pressed: true, modifiers, .. } = event {
                match key {
                    egui::Key::ArrowDown => {
                        down = true;
                        *held_key = Some((egui::Key::ArrowDown, now));
                    }
                    egui::Key::ArrowUp => {
                        up = true;
                        *held_key = Some((egui::Key::ArrowUp, now));
                    }
                    egui::Key::J if modifiers.ctrl => {
                        down = true;
                        *held_key = Some((egui::Key::ArrowDown, now));
                    }
                    egui::Key::K if modifiers.ctrl => {
                        up = true;
                        *held_key = Some((egui::Key::ArrowUp, now));
                    }
                    egui::Key::N if modifiers.ctrl => {
                        down = true;
                        *held_key = Some((egui::Key::ArrowDown, now));
                    }
                    egui::Key::P if modifiers.ctrl => {
                        up = true;
                        *held_key = Some((egui::Key::ArrowUp, now));
                    }
                    _ => {}
                }
            }
        }
    });

    // Manual key repeat
    if let Some((key, start_time)) = *held_key {
        let elapsed_ms = now.duration_since(start_time).as_millis();
        if elapsed_ms > REPEAT_DELAY_MS {
            let repeat_count = (elapsed_ms - REPEAT_DELAY_MS) / REPEAT_INTERVAL_MS;
            let last_repeat = (elapsed_ms - REPEAT_DELAY_MS - REPEAT_INTERVAL_MS) / REPEAT_INTERVAL_MS;
            if repeat_count > last_repeat || elapsed_ms < REPEAT_DELAY_MS + REPEAT_INTERVAL_MS {
                match key {
                    egui::Key::ArrowDown => down = true,
                    egui::Key::ArrowUp => up = true,
                    _ => {}
                }
            }
        }
        ctx.request_repaint();
    }

    (down, up)
}

/// Truncate string to max characters with ellipsis
pub fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ").replace('\t', " ");
    if s.chars().count() > max {
        s.chars().take(max - 1).collect::<String>() + "…"
    } else {
        s
    }
}
