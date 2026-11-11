use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

const CONFIG_PATH: &str = "/etc/flight-clock/config.json";

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

impl Default for Config {
    fn default() -> Self {
        Self {
            latitude: default_latitude(),
            longitude: default_longitude(),
            radius_nm: default_radius_nm(),
            facing_direction: default_facing_direction(),
            clock_face: default_clock_face(),
            show_date: default_show_date(),
        }
    }
}

impl Config {
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
