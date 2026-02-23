//! Picker: dmenu-like line selector using the launcher's egui UI.
//! Reads lines from stdin, shows fuzzy-searchable list, prints selected line to stdout.

use eframe::egui::{self, CentralPanel, Context, RichText, ScrollArea, FontFamily, FontId, Ui};
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
                output.response.request_focus();
                if self.query != old_query { self.filter(); }

                if !self.ghost_text_cache.is_empty() && !self.query.is_empty() {
                    let mut job = egui::text::LayoutJob::default();
                    job.append(&self.query, 0.0, egui::TextFormat {
                        font_id: input_font.clone(),
                        color: egui::Color32::TRANSPARENT,
                        ..Default::default()
                    });
                    job.append(&self.ghost_text_cache, 0.0, egui::TextFormat {
                        font_id: input_font,
                        color: colors::GHOST_TEXT,
                        ..Default::default()
                    });
                    let galley = ui.fonts(|f| f.layout_job(job));
                    ui.painter().galley(output.galley_pos, galley, egui::Color32::TRANSPARENT);
                }
            });

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
                    hyprland::dispatch_async(
                        "resizewindowpixel",
                        &format!("exact {} {},class:picker", self.max_size.0 as i32, target_height as i32),
                    );
                }

                let visible_height = (self.max_size.1 - header_height).max(row_height);
                let scroll_to_selected = down || up;
                let text_size = common::text_size();
                let text_font = FontId::new(text_size, FontFamily::Proportional);

                let filtered = &self.filtered;
                let items = &self.items;

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
                                let item = &items[idx];
                                let sel = i == self.selected;
                                let text_color = if sel { colors::TEXT_PRIMARY } else { colors::TEXT_SECONDARY };
                                let text_y = row_rect.min.y + (row_height - text_size) / 2.0;
                                ui.painter().text(
                                    egui::pos2(12.0, text_y),
                                    egui::Align2::LEFT_TOP,
                                    item,
                                    text_font.clone(),
                                    text_color,
                                );
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
            common::setup_transparent_style(cc);
            Ok(Box::new(App::new(items, flag_clone)))
        }),
    );

    if !selected_flag.load(Ordering::SeqCst) {
        std::process::exit(1);
    }
}
