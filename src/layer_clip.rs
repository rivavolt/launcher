//! Clipboard manager using eframe (regular window in special workspace)

use eframe::egui::{self, CentralPanel, Context, Frame, Color32, RichText, ScrollArea, Sense, Ui, FontFamily, FontId};
use regex::Regex;
use std::collections::HashMap;
use std::io::Write as IoWrite;
use std::path::PathBuf;
use std::process::{Command, Stdio};

// Layout constants - match launcher
const GOLDEN: f32 = 1.618;
const TEXT_SIZE: f32 = 16.0;
const INPUT_SIZE: f32 = TEXT_SIZE * GOLDEN;
const INPUT_PADDING: f32 = 8.0 * GOLDEN;
const ROW_HEIGHT: f32 = 32.0;
const MAX_VISIBLE_ITEMS: usize = 12;

// Colors
mod colors {
    use eframe::egui::Color32;
    pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(225, 225, 225);
    pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(140, 140, 140);
    pub const TEXT_MUTED: Color32 = Color32::from_rgb(80, 80, 80);
    pub const BG_SELECTED: Color32 = Color32::from_rgba_premultiplied(60, 100, 160, 50);
}

struct Entry {
    id: String,
    text: String,
    is_image: bool,
    dims: Option<(u32, u32)>,
    texture: Option<egui::TextureHandle>,
}

struct App {
    query: String,
    entries: Vec<Entry>,
    filtered: Vec<usize>,
    selected: usize,
    re: Regex,
    textures: HashMap<String, egui::TextureHandle>,
    should_hide: bool,
    loaded: bool,
    held_key: Option<(egui::Key, std::time::Instant)>,
}

impl App {
    fn new() -> Self {
        let re = Regex::new(r"\[\[\s*binary data\s+[\d.]+\s*\w+\s+(\w+)\s+(\d+)x(\d+)\s*\]\]").unwrap();
        Self {
            query: String::new(),
            entries: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            re,
            textures: HashMap::new(),
            should_hide: false,
            loaded: false,
            held_key: None,
        }
    }

    fn load_entries(&mut self, ctx: &Context) {
        self.entries = collect(ctx, &self.re, &mut self.textures);
        self.filtered = (0..self.entries.len().min(50)).collect();
        self.loaded = true;
    }

    fn filter(&mut self) {
        let q = self.query.to_lowercase();
        self.filtered = if q.is_empty() {
            (0..self.entries.len().min(50)).collect()
        } else {
            self.entries.iter().enumerate()
                .filter(|(_, e)| !e.is_image && e.text.to_lowercase().contains(&q))
                .map(|(i, _)| i)
                .take(50)
                .collect()
        };
        self.selected = 0;
    }

    fn activate(&mut self) {
        let Some(&idx) = self.filtered.get(self.selected) else { return };
        let e = &self.entries[idx];
        if let Ok(mut p) = Command::new("cliphist").arg("decode").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn() {
            if let Some(mut stdin) = p.stdin.take() { let _ = stdin.write_all(e.id.as_bytes()); }
            if let Ok(out) = p.wait_with_output() {
                if out.status.success() {
                    if let Ok(mut wl) = Command::new("wl-copy").stdin(Stdio::piped()).spawn() {
                        if let Some(mut stdin) = wl.stdin.take() { let _ = stdin.write_all(&out.stdout); }
                        let _ = wl.wait();
                    }
                }
            }
        }
        self.should_hide = true;
    }

    fn hide_and_reset(&mut self) {
        // Reset state
        self.query.clear();
        self.selected = 0;
        self.filter();
        self.should_hide = false;
        // Toggle special workspace to hide
        let _ = Command::new("hyprctl")
            .args(["dispatch", "togglespecialworkspace", "clipboard"])
            .spawn();
    }

    fn delete(&mut self, ctx: &Context) {
        let Some(&idx) = self.filtered.get(self.selected) else { return };
        let e = &self.entries[idx];
        if let Ok(mut p) = Command::new("cliphist").arg("delete").stdin(Stdio::piped()).spawn() {
            if let Some(mut stdin) = p.stdin.take() { let _ = stdin.write_all(e.id.as_bytes()); }
            let _ = p.wait();
        }
        self.load_entries(ctx);
        self.selected = self.selected.min(self.filtered.len().saturating_sub(1));
    }

    fn render(&mut self, ctx: &Context) {
        let max_sel = self.filtered.len().saturating_sub(1);
        let (mut down, mut up, mut activate, mut delete) = (false, false, false, false);
        let now = std::time::Instant::now();
        const REPEAT_DELAY_MS: u128 = 300;
        const REPEAT_INTERVAL_MS: u128 = 120;

        ctx.input(|i: &egui::InputState| {
            for event in &i.events {
                if let egui::Event::Key { key, pressed: false, .. } = event {
                    match key {
                        egui::Key::ArrowDown | egui::Key::ArrowUp |
                        egui::Key::J | egui::Key::K => {
                            self.held_key = None;
                        }
                        _ => {}
                    }
                }
            }

            for event in &i.events {
                if let egui::Event::Key { key, pressed: true, modifiers, .. } = event {
                    match key {
                        egui::Key::Escape => self.should_hide = true,
                        egui::Key::Enter => activate = true,
                        egui::Key::D if modifiers.ctrl => delete = true,
                        egui::Key::ArrowDown => { down = true; self.held_key = Some((egui::Key::ArrowDown, now)); }
                        egui::Key::ArrowUp => { up = true; self.held_key = Some((egui::Key::ArrowUp, now)); }
                        egui::Key::J if modifiers.ctrl => { down = true; self.held_key = Some((egui::Key::ArrowDown, now)); }
                        egui::Key::K if modifiers.ctrl => { up = true; self.held_key = Some((egui::Key::ArrowUp, now)); }
                        _ => {}
                    }
                }
            }
        });

        // Manual key repeat
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
            ctx.request_repaint();
        }

        if down { self.selected = (self.selected + 1).min(max_sel); }
        if up { self.selected = self.selected.saturating_sub(1); }
        if activate { self.activate(); return; }
        if delete { self.delete(ctx); }

        CentralPanel::default()
            .frame(Frame::NONE)
            .show(ctx, |ui: &mut Ui| {
                let screen = ui.available_rect_before_wrap();
                let content_width = screen.width();
                let font_id = FontId::new(INPUT_SIZE, FontFamily::Proportional);

                ui.add_space(4.0);

                // Input row
                ui.horizontal(|ui: &mut Ui| {
                    ui.add_space(INPUT_PADDING);

                    ui.label(RichText::new(">")
                        .color(colors::TEXT_MUTED)
                        .size(INPUT_SIZE));
                    ui.add_space(8.0);

                    let old_query = self.query.clone();
                    let input = egui::TextEdit::singleline(&mut self.query)
                        .font(font_id)
                        .text_color(colors::TEXT_PRIMARY)
                        .hint_text(RichText::new("Search clipboard...").color(colors::TEXT_MUTED))
                        .frame(false)
                        .desired_width(content_width - INPUT_PADDING * 3.0 - 30.0);
                    let r = ui.add(input);
                    r.request_focus();
                    if self.query != old_query { self.filter(); }
                });

                ui.add_space(8.0);

                // List area (top half)
                let list_height = MAX_VISIBLE_ITEMS as f32 * ROW_HEIGHT;
                let scroll_to_selected = down || up;

                ScrollArea::vertical()
                    .max_height(list_height)
                    .show(ui, |ui: &mut Ui| {
                        let mut clicked = None;

                        for (i, &idx) in self.filtered.iter().enumerate() {
                            let e = &self.entries[idx];
                            let sel = i == self.selected;

                            let row_rect = egui::Rect::from_min_size(
                                ui.cursor().min,
                                egui::vec2(content_width, ROW_HEIGHT),
                            );

                            if sel {
                                ui.painter().rect_filled(row_rect, 0.0, colors::BG_SELECTED);
                                if scroll_to_selected {
                                    ui.scroll_to_rect(row_rect, Some(egui::Align::Center));
                                }
                            }

                            let text_color = if sel { colors::TEXT_PRIMARY } else { colors::TEXT_SECONDARY };

                            let (_, row_response) = ui.allocate_exact_size(
                                egui::vec2(content_width, ROW_HEIGHT),
                                Sense::click(),
                            );

                            // Row content
                            let text_x = INPUT_PADDING;
                            let text_y = row_rect.min.y + (ROW_HEIGHT - TEXT_SIZE) / 2.0;

                            let display_text = if e.is_image {
                                let dims = e.dims.map(|(w, h)| format!("[image {}x{}]", w, h))
                                    .unwrap_or_else(|| "[image]".into());
                                dims
                            } else {
                                truncate(&e.text, 80)
                            };

                            ui.painter().text(
                                egui::pos2(text_x, text_y),
                                egui::Align2::LEFT_TOP,
                                &display_text,
                                FontId::new(TEXT_SIZE, FontFamily::Proportional),
                                text_color,
                            );

                            if row_response.clicked() {
                                clicked = Some(i);
                            }
                        }

                        if let Some(i) = clicked {
                            self.selected = i;
                            self.activate();
                        }
                    });

                ui.add_space(8.0);

                // Preview area (bottom half)
                let preview_height = screen.height() - list_height - 80.0;
                if preview_height > 50.0 {
                    if let Some(&idx) = self.filtered.get(self.selected) {
                        let e = &self.entries[idx];

                        if e.is_image {
                            if let Some(tex) = &e.texture {
                                let max_w = content_width - INPUT_PADDING * 2.0;
                                let max_h = preview_height - 16.0;
                                ui.horizontal(|ui| {
                                    ui.add_space(INPUT_PADDING);
                                    ui.add(egui::Image::new(tex)
                                        .max_size(egui::vec2(max_w, max_h))
                                        .corner_radius(6.0));
                                });
                            }
                        } else {
                            ScrollArea::vertical()
                                .max_height(preview_height)
                                .show(ui, |ui: &mut Ui| {
                                    ui.horizontal(|ui| {
                                        ui.add_space(INPUT_PADDING);
                                        ui.label(RichText::new(&e.text)
                                            .color(colors::TEXT_SECONDARY)
                                            .size(13.0));
                                    });
                                });
                        }
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
    let (width, height) = get_clip_size();

    eframe::run_native(
        "clip-layer",
        eframe::NativeOptions {
            viewport: egui::ViewportBuilder::default()
                .with_inner_size([width, height])
                .with_decorations(false)
                .with_transparent(true)
                .with_app_id("clip-layer"),
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

fn get_clip_size() -> (f32, f32) {
    let row_height = ROW_HEIGHT;
    let input_height = INPUT_SIZE + INPUT_PADDING * 2.0;
    let list_height = MAX_VISIBLE_ITEMS as f32 * row_height;
    let preview_height = 200.0;  // Fixed preview height
    let height = input_height + list_height + preview_height + 32.0;

    let (width, scale) = Command::new("hyprctl").args(["monitors", "-j"]).output().ok()
        .and_then(|o| serde_json::from_slice::<Vec<serde_json::Value>>(&o.stdout).ok())
        .and_then(|m| m.first().and_then(|m| {
            let w = m["width"].as_f64()?;
            let s = m["scale"].as_f64().unwrap_or(1.0);
            let logical_w = w / s;
            Some(((logical_w * 0.382 / s) as f32, s))
        }))
        .unwrap_or((300.0, 1.0));

    (width, height / scale as f32)
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ").replace('\t', " ");
    if s.chars().count() > max {
        s.chars().take(max - 1).collect::<String>() + "…"
    } else {
        s
    }
}

fn temp_dir() -> PathBuf {
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or("/tmp".into());
    PathBuf::from(runtime).join("clip-layer-cache")
}

fn collect(ctx: &Context, re: &Regex, textures: &mut HashMap<String, egui::TextureHandle>) -> Vec<Entry> {
    let Ok(out) = Command::new("cliphist").arg("list").output() else { return vec![] };
    if !out.status.success() { return vec![]; }

    let _ = std::fs::create_dir_all(temp_dir());
    let mut seen = std::collections::HashSet::new();
    let mut entries: Vec<Entry> = Vec::new();

    for (i, line) in String::from_utf8_lossy(&out.stdout).lines().enumerate() {
        let line = line.trim();
        if line.is_empty() { continue; }

        if let Some(caps) = re.captures(line) {
            if !seen.insert(line.to_string()) { continue; }
            let fmt = caps.get(1).map(|m| m.as_str().to_lowercase());
            let w = caps.get(2).and_then(|m| m.as_str().parse().ok());
            let h = caps.get(3).and_then(|m| m.as_str().parse().ok());

            let texture = if i < 20 {
                textures.get(line).cloned().or_else(|| {
                    decode_image(ctx, line, &fmt.clone().unwrap_or("png".into()), i).map(|t: egui::TextureHandle| {
                        textures.insert(line.to_string(), t.clone());
                        t
                    })
                })
            } else { None };

            entries.push(Entry { id: line.into(), text: "[image]".into(), is_image: true, dims: w.zip(h), texture });
        } else {
            let text = line.split_once('\t').map(|(_, t)| t).unwrap_or(line).to_string();
            if !seen.insert(text.clone()) { continue; }
            entries.push(Entry { id: line.into(), text, is_image: false, dims: None, texture: None });
        }
    }
    entries
}

fn decode_image(ctx: &Context, id: &str, ext: &str, idx: usize) -> Option<egui::TextureHandle> {
    let path = temp_dir().join(format!("img_{}.{}", idx, ext));

    if !path.exists() {
        let mut p = Command::new("cliphist").arg("decode").stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().ok()?;
        if let Some(mut stdin) = p.stdin.take() { let _ = stdin.write_all(id.as_bytes()); }
        let out = p.wait_with_output().ok()?;
        if !out.status.success() || out.stdout.is_empty() { return None; }
        std::fs::write(&path, &out.stdout).ok()?;
    }

    let data = std::fs::read(&path).ok()?;
    let img = image::load_from_memory(&data).ok()?.to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    Some(ctx.load_texture(
        format!("clip_{}", idx),
        egui::ColorImage::from_rgba_unmultiplied(size, &img.into_raw()),
        egui::TextureOptions::LINEAR,
    ))
}
