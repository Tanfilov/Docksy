use std::{path::PathBuf, time::SystemTime};

use serde::Deserialize;

use crate::util::data_dir;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Position {
    Bottom,
    Top,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backdrop {
    None,
    Acrylic,
    Mica,
}

/// All sizes are in logical pixels (100% scale); they are multiplied by the monitor scale.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default)]
pub struct Config {
    pub height: f32,
    pub icon_size: f32,
    pub tray_icon_size: f32,
    pub item_spacing: f32,
    pub position: Position,
    pub center_apps: bool,
    pub show_start_button: bool,
    pub show_desktop_button: bool,
    pub show_quick_settings: bool,
    pub tray_on_all_monitors: bool,
    pub hide_windows_taskbar: bool,

    pub font_size: f32,
    pub time_format: String,
    pub date_format: String,
    pub show_date: bool,

    pub backdrop: Backdrop,
    pub background: String,
    pub hover: String,
    pub accent: String,
    pub attention: String,
    pub text: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            height: 64.0,
            icon_size: 40.0,
            tray_icon_size: 22.0,
            item_spacing: 4.0,
            position: Position::Bottom,
            center_apps: true,
            show_start_button: true,
            show_desktop_button: true,
            show_quick_settings: true,
            tray_on_all_monitors: false,
            hide_windows_taskbar: true,
            font_size: 14.0,
            time_format: String::new(),
            date_format: String::new(),
            show_date: true,
            backdrop: Backdrop::Acrylic,
            background: "#202020C0".into(),
            hover: "#FFFFFF1E".into(),
            accent: "#60CDFF".into(),
            attention: "#F7A33A".into(),
            text: "#FFFFFF".into(),
        }
    }
}

const DEFAULT_FILE: &str = r##"# Seelena taskbar settings. Saved changes apply automatically.
# Sizes are in logical pixels (scaled by each monitor's display scale).

height = 64              # taskbar thickness (Windows default is 48)
icon_size = 40           # app icon size (Windows default is 24)
tray_icon_size = 22      # tray icon size
item_spacing = 4         # gap between app buttons
position = "bottom"      # "bottom" or "top"
center_apps = true       # false = apps start at the left edge
show_start_button = true
show_desktop_button = true   # thin strip at the far end, click = show desktop
show_quick_settings = true   # network/volume/battery button, click = Win+A
tray_on_all_monitors = false # tray icons on every monitor, not only the primary
hide_windows_taskbar = true

font_size = 14
time_format = ""         # empty = Windows format; e.g. "HH:mm" or "h:mm tt"
date_format = ""         # empty = Windows short date; e.g. "ddd d MMM"
show_date = true

backdrop = "acrylic"     # "acrylic", "mica" or "none"
background = "#202020C0" # #RRGGBB or #RRGGBBAA
hover = "#FFFFFF1E"
accent = "#60CDFF"
attention = "#F7A33A"    # app asking for attention (flashing)
text = "#FFFFFF"
"##;

pub fn path() -> PathBuf {
    data_dir().join("config.toml")
}

pub fn mtime() -> Option<SystemTime> {
    std::fs::metadata(path()).and_then(|m| m.modified()).ok()
}

pub fn load() -> Config {
    let p = path();
    if !p.exists() {
        let _ = std::fs::write(&p, DEFAULT_FILE);
    }
    match std::fs::read_to_string(&p) {
        Ok(text) => match toml::from_str::<Config>(&text) {
            Ok(cfg) => sanitize(cfg),
            Err(e) => {
                crate::log!("config error: {e}");
                Config::default()
            }
        },
        Err(_) => Config::default(),
    }
}

fn sanitize(mut c: Config) -> Config {
    c.height = c.height.clamp(24.0, 256.0);
    c.icon_size = c.icon_size.clamp(12.0, c.height);
    c.tray_icon_size = c.tray_icon_size.clamp(12.0, c.height);
    c.item_spacing = c.item_spacing.clamp(0.0, 64.0);
    c.font_size = c.font_size.clamp(8.0, 48.0);
    c
}
