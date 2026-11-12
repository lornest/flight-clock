#![allow(dead_code)]

mod config;
mod flight;
mod ipc;
mod qr;
mod settings_web;
mod ui_paths;
mod update;
mod utils;
mod watchdog;
mod web;
mod wifi;

use config::{Config, Location};
use flight::poller::{FlightState, NewFlightWithInfo, PollEvent, Poller};
use ipc::{FromWeb, ToWeb, WebLink};
use utils::geo::{calculate_distance, calculate_plane_rotation, direction_to_degrees, polar_to_screen};
use wifi::portal::PortalState;
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

/// Width of the QR codes on screen, in pixels (41 modules of 5 px)
const QR_PX: u32 = 205;
/// "Pair a phone": how long the QR code stays up if nobody scans it
const LINK_SHOWN: Duration = Duration::from_secs(3 * 60);
/// First-run setup: the QR code stays up, but its token is replaced this often
const LINK_ROTATE: Duration = Duration::from_secs(15 * 60);

/// Night mode "dim": how dark the picture goes (1.0 would be black)
const NIGHT_DIM: f32 = 0.8;
/// How long a touch keeps the screen awake during night hours
const NIGHT_WAKE: Duration = Duration::from_secs(60);

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
    /// When the last alert came up, for the minimum gap between alerts.
    last_started: Option<Instant>,
}

fn main() {
    // Subcommands (no UI): `--version`, `web`, `update <scheduled|now|check>`
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
        // The settings page and WiFi portal, as their own unprivileged
        // process (systemd/flight-clock-web.service)
        Some("web") => {
            eprintln!("flight-clock {} web server", update::VERSION);
            web::run(web_port());
            std::process::exit(1);
        }
        Some(other) if other.starts_with('-') || !other.is_empty() => {
            eprintln!("usage: flight-clock [--version | web | update <scheduled|now|check>]");
            std::process::exit(2);
        }
        _ => {}
    }

    eprintln!("flight-clock {}", update::VERSION);
    let config = Config::load(None);
    // Shared so the web settings page can move the clock without a restart
    let location = Arc::new(Mutex::new(config.location()));
    let facing_direction = Arc::new(Mutex::new(config.facing_direction.clone()));
    // The settings as last saved, for night mode and the alert filters
    let prefs = Arc::new(Mutex::new(config.clone()));
    // Night mode: the screen stays awake until this time after a touch
    let wake_until = Arc::new(Mutex::new(Instant::now()));
    set_backlight(true);
    let started_at = Instant::now();

    let ui = App::new().unwrap();
    ui.set_facing_direction(config.facing_direction.clone().into());
    ui.set_radar_compass(model(compass_labels(&config.facing_direction)));
    ui.set_radar_ring_labels(model(ring_labels(config.radius_nm)));
    ui.set_clock_face(normalise_face(&config.clock_face).into());
    ui.set_show_date(config.show_date);
    apply_colours(&ui, &config);
    ui.set_tour_step(if config.hints_seen { 0 } else { 1 });

    // Dev aid: FLIGHT_CLOCK_SCREEN=radar starts on the radar (e.g. for screenshots)
    let start_on_radar = std::env::var("FLIGHT_CLOCK_SCREEN").as_deref() == Ok("radar");
    if start_on_radar {
        ui.set_current_screen(1);
    }
    let poll_interval = Arc::new(AtomicU64::new(if start_on_radar { POLL_ACTIVE } else { POLL_IDLE }));
    let alert = Arc::new(Mutex::new(AlertState::default()));
    let trails = Arc::new(Mutex::new(Trails::new()));

    // --- Web server link ---
    // The settings page and WiFi setup portal run in their own unprivileged
    // process (`flight-clock web`). It asks this one to do things over a
    // socket; every request is checked here before it is acted on.
    let network_ready = Arc::new(AtomicBool::new(false));
    let portal_state = Arc::new(Mutex::new(PortalState::default()));
    let (wifi_tx, wifi_rx) = mpsc::channel::<WifiCommand>();
    let http_port = web_port();
    let web_address = web_address(http_port);
    ui.set_web_address(web_address.clone().into());
    if config.location().is_none() {
        ui.set_radar_error(format!("Set your location: {}", web_address).into());
    }
    let phone_link = PhoneLink { token: Arc::new(Mutex::new(None)), address: web_address.clone() };

    let web_link = {
        let ui_handle = ui.as_weak();
        let location = location.clone();
        let facing = facing_direction.clone();
        let phone_link = phone_link.clone();
        let wifi_tx = wifi_tx.clone();
        let portal_state = portal_state.clone();
        let prefs = prefs.clone();
        WebLink::listen(move |request| {
            let to_ui = |event: WebEvent| {
                let ui_handle = ui_handle.clone();
                let location = location.clone();
                let facing = facing.clone();
                let phone_link = phone_link.clone();
                let prefs = prefs.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_handle.upgrade() {
                        if let WebEvent::Applied(cfg) = &event {
                            *prefs.lock().unwrap() = cfg.clone();
                        }
                        apply_web_event(&ui, event, &location, &facing, &phone_link);
                    }
                });
            };
            handle_web_request(request, &to_ui, &wifi_tx, &portal_state)
        })
    };
    // Keep the web server's copy of the WiFi setup state, and of the token
    // in the QR code on screen, up to date (and send both again whenever it
    // reconnects)
    {
        let web_link = web_link.clone();
        let portal_state = portal_state.clone();
        let link_token = phone_link.token.clone();
        std::thread::spawn(move || {
            let mut sent = None;
            let mut sent_link = None;
            loop {
                let now = {
                    let portal = portal_state.lock().unwrap();
                    (web_link.connection(), portal.active, portal.networks.clone(), portal.message.clone())
                };
                if now.0 > 0 && sent.as_ref() != Some(&now) {
                    let update = ToWeb::Portal { active: now.1, networks: now.2.clone(), message: now.3.clone() };
                    if web_link.send(&update) {
                        sent = Some(now);
                    }
                }
                let link_now = (web_link.connection(), link_token.lock().unwrap().clone());
                if link_now.0 > 0 && sent_link.as_ref() != Some(&link_now) {
                    if web_link.send(&ToWeb::PairLink { token: link_now.1.clone() }) {
                        sent_link = Some(link_now);
                    }
                }
                std::thread::sleep(Duration::from_millis(250));
            }
        });
    }
    // On the Pi the web server is its own systemd unit. On a dev machine
    // (or with FLIGHT_CLOCK_WEB_IN_PROCESS=1) run it here on a thread; it
    // still goes through the socket, so the same code is exercised.
    if cfg!(not(target_os = "linux")) || std::env::var_os("FLIGHT_CLOCK_WEB_IN_PROCESS").is_some() {
        std::thread::spawn(move || web::run(http_port));
    }
    {
        let web_link = web_link.clone();
        let ui_handle = ui.as_weak();
        ui.on_pairing_cancelled(move || {
            web_link.send(&ToWeb::CancelCode);
            if let Some(ui) = ui_handle.upgrade() {
                ui.set_pairing_code("".into());
            }
        });
    }

    // --- Phone QR code: "Pair a phone" on the settings menu, and dismissing it ---
    {
        let ui_handle = ui.as_weak();
        let phone_link = phone_link.clone();
        ui.on_pair_phone_requested(move || {
            if let Some(ui) = ui_handle.upgrade() {
                ui.set_show_direction_selector(false);
                phone_link.show(&ui, false);
            }
        });
    }
    {
        let ui_handle = ui.as_weak();
        let phone_link = phone_link.clone();
        ui.on_link_dismissed(move || {
            let Some(ui) = ui_handle.upgrade() else { return };
            // "Later" on first-run setup: don't offer it again
            if ui.get_link_first_run() {
                save_setting(|cfg| cfg.setup_dismissed = true);
            }
            phone_link.hide(&ui);
        });
    }

    {
        let ui_handle = ui.as_weak();
        let network_ready = network_ready.clone();
        let phone_link = phone_link.clone();
        std::thread::spawn(move || {
            let supervisor = Supervisor::new(wifi_rx, portal_state, network_ready, move |state| {
                let ui_handle = ui_handle.clone();
                let phone_link = phone_link.clone();
                let _ = slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_handle.upgrade() {
                        apply_wifi_state(&ui, state, started_at, &phone_link);
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
        let prefs = prefs.clone();

        std::thread::spawn(move || {
            let poller = Poller::new(location.clone(), poll_interval.clone(), network_ready);
            poller.run(move |event| {
                let facing = facing.lock().unwrap().clone();
                // The poller only runs with a location set
                let Some(location) = *location.lock().unwrap() else { return };
                let ui_handle = ui_handle.clone();
                let alert = alert.clone();
                let trails = trails.clone();
                let poll_interval = poll_interval.clone();
                let prefs = prefs.lock().unwrap().clone();
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
                            update_alert(&ui, &state, &facing, location, &alert, &poll_interval, &prefs);
                        }
                    }
                });
            });
        });
    }

    // --- Clock update callback ---
    // The Pi has no clock of its own: until the time arrives from the network
    // the faces show none, rather than a confident wrong one.
    let time_synced = Arc::new(AtomicBool::new(time_is_synced()));
    if !time_synced.load(Ordering::Relaxed) {
        eprintln!("Waiting for the time from the network");
        let time_synced = time_synced.clone();
        std::thread::spawn(move || {
            while !time_is_synced() {
                std::thread::sleep(TIME_SYNC_CHECK_EVERY);
            }
            eprintln!("Time set from the network");
            time_synced.store(true, Ordering::Relaxed);
        });
    }

    let ui_handle_clock = ui.as_weak();
    let prefs_clock = prefs.clone();
    let wake_until_clock = wake_until.clone();
    ui.on_update_clock(move || {
        if let Some(ui) = ui_handle_clock.upgrade() {
            let now = chrono::Local::now();
            let h = chrono::Timelike::hour(&now) as f32;
            let m = chrono::Timelike::minute(&now) as f32;
            let s = chrono::Timelike::second(&now) as f32;

            let time_known = time_synced.load(Ordering::Relaxed);
            ui.set_time_known(time_known);

            // No night mode on a guess at the time
            let awake = Instant::now() < *wake_until_clock.lock().unwrap();
            let in_use = screen_in_use(&ui) || !time_known;
            let dim = night_dim(&prefs_clock.lock().unwrap(), h as u32 * 60 + m as u32, awake, in_use);
            set_night_dim(&ui, dim);

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
            ui.set_time_text(if time_known { now.format("%H:%M").to_string() } else { "--:--".to_string() }.into());
        }
    });

    // --- Night mode: a touch wakes the screen for a while ---
    {
        let ui_handle = ui.as_weak();
        let wake_until = wake_until.clone();
        ui.on_night_woken(move || {
            *wake_until.lock().unwrap() = Instant::now() + NIGHT_WAKE;
            if let Some(ui) = ui_handle.upgrade() {
                set_night_dim(&ui, 0.0);
            }
        });
    }

    // --- Guided tour: SKIP ---
    {
        let ui_handle = ui.as_weak();
        ui.on_tour_skipped(move || {
            if let Some(ui) = ui_handle.upgrade() {
                end_tour(&ui);
            }
        });
    }

    // --- Swipe between clock and radar ---
    {
        let ui_handle = ui.as_weak();
        let poll_interval = poll_interval.clone();
        ui.on_screen_swiped(move |screen| {
            if let Some(ui) = ui_handle.upgrade() {
                ui.set_current_screen(screen.clamp(0, 1));
                poll_interval.store(desired_poll_interval(&ui), Ordering::Relaxed);
                tour_did(&ui, if screen == 1 { TOUR_TO_RADAR } else { TOUR_TO_CLOCK });
            }
        });
    }

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
                    tour_did(&ui, TOUR_SETTINGS);
                } else {
                    let current = ui.get_current_screen();
                    if x < 0.0 && current == 1 {
                        ui.set_current_screen(0);
                        tour_did(&ui, TOUR_TO_CLOCK);
                    } else if x >= 0.0 && current == 0 {
                        ui.set_current_screen(1);
                        tour_did(&ui, TOUR_TO_RADAR);
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
            tour_did(&ui, TOUR_FACE);
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

/// Something the web server asked for, to be applied on the UI thread.
enum WebEvent {
    /// Show this pairing code on the screen.
    ShowCode(String),
    /// Hide the pairing code (paired, expired or too many attempts).
    HideCode,
    /// New settings were saved; apply them.
    Applied(Config),
    /// A phone paired with the QR code on screen.
    LinkUsed,
}

/// The QR code that opens the settings page on a phone, already paired.
/// It carries a one-time token made here; the web server is told the token
/// and pairs whichever phone presents it. Only touched on the UI thread,
/// apart from the thread that passes the token on.
#[derive(Clone)]
struct PhoneLink {
    /// The token in the code on screen, if one is showing.
    token: Arc<Mutex<Option<String>>>,
    /// The settings page address, e.g. "flight-clock.local".
    address: String,
}

impl PhoneLink {
    /// Put a new code on screen. For first-run setup it stays until it is
    /// used or dismissed, with a fresh token every `LINK_ROTATE`; otherwise
    /// it comes down by itself after `LINK_SHOWN`.
    fn show(&self, ui: &App, first_run: bool) {
        let token = utils::random::string(16, utils::random::READABLE);
        let url = format!("http://{}/s?k={}", self.address, token);
        ui.set_link_qr(qr::image(&url, QR_PX));
        ui.set_link_first_run(first_run);
        ui.set_link_active(true);
        *self.token.lock().unwrap() = Some(token.clone());

        let this = self.clone();
        let ui_handle = ui.as_weak();
        let lifetime = if first_run { LINK_ROTATE } else { LINK_SHOWN };
        slint::Timer::single_shot(lifetime, move || {
            let Some(ui) = ui_handle.upgrade() else { return };
            // Used, dismissed or replaced since: nothing to do
            let still_shown = this.token.lock().unwrap().as_deref() == Some(token.as_str());
            if !still_shown {
                return;
            }
            if first_run {
                this.show(&ui, true);
            } else {
                this.hide(&ui);
            }
        });
    }

    fn hide(&self, ui: &App) {
        *self.token.lock().unwrap() = None;
        ui.set_link_active(false);
        ui.set_link_qr(slint::Image::default());
    }

    /// First-run setup: once online, offer the code until the clock knows
    /// where it is (or the owner says "Later").
    fn offer_setup(&self, ui: &App) {
        if ui.get_link_active() {
            return;
        }
        let cfg = Config::load(None);
        if cfg.location().is_none() && !cfg.setup_dismissed {
            self.show(ui, true);
        }
    }
}

/// Act on a request from the web server process. It is unprivileged and
/// faces the network, so nothing it sends is trusted: codes, settings and
/// WiFi details are all checked here. Returns the answer, for requests that
/// have one.
fn handle_web_request(
    request: FromWeb,
    to_ui: &dyn Fn(WebEvent),
    wifi: &mpsc::Sender<WifiCommand>,
    portal: &Mutex<PortalState>,
) -> Option<ToWeb> {
    match request {
        FromWeb::ShowCode { code } => {
            if code.len() == 6 && code.bytes().all(|b| b.is_ascii_digit()) {
                to_ui(WebEvent::ShowCode(code));
            }
            None
        }
        FromWeb::HideCode => {
            to_ui(WebEvent::HideCode);
            None
        }
        FromWeb::SaveSettings { id, config } => {
            let saved = config.validate().and_then(|()| config.save(None));
            if saved.is_ok() {
                to_ui(WebEvent::Applied(config));
            }
            Some(ToWeb::Reply { id, error: saved.err() })
        }
        FromWeb::TriggerUpdate { id, install } => {
            let mode = if install { update::Mode::Now } else { update::Mode::Check };
            Some(ToWeb::Reply { id, error: update::trigger(mode).err() })
        }
        FromWeb::WifiConnect { ssid, password } => {
            // Only while the setup hotspot is up, and only plausible values
            // (an SSID is at most 32 bytes, a WPA password at most 63)
            let plausible = (1..=32).contains(&ssid.len())
                && password.len() <= 63
                && !ssid.chars().chain(password.chars()).any(char::is_control);
            if plausible && portal.lock().unwrap().active {
                let _ = wifi.send(WifiCommand::Connect { ssid, password });
            }
            None
        }
        FromWeb::PortalSeen => {
            let mut portal = portal.lock().unwrap();
            if portal.active {
                portal.last_request = Some(Instant::now());
            }
            None
        }
        FromWeb::LinkUsed => {
            to_ui(WebEvent::LinkUsed);
            None
        }
    }
}

/// Apply something the web settings page did.
fn apply_web_event(
    ui: &App,
    event: WebEvent,
    location: &Arc<Mutex<Option<Location>>>,
    facing: &Arc<Mutex<String>>,
    phone_link: &PhoneLink,
) {
    match event {
        // The phone has what it needs; take the code down. (First-run setup
        // offers it again at the next start if no location gets saved.)
        WebEvent::LinkUsed => phone_link.hide(ui),
        WebEvent::ShowCode(code) => {
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
        WebEvent::HideCode => ui.set_pairing_code("".into()),
        WebEvent::Applied(cfg) => {
            // A location has arrived: drop the "set your location" note
            if location.lock().unwrap().is_none() && cfg.location().is_some() {
                ui.set_radar_error("".into());
            }
            // First-run setup is done (or put off) from the phone
            if ui.get_link_active() && ui.get_link_first_run() && (cfg.location().is_some() || cfg.setup_dismissed) {
                phone_link.hide(ui);
            }
            *location.lock().unwrap() = cfg.location();
            *facing.lock().unwrap() = cfg.facing_direction.clone();
            ui.set_facing_direction(cfg.facing_direction.clone().into());
            ui.set_radar_compass(model(compass_labels(&cfg.facing_direction)));
            ui.set_radar_ring_labels(model(ring_labels(cfg.radius_nm)));
            ui.set_clock_face(normalise_face(&cfg.clock_face).into());
            ui.set_show_date(cfg.show_date);
            apply_colours(ui, &cfg);
            ui.invoke_update_clock();
        }
    }
}

/// Apply the chosen accent and background colours.
fn apply_colours(ui: &App, cfg: &Config) {
    let theme = ui.global::<Theme>();
    let (ground, surface, panel) = cfg.background_hex();
    let set = |hex: &str, f: &dyn Fn(slint::Color)| {
        if let Some(c) = parse_hex_colour(hex) {
            f(c)
        }
    };
    set(cfg.accent_hex(), &|c| theme.set_accent(c));
    set(ground, &|c| theme.set_bg(c));
    set(surface, &|c| theme.set_surface(c));
    set(panel, &|c| theme.set_panel(c));
}

/// "#RRGGBB" to a Slint colour.
fn parse_hex_colour(hex: &str) -> Option<slint::Color> {
    let hex = hex.strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let v = u32::from_str_radix(hex, 16).ok()?;
    Some(slint::Color::from_rgb_u8((v >> 16) as u8, (v >> 8) as u8, v as u8))
}

const TIME_SYNC_CHECK_EVERY: Duration = Duration::from_secs(5);

/// Has the system clock been set from the network since boot? Where that
/// can't be told (not Linux, no timedatectl, or network time switched off on
/// purpose) the answer is yes, so the clock is never blank for no reason.
fn time_is_synced() -> bool {
    if !cfg!(target_os = "linux") {
        return true;
    }
    // systemd-timesyncd leaves this file once it has synchronised
    if std::path::Path::new("/run/systemd/timesync/synchronized").exists() {
        return true;
    }
    match std::process::Command::new("timedatectl")
        .args(["show", "-p", "NTP", "-p", "NTPSynchronized"])
        .output()
    {
        Ok(out) if out.status.success() => synced_per_timedatectl(&String::from_utf8_lossy(&out.stdout)),
        _ => true,
    }
}

/// Reads `timedatectl show -p NTP -p NTPSynchronized`. Unsynced only when
/// network time is on and hasn't arrived yet.
fn synced_per_timedatectl(output: &str) -> bool {
    let value = |key: &str| output.lines().find_map(|l| l.trim().strip_prefix(key)).map(str::trim);
    !(value("NTP=") == Some("yes") && value("NTPSynchronized=") == Some("no"))
}

// Guided tour steps (the UI's `tour-step`): each is the gesture being asked for.
const TOUR_TO_RADAR: i32 = 1;
const TOUR_TO_CLOCK: i32 = 2;
const TOUR_SETTINGS: i32 = 3;
const TOUR_FACE: i32 = 4;
const TOUR_DONE: i32 = 5;
/// How long "That's it" stays up after the last step
const TOUR_DONE_SHOWN: Duration = Duration::from_secs(4);

/// The owner just made this gesture. If it's the one the tour is asking for,
/// move on; after the last, say so briefly and don't show the tour again.
fn tour_did(ui: &App, step: i32) {
    if ui.get_tour_step() != step {
        return;
    }
    ui.set_tour_step(step + 1);
    if step + 1 == TOUR_DONE {
        save_setting(|cfg| cfg.hints_seen = true);
        let ui_handle = ui.as_weak();
        slint::Timer::single_shot(TOUR_DONE_SHOWN, move || {
            if let Some(ui) = ui_handle.upgrade() {
                ui.set_tour_step(0);
            }
        });
    }
}

/// SKIP: the tour goes away for good.
fn end_tour(ui: &App) {
    ui.set_tour_step(0);
    save_setting(|cfg| cfg.hints_seen = true);
}

/// Something is on screen that shouldn't be dimmed or switched off under
/// whoever is using it: setup, a pairing or QR code, the settings menu, or an
/// alert (which only comes up at night if alerts are allowed then).
fn screen_in_use(ui: &App) -> bool {
    ui.get_starting_up()
        || ui.get_wifi_setup_mode()
        || ui.get_show_direction_selector()
        || ui.get_link_active()
        || ui.get_alert_active()
        || !ui.get_pairing_code().is_empty()
}

/// How dark night mode makes the picture right now: 0 for not at all, 1 for
/// black (and the backlight off).
fn night_dim(cfg: &Config, minute_of_day: u32, awake: bool, in_use: bool) -> f32 {
    if awake || in_use || !cfg.is_night(minute_of_day) {
        0.0
    } else if cfg.night_mode == "screen_off" {
        1.0
    } else {
        NIGHT_DIM
    }
}

fn set_night_dim(ui: &App, dim: f32) {
    if ui.get_night_dim() == dim {
        return;
    }
    ui.set_night_dim(dim);
    set_backlight(dim < 1.0);
}

/// Switch the display's backlight. The HyperPixel's is on/off only, which is
/// why "dim" darkens the picture instead. Does nothing where there is no
/// backlight to control (a dev machine).
fn set_backlight(on: bool) {
    let Ok(entries) = std::fs::read_dir("/sys/class/backlight") else { return };
    for entry in entries.flatten() {
        let path = entry.path().join("brightness");
        let max = std::fs::read_to_string(entry.path().join("max_brightness")).unwrap_or_default();
        let value = if on { max.trim() } else { "0" };
        if value.is_empty() {
            continue;
        }
        if let Err(e) = std::fs::write(&path, value) {
            eprintln!("Couldn't switch the backlight ({}): {}", path.display(), e);
        }
    }
}

/// The new flights worth an alert under the owner's settings: none with
/// alerts off, or at night unless allowed then; otherwise those at or below
/// the altitude limit (an aircraft with no altitude, i.e. on the ground,
/// counts as low).
fn flights_to_alert<'a>(cfg: &Config, state: &'a FlightState, night: bool) -> Vec<&'a NewFlightWithInfo> {
    if !cfg.alerts_enabled || (night && !cfg.night_alerts) {
        return Vec::new();
    }
    state
        .new_flights_with_info
        .iter()
        .filter(|new| match cfg.alert_max_altitude_ft {
            None => true,
            Some(max) => state
                .flights
                .iter()
                .find(|f| f.hex == new.hex)
                .and_then(|f| f.alt_baro)
                .is_none_or(|alt| alt <= max as i32),
        })
        .collect()
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
fn apply_wifi_state(ui: &App, state: WifiUiState, started_at: Instant, phone_link: &PhoneLink) {
    match state {
        WifiUiState::Connecting(message) => {
            ui.set_startup_status(message.clone().into());
            // After a cancel, the setup screen is still up while we rejoin.
            ui.set_wifi_status(message.into());
            ui.set_wifi_status_error(false);
            ui.set_wifi_can_cancel(false);
            set_hotspot_password(ui, "");
        }
        WifiUiState::Online { ssid } => {
            ui.set_wifi_setup_mode(false);
            ui.set_wifi_network(ssid.into());
            set_hotspot_password(ui, "");
            finish_startup(ui, started_at);
            phone_link.offer_setup(ui);
        }
        WifiUiState::Setup {
            status,
            is_error,
            can_cancel,
            password,
        } => {
            set_hotspot_password(ui, &password);
            ui.set_wifi_setup_mode(true);
            ui.set_wifi_status(status.into());
            ui.set_wifi_status_error(is_error);
            ui.set_wifi_can_cancel(can_cancel);
            ui.set_show_direction_selector(false);
            finish_startup(ui, started_at);
        }
    }
}

/// Show the setup network's password and the QR code that joins it, or
/// clear both when it isn't up.
fn set_hotspot_password(ui: &App, password: &str) {
    if ui.get_wifi_password() == password {
        return;
    }
    ui.set_wifi_password(password.into());
    ui.set_wifi_qr(if password.is_empty() {
        slint::Image::default()
    } else {
        qr::image(&qr::wifi_text(wifi::manager::AP_SSID, password), QR_PX)
    });
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
    prefs: &Config,
) {
    let now = chrono::Local::now();
    let minute_of_day = chrono::Timelike::hour(&now) * 60 + chrono::Timelike::minute(&now);
    let wanted = flights_to_alert(prefs, state, prefs.is_night(minute_of_day));
    // An alert already up takes new flights; a new one waits out the gap
    let gap = Duration::from_secs(prefs.alert_min_gap_min as u64 * 60);
    let gap_over = ui.get_alert_active() || alert.lock().unwrap().last_started.is_none_or(|t| t.elapsed() >= gap);

    if !wanted.is_empty() && gap_over {
        let generation = {
            let mut a = alert.lock().unwrap();
            if !ui.get_alert_active() {
                a.flights.clear();
                a.last_started = Some(Instant::now());
            }
            for nf in wanted {
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

#[cfg(test)]
mod tests {
    use super::*;
    use flight::types::SlimFlight;

    fn flight(hex: &str, alt: Option<i32>) -> SlimFlight {
        SlimFlight {
            hex: hex.into(),
            callsign: hex.to_uppercase(),
            lat: 0.0,
            lon: 0.0,
            alt_baro: alt,
            gs: None,
            track: None,
            baro_rate: None,
            aircraft_type: None,
        }
    }

    /// Three new flights: high, low, and on the ground (no altitude).
    fn state() -> FlightState {
        let flights = vec![flight("high", Some(35000)), flight("low", Some(2500)), flight("ground", None)];
        let new: Vec<NewFlightWithInfo> = flights.iter().map(|f| NewFlightWithInfo::from_parts(&f.hex, f, None)).collect();
        FlightState {
            new_flights: flights.iter().map(|f| f.hex.clone()).collect(),
            new_flights_with_info: new,
            flight_count: flights.len(),
            has_new_flights: true,
            flights,
        }
    }

    fn alerted(cfg: &Config, night: bool) -> Vec<String> {
        flights_to_alert(cfg, &state(), night).iter().map(|f| f.hex.clone()).collect()
    }

    #[test]
    fn alerts_follow_the_filters() {
        assert_eq!(alerted(&Config::default(), false), ["high", "low", "ground"]);
        assert!(alerted(&Config { alerts_enabled: false, ..Config::default() }, false).is_empty());

        let low_only = Config { alert_max_altitude_ft: Some(5000), ..Config::default() };
        assert_eq!(alerted(&low_only, false), ["low", "ground"]);
    }

    #[test]
    fn nights_are_quiet_unless_allowed() {
        assert!(alerted(&Config::default(), true).is_empty());
        let allowed = Config { night_alerts: true, alert_max_altitude_ft: Some(5000), ..Config::default() };
        assert_eq!(alerted(&allowed, true), ["low", "ground"]);
    }

    #[test]
    fn time_counts_as_unsynced_only_when_network_time_is_pending() {
        assert!(!synced_per_timedatectl("NTP=yes\nNTPSynchronized=no\n"));
        assert!(synced_per_timedatectl("NTP=yes\nNTPSynchronized=yes\n"));
        // Network time switched off: the owner sets the time some other way
        assert!(synced_per_timedatectl("NTP=no\nNTPSynchronized=no\n"));
        assert!(synced_per_timedatectl(""));
    }

    #[test]
    fn night_dims_only_an_idle_screen_in_night_hours() {
        let dim = Config { night_mode: "dim".into(), ..Config::default() }; // 22:00-07:00
        let off = Config { night_mode: "screen_off".into(), ..Config::default() };
        let (night, day) = (23 * 60, 12 * 60);

        assert_eq!(night_dim(&dim, night, false, false), NIGHT_DIM);
        assert_eq!(night_dim(&off, night, false, false), 1.0);
        assert_eq!(night_dim(&dim, day, false, false), 0.0);
        // Touched recently, or something on screen that's being used
        assert_eq!(night_dim(&off, night, true, false), 0.0);
        assert_eq!(night_dim(&off, night, false, true), 0.0);
        assert_eq!(night_dim(&Config::default(), night, false, false), 0.0);
    }
}
