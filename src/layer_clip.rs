//! Clipboard manager using eframe (regular window in special workspace)

mod common;

use common::{colors, handle_navigation_keys, truncate};
use common::{INPUT_PADDING, INPUT_SIZE, MAX_VISIBLE_ITEMS, ROW_HEIGHT, TEXT_SIZE};
use eframe::egui::{self, CentralPanel, Context, Frame, Color32, RichText, ScrollArea, FontFamily, FontId, Ui};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use regex::Regex;
use std::collections::HashMap;
use std::io::Write as IoWrite;
use std::path::PathBuf;
use std::process::{Command, Stdio};

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
    needs_reload: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _watcher: Option<RecommendedWatcher>,
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
            needs_reload: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            _watcher: None,
        }
    }

    fn setup_watcher(&mut self, ctx: &Context) {
        use std::sync::atomic::Ordering;

        // Watch the directory, not the file (cliphist does atomic writes via rename)
        let db_dir = std::env::var("HOME")
            .map(|h| PathBuf::from(h).join(".cache/cliphist"))
            .unwrap_or_else(|_| PathBuf::from("/tmp/cliphist"));

        let needs_reload = self.needs_reload.clone();
        let ctx = ctx.clone();

        if let Ok(mut watcher) = RecommendedWatcher::new(
            move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    // Only reload on modifications to 'db' file
                    let dominated_db = event.paths.iter().any(|p| {
                        p.file_name().is_some_and(|n| n == "db")
                    });
                    if dominated_db {
                        needs_reload.store(true, Ordering::SeqCst);
                        ctx.request_repaint();
                    }
                }
            },
            Config::default(),
        ) {
            if db_dir.exists() {
                let _ = watcher.watch(&db_dir, RecursiveMode::NonRecursive);
            }
            self._watcher = Some(watcher);
        }
    }

    fn ensure_texture(&mut self, ctx: &Context, idx: usize) {
        let e = &self.entries[idx];
        if e.is_image && e.texture.is_none() {
            if let Some(caps) = self.re.captures(&e.id) {
                let fmt = caps.get(1).map(|m| m.as_str().to_lowercase()).unwrap_or("png".into());
                if let Some(tex) = decode_image(ctx, &e.id, &fmt, idx) {
                    self.textures.insert(e.id.clone(), tex.clone());
                    self.entries[idx].texture = Some(tex);
                }
            }
        }
    }

    fn load_entries(&mut self, ctx: &Context) {
        let old_selected = self.selected;
        self.entries = collect(ctx, &self.re, &mut self.textures);
        self.filter(); // Reapply current query filter
        self.selected = old_selected.min(self.filtered.len().saturating_sub(1));
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
        let (mut activate, mut delete) = (false, false);

        // Handle navigation with shared helper
        let (down, up) = handle_navigation_keys(ctx, &mut self.held_key);

        // Handle clipboard-specific keys
        ctx.input(|i| {
            for event in &i.events {
                if let egui::Event::Key { key, pressed: true, modifiers, .. } = event {
                    match key {
                        egui::Key::Escape => self.should_hide = true,
                        egui::Key::Enter => activate = true,
                        egui::Key::D if modifiers.ctrl => delete = true,
                        _ => {}
                    }
                }
            }
        });

        if down { self.selected = (self.selected + 1).min(max_sel); }
        if up { self.selected = self.selected.saturating_sub(1); }
        if activate { self.activate(); return; }
        if delete { self.delete(ctx); }

        // Ensure texture is loaded for selected image (lazy loading)
        if let Some(&idx) = self.filtered.get(self.selected) {
            self.ensure_texture(ctx, idx);
        }

        CentralPanel::default()
            .frame(Frame::NONE)
            .show(ctx, |ui: &mut Ui| {
                let screen = ui.available_rect_before_wrap();
                let total_width = screen.width();
                let font_id = FontId::new(INPUT_SIZE, FontFamily::Proportional);

                ui.add_space(4.0);

                // Input row (full width)
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
                        .desired_width(total_width * 0.5 - INPUT_PADDING * 2.0);
                    let r = ui.add(input);
                    r.request_focus();
                    if self.query != old_query { self.filter(); }
                });

                ui.add_space(8.0);

                // Main content: list on left, preview on right
                let list_height = screen.height() - INPUT_SIZE - INPUT_PADDING * 2.0 - 20.0;
                let scroll_to_selected = down || up;

                // Use columns for side-by-side layout
                ui.columns(2, |cols| {
                    // Left column: scrollable list
                    let list_ui = &mut cols[0];
                    ScrollArea::vertical()
                        .id_salt("clip_list")
                        .max_height(list_height)
                        .show(list_ui, |ui: &mut Ui| {
                            let mut clicked = None;
                            let col_width = ui.available_width();

                            for (i, &idx) in self.filtered.iter().enumerate() {
                                let e = &self.entries[idx];
                                let is_selected = i == self.selected;

                                let (rect, response) = ui.allocate_exact_size(
                                    egui::vec2(col_width, ROW_HEIGHT),
                                    egui::Sense::click(),
                                );

                                if is_selected {
                                    ui.painter().rect_filled(rect, 0.0, colors::BG_SELECTED);
                                    if scroll_to_selected {
                                        ui.scroll_to_rect(rect, Some(egui::Align::Center));
                                    }
                                }

                                let text_color = if is_selected {
                                    colors::TEXT_PRIMARY
                                } else {
                                    colors::TEXT_SECONDARY
                                };

                                let display_text = if e.is_image {
                                    e.dims.map(|(w, h)| format!("[img {}x{}]", w, h))
                                        .unwrap_or_else(|| "[image]".into())
                                } else {
                                    truncate(&e.text, 45)
                                };

                                let text_pos = egui::pos2(
                                    rect.min.x + INPUT_PADDING,
                                    rect.min.y + (ROW_HEIGHT - TEXT_SIZE) / 2.0,
                                );
                                ui.painter().text(
                                    text_pos,
                                    egui::Align2::LEFT_TOP,
                                    &display_text,
                                    FontId::new(TEXT_SIZE, FontFamily::Proportional),
                                    text_color,
                                );

                                if response.clicked() {
                                    clicked = Some(i);
                                }
                            }

                            if let Some(i) = clicked {
                                self.selected = i;
                                self.activate();
                            }
                        });

                    // Right column: preview
                    let preview_ui = &mut cols[1];
                    if let Some(&idx) = self.filtered.get(self.selected) {
                        let e = &self.entries[idx];

                        if e.is_image {
                            if let Some(tex) = &e.texture {
                                let max_w = preview_ui.available_width() - 16.0;
                                let max_h = list_height - 16.0;
                                preview_ui.add(egui::Image::new(tex)
                                    .max_size(egui::vec2(max_w, max_h))
                                    .corner_radius(6.0));
                            }
                        } else {
                            ScrollArea::vertical()
                                .id_salt("clip_preview")
                                .max_height(list_height)
                                .show(preview_ui, |ui: &mut Ui| {
                                    ui.add(egui::Label::new(
                                        RichText::new(&e.text)
                                            .color(colors::TEXT_SECONDARY)
                                            .size(13.0)
                                    ).wrap());
                                });
                        }
                    }
                });
            });
    }
}

impl eframe::App for App {
    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        [0.0, 0.0, 0.0, 0.0]
    }

    fn update(&mut self, ctx: &Context, _: &mut eframe::Frame) {
        use std::sync::atomic::Ordering;

        // Set up watcher on first update (need Context)
        if self._watcher.is_none() {
            self.setup_watcher(ctx);
        }

        // Check for file change events
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
    let input_height = INPUT_SIZE + INPUT_PADDING * 2.0;
    let list_height = MAX_VISIBLE_ITEMS as f32 * ROW_HEIGHT;
    let height = input_height + list_height + 24.0;

    // Wider window for side-by-side layout (golden ratio inverse ~0.618)
    let width = Command::new("hyprctl").args(["monitors", "-j"]).output().ok()
        .and_then(|o| serde_json::from_slice::<Vec<serde_json::Value>>(&o.stdout).ok())
        .and_then(|m| m.first().and_then(|m| {
            let w = m["width"].as_f64()?;
            let s = m["scale"].as_f64().unwrap_or(1.0);
            // Golden ratio: 61.8% of logical screen width (eframe applies scaling)
            Some((w / s * 0.618) as f32)
        }))
        .unwrap_or(500.0);

    (width, height)
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
