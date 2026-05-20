//! Shared constants and utilities for launcher and clipboard

use eframe::egui::{self, Color32, Context, FontFamily, FontId, Frame, RichText, Sense, Ui, Rect};
use eframe::epaint::Mesh;
use std::sync::OnceLock;

pub const GOLDEN: f32 = 1.618;
pub const MAX_VISIBLE_ITEMS: usize = 12;
/// Y offset (as fraction of monitor height) where the input row sits.
/// 0.236 = 1 - 1/golden, matches the spawn rule in flake.nix nixosModule.
pub const Y_ANCHOR_RATIO: f32 = 0.236;

pub fn text_size() -> f32 {
    static V: OnceLock<f32> = OnceLock::new();
    *V.get_or_init(|| {
        std::env::var("LAUNCHER_FONT_SIZE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(16.0)
    })
}

pub fn font_family() -> &'static str {
    static V: OnceLock<String> = OnceLock::new();
    V.get_or_init(|| {
        std::env::var("LAUNCHER_FONT_FAMILY").unwrap_or_else(|_| "Inter".into())
    })
}

/// Input/query field font size — sits 2pt above row text for focal weight.
pub fn input_size() -> f32 { 18.0 }
pub fn row_height() -> f32 { (text_size() * 1.5).round() }
/// Icon/prompt container: square area for row icons and the input `>` glyph
pub fn icon_container() -> f32 { (text_size() * 1.5).round() + 4.0 }

// Key repeat timing
pub const REPEAT_DELAY_MS: u128 = 300;
pub const REPEAT_INTERVAL_MS: u128 = 120;

// Colors — monochrome with single cool-white accent
pub mod colors {
    use eframe::egui::Color32;
    pub const BG_BASE: Color32 = Color32::from_rgb(0, 0, 0);
    pub const BG_INPUT: Color32 = Color32::from_rgb(8, 8, 8);
    pub const BG_SELECTED: Color32 = Color32::from_rgb(18, 18, 20);
    pub const BG_HOVER: Color32 = Color32::from_rgb(10, 10, 11);
    pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(210, 210, 210);
    pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(120, 120, 120);
    pub const TEXT_SUBTITLE: Color32 = Color32::from_rgb(70, 70, 70);
    pub const TEXT_MUTED: Color32 = Color32::from_rgb(45, 45, 45);
    pub const GHOST_TEXT: Color32 = Color32::from_rgb(35, 35, 35);
    pub const BG_PREVIEW: Color32 = Color32::from_rgb(8, 8, 8);
    pub const ACCENT: Color32 = Color32::from_rgb(200, 160, 60);
    pub const ACCENT_BAR: f32 = 1.5;
}

/// Panel frame with semi-transparent dark background
pub fn panel_frame() -> Frame {
    Frame {
        fill: colors::BG_BASE,
        corner_radius: egui::CornerRadius::ZERO,
        ..Frame::NONE
    }
}

/// Input field frame — tight padding
pub fn input_frame() -> Frame {
    Frame {
        fill: colors::BG_INPUT,
        inner_margin: egui::Margin::symmetric(8, 7),
        outer_margin: egui::Margin { bottom: 0, ..Default::default() },
        corner_radius: egui::CornerRadius::ZERO,
        ..Frame::NONE
    }
}

pub struct InputPanelOutput {
    pub response: egui::InnerResponse<()>,
    pub changed: bool,
    pub cleared: bool,
    pub text_edit_id: egui::Id,
}

/// Render the shared input panel with `>` prompt, focus handling, and optional ghost text.
/// Returns the panel response and whether the query changed.
pub fn input_panel(
    ctx: &Context,
    query: &mut String,
    hint: &str,
    ghost_text: Option<&str>,
) -> InputPanelOutput {
    let mut changed = false;
    let mut text_edit_id = egui::Id::NULL;
    // Handle Ctrl+U to clear input
    let mut cleared = false;
    ctx.input(|i| {
        for event in &i.events {
            if let egui::Event::Key { key: egui::Key::U, pressed: true, modifiers, .. } = event {
                if modifiers.ctrl { cleared = true; }
            }
        }
    });
    if cleared && !query.is_empty() {
        query.clear();
        changed = true;
    }
    let response = egui::TopBottomPanel::top("input")
        .frame(input_frame())
        .show(ctx, |ui: &mut Ui| {
            let input_font = FontId::new(input_size(), FontFamily::Proportional);
            let old_query = query.clone();
            let output = ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 10.0;
                // Prompt glyph centered in icon_container-sized area
                let container = icon_container();
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(container, container),
                    Sense::hover(),
                );
                let prompt_font = FontId::new(input_size(), FontFamily::Proportional);
                ui.painter().text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    ">",
                    prompt_font,
                    colors::TEXT_SUBTITLE,
                );
                egui::TextEdit::singleline(query)
                    .font(input_font.clone())
                    .text_color(colors::TEXT_PRIMARY)
                    .hint_text(RichText::new(hint).color(colors::TEXT_MUTED))
                    .frame(false)
                    .desired_width(ui.available_width())
                    .show(ui)
            }).inner;
            if ui.ctx().input(|i| i.focused) {
                output.response.request_focus();
            } else {
                output.response.surrender_focus();
            }
            changed = *query != old_query;
            text_edit_id = output.response.id;

            if let Some(ghost) = ghost_text {
                if !ghost.is_empty() && !query.is_empty() {
                    let mut job = egui::text::LayoutJob::default();
                    job.append(query, 0.0, egui::TextFormat {
                        font_id: input_font.clone(),
                        color: Color32::TRANSPARENT,
                        ..Default::default()
                    });
                    job.append(ghost, 0.0, egui::TextFormat {
                        font_id: input_font,
                        color: colors::GHOST_TEXT,
                        ..Default::default()
                    });
                    let galley = ui.fonts(|f| f.layout_job(job));
                    ui.painter().galley(output.galley_pos, galley, Color32::TRANSPARENT);
                }
            }
        });
    InputPanelOutput { response, changed, cleared, text_edit_id }
}

/// Preview pane frame
pub fn preview_frame() -> Frame {
    Frame {
        fill: colors::BG_PREVIEW,
        corner_radius: egui::CornerRadius::same(4),
        inner_margin: egui::Margin::same(8),
        ..Frame::NONE
    }
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
        let (_, response) = ui.allocate_exact_size(
            egui::vec2(content_width, row_height),
            Sense::click(),
        );

        if is_selected {
            if !skip_selected_highlight {
                ui.painter().rect_filled(row_rect, 0.0, colors::BG_SELECTED);
                let bar = Rect::from_min_size(
                    row_rect.left_top(),
                    egui::vec2(colors::ACCENT_BAR, row_height),
                );
                ui.painter().rect_filled(bar, 0.0, colors::ACCENT);
            }
            selected_rect = Some(row_rect);
        } else if response.hovered() {
            ui.painter().rect_filled(row_rect, 0.0, colors::BG_HOVER);
        }

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

/// Configure style on the egui context
pub fn setup_transparent_style(cc: &eframe::CreationContext) {
    let mut style = egui::Style::default();
    style.visuals.window_fill = egui::Color32::TRANSPARENT;
    style.visuals.panel_fill = egui::Color32::TRANSPARENT;
    style.spacing.scroll.bar_width = 4.0;
    style.visuals.text_cursor.stroke = egui::Stroke::new(1.0, colors::ACCENT);
    // Minimal scrollbar
    style.visuals.widgets.inactive.bg_fill = egui::Color32::from_rgba_premultiplied(40, 40, 40, 40);
    style.visuals.widgets.hovered.bg_fill = egui::Color32::from_rgba_premultiplied(60, 60, 60, 80);
    style.visuals.widgets.active.bg_fill = egui::Color32::from_rgba_premultiplied(80, 80, 80, 100);
    cc.egui_ctx.set_style(style);

    if let Some(font_path) = find_font(font_family()) {
        if let Ok(font_data) = std::fs::read(&font_path) {
            let mut fonts = egui::FontDefinitions::default();
            fonts.font_data.insert(
                "inter".to_owned(),
                std::sync::Arc::new(egui::FontData::from_owned(font_data)),
            );
            fonts.families.get_mut(&egui::FontFamily::Proportional).unwrap()
                .insert(0, "inter".to_owned());
            cc.egui_ctx.set_fonts(fonts);
        }
    }
}

/// Find a font file by family name via fontconfig
fn find_font(name: &str) -> Option<String> {
    std::process::Command::new("fc-match")
        .args([name, "--format=%{file}"])
        .output()
        .ok()
        .and_then(|o| {
            let path = String::from_utf8_lossy(&o.stdout).to_string();
            if path.is_empty() { None } else { Some(path) }
        })
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

/// Text color with dimming for unselected rows (0.5 opacity pattern)
pub fn row_text_color(selected: bool) -> Color32 {
    if selected { colors::TEXT_PRIMARY } else { colors::TEXT_SECONDARY }
}

/// Render "No results" empty state
pub fn empty_state(ui: &mut Ui) {
    let font = FontId::new(text_size(), FontFamily::Proportional);
    let rect = ui.available_rect_before_wrap();
    let center = egui::pos2(rect.center().x, rect.min.y + row_height());
    ui.painter().text(center, egui::Align2::CENTER_CENTER, "No results", font, colors::TEXT_MUTED);
}

/// Find character indices where the query matches as substring or fuzzy
pub fn match_indices(text: &str, query: &str) -> Vec<usize> {
    if query.is_empty() { return vec![]; }
    let text_lower = text.to_lowercase();
    let query_lower = query.to_lowercase();
    let query_chars: Vec<char> = query_lower.chars().collect();
    // Try substring match first (contiguous)
    if let Some(start) = text_lower.find(&query_lower) {
        let mut indices = Vec::new();
        let mut pos = start;
        for _ in 0..query_lower.len() {
            indices.push(pos);
            pos += text_lower[pos..].chars().next().map_or(1, |c| c.len_utf8());
        }
        return indices;
    }
    // Fall back to fuzzy: sequential character matching
    let mut indices = Vec::new();
    let mut qi = 0;
    for (ci, ch) in text_lower.char_indices() {
        if qi >= query_chars.len() { break; }
        if ch == query_chars[qi] {
            indices.push(ci);
            qi += 1;
        }
    }
    if qi == query_chars.len() { indices } else { vec![] }
}

/// Paint text with highlighted match indices (underline + bright color)
pub fn paint_highlighted(
    ui: &Ui,
    pos: egui::Pos2,
    text: &str,
    font: &FontId,
    base_color: Color32,
    highlight_color: Color32,
    indices: &[usize],
) {
    if indices.is_empty() {
        ui.painter().text(pos, egui::Align2::LEFT_TOP, text, font.clone(), base_color);
        return;
    }
    let mut job = egui::text::LayoutJob::default();
    let base_fmt = egui::TextFormat { font_id: font.clone(), color: base_color, ..Default::default() };
    let highlight_fmt = egui::TextFormat {
        font_id: font.clone(),
        color: highlight_color,
        underline: egui::Stroke::new(1.0, highlight_color),
        ..Default::default()
    };
    let mut last = 0;
    for &idx in indices {
        if idx > last {
            job.append(&text[last..idx], 0.0, base_fmt.clone());
        }
        let ch_len = text[idx..].chars().next().map_or(1, |c| c.len_utf8());
        job.append(&text[idx..idx + ch_len], 0.0, highlight_fmt.clone());
        last = idx + ch_len;
    }
    if last < text.len() {
        job.append(&text[last..], 0.0, base_fmt);
    }
    let galley = ui.fonts(|f| f.layout_job(job));
    ui.painter().galley(pos, galley, Color32::TRANSPARENT);
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

/// Normalize clipboard text for list-row display:
/// strip leading whitespace, collapse internal whitespace runs (including
/// newlines/tabs) to a single space. Original content is preserved at paste
/// time — this only adjusts the rendered preview.
pub fn clip_display_line(s: &str) -> String {
    let trimmed = s.trim_start();
    let mut out = String::with_capacity(trimmed.len());
    let mut in_ws = false;
    for ch in trimmed.chars() {
        if ch.is_whitespace() {
            if !in_ws {
                out.push(' ');
                in_ws = true;
            }
        } else {
            out.push(ch);
            in_ws = false;
        }
    }
    out
}

/// Whether the MIME label should be rendered next to a clipboard row's
/// metadata. Common defaults (plain text, common image types, uri lists)
/// are obvious from the content itself and add noise; anything else carries
/// real information.
pub fn should_show_mime_label(mime: &str) -> bool {
    let base = mime.split(';').next().unwrap_or(mime).trim();
    !matches!(base, "text/plain" | "image/png" | "image/jpeg" | "text/uri-list")
}

/// Workspace chip colors — sharp 18x18 square with the workspace number
/// centered inside. Background is a subtle elevated-surface gray, foreground
/// a warm off-white. No gold accent (reserved for window-focus signaling).
pub mod chip {
    use eframe::egui::Color32;
    pub const SIZE: f32 = 18.0;
    pub const BG: Color32 = Color32::from_rgb(0x25, 0x22, 0x20);
    pub const FG: Color32 = Color32::from_rgb(0xd4, 0xd0, 0xca);
    pub const FONT_SIZE: f32 = 11.0;
}

/// Paint a workspace chip centered at `center`. Square, sharp corners.
pub fn paint_workspace_chip(ui: &Ui, center: egui::Pos2, label: &str) {
    let rect = Rect::from_center_size(center, egui::vec2(chip::SIZE, chip::SIZE));
    ui.painter().rect_filled(rect, 0.0, chip::BG);
    let font = FontId::new(chip::FONT_SIZE, FontFamily::Proportional);
    ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, label, font, chip::FG);
}

/// Paint fade gradients at top and bottom edges of a scroll area.
/// Call after rendering the scroll area content, passing the scroll area's outer rect.
pub fn paint_scroll_fade(ui: &Ui, rect: Rect, fade_h: f32) {
    let base = colors::BG_BASE;
    let transparent = Color32::from_rgba_premultiplied(0, 0, 0, 0);

    // Top fade
    let top = Rect::from_min_max(rect.left_top(), egui::pos2(rect.right(), rect.top() + fade_h));
    let mut top_mesh = Mesh::default();
    top_mesh.colored_vertex(top.left_top(), base);
    top_mesh.colored_vertex(top.right_top(), base);
    top_mesh.colored_vertex(top.right_bottom(), transparent);
    top_mesh.colored_vertex(top.left_bottom(), transparent);
    top_mesh.add_triangle(0, 1, 2);
    top_mesh.add_triangle(0, 2, 3);
    ui.painter().add(egui::Shape::mesh(top_mesh));

    // Bottom fade
    let bot = Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - fade_h), rect.right_bottom());
    let mut bot_mesh = Mesh::default();
    bot_mesh.colored_vertex(bot.left_top(), transparent);
    bot_mesh.colored_vertex(bot.right_top(), transparent);
    bot_mesh.colored_vertex(bot.right_bottom(), base);
    bot_mesh.colored_vertex(bot.left_bottom(), base);
    bot_mesh.add_triangle(0, 1, 2);
    bot_mesh.add_triangle(0, 2, 3);
    ui.painter().add(egui::Shape::mesh(bot_mesh));
}
