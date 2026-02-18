//! App launcher using eframe (regular window in special workspace)

use eframe::egui::{self, CentralPanel, Context, Color32, RichText, ScrollArea, Ui, FontFamily, FontId};
use launcher::common::{self, colors, handle_navigation_keys, virtual_list};
use launcher::scroll::ScrollMomentum;
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

const MAX_VISIBLE_ITEMS: usize = 15;

fn icon_size() -> f32 { (common::text_size() * 1.5).round() }
fn icon_container() -> f32 { icon_size() + 4.0 }
fn row_padding() -> f32 { (common::text_size() * 0.5).round() }
fn icon_label_spacing() -> f32 { (common::text_size() * 0.625).round() }

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
    scroll_momentum: ScrollMomentum,
    max_size: (f32, f32),
    last_height: f32,
    // Caches
    ghost_text_cache: String,
    display_names: HashMap<usize, String>,
    last_content_width: f32,
}

impl App {
    fn new() -> Self {
        let eframe_size = hyprland::window_size(0.382, 0.618, (300.0, 400.0));
        // Store max size in hyprland logical coords (matches egui screen_rect)
        let max_size = (eframe_size.0 * 2.0, eframe_size.1 * 2.0);
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
            scroll_momentum: ScrollMomentum::new(),
            max_size,
            last_height: 0.0,
            ghost_text_cache: String::new(),
            display_names: HashMap::new(),
            last_content_width: 0.0,
        }
    }

    fn setup_hyprland_events(&mut self, ctx: &Context) {
        let needs_reload = self.needs_reload.clone();
        let ctx = ctx.clone();

        self._hypr_thread = hyprland::subscribe_events(move |line| {
            if line.starts_with("openwindow>>") || line.starts_with("closewindow>>") {
                needs_reload.store(true, Ordering::SeqCst);
                ctx.request_repaint();
            } else if line.starts_with("windowtitle>>") || line.starts_with("movewindow>>") {
                // Mark for reload but don't repaint - will refresh when focused
                needs_reload.store(true, Ordering::SeqCst);
            }
        });
    }

    fn load_entries(&mut self, ctx: &Context) {
        let old_selected = self.selected;
        let mut icon_index = desktop::build_icon_index();
        desktop::cache_svgs(&mut icon_index);
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

                    let name_lower = e.name().to_lowercase();
                    let prefix_bonus: u32 = if name_lower.starts_with(&query_lower)
                    { 10000 } else { 0 };

                    let name_bonus: u32 = {
                        let mut buf = Vec::new();
                        let haystack = Utf32Str::new(e.name(), &mut buf);
                        if pattern.score(haystack, &mut self.matcher).unwrap_or(0) > 0
                            || name_lower.contains(&query_lower)
                        { 5000 } else { 0 }
                    };

                    let match_score = nucleo_score.max(jw_score) + prefix_bonus + name_bonus;
                    if match_score == 0 { return None; }
                    Some((match_score, idx))
                })
                .collect();

            scored.sort_by(|a, b| b.0.cmp(&a.0));
            self.filtered = scored.into_iter().map(|(_, idx)| idx).take(50).collect();
        }
        self.selected = 0;
        self.display_names.clear(); // Invalidate truncation cache
        self.update_ghost_text();
    }

    fn update_ghost_text(&mut self) {
        self.ghost_text_cache.clear();
        if self.query.is_empty() {
            return;
        }
        if let Some(&idx) = self.filtered.first() {
            let name = self.entries[idx].name();
            let name_lower = name.to_lowercase();
            let query_lower = self.query.to_lowercase();
            if name_lower.starts_with(&query_lower) {
                self.ghost_text_cache = name.chars().skip(self.query.chars().count()).collect();
            }
        }
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
                            let term_bin = term.split_whitespace().next().unwrap_or("kitty");
                            let _ = Command::new(term_bin)
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

        // Input panel
        let input_response = egui::TopBottomPanel::top("input")
            .frame(common::input_frame())
            .show(ctx, |ui: &mut Ui| {
                let input_font = FontId::new(common::input_size(), FontFamily::Proportional);
                let old_query = self.query.clone();
                let input = egui::TextEdit::singleline(&mut self.query)
                    .font(input_font.clone())
                    .text_color(colors::TEXT_PRIMARY)
                    .hint_text(RichText::new("Search...").color(colors::TEXT_MUTED))
                    .frame(false)
                    .desired_width(ui.available_width());
                let output = input.show(ui);
                if ui.ctx().input(|i| i.focused) {
                    output.response.request_focus();
                } else {
                    output.response.surrender_focus();
                }
                if self.query != old_query { self.filter(); }

                if !self.ghost_text_cache.is_empty() && !self.query.is_empty() {
                    let mut job = egui::text::LayoutJob::default();
                    job.append(&self.query, 0.0, egui::TextFormat {
                        font_id: input_font.clone(),
                        color: Color32::TRANSPARENT,
                        ..Default::default()
                    });
                    job.append(&self.ghost_text_cache, 0.0, egui::TextFormat {
                        font_id: input_font,
                        color: colors::GHOST_TEXT,
                        ..Default::default()
                    });
                    let galley = ui.fonts(|f| f.layout_job(job));
                    ui.painter().galley(output.galley_pos, galley, Color32::TRANSPARENT);
                }
            });

        // List panel
        CentralPanel::default()
            .frame(common::panel_frame())
            .show(ctx, |ui: &mut Ui| {
                let content_width = ui.available_width();
                let row_height = icon_container() + row_padding() * 2.0;
                let header_height = input_response.response.rect.height();
                let spacing_y = ui.spacing().item_spacing.y;

                // Auto-resize window to fit content
                let num_items = self.filtered.len().min(MAX_VISIBLE_ITEMS);
                let list_height = if num_items > 0 {
                    num_items as f32 * row_height + (num_items - 1) as f32 * spacing_y
                } else {
                    0.0
                };
                let target_height = (header_height + list_height).min(self.max_size.1);
                if (target_height - self.last_height).abs() > 1.0 {
                    self.last_height = target_height;
                    hyprland::dispatch_async(
                        "resizewindowpixel",
                        &format!("exact {} {},class:launcher", self.max_size.0 as i32, target_height as i32),
                    );
                }

                let visible_height = (self.max_size.1 - header_height).max(row_height);
                let scroll_to_selected = down || up;

                let text_size = common::text_size();
                let text_font = FontId::new(text_size, FontFamily::Proportional);
                let ws_font = FontId::new(text_size * 0.8, FontFamily::Monospace);
                let text_x = row_padding() + icon_container() + icon_label_spacing();

                // Cache display names on width change
                if (self.last_content_width - content_width).abs() > 1.0 || self.display_names.is_empty() {
                    self.last_content_width = content_width;
                    self.display_names.clear();
                    for &idx in &self.filtered {
                        let e = &self.entries[idx];
                        let right_margin = if e.is_window() { icon_container() + row_padding() * 2.0 } else { row_padding() };
                        let available_width = content_width - text_x - right_margin;
                        let display_name = truncate_to_width(ui, e.name(), text_font.clone(), available_width);
                        self.display_names.insert(idx, display_name);
                    }
                }

                let filtered = &self.filtered;
                let entries = &self.entries;
                let display_names = &self.display_names;

                let vl_output = ScrollArea::vertical()
                    .max_height(visible_height)
                    .show(ui, |ui: &mut Ui| {
                    virtual_list(
                        ui,
                        filtered.len(),
                        row_height,
                        self.selected,
                        scroll_to_selected,
                        false,
                        |ui, i, row_rect| {
                            let idx = filtered[i];
                            let e = &entries[idx];
                            let sel = i == self.selected;
                            let text_color = if sel { colors::TEXT_PRIMARY } else { colors::TEXT_SECONDARY };
                            let row_y = row_rect.min.y;

                            if let Some(tex) = e.icon() {
                                let img_rect = egui::Rect::from_min_size(
                                    egui::pos2(row_padding() + (icon_container() - icon_size()) / 2.0, row_y + row_padding() + (icon_container() - icon_size()) / 2.0),
                                    egui::vec2(icon_size(), icon_size()),
                                );
                                ui.painter().image(
                                    tex.id(),
                                    img_rect,
                                    egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                                    Color32::WHITE,
                                );
                            }

                            let text_y = row_y + (row_height - text_size) / 2.0;
                            let display_name = display_names.get(&idx).map(|s| s.as_str()).unwrap_or(e.name());
                            ui.painter().text(
                                egui::pos2(text_x, text_y),
                                egui::Align2::LEFT_TOP,
                                display_name,
                                text_font.clone(),
                                text_color,
                            );

                            if let Some(ws) = e.workspace() {
                                let badge_center = egui::pos2(
                                    content_width - row_padding() - icon_container() / 2.0,
                                    row_y + row_height / 2.0,
                                );
                                let badge_r = icon_container() / 2.0;
                                ui.painter().circle_filled(badge_center, badge_r, colors::BG_SELECTED);
                                ui.painter().text(
                                    badge_center,
                                    egui::Align2::CENTER_CENTER,
                                    ws,
                                    ws_font.clone(),
                                    colors::ACCENT,
                                );
                            }
                        },
                    )
                }).inner;

                if let Some(i) = vl_output.clicked {
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
        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.should_hide = true;
        }

        if self._hypr_thread.is_none() {
            self.setup_hyprland_events(ctx);
        }

        // Render content when unfocused but skip input processing
        if !ctx.input(|i| i.focused) {
            self.render(ctx);
            return;
        }

        if self.needs_reload.swap(false, Ordering::SeqCst) {
            self.load_entries(ctx);
        }

        if !self.loaded {
            self.load_entries(ctx);
        }
        self.scroll_momentum.update(ctx);
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
        common::window_options("launcher", width, height),
        Box::new(|cc| {
            common::setup_transparent_style(cc);
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
