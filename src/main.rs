use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use iced::font::{Family, Weight};
use iced::keyboard::{key::Named, Key};
use iced::widget::{column, container, image, rich_text, row, scrollable, span, text_input, Column};
use iced::window;
use iced::{Element, Font, Length, Subscription, Task, Theme};
use serde::Deserialize;
use std::collections::HashMap;
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::process::Command;

const ICON_SIZE: u16 = 28;
const SOCKET_PATH: &str = "/tmp/launcher.sock";
const FONT: Font = Font {
    family: Family::Name("Roboto"),
    weight: Weight::Black,
    ..Font::DEFAULT
};

fn main() -> iced::Result {
    let window_settings = window::Settings {
        size: iced::Size::new(600.0, 50.0),
        decorations: false,
        resizable: true,
        platform_specific: window::settings::PlatformSpecific {
            application_id: "launcher".to_string(),
            ..Default::default()
        },
        ..Default::default()
    };

    iced::application("launcher", App::update, App::view)
        .subscription(App::subscription)
        .theme(|_| Theme::Dark)
        .default_font(FONT)
        .window(window_settings)
        .exit_on_close_request(false)
        .run_with(App::new)
}

#[derive(Clone, Debug)]
enum Entry {
    Desktop {
        name: String,
        exec: String,
        icon: Option<PathBuf>,
        keywords: Vec<String>,
    },
    Window {
        title: String,
        class: String,
        address: String,
        icon: Option<PathBuf>,
    },
}

impl Entry {
    fn name(&self) -> &str {
        match self {
            Entry::Desktop { name, .. } => name,
            Entry::Window { title, class, .. } => {
                if title.is_empty() { class } else { title }
            }
        }
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

    fn icon(&self) -> Option<&PathBuf> {
        match self {
            Entry::Desktop { icon, .. } => icon.as_ref(),
            Entry::Window { icon, .. } => icon.as_ref(),
        }
    }

    fn is_window(&self) -> bool {
        matches!(self, Entry::Window { .. })
    }
}

struct App {
    query: String,
    entries: Vec<Entry>,
    filtered: Vec<(usize, Vec<usize>)>, // (entry_idx, match_indices)
    selected: usize,
    matcher: SkimMatcherV2,
    visible: bool,
}

#[derive(Debug, Clone)]
enum Message {
    QueryChanged(String),
    Submit,
    SelectNext,
    SelectPrev,
    ClearQuery,
    DeleteWord,
    Hide,
    Toggle,
    Show,
    Reset,
}

fn ipc_subscription() -> Subscription<Message> {
    Subscription::run(|| {
        iced::stream::channel(100, |mut output| async move {
            use iced::futures::SinkExt;

            // Remove old socket
            let _ = std::fs::remove_file(SOCKET_PATH);

            let listener = match UnixListener::bind(SOCKET_PATH) {
                Ok(l) => l,
                Err(e) => {
                    eprintln!("Failed to bind socket: {}", e);
                    std::future::pending::<()>().await;
                    unreachable!()
                }
            };

            // Set non-blocking for async compatibility
            listener.set_nonblocking(true).ok();

            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        let reader = BufReader::new(&stream);
                        for line in reader.lines().flatten() {
                            let msg = match line.trim() {
                                "toggle" => Some(Message::Toggle),
                                "show" => Some(Message::Show),
                                "hide" => Some(Message::Hide),
                                _ => None,
                            };
                            if let Some(m) = msg {
                                let _ = output.send(m).await;
                            }
                        }
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                    Err(_) => {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                    }
                }
            }
        })
    })
}

impl App {
    fn new() -> (Self, Task<Message>) {
        let entries = collect_all_entries();
        let filtered: Vec<(usize, Vec<usize>)> = (0..entries.len()).map(|i| (i, vec![])).collect();

        // Start hidden in special workspace
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(200));
            let _ = Command::new("hyprctl")
                .args(["dispatch", "movetoworkspacesilent", "special:launcher,class:launcher"])
                .output();
        });

        (
            Self {
                query: String::new(),
                entries,
                filtered,
                selected: 0,
                matcher: SkimMatcherV2::default(),
                visible: false,
            },
            Task::none(),
        )
    }

    fn subscription(&self) -> Subscription<Message> {
        use iced::event::{self, Event};
        use iced::keyboard::Event as KeyEvent;
        use iced::window::Event as WindowEvent;

        // Listen to ALL events and filter
        let events = event::listen_with(|event, status, _window| {
            match &event {
                Event::Keyboard(KeyEvent::KeyPressed { key, modifiers, .. }) => {
                    match key {
                        Key::Named(Named::Escape) => Some(Message::Hide),
                        Key::Named(Named::ArrowDown) => Some(Message::SelectNext),
                        Key::Named(Named::ArrowUp) => Some(Message::SelectPrev),
                        Key::Named(Named::Enter) => Some(Message::Submit),
                        Key::Character(c) if modifiers.control() => {
                            match c.to_lowercase().as_str() {
                                "j" => Some(Message::SelectNext),
                                "k" => Some(Message::SelectPrev),
                                "n" => Some(Message::SelectNext),
                                "p" => Some(Message::SelectPrev),
                                "u" => Some(Message::ClearQuery),
                                "w" => Some(Message::DeleteWord),
                                _ => None,
                            }
                        }
                        _ => None,
                    }
                }
                Event::Window(WindowEvent::CloseRequested) => Some(Message::Hide),
                Event::Window(WindowEvent::Unfocused) => Some(Message::Reset),
                Event::Window(WindowEvent::Focused) => Some(Message::Show),
                _ => None,
            }
        });

        Subscription::batch([events, ipc_subscription()])
    }

    fn update(&mut self, message: Message) -> Task<Message> {
        match message {
            Message::Reset => {
                self.visible = false;
                self.query.clear();
                self.selected = 0;
                return Task::none();
            }
            Message::QueryChanged(query) => {
                self.query = query;
                self.filter();
                self.selected = 0;
                return self.resize_to_fit();
            }
            Message::Submit => {
                if let Some((idx, _)) = self.filtered.get(self.selected) {
                    if let Some(entry) = self.entries.get(*idx) {
                        // Close special workspace first so new app opens on main workspace
                        let _ = Command::new("hyprctl")
                            .args(["dispatch", "togglespecialworkspace", "launcher"])
                            .output();
                        self.visible = false;
                        activate(entry);
                    }
                }
            }
            Message::SelectNext => {
                let max_visible = 20.min(self.filtered.len());
                if self.selected < max_visible.saturating_sub(1) {
                    self.selected += 1;
                }
                return self.scroll_to_visible(true);
            }
            Message::SelectPrev => {
                self.selected = self.selected.saturating_sub(1);
                return self.scroll_to_visible(false);
            }
            Message::ClearQuery => {
                self.query.clear();
                self.filter();
                self.selected = 0;
                return self.resize_to_fit();
            }
            Message::DeleteWord => {
                // Delete last word (from end to previous space/start)
                let trimmed = self.query.trim_end();
                if let Some(pos) = trimmed.rfind(|c: char| c.is_whitespace()) {
                    self.query = trimmed[..=pos].to_string();
                } else {
                    self.query.clear();
                }
                self.filter();
                self.selected = 0;
                return self.resize_to_fit();
            }
            Message::Hide => {
                return self.hide();
            }
            Message::Show => {
                return self.show();
            }
            Message::Toggle => {
                if self.visible {
                    return self.hide();
                } else {
                    return self.show();
                }
            }
        }
        Task::none()
    }

    fn hide(&mut self) -> Task<Message> {
        self.visible = false;
        self.query.clear();
        self.selected = 0;
        let _ = Command::new("hyprctl")
            .args(["dispatch", "togglespecialworkspace", "launcher"])
            .output();
        Task::none()
    }

    fn show(&mut self) -> Task<Message> {
        self.visible = true;
        self.query.clear();
        self.entries = collect_all_entries();
        self.filter();
        self.selected = 0;
        Task::batch([
            text_input::focus(text_input::Id::new("search")),
            scrollable::scroll_to(scrollable::Id::new("results"), scrollable::AbsoluteOffset { x: 0.0, y: 0.0 }),
            self.resize_to_fit(),
        ])
    }

    fn resize_to_fit(&self) -> Task<Message> {
        let row_height = 40;
        let input_height = 50;
        let max_visible = 12;
        let num_results = self.filtered.len().min(max_visible);
        let height = input_height + (num_results * row_height);
        let _ = Command::new("hyprctl")
            .args(["dispatch", "resizewindowpixel", &format!("exact 600 {},class:launcher", height)])
            .output();
        Task::none()
    }

    fn view(&self) -> Element<'_, Message> {
        let input = text_input("Search...", &self.query)
            .id(text_input::Id::new("search"))
            .on_input(Message::QueryChanged)
            .padding(8)
            .size(18)
            .style(|_theme, _status| text_input::Style {
                background: iced::Background::Color(iced::Color::from_rgb(0.12, 0.12, 0.12)),
                border: iced::Border {
                    color: iced::Color::TRANSPARENT,
                    width: 0.0,
                    radius: 0.0.into(),
                },
                icon: iced::Color::from_rgb(0.6, 0.6, 0.6),
                placeholder: iced::Color::from_rgb(0.4, 0.4, 0.4),
                value: iced::Color::from_rgb(0.9, 0.9, 0.9),
                selection: iced::Color::from_rgba(0.3, 0.5, 0.8, 0.3),
            });

        let results: Column<Message> = self
            .filtered
            .iter()
            .take(20)
            .enumerate()
            .fold(Column::new().spacing(0), |col, (i, (idx, _))| {
                let entry = &self.entries[*idx];
                let selected = i == self.selected;

                let prefix = if entry.is_window() { "● " } else { "" };
                let name = entry.name();

                // Build highlighted text with spans
                let base_color = if selected {
                    iced::Color::from_rgb(0.9, 0.9, 0.9)
                } else {
                    iced::Color::from_rgb(0.6, 0.6, 0.6)
                };
                let highlight_color = iced::Color::from_rgb(1.0, 0.8, 0.2);

                // Re-match against name to get correct indices
                let match_indices: Vec<usize> = if !self.query.is_empty() {
                    self.matcher.fuzzy_indices(name, &self.query)
                        .map(|(_, indices)| indices)
                        .unwrap_or_default()
                } else {
                    vec![]
                };

                let mut spans = vec![span(prefix).color(base_color)];
                let chars: Vec<char> = name.chars().collect();
                let mut last_end = 0;

                for match_idx in match_indices {
                    if match_idx > last_end {
                        let s: String = chars[last_end..match_idx].iter().collect();
                        spans.push(span(s).color(base_color));
                    }
                    if match_idx < chars.len() {
                        spans.push(span(chars[match_idx].to_string()).color(highlight_color).font(iced::Font { weight: Weight::Bold, ..FONT }));
                        last_end = match_idx + 1;
                    }
                }
                if last_end < chars.len() {
                    let s: String = chars[last_end..].iter().collect();
                    spans.push(span(s).color(base_color));
                }

                let label = rich_text(spans).size(18);

                // Always use row with icon placeholder for alignment
                let icon_element: Element<Message> = if let Some(icon_path) = entry.icon() {
                    image(icon_path.clone())
                        .width(ICON_SIZE)
                        .height(ICON_SIZE)
                        .into()
                } else {
                    iced::widget::Space::new(ICON_SIZE, ICON_SIZE).into()
                };

                let content: Element<Message> = row![icon_element, label]
                    .spacing(8)
                    .align_y(iced::Alignment::Center)
                    .into();

                col.push(
                    container(content)
                        .padding(6)
                        .width(Length::Fill)
                        .style(if selected {
                            container::dark
                        } else {
                            container::transparent
                        }),
                )
            });

        let scroll_area = scrollable(results)
            .id(scrollable::Id::new("results"))
            .height(Length::Shrink)
            .style(|_theme, _status| scrollable::Style {
                container: container::Style::default(),
                vertical_rail: scrollable::Rail {
                    background: None,
                    border: iced::Border::default(),
                    scroller: scrollable::Scroller {
                        color: iced::Color::TRANSPARENT,
                        border: iced::Border::default(),
                    },
                },
                horizontal_rail: scrollable::Rail {
                    background: None,
                    border: iced::Border::default(),
                    scroller: scrollable::Scroller {
                        color: iced::Color::TRANSPARENT,
                        border: iced::Border::default(),
                    },
                },
                gap: None,
            });

        let content: Element<Message> = if self.filtered.is_empty() {
            column![input].spacing(0).into()
        } else {
            column![input, scroll_area].spacing(0).into()
        };

        container(content)
            .width(600)
            .into()
    }

    fn scroll_to_visible(&self, going_down: bool) -> Task<Message> {
        let row_height = 40.0;
        let visible_rows = 8;

        let offset = if going_down {
            if self.selected >= visible_rows {
                let top = self.selected - visible_rows + 1;
                top as f32 * row_height
            } else {
                return Task::none();
            }
        } else {
            self.selected as f32 * row_height
        };

        scrollable::scroll_to(
            scrollable::Id::new("results"),
            scrollable::AbsoluteOffset { x: 0.0, y: offset },
        )
    }

    fn filter(&mut self) {
        if self.query.is_empty() {
            self.filtered = (0..self.entries.len()).map(|i| (i, vec![])).collect();
        } else {
            let mut scored: Vec<_> = self
                .entries
                .iter()
                .enumerate()
                .filter_map(|(idx, e)| {
                    // Get best match with indices
                    let best = e
                        .searchable()
                        .iter()
                        .filter_map(|s| self.matcher.fuzzy_indices(s, &self.query))
                        .max_by_key(|(score, _)| *score)?;
                    Some((best.0, idx, best.1))
                })
                .collect();
            scored.sort_by(|a, b| b.0.cmp(&a.0));
            self.filtered = scored.into_iter().map(|(_, idx, indices)| (idx, indices)).collect();
        }
    }
}

fn activate(entry: &Entry) {
    match entry {
        Entry::Desktop { exec, .. } => {
            let cmd: String = exec
                .split_whitespace()
                .filter(|s| !s.starts_with('%'))
                .collect::<Vec<_>>()
                .join(" ");
            let _ = Command::new("setsid")
                .args(["-f", "sh", "-c", &cmd])
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn();
        }
        Entry::Window { address, .. } => {
            let _ = Command::new("hyprctl")
                .args(["dispatch", "focuswindow", &format!("address:{}", address)])
                .output();
        }
    }
}

fn collect_all_entries() -> Vec<Entry> {
    let icon_index = build_icon_index();
    let mut entries = collect_hyprland_windows(&icon_index);
    entries.extend(collect_desktop_entries(&icon_index));
    entries
}

#[derive(Deserialize)]
struct HyprClient {
    address: String,
    title: String,
    class: String,
    #[serde(rename = "focusHistoryID")]
    focus_history_id: i32,
}

fn collect_hyprland_windows(icon_index: &HashMap<String, PathBuf>) -> Vec<Entry> {
    let output = Command::new("hyprctl")
        .args(["clients", "-j"])
        .output()
        .ok();

    let Some(output) = output else { return vec![] };
    if !output.status.success() { return vec![]; }

    let mut clients: Vec<HyprClient> = serde_json::from_slice(&output.stdout).unwrap_or_default();

    // Sort by focus recency (lower focusHistoryID = more recent)
    clients.sort_by_key(|c| c.focus_history_id);

    clients
        .into_iter()
        .filter(|c| !c.class.is_empty() && c.class != "launcher")
        .map(|c| {
            let icon = icon_index.get(&c.class.to_lowercase()).cloned();
            Entry::Window {
                title: c.title,
                class: c.class,
                address: c.address,
                icon,
            }
        })
        .collect()
}

fn collect_desktop_entries(icon_index: &HashMap<String, PathBuf>) -> Vec<Entry> {
    let mut seen_files: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut entries = Vec::new();

    for dir in get_applications_dirs() {
        if let Ok(read_dir) = fs::read_dir(&dir) {
            for entry in read_dir.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "desktop") {
                    let key = path.file_name().unwrap().to_string_lossy().to_string();
                    if seen_files.insert(key) {
                        entries.extend(parse_desktop_file(&path, icon_index));
                    }
                }
            }
        }
    }

    entries.sort_by(|a, b| a.name().to_lowercase().cmp(&b.name().to_lowercase()));
    entries
}

fn get_applications_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if let Ok(home) = env::var("HOME") {
        dirs.push(PathBuf::from(home).join(".local/share/applications"));
    }

    let data_dirs = env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    for dir in data_dirs.split(':') {
        dirs.push(PathBuf::from(dir).join("applications"));
    }

    dirs.push(PathBuf::from("/var/lib/flatpak/exports/share/applications"));

    dirs
}

fn get_icon_dirs() -> Vec<PathBuf> {
    let mut dirs = Vec::new();

    if let Ok(home) = env::var("HOME") {
        dirs.push(PathBuf::from(&home).join(".local/share/icons"));
        dirs.push(PathBuf::from(&home).join(".icons"));
    }

    let data_dirs = env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    for dir in data_dirs.split(':') {
        dirs.push(PathBuf::from(dir).join("icons"));
    }

    dirs.push(PathBuf::from("/usr/share/pixmaps"));

    dirs
}

fn build_icon_index() -> HashMap<String, PathBuf> {
    let mut index: HashMap<String, PathBuf> = HashMap::new();

    let sizes = ["256x256", "128x128", "64x64", "48x48", "32x32", "24x24", "scalable"];
    let categories = ["apps", "applications"];

    for base_dir in get_icon_dirs() {
        let hicolor = base_dir.join("hicolor");
        for size in &sizes {
            for category in &categories {
                let dir = hicolor.join(size).join(category);
                if let Ok(entries) = fs::read_dir(&dir) {
                    for entry in entries.flatten() {
                        let path = entry.path();
                        if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                            index.entry(stem.to_string()).or_insert(path);
                        }
                    }
                }
            }
        }

        if let Ok(entries) = fs::read_dir(&base_dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_file() {
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        index.entry(stem.to_string()).or_insert(path);
                    }
                }
            }
        }
    }

    index
}

fn parse_desktop_file(path: &PathBuf, icon_index: &HashMap<String, PathBuf>) -> Vec<Entry> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let mut entries = Vec::new();
    let mut main_name = None;
    let mut main_exec = None;
    let mut main_icon_name = None;
    let mut no_display = false;
    let mut hidden = false;
    let mut keywords = Vec::new();
    let mut actions_list: Vec<String> = Vec::new();

    // Action data: action_id -> (name, exec)
    let mut actions: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    let mut current_section = String::new();
    let mut current_action_id: Option<String> = None;

    for line in content.lines() {
        let line = line.trim();

        if line.starts_with('[') && line.ends_with(']') {
            current_section = line[1..line.len()-1].to_string();
            if current_section.starts_with("Desktop Action ") {
                current_action_id = Some(current_section["Desktop Action ".len()..].to_string());
            } else {
                current_action_id = None;
            }
            continue;
        }

        if let Some((key, value)) = line.split_once('=') {
            if current_section == "Desktop Entry" {
                match key {
                    "Name" if main_name.is_none() => main_name = Some(value.to_string()),
                    "Exec" => main_exec = Some(value.to_string()),
                    "Icon" => main_icon_name = Some(value.to_string()),
                    "NoDisplay" => no_display = value == "true",
                    "Hidden" => hidden = value == "true",
                    "Keywords" => {
                        keywords = value.split(';').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect();
                    }
                    "Actions" => {
                        actions_list = value.split(';').filter(|s| !s.is_empty()).map(|s| s.to_string()).collect();
                    }
                    _ => {}
                }
            } else if let Some(ref action_id) = current_action_id {
                let entry = actions.entry(action_id.clone()).or_insert((None, None));
                match key {
                    "Name" => entry.0 = Some(value.to_string()),
                    "Exec" => entry.1 = Some(value.to_string()),
                    _ => {}
                }
            }
        }
    }

    if no_display || hidden {
        return vec![];
    }

    let icon = main_icon_name.as_ref().and_then(|name| {
        let path = PathBuf::from(name);
        if path.is_absolute() && path.exists() {
            return Some(path);
        }
        icon_index.get(name).cloned()
    });

    // Add main entry
    if let (Some(name), Some(exec)) = (main_name.clone(), main_exec) {
        entries.push(Entry::Desktop {
            name,
            exec,
            icon: icon.clone(),
            keywords: keywords.clone(),
        });
    }

    // Add action entries
    for action_id in actions_list {
        if let Some((Some(action_name), Some(action_exec))) = actions.get(&action_id) {
            let display_name = if let Some(ref app_name) = main_name {
                format!("{}: {}", app_name, action_name)
            } else {
                action_name.clone()
            };
            entries.push(Entry::Desktop {
                name: display_name,
                exec: action_exec.clone(),
                icon: icon.clone(),
                keywords: vec![],
            });
        }
    }

    entries
}
