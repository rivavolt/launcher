//! App launcher rendered on a wlr-layer-shell overlay surface (see
//! `launcher::layer`). Persistent daemon: idles until a SIGUSR1 show request,
//! pops up an overlay surface that grabs the keyboard, and dismisses on
//! Escape, activation, or focus loss.

use egui::{self, CentralPanel, Context, Color32, ScrollArea, Ui, FontFamily, FontId};
use launcher::common::{self, colors, handle_navigation_keys, virtual_list};
use launcher::layer::{self, LayerApp};
use launcher::scroll::ScrollMomentum;
use launcher::usage::UsageLog;
use launcher::{desktop, hyprland};
use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};
use std::collections::HashMap;
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use std::{env, fs};
use strsim::jaro_winkler;

const MAX_VISIBLE_ITEMS: usize = 15;

fn icon_size() -> f32 { (common::text_size() * 1.5).round() }
fn icon_container() -> f32 { common::icon_container() }
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

    /// Primary fields (name, title, class) — full weight in scoring
    fn primary_fields(&self) -> Vec<&str> {
        match self {
            Entry::Desktop { name, .. } => vec![name.as_str()],
            Entry::Window { title, class, .. } => vec![title.as_str(), class.as_str()],
        }
    }

    /// Secondary fields (keywords, generic name) — reduced weight in scoring
    fn secondary_fields(&self) -> Vec<&str> {
        match self {
            Entry::Desktop { keywords, .. } => keywords.iter().map(|s| s.as_str()).collect(),
            Entry::Window { .. } => vec![],
        }
    }
}

struct App {
    query: String,
    entries: Vec<Entry>,
    filtered: Vec<usize>,
    selected: usize,
    should_hide: bool,
    loaded: bool,
    held_key: Option<(egui::Key, std::time::Instant)>,
    matcher: Matcher,
    needs_reload: Arc<AtomicBool>,
    _hypr_thread: Option<std::thread::JoinHandle<()>>,
    scroll_momentum: ScrollMomentum,
    /// Surface width and maximum height, in logical pixels. Width is fixed;
    /// the rendered height grows with the result list up to `max_size.1`.
    max_size: (f32, f32),
    usage: Arc<Mutex<UsageLog>>,
    // Caches
    ghost_text_cache: String,
    display_names: HashMap<usize, String>,
    last_content_width: f32,
    /// Icon textures keyed by source path. Within one pop-up each
    /// `load_entries` would otherwise re-upload the same ~200 icons via
    /// `ctx.load_texture` (which always allocates a fresh GPU texture — there
    /// is no by-name caching inside egui), and dropping the old
    /// `TextureHandle`s only queues the frees for the next frame, so rapid
    /// reloads overlap two full sets of textures. Persisting handles here keeps
    /// the GPU set bounded by the installed-app count. The egui context is
    /// rebuilt with each pop-up, so this is cleared on hide (handles from a
    /// dropped context are stale).
    icon_cache: HashMap<PathBuf, egui::TextureHandle>,
    /// Memoized icon path index (icon name → file). `build_icon_index` walks
    /// every theme/size/category directory and `cache_svgs` shells out to
    /// `magick`; both are far too expensive to redo on every window event, and
    /// the icon set is static for the session — so it's built once and reused
    /// across `load_entries` calls. (Distinct from `icon_cache`, which holds
    /// the GPU textures; this is the path index that feeds it.)
    icon_index: Option<HashMap<String, PathBuf>>,
}

impl App {
    fn new() -> Self {
        // Surface dimensions in logical pixels: width = 0.382 of the monitor,
        // height capped at 0.618. (The old eframe path computed half these and
        // relied on eframe's 2x HiDPI scaling; layer-shell `set_size` is in
        // logical pixels directly, so use the monitor logical size as-is.)
        let (mon_w, mon_h) = hyprland::monitor_logical_size();
        let max_size = if mon_w > 0.0 && mon_h > 0.0 {
            (mon_w * 0.382, mon_h * 0.618)
        } else {
            (600.0, 800.0)
        };
        Self {
            query: String::new(),
            entries: Vec::new(),
            filtered: Vec::new(),
            selected: 0,
            should_hide: false,
            loaded: false,
            held_key: None,
            matcher: Matcher::new(Config::DEFAULT),
            needs_reload: Arc::new(AtomicBool::new(false)),
            _hypr_thread: None,
            scroll_momentum: ScrollMomentum::new(),
            max_size,
            usage: Arc::new(Mutex::new(UsageLog::load())),
            ghost_text_cache: String::new(),
            display_names: HashMap::new(),
            last_content_width: 0.0,
            icon_cache: HashMap::new(),
            icon_index: None,
        }
    }

    fn setup_hyprland_events(&mut self) {
        let needs_reload = self.needs_reload.clone();
        let usage = self.usage.clone();

        let mut active_class: Option<String> = None;
        let mut focused_at: f64 = 0.0;

        self._hypr_thread = hyprland::subscribe_events(move |line| {
            // Only window list changes warrant a reload — title changes fire
            // constantly (every browser tab switch, every terminal prompt) and
            // load_entries is heavy. Stale titles on Window entries are a
            // minor display issue; reloading on every title change was the
            // driver of unbounded memory growth. The `needs_reload` flag is
            // consumed by the event loop's next frame (it polls ~60x/sec while
            // visible), so no context-repaint nudge is needed here.
            if line.starts_with("openwindow>>")
                || line.starts_with("closewindow>>")
                || line.starts_with("movewindow>>")
            {
                needs_reload.store(true, Ordering::SeqCst);
            } else if let Some(rest) = line.strip_prefix("activewindow>>") {
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs_f64();

                // Record focus duration for the previous window
                if let Some(prev) = active_class.take() {
                    let duration = now - focused_at;
                    if let Ok(mut u) = usage.lock() {
                        u.record_focus(&prev, duration);
                    }
                }

                // Track the newly focused window
                let class = rest.split(',').next().unwrap_or("").to_string();
                if !class.is_empty() && class != "launcher" {
                    active_class = Some(class);
                    focused_at = now;
                }
            }
        });
    }

    fn load_entries(&mut self, ctx: &Context) {
        let old_selected = self.selected;
        // Memoized icon index — see the field doc. build_icon_index walks every
        // theme/size/category dir and cache_svgs spawns `magick` per uncached
        // SVG, both too costly to redo on each window add/remove/move. Built
        // once, then reused; taken out of self for the borrow and put back.
        let icon_index = self.icon_index.take().unwrap_or_else(|| {
            let mut idx = desktop::build_icon_index();
            desktop::cache_svgs(&mut idx);
            idx
        });
        let desktop_entries = desktop::collect_entries();
        let wmclass_icons = desktop::wmclass_icon_map(&desktop_entries, &icon_index);

        self.entries = self.collect_hyprland_windows(ctx, &icon_index, &wmclass_icons);
        let new_desktop = self.convert_desktop_entries(ctx, &icon_index, desktop_entries);
        self.entries.extend(new_desktop);
        self.icon_index = Some(icon_index);

        self.filter();
        self.selected = old_selected.min(self.filtered.len().saturating_sub(1));
        self.loaded = true;
    }

    /// Return a handle to the texture for `path`, loading it on first use.
    /// Bounded by the installed-app count; no eviction needed.
    fn icon_for(&mut self, ctx: &Context, path: &PathBuf) -> Option<egui::TextureHandle> {
        if let Some(tex) = self.icon_cache.get(path) {
            return Some(tex.clone());
        }
        let tex = load_icon(ctx, path)?;
        self.icon_cache.insert(path.clone(), tex.clone());
        Some(tex)
    }

    fn collect_hyprland_windows(
        &mut self,
        ctx: &Context,
        icon_index: &HashMap<String, PathBuf>,
        wmclass_icons: &HashMap<String, PathBuf>,
    ) -> Vec<Entry> {
        hyprland::clients()
            .into_iter()
            .filter(|c| !c.class.is_empty() && c.class != "launcher")
            .filter(|c| !c.workspace.name.starts_with("special:"))
            .filter(|c| !c.pinned)
            .map(|c| {
                let class_lower = c.class.to_lowercase();
                let icon_path = wmclass_icons.get(&class_lower)
                    .or_else(|| icon_index.get(&class_lower))
                    .cloned();
                let icon = icon_path.and_then(|p| self.icon_for(ctx, &p));
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

    fn convert_desktop_entries(
        &mut self,
        ctx: &Context,
        icon_index: &HashMap<String, PathBuf>,
        entries: Vec<desktop::DesktopEntry>,
    ) -> Vec<Entry> {
        entries
            .into_iter()
            .map(|de| {
                let icon_path = de.icon.as_ref().and_then(|i| i.resolve(icon_index));
                let icon = icon_path.and_then(|p| self.icon_for(ctx, &p));
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

    fn default_order(&self) -> Vec<usize> {
        let usage = self.usage.lock().unwrap();
        let mut scored: Vec<(f64, usize)> = (0..self.entries.len().min(50))
            .map(|i| (usage.score(self.entries[i].name()), i))
            .collect();
        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.into_iter().map(|(_, i)| i).collect()
    }

    fn filter(&mut self) {
        if self.query.is_empty() {
            self.filtered = self.default_order();
        } else {
            let usage = self.usage.lock().unwrap();
            let query_lower = self.query.to_lowercase();
            let tokens: Vec<&str> = query_lower.split_whitespace().collect();
            let is_multi = tokens.len() > 1;

            // Primary token (first word) drives main scoring
            let primary_token = tokens[0];
            let pattern = Pattern::parse(primary_token, CaseMatching::Ignore, Normalization::Smart);

            // Additional tokens for AND filtering
            let extra_patterns: Vec<Pattern> = tokens[1..].iter()
                .map(|t| Pattern::parse(t, CaseMatching::Ignore, Normalization::Smart))
                .collect();

            let mut scored: Vec<_> = self.entries.iter().enumerate()
                .filter_map(|(idx, e)| {
                    let primary = e.primary_fields();
                    let secondary = e.secondary_fields();
                    let all_fields: Vec<&str> = primary.iter().chain(secondary.iter()).copied().collect();

                    // Multi-word AND: all extra tokens must match somewhere
                    if is_multi {
                        for ep in &extra_patterns {
                            let matched = all_fields.iter().any(|s| {
                                let mut buf = Vec::new();
                                let haystack = Utf32Str::new(s, &mut buf);
                                ep.score(haystack, &mut self.matcher).unwrap_or(0) > 0
                            }) || all_fields.iter().any(|s| {
                                let sl = s.to_lowercase();
                                tokens[1..].iter().any(|t| sl.contains(t))
                            });
                            if !matched { return None; }
                        }
                    }

                    // Score primary token against fields
                    let score_pat = |fields: &[&str], pat: &Pattern, m: &mut Matcher| -> u32 {
                        fields.iter()
                            .filter_map(|s| {
                                let mut buf = Vec::new();
                                let haystack = Utf32Str::new(s, &mut buf);
                                pat.score(haystack, m)
                            })
                            .max()
                            .unwrap_or(0)
                    };

                    let primary_score = score_pat(&primary, &pattern, &mut self.matcher);
                    let primary_jw = if primary_score == 0 {
                        primary.iter()
                            .map(|s| (jaro_winkler(primary_token, &s.to_lowercase()) * 1000.0) as u32)
                            .filter(|&s| s >= 850).max().unwrap_or(0)
                    } else { 0 };

                    let secondary_score = (score_pat(&secondary, &pattern, &mut self.matcher) as f32 * 0.3) as u32;
                    let secondary_jw = if secondary_score == 0 {
                        (secondary.iter()
                            .map(|s| (jaro_winkler(primary_token, &s.to_lowercase()) * 1000.0) as u32)
                            .filter(|&s| s >= 850).max().unwrap_or(0) as f32 * 0.3) as u32
                    } else { 0 };

                    let nucleo_score = primary_score.max(secondary_score);
                    let jw_score = primary_jw.max(secondary_jw);

                    let name_lower = e.name().to_lowercase();
                    let prefix_bonus: u32 = if name_lower.starts_with(primary_token)
                    { 10000 } else { 0 };

                    let word_start_bonus: u32 = if primary.iter().any(|s| {
                        let s_lower = s.to_lowercase();
                        s_lower.split(|c: char| !c.is_alphanumeric()).any(|w| w.starts_with(primary_token))
                    }) { 4000 } else { 0 };

                    let name_bonus: u32 = if primary_score > 0 || name_lower.contains(primary_token)
                    { 5000 } else { 0 };

                    // Usage-based bonus: frecency score capped at 5000
                    let usage_bonus: u32 = (usage.score(e.name()).min(50.0) * 100.0) as u32;

                    // Query-specific frecency: boost entries previously chosen for this query
                    let query_bonus: u32 = (usage.query_score(&self.query, e.name()).min(10.0) * 500.0) as u32;

                    let length_bonus: u32 = {
                        let ratio = primary_token.len() as f32 / name_lower.len().max(1) as f32;
                        (ratio.min(1.0) * 1000.0) as u32
                    };

                    let base_score = nucleo_score.max(jw_score) + prefix_bonus + name_bonus + word_start_bonus;
                    if base_score == 0 { return None; }

                    // Filter scattered fuzzy matches: require substring or minimum score
                    if !is_multi {
                        let has_substring = all_fields.iter()
                            .any(|s| s.to_lowercase().contains(&query_lower));
                        if !has_substring && base_score < query_lower.len() as u32 * 20 {
                            return None;
                        }
                    }

                    let match_score = base_score + usage_bonus + query_bonus + length_bonus;
                    Some((match_score, idx))
                })
                .collect();

            scored.sort_by(|a, b| b.0.cmp(&a.0));
            self.filtered = scored.into_iter().map(|(_, idx)| idx).take(50).collect();
        }
        self.selected = 0;
        self.display_names.clear();
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

            // Record activation in usage log
            if let Ok(mut usage) = self.usage.lock() {
                usage.record_launch(e.name());
                if !self.query.is_empty() {
                    usage.record_selection(&self.query, e.name());
                }
            }

            match e {
                Entry::Desktop { exec, terminal, .. } => {
                    let parts = desktop::parse_exec(exec);
                    if let Some((bin, args)) = parts.split_first() {
                        // The overlay floats over the user's real workspace
                        // (no special workspace to dismiss any more), so the
                        // spawned app maps there directly. The harness unmaps
                        // the overlay once this returns and `should_hide` is set.
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
                    hyprland::dispatch(&format!(r#"hl.dsp.focus({{ window = "address:{address}" }})"#));
                }
            }
        }
        self.should_hide = true;
    }

    /// Reset transient state when the overlay is dismissed. The harness has
    /// already unmapped the surface; here we persist usage, clear the query, and
    /// drop the per-entry icon handles (entries are rebuilt on the next show).
    /// The `icon_cache` itself is kept: it is keyed by icon file path and the
    /// egui context now persists across pop-ups, so the decoded textures stay
    /// valid and the next show reuses them instead of re-decoding every PNG.
    fn hide_and_reset(&mut self) {
        if let Ok(mut usage) = self.usage.lock() {
            usage.save();
        }
        self.query.clear();
        self.selected = 0;
        self.filtered = self.default_order();
        self.should_hide = false;
        self.loaded = false;
        self.display_names.clear();
        for e in &mut self.entries {
            let (Entry::Desktop { icon, .. } | Entry::Window { icon, .. }) = e;
            *icon = None;
        }
    }

    /// Draw one frame and return the desired total surface height in logical
    /// pixels. The harness diffs this against the live surface height and
    /// issues a `set_size` when it changes — the top-anchored auto-grow that
    /// `hyprland::resize_anchored` used to provide.
    fn render(&mut self, ctx: &Context) -> f32 {
        // The 1px gold outline + rounded corners that Hyprland window rules
        // used to paint; a layer surface has no server-side decorations.
        common::popup_border(ctx);

        if self.entries.is_empty() {
            self.load_entries(ctx);
        }

        let max_sel = self.filtered.len().saturating_sub(1);
        let mut activate = false;

        let (down, up) = handle_navigation_keys(ctx, &mut self.held_key);

        let mut accept_ghost = false;
        ctx.input(|i: &egui::InputState| {
            for event in &i.events {
                if let egui::Event::Key { key, pressed: true, .. } = event {
                    match key {
                        egui::Key::Escape => self.should_hide = true,
                        egui::Key::Enter => activate = true,
                        egui::Key::Tab => accept_ghost = true,
                        _ => {}
                    }
                }
            }
        });

        if accept_ghost {
            if !self.ghost_text_cache.is_empty() {
                self.query.push_str(&self.ghost_text_cache);
                self.filter();
            } else if self.filtered.len() > 1 {
                // No ghost text: cycle to next result and adopt its name
                self.selected = (self.selected + 1) % self.filtered.len();
                if let Some(&idx) = self.filtered.get(self.selected) {
                    self.query = self.entries[idx].name().to_string();
                    self.filter();
                }
            }
        }

        if down { self.selected = (self.selected + 1).min(max_sel); }
        if up { self.selected = self.selected.saturating_sub(1); }
        if down || up {
            if let Some(&idx) = self.filtered.get(self.selected) {
                if let Entry::Window { ref workspace, ref address, .. } = self.entries[idx] {
                    hyprland::dispatch_batch_async(&[
                        format!(r#"hl.dsp.focus({{ workspace = "{workspace}" }})"#),
                        format!(r#"hl.dsp.window.alter_zorder({{ mode = "top", window = "address:{address}" }})"#),
                    ]);
                }
            }
        }
        if activate { self.activate(); return self.max_size.1; }

        // Input panel
        let ghost = if self.ghost_text_cache.is_empty() { None } else { Some(self.ghost_text_cache.as_str()) };
        let input_panel = common::input_panel(ctx, &mut self.query, "Search...", ghost);
        if input_panel.changed || input_panel.cleared { self.filter(); }
        if accept_ghost {
            if let Some(mut state) = egui::TextEdit::load_state(ctx, input_panel.text_edit_id) {
                let ccursor = egui::text::CCursor::new(self.query.chars().count());
                state.cursor.set_char_range(Some(egui::text::CCursorRange::one(ccursor)));
                state.store(ctx, input_panel.text_edit_id);
            }
        }
        let input_response = input_panel.response;

        // List panel. Its closure returns the desired total surface height so
        // the harness can resize the layer surface (top-anchored, so the input
        // row stays put as the list grows).
        let panel = CentralPanel::default()
            .frame(common::panel_frame())
            .show(ctx, |ui: &mut Ui| {
                let content_width = ui.available_width();
                let row_height = icon_container() + row_padding() * 2.0;
                let header_height = input_response.response.rect.height();
                let spacing_y = ui.spacing().item_spacing.y;

                // Fit the surface to the visible rows, up to the height budget.
                let num_items = self.filtered.len().min(MAX_VISIBLE_ITEMS);
                let list_height = if num_items > 0 {
                    num_items as f32 * row_height + (num_items - 1) as f32 * spacing_y
                } else if !self.query.is_empty() {
                    row_height
                } else {
                    0.0
                };
                let target_height = (header_height + list_height).min(self.max_size.1);

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
                    common::empty_state(ui);
                } else {
                    let filtered = &self.filtered;
                    let entries = &self.entries;
                    let display_names = &self.display_names;
                    let query = &self.query;

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
                                let e = &entries[idx];
                                let sel = i == self.selected;
                                let text_color = common::row_text_color(sel);
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
                                let highlight = colors::ACCENT;
                                if let Some(sub) = e.subtitle() {
                                    let right_margin = icon_container() + row_padding() * 2.0;
                                    let avail = content_width - text_x - right_margin;
                                    let title_display = truncate_to_width(ui, sub, text_font.clone(), avail);
                                    let total_h = text_size + line_gap + subtitle_size;
                                    let primary_y = row_y + (row_height - total_h) / 2.0;
                                    let title_matches = common::match_indices(&title_display, query);
                                    common::paint_highlighted(ui, egui::pos2(text_x, primary_y), &title_display, &text_font, text_color, highlight, &title_matches);
                                    let sub_color = if sel { colors::TEXT_SECONDARY } else { colors::TEXT_SUBTITLE };
                                    let name_matches = common::match_indices(display_name, query);
                                    common::paint_highlighted(ui, egui::pos2(text_x, primary_y + text_size + line_gap), display_name, &subtitle_font, sub_color, highlight, &name_matches);
                                } else {
                                    let text_y = row_y + (row_height - text_size) / 2.0;
                                    let name_matches = common::match_indices(display_name, query);
                                    common::paint_highlighted(ui, egui::pos2(text_x, text_y), display_name, &text_font, text_color, highlight, &name_matches);
                                }

                                if let Entry::Window { workspace, .. } = e {
                                    let chip_cx = content_width - row_padding() - icon_container() / 2.0;
                                    let chip_cy = row_y + row_height / 2.0;
                                    common::paint_workspace_chip(
                                        ui,
                                        egui::pos2(chip_cx, chip_cy),
                                        workspace,
                                    );
                                }
                            },
                        )
                    });

                    common::paint_scroll_fade(ui, scroll_output.inner_rect, 16.0);

                    if let Some(i) = scroll_output.inner.clicked {
                        self.selected = i;
                        self.activate();
                    }
                }

                target_height
            });

        panel.inner
    }
}

impl LayerApp for App {
    fn width(&self) -> u32 {
        self.max_size.0.round().max(1.0) as u32
    }

    fn init_height(&self) -> u32 {
        // Start at the input row's height; the first frame's auto-resize grows
        // it to fit the default result list.
        (common::input_size() + 16.0).round() as u32
    }

    fn on_frame_init(&mut self, ctx: &Context) {
        // Fonts/style are applied once for the process: the egui context now
        // persists across pop-ups (see LayerApp::on_frame_init), so re-applying
        // them per show would re-rasterize the font atlas needlessly.
        common::setup_transparent_style(ctx);
    }

    fn on_show(&mut self, ctx: &Context) {
        // Reload on focus only if the pre-focus first frame hasn't already
        // populated the list this pop-up. The window query (`hyprctl clients`,
        // a subprocess) is the dominant per-show cost, and it otherwise ran
        // twice — once from `update_ui`'s `!loaded` path before focus, once here
        // on focus. A window opening while the launcher is up still refreshes
        // through the `needs_reload` event path, so freshness is unaffected.
        if !self.loaded {
            self.load_entries(ctx);
        }
    }

    fn update_ui(&mut self, ctx: &Context) -> (f32, bool) {
        if self.needs_reload.swap(false, Ordering::SeqCst) {
            self.load_entries(ctx);
        }
        if !self.loaded {
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
    // Subscribe to Hyprland events once for the process lifetime so window-list
    // reloads and per-window focus-duration accounting keep running while the
    // launcher idles between pop-ups (the old daemon tracked focus the whole
    // time it was alive).
    app.setup_hyprland_events();
    layer::run("launcher", app);
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

