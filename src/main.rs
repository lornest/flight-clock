#![allow(dead_code)]

mod config;
mod flight;
mod utils;
mod watchdog;
mod wifi;

use config::Config;
use flight::poller::{FlightState, NewFlightWithInfo, PollEvent, Poller};
use utils::geo::{calculate_distance, calculate_plane_rotation, polar_to_screen};
use wifi::portal::{Portal, PortalState};
use wifi::supervisor::{Supervisor, WifiCommand, WifiUiState};

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
const MIN_SPLASH: Duration = Duration::from_secs(3);

/// Max radius for radar display (240 * 0.85)
const DISPLAY_MAX_RADIUS_PX: f64 = 204.0;

/// Double-tap detection parameters
const DOUBLE_TAP_TIMEOUT_MS: u64 = 500;
const DOUBLE_TAP_DISTANCE: f64 = 50.0;
const CENTER_RADIUS: f64 = 72.0; // 480 * 0.15

/// Observer position and radar scale.
#[derive(Clone, Copy)]
struct Location {
    lat: f64,
    lon: f64,
    max_radius_nm: f64,
}

/// Flights collected by the alert currently on screen. Only touched on the
/// UI thread; the Mutex is just so it can travel in Send closures.
#[derive(Default)]
struct AlertState {
    /// Bumped on every new flight so older auto-dismiss timers do nothing.
    generation: u64,
    flights: Vec<NewFlightWithInfo>,
}

fn main() {
    let config = Config::load(None);
    let location = Location {
        lat: config.latitude,
        lon: config.longitude,
        max_radius_nm: config.radius_nm,
    };
    let facing_direction = Arc::new(Mutex::new(config.facing_direction.clone()));
    let started_at = Instant::now();

    let ui = App::new().unwrap();
    ui.set_facing_direction(config.facing_direction.clone().into());

    let poll_interval = Arc::new(AtomicU64::new(POLL_IDLE));
    let alert = Arc::new(Mutex::new(AlertState::default()));

    // --- WiFi ---
    // The portal thread lives for the whole run and only answers while the
    // setup hotspot is up. The supervisor decides online vs. setup.
    let network_ready = Arc::new(AtomicBool::new(false));
    let portal_state = Arc::new(Mutex::new(PortalState::default()));
    let (wifi_tx, wifi_rx) = mpsc::channel::<WifiCommand>();

    if cfg!(target_os = "linux") {
        let portal = Portal::new(portal_state.clone(), wifi_tx.clone());
        std::thread::spawn(move || portal.serve());
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
        let network_ready = network_ready.clone();
        let config = config.clone();

        std::thread::spawn(move || {
            let poller = Poller::new(config, poll_interval.clone(), network_ready);
            poller.run(move |event| {
                let facing = facing.lock().unwrap().clone();
                let ui_handle = ui_handle.clone();
                let alert = alert.clone();
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
                            update_radar(&ui, &state, &facing, location);
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
            ui.set_facing_direction(dir_str.into());
            ui.set_show_direction_selector(false);

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

fn update_radar(ui: &App, state: &FlightState, facing_direction: &str, loc: Location) {
    let mut radar_beacons: Vec<RadarBeacon> = Vec::new();

    for flight in &state.flights {
        let distance_nm = calculate_distance(loc.lat, loc.lon, flight.lat, flight.lon);
        if distance_nm > loc.max_radius_nm {
            continue;
        }

        let rotation = calculate_plane_rotation(loc.lat, loc.lon, flight.lat, flight.lon, facing_direction);
        let distance_ratio = (distance_nm / loc.max_radius_nm).min(1.0);
        let (sx, sy) = polar_to_screen(rotation, distance_ratio * DISPLAY_MAX_RADIUS_PX);

        radar_beacons.push(RadarBeacon {
            hex: flight.hex.clone().into(),
            callsign: flight.callsign.clone().into(),
            screen_x: sx as f32,
            screen_y: sy as f32,
            rotation: rotation as f32,
            distance_nm: distance_nm as f32,
            altitude: flight.alt_baro.map(|a| format!("{}ft", a)).unwrap_or_default().into(),
            speed: flight.gs.map(|s| format!("{}kts", s as i32)).unwrap_or_default().into(),
        });
    }

    ui.set_radar_beacons(std::rc::Rc::new(slint::VecModel::from(radar_beacons)).into());
    ui.set_flight_count(state.flight_count as i32);
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
            AlertBeacon {
                hex: flight.hex.clone().into(),
                callsign: flight.callsign.clone().into(),
                screen_x: sx as f32,
                screen_y: sy as f32,
                rotation: rotation as f32,
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
