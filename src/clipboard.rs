//! Clipboard manager — reads from clipd's SQLite database

use launcher::clip::db::{self, ClipboardDb};
use launcher::clip::mime as clip_mime;
use launcher::common::{self, colors, handle_navigation_keys, virtual_list};
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
    is_image: bool,
    dims: Option<(u32, u32)>,
    texture: Option<egui::TextureHandle>,
    thumb: Option<egui::TextureHandle>,
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64
}

fn relative_time(unix_secs: i64) -> String {
    let diff = now_secs() - unix_secs;
    if diff < 60 { "now".into() }
    else if diff < 3600 { format!("{}m", diff / 60) }
    else if diff < 86400 { format!("{}h", diff / 3600) }
    else if diff < 7 * 86400 { format!("{}d", diff / 86400) }
    else { format!("{}w", diff / (7 * 86400)) }
}

fn time_bucket(unix_secs: i64, now: i64) -> &'static str {
    let diff = now - unix_secs;
    if diff < 300 { "Just now" }
    else if diff < 86400 { "Today" }
    else if diff < 172800 { "Yesterday" }
    else if diff < 604800 { "This week" }
    else if diff < 2592000 { "This month" }
    else { "Older" }
}

#[derive(Clone, Copy)]
enum DisplayItem {
    Header(&'static str),
    Entry(usize), // index into entries
}

struct App {
    query: String,
    entries: Vec<Entry>,
    display: Vec<DisplayItem>,
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
    monitor_size: (f32, f32),
    last_height: f32,
    deleting: Option<(usize, std::time::Instant)>,
}

impl App {
    fn new() -> Self {
        let eframe_size = hyprland::window_size(0.618, 0.618, (500.0, 400.0));
        let max_size = (eframe_size.0 * 2.0, eframe_size.1 * 2.0);
        let monitor_size = hyprland::monitor_logical_size();

        Self {
            query: String::new(),
            entries: Vec::new(),
            display: Vec::new(),
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
            monitor_size,
            last_height: 0.0,
            deleting: None,
        }
    }

    /// Get the entry index for the currently selected display item
    fn selected_entry_idx(&self) -> Option<usize> {
        match self.display.get(self.selected)? {
            DisplayItem::Entry(idx) => Some(*idx),
            DisplayItem::Header(_) => None,
        }
    }

    /// Navigate to the next Entry item in the given direction, skipping headers
    fn nav_next(&self) -> usize {
        let mut next = self.selected + 1;
        while next < self.display.len() {
            if matches!(self.display[next], DisplayItem::Entry(_)) { return next; }
            next += 1;
        }
        self.selected
    }

    fn nav_prev(&self) -> usize {
        if self.selected == 0 { return self.selected; }
        let mut prev = self.selected - 1;
        loop {
            if matches!(self.display[prev], DisplayItem::Entry(_)) { return prev; }
            if prev == 0 { return self.selected; }
            prev -= 1;
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
                text,
                is_image,
                dims,
                texture: None,
                thumb: None,
            }
        }).collect();

        self.filter();
        self.selected = old_selected.min(self.display.len().saturating_sub(1));
        // Snap to nearest entry if we landed on a header
        if matches!(self.display.get(self.selected), Some(DisplayItem::Header(_))) {
            self.selected = self.nav_next();
        }
        self.loaded = true;
    }

    fn filter(&mut self) {
        let q = self.query.to_lowercase();
        let entry_indices: Vec<usize> = if q.is_empty() {
            (0..self.entries.len().min(50)).collect()
        } else {
            self.entries.iter().enumerate()
                .filter(|(_, e)| !e.is_image && e.text.to_lowercase().contains(&q))
                .map(|(i, _)| i)
                .take(50)
                .collect()
        };

        // Build display list with section headers
        self.display.clear();
        let now = now_secs();
        let mut last_bucket = "";
        for &idx in &entry_indices {
            let bucket = time_bucket(self.entries[idx].last_used, now);
            if bucket != last_bucket {
                self.display.push(DisplayItem::Header(bucket));
                last_bucket = bucket;
            }
            self.display.push(DisplayItem::Entry(idx));
        }

        // Select first entry (skip headers)
        self.selected = self.display.iter()
            .position(|d| matches!(d, DisplayItem::Entry(_)))
            .unwrap_or(0);
    }

    fn activate(&mut self) {
        let Some(idx) = self.selected_entry_idx() else { return };
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
        hyprland::dispatch_async(r#"hl.dsp.workspace.toggle_special("clipboard")"#);
    }

    fn delete(&mut self, ctx: &Context) {
        let Some(idx) = self.selected_entry_idx() else { return };
        let e = &self.entries[idx];

        if let Ok(db) = ClipboardDb::open_default() {
            let _ = db.delete(e.id);
        }

        self.load_entries(ctx);
    }

    fn render(&mut self, ctx: &Context) {
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
                self.deleting = None;
                self.delete(ctx);
            } else {
                ctx.request_repaint();
            }
        }

        if down { self.selected = self.nav_next(); }
        if up { self.selected = self.nav_prev(); }
        if activate { self.activate(); return; }
        if delete && self.deleting.is_none() {
            self.deleting = Some((self.selected, std::time::Instant::now()));
            ctx.request_repaint();
        }

        // Ensure texture for selected entry
        if let Some(idx) = self.selected_entry_idx() {
            if self.last_ensured != Some(idx) {
                self.ensure_texture(ctx, idx);
                self.last_ensured = Some(idx);
            }
        }

        let input_panel = common::input_panel(ctx, &mut self.query, "Search clipboard...", None);
        if input_panel.changed || input_panel.cleared { self.filter(); }
        let input_response = input_panel.response;

        // Check if selected entry has preview content
        let (has_preview, preview_content_height) = self.selected_entry_idx()
            .map(|idx| {
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
                    let font = FontId::new(common::text_size(), FontFamily::Proportional);
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
                    if let Some(idx) = self.selected_entry_idx() {
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
                                    let font = FontId::new(common::text_size(), FontFamily::Proportional);
                                    let galley = ui.painter().layout(
                                        e.text.clone(),
                                        font,
                                        colors::TEXT_PRIMARY,
                                        ui.available_width(),
                                    );
                                    let (rect, _) = ui.allocate_exact_size(galley.size(), egui::Sense::hover());
                                    ui.painter().galley(rect.min, galley, colors::TEXT_PRIMARY);
                                }

                                // Metadata line — hide obvious-default MIME types
                                ui.add_space(8.0);
                                let meta_font = FontId::new(common::text_size() * 0.75, FontFamily::Monospace);
                                let mut parts: Vec<&str> = Vec::new();
                                if common::should_show_mime_label(&e.mime) {
                                    parts.push(e.mime.as_str());
                                }
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
                let num_items = self.display.len().min(max_visible);
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
                    // Top-anchored: pin the input at a fixed Y as the list grows,
                    // instead of letting the resize recentroid (which would slide
                    // the input vertically each keystroke).
                    hyprland::resize_anchored(
                        "clipboard",
                        self.max_size.0 as i32, target_height as i32,
                        self.monitor_size.0, self.monitor_size.1,
                        common::Y_ANCHOR_RATIO,
                    );
                }

                let list_height = (self.max_size.1 - header_height).max(common::row_height());
                let scroll_to_selected = down || up;

                let has_entries = self.display.iter().any(|d| matches!(d, DisplayItem::Entry(_)));
                if !has_entries && !self.query.is_empty() {
                    common::empty_state(ui);
                } else {
                    let display = &self.display;
                    let entries = &self.entries;
                    let query = &self.query;

                    let scroll_output = ScrollArea::vertical()
                        .id_salt("clip_list")
                        .max_height(list_height)
                        .show(ui, |ui: &mut Ui| {
                            let deleting = self.deleting;
                            let vl = virtual_list(
                                ui,
                                display.len(),
                                common::row_height(),
                                self.selected,
                                scroll_to_selected,
                                true, // we handle selection highlight ourselves
                                |ui, i, rect| {
                                    match display[i] {
                                        DisplayItem::Header(label) => {
                                            // Section header: label with subtle line
                                            let font = FontId::new(common::text_size() * 0.8, FontFamily::Proportional);
                                            let galley = ui.painter().layout_no_wrap(
                                                label.to_string(), font.clone(), colors::TEXT_SUBTITLE,
                                            );
                                            let text_w = galley.size().x;
                                            let y = rect.center().y;
                                            ui.painter().galley(
                                                egui::pos2(rect.min.x + 12.0, y - galley.size().y / 2.0),
                                                galley,
                                                colors::TEXT_SUBTITLE,
                                            );
                                            // Subtle line after label
                                            ui.painter().line_segment(
                                                [egui::pos2(rect.min.x + 12.0 + text_w + 8.0, y),
                                                 egui::pos2(rect.max.x - 12.0, y)],
                                                egui::Stroke::new(0.5, colors::TEXT_MUTED),
                                            );
                                        }
                                        DisplayItem::Entry(idx) => {
                                            let e = &entries[idx];
                                            let sel = i == self.selected;

                                            // Selection highlight (since we use skip_selected_highlight)
                                            if sel {
                                                ui.painter().rect_filled(rect, 0.0, colors::BG_SELECTED);
                                                let bar = egui::Rect::from_min_size(
                                                    rect.left_top(),
                                                    egui::vec2(colors::ACCENT_BAR, common::row_height()),
                                                );
                                                ui.painter().rect_filled(bar, 0.0, colors::ACCENT);
                                            }

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
                                            let content_y = rect.min.y + (common::row_height() - common::text_size()) / 2.0;

                                            if e.is_image {
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
                                                let display_text = e.dims.map(|(w, h)| format!("{}×{}", w, h))
                                                    .unwrap_or_else(|| "image".into());
                                                let mono_font = FontId::new(common::text_size() * 0.85, FontFamily::Monospace);
                                                let text_x = rx + 8.0 + container_size + 8.0;
                                                ui.painter().text(
                                                    egui::pos2(text_x, content_y),
                                                    egui::Align2::LEFT_TOP, &display_text, mono_font, text_color,
                                                );
                                            } else {
                                                let display = common::clip_display_line(&e.text);
                                                let line = display.as_str();
                                                let text_x = rx + 12.0;
                                                let avail_w = rect.max.x - text_x - 12.0;
                                                let text_pos = egui::pos2(text_x, content_y);
                                                let indices = common::match_indices(line, query);

                                                let base_fmt = egui::TextFormat { font_id: text_font.clone(), color: text_color, ..Default::default() };
                                                let hl_fmt = egui::TextFormat {
                                                    font_id: text_font.clone(),
                                                    color: colors::ACCENT,
                                                    underline: egui::Stroke::new(1.0, colors::ACCENT),
                                                    ..Default::default()
                                                };

                                                let mut job = egui::text::LayoutJob::default();
                                                if indices.is_empty() {
                                                    job.append(line, 0.0, base_fmt);
                                                } else {
                                                    let mut last = 0;
                                                    for &idx in &indices {
                                                        if idx > last { job.append(&line[last..idx], 0.0, base_fmt.clone()); }
                                                        let ch_len = line[idx..].chars().next().map_or(1, |c| c.len_utf8());
                                                        job.append(&line[idx..idx + ch_len], 0.0, hl_fmt.clone());
                                                        last = idx + ch_len;
                                                    }
                                                    if last < line.len() { job.append(&line[last..], 0.0, base_fmt); }
                                                }
                                                job.wrap = egui::text::TextWrapping {
                                                    max_rows: 1,
                                                    max_width: avail_w,
                                                    overflow_character: Some('…'),
                                                    break_anywhere: true,
                                                };
                                                let galley = ui.fonts(|f| f.layout_job(job));
                                                ui.painter().galley(text_pos, galley, egui::Color32::TRANSPARENT);
                                            }
                                        }
                                    }
                                },
                            );
                            vl
                        });

                    common::paint_scroll_fade(ui, scroll_output.inner_rect, 16.0);

                    if let Some(i) = scroll_output.inner.clicked {
                        if matches!(display[i], DisplayItem::Entry(_)) {
                            self.selected = i;
                            self.activate();
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
