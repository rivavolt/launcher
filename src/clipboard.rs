//! Clipboard manager rendered on a wlr-layer-shell overlay surface (see
//! `launcher::layer`). Reads history from clipd's SQLite database. Persistent
//! daemon: idles until a SIGUSR1 show request, pops up an overlay surface that
//! grabs the keyboard, and dismisses on Escape, activation, or focus loss.

use launcher::clip::db::{self, ClipboardDb};
use launcher::clip::mime as clip_mime;
use launcher::common::{self, colors, handle_navigation_keys};
use launcher::layer::{self, LayerApp};
use launcher::scroll::ScrollMomentum;
use launcher::hyprland;
use egui::{self, CentralPanel, Context, ScrollArea, FontFamily, FontId, Ui};
use notify::{Config, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::io::Write as IoWrite;
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// Horizontal inset of the expanded focused-row content from the row edges.
const EXPAND_PAD_X: f32 = 12.0;
/// Vertical gap between stacked elements inside an expanded row.
const EXPAND_GAP: f32 = 8.0;
/// Metadata line font size in the expanded row.
const META_FONT_SIZE: f32 = 11.0;

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

/// Verbose relative time for the focused row's metadata line.
fn relative_time(unix_secs: i64) -> String {
    let diff = now_secs() - unix_secs;
    if diff < 60 { "now".into() }
    else if diff < 3600 { format!("{}m", diff / 60) }
    else if diff < 86400 { format!("{}h", diff / 3600) }
    else if diff < 7 * 86400 { format!("{}d", diff / 86400) }
    else { format!("{}w", diff / (7 * 86400)) }
}

/// Compact per-row timestamp: relative for recent items, absolute for older
/// ones. Thresholds — <1m: "now"; <1h: "5m"; <24h: "2h"; <7d: weekday ("Mon");
/// older: short date ("5 Mar", with the year once it's last year or earlier).
fn compact_time(unix_secs: i64) -> String {
    let now = now_secs();
    let diff = now - unix_secs;
    if diff < 0 { return "now".into(); }
    if diff < 60 { return "now".into(); }
    if diff < 3600 { return format!("{}m", diff / 60); }
    if diff < 86400 { return format!("{}h", diff / 3600); }
    if diff < 7 * 86400 {
        const DAYS: [&str; 7] = ["Mon", "Tue", "Wed", "Thu", "Fri", "Sat", "Sun"];
        // Unix epoch (1970-01-01) was a Thursday → index 3.
        let dow = (unix_secs.div_euclid(86400) + 3).rem_euclid(7) as usize;
        return DAYS[dow].to_string();
    }
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun",
        "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (y, m, d) = civil_from_days(unix_secs.div_euclid(86400));
    let (cur_y, _, _) = civil_from_days(now.div_euclid(86400));
    if y < cur_y {
        format!("{} {} {}", d, MONTHS[(m - 1) as usize], y)
    } else {
        format!("{} {}", d, MONTHS[(m - 1) as usize])
    }
}

/// Convert a count of days since the Unix epoch into a civil (year, month,
/// day) date. Howard Hinnant's `civil_from_days` algorithm — proleptic
/// Gregorian, valid across the full range we care about, no external deps.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[derive(Clone, Copy)]
enum DisplayItem {
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
    /// Surface width and maximum height, in logical pixels. Width is fixed;
    /// the rendered height grows with the entry list up to `max_size.1`.
    max_size: (f32, f32),
    deleting: Option<(usize, std::time::Instant)>,
    /// Bring the focused row into view next frame — set on selection change
    /// and after a focused image's texture loads (its row just grew).
    scroll_pending: bool,
    last_selected: usize,
}

impl App {
    fn new() -> Self {
        // Surface dimensions in logical pixels — same ratios as the launcher
        // (src/launcher.rs), so the two overlays are spatially consistent.
        // layer-shell `set_size` is logical, so use the monitor logical size
        // directly (no eframe 2x-HiDPI halving).
        let (mon_w, mon_h) = hyprland::monitor_logical_size();
        let max_size = if mon_w > 0.0 && mon_h > 0.0 {
            (mon_w * 0.382, mon_h * 0.618)
        } else {
            (600.0, 800.0)
        };

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
            deleting: None,
            scroll_pending: false,
            last_selected: 0,
        }
    }

    /// Get the entry index for the currently selected display item
    fn selected_entry_idx(&self) -> Option<usize> {
        match self.display.get(self.selected)? {
            DisplayItem::Entry(idx) => Some(*idx),
        }
    }

    /// Navigate to the next row, clamped to the list bounds.
    fn nav_next(&self) -> usize {
        let next = self.selected + 1;
        if next < self.display.len() { next } else { self.selected }
    }

    fn nav_prev(&self) -> usize {
        self.selected.saturating_sub(1)
    }

    fn setup_watcher(&mut self) {
        use std::sync::atomic::Ordering;

        let db_dir = db::default_db_path()
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| "/tmp".into());

        let needs_reload = self.needs_reload.clone();

        if let Ok(mut watcher) = RecommendedWatcher::new(
            move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    use notify::EventKind;
                    if matches!(event.kind, EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)) {
                        // Consumed by the event loop's next frame while visible
                        // (it polls ~60x/sec); no context-repaint nudge needed.
                        needs_reload.store(true, Ordering::SeqCst);
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
        let (is_image, needs_texture, id) = {
            let e = &self.entries[idx];
            (e.is_image, e.texture.is_none(), e.id)
        };
        if !is_image || !needs_texture || self.failed_textures.contains(&idx) {
            return;
        }

        // The full image bytes aren't held in memory (see load_entries); re-read
        // them from the DB by id only when this row actually needs its textures.
        let bytes = ClipboardDb::open_default()
            .ok()
            .and_then(|db| db.get(id).ok().flatten())
            .map(|row| row.content);
        let Some(bytes) = bytes else {
            self.failed_textures.insert(idx);
            return;
        };

        if let Ok(img) = image::load_from_memory(&bytes) {
            let full = img.to_rgba8();
            let full_size = [full.width() as usize, full.height() as usize];
            let tex = ctx.load_texture(
                format!("clip_{}", id),
                egui::ColorImage::from_rgba_unmultiplied(full_size, &full.into_raw()),
                egui::TextureOptions::LINEAR,
            );

            let thumb_img = img.resize(128, 128, image::imageops::FilterType::CatmullRom).to_rgba8();
            let thumb_size = [thumb_img.width() as usize, thumb_img.height() as usize];
            let thumb = ctx.load_texture(
                format!("clip_{}_thumb", id),
                egui::ColorImage::from_rgba_unmultiplied(thumb_size, &thumb_img.into_raw()),
                egui::TextureOptions::LINEAR,
            );

            self.entries[idx].texture = Some(tex);
            self.entries[idx].thumb = Some(thumb);
        } else {
            self.failed_textures.insert(idx);
        }
    }

    fn load_entries(&mut self, _ctx: &Context) {
        let old_selected = self.selected;
        self.failed_textures.clear();
        self.last_ensured = None;

        let db_entries = ClipboardDb::open_default()
            .and_then(|db| db.list_meta(500))
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
                // Don't retain image bytes: list_meta only fetched a 64 KiB
                // header for images (consumed above for dims), and keeping even
                // that across the whole history — let alone the full blobs we
                // used to hold — is what bloated the idle daemon and made
                // Super+C slow to page back in. Text stays inline; images
                // re-read their full bytes from the DB by id on preview/paste.
                content: if is_image { Vec::new() } else { e.content },
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

        // Flat list — one row per entry, no section headers.
        self.display = entry_indices.into_iter().map(DisplayItem::Entry).collect();
        self.selected = 0;
    }

    fn activate(&mut self) {
        let Some(idx) = self.selected_entry_idx() else { return };
        let (is_image, id, mime) = {
            let e = &self.entries[idx];
            (e.is_image, e.id, e.mime.clone())
        };

        // Images aren't kept in memory (see load_entries), so re-read the blob
        // from the DB to paste it; text content is small and still held inline.
        let content: Vec<u8> = if is_image {
            match ClipboardDb::open_default().ok().and_then(|db| db.get(id).ok().flatten()) {
                Some(row) => row.content,
                None => return,
            }
        } else {
            self.entries[idx].content.clone()
        };

        let mut cmd = Command::new("wl-copy");
        if is_image {
            cmd.arg("--type").arg(&mime);
        }
        if let Ok(mut wl) = cmd.stdin(Stdio::piped()).spawn() {
            if let Some(mut stdin) = wl.stdin.take() {
                let _ = stdin.write_all(&content);
            }
            let _ = wl.wait();
        }

        if let Ok(db) = ClipboardDb::open_default() {
            let _ = db.update_last_used(id);
        }

        self.should_hide = true;
    }

    /// Reset transient state when the overlay is dismissed. The harness has
    /// already unmapped the surface; here we clear the query and drop the
    /// (now stale, context-bound) entry textures so the next pop-up rebuilds
    /// them against its fresh egui context.
    fn hide_and_reset(&mut self) {
        self.query.clear();
        self.selected = 0;
        self.should_hide = false;
        self.loaded = false;
        self.last_ensured = None;
        self.failed_textures.clear();
        for e in &mut self.entries {
            e.texture = None;
            e.thumb = None;
        }
        self.filter();
    }

    fn delete(&mut self, ctx: &Context) {
        let Some(idx) = self.selected_entry_idx() else { return };
        let e = &self.entries[idx];

        if let Ok(db) = ClipboardDb::open_default() {
            let _ = db.delete(e.id);
        }

        self.load_entries(ctx);
    }

    /// Inner content width available to a row, after the panel's own margins.
    fn row_content_width(&self) -> f32 {
        self.max_size.0
    }

    /// Largest height the expanded image preview may occupy.
    fn max_image_preview_height(&self) -> f32 {
        self.max_size.1 * 0.5
    }

    /// Largest height the expanded text preview may occupy.
    fn max_text_preview_height(&self) -> f32 {
        self.max_size.1 * 0.55
    }

    /// Total height of the focused entry's expanded row — a self-contained
    /// card with `EXPAND_PAD_X` inset top and bottom, holding the wrapped text
    /// galley (capped) or the scaled inline image, then the metadata line.
    /// No compact summary line: the full content's own first line serves it.
    fn expanded_extra_height(&self, ctx: &Context, idx: usize) -> f32 {
        let e = &self.entries[idx];
        let inner_w = (self.row_content_width() - EXPAND_PAD_X * 2.0).max(1.0);
        // Top/bottom inset + content + gap + metadata line.
        let chrome = EXPAND_PAD_X * 2.0 + EXPAND_GAP + META_FONT_SIZE;

        if e.is_image {
            let img_h = if let Some(tex) = &e.texture {
                let ts = tex.size_vec2();
                let scale = (inner_w / ts.x).min(1.0);
                (ts.y * scale).min(self.max_image_preview_height())
            } else {
                // Texture not loaded yet — reserve from intrinsic dims.
                e.dims
                    .map(|(w, h)| {
                        let scale = (inner_w / w as f32).min(1.0);
                        (h as f32 * scale).min(self.max_image_preview_height())
                    })
                    .unwrap_or(common::row_height() * 2.0)
            };
            img_h + chrome
        } else {
            let galley = ctx.fonts_mut(|f| {
                expanded_text_galley(f, &e.text, inner_w, self.max_text_preview_height())
            });
            galley.size().y + chrome
        }
    }

    /// Per-display-item heights for the current `display` list. The focused
    /// entry is replaced by its expanded card (full content + metadata, no
    /// compact line); everything else stays compact (one row tall).
    fn item_heights(&self, ctx: &Context) -> Vec<f32> {
        self.display
            .iter()
            .enumerate()
            .map(|(i, item)| match item {
                DisplayItem::Entry(idx) => {
                    if i == self.selected {
                        self.expanded_extra_height(ctx, *idx)
                    } else {
                        common::row_height()
                    }
                }
            })
            .collect()
    }

    /// Draw one frame and return the desired total surface height in logical
    /// pixels. The harness diffs this against the live surface height and
    /// issues a `set_size` when it changes — the top-anchored auto-grow that
    /// `hyprland::resize_anchored` used to provide.
    fn render(&mut self, ctx: &Context) -> f32 {
        // The 1px gold outline + rounded corners that Hyprland window rules
        // used to paint; a layer surface has no server-side decorations.
        common::popup_border(ctx);

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
        if activate { self.activate(); return self.max_size.1; }
        if delete && self.deleting.is_none() {
            self.deleting = Some((self.selected, std::time::Instant::now()));
            ctx.request_repaint();
        }

        // Ensure texture for selected entry. A focused image row grows once
        // its texture loads, so re-scroll it into view when that happens.
        if let Some(idx) = self.selected_entry_idx() {
            if self.last_ensured != Some(idx) {
                let had_tex = self.entries[idx].texture.is_some();
                self.ensure_texture(ctx, idx);
                if !had_tex && self.entries[idx].texture.is_some() {
                    self.scroll_pending = true;
                }
                self.last_ensured = Some(idx);
            }
        }

        let input_panel = common::input_panel(ctx, &mut self.query, "Search clipboard...", None);
        if input_panel.changed || input_panel.cleared { self.filter(); }
        let input_response = input_panel.response;

        // Per-item heights — the focused entry expands in place; everything
        // else stays compact (one row tall).
        let heights = self.item_heights(ctx);
        let spacing_y = 0.0; // rows are laid out contiguously; gaps live inside rows

        let panel = CentralPanel::default()
            .frame(common::panel_frame())
            .show(ctx, |ui: &mut Ui| {
                let header_height = input_response.response.rect.height();

                // Fit the surface to the visible rows, up to the height budget.
                // The closure returns this so the harness can resize the layer
                // surface (top-anchored, so the input row stays put).
                let items_height: f32 = if heights.is_empty() {
                    if self.query.is_empty() { 0.0 } else { common::row_height() }
                } else {
                    heights.iter().sum::<f32>()
                        + spacing_y * heights.len().saturating_sub(1) as f32
                };
                let target_height =
                    (header_height + items_height).min(self.max_size.1);

                let list_height = (self.max_size.1 - header_height).max(common::row_height());
                if self.selected != self.last_selected {
                    self.scroll_pending = true;
                    self.last_selected = self.selected;
                }
                let scroll_to_selected = down || up || self.scroll_pending;
                self.scroll_pending = false;

                let has_entries = !self.display.is_empty();
                if !has_entries && !self.query.is_empty() {
                    common::empty_state(ui);
                } else {
                    let display = &self.display;
                    let entries = &self.entries;
                    let query = &self.query;
                    let selected = self.selected;
                    let deleting = self.deleting;

                    let scroll_output = ScrollArea::vertical()
                        .id_salt("clip_list")
                        .max_height(list_height)
                        .show(ui, |ui: &mut Ui| {
                            let list_top = ui.cursor().min.y;
                            let full_w = ui.available_width();

                            // Row offsets (prefix sum of heights).
                            let mut offsets = Vec::with_capacity(heights.len() + 1);
                            let mut acc = 0.0;
                            for &h in &heights {
                                offsets.push(acc);
                                acc += h + spacing_y;
                            }
                            offsets.push(acc);
                            let total_h = acc;

                            // Scroll the focused (possibly expanded) row into view.
                            if scroll_to_selected && selected < heights.len() {
                                let r = egui::Rect::from_min_size(
                                    egui::pos2(0.0, list_top + offsets[selected]),
                                    egui::vec2(full_w, heights[selected]),
                                );
                                ui.scroll_to_rect(r, None);
                            }

                            // Allocate the full content area; paint rows into it.
                            let (area, _) = ui.allocate_exact_size(
                                egui::vec2(full_w, total_h),
                                egui::Sense::hover(),
                            );

                            let clip = ui.clip_rect();
                            let mut clicked: Option<usize> = None;

                            for i in 0..display.len() {
                                let row_rect = egui::Rect::from_min_size(
                                    egui::pos2(area.min.x, list_top + offsets[i]),
                                    egui::vec2(full_w, heights[i]),
                                );
                                // Skip rows fully outside the viewport.
                                if row_rect.max.y < clip.min.y || row_rect.min.y > clip.max.y {
                                    continue;
                                }

                                let row_resp = ui.interact(
                                    row_rect,
                                    ui.id().with(("clip_row", i)),
                                    egui::Sense::click(),
                                );

                                match display[i] {
                                    DisplayItem::Entry(idx) => {
                                        let sel = i == selected;
                                        // Hover hint, painted under the content
                                        // (compact, non-selected rows only).
                                        if !sel && row_resp.hovered() {
                                            ui.painter().rect_filled(
                                                egui::Rect::from_min_size(
                                                    row_rect.min,
                                                    egui::vec2(full_w, common::row_height()),
                                                ),
                                                0.0, colors::BG_HOVER,
                                            );
                                        }
                                        paint_entry_row(
                                            ui, row_rect, &entries[idx], sel, query,
                                            deleting.filter(|(d, _)| *d == i).map(|(_, t)| t),
                                        );
                                    }
                                }

                                if row_resp.clicked() {
                                    clicked = Some(i);
                                }
                            }

                            clicked
                        });

                    common::paint_scroll_fade(ui, scroll_output.inner_rect, 16.0);

                    // Click an entry to paste it (matches keyboard Enter).
                    if let Some(i) = scroll_output.inner {
                        self.selected = i;
                        self.activate();
                    }
                }

                target_height
            });

        panel.inner
    }
}

/// Lay out the focused entry's full text for the expanded area: word-wrapped
/// to `width`, capped to whatever number of whole rows fits in `max_height`,
/// with an ellipsis on the last row when truncated. Both the height estimate
/// (`expanded_extra_height`) and the painter use this so they always agree.
fn expanded_text_galley(
    fonts: &mut egui::epaint::text::FontsView<'_>,
    text: &str,
    width: f32,
    max_height: f32,
) -> std::sync::Arc<egui::Galley> {
    let font = FontId::new(common::text_size(), FontFamily::Proportional);
    let row_h = fonts.row_height(&font);
    let max_rows = ((max_height / row_h).floor() as usize).max(1);

    // Trim surrounding blank space (internal layout is preserved) so the
    // preview doesn't open with empty lines.
    let mut job = egui::text::LayoutJob::single_section(
        text.trim().to_owned(),
        egui::TextFormat { font_id: font, color: colors::TEXT_PRIMARY, ..Default::default() },
    );
    job.wrap = egui::text::TextWrapping {
        max_rows,
        max_width: width,
        overflow_character: Some('…'),
        // Break inside long unbroken runs (URLs, tokens, base64) so the
        // preview never overflows the row width.
        break_anywhere: true,
    };
    fonts.layout_job(job)
}

/// Paint a single clipboard entry row. A non-focused row shows a single
/// compact line plus a small right-aligned timestamp. The focused row
/// (`sel` true) is replaced by an expanded card: the full word-wrapped text
/// (or a large inline image) plus a metadata line — no compact line, since
/// the full content's own first line already serves it, and no separate
/// timestamp, since the metadata line carries it. `deleting` carries the
/// fade/slide animation start time.
fn paint_entry_row(
    ui: &Ui,
    rect: egui::Rect,
    e: &Entry,
    sel: bool,
    query: &str,
    deleting: Option<std::time::Instant>,
) {
    let row_h = common::row_height();

    // Selection background spans the whole (possibly expanded) row.
    if sel {
        ui.painter().rect_filled(rect, 0.0, colors::BG_SELECTED);
        let bar = egui::Rect::from_min_size(
            rect.left_top(),
            egui::vec2(colors::ACCENT_BAR, rect.height()),
        );
        ui.painter().rect_filled(bar, 0.0, colors::ACCENT);
    }

    // Delete animation: fade out + slide left.
    let (alpha, x_offset) = if let Some(t) = deleting {
        let progress = (t.elapsed().as_secs_f32() / 0.15).min(1.0);
        (((1.0 - progress) * 255.0) as u8, -progress * 40.0)
    } else {
        (255, 0.0)
    };

    let base_color = common::row_text_color(sel);
    let text_color = egui::Color32::from_rgba_unmultiplied(
        base_color.r(), base_color.g(), base_color.b(), alpha,
    );
    let text_font = FontId::new(common::text_size(), FontFamily::Proportional);
    let rx = rect.min.x + x_offset;
    let content_y = rect.min.y + (row_h - common::text_size()) / 2.0;

    if sel {
        paint_expanded_row(ui, rect, e, x_offset);
        return;
    }

    // --- per-row timestamp (compact rows): small, dim, right-aligned ---
    // Painted first so the content line can reserve room and not overlap it.
    let time_font = FontId::new(common::text_size() * 0.78, FontFamily::Proportional);
    let time_galley = ui.painter().layout_no_wrap(
        compact_time(e.last_used), time_font, colors::TEXT_MUTED,
    );
    let time_w = time_galley.size().x;
    let time_right = rect.max.x - EXPAND_PAD_X + x_offset;
    {
        let muted = colors::TEXT_MUTED;
        let time_color = egui::Color32::from_rgba_unmultiplied(
            muted.r(), muted.g(), muted.b(),
            ((muted.a() as u32 * alpha as u32) / 255) as u8,
        );
        let ty = rect.min.y + (row_h - time_galley.size().y) / 2.0;
        ui.painter().galley(
            egui::pos2(time_right - time_w, ty), time_galley, time_color,
        );
    }
    // Right edge available to the compact content, clear of the time label.
    let content_right = time_right - time_w - 10.0;

    // --- compact line ---
    if e.is_image {
        let thumb_box = row_h - 4.0;
        let box_rect = egui::Rect::from_min_size(
            egui::pos2(rx + 8.0, rect.min.y + 2.0),
            egui::vec2(thumb_box, thumb_box),
        );
        ui.painter().rect_filled(box_rect, 2.0,
            egui::Color32::from_rgba_premultiplied(13, 13, 13, alpha));
        let row_tex = e.thumb.as_ref().or(e.texture.as_ref());
        if let Some(tex) = row_tex {
            let ts = tex.size_vec2();
            let scale = (thumb_box / ts.x).min(thumb_box / ts.y);
            let (iw, ih) = (ts.x * scale, ts.y * scale);
            let img_rect = egui::Rect::from_min_size(
                egui::pos2(
                    box_rect.min.x + (thumb_box - iw) / 2.0,
                    box_rect.min.y + (thumb_box - ih) / 2.0,
                ),
                egui::vec2(iw, ih),
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
        let label = e.dims.map(|(w, h)| format!("{}×{}", w, h))
            .unwrap_or_else(|| "image".into());
        let mono = FontId::new(common::text_size() * 0.85, FontFamily::Monospace);
        let label_x = rx + 8.0 + thumb_box + 8.0;
        let mut label_job = egui::text::LayoutJob::single_section(
            label,
            egui::TextFormat { font_id: mono, color: text_color, ..Default::default() },
        );
        label_job.wrap = egui::text::TextWrapping {
            max_rows: 1,
            max_width: (content_right - label_x).max(1.0),
            overflow_character: Some('…'),
            break_anywhere: true,
        };
        let label_galley = ui.fonts_mut(|f| f.layout_job(label_job));
        ui.painter().galley(
            egui::pos2(label_x, content_y), label_galley, egui::Color32::TRANSPARENT,
        );
    } else {
        let line = common::clip_display_line(&e.text);
        let text_x = rx + EXPAND_PAD_X;
        let avail_w = (content_right - text_x).max(1.0);
        let indices = common::match_indices(&line, query);
        let base_fmt = egui::TextFormat {
            font_id: text_font.clone(), color: text_color, ..Default::default()
        };
        let hl_fmt = egui::TextFormat {
            font_id: text_font.clone(),
            color: colors::ACCENT,
            underline: egui::Stroke::new(1.0, colors::ACCENT),
            ..Default::default()
        };
        let mut job = egui::text::LayoutJob::default();
        if indices.is_empty() {
            job.append(&line, 0.0, base_fmt);
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
        let galley = ui.fonts_mut(|f| f.layout_job(job));
        ui.painter().galley(egui::pos2(text_x, content_y), galley, egui::Color32::TRANSPARENT);
    }
}

/// Paint the focused entry's expanded card into `rect`: the full word-wrapped
/// text (or a large inline image) at the top, then a metadata line at the
/// bottom. `EXPAND_PAD_X` insets the content top and bottom; the layout
/// mirrors `expanded_extra_height` so reserved and painted heights agree.
/// There is deliberately no compact summary line and no separate timestamp —
/// the full content's first line and the metadata line cover both.
fn paint_expanded_row(ui: &Ui, rect: egui::Rect, e: &Entry, x_offset: f32) {
    let inner_x = rect.min.x + EXPAND_PAD_X + x_offset;
    let inner_w = (rect.width() - EXPAND_PAD_X * 2.0).max(1.0);
    let content_top = rect.min.y + EXPAND_PAD_X;
    let meta_y = rect.max.y - EXPAND_PAD_X - META_FONT_SIZE;
    // Content runs from the top inset down to the gap above the metadata line.
    let content_h = (meta_y - EXPAND_GAP - content_top).max(1.0);

    if e.is_image {
        if let Some(tex) = &e.texture {
            let ts = tex.size_vec2();
            let scale = (inner_w / ts.x).min(content_h / ts.y).min(1.0);
            let (iw, ih) = (ts.x * scale, ts.y * scale);
            let img_rect = egui::Rect::from_min_size(
                egui::pos2(inner_x, content_top),
                egui::vec2(iw, ih),
            );
            ui.painter().image(
                tex.id(), img_rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );
        }
    } else {
        let galley = ui.fonts_mut(|f| expanded_text_galley(f, &e.text, inner_w, content_h));
        ui.painter().galley(
            egui::pos2(inner_x, content_top), galley, colors::TEXT_PRIMARY,
        );
    }

    // Metadata line: dims/MIME (when non-obvious) · source app · relative time.
    let meta_font = FontId::new(META_FONT_SIZE, FontFamily::Monospace);
    let mut parts: Vec<String> = Vec::new();
    if e.is_image {
        if let Some((w, h)) = e.dims {
            parts.push(format!("{}×{}", w, h));
        }
    }
    if common::should_show_mime_label(&e.mime) {
        parts.push(e.mime.clone());
    }
    if let Some(app) = &e.source_app {
        parts.push(app.clone());
    }
    parts.push(relative_time(e.last_used));
    ui.painter().text(
        egui::pos2(inner_x, meta_y),
        egui::Align2::LEFT_TOP,
        parts.join("  ·  "),
        meta_font,
        colors::TEXT_SUBTITLE,
    );
}

impl LayerApp for App {
    fn width(&self) -> u32 {
        self.max_size.0.round().max(1.0) as u32
    }

    fn init_height(&self) -> u32 {
        // Start at the input row's height; the first frame's auto-resize grows
        // it to fit the entry list.
        (common::input_size() + 16.0).round() as u32
    }

    fn on_frame_init(&mut self, ctx: &Context) {
        // The egui context is rebuilt per pop-up, so re-apply fonts/style.
        common::setup_transparent_style(ctx);
    }

    // Reload happens via the `!self.loaded` path in `update_ui` (on_hidden
    // clears `loaded`), so each pop-up refreshes history made while idle.

    fn update_ui(&mut self, ctx: &Context) -> (f32, bool) {
        use std::sync::atomic::Ordering;
        if !self.loaded {
            self.load_entries(ctx);
            self.needs_reload.store(false, Ordering::SeqCst);
        } else if self.needs_reload.swap(false, Ordering::SeqCst) {
            self.load_entries(ctx);
        }
        self.scroll_momentum.update(ctx);
        let height = self.render(ctx);
        (height, self.should_hide)
    }

    fn on_hidden(&mut self) {
        self.hide_and_reset();
    }
}

fn main() {
    let mut app = App::new();
    // Watch clipd's database for the process lifetime so a copy made while the
    // clipboard idles marks the list stale for the next pop-up.
    app.setup_watcher();
    layer::run("clipboard", app);
}
