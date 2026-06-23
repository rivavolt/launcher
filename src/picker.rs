//! Picker: dmenu-like line selector using the launcher's egui UI.
//! Reads lines from stdin, shows fuzzy-searchable list, prints selected line to stdout.

use eframe::egui::{self, CentralPanel, Context, ScrollArea, FontFamily, FontId, Ui};
use launcher::common::{self, colors, handle_navigation_keys, virtual_list};
use launcher::scroll::ScrollMomentum;
use launcher::hyprland;
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

struct App {
    items: Vec<String>,
    query: String,
    filtered: Vec<usize>,
    selected: usize,
    should_exit: bool,
    result: Option<String>,
    selected_flag: Arc<AtomicBool>,
    held_key: Option<(egui::Key, std::time::Instant)>,
    matcher: Matcher,
    scroll_momentum: ScrollMomentum,
    max_size: (f32, f32),
    last_height: f32,
    ghost_text_cache: String,
}

impl App {
    fn new(items: Vec<String>, selected_flag: Arc<AtomicBool>) -> Self {
        let eframe_size = hyprland::window_size(0.382, 0.618, (300.0, 400.0));
        let max_size = (eframe_size.0 * 2.0, eframe_size.1 * 2.0);
        let filtered: Vec<usize> = (0..items.len().min(50)).collect();
        Self {
            items,
            query: String::new(),
            filtered,
            selected: 0,
            should_exit: false,
            result: None,
            selected_flag,
            held_key: None,
            matcher: Matcher::new(Config::DEFAULT),
            scroll_momentum: ScrollMomentum::new(),
            max_size,
            last_height: 0.0,
            ghost_text_cache: String::new(),
        }
    }

    fn filter(&mut self) {
        if self.query.is_empty() {
            self.filtered = (0..self.items.len().min(50)).collect();
        } else {
            let pattern = Pattern::parse(&self.query, CaseMatching::Ignore, Normalization::Smart);
            let mut scored: Vec<_> = self.items.iter().enumerate()
                .filter_map(|(idx, item)| {
                    let mut buf = Vec::new();
                    let haystack = Utf32Str::new(item, &mut buf);
                    let score = pattern.score(haystack, &mut self.matcher)?;
                    let name_lower = item.to_lowercase();
                    let query_lower = self.query.to_lowercase();
                    let prefix_bonus: u32 = if name_lower.starts_with(&query_lower) { 10000 } else { 0 };
                    Some((score + prefix_bonus, idx))
                })
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0));
            self.filtered = scored.into_iter().map(|(_, idx)| idx).take(50).collect();
        }
        self.selected = 0;
        self.update_ghost_text();
    }

    fn update_ghost_text(&mut self) {
        self.ghost_text_cache.clear();
        if self.query.is_empty() { return; }
        if let Some(&idx) = self.filtered.first() {
            let name = &self.items[idx];
            let name_lower = name.to_lowercase();
            let query_lower = self.query.to_lowercase();
            if name_lower.starts_with(&query_lower) {
                self.ghost_text_cache = name.chars().skip(self.query.chars().count()).collect();
            }
        }
    }

    fn activate(&mut self) {
        if let Some(&idx) = self.filtered.get(self.selected) {
            self.result = Some(self.items[idx].clone());
            self.selected_flag.store(true, Ordering::SeqCst);
        }
        self.should_exit = true;
    }
}

impl eframe::App for App {
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &Context, _: &mut eframe::Frame) {
        if ctx.input(|i| i.viewport().close_requested()) {
            self.should_exit = true;
            return;
        }

        // Same 1px gold outline + rounded corners the launcher/clipboard draw,
        // so the picker reads as the same surface.
        common::popup_border(ctx);
        self.scroll_momentum.update(ctx);

        let max_sel = self.filtered.len().saturating_sub(1);
        let mut activate = false;

        let (down, up) = handle_navigation_keys(ctx, &mut self.held_key);

        ctx.input(|i| {
            for event in &i.events {
                if let egui::Event::Key { key, pressed: true, .. } = event {
                    match key {
                        egui::Key::Escape => self.should_exit = true,
                        egui::Key::Enter => activate = true,
                        _ => {}
                    }
                }
            }
        });

        if down { self.selected = (self.selected + 1).min(max_sel); }
        if up { self.selected = self.selected.saturating_sub(1); }
        if activate { self.activate(); }

        if self.should_exit {
            if let Some(ref result) = self.result {
                print!("{}", result);
            }
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }

        // Shared input panel: the `>` prompt, focus handling, ghost completion
        // and Ctrl+U — identical to the launcher and clipboard.
        let ghost = (!self.ghost_text_cache.is_empty()).then_some(self.ghost_text_cache.as_str());
        let input_panel = common::input_panel(ctx, &mut self.query, "Search...", ghost);
        // The picker is a normal toplevel window, so egui's `i.focused` — which
        // `input_panel` keys its focus on — isn't reliably set the way it is for
        // the layer-shell launcher/clipboard. Force keyboard focus onto the query
        // field every frame so typing always lands here (the dmenu contract); the
        // field consumed the keys at its `show()` this frame before `input_panel`
        // surrendered, so re-requesting here just holds focus for the next frame.
        ctx.memory_mut(|m| m.request_focus(input_panel.text_edit_id));
        if input_panel.changed || input_panel.cleared {
            self.filter();
        }
        let input_response = input_panel.response;

        CentralPanel::default()
            .frame(common::panel_frame())
            .show(ctx, |ui: &mut Ui| {
                let row_height = common::row_height();
                let header_height = input_response.response.rect.height();
                let spacing_y = ui.spacing().item_spacing.y;
                let max_visible = 12;
                let num_items = self.filtered.len().min(max_visible);
                let list_height = if num_items > 0 {
                    num_items as f32 * row_height + (num_items - 1) as f32 * spacing_y
                } else {
                    0.0
                };
                let target_height = (header_height + list_height).min(self.max_size.1);
                if (target_height - self.last_height).abs() > 1.0 {
                    self.last_height = target_height;
                    hyprland::dispatch_async(&format!(
                        r#"hl.dsp.window.resize({{ x = {}, y = {}, window = "class:picker" }})"#,
                        self.max_size.0 as i32, target_height as i32,
                    ));
                }

                let visible_height = (self.max_size.1 - header_height).max(row_height);
                let scroll_to_selected = down || up;
                let text_size = common::text_size();
                let text_font = FontId::new(text_size, FontFamily::Proportional);

                let filtered = &self.filtered;
                let items = &self.items;
                let query = &self.query;
                // Start row text on the launcher's text column — aligned under the
                // input text, past the `>` prompt's icon-sized container + gap — so
                // the query field and the rows share one left edge.
                let text_x = (text_size * 0.5).round()
                    + common::icon_container()
                    + (text_size * 0.625).round();

                let scroll_output = ScrollArea::vertical()
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
                                let item = &items[idx];
                                let sel = i == self.selected;
                                let text_color = common::row_text_color(sel);
                                let text_y = row_rect.min.y + (row_height - text_size) / 2.0;
                                // Highlight the matched query characters in gold,
                                // exactly like the launcher's result rows.
                                let matches = common::match_indices(item, query);
                                common::paint_highlighted(
                                    ui,
                                    egui::pos2(text_x, text_y),
                                    item,
                                    &text_font,
                                    text_color,
                                    colors::ACCENT,
                                    &matches,
                                );
                            },
                        )
                    });

                common::paint_scroll_fade(ui, scroll_output.inner_rect, 16.0);

                if let Some(i) = scroll_output.inner.clicked {
                    self.selected = i;
                    self.activate();
                }
            });
    }
}

fn main() {
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap_or(0);
    let items: Vec<String> = input.lines().filter(|l| !l.is_empty()).map(String::from).collect();

    if items.is_empty() {
        std::process::exit(1);
    }

    let selected_flag = Arc::new(AtomicBool::new(false));
    let flag_clone = selected_flag.clone();

    let (width, height) = hyprland::window_size(0.382, 0.618, (300.0, 400.0));

    let _ = eframe::run_native(
        "picker",
        common::window_options("picker", width, height),
        Box::new(move |cc| {
            common::setup_transparent_style(&cc.egui_ctx);
            Ok(Box::new(App::new(items, flag_clone)))
        }),
    );

    if !selected_flag.load(Ordering::SeqCst) {
        std::process::exit(1);
    }
}
