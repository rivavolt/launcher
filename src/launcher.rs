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

/// Find character indices where the query matches at word boundaries or as substring
fn match_indices(text: &str, query: &str) -> Vec<usize> {
    if query.is_empty() { return vec![]; }
    let text_lower = text.to_lowercase();
    let query_lower = query.to_lowercase();
    // Try word-start matching first: greedily match query chars at word boundaries
    let mut indices = Vec::new();
    let mut qi = 0;
    let query_chars: Vec<char> = query_lower.chars().collect();
    // Try substring match first (contiguous)
    if let Some(start) = text_lower.find(&query_lower) {
        let mut pos = start;
        for _ in 0..query_lower.len() {
            indices.push(pos);
            pos += text_lower[pos..].chars().next().map_or(1, |c| c.len_utf8());
        }
        return indices;
    }
    // Fall back to fuzzy: prefer word starts, then sequential
    for (ci, ch) in text_lower.char_indices() {
        if qi >= query_chars.len() { break; }
        if ch == query_chars[qi] {
            indices.push(ci);
            qi += 1;
        }
    }
    if qi == query_chars.len() { indices } else { vec![] }
}

/// Paint text with highlighted match indices
fn paint_highlighted(
    ui: &Ui,
    pos: egui::Pos2,
    text: &str,
    font: &FontId,
    base_color: Color32,
    highlight_color: Color32,
    match_indices: &[usize],
) {
    if match_indices.is_empty() {
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
    for &idx in match_indices {
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
        focus_history_id: i32,
    },
}

impl Entry {
    fn name(&self) -> &str {
        match self {
            Entry::Desktop { name, .. } => name,
            Entry::Window { class, .. } => class,
        }
    }

    fn subtitle(&self) -> Option<&str> {
        match self {
            Entry::Window { title, class, .. } if !title.is_empty() && title != class => Some(title),
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
    was_focused: bool,
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
            was_focused: false,
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

    fn default_order(&self) -> Vec<usize> {
        let mut indices: Vec<usize> = (0..self.entries.len().min(50)).collect();
        // Previously focused windows first; skip fhid 0 (launcher) and 1 (window you just left)
        indices.sort_by_key(|&i| match &self.entries[i] {
            Entry::Window { focus_history_id, .. } if *focus_history_id <= 1 => (1, *focus_history_id),
            Entry::Window { focus_history_id, .. } => (0, *focus_history_id),
            Entry::Desktop { .. } => (2, i as i32),
        });
        indices
    }

    fn filter(&mut self) {
        if self.query.is_empty() {
            self.filtered = self.default_order();
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

                    // Bonus for matching at a word boundary in any searchable field
                    let word_start_bonus: u32 = if e.searchable().iter().any(|s| {
                        let s_lower = s.to_lowercase();
                        s_lower.split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(&query_lower))
                    }) { 4000 } else { 0 };

                    let name_bonus: u32 = {
                        let mut buf = Vec::new();
                        let haystack = Utf32Str::new(e.name(), &mut buf);
                        if pattern.score(haystack, &mut self.matcher).unwrap_or(0) > 0
                            || name_lower.contains(&query_lower)
                        { 5000 } else { 0 }
                    };

                    // Open windows rank above desktop entries for same app
                    let window_bonus: u32 = if e.is_window() { 3000 } else { 0 };

                    // Recently focused windows rank higher (skip launcher=0 and just-left=1)
                    let recency_bonus: u32 = match e {
                        Entry::Window { focus_history_id, .. } if *focus_history_id > 1 => {
                            2000 / (*focus_history_id as u32 - 1)
                        }
                        _ => 0,
                    };

                    // Shorter names rank higher (query coverage ratio)
                    let length_bonus: u32 = {
                        let ratio = query_lower.len() as f32 / name_lower.len().max(1) as f32;
                        (ratio.min(1.0) * 1000.0) as u32
                    };

                    let base_score = nucleo_score.max(jw_score) + prefix_bonus + name_bonus + word_start_bonus;
                    if base_score == 0 { return None; }

                    let match_score = base_score
                        + window_bonus + recency_bonus + length_bonus;
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
        self.filtered = self.default_order();
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

        let mut accept_ghost = false;
        let mut clear_input = false;
        ctx.input(|i: &egui::InputState| {
            for event in &i.events {
                if let egui::Event::Key { key, pressed: true, modifiers, .. } = event {
                    match key {
                        egui::Key::Escape => self.should_hide = true,
                        egui::Key::Enter => activate = true,
                        egui::Key::Tab => accept_ghost = true,
                        egui::Key::U if modifiers.ctrl => clear_input = true,
                        _ => {}
                    }
                }
            }
        });

        if clear_input && !self.query.is_empty() {
            self.query.clear();
            self.filter();
        }

        if accept_ghost && !self.ghost_text_cache.is_empty() {
            self.query.push_str(&self.ghost_text_cache);
            self.filter();
        }

        if down { self.selected = (self.selected + 1).min(max_sel); }
        if up { self.selected = self.selected.saturating_sub(1); }
        if down || up {
            if let Some(&idx) = self.filtered.get(self.selected) {
                if let Entry::Window { ref workspace, ref address, .. } = self.entries[idx] {
                    hyprland::dispatch_batch_async(&[
                        ("workspace", workspace),
                        ("alterzorder", &format!("top,address:{}", address)),
                        ("focuswindow", "class:launcher"),
                    ]);
                }
            }
        }
        if activate { self.activate(); return; }

        // Input panel
        let input_response = egui::TopBottomPanel::top("input")
            .frame(common::input_frame())
            .show(ctx, |ui: &mut Ui| {
                let input_font = FontId::new(common::input_size(), FontFamily::Proportional);
                let old_query = self.query.clone();
                let output = ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 8.0;
                    ui.label(RichText::new(">").font(input_font.clone()).color(colors::TEXT_SUBTITLE));
                    egui::TextEdit::singleline(&mut self.query)
                        .font(input_font.clone())
                        .text_color(colors::TEXT_PRIMARY)
                        .hint_text(RichText::new("Search...").color(colors::TEXT_MUTED))
                        .frame(false)
                        .desired_width(ui.available_width())
                        .show(ui)
                }).inner;
                output.response.request_focus();
                if self.query != old_query { self.filter(); }

                // Move cursor to end after ghost text acceptance
                if accept_ghost {
                    if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), output.response.id) {
                        let ccursor = egui::text::CCursor::new(self.query.chars().count());
                        state.cursor.set_char_range(Some(egui::text::CCursorRange::one(ccursor)));
                        state.store(ui.ctx(), output.response.id);
                    }
                }

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
                } else if !self.query.is_empty() {
                    row_height
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
                let subtitle_size = (text_size / common::GOLDEN).round();
                let subtitle_font = FontId::new(subtitle_size, FontFamily::Proportional);
                let line_gap = 2.0;

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

                if self.filtered.is_empty() && !self.query.is_empty() {
                    ui.add_space(row_height / 2.0);
                    ui.vertical_centered(|ui| {
                        ui.label(RichText::new("No results").font(text_font.clone()).color(colors::TEXT_MUTED));
                    });
                } else {
                    let filtered = &self.filtered;
                    let entries = &self.entries;
                    let display_names = &self.display_names;
                    let query = &self.query;

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
                                    let tint = if sel { Color32::WHITE } else { Color32::from_rgba_premultiplied(128, 128, 128, 128) };
                                    ui.painter().image(
                                        tex.id(),
                                        img_rect,
                                        egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                                        tint,
                                    );
                                }

                                let display_name = display_names.get(&idx).map(|s| s.as_str()).unwrap_or(e.name());
                                let highlight = colors::TEXT_PRIMARY;
                                if let Some(sub) = e.subtitle() {
                                    let right_margin = icon_container() + row_padding() * 2.0;
                                    let avail = content_width - text_x - right_margin;
                                    let title_display = truncate_to_width(ui, sub, text_font.clone(), avail);
                                    let total_h = text_size + line_gap + subtitle_size;
                                    let primary_y = row_y + (row_height - total_h) / 2.0;
                                    let title_matches = match_indices(&title_display, query);
                                    paint_highlighted(ui, egui::pos2(text_x, primary_y), &title_display, &text_font, text_color, highlight, &title_matches);
                                    let sub_color = if sel { colors::TEXT_SECONDARY } else { colors::TEXT_SUBTITLE };
                                    let name_matches = match_indices(display_name, query);
                                    paint_highlighted(ui, egui::pos2(text_x, primary_y + text_size + line_gap), display_name, &subtitle_font, sub_color, highlight, &name_matches);
                                } else {
                                    let text_y = row_y + (row_height - text_size) / 2.0;
                                    let name_matches = match_indices(display_name, query);
                                    paint_highlighted(ui, egui::pos2(text_x, text_y), display_name, &text_font, text_color, highlight, &name_matches);
                                }

                                if let Entry::Window { workspace, .. } = e {
                                    let dot_x = content_width - row_padding() - icon_container() / 2.0;
                                    ui.painter().circle_filled(
                                        egui::pos2(dot_x, row_y + row_height / 2.0),
                                        3.0,
                                        colors::ACCENT,
                                    );
                                    let ws_color = if sel { colors::TEXT_SUBTITLE } else { colors::TEXT_MUTED };
                                    ui.painter().text(
                                        egui::pos2(dot_x - 8.0, row_y + row_height / 2.0),
                                        egui::Align2::RIGHT_CENTER,
                                        workspace,
                                        subtitle_font.clone(),
                                        ws_color,
                                    );
                                }
                            },
                        )
                    }).inner;

                    if let Some(i) = vl_output.clicked {
                        self.selected = i;
                        self.activate();
                    }
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

        let focused = ctx.input(|i| i.focused);

        // Render content when unfocused but skip input processing
        if !focused {
            self.was_focused = false;
            self.render(ctx);
            return;
        }

        // Reload entries on focus gain (fresh focus_history_ids)
        if !self.was_focused {
            self.was_focused = true;
            self.load_entries(ctx);
        } else if self.needs_reload.swap(false, Ordering::SeqCst) {
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
        .filter(|c| !c.pinned)
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
                focus_history_id: c.focus_history_id,
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
