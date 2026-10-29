//! Local UI preferences (colors, borders, symbols, refresh rate, mouse).
//! Stored in <AIWS_HOME>/ui.toml, separate from the shared daemon config, so
//! changing how *your* terminal looks never touches a remote server.

use anyhow::Result;
use ratatui::style::Color;
use ratatui::symbols;
use ratatui::widgets::{Block, BorderType};
use serde::{Deserialize, Serialize};
use std::sync::RwLock;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
#[serde(default)]
pub struct UiPrefs {
    pub theme: String,
    pub accent: String,
    pub user_color: String,
    pub ai_color: String,
    pub border: String,  // plain | rounded | double | thick
    pub symbols: String, // unicode | ascii
    pub refresh_ms: u64,
    pub show_hints: bool,
    pub mouse: bool,
}

impl Default for UiPrefs {
    fn default() -> Self {
        let mut p = Self {
            theme: "default".into(),
            accent: String::new(),
            user_color: String::new(),
            ai_color: String::new(),
            border: "plain".into(),
            symbols: "unicode".into(),
            refresh_ms: 1500,
            show_hints: true,
            mouse: true,
        };
        apply_preset(&mut p, "default");
        p
    }
}

/// (name, accent, you, ai)
pub const PRESETS: &[(&str, &str, &str, &str)] = &[
    ("default", "cyan", "green", "cyan"),
    ("ocean", "lightblue", "lightcyan", "blue"),
    ("forest", "green", "lightgreen", "lightyellow"),
    ("sunset", "yellow", "lightred", "magenta"),
    ("mono", "white", "gray", "white"),
    ("contrast", "lightyellow", "lightgreen", "lightcyan"),
];

pub const COLORS: &[&str] = &[
    "cyan", "lightcyan", "blue", "lightblue", "green", "lightgreen", "yellow", "lightyellow", "red", "lightred", "magenta",
    "lightmagenta", "white", "gray",
];

pub const BORDERS: &[&str] = &["plain", "rounded", "double", "thick"];
pub const SYMBOLS: &[&str] = &["unicode", "ascii"];
pub const REFRESH: &[u64] = &[500, 1000, 1500, 3000, 5000];

pub fn apply_preset(p: &mut UiPrefs, name: &str) {
    if let Some((n, a, u, ai)) = PRESETS.iter().find(|x| x.0 == name) {
        p.theme = (*n).into();
        p.accent = (*a).into();
        p.user_color = (*u).into();
        p.ai_color = (*ai).into();
    }
}

pub fn load() -> UiPrefs {
    let path = crate::config::home_dir().join("ui.toml");
    std::fs::read_to_string(path).ok().and_then(|t| toml::from_str(&t).ok()).unwrap_or_default()
}

pub fn save(p: &UiPrefs) -> Result<()> {
    let dir = crate::config::home_dir();
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("ui.toml"), toml::to_string_pretty(p)?)?;
    Ok(())
}

/// Parse a color name or #rrggbb; fall back to `fallback` for anything invalid.
pub fn color(s: &str, fallback: Color) -> Color {
    s.trim().parse::<Color>().unwrap_or(fallback)
}

pub fn valid_color(s: &str) -> bool {
    s.trim().parse::<Color>().is_ok()
}

#[derive(Clone, Copy)]
pub struct Theme {
    pub accent: Color,
    pub user: Color,
    pub ai: Color,
    pub border: BorderType,
    pub ascii: bool,
    pub hints: bool,
}

impl Theme {
    pub fn from(p: &UiPrefs) -> Self {
        Self {
            accent: color(&p.accent, Color::Cyan),
            user: color(&p.user_color, Color::Green),
            ai: color(&p.ai_color, Color::Cyan),
            border: match p.border.as_str() {
                "rounded" => BorderType::Rounded,
                "double" => BorderType::Double,
                "thick" => BorderType::Thick,
                _ => BorderType::Plain,
            },
            ascii: p.symbols == "ascii",
            hints: p.show_hints,
        }
    }
    pub fn pointer(&self) -> &'static str {
        if self.ascii { "> " } else { "▶ " }
    }
    pub fn cursor(&self) -> &'static str {
        if self.ascii { "_" } else { "█" }
    }
    pub fn dot(&self) -> &'static str {
        if self.ascii { "*" } else { "●" }
    }
}

static THEME: RwLock<Option<Theme>> = RwLock::new(None);

pub fn set(p: &UiPrefs) {
    *THEME.write().unwrap() = Some(Theme::from(p));
}

pub fn theme() -> Theme {
    THEME.read().unwrap().unwrap_or_else(|| Theme::from(&UiPrefs::default()))
}

/// A bordered block that follows the user's border/symbol preferences.
pub fn bordered<'a>() -> Block<'a> {
    let t = theme();
    if t.ascii {
        Block::bordered().border_set(symbols::border::Set {
            top_left: "+",
            top_right: "+",
            bottom_left: "+",
            bottom_right: "+",
            vertical_left: "|",
            vertical_right: "|",
            horizontal_top: "-",
            horizontal_bottom: "-",
        })
    } else {
        Block::bordered().border_type(t.border)
    }
}
