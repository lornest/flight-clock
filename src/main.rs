#![allow(dead_code)]

mod config;
mod flight;
mod settings_web;
mod ui_paths;
mod update;
mod utils;
mod watchdog;
mod wifi;

use config::{Config, Location};
use flight::poller::{FlightState, NewFlightWithInfo, PollEvent, Poller};
use utils::geo::{calculate_distance, calculate_plane_rotation, direction_to_degrees, polar_to_screen};
use settings_web::{SettingsEvent, SettingsWeb};
use wifi::portal::{Portal, PortalState};
use wifi::supervisor::{Supervisor, WifiCommand, WifiUiState};

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

slint::include_modules!();

/// Polling intervals in milliseconds
const POLL_IDLE: u64 = 30000; // Clock screen, no alert
const POLL_ACTIVE: u64 = 5000; // Alert active or radar screen

/// How long an alert stays up after the most recent new flight
const ALERT_DURATION: Duration = Duration::from_secs(30);
/// Flight cards shown on the alert; the rest are counted as "+N more"
const ALERT_MAX_CARDS: usize = 2;
const ALERT_MAX_BEACONS: usize = 8;

/// Shortest time the startup splash stays up
const MIN_SPLASH: Duration = Duration::from_secs(4);

/// Radius of the outer radar range ring, in display pixels
const DISPLAY_MAX_RADIUS_PX: f64 = 210.0;
/// Length of an aircraft's heading line on the radar/alert
const HEADING_LINE_PX: f64 = 22.0;
/// Earlier positions kept per aircraft for radar trails
const TRAIL_POINTS: usize = 3;
/// Second hand plane distance from the centre
const SECOND_PLANE_RADIUS: f64 = 186.0;
/// Digital face: radius of the progress ring the plane orbits on
const ORBIT_RADIUS: f64 = 220.0;
/// Below this altitude (feet) labels show altitude ("A025"), above it flight level ("FL340")
const TRANSITION_ALTITUDE_FT: i32 = 6000;

/// Recent positions (lat, lon) per aircraft, oldest first, for radar trails.
type Trails = HashMap<String, VecDeque<(f64, f64)>>;

/// Double-tap detection parameters
const DOUBLE_TAP_TIMEOUT_MS: u64 = 500;
const DOUBLE_TAP_DISTANCE: f64 = 50.0;
const CENTER_RADIUS: f64 = 72.0; // 480 * 0.15

/// Flights collected by the alert currently on screen. Only touched on the
/// UI thread; the Mutex is just so it can travel in Send closures.
#[derive(Default)]
struct AlertState {
    /// Bumped on every new flight so older auto-dismiss timers do nothing.
    generation: u64,
    flights: Vec<NewFlightWithInfo>,
}

fn main() {
    // Subcommands (no UI): `--version`, `update <scheduled|now|check>`
    let args: Vec<String> = std::env::args().collect();
    match args.get(1).map(String::as_str) {
        Some("--version") | Some("-V") => {
            println!("flight-clock {}", update::VERSION);
            return;
        }
        Some("update") => {
            let mode = args.get(2).map(String::as_str).unwrap_or("check");
            let Some(mode) = update::Mode::parse(mode) else {
                eprintln!("usage: flight-clock update <scheduled|now|check>");
                std::process::exit(2);
            };
            std::process::exit(match update::run(mode) {
                Ok(()) => 0,
                Err(e) => {
                    eprintln!("update failed: {}", e);
                    1
                }
            });
        }
        Some(other) if other.starts_with('-') || !other.is_empty() => {
            eprintln!("usage: flight-clock [--version | update <scheduled|now|check>]");
            std::process::exit(2);
        }
        _ => {}
    }

    eprintln!("flight-clock {}", update::VERSION);
    let config = Config::load(None);
    // Shared so the web settings page can move the clock without a restart
    let location = Arc::new(Mutex::new(config.location()));
    let facing_direction = Arc::new(Mutex::new(config.facing_direction.clone()));
    let started_at = Instant::now();

    let ui = App::new().unwrap();
    ui.set_facing_direction(config.facing_direction.clone().into());
    ui.set_radar_compass(model(compass_labels(&config.facing_direction)));
    ui.set_radar_ring_labels(model(ring_labels(config.radius_nm)));
    ui.set_clock_face(normalise_face(&config.clock_face).into());
    ui.set_show_date(config.show_date);

    // Dev aid: FLIGHT_CLOCK_SCREEN=radar starts on the radar (e.g. for screenshots)
    let start_on_radar = std::env::var("FLIGHT_CLOCK_SCREEN").as_deref() == Ok("radar");
    if start_on_radar {
        ui.set_current_screen(1);
    }
    let poll_interval = Arc::new(AtomicU64::new(if start_on_radar { POLL_ACTIVE } else { POLL_IDLE }));
    let alert = Arc::new(Mutex::new(AlertState::default()));
    let trails = Arc::new(Mutex::new(Trails::new()));

    // --- Web server: WiFi setup portal (hotspot up) or settings page (on the LAN) ---
    let network_ready = Arc::new(AtomicBool::new(false));
    let portal_state = Arc::new(Mutex::new(PortalState::default()));
    let (wifi_tx, wifi_rx) = mpsc::channel::<WifiCommand>();
    let http_port = web_port();
    ui.set_web_address(web_address(http_port).into());

    let settings_web = {
        let ui_handle = ui.as_weak();
        let location = location.clone();
        let facing = facing_direction.clone();
        Arc::new(SettingsWeb::new(move |event| {
            let ui_handle = ui_handle.clone();
            let location = location.clone();
            let facing = facing.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_handle.upgrade() {
                    apply_settings_event(&ui, event, &location, &facing);
                }
            });
        }))
    };
    {
        let portal = Portal::new(portal_state.clone(), wifi_tx.clone(), settings_web.clone(), http_port);
        std::thread::spawn(move || portal.serve());
    }
    {
        let settings_web = settings_web.clone();
        let ui_handle = ui.as_weak();
        ui.on_pairing_cancelled(move || {
            settings_web.cancel_code();
            if let Some(ui) = ui_handle.upgrade() {
                ui.set_pairing_code("".into());
            }
        });
    }

    {
        let ui_handle = ui.as_weak();
        let network_ready = network_ready.clone();
        std::thread::spawn(move || {
            let supervisor = Supervisor::new(wifi_rx, portal_state, network_ready, move |state| {
                let ui_handle = ui_handle.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_handle.upgrade() {
                        apply_wifi_state(&ui, state, started_at);
                    }
                });
            });
            supervisor.run();
        });
    }

    // --- Flight poller (background thread, paused while offline/in setup) ---
    {
        let ui_handle = ui.as_weak();
        let poll_interval = poll_interval.clone();
        let facing = facing_direction.clone();
        let alert = alert.clone();
        let trails = trails.clone();
        let network_ready = network_ready.clone();
        let location = location.clone();

        std::thread::spawn(move || {
            let poller = Poller::new(location.clone(), poll_interval.clone(), network_ready);
            poller.run(move |event| {
                let facing = facing.lock().unwrap().clone();
                let location = *location.lock().unwrap();
                let ui_handle = ui_handle.clone();
                let alert = alert.clone();
                let trails = trails.clone();
                let poll_interval = poll_interval.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    let Some(ui) = ui_handle.upgrade() else { return };
                    match event {
                        PollEvent::Fetching => ui.set_radar_updating(true),
                        PollEvent::Failed(msg) => {
                            ui.set_radar_updating(false);
                            ui.set_radar_error(msg.into());
                        }
                        PollEvent::Updated(state) => {
                            ui.set_radar_updating(false);
                            ui.set_radar_error("".into());
                            update_radar(&ui, &state, &facing, location, &trails);
                            update_alert(&ui, &state, &facing, location, &alert, &poll_interval);
                        }
                    }
                });
            });
        });
    }

    // --- Clock update callback ---
    let ui_handle_clock = ui.as_weak();
    ui.on_update_clock(move || {
        if let Some(ui) = ui_handle_clock.upgrade() {
            let now = chrono::Local::now();
            let h = chrono::Timelike::hour(&now) as f32;
            let m = chrono::Timelike::minute(&now) as f32;
            let s = chrono::Timelike::second(&now) as f32;

            ui.set_second_angle(s * 6.0);
            ui.set_minute_angle(m * 6.0 + s * 0.1);
            ui.set_hour_angle((h % 12.0) * 30.0 + m * 0.5);
            if ui.get_clock_face() == "digital" {
                let minute_fraction = s as f64 / 60.0;
                ui.set_digital_progress(ui_paths::progress_arc(ORBIT_RADIUS, minute_fraction).into());
                ui.set_digital_plane(ui_paths::orbit_plane(ORBIT_RADIUS, minute_fraction).into());
            } else {
                ui.set_second_plane(ui_paths::second_hand_plane(s as f64 * 6.0, SECOND_PLANE_RADIUS).into());
            }
            ui.set_date_text(now.format("%a %d %b").to_string().to_uppercase().into());
            ui.set_time_text(now.format("%H:%M").to_string().into());
        }
    });

    // --- Alert dismiss callback ---
    {
        let ui_handle = ui.as_weak();
        let alert = alert.clone();
        let poll_interval = poll_interval.clone();
        ui.on_alert_dismissed(move || {
            if let Some(ui) = ui_handle.upgrade() {
                end_alert(&ui, &alert, &poll_interval);
            }
        });
    }

    // --- Screen tap handler (double-tap navigation) ---
    let ui_handle_tap = ui.as_weak();
    let poll_interval_tap = poll_interval.clone();
    let last_tap_time = Arc::new(AtomicU64::new(0));
    let last_tap_x = Arc::new(Mutex::new(0.0f64));
    let last_tap_y = Arc::new(Mutex::new(0.0f64));

    ui.on_screen_tapped(move |x, y| {
        let x = x as f64;
        let y = y as f64;
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let prev_time = last_tap_time.load(Ordering::Relaxed);
        let prev_x = *last_tap_x.lock().unwrap();
        let prev_y = *last_tap_y.lock().unwrap();

        let time_diff = now_ms.saturating_sub(prev_time);
        let distance = ((x - prev_x).powi(2) + (y - prev_y).powi(2)).sqrt();

        let is_double_tap =
            time_diff < DOUBLE_TAP_TIMEOUT_MS && time_diff > 50 && distance < DOUBLE_TAP_DISTANCE;

        let dist_from_center = (x * x + y * y).sqrt();

        if is_double_tap {
            if let Some(ui) = ui_handle_tap.upgrade() {
                if dist_from_center <= CENTER_RADIUS {
                    ui.set_show_direction_selector(true);
                } else {
                    let current = ui.get_current_screen();
                    if x < 0.0 && current == 1 {
                        ui.set_current_screen(0);
                    } else if x >= 0.0 && current == 0 {
                        ui.set_current_screen(1);
                    }
                    poll_interval_tap.store(desired_poll_interval(&ui), Ordering::Relaxed);
                }
            }
            last_tap_time.store(0, Ordering::Relaxed);
        } else {
            last_tap_time.store(now_ms, Ordering::Relaxed);
            *last_tap_x.lock().unwrap() = x;
            *last_tap_y.lock().unwrap() = y;
        }
    });

    // --- Direction selected ---
    let ui_handle_dir = ui.as_weak();
    let facing_dir = facing_direction.clone();
    ui.on_direction_selected(move |dir| {
        let dir_str: String = dir.into();
        if let Some(ui) = ui_handle_dir.upgrade() {
            *facing_dir.lock().unwrap() = dir_str.clone();
            ui.set_radar_compass(model(compass_labels(&dir_str)));
            ui.set_facing_direction(dir_str.into());
            // Settings stays open: the Facing page returns to the menu

            // Save to config
            let mut cfg = Config::load(None);
            cfg.facing_direction = facing_dir.lock().unwrap().clone();
            let _ = cfg.save(None);
        }
    });

    // --- Direction selector close ---
    let ui_handle_dir_close = ui.as_weak();
    ui.on_direction_selector_closed(move || {
        if let Some(ui) = ui_handle_dir_close.upgrade() {
            ui.set_show_direction_selector(false);
        }
    });

    // --- Clock face (hold the logo) and date (settings screen) ---
    {
        let ui_handle = ui.as_weak();
        ui.on_clock_face_toggle_requested(move || {
            let Some(ui) = ui_handle.upgrade() else { return };
            let next = if ui.get_clock_face() == "digital" { "analog" } else { "digital" };
            ui.set_clock_face(next.into());
            ui.invoke_update_clock();
            save_setting(|cfg| cfg.clock_face = next.to_string());
        });
    }
    {
        let ui_handle = ui.as_weak();
        ui.on_show_date_toggled(move || {
            let Some(ui) = ui_handle.upgrade() else { return };
            let show = !ui.get_show_date();
            ui.set_show_date(show);
            save_setting(|cfg| cfg.show_date = show);
        });
    }

    // --- WiFi setup from the settings screen ---
    {
        let ui_handle = ui.as_weak();
        let tx = wifi_tx.clone();
        ui.on_wifi_setup_requested(move || {
            if let Some(ui) = ui_handle.upgrade() {
                ui.set_show_direction_selector(false);
                // Show the setup screen straight away; the supervisor fills in status.
                ui.set_wifi_setup_mode(true);
                ui.set_wifi_status("Starting setup network...".into());
                ui.set_wifi_status_error(false);
                ui.set_wifi_can_cancel(false);
            }
            let _ = tx.send(WifiCommand::EnterSetup);
        });
    }
    {
        let tx = wifi_tx.clone();
        ui.on_wifi_setup_cancelled(move || {
            let _ = tx.send(WifiCommand::CancelSetup);
        });
    }

    // --- systemd watchdog: ping from the UI event loop so a hung UI gets restarted ---
    let _watchdog_timer = watchdog::ping_interval().map(|interval| {
        let timer = slint::Timer::default();
        timer.start(slint::TimerMode::Repeated, interval, || watchdog::notify("WATCHDOG=1"));
        timer
    });
    watchdog::notify("READY=1");

    // Run the Slint event loop
    ui.run().unwrap();
}

/// Port for the web server: $FLIGHT_CLOCK_HTTP_PORT, else 80 on the Pi and
/// 8080 elsewhere (port 80 needs root on a dev machine).
fn web_port() -> u16 {
    std::env::var("FLIGHT_CLOCK_HTTP_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(if cfg!(target_os = "linux") { 80 } else { 8080 })
}

/// Address shown on the settings menu, e.g. "flight-clock.local".
fn web_address(port: u16) -> String {
    let host = std::fs::read_to_string("/etc/hostname")
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty() && cfg!(target_os = "linux"))
        .map(|h| format!("{}.local", h))
        .unwrap_or_else(|| "localhost".to_string());
    if port == 80 {
        host
    } else {
        format!("{}:{}", host, port)
    }
}

/// How long a pairing code stays on screen (matches the page's code expiry).
const PAIRING_CODE_SHOWN: Duration = Duration::from_secs(120);

/// Apply something the web settings page did.
fn apply_settings_event(
    ui: &App,
    event: SettingsEvent,
    location: &Arc<Mutex<Location>>,
    facing: &Arc<Mutex<String>>,
) {
    match event {
        SettingsEvent::ShowCode(code) => {
            ui.set_show_direction_selector(false);
            // Shown grouped for reading: "482913" -> "482 913"
            let code = if code.len() == 6 { format!("{} {}", &code[..3], &code[3..]) } else { code };
            ui.set_pairing_code(code.clone().into());
            let ui_handle = ui.as_weak();
            slint::Timer::single_shot(PAIRING_CODE_SHOWN, move || {
                if let Some(ui) = ui_handle.upgrade() {
                    if ui.get_pairing_code() == code.as_str() {
                        ui.set_pairing_code("".into());
                    }
                }
            });
        }
        SettingsEvent::HideCode => ui.set_pairing_code("".into()),
        SettingsEvent::Applied(cfg) => {
            *location.lock().unwrap() = cfg.location();
            *facing.lock().unwrap() = cfg.facing_direction.clone();
            ui.set_facing_direction(cfg.facing_direction.clone().into());
            ui.set_radar_compass(model(compass_labels(&cfg.facing_direction)));
            ui.set_radar_ring_labels(model(ring_labels(cfg.radius_nm)));
            ui.set_clock_face(normalise_face(&cfg.clock_face).into());
            ui.set_show_date(cfg.show_date);
            ui.invoke_update_clock();
        }
    }
}

/// Load the config, change one setting, write it back.
fn save_setting(change: impl FnOnce(&mut Config)) {
    let mut cfg = Config::load(None);
    change(&mut cfg);
    if let Err(e) = cfg.save(None) {
        eprintln!("Couldn't save settings: {}", e);
    }
}

fn normalise_face(face: &str) -> &'static str {
    if face.eq_ignore_ascii_case("digital") {
        "digital"
    } else {
        "analog"
    }
}

fn desired_poll_interval(ui: &App) -> u64 {
    if ui.get_current_screen() == 1 || ui.get_alert_active() {
        POLL_ACTIVE
    } else {
        POLL_IDLE
    }
}

/// Reflect the WiFi supervisor's state on screen.
fn apply_wifi_state(ui: &App, state: WifiUiState, started_at: Instant) {
    match state {
        WifiUiState::Connecting(message) => {
            ui.set_startup_status(message.clone().into());
            // After a cancel, the setup screen is still up while we rejoin.
            ui.set_wifi_status(message.into());
            ui.set_wifi_status_error(false);
            ui.set_wifi_can_cancel(false);
        }
        WifiUiState::Online { ssid } => {
            ui.set_wifi_setup_mode(false);
            ui.set_wifi_network(ssid.into());
            finish_startup(ui, started_at);
        }
        WifiUiState::Setup {
            status,
            is_error,
            can_cancel,
        } => {
            ui.set_wifi_setup_mode(true);
            ui.set_wifi_status(status.into());
            ui.set_wifi_status_error(is_error);
            ui.set_wifi_can_cancel(can_cancel);
            ui.set_show_direction_selector(false);
            finish_startup(ui, started_at);
        }
    }
}

/// Drop the splash, but not before it's been up for MIN_SPLASH.
fn finish_startup(ui: &App, started_at: Instant) {
    if !ui.get_starting_up() {
        return;
    }
    let remaining = MIN_SPLASH.saturating_sub(started_at.elapsed());
    if remaining.is_zero() {
        ui.set_starting_up(false);
        return;
    }
    let ui_handle = ui.as_weak();
    slint::Timer::single_shot(remaining, move || {
        if let Some(ui) = ui_handle.upgrade() {
            ui.set_starting_up(false);
        }
    });
}

fn update_radar(
    ui: &App,
    state: &FlightState,
    facing_direction: &str,
    loc: Location,
    trails: &Mutex<Trails>,
) {
    let facing_deg = direction_to_degrees(facing_direction);
    let to_screen = |lat: f64, lon: f64| -> (f64, f64, f64) {
        let distance_nm = calculate_distance(loc.lat, loc.lon, lat, lon);
        let rotation = calculate_plane_rotation(loc.lat, loc.lon, lat, lon, facing_direction);
        let ratio = (distance_nm / loc.max_radius_nm).min(1.0);
        let (sx, sy) = polar_to_screen(rotation, ratio * DISPLAY_MAX_RADIUS_PX);
        (240.0 + sx, 240.0 + sy, distance_nm)
    };

    let mut trails = trails.lock().unwrap();
    trails.retain(|hex, _| state.flights.iter().any(|f| &f.hex == hex));

    let in_range: Vec<_> = state
        .flights
        .iter()
        .map(|f| (f, to_screen(f.lat, f.lon)))
        .filter(|(_, (_, _, d))| *d <= loc.max_radius_nm)
        .collect();
    let closest = in_range
        .iter()
        .min_by(|a, b| a.1 .2.total_cmp(&b.1 .2))
        .map(|(f, _)| f.hex.clone());

    let beacons: Vec<RadarBeacon> = in_range
        .iter()
        .map(|(flight, (x, y, _))| {
            // Trail from earlier fixes, then record this one
            let history = trails.entry(flight.hex.clone()).or_default();
            let trail: Vec<TrailPoint> = history
                .iter()
                .map(|&(lat, lon)| {
                    let (tx, ty, _) = to_screen(lat, lon);
                    TrailPoint { x: tx as f32, y: ty as f32 }
                })
                .collect();
            if history.back() != Some(&(flight.lat, flight.lon)) {
                history.push_back((flight.lat, flight.lon));
                while history.len() > TRAIL_POINTS {
                    history.pop_front();
                }
            }

            let (hx, hy) = heading_end(*x, *y, flight.track, facing_deg);
            RadarBeacon {
                hex: flight.hex.clone().into(),
                callsign: display_callsign(&flight.callsign, &flight.hex).into(),
                x: *x as f32,
                y: *y as f32,
                hx: hx as f32,
                hy: hy as f32,
                trail: model(trail),
                info: flight_info_label(flight.alt_baro, flight.gs).into(),
                label_left: *x > 330.0,
                highlight: closest.as_deref() == Some(flight.hex.as_str()),
            }
        })
        .collect();

    ui.set_radar_beacons(model(beacons));
    ui.set_flight_count(in_range.len() as i32);
}

/// End point of a heading line from (x, y) for a true track, on a display
/// whose top faces `facing_deg`. No track: the line has zero length.
fn heading_end(x: f64, y: f64, track: Option<f64>, facing_deg: f64) -> (f64, f64) {
    match track {
        Some(t) => {
            let (s, c) = (t - facing_deg).to_radians().sin_cos();
            (x + HEADING_LINE_PX * s, y - HEADING_LINE_PX * c)
        }
        None => (x, y),
    }
}

/// "FL340 452" above the transition altitude, "A025 110" below it,
/// "GND" on the ground; speed in knots.
fn flight_info_label(alt_baro: Option<i32>, gs: Option<f64>) -> String {
    let alt = match alt_baro {
        Some(a) if a >= TRANSITION_ALTITUDE_FT => format!("FL{:03}", (a + 50) / 100),
        Some(a) => format!("A{:03}", (a.max(0) + 50) / 100),
        None => "GND".to_string(),
    };
    match gs {
        Some(speed) => format!("{} {}", alt, speed.round() as i64),
        None => alt,
    }
}

fn display_callsign(callsign: &str, hex: &str) -> String {
    if callsign.is_empty() {
        hex.to_uppercase()
    } else {
        callsign.to_string()
    }
}

/// N/E/S/W around the radar edge for the facing direction (top of screen).
fn compass_labels(facing_direction: &str) -> Vec<CompassLabel> {
    let facing_deg = direction_to_degrees(facing_direction);
    [("N", 0.0), ("E", 90.0), ("S", 180.0), ("W", 270.0)]
        .iter()
        .map(|&(text, bearing)| {
            let (s, c) = (bearing - facing_deg as f64).to_radians().sin_cos();
            CompassLabel {
                text: text.into(),
                x: (240.0 + 194.0 * s) as f32,
                y: (240.0 - 194.0 * c) as f32,
                primary: text == "N",
            }
        })
        .collect()
}

/// Range ring labels at 1/3, 2/3 and the full radius, e.g. "2", "4", "6 NM".
fn ring_labels(max_radius_nm: f64) -> Vec<slint::SharedString> {
    let fmt = |v: f64| {
        if (v - v.round()).abs() < 0.05 {
            format!("{}", v.round() as i64)
        } else {
            format!("{:.1}", v)
        }
    };
    vec![
        fmt(max_radius_nm / 3.0).into(),
        fmt(max_radius_nm * 2.0 / 3.0).into(),
        format!("{} NM", fmt(max_radius_nm)).into(),
    ]
}

fn model<T: Clone + 'static>(items: Vec<T>) -> slint::ModelRc<T> {
    slint::ModelRc::new(slint::VecModel::from(items))
}

/// Start or extend the alert for new flights, and keep its beacons moving
/// on every poll while it's up.
fn update_alert(
    ui: &App,
    state: &FlightState,
    facing_direction: &str,
    loc: Location,
    alert: &Arc<Mutex<AlertState>>,
    poll_interval: &Arc<AtomicU64>,
) {
    if state.has_new_flights {
        let generation = {
            let mut a = alert.lock().unwrap();
            if !ui.get_alert_active() {
                a.flights.clear();
            }
            for nf in &state.new_flights_with_info {
                if !a.flights.iter().any(|f| f.hex == nf.hex) {
                    a.flights.push(nf.clone());
                }
            }
            a.generation += 1;

            let cards: Vec<AlertFlightDetail> = a
                .flights
                .iter()
                .take(ALERT_MAX_CARDS)
                .map(|f| AlertFlightDetail {
                    callsign: f.callsign.clone().into(),
                    origin_location: f.origin_location.clone().into(),
                    origin_name: f.origin_name.clone().into(),
                    destination_location: f.destination_location.clone().into(),
                    destination_name: f.destination_name.clone().into(),
                    has_route: f.has_route,
                    aircraft_type: f.aircraft_type.clone().into(),
                })
                .collect();
            ui.set_alert_flight_details(std::rc::Rc::new(slint::VecModel::from(cards)).into());
            ui.set_alert_more_count(a.flights.len().saturating_sub(ALERT_MAX_CARDS) as i32);
            a.generation
        };

        ui.set_alert_active(true);
        poll_interval.store(POLL_ACTIVE, Ordering::Relaxed);

        // Auto-dismiss 30s after the latest new flight. Each new flight bumps
        // the generation, so earlier timers become no-ops.
        let ui_handle = ui.as_weak();
        let alert = alert.clone();
        let poll_interval = poll_interval.clone();
        slint::Timer::single_shot(ALERT_DURATION, move || {
            let Some(ui) = ui_handle.upgrade() else { return };
            let current = alert.lock().unwrap().generation == generation;
            if current && ui.get_alert_active() {
                end_alert(&ui, &alert, &poll_interval);
            }
        });
    }

    if !ui.get_alert_active() {
        return;
    }

    let beacons: Vec<AlertBeacon> = state
        .flights
        .iter()
        .take(ALERT_MAX_BEACONS)
        .map(|flight| {
            let rotation = calculate_plane_rotation(loc.lat, loc.lon, flight.lat, flight.lon, facing_direction);
            let distance_nm = calculate_distance(loc.lat, loc.lon, flight.lat, flight.lon);
            let distance_ratio = (distance_nm / loc.max_radius_nm).min(1.0);
            let (sx, sy) = polar_to_screen(rotation, distance_ratio * DISPLAY_MAX_RADIUS_PX);
            let (x, y) = (240.0 + sx, 240.0 + sy);
            let (hx, hy) = heading_end(x, y, flight.track, direction_to_degrees(facing_direction));
            AlertBeacon {
                hex: flight.hex.clone().into(),
                callsign: flight.callsign.clone().into(),
                x: x as f32,
                y: y as f32,
                hx: hx as f32,
                hy: hy as f32,
                is_new: state.new_flights.contains(&flight.hex),
            }
        })
        .collect();
    ui.set_alert_beacons(std::rc::Rc::new(slint::VecModel::from(beacons)).into());
}

fn end_alert(ui: &App, alert: &Arc<Mutex<AlertState>>, poll_interval: &Arc<AtomicU64>) {
    {
        let mut a = alert.lock().unwrap();
        a.flights.clear();
        a.generation += 1;
    }
    ui.set_alert_active(false);
    ui.set_alert_more_count(0);
    poll_interval.store(desired_poll_interval(ui), Ordering::Relaxed);
}
