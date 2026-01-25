//! App launcher using eframe (regular window in special workspace)

use eframe::egui::{self, CentralPanel, Context, Frame, Color32, RichText, ScrollArea, Sense, Ui, FontFamily, FontId, Stroke};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::{env, fs};
use strsim::jaro_winkler;

// Layout constants - golden ratio based
const GOLDEN: f32 = 1.618;
const TEXT_SIZE: f32 = 16.0;
const INPUT_SIZE: f32 = TEXT_SIZE * GOLDEN;  // ~26
const INPUT_PADDING: f32 = 8.0 * GOLDEN;     // ~13
const ICON_SIZE: f32 = 20.0;
const ICON_CONTAINER: f32 = 24.0;
const ROW_PADDING: f32 = 6.0;
const ICON_LABEL_SPACING: f32 = 8.0;
const MAX_VISIBLE_ITEMS: usize = 15;
const WS_INDICATOR_WIDTH: f32 = 28.0;

// Colors - refined palette (transparent for blur via special workspace)
mod colors {
    use eframe::egui::Color32;
    pub const BG_PANEL: Color32 = Color32::TRANSPARENT;
    pub const BG_INPUT: Color32 = Color32::from_rgba_premultiplied(0, 0, 0, 30);
    pub const BG_SELECTED: Color32 = Color32::from_rgba_premultiplied(60, 100, 160, 50);
    pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(225, 225, 225);
    pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(140, 140, 140);
    pub const TEXT_MUTED: Color32 = Color32::from_rgb(80, 80, 80);
    pub const GHOST_TEXT: Color32 = Color32::from_rgba_premultiplied(120, 120, 120, 140);
    pub const ACCENT: Color32 = Color32::from_rgb(100, 160, 220);
    pub const ICON_BG: Color32 = Color32::from_rgba_premultiplied(255, 255, 255, 15);
}

fn truncate_to_width(ui: &Ui, s: &str, font: FontId, max_width: f32) -> String {
    let full = ui.painter().layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE);
    if full.rect.width() <= max_width {
        return s.to_string();
    }

    // Binary search for the right length
    let chars: Vec<char> = s.chars().collect();
    let mut low = 0;
    let mut high = chars.len();

    while low < high {
        let mid = (low + high + 1) / 2;
        let test: String = chars[..mid].iter().collect::<String>() + "…";
        let width = ui.painter().layout_no_wrap(test, font.clone(), Color32::WHITE).rect.width();
        if width <= max_width {
            low = mid;
        } else {
            high = mid - 1;
        }
    }

    if low == 0 {
        "…".to_string()
    } else {
        chars[..low].iter().collect::<String>() + "…"
    }
}

#[derive(Clone)]
enum Entry {
    Desktop {
        name: String,
        exec: String,
        terminal: bool,
        icon: Option<egui::TextureHandle>,
    },
    Window {
        title: String,
        class: String,
        address: String,
        workspace: String,
        icon: Option<egui::TextureHandle>,
    },
}

impl Entry {
    fn name(&self) -> &str {
        match self {
            Entry::Desktop { name, .. } => name,
            Entry::Window { title, class, .. } => {
                if title.is_empty() { class } else { title }
            }
        }
    }

    fn workspace(&self) -> Option<&str> {
        match self {
            Entry::Window { workspace, .. } => Some(workspace),
            _ => None,
        }
    }

    fn icon(&self) -> Option<&egui::TextureHandle> {
        match self {
            Entry::Desktop { icon, .. } => icon.as_ref(),
            Entry::Window { icon, .. } => icon.as_ref(),
        }
    }

    fn is_window(&self) -> bool {
        matches!(self, Entry::Window { .. })
    }

    fn searchable(&self) -> Vec<&str> {
        match self {
            Entry::Desktop { name, .. } => vec![name.as_str()],
            Entry::Window { title, class, .. } => vec![title.as_str(), class.as_str()],
        }
    }
}

struct App {
    query: String,
    entries: Vec<Entry>,
    filtered: Vec<usize>,
    selected: usize,
    should_hide: bool,
    loaded: bool,
    // Key repeat state
    held_key: Option<(egui::Key, std::time::Instant)>,
    // Fuzzy matcher
    matcher: Matcher,
}

impl App {
    fn new() -> Self {
        Self {
            query: String::new(),
            entries: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            should_hide: false,
            loaded: false,
            held_key: None,
            matcher: Matcher::new(Config::DEFAULT),
        }
    }

    fn load_entries(&mut self, ctx: &Context) {
        let icon_index = build_icon_index();
        let wmclass_icons = build_wmclass_icon_map(&icon_index);

        // Collect windows first, then desktop entries
        self.entries = collect_hyprland_windows(ctx, &icon_index, &wmclass_icons);
        self.entries.extend(collect_desktop_entries(ctx, &icon_index));

        self.filtered = (0..self.entries.len().min(20)).collect();
        self.loaded = true;
    }

    fn filter(&mut self) {
        if self.query.is_empty() {
            self.filtered = (0..self.entries.len().min(50)).collect();
        } else {
            let pattern = Pattern::parse(&self.query, CaseMatching::Ignore, Normalization::Smart);
            let query_lower = self.query.to_lowercase();

            let mut scored: Vec<_> = self.entries.iter().enumerate()
                .filter_map(|(idx, e)| {
                    // Try nucleo (subsequence matching)
                    let nucleo_score: u32 = e.searchable().iter()
                        .filter_map(|s| {
                            let mut buf = Vec::new();
                            let haystack = Utf32Str::new(s, &mut buf);
                            pattern.score(haystack, &mut self.matcher)
                        })
                        .max()
                        .unwrap_or(0);

                    // Use jaro-winkler for typo tolerance if nucleo found nothing
                    let jw_score: u32 = if nucleo_score == 0 {
                        e.searchable().iter()
                            .map(|s| (jaro_winkler(&query_lower, &s.to_lowercase()) * 1000.0) as u32)
                            .filter(|&s| s >= 850)
                            .max()
                            .unwrap_or(0)
                    } else {
                        0
                    };

                    // Prefix bonus
                    let prefix_bonus: u32 = if e.searchable().iter()
                        .any(|s| s.to_lowercase().starts_with(&query_lower))
                    { 10000 } else { 0 };

                    let match_score = nucleo_score.max(jw_score) + prefix_bonus;
                    if match_score == 0 { return None; }
                    Some((match_score, idx))
                })
                .collect();

            scored.sort_by(|a, b| b.0.cmp(&a.0));
            self.filtered = scored.into_iter().map(|(_, idx)| idx).take(50).collect();
        }
        self.selected = 0;
    }

    fn ghost_text(&self) -> String {
        if self.query.is_empty() {
            return String::new();
        }
        if let Some(&idx) = self.filtered.first() {
            let name = self.entries[idx].name();
            let name_lower = name.to_lowercase();
            let query_lower = self.query.to_lowercase();
            if name_lower.starts_with(&query_lower) {
                return name.chars().skip(self.query.chars().count()).collect();
            }
        }
        String::new()
    }

    fn activate(&mut self) {
        if let Some(&idx) = self.filtered.get(self.selected) {
            let e = &self.entries[idx];
            match e {
                Entry::Desktop { exec, terminal, .. } => {
                    let parts: Vec<&str> = exec.split_whitespace()
                        .filter(|s| !s.starts_with('%')).collect();
                    if let Some((bin, args)) = parts.split_first() {
                        if *terminal {
                            let term = env::var("TERMINAL").unwrap_or("kitty".into());
                            let mut term_parts = term.split_whitespace();
                            let term_bin = term_parts.next().unwrap_or("kitty");
                            let term_args: Vec<&str> = term_parts.collect();
                            let _ = Command::new(term_bin)
                                .args(&term_args)
                                .arg("-e")
                                .arg(bin)
                                .args(args)
                                .stdin(std::process::Stdio::null())
                                .stdout(std::process::Stdio::null())
                                .stderr(std::process::Stdio::null())
                                .spawn();
                        } else {
                            let _ = Command::new(bin).args(args)
                                .stdin(std::process::Stdio::null())
                                .stdout(std::process::Stdio::null())
                                .stderr(std::process::Stdio::null())
                                .spawn();
                        }
                    }
                }
                Entry::Window { address, .. } => {
                    let _ = Command::new("hyprctl")
                        .args(["dispatch", "focuswindow", &format!("address:{}", address)])
                        .output();
                }
            }
        }
        self.should_hide = true;
    }

    fn hide_and_reset(&mut self) {
        // Reset state
        self.query.clear();
        self.selected = 0;
        self.filtered = (0..self.entries.len().min(50)).collect();
        self.should_hide = false;
        // Toggle special workspace to hide
        let _ = Command::new("hyprctl")
            .args(["dispatch", "togglespecialworkspace", "launcher"])
            .spawn();
    }

    fn render(&mut self, ctx: &Context) {
        if self.entries.is_empty() {
            self.load_entries(ctx);
        }

        let max_sel = self.filtered.len().saturating_sub(1);
        let (mut down, mut up, mut activate) = (false, false, false);

        let now = std::time::Instant::now();
        const REPEAT_DELAY_MS: u128 = 300;
        const REPEAT_INTERVAL_MS: u128 = 120;

        ctx.input(|i: &egui::InputState| {
            // Check for key releases - clear held state
            for event in &i.events {
                if let egui::Event::Key { key, pressed: false, .. } = event {
                    match key {
                        egui::Key::ArrowDown | egui::Key::ArrowUp |
                        egui::Key::J | egui::Key::K | egui::Key::N | egui::Key::P => {
                            self.held_key = None;
                        }
                        _ => {}
                    }
                }
            }

            // Check all key events (pressed)
            for event in &i.events {
                if let egui::Event::Key { key, pressed: true, modifiers, .. } = event {
                    match key {
                        egui::Key::Escape => self.should_hide = true,
                        egui::Key::Enter => activate = true,
                        egui::Key::ArrowDown => { down = true; self.held_key = Some((egui::Key::ArrowDown, now)); }
                        egui::Key::ArrowUp => { up = true; self.held_key = Some((egui::Key::ArrowUp, now)); }
                        egui::Key::J if modifiers.ctrl => { down = true; self.held_key = Some((egui::Key::ArrowDown, now)); }
                        egui::Key::K if modifiers.ctrl => { up = true; self.held_key = Some((egui::Key::ArrowUp, now)); }
                        egui::Key::N if modifiers.ctrl => { down = true; self.held_key = Some((egui::Key::ArrowDown, now)); }
                        egui::Key::P if modifiers.ctrl => { up = true; self.held_key = Some((egui::Key::ArrowUp, now)); }
                        _ => {}
                    }
                }
            }
        });

        // Manual key repeat (outside input closure)
        if let Some((key, start_time)) = self.held_key {
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
            // Request continuous repaints while key is held
            ctx.request_repaint();
        }

        if down { self.selected = (self.selected + 1).min(max_sel); }
        if up { self.selected = self.selected.saturating_sub(1); }
        if activate { self.activate(); return; }

        let row_height = ICON_CONTAINER + ROW_PADDING * 2.0;
        let input_height = INPUT_SIZE + INPUT_PADDING * 2.0;

        CentralPanel::default()
            .frame(Frame::NONE)
            .show(ctx, |ui: &mut Ui| {
                let screen = ui.available_rect_before_wrap();
                let content_width = screen.width();

                let font_id = FontId::new(INPUT_SIZE, FontFamily::Proportional);
                let ghost = self.ghost_text();

                // Use vertical layout with proper spacing
                ui.add_space(4.0);

                ui.horizontal(|ui: &mut Ui| {
                    ui.add_space(INPUT_PADDING);

                    ui.label(RichText::new(">")
                        .color(colors::TEXT_MUTED)
                        .size(INPUT_SIZE));
                    ui.add_space(8.0);

                    let text_start = ui.cursor().min;

                    let old_query = self.query.clone();
                    let input = egui::TextEdit::singleline(&mut self.query)
                        .font(font_id.clone())
                        .text_color(colors::TEXT_PRIMARY)
                        .hint_text(RichText::new("Search...").color(colors::TEXT_MUTED))
                        .frame(false)
                        .desired_width(content_width - INPUT_PADDING * 3.0 - 30.0);
                    let r = ui.add(input);
                    r.request_focus();
                    if self.query != old_query { self.filter(); }

                    // Ghost text
                    if !ghost.is_empty() && self.query.is_empty() {
                        // Don't show ghost when there's hint text
                    } else if !ghost.is_empty() {
                        let query_galley = ui.painter().layout_no_wrap(
                            self.query.clone(),
                            font_id.clone(),
                            Color32::WHITE,
                        );
                        let ghost_x = text_start.x + query_galley.rect.width();
                        let ghost_y = text_start.y;
                        ui.painter().text(
                            egui::pos2(ghost_x, ghost_y),
                            egui::Align2::LEFT_TOP,
                            &ghost,
                            font_id,
                            colors::GHOST_TEXT,
                        );
                    }
                });

                ui.add_space(8.0);

                // Results
                let mut clicked = None;
                let visible_height = MAX_VISIBLE_ITEMS as f32 * row_height;
                let scroll_to_selected = down || up;

                ScrollArea::vertical()
                    .max_height(visible_height)
                    .show(ui, |ui: &mut Ui| {
                    for (i, &idx) in self.filtered.iter().enumerate() {
                        let e = &self.entries[idx];
                        let sel = i == self.selected;
                        let is_window = e.is_window();

                        let row_y = ui.cursor().min.y;
                        let row_rect = egui::Rect::from_min_size(
                            egui::pos2(0.0, row_y),
                            egui::vec2(screen.width(), row_height),
                        );

                        if sel {
                            ui.painter().rect_filled(row_rect, 0.0, colors::BG_SELECTED);
                            if scroll_to_selected {
                                ui.scroll_to_rect(row_rect, Some(egui::Align::Center));
                            }
                        }

                        let text_color = if sel { colors::TEXT_PRIMARY } else { colors::TEXT_SECONDARY };

                        let (_, row_response) = ui.allocate_exact_size(
                            egui::vec2(content_width, row_height),
                            Sense::click(),
                        );

                        // Icon container - always render circle
                        let icon_rect = egui::Rect::from_min_size(
                            egui::pos2(ROW_PADDING, row_y + ROW_PADDING),
                            egui::vec2(ICON_CONTAINER, ICON_CONTAINER),
                        );

                        // Circle background
                        ui.painter().rect_filled(
                            icon_rect,
                            ICON_CONTAINER / 2.0,
                            colors::ICON_BG,
                        );

                        // Window indicator - accent ring
                        if is_window {
                            ui.painter().circle_stroke(
                                icon_rect.center(),
                                ICON_CONTAINER / 2.0,
                                Stroke::new(1.5, colors::ACCENT),
                            );
                        }

                        // Icon
                        if let Some(tex) = e.icon() {
                            let icon_offset = (ICON_CONTAINER - ICON_SIZE) / 2.0;
                            let img_rect = egui::Rect::from_min_size(
                                icon_rect.min + egui::vec2(icon_offset, icon_offset),
                                egui::vec2(ICON_SIZE, ICON_SIZE),
                            );
                            ui.painter().image(
                                tex.id(),
                                img_rect,
                                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                                Color32::WHITE,
                            );
                        }

                        // Text
                        let text_x = ROW_PADDING + ICON_CONTAINER + ICON_LABEL_SPACING;
                        let text_y = row_y + (row_height - TEXT_SIZE) / 2.0;
                        let text_font = FontId::new(TEXT_SIZE, FontFamily::Proportional);

                        // Calculate available width for text (leave room for workspace indicator if window)
                        let right_margin = if e.is_window() { WS_INDICATOR_WIDTH + ROW_PADDING * 2.0 } else { ROW_PADDING };
                        let available_width = content_width - text_x - right_margin;

                        // Truncate to fit available width
                        let display_name = truncate_to_width(ui, e.name(), text_font.clone(), available_width);
                        ui.painter().text(
                            egui::pos2(text_x, text_y),
                            egui::Align2::LEFT_TOP,
                            &display_name,
                            text_font,
                            text_color,
                        );

                        // Workspace indicator for windows (right-aligned)
                        if let Some(ws) = e.workspace() {
                            ui.painter().text(
                                egui::pos2(content_width - ROW_PADDING, text_y),
                                egui::Align2::RIGHT_TOP,
                                ws,
                                FontId::new(TEXT_SIZE * 0.85, FontFamily::Proportional),
                                colors::TEXT_MUTED,
                            );
                        }

                        if row_response.clicked() {
                            clicked = Some(i);
                        }
                    }
                });

                if let Some(i) = clicked {
                    self.selected = i;
                    self.activate();
                }
            });
    }
}

impl eframe::App for App {
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &Context, _: &mut eframe::Frame) {
        if !self.loaded {
            self.load_entries(ctx);
        }
        self.render(ctx);
        if self.should_hide {
            self.hide_and_reset();
        }
    }
}

fn main() -> eframe::Result<()> {
    let (width, height) = get_launcher_size();

    eframe::run_native(
        "launcher-layer",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([width, height])
                .with_decorations(false)
                .with_transparent(true)
                .with_app_id("launcher-layer"),
            ..Default::default()
        },
        Box::new(|cc| {
            let mut style = egui::Style::default();
            style.visuals.window_fill = Color32::TRANSPARENT;
            style.visuals.panel_fill = Color32::TRANSPARENT;
            cc.egui_ctx.set_style(style);
            Ok(Box::new(App::new()))
        }),
    )
}

fn get_launcher_size() -> (f32, f32) {
    let row_height = ICON_CONTAINER + ROW_PADDING * 2.0;
    let input_height = INPUT_SIZE + INPUT_PADDING * 2.0;
    // Height based on content: input + max visible items + padding
    let height = input_height + (MAX_VISIBLE_ITEMS as f32 * row_height) + 16.0;

    let width = Command::new("hyprctl").args(["monitors", "-j"]).output().ok()
        .and_then(|o| serde_json::from_slice::<Vec<serde_json::Value>>(&o.stdout).ok())
        .and_then(|m| m.first().and_then(|m| {
            let w = m["width"].as_f64()?;
            let s = m["scale"].as_f64().unwrap_or(1.0);
            let logical_w = w / s;
            // Golden ratio: width = 38.2% of screen
            Some((logical_w * 0.382) as f32)
        }))
        .unwrap_or(300.0);

    // Divide height by scale too
    let scale = Command::new("hyprctl").args(["monitors", "-j"]).output().ok()
        .and_then(|o| serde_json::from_slice::<Vec<serde_json::Value>>(&o.stdout).ok())
        .and_then(|m| m.first().and_then(|m| m["scale"].as_f64()))
        .unwrap_or(1.0);

    (width, (height / scale as f32))
}

fn build_icon_index() -> HashMap<String, PathBuf> {
    let mut index = HashMap::new();
    let sizes = ["48x48", "64x64", "128x128", "256x256", "32x32", "scalable"];

    let mut dirs = vec![
        PathBuf::from("/run/current-system/sw/share/icons"),
        PathBuf::from("/usr/share/icons"),
    ];
    if let Ok(home) = env::var("HOME") {
        dirs.insert(0, PathBuf::from(&home).join(".local/share/icons"));
        dirs.insert(0, PathBuf::from(&home).join(".icons"));
    }
    if let Ok(xdg) = env::var("XDG_DATA_DIRS") {
        for d in xdg.split(':') {
            dirs.push(PathBuf::from(d).join("icons"));
        }
    }

    for base in dirs {
        let hicolor = base.join("hicolor");
        for size in &sizes {
            for cat in ["apps", "applications"] {
                let dir = hicolor.join(size).join(cat);
                if let Ok(entries) = fs::read_dir(&dir) {
                    for e in entries.flatten() {
                        let path = e.path();
                        if path.extension().is_some_and(|e| e == "svg") { continue; }
                        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                            index.entry(stem.to_string()).or_insert(path);
                        }
                    }
                }
            }
        }
    }

    for pixmaps in ["/run/current-system/sw/share/pixmaps", "/usr/share/pixmaps"] {
        if let Ok(entries) = fs::read_dir(pixmaps) {
            for e in entries.flatten() {
                let path = e.path();
                if path.extension().is_some_and(|e| e == "svg") { continue; }
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    index.entry(stem.to_string()).or_insert(path);
                }
            }
        }
    }
    index
}

fn build_wmclass_icon_map(icon_index: &HashMap<String, PathBuf>) -> HashMap<String, PathBuf> {
    let mut map = HashMap::new();

    for dir in get_applications_dirs() {
        if let Ok(read_dir) = fs::read_dir(&dir) {
            for entry in read_dir.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "desktop") {
                    if let Ok(content) = fs::read_to_string(&path) {
                        let mut wmclass = None;
                        let mut icon_name = None;

                        for line in content.lines() {
                            if let Some(v) = line.strip_prefix("StartupWMClass=") {
                                wmclass = Some(v.to_lowercase());
                            } else if let Some(v) = line.strip_prefix("Icon=") {
                                icon_name = Some(v.to_string());
                            }
                        }

                        if let (Some(wm), Some(icon)) = (wmclass, icon_name) {
                            if let Some(icon_path) = icon_index.get(&icon) {
                                map.entry(wm).or_insert_with(|| icon_path.clone());
                            }
                        }
                    }
                }
            }
        }
    }
    map
}

fn get_applications_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();
    if let Ok(home) = env::var("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/applications"));
    }
    let data_dirs = env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    for dir in data_dirs.split(':') {
        dirs.push(PathBuf::from(dir).join("applications"));
    }
    dirs.push(PathBuf::from("/var/lib/flatpak/exports/share/applications"));
    dirs
}

fn load_icon(ctx: &Context, path: &PathBuf) -> Option<egui::TextureHandle> {
    let data = fs::read(path).ok()?;
    let img = image::load_from_memory(&data).ok()?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    Some(ctx.load_texture(
        path.to_string_lossy(),
        egui::ColorImage::from_rgba_unmultiplied(size, &img.into_raw()),
        egui::TextureOptions::LINEAR,
    ))
}

#[derive(serde::Deserialize)]
struct HyprWorkspace {
    id: i32,
    name: String,
}

#[derive(serde::Deserialize)]
struct HyprClient {
    address: String,
    title: String,
    class: String,
    workspace: HyprWorkspace,
    #[serde(rename = "focusHistoryID")]
    focus_history_id: i32,
}

fn collect_hyprland_windows(ctx: &Context, icon_index: &HashMap<String, PathBuf>, wmclass_icons: &HashMap<String, PathBuf>) -> Vec<Entry> {
    let output = Command::new("hyprctl")
        .args(["clients", "-j"])
        .output()
        .ok();

    let Some(output) = output else { return vec![] };
    if !output.status.success() { return vec![]; }

    let mut clients: Vec<HyprClient> = serde_json::from_slice(&output.stdout).unwrap_or_default();
    clients.sort_by_key(|c| c.focus_history_id);

    clients
        .into_iter()
        .filter(|c| !c.class.is_empty() && c.class != "launcher" && c.class != "launcher-layer")
        .filter(|c| !c.workspace.name.starts_with("special:"))
        .map(|c| {
            let class_lower = c.class.to_lowercase();
            let icon_path = wmclass_icons.get(&class_lower)
                .or_else(|| icon_index.get(&class_lower));
            let icon = icon_path.and_then(|p| load_icon(ctx, p));
            // Show workspace number (use name if it's a named workspace, otherwise id)
            let workspace = if c.workspace.id > 0 {
                c.workspace.id.to_string()
            } else {
                c.workspace.name.clone()
            };
            Entry::Window {
                title: c.title,
                class: c.class,
                address: c.address,
                workspace,
                icon,
            }
        })
        .collect()
}

fn collect_desktop_entries(ctx: &Context, icon_index: &HashMap<String, PathBuf>) -> Vec<Entry> {
    let mut seen_files = std::collections::HashSet::new();
    let mut entries = Vec::new();

    for dir in get_applications_dirs() {
        if let Ok(read_dir) = fs::read_dir(&dir) {
            for entry in read_dir.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "desktop") {
                    let key = path.file_name().unwrap().to_string_lossy().to_string();
                    if seen_files.insert(key) {
                        entries.extend(parse_desktop_file(ctx, &path, icon_index));
                    }
                }
            }
        }
    }

    entries.sort_by(|a, b| a.name().to_lowercase().cmp(&b.name().to_lowercase()));
    entries
}

fn parse_desktop_file(ctx: &Context, path: &PathBuf, icon_index: &HashMap<String, PathBuf>) -> Vec<Entry> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let mut entries = Vec::new();
    let mut main_name = None;
    let mut main_exec = None;
    let mut main_icon_name = None;
    let mut no_display = false;
    let mut hidden = false;
    let mut terminal = false;
    let mut actions_list: Vec<String> = Vec::new();
    let mut actions: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    let mut current_section = String::new();
    let mut current_action_id: Option<String> = None;

    for line in content.lines() {
        let line = line.trim();

        if line.starts_with('[') && line.ends_with(']') {
            current_section = line[1..line.len()-1].to_string();
            if current_section.starts_with("Desktop Action ") {
                current_action_id = Some(current_section["Desktop Action ".len()..].to_string());
            } else {
                current_action_id = None;
            }
            continue;
        }

        if let Some((key, value)) = line.split_once('=') {
            if current_section == "Desktop Entry" {
                match key {
                    "Name" if main_name.is_none() => main_name = Some(value.to_string()),
                    "Exec" => main_exec = Some(value.to_string()),
                    "Icon" => main_icon_name = Some(value.to_string()),
                    "NoDisplay" => no_display = value == "true",
                    "Hidden" => hidden = value == "true",
                    "Terminal" => terminal = value == "true",
                    "Actions" => {
                        actions_list = value.split(';').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect();
                    }
                    _ => {}
                }
            } else if let Some(ref action_id) = current_action_id {
                let entry = actions.entry(action_id.clone()).or_insert((None, None));
                match key {
                    "Name" => entry.0 = Some(value.to_string()),
                    "Exec" => entry.1 = Some(value.to_string()),
                    _ => {}
                }
            }
        }
    }

    if no_display || hidden {
        return vec![];
    }

    let icon = main_icon_name.as_ref().and_then(|name| {
        let p = PathBuf::from(name);
        if p.is_absolute() && p.exists() {
            load_icon(ctx, &p)
        } else {
            icon_index.get(name).and_then(|p| load_icon(ctx, p))
        }
    });

    // Main entry
    if let (Some(name), Some(exec)) = (main_name.clone(), main_exec.clone()) {
        entries.push(Entry::Desktop { name, exec, terminal, icon: icon.clone() });
    }

    // Action entries
    for action_id in actions_list {
        if let Some((Some(action_name), Some(action_exec))) = actions.get(&action_id) {
            let display_name = if let Some(ref app_name) = main_name {
                format!("{}: {}", app_name, action_name)
            } else {
                action_name.clone()
            };
            entries.push(Entry::Desktop {
                name: display_name,
                exec: action_exec.clone(),
                terminal,
                icon: icon.clone(),
            });
        }
    }

    entries
}
