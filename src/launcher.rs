//! App launcher using eframe (regular window in special workspace)

use eframe::egui::{self, CentralPanel, Context, Frame, Color32, RichText, ScrollArea, Sense, Ui, FontFamily, FontId, Stroke};
use launcher::common::{colors, handle_navigation_keys, TEXT_SIZE, INPUT_SIZE, INPUT_PADDING};
use launcher::{desktop, hyprland};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::{env, fs};
use strsim::jaro_winkler;

// Launcher-specific layout
const ICON_SIZE: f32 = 20.0;
const ICON_CONTAINER: f32 = 24.0;
const ROW_PADDING: f32 = 6.0;
const ICON_LABEL_SPACING: f32 = 8.0;
const MAX_VISIBLE_ITEMS: usize = 15;
const WS_INDICATOR_WIDTH: f32 = 28.0;

fn truncate_to_width(ui: &Ui, s: &str, font: FontId, max_width: f32) -> String {
    let full = ui.painter().layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE);
    if full.rect.width() <= max_width {
        return s.to_string();
    }

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
        keywords: Vec<String>,
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
            Entry::Desktop { name, keywords, .. } => {
                let mut v = vec![name.as_str()];
                v.extend(keywords.iter().map(|s| s.as_str()));
                v
            }
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
    activated_window: bool,
    loaded: bool,
    held_key: Option<(egui::Key, std::time::Instant)>,
    matcher: Matcher,
    needs_reload: Arc<AtomicBool>,
    _hypr_thread: Option<std::thread::JoinHandle<()>>,
}

impl App {
    fn new() -> Self {
        Self {
            query: String::new(),
            entries: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            should_hide: false,
            activated_window: false,
            loaded: false,
            held_key: None,
            matcher: Matcher::new(Config::DEFAULT),
            needs_reload: Arc::new(AtomicBool::new(false)),
            _hypr_thread: None,
        }
    }

    fn setup_hyprland_events(&mut self, ctx: &Context) {
        let needs_reload = self.needs_reload.clone();
        let ctx = ctx.clone();

        self._hypr_thread = hyprland::subscribe_events(move |line| {
            if line.starts_with("openwindow>>")
                || line.starts_with("closewindow>>")
                || line.starts_with("windowtitle>>")
                || line.starts_with("movewindow>>")
            {
                needs_reload.store(true, Ordering::SeqCst);
                ctx.request_repaint();
            }
        });
    }

    fn load_entries(&mut self, ctx: &Context) {
        let old_selected = self.selected;
        let icon_index = desktop::build_icon_index();
        let desktop_entries = desktop::collect_entries();
        let wmclass_icons = desktop::wmclass_icon_map(&desktop_entries, &icon_index);

        self.entries = collect_hyprland_windows(ctx, &icon_index, &wmclass_icons);
        self.entries.extend(convert_desktop_entries(ctx, &icon_index, desktop_entries));

        self.filter();
        self.selected = old_selected.min(self.filtered.len().saturating_sub(1));
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
                    let nucleo_score: u32 = e.searchable().iter()
                        .filter_map(|s| {
                            let mut buf = Vec::new();
                            let haystack = Utf32Str::new(s, &mut buf);
                            pattern.score(haystack, &mut self.matcher)
                        })
                        .max()
                        .unwrap_or(0);

                    let jw_score: u32 = if nucleo_score == 0 {
                        e.searchable().iter()
                            .map(|s| (jaro_winkler(&query_lower, &s.to_lowercase()) * 1000.0) as u32)
                            .filter(|&s| s >= 850)
                            .max()
                            .unwrap_or(0)
                    } else {
                        0
                    };

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
                    hyprland::dispatch("focuswindow", &format!("address:{}", address));
                    self.activated_window = true;
                }
            }
        }
        self.should_hide = true;
    }

    fn hide_and_reset(&mut self) {
        self.query.clear();
        self.selected = 0;
        self.filtered = (0..self.entries.len().min(50)).collect();
        self.should_hide = false;
        if !self.activated_window {
            hyprland::dispatch_async("togglespecialworkspace", "launcher");
        }
        self.activated_window = false;
    }

    fn render(&mut self, ctx: &Context) {
        if self.entries.is_empty() {
            self.load_entries(ctx);
        }

        let max_sel = self.filtered.len().saturating_sub(1);
        let mut activate = false;

        let (down, up) = handle_navigation_keys(ctx, &mut self.held_key);

        ctx.input(|i: &egui::InputState| {
            for event in &i.events {
                if let egui::Event::Key { key, pressed: true, .. } = event {
                    match key {
                        egui::Key::Escape => self.should_hide = true,
                        egui::Key::Enter => activate = true,
                        _ => {}
                    }
                }
            }
        });

        if down { self.selected = (self.selected + 1).min(max_sel); }
        if up { self.selected = self.selected.saturating_sub(1); }
        if activate { self.activate(); return; }

        let row_height = ICON_CONTAINER + ROW_PADDING * 2.0;

        CentralPanel::default()
            .frame(Frame::NONE)
            .show(ctx, |ui: &mut Ui| {
                let screen = ui.available_rect_before_wrap();
                let content_width = screen.width();

                let font_id = FontId::new(INPUT_SIZE, FontFamily::Proportional);
                let ghost = self.ghost_text();

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

                    if !ghost.is_empty() && !self.query.is_empty() {
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

                        let icon_rect = egui::Rect::from_min_size(
                            egui::pos2(ROW_PADDING, row_y + ROW_PADDING),
                            egui::vec2(ICON_CONTAINER, ICON_CONTAINER),
                        );

                        if is_window {
                            ui.painter().circle_stroke(
                                icon_rect.center(),
                                ICON_CONTAINER / 2.0,
                                Stroke::new(1.5, colors::ACCENT),
                            );
                        }

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

                        let text_x = ROW_PADDING + ICON_CONTAINER + ICON_LABEL_SPACING;
                        let text_y = row_y + (row_height - TEXT_SIZE) / 2.0;
                        let text_font = FontId::new(TEXT_SIZE, FontFamily::Proportional);

                        let right_margin = if e.is_window() { WS_INDICATOR_WIDTH + ROW_PADDING * 2.0 } else { ROW_PADDING };
                        let available_width = content_width - text_x - right_margin;

                        let display_name = truncate_to_width(ui, e.name(), text_font.clone(), available_width);
                        ui.painter().text(
                            egui::pos2(text_x, text_y),
                            egui::Align2::LEFT_TOP,
                            &display_name,
                            text_font,
                            text_color,
                        );

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
        if self._hypr_thread.is_none() {
            self.setup_hyprland_events(ctx);
        }

        if self.needs_reload.swap(false, Ordering::SeqCst) {
            self.load_entries(ctx);
        }

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
    let (width, height) = hyprland::window_size(0.382, 0.618, (300.0, 400.0));

    eframe::run_native(
        "launcher",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([width, height])
                .with_decorations(false)
                .with_transparent(true)
                .with_app_id("launcher"),
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

fn collect_hyprland_windows(ctx: &Context, icon_index: &HashMap<String, PathBuf>, wmclass_icons: &HashMap<String, PathBuf>) -> Vec<Entry> {
    hyprland::clients()
        .into_iter()
        .filter(|c| !c.class.is_empty() && c.class != "launcher")
        .filter(|c| !c.workspace.name.starts_with("special:"))
        .map(|c| {
            let class_lower = c.class.to_lowercase();
            let icon_path = wmclass_icons.get(&class_lower)
                .or_else(|| icon_index.get(&class_lower));
            let icon = icon_path.and_then(|p| load_icon(ctx, p));
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

fn convert_desktop_entries(ctx: &Context, icon_index: &HashMap<String, PathBuf>, entries: Vec<desktop::DesktopEntry>) -> Vec<Entry> {
    entries
        .into_iter()
        .map(|de| {
            let icon = de.icon.as_ref()
                .and_then(|i| i.resolve(icon_index))
                .and_then(|p| load_icon(ctx, &p));
            let mut keywords = de.keywords;
            if let Some(gn) = de.generic_name {
                keywords.push(gn);
            }
            Entry::Desktop {
                name: de.name,
                exec: de.exec,
                terminal: de.terminal,
                icon,
                keywords,
            }
        })
        .collect()
}
