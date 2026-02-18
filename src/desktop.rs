//! Desktop file parsing and icon system

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::{env, fs};

/// Icon reference — either an absolute path or a theme name to look up
#[derive(Clone)]
pub enum Icon {
    Path(PathBuf),
    Name(String),
}

impl Icon {
    fn from_value(s: &str) -> Icon {
        let p = PathBuf::from(s);
        if p.is_absolute() {
            Icon::Path(p)
        } else {
            Icon::Name(s.to_string())
        }
    }

    /// Resolve to a filesystem path using the icon index for theme names
    pub fn resolve(&self, icon_index: &HashMap<String, PathBuf>) -> Option<PathBuf> {
        match self {
            Icon::Path(p) if p.exists() => Some(p.clone()),
            Icon::Name(name) => icon_index.get(&name.to_lowercase()).cloned(),
            _ => None,
        }
    }
}

pub struct DesktopEntry {
    pub name: String,
    pub generic_name: Option<String>,
    pub exec: String,
    pub terminal: bool,
    pub icon: Option<Icon>,
    pub keywords: Vec<String>,
    pub wm_class: Option<String>,
}

pub fn applications_dirs() -> Vec<PathBuf> {
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

pub fn parse_desktop_file(path: &Path) -> Vec<DesktopEntry> {
    let content = match fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return vec![],
    };

    let mut entries = Vec::new();
    let mut main_name = None;
    let mut main_generic_name = None;
    let mut main_exec = None;
    let mut main_icon = None;
    let mut main_keywords: Vec<String> = Vec::new();
    let mut main_wm_class = None;
    let mut no_display = false;
    let mut hidden = false;
    let mut terminal = false;
    let mut actions_list: Vec<String> = Vec::new();
    let mut actions: HashMap<String, (Option<String>, Option<String>)> = HashMap::new();
    let mut current_section = String::new();
    let mut current_action_id: Option<String> = None;

    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }

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
                    "GenericName" if main_generic_name.is_none() => main_generic_name = Some(value.to_string()),
                    "Exec" => main_exec = Some(value.to_string()),
                    "Icon" => main_icon = Some(Icon::from_value(value)),
                    "NoDisplay" => no_display = value == "true",
                    "Hidden" => hidden = value == "true",
                    "Terminal" => terminal = value == "true",
                    "StartupWMClass" => main_wm_class = Some(value.to_string()),
                    "Keywords" if main_keywords.is_empty() => {
                        main_keywords = value.split(';')
                            .filter(|s| !s.is_empty())
                            .map(|s| s.to_string())
                            .collect();
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

    if let (Some(name), Some(exec)) = (main_name.clone(), main_exec.clone()) {
        entries.push(DesktopEntry {
            name,
            generic_name: main_generic_name.clone(),
            exec,
            terminal,
            icon: main_icon.clone(),
            keywords: main_keywords.clone(),
            wm_class: main_wm_class.clone(),
        });
    }

    for action_id in actions_list {
        if let Some((Some(action_name), Some(action_exec))) = actions.get(&action_id) {
            let display_name = if let Some(ref app_name) = main_name {
                format!("{}: {}", app_name, action_name)
            } else {
                action_name.clone()
            };
            // Actions share parent icon but not search metadata
            entries.push(DesktopEntry {
                name: display_name,
                generic_name: None,
                exec: action_exec.clone(),
                terminal,
                icon: main_icon.clone(),
                keywords: vec![],
                wm_class: None,
            });
        }
    }

    entries
}

/// Collect all desktop entries from XDG dirs, deduplicated by filename
pub fn collect_entries() -> Vec<DesktopEntry> {
    let mut seen_files = std::collections::HashSet::new();
    let mut seen_names = std::collections::HashSet::new();
    let mut entries = Vec::new();

    for dir in applications_dirs() {
        if let Ok(read_dir) = fs::read_dir(&dir) {
            for entry in read_dir.flatten() {
                let path = entry.path();
                if path.extension().is_some_and(|e| e == "desktop") {
                    let key = path.file_name().unwrap().to_string_lossy().to_string();
                    if seen_files.insert(key) {
                        for de in parse_desktop_file(&path) {
                            if seen_names.insert(de.name.to_lowercase()) {
                                entries.push(de);
                            }
                        }
                    }
                }
            }
        }
    }

    entries.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    entries
}

/// Build icon index from system icon theme directories
pub fn build_icon_index() -> HashMap<String, PathBuf> {
    let mut index = HashMap::new();
    let sizes = ["48x48", "64x64", "128x128", "256x256", "512x512", "32x32", "scalable"];

    let mut dirs = vec![
        PathBuf::from("/run/current-system/sw/share/icons"),
        PathBuf::from("/usr/share/icons"),
    ];
    if let Ok(home) = env::var("HOME") {
        dirs.insert(0, PathBuf::from(&home).join(".local/share/icons"));
        dirs.insert(0, PathBuf::from(&home).join(".icons"));
    }
    if let Ok(xdg) = env::var("XDG_DATA_DIRS") {
        for d in xdg.split(':') {
            dirs.push(PathBuf::from(d).join("icons"));
        }
    }

    // Read GTK icon theme name from settings
    let gtk_theme = fs::read_to_string(
        env::var("HOME").unwrap_or_default() + "/.config/gtk-3.0/settings.ini"
    ).ok().and_then(|content| {
        content.lines()
            .find(|l| l.starts_with("gtk-icon-theme-name="))
            .map(|l| l.trim_start_matches("gtk-icon-theme-name=").to_string())
    });

    // Search active theme first (higher priority), then hicolor as fallback
    let themes: Vec<&str> = if let Some(ref t) = gtk_theme {
        vec![t.as_str(), "hicolor"]
    } else {
        vec!["hicolor"]
    };

    for theme in &themes {
        for base in &dirs {
            let theme_dir = base.join(theme);
            for size in &sizes {
                for cat in ["apps", "applications"] {
                    let dir = theme_dir.join(size).join(cat);
                    if let Ok(entries) = fs::read_dir(&dir) {
                        for e in entries.flatten() {
                            let path = e.path();
                            if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                                index.entry(stem.to_lowercase()).or_insert(path);
                            }
                        }
                    }
                }
            }
        }
    }

    for pixmaps in ["/run/current-system/sw/share/pixmaps", "/usr/share/pixmaps"] {
        if let Ok(entries) = fs::read_dir(pixmaps) {
            for e in entries.flatten() {
                let path = e.path();
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    index.entry(stem.to_lowercase()).or_insert(path);
                }
            }
        }
    }
    index
}

/// Convert SVG icons to cached PNGs via ImageMagick.
/// Updates the index in-place to point to the cached PNG paths.
pub fn cache_svgs(index: &mut HashMap<String, PathBuf>) {
    let cache_dir = svg_cache_dir();
    let _ = fs::create_dir_all(&cache_dir);

    for (_name, path) in index.iter_mut() {
        if path.extension().is_some_and(|e| e == "svg") {
            let cached = cache_dir.join(
                path.file_stem().unwrap_or_default()
            ).with_extension("png");

            if cached.exists() {
                *path = cached;
                continue;
            }

            let output = format!("png32:{}", cached.display());
            let ok = std::process::Command::new("magick")
                .args([
                    std::ffi::OsStr::new("-background"),
                    std::ffi::OsStr::new("none"),
                    path.as_os_str(),
                    std::ffi::OsStr::new("-resize"),
                    std::ffi::OsStr::new("128x128"),
                    std::ffi::OsStr::new(&output),
                ])
                .status()
                .is_ok_and(|s| s.success());

            if ok {
                *path = cached;
            }
        }
    }
}

fn svg_cache_dir() -> PathBuf {
    let runtime = env::var("XDG_RUNTIME_DIR").unwrap_or("/tmp".into());
    PathBuf::from(runtime).join("launcher-svg-cache")
}

/// Build WMClass to icon path mapping from already-parsed desktop entries
pub fn wmclass_icon_map(entries: &[DesktopEntry], icon_index: &HashMap<String, PathBuf>) -> HashMap<String, PathBuf> {
    let mut map = HashMap::new();
    for de in entries {
        if let (Some(wm), Some(icon)) = (&de.wm_class, &de.icon) {
            if let Some(path) = icon.resolve(icon_index) {
                map.entry(wm.to_lowercase()).or_insert(path);
            }
        }
    }
    map
}
