#![allow(dead_code)]

mod config;
mod flight;
mod utils;
mod wifi;

use config::Config;
use flight::poller::{FlightState, Poller};
use utils::geo::{calculate_distance, calculate_plane_rotation, polar_to_screen};

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

slint::include_modules!();

/// Polling intervals in milliseconds
const POLL_IDLE: u64 = 30000; // Clock screen, no alert
const POLL_ACTIVE: u64 = 5000; // Alert active or radar screen

/// Alert auto-dismiss time
const ALERT_DURATION_MS: u64 = 30000;

/// Max radius for radar display (240 * 0.85)
const DISPLAY_MAX_RADIUS_PX: f64 = 204.0;

/// Double-tap detection parameters
const DOUBLE_TAP_TIMEOUT_MS: u64 = 500;
const DOUBLE_TAP_DISTANCE: f64 = 50.0;
const CENTER_RADIUS: f64 = 72.0; // 480 * 0.15

fn main() {
    let config = Config::load(None);
    let max_radius_nm = config.radius_nm;
    let facing_direction = Arc::new(Mutex::new(config.facing_direction.clone()));

    let ui = App::new().unwrap();

    // Set initial facing direction
    ui.set_facing_direction(config.facing_direction.clone().into());

    // --- WiFi Setup ---
    let _wifi = Arc::new(Mutex::new(wifi::manager::WifiManager::new()));
    let _wifi_connected = Arc::new(AtomicBool::new(false));

    // Check if we need WiFi setup (only on Linux/Pi)
    #[cfg(target_os = "linux")]
    {
        let wifi_lock = _wifi.lock().unwrap();
        if !wifi_lock.has_saved_network() {
            drop(wifi_lock);

            let mut wifi_lock = _wifi.lock().unwrap();
            if let Err(e) = wifi_lock.start_ap_mode() {
                eprintln!("Failed to start AP mode: {}", e);
            }
            drop(wifi_lock);

            let portal = wifi::portal::Portal::new(_wifi.clone(), _wifi_connected.clone());
            std::thread::spawn(move || portal.serve());

            ui.set_wifi_setup_mode(true);
            ui.set_wifi_status("Waiting for WiFi setup...".into());
        } else {
            drop(wifi_lock);

            // Try connecting with saved credentials
            let mut wifi_lock = _wifi.lock().unwrap();
            match wifi_lock.connect_saved() {
                Ok(true) => {
                    eprintln!("Connected to saved WiFi network");
                }
                Ok(false) => {
                    eprintln!("Saved WiFi connection failed, starting AP mode");
                    if let Err(e) = wifi_lock.start_ap_mode() {
                        eprintln!("Failed to start AP mode: {}", e);
                    }
                    drop(wifi_lock);

                    let portal = wifi::portal::Portal::new(_wifi.clone(), _wifi_connected.clone());
                    std::thread::spawn(move || portal.serve());

                    ui.set_wifi_setup_mode(true);
                    ui.set_wifi_status("Saved network unavailable. Reconnect via portal.".into());
                }
                Err(e) => {
                    eprintln!("WiFi error: {}", e);
                }
            }
        }
    }

    // --- Shared flight details for beacon tap lookup ---
    let flight_details_map: Arc<Mutex<HashMap<String, FlightDetail>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // --- Poll interval (shared with poller thread) ---
    let poll_interval = Arc::new(AtomicU64::new(POLL_IDLE));

    // --- Flight Poller (background thread) ---
    let ui_handle = ui.as_weak();
    let poll_interval_clone = poll_interval.clone();
    let facing_clone = facing_direction.clone();
    let config_clone = config.clone();

    // Clone wifi-related Arcs for the poller thread
    #[cfg(target_os = "linux")]
    let wifi_for_poller = _wifi.clone();
    #[cfg(target_os = "linux")]
    let wifi_connected_for_poller = _wifi_connected.clone();

    let details_map_for_poller = flight_details_map.clone();
    std::thread::spawn(move || {
        // On Linux, wait for WiFi connection before starting poller
        #[cfg(target_os = "linux")]
        {
            loop {
                if wifi_connected_for_poller.load(Ordering::Relaxed) {
                    break;
                }
                let wifi_lock = wifi_for_poller.lock().unwrap();
                if wifi_lock.is_connected() {
                    break;
                }
                drop(wifi_lock);
                std::thread::sleep(std::time::Duration::from_secs(1));
            }

            // Transition UI from WiFi setup to clock
            let ui_h = ui_handle.clone();
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_h.upgrade() {
                    ui.set_wifi_setup_mode(false);
                }
            });
        }

        let poller = Poller::new(config_clone, poll_interval_clone);

        poller.run(move |state: FlightState| {
            let facing = facing_clone.lock().unwrap().clone();
            let ui_h = ui_handle.clone();
            let max_nm = max_radius_nm;
            let details_map = details_map_for_poller.clone();

            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_h.upgrade() {
                    update_ui_with_flight_state(&ui, &state, &facing, max_nm, &details_map);
                }
            });
        });
    });

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
    let ui_handle_alert = ui.as_weak();
    let poll_interval_alert = poll_interval.clone();
    ui.on_alert_dismissed(move || {
        if let Some(ui) = ui_handle_alert.upgrade() {
            ui.set_alert_active(false);

            let interval = if ui.get_current_screen() == 1 {
                POLL_ACTIVE
            } else {
                POLL_IDLE
            };
            poll_interval_alert.store(interval, Ordering::Relaxed);
        }
    });

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
                        poll_interval_tap.store(POLL_IDLE, Ordering::Relaxed);
                    } else if x >= 0.0 && current == 0 {
                        ui.set_current_screen(1);
                        poll_interval_tap.store(POLL_ACTIVE, Ordering::Relaxed);
                    }
                }
            }
            last_tap_time.store(0, Ordering::Relaxed);
        } else {
            last_tap_time.store(now_ms, Ordering::Relaxed);
            *last_tap_x.lock().unwrap() = x;
            *last_tap_y.lock().unwrap() = y;
        }
    });

    // --- Radar beacon tap handler ---
    let ui_handle_radar = ui.as_weak();
    let details_map_for_tap = flight_details_map.clone();
    ui.on_radar_beacon_tapped(move |hex| {
        if let Some(ui) = ui_handle_radar.upgrade() {
            let hex_str: String = hex.into();
            if let Some(detail) = details_map_for_tap.lock().unwrap().get(&hex_str) {
                ui.set_selected_flight_detail(detail.clone());
            }
            ui.set_show_flight_detail(true);
        }
    });

    // --- Radar detail dismiss ---
    let ui_handle_detail = ui.as_weak();
    ui.on_radar_detail_dismissed(move || {
        if let Some(ui) = ui_handle_detail.upgrade() {
            ui.set_show_flight_detail(false);
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

    // Run the Slint event loop
    ui.run().unwrap();
}

/// Update the Slint UI with new flight data from the poller.
fn update_ui_with_flight_state(
    ui: &App,
    state: &FlightState,
    facing_direction: &str,
    max_radius_nm: f64,
    flight_details_map: &Arc<Mutex<HashMap<String, FlightDetail>>>,
) {
    let config = Config::load(None);
    let user_lat = config.latitude;
    let user_lon = config.longitude;

    // --- Build radar beacons and flight detail map ---
    let mut radar_beacons: Vec<RadarBeacon> = Vec::new();
    let mut details: HashMap<String, FlightDetail> = HashMap::new();

    for flight in &state.flights {
        let distance_nm = calculate_distance(user_lat, user_lon, flight.lat, flight.lon);

        if distance_nm > max_radius_nm {
            continue;
        }

        let rotation = calculate_plane_rotation(
            user_lat,
            user_lon,
            flight.lat,
            flight.lon,
            facing_direction,
        );

        let distance_ratio = (distance_nm / max_radius_nm).min(1.0);
        let screen_radius = distance_ratio * DISPLAY_MAX_RADIUS_PX;
        let (sx, sy) = polar_to_screen(rotation, screen_radius);

        radar_beacons.push(RadarBeacon {
            hex: flight.hex.clone().into(),
            callsign: flight.callsign.clone().into(),
            screen_x: sx as f32,
            screen_y: sy as f32,
            rotation: rotation as f32,
            distance_nm: distance_nm as f32,
        });

        // Build detail for every flight using cached route info
        let info = state
            .all_flights_with_info
            .iter()
            .find(|nfi| nfi.hex == flight.hex);

        let (origin, dest, has_route) = match info {
            Some(nfi) if nfi.has_route => {
                let o = format!("{} ({})", nfi.origin_name, nfi.origin_location);
                let d = format!("{} ({})", nfi.destination_name, nfi.destination_location);
                (o, d, true)
            }
            _ => (String::new(), String::new(), false),
        };

        let alt = flight
            .alt_baro
            .map(|a| format!("{} ft", a))
            .unwrap_or_default();
        let speed = flight
            .gs
            .map(|s| format!("{} kts", s as i32))
            .unwrap_or_default();
        let (vs_str, vs_dir) = match flight.baro_rate {
            Some(r) if r > 0 => (format!("{} ft/min", r.abs()), "\u{2197}".to_string()),
            Some(r) if r < 0 => (format!("{} ft/min", r.abs()), "\u{2198}".to_string()),
            Some(_) => ("0 ft/min".to_string(), "\u{2192}".to_string()),
            None => (String::new(), String::new()),
        };

        details.insert(
            flight.hex.clone(),
            FlightDetail {
                callsign: flight.callsign.clone().into(),
                hex: flight.hex.clone().into(),
                origin: origin.into(),
                destination: dest.into(),
                has_route,
                altitude: alt.into(),
                speed: speed.into(),
                vertical_speed: vs_str.into(),
                vs_direction: vs_dir.into(),
            },
        );
    }

    let radar_model = std::rc::Rc::new(slint::VecModel::from(radar_beacons));
    ui.set_radar_beacons(radar_model.into());
    ui.set_flight_count(state.flight_count as i32);

    // Update shared flight details map for beacon tap lookups
    *flight_details_map.lock().unwrap() = details;

    // --- Handle alert state ---
    if state.has_new_flights {
        ui.set_alert_active(true);

        // Build alert beacons (up to 8)
        let mut alert_beacons: Vec<AlertBeacon> = Vec::new();
        for flight in state.flights.iter().take(8) {
            let rotation = calculate_plane_rotation(
                user_lat,
                user_lon,
                flight.lat,
                flight.lon,
                facing_direction,
            );
            let distance_nm = calculate_distance(user_lat, user_lon, flight.lat, flight.lon);
            let distance_ratio = (distance_nm / max_radius_nm).min(1.0);
            let screen_radius = distance_ratio * DISPLAY_MAX_RADIUS_PX;
            let (sx, sy) = polar_to_screen(rotation, screen_radius);
            let is_new = state.new_flights.contains(&flight.hex);

            alert_beacons.push(AlertBeacon {
                hex: flight.hex.clone().into(),
                callsign: flight.callsign.clone().into(),
                screen_x: sx as f32,
                screen_y: sy as f32,
                rotation: rotation as f32,
                is_new,
            });
        }

        let alert_model = std::rc::Rc::new(slint::VecModel::from(alert_beacons));
        ui.set_alert_beacons(alert_model.into());

        // Build flight details (up to 2)
        let mut details: Vec<AlertFlightDetail> = Vec::new();
        for nfi in state.new_flights_with_info.iter().take(2) {
            details.push(AlertFlightDetail {
                callsign: nfi.callsign.clone().into(),
                origin_location: nfi.origin_location.clone().into(),
                origin_name: nfi.origin_name.clone().into(),
                destination_location: nfi.destination_location.clone().into(),
                destination_name: nfi.destination_name.clone().into(),
                has_route: nfi.has_route,
            });
        }

        let details_model = std::rc::Rc::new(slint::VecModel::from(details));
        ui.set_alert_flight_details(details_model.into());

        // Auto-dismiss alert after 30 seconds
        let ui_handle = ui.as_weak();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(ALERT_DURATION_MS));
            let _ = slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_handle.upgrade() {
                    ui.set_alert_active(false);
                }
            });
        });
    }
}
