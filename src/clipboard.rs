//! Clipboard manager — reads from clipd's SQLite database

use launcher::clip::db::{self, ClipboardDb};
use launcher::clip::mime as clip_mime;
use launcher::common::{self, colors, handle_navigation_keys, truncate, virtual_list};
use launcher::scroll::ScrollMomentum;
use launcher::hyprland;
use eframe::egui::{self, CentralPanel, Context, ScrollArea, FontFamily, FontId, Ui};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::io::Write as IoWrite;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

struct Entry {
    id: i64,
    content: Vec<u8>,
    text: String,
    mime: String,
    source_app: Option<String>,
    last_used: i64,
    pinned: bool,
    is_image: bool,
    dims: Option<(u32, u32)>,
    texture: Option<egui::TextureHandle>,
    thumb: Option<egui::TextureHandle>,
}

/// Format a unix timestamp as compact relative time
fn relative_time(unix_secs: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let diff = now - unix_secs;
    if diff < 60 { "now".into() }
    else if diff < 3600 { format!("{}m", diff / 60) }
    else if diff < 86400 { format!("{}h", diff / 3600) }
    else if diff < 7 * 86400 { format!("{}d", diff / 86400) }
    else { format!("{}w", diff / (7 * 86400)) }
}

struct App {
    query: String,
    entries: Vec<Entry>,
    filtered: Vec<usize>,
    selected: usize,
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
        let eframe_size = hyprland::window_size(0.618, 0.618, (500.0, 400.0));
        let max_size = (eframe_size.0 * 2.0, eframe_size.1 * 2.0);

        Self {
            query: String::new(),
            entries: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
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

        let db_dir = db::default_db_path()
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| "/tmp".into());

        let needs_reload = self.needs_reload.clone();
        let ctx = ctx.clone();

        if let Ok(mut watcher) = RecommendedWatcher::new(
            move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    use notify::EventKind;
                    if matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)) {
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
        if e.is_image && e.texture.is_none() && !self.failed_textures.contains(&idx) {
            if let Ok(img) = image::load_from_memory(&e.content) {
                let full = img.to_rgba8();
                let full_size = [full.width() as usize, full.height() as usize];
                let tex = ctx.load_texture(
                    format!("clip_{}", e.id),
                    egui::ColorImage::from_rgba_unmultiplied(full_size, &full.into_raw()),
                    egui::TextureOptions::LINEAR,
                );

                let thumb_img = img.resize(128, 128, image::imageops::FilterType::CatmullRom).to_rgba8();
                let thumb_size = [thumb_img.width() as usize, thumb_img.height() as usize];
                let thumb = ctx.load_texture(
                    format!("clip_{}_thumb", e.id),
                    egui::ColorImage::from_rgba_unmultiplied(thumb_size, &thumb_img.into_raw()),
                    egui::TextureOptions::LINEAR,
                );

                self.entries[idx].texture = Some(tex);
                self.entries[idx].thumb = Some(thumb);
            } else {
                self.failed_textures.insert(idx);
            }
        }
    }

    fn load_entries(&mut self, _ctx: &Context) {
        let old_selected = self.selected;
        self.failed_textures.clear();
        self.last_ensured = None;

        let db_entries = ClipboardDb::open_default()
            .and_then(|db| db.list(500))
            .unwrap_or_default();

        self.entries = db_entries.into_iter().map(|e| {
            let is_image = clip_mime::is_image_mime(&e.mime);
            let text = if is_image {
                String::new()
            } else {
                String::from_utf8_lossy(&e.content).into_owned()
            };
            let dims = if is_image {
                imagesize::blob_size(&e.content).ok().map(|s| (s.width as u32, s.height as u32))
            } else {
                None
            };
            Entry {
                id: e.id,
                content: e.content,
                mime: e.mime,
                source_app: e.source_app,
                last_used: e.last_used,
                pinned: e.pinned,
                text,
                is_image,
                dims,
                texture: None,
                thumb: None,
            }
        }).collect();

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

        let mut cmd = Command::new("wl-copy");
        if e.is_image {
            cmd.arg("--type").arg(&e.mime);
        }
        if let Ok(mut wl) = cmd.stdin(Stdio::piped()).spawn() {
            if let Some(mut stdin) = wl.stdin.take() {
                let _ = stdin.write_all(&e.content);
            }
            let _ = wl.wait();
        }

        if let Ok(db) = ClipboardDb::open_default() {
            let _ = db.update_last_used(e.id);
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

        if let Ok(db) = ClipboardDb::open_default() {
            let _ = db.delete(e.id);
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

        // Ensure texture for selected entry
        if let Some(&idx) = self.filtered.get(self.selected) {
            if self.last_ensured != Some(idx) {
                self.ensure_texture(ctx, idx);
                self.last_ensured = Some(idx);
            }
        }

        let input_panel = common::input_panel(ctx, &mut self.query, "Search clipboard...", None);
        if input_panel.changed || input_panel.cleared { self.filter(); }
        let input_response = input_panel.response;

        // Check if selected entry has preview content and compute its natural height
        let (has_preview, preview_content_height) = self.filtered.get(self.selected)
            .map(|&idx| {
                let e = &self.entries[idx];
                if e.is_image {
                    let h = if let Some(tex) = &e.texture {
                        let ts = tex.size_vec2();
                        let preview_w = self.max_size.0 * 0.382 - 20.0;
                        let scale = (preview_w / ts.x).min(1.0);
                        ts.y * scale + common::text_size() * 2.0 + 16.0
                    } else { 0.0 };
                    (true, h)
                } else if e.text.contains('\n') || e.text.chars().count() > 80 {
                    let preview_w = self.max_size.0 * 0.382 - 20.0;
                    let font = FontId::new(common::text_size() * 0.85, FontFamily::Proportional);
                    let galley = ctx.fonts(|f| f.layout(
                        e.text.clone(), font, colors::TEXT_PRIMARY, preview_w,
                    ));
                    (true, galley.size().y + common::text_size() + 16.0)
                } else {
                    (false, 0.0)
                }
            })
            .unwrap_or((false, 0.0));

        // Preview pane (right side)
        if has_preview {
            egui::SidePanel::right("preview")
                .frame(common::preview_frame())
                .resizable(false)
                .exact_width(self.max_size.0 * 0.382)
                .show(ctx, |ui| {
                    if let Some(&idx) = self.filtered.get(self.selected) {
                        let e = &self.entries[idx];
                        ScrollArea::vertical()
                            .id_salt("preview_scroll")
                            .show(ui, |ui| {
                                if e.is_image {
                                    if let Some(tex) = &e.texture {
                                        let ts = tex.size_vec2();
                                        let max_w = ui.available_width();
                                        let scale = (max_w / ts.x).min(1.0);
                                        let img_size = egui::vec2(ts.x * scale, ts.y * scale);
                                        ui.image(egui::load::SizedTexture::new(tex.id(), img_size));
                                    }
                                    if let Some((w, h)) = e.dims {
                                        ui.add_space(4.0);
                                        ui.label(egui::RichText::new(format!("{}×{}", w, h))
                                            .font(FontId::new(common::text_size() * 0.85, FontFamily::Monospace))
                                            .color(colors::TEXT_SUBTITLE));
                                    }
                                } else {
                                    let font = FontId::new(common::text_size() * 0.85, FontFamily::Proportional);
                                    let galley = ui.painter().layout(
                                        e.text.clone(),
                                        font,
                                        colors::TEXT_PRIMARY,
                                        ui.available_width(),
                                    );
                                    let (rect, _) = ui.allocate_exact_size(galley.size(), egui::Sense::hover());
                                    ui.painter().galley(rect.min, galley, colors::TEXT_PRIMARY);
                                }

                                // Metadata line: mime · source_app · timestamp
                                ui.add_space(8.0);
                                let meta_font = FontId::new(common::text_size() * 0.75, FontFamily::Monospace);
                                let mut parts = vec![e.mime.as_str()];
                                if let Some(app) = &e.source_app {
                                    parts.push(app.as_str());
                                }
                                let time_str = relative_time(e.last_used);
                                parts.push(&time_str);
                                ui.label(egui::RichText::new(parts.join(" · "))
                                    .font(meta_font)
                                    .color(colors::TEXT_SUBTITLE));
                            });
                    }
                });
        }

        CentralPanel::default()
            .frame(common::panel_frame())
            .show(ctx, |ui: &mut Ui| {
                let header_height = input_response.response.rect.height();
                let max_visible = 12;
                let num_items = self.filtered.len().min(max_visible);
                let spacing_y = ui.spacing().item_spacing.y;
                let items_height = if num_items > 0 {
                    num_items as f32 * common::row_height() + (num_items - 1) as f32 * spacing_y
                } else if !self.query.is_empty() {
                    common::row_height()
                } else {
                    0.0
                };
                let min_height = if has_preview {
                    (header_height + preview_content_height + 20.0).min(self.max_size.1)
                } else {
                    0.0
                };
                let desired_height = (header_height + items_height).max(min_height);
                let target_height = desired_height.min(self.max_size.1);
                if (target_height - self.last_height).abs() > 1.0 {
                    self.last_height = target_height;
                    hyprland::dispatch_async(
                        "resizewindowpixel",
                        &format!("exact {} {},class:clipboard", self.max_size.0 as i32, target_height as i32),
                    );
                }

                let list_height = (self.max_size.1 - header_height).max(common::row_height());
                let scroll_to_selected = down || up;

                if self.filtered.is_empty() && !self.query.is_empty() {
                    common::empty_state(ui);
                } else {
                    let filtered = &self.filtered;
                    let entries = &self.entries;
                    let query = &self.query;

                    let vl_output = ScrollArea::vertical()
                        .id_salt("clip_list")
                        .max_height(list_height)
                        .show(ui, |ui: &mut Ui| {
                            let deleting = self.deleting;
                            let vl = virtual_list(
                                ui,
                                filtered.len(),
                                common::row_height(),
                                self.selected,
                                scroll_to_selected,
                                false,
                                |ui, i, rect| {
                                    let idx = filtered[i];
                                    let e = &entries[idx];
                                    let sel = i == self.selected;

                                    // Fade + slide animation for deleting row
                                    let (alpha, x_offset) = if let Some((del_i, t)) = deleting {
                                        if i == del_i {
                                            let progress = (t.elapsed().as_secs_f32() / 0.15).min(1.0);
                                            (((1.0 - progress) * 255.0) as u8, -progress * 40.0)
                                        } else { (255, 0.0) }
                                    } else { (255, 0.0) };

                                    let base_color = common::row_text_color(sel);
                                    let text_color = egui::Color32::from_rgba_unmultiplied(
                                        base_color.r(), base_color.g(), base_color.b(), alpha,
                                    );

                                    let rx = rect.min.x + x_offset;
                                    let text_font = FontId::new(common::text_size(), FontFamily::Proportional);

                                    // Right-aligned timestamp
                                    let time_text = relative_time(e.last_used);
                                    let time_font = FontId::new(common::text_size() * 0.8, FontFamily::Monospace);
                                    let time_color = egui::Color32::from_rgba_unmultiplied(
                                        colors::TEXT_SUBTITLE.r(), colors::TEXT_SUBTITLE.g(),
                                        colors::TEXT_SUBTITLE.b(), alpha,
                                    );
                                    ui.painter().text(
                                        egui::pos2(rect.max.x - 12.0, rect.min.y + (common::row_height() - common::text_size() * 0.8) / 2.0),
                                        egui::Align2::RIGHT_TOP,
                                        &time_text,
                                        time_font,
                                        time_color,
                                    );

                                    if e.is_image {
                                        // Fixed square thumbnail container
                                        let container_size = common::row_height() - 4.0;
                                        let container_rect = egui::Rect::from_min_size(
                                            egui::pos2(rx + 8.0, rect.min.y + 2.0),
                                            egui::vec2(container_size, container_size),
                                        );
                                        ui.painter().rect_filled(container_rect, 2.0,
                                            egui::Color32::from_rgba_premultiplied(13, 13, 13, alpha));

                                        let row_tex = e.thumb.as_ref().or(e.texture.as_ref());
                                        if let Some(tex) = row_tex {
                                            let tex_size = tex.size_vec2();
                                            let scale = (container_size / tex_size.x).min(container_size / tex_size.y);
                                            let img_w = tex_size.x * scale;
                                            let img_h = tex_size.y * scale;
                                            let img_rect = egui::Rect::from_min_size(
                                                egui::pos2(
                                                    container_rect.min.x + (container_size - img_w) / 2.0,
                                                    container_rect.min.y + (container_size - img_h) / 2.0,
                                                ),
                                                egui::vec2(img_w, img_h),
                                            );
                                            let tint = if sel {
                                                egui::Color32::from_rgba_unmultiplied(255, 255, 255, alpha)
                                            } else {
                                                egui::Color32::from_rgba_unmultiplied(128, 128, 128, alpha)
                                            };
                                            ui.painter().image(
                                                tex.id(), img_rect,
                                                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                                                tint,
                                            );
                                        }
                                        // Resolution text in monospace
                                        let display_text = e.dims.map(|(w, h)| format!("{}×{}", w, h))
                                            .unwrap_or_else(|| "image".into());
                                        let mono_font = FontId::new(common::text_size() * 0.85, FontFamily::Monospace);
                                        let text_x = rx + 8.0 + container_size + 8.0;
                                        ui.painter().text(
                                            egui::pos2(text_x, rect.min.y + (common::row_height() - common::text_size()) / 2.0),
                                            egui::Align2::LEFT_TOP, &display_text, mono_font, text_color,
                                        );
                                    } else {
                                        let display_text = truncate(&e.text, 70);
                                        let text_pos = egui::pos2(
                                            rx + 12.0,
                                            rect.min.y + (common::row_height() - common::text_size()) / 2.0,
                                        );
                                        let highlight = colors::TEXT_PRIMARY;
                                        let indices = common::match_indices(&display_text, query);
                                        common::paint_highlighted(ui, text_pos, &display_text, &text_font, text_color, highlight, &indices);
                                    }
                                },
                            );
                            vl
                        });

                    if let Some(i) = vl_output.inner.clicked {
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
    let (width, height) = hyprland::window_size(0.618, 0.618, (500.0, 400.0));

    eframe::run_native(
        "clipboard",
        common::window_options("clipboard", width, height),
        Box::new(|cc| {
            common::setup_transparent_style(cc);
            Ok(Box::new(App::new()))
        }),
    )
}
