//! Shared constants and utilities for launcher and clipboard

use eframe::egui::{self, Frame, Sense, Ui, Rect};

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
    pub const BG_BASE: Color32 = Color32::from_rgba_premultiplied(12, 12, 12, 160);
    pub const BG_INPUT: Color32 = Color32::TRANSPARENT;
    pub const BG_SELECTED: Color32 = Color32::from_rgba_premultiplied(60, 100, 160, 50);
    pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(240, 240, 240);
    pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(210, 210, 210);
    pub const TEXT_MUTED: Color32 = Color32::from_rgb(95, 95, 95);
    pub const GHOST_TEXT: Color32 = Color32::from_rgba_premultiplied(120, 120, 120, 140);
    pub const BG_PREVIEW: Color32 = Color32::from_rgba_premultiplied(18, 18, 18, 160);
    pub const ACCENT: Color32 = Color32::from_rgb(100, 160, 220);
}

/// Panel frame with semi-transparent dark background
pub fn panel_frame() -> Frame {
    Frame {
        fill: colors::BG_BASE,
        ..Frame::NONE
    }
}

/// Input field frame with opaque dark background and rounded corners
pub fn input_frame() -> Frame {
    Frame {
        fill: colors::BG_INPUT,
        corner_radius: egui::CornerRadius::same(6),
        inner_margin: egui::Margin::symmetric(12, 6),
        outer_margin: egui::Margin::same(6),
        ..Frame::NONE
    }
}

/// Preview pane frame with subtle background lift
pub fn preview_frame() -> Frame {
    Frame {
        fill: colors::BG_PREVIEW,
        corner_radius: egui::CornerRadius::same(6),
        inner_margin: egui::Margin::same(10),
        ..Frame::NONE
    }
}

/// Thin separator line below the input area (call after scroll area to render on top)
pub fn paint_input_separator(ui: &mut Ui, y: f32) {
    let width = ui.available_width();
    let stroke = egui::Stroke::new(1.0, egui::Color32::from_white_alpha(8));
    ui.painter().hline(0.0..=width, y, stroke);
}

pub struct VirtualListOutput {
    pub clicked: Option<usize>,
    pub selected_rect: Option<Rect>,
}

/// Render a virtualized list inside a ScrollArea closure.
/// Only renders rows visible in the clip rect, using blank space for the rest.
pub fn virtual_list(
    ui: &mut Ui,
    total_items: usize,
    row_height: f32,
    selected: usize,
    scroll_to_selected: bool,
    skip_selected_highlight: bool,
    mut render_row: impl FnMut(&mut Ui, usize, Rect),
) -> VirtualListOutput {
    if total_items == 0 {
        return VirtualListOutput { clicked: None, selected_rect: None };
    }

    let content_width = ui.available_width();
    let spacing_y = ui.spacing().item_spacing.y;
    let stride = row_height + spacing_y;
    let list_top = ui.cursor().min.y;

    // Handle scroll-to-selected before computing visible range
    if scroll_to_selected {
        let sel_top = list_top + selected as f32 * stride;
        let sel_rect = Rect::from_min_size(
            egui::pos2(0.0, sel_top),
            egui::vec2(content_width, row_height),
        );
        ui.scroll_to_rect(sel_rect, Some(egui::Align::Center));
    }

    // Determine visible range from clip rect
    let clip = ui.clip_rect();
    let first_visible = ((clip.min.y - list_top) / stride).floor().max(0.0) as usize;
    let last_visible = (((clip.max.y - list_top) / stride).ceil() as usize).min(total_items);
    // Add 1-row buffer on each side for smooth scrolling
    let render_start = first_visible.saturating_sub(1);
    let render_end = (last_visible + 1).min(total_items);



    // Space before visible rows
    if render_start > 0 {
        let skip_h = render_start as f32 * stride - spacing_y;
        ui.add_space(skip_h);
        // After add_space, egui adds item_spacing automatically before next widget.
        // We already accounted for spacing in stride, so add spacing_y back
        // to compensate (the next allocate_exact_size will get extra spacing from egui).
    }

    let mut clicked = None;
    let mut selected_rect = None;

    for i in render_start..render_end {
        let row_y = ui.cursor().min.y;
        let row_rect = Rect::from_min_size(
            egui::pos2(0.0, row_y),
            egui::vec2(content_width, row_height),
        );

        let is_selected = i == selected;
        if is_selected {
            if !skip_selected_highlight {
                ui.painter().rect_filled(row_rect, 0.0, colors::BG_SELECTED);
            }
            selected_rect = Some(row_rect);
        }

        let (_, response) = ui.allocate_exact_size(
            egui::vec2(content_width, row_height),
            Sense::click(),
        );

        render_row(ui, i, row_rect);

        if response.clicked() {
            clicked = Some(i);
        }
    }

    // Space after visible rows
    if render_end < total_items {
        let remaining = total_items - render_end;
        let skip_h = remaining as f32 * stride - spacing_y;
        ui.add_space(skip_h);
    }

    VirtualListOutput { clicked, selected_rect }
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

    if !ctx.input(|i| i.focused) {
        *held_key = None;
        return (false, false);
    }

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
            let last_repeat = (elapsed_ms - REPEAT_DELAY_MS).saturating_sub(REPEAT_INTERVAL_MS) / REPEAT_INTERVAL_MS;
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

/// Configure transparent style on the egui context
pub fn setup_transparent_style(cc: &eframe::CreationContext) {
    let mut style = egui::Style::default();
    style.visuals.window_fill = egui::Color32::TRANSPARENT;
    style.visuals.panel_fill = egui::Color32::TRANSPARENT;
    cc.egui_ctx.set_style(style);
}

/// Build NativeOptions for a transparent, undecorated window
pub fn window_options(app_id: &str, width: f32, height: f32) -> eframe::NativeOptions {
    eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([width, height])
            .with_decorations(false)
            .with_transparent(true)
            .with_app_id(app_id),
        ..Default::default()
    }
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
