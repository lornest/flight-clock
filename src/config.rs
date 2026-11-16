use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

const CONFIG_PATH: &str = "/etc/flight-clock/config.json";

pub const FACINGS: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
pub const RADIUS_MIN_NM: f64 = 1.0;
pub const RADIUS_MAX_NM: f64 = 25.0;

/// Clock faces, in the order swiping up on the clock goes through them. The
/// first is the default. A new face is added here and in ui/main.slint; the
/// settings page doesn't list them.
pub const CLOCK_FACES: [&str; 2] = ["analog", "digital"];

/// The face after `current` (`step` 1) or before it (-1), going round. An
/// unknown id counts as the first face.
pub fn next_clock_face(current: &str, step: i32) -> &'static str {
    let count = CLOCK_FACES.len() as i32;
    let at = CLOCK_FACES.iter().position(|f| *f == current).unwrap_or(0) as i32;
    CLOCK_FACES[(at + step).rem_euclid(count) as usize]
}

/// Night modes: (config id, name). "dim" darkens the picture; "screen_off"
/// also switches the backlight off (the display's backlight is on/off only).
pub const NIGHT_MODES: [(&str, &str); 3] = [("off", "Off"), ("dim", "Dim"), ("screen_off", "Screen off")];
/// Choices for the shortest time between alerts, in minutes (0: no limit).
pub const ALERT_GAPS_MIN: [u32; 6] = [0, 1, 2, 5, 10, 30];
pub const ALERT_ALTITUDE_MIN_FT: u32 = 500;
pub const ALERT_ALTITUDE_MAX_FT: u32 = 60_000;
pub const ALTITUDE_ORDER_ERROR: &str = "The lowest alert altitude must be below the highest.";

/// Does an aircraft at `altitude_ft` (None: on the ground or not reported)
/// pass the alert altitude limits? Without a lower limit it counts as low,
/// so it passes; with one it can't be shown to be above it, so it doesn't.
pub fn altitude_alerts(altitude_ft: Option<i32>, min_ft: Option<u32>, max_ft: Option<u32>) -> bool {
    match altitude_ft {
        None => min_ft.is_none(),
        Some(alt) => min_ft.is_none_or(|min| alt >= min as i32) && max_ft.is_none_or(|max| alt <= max as i32),
    }
}

/// One sentence describing the alert altitude limits, for the settings page.
/// (The page's script words it the same way as you type.)
pub fn altitude_summary(min_ft: Option<u32>, max_ft: Option<u32>) -> String {
    fn ft(n: u32) -> String {
        let digits = n.to_string();
        let mut out = String::new();
        for (i, c) in digits.chars().enumerate() {
            if i > 0 && (digits.len() - i) % 3 == 0 {
                out.push(',');
            }
            out.push(c);
        }
        out + " ft"
    }
    const GROUND_IN: &str = " Aircraft on the ground or not reporting their altitude are included.";
    const GROUND_OUT: &str = " Aircraft on the ground or not reporting their altitude are left out.";
    match (min_ft, max_ft) {
        (None, None) => "Alerts for aircraft at any altitude.".into(),
        (None, Some(max)) => format!("Alerts for aircraft up to {}.{}", ft(max), GROUND_IN),
        (Some(min), None) => format!("Alerts for aircraft from {} up.{}", ft(min), GROUND_OUT),
        (Some(min), Some(max)) if min >= max => ALTITUDE_ORDER_ERROR.into(),
        (Some(min), Some(max)) => format!("Alerts for aircraft from {} to {}.{}", ft(min), ft(max), GROUND_OUT),
    }
}

/// Backgrounds: (config id, name, ground, surface, date panel). The first is the default.
pub const BACKGROUNDS: [(&str, &str, &str, &str, &str); 2] = [
    ("aviator-blue", "Aviator blue", "#0B1118", "#121A23", "#141D28"),
    ("black", "Black", "#000000", "#101214", "#15181B"),
];

/// Accent colours from the design: (config id, name, hex), in the design's order.
pub const DEFAULT_ACCENT: &str = "amber";
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
    /// Where the clock is. Unset until the owner sets it on the settings
    /// page: there is no default place, and no flights are fetched without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latitude: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub longitude: Option<f64>,
    #[serde(default = "default_radius_nm")]
    pub radius_nm: f64,
    #[serde(default = "default_facing_direction")]
    pub facing_direction: String,
    /// Face id from `CLOCK_FACES`; changed by swiping up or down on the clock
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
    /// The owner chose "Later" on first-run setup: stop offering it on the
    /// clock's screen while no location is set.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub setup_dismissed: bool,
    /// The "how it works" gesture hints have been read.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hints_seen: bool,
    /// Night mode id from `NIGHT_MODES`, in force from `night_start` to
    /// `night_end` ("HH:MM", local time; may run past midnight).
    #[serde(default = "default_night_mode")]
    pub night_mode: String,
    #[serde(default = "default_night_start")]
    pub night_start: String,
    #[serde(default = "default_night_end")]
    pub night_end: String,
    /// Show new-flight alerts during night hours too.
    #[serde(default)]
    pub night_alerts: bool,
    /// New-flight alerts take over the screen. Off: flights only show on the radar.
    #[serde(default = "default_true")]
    pub alerts_enabled: bool,
    /// Only alert for aircraft at or above this altitude (feet). Unset: no
    /// lower limit. When set, aircraft on the ground or not reporting an
    /// altitude don't alert.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert_min_altitude_ft: Option<u32>,
    /// Only alert for aircraft at or below this altitude (feet). Unset: any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alert_max_altitude_ft: Option<u32>,
    /// Shortest time between alerts, in minutes (0: no limit).
    #[serde(default)]
    pub alert_min_gap_min: u32,
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
fn default_night_mode() -> String {
    "off".to_string()
}
fn default_night_start() -> String {
    "22:00".to_string()
}
fn default_night_end() -> String {
    "07:00".to_string()
}
fn default_true() -> bool {
    true
}
fn default_accent() -> String {
    DEFAULT_ACCENT.to_string()
}
fn default_background() -> String {
    BACKGROUNDS[0].0.to_string()
}

/// "HH:MM" (24-hour) to minutes after midnight.
pub fn parse_hhmm(text: &str) -> Option<u32> {
    let (h, m) = text.split_once(':')?;
    if h.len() != 2 || m.len() != 2 || !h.bytes().chain(m.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (h, m): (u32, u32) = (h.parse().ok()?, m.parse().ok()?);
    (h < 24 && m < 60).then_some(h * 60 + m)
}

impl Default for Config {
    fn default() -> Self {
        Self {
            latitude: None,
            longitude: None,
            radius_nm: default_radius_nm(),
            facing_direction: default_facing_direction(),
            clock_face: default_clock_face(),
            show_date: default_show_date(),
            update_channel: default_update_channel(),
            auto_update: default_auto_update(),
            accent: default_accent(),
            background: default_background(),
            setup_dismissed: false,
            hints_seen: false,
            night_mode: default_night_mode(),
            night_start: default_night_start(),
            night_end: default_night_end(),
            night_alerts: false,
            alerts_enabled: true,
            alert_min_altitude_ft: None,
            alert_max_altitude_ft: None,
            alert_min_gap_min: 0,
        }
    }
}

impl Config {
    /// The accent colour as "#RRGGBB"; the default (amber) if the id is unknown.
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

    /// None until a location has been set.
    pub fn location(&self) -> Option<Location> {
        Some(Location {
            lat: self.latitude?,
            lon: self.longitude?,
            max_radius_nm: self.radius_nm,
        })
    }

    /// Check every field holds a value the clock can use. The settings page
    /// runs in a separate, unprivileged process; the clock checks what it
    /// sends with this before saving it.
    pub fn validate(&self) -> Result<(), String> {
        match (self.latitude, self.longitude) {
            (Some(lat), Some(lon)) => {
                if !lat.is_finite() || !(-90.0..=90.0).contains(&lat) {
                    return Err("Latitude must be between -90 and 90.".into());
                }
                if !lon.is_finite() || !(-180.0..=180.0).contains(&lon) {
                    return Err("Longitude must be between -180 and 180.".into());
                }
            }
            (None, None) => {}
            _ => return Err("Latitude and longitude must be set together.".into()),
        }
        if !self.radius_nm.is_finite() || !(RADIUS_MIN_NM..=RADIUS_MAX_NM).contains(&self.radius_nm) {
            return Err(format!("Radius must be between {} and {} NM.", RADIUS_MIN_NM, RADIUS_MAX_NM));
        }
        if !FACINGS.contains(&self.facing_direction.as_str()) {
            return Err("Pick a facing direction.".into());
        }
        if !CLOCK_FACES.contains(&self.clock_face.as_str()) {
            return Err("Pick a clock face.".into());
        }
        if self.update_channel != "stable" && self.update_channel != "beta" {
            return Err("Pick an update channel.".into());
        }
        if !ACCENTS.iter().any(|(id, _, _)| *id == self.accent) {
            return Err("Pick a colour.".into());
        }
        if !BACKGROUNDS.iter().any(|(id, ..)| *id == self.background) {
            return Err("Pick a background.".into());
        }
        if !NIGHT_MODES.iter().any(|(id, _)| *id == self.night_mode) {
            return Err("Pick a night mode.".into());
        }
        if parse_hhmm(&self.night_start).is_none() || parse_hhmm(&self.night_end).is_none() {
            return Err("Night hours must be times like 22:00.".into());
        }
        let out_of_range = |ft: Option<u32>| ft.is_some_and(|ft| !(ALERT_ALTITUDE_MIN_FT..=ALERT_ALTITUDE_MAX_FT).contains(&ft));
        if out_of_range(self.alert_min_altitude_ft) || out_of_range(self.alert_max_altitude_ft) {
            return Err(format!(
                "Alert altitudes must be between {} and {} feet.",
                ALERT_ALTITUDE_MIN_FT, ALERT_ALTITUDE_MAX_FT
            ));
        }
        if let (Some(low), Some(high)) = (self.alert_min_altitude_ft, self.alert_max_altitude_ft) {
            if low >= high {
                return Err(ALTITUDE_ORDER_ERROR.into());
            }
        }
        if self.alert_min_gap_min > 120 {
            return Err("The gap between alerts can be at most 120 minutes.".into());
        }
        Ok(())
    }

    /// Is night mode in force at this time of day (minutes after midnight)?
    /// The hours may run past midnight (22:00 to 07:00). The same start and
    /// end means no night at all.
    pub fn is_night(&self, minute_of_day: u32) -> bool {
        if self.night_mode == "off" {
            return false;
        }
        let (Some(start), Some(end)) = (parse_hhmm(&self.night_start), parse_hhmm(&self.night_end)) else {
            return false;
        };
        if start <= end {
            (start..end).contains(&minute_of_day)
        } else {
            minute_of_day >= start || minute_of_day < end
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

        // Write a new file and rename it over the old one, so a power cut
        // mid-write can't leave half a config (which would load as defaults).
        let tmp = p.with_extension("json.tmp");
        let written = (|| -> std::io::Result<()> {
            use std::io::Write;
            let mut file = fs::File::create(&tmp)?;
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
            fs::rename(&tmp, p)
        })();
        written.map_err(|e| {
            let _ = fs::remove_file(&tmp);
            format!("Failed to write config: {}", e)
        })
    }
}

#[cfg(test)]
mod accent_tests {
    use super::*;

    #[test]
    fn defaults_to_amber() {
        assert_eq!(Config::default().accent_hex(), "#FFB547");
    }

    #[test]
    fn unknown_accent_falls_back_to_amber() {
        let cfg = Config { accent: "magenta".into(), ..Config::default() };
        assert_eq!(cfg.accent_hex(), "#FFB547");
    }

    #[test]
    fn coral_selectable() {
        let cfg = Config { accent: "coral".into(), ..Config::default() };
        assert_eq!(cfg.accent_hex(), "#FF7F6B");
    }

    #[test]
    fn old_config_without_colours_loads() {
        let cfg: Config = serde_json::from_str(r#"{"latitude":55.9,"longitude":-3.2}"#).unwrap();
        assert_eq!(cfg.accent, "amber");
        assert_eq!(cfg.background, "aviator-blue");
    }

    #[test]
    fn no_location_until_one_is_set() {
        assert!(Config::default().location().is_none());
        let cfg: Config = serde_json::from_str(r#"{"radius_nm":6.0}"#).unwrap();
        assert!(cfg.location().is_none());
        // Unset coordinates aren't written out as null
        assert!(!serde_json::to_string(&cfg).unwrap().contains("latitude"));

        let cfg: Config = serde_json::from_str(r#"{"latitude":51.47,"longitude":-0.45}"#).unwrap();
        let loc = cfg.location().unwrap();
        assert_eq!((loc.lat, loc.lon, loc.max_radius_nm), (51.47, -0.45, 6.0));
    }

    #[test]
    fn validate_rejects_bad_values() {
        assert!(Config::default().validate().is_ok());
        let ok = Config { latitude: Some(51.47), longitude: Some(-0.45), ..Config::default() };
        assert!(ok.validate().is_ok());

        let bad = [
            Config { latitude: Some(91.0), longitude: Some(0.0), ..Config::default() },
            Config { latitude: Some(f64::NAN), longitude: Some(0.0), ..Config::default() },
            Config { latitude: Some(10.0), ..Config::default() },
            Config { radius_nm: 500.0, ..Config::default() },
            Config { facing_direction: "UP".into(), ..Config::default() },
            Config { clock_face: "sundial".into(), ..Config::default() },
            Config { update_channel: "nightly".into(), ..Config::default() },
            Config { accent: "#000000".into(), ..Config::default() },
            Config { background: "white".into(), ..Config::default() },
            Config { night_mode: "sleepy".into(), ..Config::default() },
            Config { night_start: "25:00".into(), ..Config::default() },
            Config { night_end: "7am".into(), ..Config::default() },
            Config { alert_max_altitude_ft: Some(10), ..Config::default() },
            Config { alert_min_gap_min: 100_000, ..Config::default() },
        ];
        for cfg in bad {
            assert!(cfg.validate().is_err(), "{:?}", cfg);
        }
    }

    #[test]
    fn setup_dismissed_is_only_written_when_set() {
        let json = serde_json::to_string(&Config::default()).unwrap();
        assert!(!json.contains("setup_dismissed"));
        let cfg = Config { setup_dismissed: true, ..Config::default() };
        let json = serde_json::to_string(&cfg).unwrap();
        assert!(serde_json::from_str::<Config>(&json).unwrap().setup_dismissed);
    }

    #[test]
    fn swiping_goes_round_the_clock_faces() {
        let first = CLOCK_FACES[0];
        let last = CLOCK_FACES[CLOCK_FACES.len() - 1];
        assert_eq!(next_clock_face(first, 1), CLOCK_FACES[1]);
        assert_eq!(next_clock_face(last, 1), first);
        assert_eq!(next_clock_face(first, -1), last);
        // All the way round, either way, comes back
        let mut face = first;
        for _ in 0..CLOCK_FACES.len() {
            face = next_clock_face(face, 1);
        }
        assert_eq!(face, first);
        for _ in 0..CLOCK_FACES.len() {
            face = next_clock_face(face, -1);
        }
        assert_eq!(face, first);
        // An id this version doesn't know starts from the first face
        assert_eq!(next_clock_face("sundial", 1), CLOCK_FACES[1]);
        assert_eq!(default_clock_face(), first);
    }

    #[test]
    fn parses_times_of_day() {
        assert_eq!(parse_hhmm("00:00"), Some(0));
        assert_eq!(parse_hhmm("22:30"), Some(22 * 60 + 30));
        for bad in ["24:00", "12:60", "7:00", "0700", "", "ab:cd", "12:5", "+1:00"] {
            assert_eq!(parse_hhmm(bad), None, "{:?}", bad);
        }
    }

    #[test]
    fn night_hours_can_run_past_midnight() {
        let at = |h: u32, m: u32| h * 60 + m;
        let off = Config::default();
        assert!(!off.is_night(at(23, 0)));

        let overnight = Config { night_mode: "dim".into(), ..Config::default() }; // 22:00-07:00
        assert!(overnight.is_night(at(22, 0)));
        assert!(overnight.is_night(at(3, 0)));
        assert!(overnight.is_night(at(6, 59)));
        assert!(!overnight.is_night(at(7, 0)));
        assert!(!overnight.is_night(at(21, 59)));

        let daytime = Config { night_mode: "screen_off".into(), night_start: "09:00".into(), night_end: "17:00".into(), ..Config::default() };
        assert!(daytime.is_night(at(12, 0)));
        assert!(!daytime.is_night(at(17, 0)));
        assert!(!daytime.is_night(at(8, 59)));

        let never = Config { night_mode: "dim".into(), night_start: "08:00".into(), night_end: "08:00".into(), ..Config::default() };
        assert!(!never.is_night(at(8, 0)) && !never.is_night(at(20, 0)));
    }

    #[test]
    fn old_config_files_get_the_new_defaults() {
        let cfg: Config = serde_json::from_str(r#"{"radius_nm":6.0,"facing_direction":"N"}"#).unwrap();
        assert!(cfg.alerts_enabled && !cfg.night_alerts && !cfg.hints_seen);
        assert_eq!((cfg.night_mode.as_str(), cfg.alert_min_gap_min, cfg.alert_max_altitude_ft), ("off", 0, None));
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn save_replaces_the_file_whole() {
        let path = std::env::temp_dir().join(format!("fc-config-test-{}.json", std::process::id()));
        let path = path.to_str().unwrap();
        let cfg = Config { latitude: Some(51.47), longitude: Some(-0.45), accent: "lime".into(), ..Config::default() };
        cfg.save(Some(path)).unwrap();
        cfg.save(Some(path)).unwrap();
        let loaded = Config::load(Some(path));
        assert_eq!(loaded.accent, "lime");
        assert_eq!(loaded.latitude, Some(51.47));
        assert!(!Path::new(path).with_extension("json.tmp").exists());
        let _ = fs::remove_file(path);
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

#[cfg(test)]
mod altitude_tests {
    use super::*;

    #[test]
    fn altitude_limits_match_the_examples() {
        // A 1,500  B 5,000  C 12,000  D 35,000  E ground  F unknown (both None)
        let fleet = [Some(1_500), Some(5_000), Some(12_000), Some(35_000), None];
        let passes = |min, max| fleet.iter().map(|alt| altitude_alerts(*alt, min, max)).collect::<Vec<_>>();
        assert_eq!(passes(None, None), [true, true, true, true, true]);
        assert_eq!(passes(None, Some(10_000)), [true, true, false, false, true]);
        assert_eq!(passes(Some(2_000), None), [false, true, true, true, false]);
        assert_eq!(passes(Some(2_000), Some(10_000)), [false, true, false, false, false]);
        assert_eq!(passes(Some(20_000), None), [false, false, false, true, false]);
        // Limits are inclusive
        assert!(altitude_alerts(Some(2_000), Some(2_000), Some(10_000)));
        assert!(altitude_alerts(Some(10_000), Some(2_000), Some(10_000)));
    }

    #[test]
    fn altitude_summary_reads_naturally() {
        assert_eq!(altitude_summary(None, None), "Alerts for aircraft at any altitude.");
        assert!(altitude_summary(None, Some(10_000)).starts_with("Alerts for aircraft up to 10,000 ft. "));
        assert!(altitude_summary(None, Some(10_000)).ends_with("are included."));
        assert!(altitude_summary(Some(2_000), None).starts_with("Alerts for aircraft from 2,000 ft up. "));
        assert!(altitude_summary(Some(2_000), Some(10_000)).starts_with("Alerts for aircraft from 2,000 ft to 10,000 ft. "));
        assert!(altitude_summary(Some(2_000), Some(10_000)).ends_with("are left out."));
        assert_eq!(altitude_summary(Some(500), Some(500)), ALTITUDE_ORDER_ERROR);
    }

    #[test]
    fn altitude_limits_validate() {
        let ok = Config { alert_min_altitude_ft: Some(2_000), alert_max_altitude_ft: Some(10_000), ..Config::default() };
        assert!(ok.validate().is_ok());
        let crossed = Config { alert_min_altitude_ft: Some(10_000), alert_max_altitude_ft: Some(2_000), ..Config::default() };
        assert!(crossed.validate().is_err());
        let equal = Config { alert_min_altitude_ft: Some(5_000), alert_max_altitude_ft: Some(5_000), ..Config::default() };
        assert!(equal.validate().is_err());
        assert!(Config { alert_min_altitude_ft: Some(100), ..Config::default() }.validate().is_err());
        // Older configs have neither
        let old: Config = serde_json::from_str(r#"{"alert_max_altitude_ft":8000}"#).unwrap();
        assert_eq!((old.alert_min_altitude_ft, old.alert_max_altitude_ft), (None, Some(8000)));
    }
}
