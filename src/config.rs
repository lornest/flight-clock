use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

const CONFIG_PATH: &str = "/etc/flight-clock/config.json";

/// Backgrounds: (config id, name, ground, surface, date panel). The first is the default.
pub const BACKGROUNDS: [(&str, &str, &str, &str, &str); 2] = [
    ("aviator-blue", "Aviator blue", "#0B1118", "#121A23", "#141D28"),
    ("black", "Black", "#000000", "#101214", "#15181B"),
];

/// Accent colours from the design: (config id, name, hex), in the design's order.
pub const DEFAULT_ACCENT: &str = "coral";
pub const ACCENTS: [(&str, &str, &str); 5] = [
    ("amber", "Amber", "#FFB547"),
    ("powder-blue", "Powder blue", "#7FD3D8"),
    ("coral", "Coral", "#FF7F6B"),
    ("lime", "Lime", "#B4E06A"),
    ("lilac", "Lilac", "#B9A2FF"),
];

/// Config file path: $FLIGHT_CLOCK_CONFIG if set (e.g. for local testing),
/// else /etc/flight-clock/config.json.
pub fn config_path() -> String {
    std::env::var("FLIGHT_CLOCK_CONFIG").unwrap_or_else(|_| CONFIG_PATH.to_string())
}

/// Observer position and radar range. Shared between the poller and the UI
/// so the web settings page can change it without a restart.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Location {
    pub lat: f64,
    pub lon: f64,
    pub max_radius_nm: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default = "default_latitude")]
    pub latitude: f64,
    #[serde(default = "default_longitude")]
    pub longitude: f64,
    #[serde(default = "default_radius_nm")]
    pub radius_nm: f64,
    #[serde(default = "default_facing_direction")]
    pub facing_direction: String,
    /// "analog" or "digital"; switched by holding the FlightClock logo
    #[serde(default = "default_clock_face")]
    pub clock_face: String,
    /// Date panel on the clock faces; toggled from the settings screen
    #[serde(default = "default_show_date")]
    pub show_date: bool,
    /// "stable" or "beta" (beta also gets pre-releases)
    #[serde(default = "default_update_channel")]
    pub update_channel: String,
    /// Install updates automatically (daily). Off: only check.
    #[serde(default = "default_auto_update")]
    pub auto_update: bool,
    /// Accent colour id from `ACCENTS`
    #[serde(default = "default_accent")]
    pub accent: String,
    /// Background id from `BACKGROUNDS`
    #[serde(default = "default_background")]
    pub background: String,
}

fn default_latitude() -> f64 {
    55.979636
}
fn default_longitude() -> f64 {
    -3.577456
}
fn default_radius_nm() -> f64 {
    6.0
}
fn default_facing_direction() -> String {
    "N".to_string()
}
fn default_clock_face() -> String {
    "analog".to_string()
}
fn default_show_date() -> bool {
    true
}
fn default_update_channel() -> String {
    "stable".to_string()
}
fn default_auto_update() -> bool {
    true
}
fn default_accent() -> String {
    DEFAULT_ACCENT.to_string()
}
fn default_background() -> String {
    BACKGROUNDS[0].0.to_string()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            latitude: default_latitude(),
            longitude: default_longitude(),
            radius_nm: default_radius_nm(),
            facing_direction: default_facing_direction(),
            clock_face: default_clock_face(),
            show_date: default_show_date(),
            update_channel: default_update_channel(),
            auto_update: default_auto_update(),
            accent: default_accent(),
            background: default_background(),
        }
    }
}

impl Config {
    /// The accent colour as "#RRGGBB"; the default (coral) if the id is unknown.
    pub fn accent_hex(&self) -> &'static str {
        let find = |want: &str| ACCENTS.iter().find(|(id, _, _)| *id == want).map(|(_, _, hex)| *hex);
        find(&self.accent).or_else(|| find(DEFAULT_ACCENT)).unwrap_or(ACCENTS[0].2)
    }

    /// Background colours as "#RRGGBB": (ground, surface, date panel).
    /// Aviator blue if the id is unknown.
    pub fn background_hex(&self) -> (&'static str, &'static str, &'static str) {
        let b = BACKGROUNDS
            .iter()
            .find(|(id, ..)| *id == self.background)
            .unwrap_or(&BACKGROUNDS[0]);
        (b.2, b.3, b.4)
    }

    pub fn location(&self) -> Location {
        Location {
            lat: self.latitude,
            lon: self.longitude,
            max_radius_nm: self.radius_nm,
        }
    }

    pub fn load(path: Option<&str>) -> Self {
        let default_path = config_path();
        let config_path = path.unwrap_or(&default_path);
        let p = Path::new(config_path);

        if !p.exists() {
            eprintln!("Config file not found at {}, using defaults", config_path);
            return Self::default();
        }

        match fs::read_to_string(p) {
            Ok(contents) => match serde_json::from_str(&contents) {
                Ok(config) => config,
                Err(e) => {
                    eprintln!("Failed to parse config: {}, using defaults", e);
                    Self::default()
                }
            },
            Err(e) => {
                eprintln!("Failed to read config file: {}, using defaults", e);
                Self::default()
            }
        }
    }

    pub fn save(&self, path: Option<&str>) -> Result<(), String> {
        let default_path = config_path();
        let config_path = path.unwrap_or(&default_path);
        let p = Path::new(config_path);

        // Ensure parent directory exists
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create config directory: {}", e))?;
        }

        let json = serde_json::to_string_pretty(self)
            .map_err(|e| format!("Failed to serialize config: {}", e))?;

        fs::write(p, json).map_err(|e| format!("Failed to write config: {}", e))
    }
}

#[cfg(test)]
mod accent_tests {
    use super::*;

    #[test]
    fn defaults_to_coral() {
        assert_eq!(Config::default().accent_hex(), "#FF7F6B");
    }

    #[test]
    fn unknown_accent_falls_back_to_coral() {
        let cfg = Config { accent: "magenta".into(), ..Config::default() };
        assert_eq!(cfg.accent_hex(), "#FF7F6B");
    }

    #[test]
    fn amber_still_selectable() {
        let cfg = Config { accent: "amber".into(), ..Config::default() };
        assert_eq!(cfg.accent_hex(), "#FFB547");
    }

    #[test]
    fn old_config_without_colours_loads() {
        let cfg: Config = serde_json::from_str(r#"{"latitude":55.9,"longitude":-3.2}"#).unwrap();
        assert_eq!(cfg.accent, "coral");
        assert_eq!(cfg.background, "aviator-blue");
    }

    #[test]
    fn background_lookup() {
        assert_eq!(Config::default().background_hex().0, "#0B1118");
        let black = Config { background: "black".into(), ..Config::default() };
        assert_eq!(black.background_hex().0, "#000000");
        let unknown = Config { background: "white".into(), ..Config::default() };
        assert_eq!(unknown.background_hex().0, "#0B1118");
    }
}
