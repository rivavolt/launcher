//! Clipboard manager using eframe (regular window in special workspace)

use launcher::common::{self, colors, handle_navigation_keys, truncate, virtual_list};
use launcher::scroll::ScrollMomentum;
use launcher::common::{INPUT_SIZE, ROW_HEIGHT, TEXT_SIZE};
use launcher::hyprland;
use eframe::egui::{self, CentralPanel, Context, RichText, ScrollArea, FontFamily, FontId, Ui};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::io::Write as IoWrite;
use std::path::PathBuf;
use std::process::{Command, Stdio};

struct Entry {
    id: String,
    text: String,
    full_text: Option<String>,
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
    failed_textures: HashSet<usize>,
    should_hide: bool,
    loaded: bool,
    held_key: Option<(egui::Key, std::time::Instant)>,
    needs_reload: std::sync::Arc<std::sync::atomic::AtomicBool>,
    _watcher: Option<RecommendedWatcher>,
    scroll_momentum: ScrollMomentum,
    last_ensured: Option<usize>,
    max_size: (f32, f32),
    last_height: f32,
    deleting: Option<(usize, std::time::Instant)>,
}

impl App {
    fn new() -> Self {
        let re = Regex::new(r"\[\[\s*binary data\s+[\d.]+\s*\w+\s+(\w+)\s+(\d+)x(\d+)\s*\]\]").unwrap();
        let eframe_size = hyprland::window_size(0.382, 0.618, (300.0, 400.0));
        let max_size = (eframe_size.0 * 2.0, eframe_size.1 * 2.0);

        Self {
            query: String::new(),
            entries: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            re,
            textures: HashMap::new(),
            failed_textures: HashSet::new(),
            should_hide: false,
            loaded: false,
            held_key: None,
            needs_reload: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            _watcher: None,
            scroll_momentum: ScrollMomentum::new(),
            last_ensured: None,
            max_size,
            last_height: 0.0,
            deleting: None,
        }
    }

    fn setup_watcher(&mut self, ctx: &Context) {
        use std::sync::atomic::Ordering;

        let db_dir = std::env::var("HOME")
            .map(|h| PathBuf::from(h).join(".cache/cliphist"))
            .unwrap_or_else(|_| PathBuf::from("/tmp/cliphist"));

        let needs_reload = self.needs_reload.clone();
        let ctx = ctx.clone();

        if let Ok(mut watcher) = RecommendedWatcher::new(
            move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    use notify::EventKind;
                    let is_write = matches!(
                        event.kind,
                        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
                    );
                    let is_db = is_write && event.paths.iter().any(|p| {
                        p.file_name().is_some_and(|n| n == "db")
                    });
                    if is_db {
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

    fn ensure_full_text(&mut self, idx: usize) {
        let e = &self.entries[idx];
        if !e.is_image && e.full_text.is_none() {
            if let Ok(mut p) = Command::new("cliphist").arg("decode")
                .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn()
            {
                if let Some(mut stdin) = p.stdin.take() {
                    let _ = stdin.write_all(e.id.as_bytes());
                }
                if let Ok(out) = p.wait_with_output() {
                    if out.status.success() {
                        self.entries[idx].full_text =
                            Some(String::from_utf8_lossy(&out.stdout).into_owned());
                    }
                }
            }
        }
    }

    fn ensure_texture(&mut self, ctx: &Context, idx: usize) {
        let e = &self.entries[idx];
        if e.is_image && e.texture.is_none() && !self.failed_textures.contains(&idx) {
            if let Some(caps) = self.re.captures(&e.id) {
                let fmt = caps.get(1).map(|m| m.as_str().to_lowercase()).unwrap_or("png".into());
                if let Some(tex) = decode_image(ctx, &e.id, &fmt) {
                    self.textures.insert(e.id.clone(), tex.clone());
                    self.entries[idx].texture = Some(tex);
                } else {
                    self.failed_textures.insert(idx);
                }
            } else {
                self.failed_textures.insert(idx);
            }
        }
    }

    fn load_entries(&mut self, ctx: &Context) {
        let old_selected = self.selected;
        self.failed_textures.clear();
        self.last_ensured = None;
        self.entries = collect(ctx, &self.re, &mut self.textures);
        self.filter();
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
        self.query.clear();
        self.selected = 0;
        self.filter();
        self.should_hide = false;
        hyprland::dispatch_async("togglespecialworkspace", "clipboard");
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

        let (down, up) = handle_navigation_keys(ctx, &mut self.held_key);

        ctx.input(|i| {
            for event in &i.events {
                if let egui::Event::Key { key, pressed: true, modifiers, .. } = event {
                    match key {
                        egui::Key::Escape => self.should_hide = true,
                        egui::Key::Enter => activate = true,
                        egui::Key::C if modifiers.ctrl => activate = true,
                        egui::Key::D if modifiers.ctrl => delete = true,
                        egui::Key::Delete => delete = true,
                        _ => {}
                    }
                }
            }
        });

        // Complete delete animation
        if let Some((_, t)) = self.deleting {
            let elapsed = t.elapsed().as_secs_f32();
            if elapsed >= 0.15 {
                let sel = self.selected;
                self.deleting = None;
                self.delete(ctx);
                self.selected = sel.min(self.filtered.len().saturating_sub(1));
            } else {
                ctx.request_repaint();
            }
        }

        if down { self.selected = (self.selected + 1).min(max_sel); }
        if up { self.selected = self.selected.saturating_sub(1); }
        if activate { self.activate(); return; }
        if delete && self.deleting.is_none() {
            self.deleting = Some((self.selected, std::time::Instant::now()));
            ctx.request_repaint();
        }

        // Ensure texture + full text for selected entry (skip if unchanged)
        if let Some(&idx) = self.filtered.get(self.selected) {
            if self.last_ensured != Some(idx) {
                self.ensure_texture(ctx, idx);
                self.ensure_full_text(idx);
                self.last_ensured = Some(idx);
            }
        }

        let input_response = egui::TopBottomPanel::top("input")
            .frame(common::input_frame())
            .show(ctx, |ui: &mut Ui| {
                let font_id = FontId::new(INPUT_SIZE, FontFamily::Proportional);
                let old_query = self.query.clone();
                let input = egui::TextEdit::singleline(&mut self.query)
                    .font(font_id)
                    .text_color(colors::TEXT_PRIMARY)
                    .hint_text(RichText::new("Search clipboard...").color(colors::TEXT_MUTED))
                    .frame(false)
                    .desired_width(ui.available_width());
                let r = ui.add(input);
                if ui.ctx().input(|i| i.focused) {
                    r.request_focus();
                } else {
                    r.surrender_focus();
                }
                if self.query != old_query { self.filter(); }
            });

        CentralPanel::default()
            .frame(common::panel_frame())
            .show(ctx, |ui: &mut Ui| {
                let header_height = input_response.response.rect.height();
                let max_visible = 12;
                let num_items = self.filtered.len().min(max_visible);
                let spacing_y = ui.spacing().item_spacing.y;
                let items_height = if num_items > 0 {
                    num_items as f32 * ROW_HEIGHT + (num_items - 1) as f32 * spacing_y
                } else {
                    0.0
                };
                let desired_height = header_height + items_height;
                let target_height = desired_height.min(self.max_size.1);
                if (target_height - self.last_height).abs() > 1.0 {
                    self.last_height = target_height;
                    let w = self.max_size.0 as i32;
                    let h = target_height as i32;
                    hyprland::dispatch_async(
                        "resizewindowpixel",
                        &format!("exact {} {},class:clipboard", w, h),
                    );
                }

                let list_height = (self.max_size.1 - header_height).max(ROW_HEIGHT);
                let scroll_to_selected = down || up;

                // Pre-compute whether selected entry needs expansion overlay
                let selected_needs_expand = self.filtered.get(self.selected)
                    .map(|&idx| &self.entries[idx])
                    .map(|e| {
                        if e.is_image { true }
                        else {
                            let full = e.full_text.as_deref().unwrap_or(&e.text);
                            full.contains('\n') || full.chars().count() > 80
                        }
                    })
                    .unwrap_or(false);

                let filtered = &self.filtered;
                let entries = &self.entries;

                let deleting = self.deleting;
                let vl_output = ScrollArea::vertical()
                    .id_salt("clip_list")
                    .max_height(list_height)
                    .show(ui, |ui: &mut Ui| {
                        let col_width = ui.available_width();

                        let vl = virtual_list(
                            ui,
                            filtered.len(),
                            ROW_HEIGHT,
                            self.selected,
                            scroll_to_selected,
                            selected_needs_expand,
                            |ui, i, rect| {
                                let idx = filtered[i];
                                let e = &entries[idx];
                                let is_selected = i == self.selected;

                                // Fade + slide animation for deleting row
                                let (alpha, x_offset) = if let Some((del_i, t)) = deleting {
                                    if i == del_i {
                                        let progress = (t.elapsed().as_secs_f32() / 0.15).min(1.0);
                                        let alpha = ((1.0 - progress) * 255.0) as u8;
                                        let x_offset = -progress * 40.0;
                                        (alpha, x_offset)
                                    } else { (255, 0.0) }
                                } else { (255, 0.0) };

                                let text_color = if is_selected {
                                    colors::TEXT_PRIMARY
                                } else {
                                    colors::TEXT_SECONDARY
                                };
                                let text_color = egui::Color32::from_rgba_unmultiplied(
                                    text_color.r(), text_color.g(), text_color.b(), alpha,
                                );

                                let rx = rect.min.x + x_offset;

                                if e.is_image {
                                    if let Some(tex) = &e.texture {
                                        let tex_size = tex.size_vec2();
                                        let thumb_h = ROW_HEIGHT - 4.0;
                                        let thumb_w = thumb_h * (tex_size.x / tex_size.y);
                                        let thumb_rect = egui::Rect::from_min_size(
                                            egui::pos2(rx + 8.0, rect.min.y + 2.0),
                                            egui::vec2(thumb_w, thumb_h),
                                        );
                                        let tint = egui::Color32::from_rgba_unmultiplied(255, 255, 255, alpha);
                                        ui.painter().image(
                                            tex.id(),
                                            thumb_rect,
                                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                                            tint,
                                        );

                                        let display_text = e.dims.map(|(w, h)| format!("{}x{}", w, h))
                                            .unwrap_or_else(|| "image".into());
                                        let text_pos = egui::pos2(
                                            rx + 8.0 + thumb_w + 8.0,
                                            rect.min.y + (ROW_HEIGHT - TEXT_SIZE) / 2.0,
                                        );
                                        ui.painter().text(
                                            text_pos,
                                            egui::Align2::LEFT_TOP,
                                            &display_text,
                                            FontId::new(TEXT_SIZE, FontFamily::Proportional),
                                            text_color,
                                        );
                                    } else {
                                        let display_text = e.dims.map(|(w, h)| format!("[img {}x{}]", w, h))
                                            .unwrap_or_else(|| "[image]".into());
                                        let text_pos = egui::pos2(
                                            rx + 12.0,
                                            rect.min.y + (ROW_HEIGHT - TEXT_SIZE) / 2.0,
                                        );
                                        ui.painter().text(
                                            text_pos,
                                            egui::Align2::LEFT_TOP,
                                            &display_text,
                                            FontId::new(TEXT_SIZE, FontFamily::Proportional),
                                            text_color,
                                        );
                                    }
                                } else {
                                    let display_text = truncate(&e.text, 80);
                                    let text_pos = egui::pos2(
                                        rx + 12.0,
                                        rect.min.y + (ROW_HEIGHT - TEXT_SIZE) / 2.0,
                                    );
                                    ui.painter().text(
                                        text_pos,
                                        egui::Align2::LEFT_TOP,
                                        &display_text,
                                        FontId::new(TEXT_SIZE, FontFamily::Proportional),
                                        text_color,
                                    );
                                }
                            },
                        );

                        // Paint expansion overlay only when content doesn't fit one row
                        if selected_needs_expand {
                            if let (Some(sel_rect), Some(&sel_idx)) = (vl.selected_rect, filtered.get(self.selected)) {
                                let e = &entries[sel_idx];
                                let max_content_h = ROW_HEIGHT * 8.0;
                                let text_x = sel_rect.min.x + 12.0;
                                let text_y = sel_rect.min.y + (ROW_HEIGHT - TEXT_SIZE) / 2.0;
                                let text_w = col_width - 24.0;
                                let pad_bottom = 10.0;

                                let (natural_h, text_galley) = if e.is_image {
                                    let h = if let Some(tex) = &e.texture {
                                        let ts = tex.size_vec2();
                                        let scale = (text_w / ts.x).min(max_content_h / ts.y).min(1.0);
                                        ts.y * scale
                                    } else { 0.0 };
                                    (h, None)
                                } else {
                                    let display = e.full_text.as_deref().unwrap_or(&e.text);
                                    let galley = ui.painter().layout(
                                        display.to_owned(),
                                        FontId::new(TEXT_SIZE, FontFamily::Proportional),
                                        colors::TEXT_PRIMARY,
                                        text_w,
                                    );
                                    let h = galley.size().y;
                                    (h, Some(galley))
                                };

                                let content_h = natural_h.min(max_content_h);
                                let overlay_h = (text_y - sel_rect.min.y) + content_h + pad_bottom;

                                let overlay_rect = egui::Rect::from_min_size(
                                    egui::pos2(sel_rect.min.x, sel_rect.min.y),
                                    egui::vec2(col_width, overlay_h),
                                );

                                // Soft shadow below overlay
                                for i in 0..6u8 {
                                    let alpha = 30u8.saturating_sub(i * 5);
                                    let y = overlay_rect.max.y + i as f32;
                                    ui.painter().hline(
                                        overlay_rect.min.x..=overlay_rect.max.x,
                                        y,
                                        egui::Stroke::new(1.0, egui::Color32::from_black_alpha(alpha)),
                                    );
                                }

                                ui.painter().rect_filled(overlay_rect, 0.0, egui::Color32::from_rgb(12, 12, 12));
                                ui.painter().rect_filled(overlay_rect, 0.0, colors::BG_SELECTED);

                                let clip_rect = egui::Rect::from_min_max(
                                    egui::pos2(text_x, text_y),
                                    egui::pos2(text_x + text_w, overlay_rect.max.y - pad_bottom),
                                );
                                let painter = ui.painter().with_clip_rect(clip_rect);

                                if e.is_image {
                                    if let Some(tex) = &e.texture {
                                        let ts = tex.size_vec2();
                                        let scale = (text_w / ts.x).min(content_h / ts.y).min(1.0);
                                        let img_rect = egui::Rect::from_min_size(
                                            egui::pos2(text_x, text_y),
                                            egui::vec2(ts.x * scale, ts.y * scale),
                                        );
                                        painter.image(
                                            tex.id(),
                                            img_rect,
                                            egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                                            egui::Color32::WHITE,
                                        );
                                    }
                                } else if let Some(galley) = text_galley {
                                    painter.galley(egui::pos2(text_x, text_y), galley, colors::TEXT_PRIMARY);
                                }
                            }
                        }

                        vl
                    });

                if let Some(i) = vl_output.inner.clicked {
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
        use std::sync::atomic::Ordering;

        if ctx.input(|i| i.viewport().close_requested()) {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.should_hide = true;
        }

        if self._watcher.is_none() {
            self.setup_watcher(ctx);
        }

        if !self.loaded {
            self.load_entries(ctx);
            self.needs_reload.store(false, Ordering::SeqCst);
        } else if self.needs_reload.swap(false, Ordering::SeqCst) {
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
        "clipboard",
        common::window_options("clipboard", width, height),
        Box::new(|cc| {
            common::setup_transparent_style(cc);
            Ok(Box::new(App::new()))
        }),
    )
}

fn temp_dir() -> PathBuf {
    let runtime = std::env::var("XDG_RUNTIME_DIR").unwrap_or("/tmp".into());
    PathBuf::from(runtime).join("clipboard-cache")
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
                    decode_image(ctx, line, &fmt.clone().unwrap_or("png".into())).map(|t: egui::TextureHandle| {
                        textures.insert(line.to_string(), t.clone());
                        t
                    })
                })
            } else { None };

            entries.push(Entry { id: line.into(), text: "[image]".into(), full_text: None, is_image: true, dims: w.zip(h), texture });
        } else {
            let text = line.split_once('\t').map(|(_, t)| t).unwrap_or(line).to_string();
            if !seen.insert(text.clone()) { continue; }
            entries.push(Entry { id: line.into(), text, full_text: None, is_image: false, dims: None, texture: None });
        }
    }
    entries
}

fn id_hash(id: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    id.hash(&mut h);
    h.finish()
}

fn decode_image(ctx: &Context, id: &str, ext: &str) -> Option<egui::TextureHandle> {
    let hash = id_hash(id);
    let path = temp_dir().join(format!("img_{:x}.{}", hash, ext));

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
        format!("clip_{:x}", hash),
        egui::ColorImage::from_rgba_unmultiplied(size, &img.into_raw()),
        egui::TextureOptions::LINEAR,
    ))
}
