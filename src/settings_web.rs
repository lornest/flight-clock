//! Settings page on the home network: http://<hostname>.local
//!
//! Served by the web server process (`flight-clock web`, see src/ipc.rs)
//! whenever the setup hotspot is *not* up. Changing settings needs a paired
//! phone:
//!
//! 1. The phone asks for a code; the clock shows a 6-digit code for 2 minutes.
//! 2. Entering it sets a long-lived session cookie (HttpOnly, SameSite=Strict),
//!    so the phone only pairs once. Sessions are kept in a file only the web
//!    server's user can read, so they survive restarts.
//! 3. Five wrong attempts cancel the code. Ten wrong codes in an hour, or six
//!    code requests in ten minutes, pause pairing altogether, so asking for
//!    code after code doesn't get anyone more guesses.
//!
//! A phone can also pair by scanning the QR code on the clock (first-run
//! setup, or "Pair a phone" on the clock's settings). The code holds a
//! one-time token the clock made; opening /s?k=<token> pairs the phone. Being
//! able to read the screen is the proof of presence either way.
//!
//! Until a location is set (and unless the owner chose "Later"), a paired
//! phone gets a short setup page instead of the full settings.
//!
//! Requests must be addressed to the clock itself (its name or an IP
//! address), and form posts must come from the clock's own pages. That stops
//! a web page elsewhere from driving the clock through a visitor's browser
//! (DNS rebinding, cross-site form posts).
//!
//! Location can be searched for: the *phone* asks postcodes.io (UK
//! postcodes) or OpenStreetMap's Nominatim (anything else); the clock itself
//! never contacts either.

use crate::config::{
    config_path, parse_hhmm, Config, ACCENTS, ALERT_ALTITUDE_MAX_FT, ALERT_ALTITUDE_MIN_FT, ALERT_GAPS_MIN, BACKGROUNDS,
    FACINGS, NIGHT_MODES, RADIUS_MAX_NM, RADIUS_MIN_NM,
};
use crate::ipc::Clock;
use crate::update;
use crate::wifi::portal::{html_escape, parse_form};
use crate::utils::random;
use std::collections::VecDeque;
use std::io::Read;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tiny_http::{Header, Request, Response};

const CODE_TTL: Duration = Duration::from_secs(120);
const MAX_CODE_ATTEMPTS: u8 = 5;
/// Wrong codes allowed in `FAILURE_WINDOW`, across all codes, before pairing pauses.
const MAX_FAILURES: usize = 10;
const FAILURE_WINDOW: Duration = Duration::from_secs(3600);
/// Code requests allowed in `START_WINDOW`.
const MAX_STARTS: usize = 6;
const START_WINDOW: Duration = Duration::from_secs(600);
const MAX_SESSIONS: usize = 20;
const MAX_BODY_BYTES: u64 = 8 * 1024;
const COOKIE_NAME: &str = "fc_session";
const COOKIE_MAX_AGE_SECS: u64 = 365 * 24 * 3600;

/// Suffixes a home router may give the clock's name. None is a public
/// top-level domain, so nobody outside the home can own such a name.
const LOCAL_SUFFIXES: [&str; 6] = [".local", ".lan", ".home", ".home.arpa", ".internal", ".localdomain"];

// Brand fonts, served to the phone so the page works without internet
const FONT_DISPLAY: &[u8] = include_bytes!("../assets/fonts/SpaceGrotesk-SemiBold.ttf");
const FONT_DISPLAY_MEDIUM: &[u8] = include_bytes!("../assets/fonts/SpaceGrotesk-Medium.ttf");
const FONT_MONO: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Medium.ttf");

pub struct SettingsWeb {
    state: Mutex<State>,
    clock: Arc<dyn Clock>,
    sessions_path: PathBuf,
    /// This clock's hostname, lower case (e.g. "flight-clock").
    hostname: String,
    /// The token in the QR code on the clock's screen, while one is showing.
    link_token: Arc<Mutex<Option<String>>>,
}

#[derive(Default)]
struct State {
    pairing: Option<Pairing>,
    sessions: Vec<String>,
    /// When recent wrong codes were entered, oldest first.
    failures: VecDeque<Instant>,
    /// When recent codes were asked for, oldest first.
    starts: VecDeque<Instant>,
}

struct Pairing {
    code: String,
    expires: Instant,
    attempts: u8,
}

impl SettingsWeb {
    pub fn new(clock: Arc<dyn Clock>, link_token: Arc<Mutex<Option<String>>>) -> Self {
        // Under systemd: the unit's StateDirectory (/var/lib/flight-clock-web).
        // On a dev machine: next to the config file.
        let sessions_dir = std::env::var_os("STATE_DIRECTORY")
            .map(PathBuf::from)
            .or_else(|| PathBuf::from(config_path()).parent().map(PathBuf::from))
            .unwrap_or_default();
        let sessions_path = sessions_dir.join("web-sessions.json");
        let sessions = std::fs::read_to_string(&sessions_path)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .unwrap_or_default();

        let hostname = std::fs::read_to_string("/etc/hostname")
            .map(|h| h.trim().to_ascii_lowercase())
            .unwrap_or_default();

        Self {
            state: Mutex::new(State { sessions, ..State::default() }),
            clock,
            sessions_path,
            hostname,
            link_token,
        }
    }

    /// The code shown on the clock was dismissed there (or the clock restarted).
    pub fn cancel_code(&self) {
        self.state.lock().unwrap().pairing = None;
    }

    pub fn handle(&self, mut request: Request) {
        let method = request.method().as_str().to_string();
        let (path, query) = match request.url().split_once('?') {
            Some((path, query)) => (path.to_string(), query.to_string()),
            None => (request.url().to_string(), String::new()),
        };

        let host = header(&request, "Host");
        if !host_allowed(host.as_deref(), &self.hostname) {
            let name = if self.hostname.is_empty() { "the clock's name".to_string() } else { format!("{}.local", self.hostname) };
            let msg = format!("Open the clock's settings at http://{} or at its IP address.", name);
            let _ = request.respond(Response::from_string(msg).with_status_code(403));
            return;
        }
        if method == "POST" && !same_origin(header(&request, "Origin").as_deref(), host.as_deref()) {
            let _ = request.respond(Response::from_string("Forms must be sent from the clock's own page.").with_status_code(403));
            return;
        }

        match (method.as_str(), path.as_str()) {
            ("GET", font_path) if font(font_path).is_some() => {
                respond_font(request, font(font_path).unwrap_or_default())
            }

            ("GET", "/") => {
                let html = if self.is_authed(&request) {
                    let cfg = Config::load(None);
                    if needs_setup(&cfg) { render_setup(&cfg, None) } else { render_settings(&cfg, None) }
                } else {
                    self.render_pairing(None)
                };
                respond_html(request, html);
            }

            // The QR code on the clock: http://<clock>/s?k=<one-time token>
            ("GET", "/s") => {
                let key = field(&parse_form(&query), "k");
                match self.use_link(&key) {
                    Some(token) => {
                        self.clock.link_used();
                        respond_paired(request, &token);
                    }
                    // Already paired (e.g. the page was reloaded): carry on
                    None if self.is_authed(&request) => redirect(request, "/"),
                    None => {
                        let html = self.render_pairing(Some(
                            "That QR code is no longer valid. Scan the one on the clock again, or pair with a code.",
                        ));
                        respond_html(request, html);
                    }
                }
            }

            ("GET", "/setup") => {
                if !self.is_authed(&request) {
                    return redirect(request, "/");
                }
                respond_html(request, render_setup(&Config::load(None), None));
            }

            ("POST", "/setup") | ("POST", "/setup/later") => {
                if !self.is_authed(&request) {
                    return redirect(request, "/");
                }
                let form = read_form(&mut request);
                let current = Config::load(None);
                let updated = if path == "/setup/later" {
                    Ok(Config { setup_dismissed: true, ..current.clone() })
                } else {
                    apply_place(&current, &form)
                };
                let saved = updated.and_then(|cfg| self.clock.save_settings(&cfg).map(|()| cfg));
                match saved {
                    Ok(cfg) if path == "/setup/later" => respond_html(request, render_settings(&cfg, None)),
                    Ok(cfg) => respond_html(request, render_settings(&cfg, Some(Banner::Saved))),
                    Err(msg) => respond_html(request, render_setup(&current, Some(&msg))),
                }
            }

            ("POST", "/pair/start") => {
                let shown = match self.start_pairing(Instant::now()) {
                    // A code that can't be shown is no use to anyone
                    Ok(code) => self.clock.show_code(&code).inspect_err(|_| self.cancel_code()),
                    // Refused: a code already on the clock stays valid
                    Err(wait) => Err(format!("Too many attempts. Pairing is paused for {}.", minutes(wait))),
                };
                match shown {
                    Ok(()) => redirect(request, "/"),
                    Err(msg) => {
                        let html = self.render_pairing(Some(&msg));
                        respond_html(request, html);
                    }
                }
            }

            ("POST", "/pair") => {
                let form = read_form(&mut request);
                let entered: String = field(&form, "code").chars().filter(|c| c.is_ascii_digit()).collect();
                match self.check_code(&entered, Instant::now()) {
                    CodeResult::Ok(token) => {
                        self.clock.hide_code();
                        respond_paired(request, &token);
                    }
                    CodeResult::Wrong => {
                        let html = self.render_pairing(Some("That code didn't match. Check the clock and try again."));
                        respond_html(request, html);
                    }
                    CodeResult::Locked => {
                        self.clock.hide_code();
                        let html = self.render_pairing(Some("Too many wrong codes. Ask the clock for a new one."));
                        respond_html(request, html);
                    }
                    CodeResult::Paused(wait) => {
                        self.clock.hide_code();
                        let msg = format!("Too many wrong codes. Pairing is paused for {}.", minutes(wait));
                        let html = self.render_pairing(Some(&msg));
                        respond_html(request, html);
                    }
                    CodeResult::NoCode => {
                        let html = self.render_pairing(Some("That code has expired. Ask the clock for a new one."));
                        respond_html(request, html);
                    }
                }
            }

            ("POST", "/settings") => {
                if !self.is_authed(&request) {
                    return redirect(request, "/");
                }
                let form = read_form(&mut request);
                let current = Config::load(None);
                match apply_form(&current, &form) {
                    // The clock checks the settings again, saves and applies them
                    Ok(updated) => match self.clock.save_settings(&updated) {
                        Ok(()) => respond_html(request, render_settings(&updated, Some(Banner::Saved))),
                        Err(e) => {
                            eprintln!("Settings page: save failed: {}", e);
                            let msg = format!("Couldn't save settings: {}", e);
                            respond_html(request, render_settings(&current, Some(Banner::Error(msg))));
                        }
                    },
                    Err(msg) => respond_html(request, render_settings(&current, Some(Banner::Error(msg)))),
                }
            }

            // Polled by the settings page so the Updates card keeps up with
            // the updater without a reload
            ("GET", "/update/status") => {
                if !self.is_authed(&request) {
                    let _ = request.respond(Response::from_string("Not paired").with_status_code(403));
                    return;
                }
                respond_json(request, update_status_json(update::read_status().as_ref()));
            }

            ("POST", "/update/check") | ("POST", "/update/install") => {
                if !self.is_authed(&request) {
                    return redirect(request, "/");
                }
                let install = path == "/update/install";
                let banner = match self.clock.trigger_update(install) {
                    Ok(()) if install => Banner::Info(
                        "Updating. The clock restarts when it's done, which takes a few minutes; the result will show under Updates.".into(),
                    ),
                    Ok(()) => Banner::Info("Checking for updates...".into()),
                    Err(e) => Banner::Error(format!("Couldn't start the updater: {}", e)),
                };
                respond_html(request, render_settings(&Config::load(None), Some(banner)));
            }

            ("POST", "/forget") | ("POST", "/forget-all") => {
                if path == "/forget-all" {
                    // Every paired phone, this one included
                    if !self.is_authed(&request) {
                        return redirect(request, "/");
                    }
                    let mut state = self.state.lock().unwrap();
                    state.sessions.clear();
                    self.save_sessions(&state.sessions);
                } else if let Some(token) = session_token(&request) {
                    let mut state = self.state.lock().unwrap();
                    state.sessions.retain(|t| t != &token);
                    self.save_sessions(&state.sessions);
                }
                let cookie = format!("{}=; Max-Age=0; Path=/; HttpOnly; SameSite=Strict", COOKIE_NAME);
                let header = Header::from_bytes(&b"Set-Cookie"[..], cookie.as_bytes()).unwrap();
                let location = Header::from_bytes(&b"Location"[..], &b"/"[..]).unwrap();
                let _ = request.respond(Response::empty(303).with_header(header).with_header(location));
            }

            _ => {
                let _ = request.respond(Response::from_string("Not found").with_status_code(404));
            }
        }
    }

    fn is_authed(&self, request: &Request) -> bool {
        match session_token(request) {
            Some(token) => self.state.lock().unwrap().sessions.iter().any(|t| same_bytes(t, &token)),
            None => false,
        }
    }

    /// Make a new pairing code, or say how long pairing is paused for.
    fn start_pairing(&self, now: Instant) -> Result<String, Duration> {
        let mut state = self.state.lock().unwrap();
        if let Some(wait) = state.paused_for(now) {
            return Err(wait);
        }
        if state.starts.len() >= MAX_STARTS {
            // Pruned by paused_for, so the oldest is still inside the window
            let oldest = state.starts.front().copied().unwrap_or(now);
            return Err((oldest + START_WINDOW).saturating_duration_since(now));
        }
        state.starts.push_back(now);

        let code = format!("{:06}", random::below(1_000_000));
        state.pairing = Some(Pairing {
            code: code.clone(),
            expires: now + CODE_TTL,
            attempts: 0,
        });
        Ok(code)
    }

    fn check_code(&self, entered: &str, now: Instant) -> CodeResult {
        let mut state = self.state.lock().unwrap();
        if let Some(wait) = state.paused_for(now) {
            state.pairing = None;
            return CodeResult::Paused(wait);
        }
        let Some(pairing) = state.pairing.as_mut() else {
            return CodeResult::NoCode;
        };
        if now > pairing.expires {
            state.pairing = None;
            return CodeResult::NoCode;
        }
        if same_bytes(entered, &pairing.code) {
            state.pairing = None;
            return CodeResult::Ok(self.new_session(&mut state));
        }
        pairing.attempts += 1;
        let code_used_up = pairing.attempts >= MAX_CODE_ATTEMPTS;
        state.failures.push_back(now);
        if let Some(wait) = state.paused_for(now) {
            state.pairing = None;
            return CodeResult::Paused(wait);
        }
        if code_used_up {
            state.pairing = None;
            return CodeResult::Locked;
        }
        CodeResult::Wrong
    }

    /// Pair with the token from the QR code on the clock. It works once: the
    /// clock makes a new one if it still wants a code on screen. Returns the
    /// new session.
    fn use_link(&self, key: &str) -> Option<String> {
        {
            let mut link = self.link_token.lock().unwrap();
            let matches = link.as_deref().is_some_and(|token| !token.is_empty() && same_bytes(key, token));
            if !matches {
                return None;
            }
            *link = None;
        }
        Some(self.new_session(&mut self.state.lock().unwrap()))
    }

    /// Remember a newly paired phone; returns its session token.
    fn new_session(&self, state: &mut State) -> String {
        let token = random::hex(16);
        state.sessions.push(token.clone());
        let excess = state.sessions.len().saturating_sub(MAX_SESSIONS);
        state.sessions.drain(..excess);
        self.save_sessions(&state.sessions);
        token
    }

    /// Replace the sessions file. It is created readable only by this user,
    /// and swapped in whole so a power cut can't leave it half-written.
    fn save_sessions(&self, sessions: &[String]) {
        let json = serde_json::to_string(sessions).unwrap_or_else(|_| "[]".into());
        let tmp = self.sessions_path.with_extension("json.tmp");
        let written = (|| -> std::io::Result<()> {
            use std::io::Write;
            let _ = std::fs::remove_file(&tmp);
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            let mut file = options.open(&tmp)?;
            file.write_all(json.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&tmp, &self.sessions_path)
        })();
        if let Err(e) = written {
            let _ = std::fs::remove_file(&tmp);
            eprintln!("Settings page: couldn't save sessions to {}: {}", self.sessions_path.display(), e);
        }
    }

    fn render_pairing(&self, error: Option<&str>) -> String {
        let code_active = self
            .state
            .lock()
            .unwrap()
            .pairing
            .as_ref()
            .is_some_and(|p| Instant::now() <= p.expires);

        let banner = error
            .map(|e| format!(r#"<p class="banner error">{}</p>"#, html_escape(e)))
            .unwrap_or_default();

        let body = if code_active {
            format!(
                r#"{banner}
<section class="card">
  <p class="lead">Enter the 6-digit code shown on the clock.</p>
  <form method="POST" action="/pair" class="stack">
    <label class="label" for="code">Code</label>
    <input id="code" name="code" inputmode="numeric" autocomplete="one-time-code" pattern="[0-9 ]*" maxlength="7" class="code-input" autofocus required>
    <button type="submit" class="primary">Pair this phone</button>
  </form>
  <form method="POST" action="/pair/start"><button type="submit" class="link">Show a new code</button></form>
</section>"#
            )
        } else {
            format!(
                r#"{banner}
<section class="card">
  <p class="lead">To change settings, pair this phone with the clock. You'll only need to do this once.</p>
  <form method="POST" action="/pair/start"><button type="submit" class="primary">Show code on the clock</button></form>
  <p class="hint">Or scan instead: double-tap the middle of the clock, tap Pair a phone, and point this phone's camera at the screen.</p>
</section>"#
            )
        };

        page("Pair", &body, &Config::load(None))
    }
}

impl State {
    /// Forget attempts that have aged out, then: is pairing paused, and for
    /// how much longer? It pauses after `MAX_FAILURES` wrong codes in
    /// `FAILURE_WINDOW`, until the oldest of them is that old.
    fn paused_for(&mut self, now: Instant) -> Option<Duration> {
        while self.failures.front().is_some_and(|t| now.saturating_duration_since(*t) >= FAILURE_WINDOW) {
            self.failures.pop_front();
        }
        while self.starts.front().is_some_and(|t| now.saturating_duration_since(*t) >= START_WINDOW) {
            self.starts.pop_front();
        }
        if self.failures.len() < MAX_FAILURES {
            return None;
        }
        let oldest = self.failures[self.failures.len() - MAX_FAILURES];
        Some((oldest + FAILURE_WINDOW).saturating_duration_since(now))
    }
}

enum CodeResult {
    Ok(String),
    Wrong,
    /// This code has had its five attempts; a new one can be asked for.
    Locked,
    /// Too many wrong codes overall; no pairing for this long.
    Paused(Duration),
    NoCode,
}

/// "1 minute", "12 minutes": a wait rounded up to whole minutes.
fn minutes(wait: Duration) -> String {
    match wait.as_secs().div_ceil(60).max(1) {
        1 => "1 minute".to_string(),
        n => format!("{} minutes", n),
    }
}

/// Compare without stopping at the first difference, so how long a wrong
/// code or session token takes to reject says nothing about how close it was.
fn same_bytes(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Is this Host header one of the clock's own addresses? An IP address
/// always is. A name must be the clock's hostname, alone or with a home
/// network suffix ("flight-clock.local"), or avahi's renamed form
/// ("flight-clock-2.local"). Anything else means the browser thinks it is
/// talking to some other site: a DNS rebinding attack looks like that.
fn host_allowed(host: Option<&str>, hostname: &str) -> bool {
    let Some(host) = host else { return false };
    let host = host.trim().to_ascii_lowercase();

    // "[::1]:8080" -> "::1"
    if let Some(rest) = host.strip_prefix('[') {
        return rest.split_once(']').is_some_and(|(ip, _)| ip.parse::<std::net::Ipv6Addr>().is_ok());
    }
    // "name:8080" -> "name"
    let name = match host.rsplit_once(':') {
        Some((name, port)) if !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()) => name,
        _ => host.as_str(),
    };
    let name = name.strip_suffix('.').unwrap_or(name);

    if name.parse::<std::net::Ipv4Addr>().is_ok() || name == "localhost" {
        return true;
    }
    if hostname.is_empty() {
        return false;
    }
    let label = LOCAL_SUFFIXES
        .iter()
        .find_map(|suffix| name.strip_suffix(suffix))
        .unwrap_or(name);
    if label == hostname {
        return true;
    }
    // avahi adds "-2", "-3"... when it thinks the name is taken
    label
        .strip_prefix(hostname)
        .and_then(|rest| rest.strip_prefix('-'))
        .is_some_and(|n| !n.is_empty() && n.len() <= 3 && n.bytes().all(|b| b.is_ascii_digit()))
}

/// For a form post: does the page that sent it live at this address?
/// Browsers send Origin with cross-site posts; no Origin at all (curl, very
/// old browsers) is let through, since such a request carries no session
/// cookie from another site anyway.
fn same_origin(origin: Option<&str>, host: Option<&str>) -> bool {
    let Some(origin) = origin else { return true };
    let Some(host) = host else { return false };
    let origin_host = origin.trim().strip_prefix("http://").unwrap_or("");
    !origin_host.is_empty() && origin_host.eq_ignore_ascii_case(host.trim())
}

fn header(request: &Request, name: &'static str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|h| h.field.equiv(name))
        .map(|h| h.value.as_str().to_string())
}

enum Banner {
    Saved,
    Info(String),
    Error(String),
}

/// First-run setup is still owed: no location, and the owner hasn't said "Later".
fn needs_setup(cfg: &Config) -> bool {
    cfg.location().is_none() && !cfg.setup_dismissed
}

/// Validate where the clock is and which way it faces (the setup page's
/// fields, and part of the settings form) and return the updated config.
fn apply_place(current: &Config, form: &[(String, String)]) -> Result<Config, String> {
    let number = |name: &str, label: &str| -> Result<f64, String> {
        field(form, name)
            .trim()
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .ok_or_else(|| format!("{} must be a number.", label))
    };

    let lat = number("latitude", "Latitude")?;
    let lon = number("longitude", "Longitude")?;
    let radius = number("radius_nm", "Radius")?;
    if !(-90.0..=90.0).contains(&lat) {
        return Err("Latitude must be between -90 and 90.".into());
    }
    if !(-180.0..=180.0).contains(&lon) {
        return Err("Longitude must be between -180 and 180.".into());
    }
    if !(RADIUS_MIN_NM..=RADIUS_MAX_NM).contains(&radius) {
        return Err(format!("Radius must be between {} and {} NM.", RADIUS_MIN_NM, RADIUS_MAX_NM));
    }

    let facing = field(form, "facing_direction");
    if !FACINGS.contains(&facing.as_str()) {
        return Err("Pick a facing direction.".into());
    }

    let mut updated = current.clone();
    updated.latitude = Some((lat * 1e6).round() / 1e6);
    updated.longitude = Some((lon * 1e6).round() / 1e6);
    updated.radius_nm = (radius * 10.0).round() / 10.0;
    updated.facing_direction = facing;
    Ok(updated)
}

/// Validate the submitted settings form and return the updated config.
fn apply_form(current: &Config, form: &[(String, String)]) -> Result<Config, String> {
    let mut updated = apply_place(current, form)?;

    let face = field(form, "clock_face");
    if face != "analog" && face != "digital" {
        return Err("Pick a clock face.".into());
    }

    let channel = field(form, "update_channel");
    if channel != "stable" && channel != "beta" {
        return Err("Pick an update channel.".into());
    }

    // Missing (e.g. a page loaded before colours existed): keep the current one
    let accent = field(form, "accent");
    let accent = if accent.is_empty() {
        // An id from a future or hand-edited config: back to the default
        let known = ACCENTS.iter().any(|(id, _, _)| *id == current.accent);
        if known { current.accent.clone() } else { ACCENTS[0].0.to_string() }
    } else if ACCENTS.iter().any(|(id, _, _)| *id == accent) {
        accent
    } else {
        return Err("Pick a colour.".into());
    };

    let background = field(form, "background");
    let background = if background.is_empty() {
        let known = BACKGROUNDS.iter().any(|(id, ..)| *id == current.background);
        if known { current.background.clone() } else { BACKGROUNDS[0].0.to_string() }
    } else if BACKGROUNDS.iter().any(|(id, ..)| *id == background) {
        background
    } else {
        return Err("Pick a background.".into());
    };

    // Night mode and alert filters. A page loaded before these existed
    // doesn't send the marker, and leaves them as they are.
    if field(form, "has_night_and_alerts") == "1" {
        let night_mode = field(form, "night_mode");
        if !NIGHT_MODES.iter().any(|(id, _)| *id == night_mode) {
            return Err("Pick a night mode.".into());
        }
        let (start, end) = (field(form, "night_start"), field(form, "night_end"));
        if parse_hhmm(&start).is_none() || parse_hhmm(&end).is_none() {
            return Err("Night hours must be times like 22:00.".into());
        }
        let altitude = field(form, "alert_max_altitude_ft");
        let altitude = match altitude.trim() {
            "" => None,
            text => Some(
                text.parse::<u32>()
                    .ok()
                    .filter(|ft| (ALERT_ALTITUDE_MIN_FT..=ALERT_ALTITUDE_MAX_FT).contains(ft))
                    .ok_or_else(|| {
                        format!(
                            "The alert altitude must be between {} and {} feet, or empty for any.",
                            ALERT_ALTITUDE_MIN_FT, ALERT_ALTITUDE_MAX_FT
                        )
                    })?,
            ),
        };
        let gap = field(form, "alert_min_gap_min")
            .parse::<u32>()
            .ok()
            .filter(|gap| ALERT_GAPS_MIN.contains(gap))
            .ok_or("Pick a gap between alerts.")?;

        updated.night_mode = night_mode;
        updated.night_start = start;
        updated.night_end = end;
        updated.night_alerts = form.iter().any(|(k, _)| k == "night_alerts");
        updated.alerts_enabled = form.iter().any(|(k, _)| k == "alerts_enabled");
        updated.alert_max_altitude_ft = altitude;
        updated.alert_min_gap_min = gap;
    }

    updated.accent = accent;
    updated.background = background;
    updated.update_channel = channel;
    updated.auto_update = form.iter().any(|(k, _)| k == "auto_update");
    updated.clock_face = face;
    // Unchecked checkboxes aren't submitted
    updated.show_date = form.iter().any(|(k, _)| k == "show_date");
    Ok(updated)
}

fn render_settings(cfg: &Config, banner: Option<Banner>) -> String {
    let status = update::read_status();
    // After "Check now" or "Update now", look for the result often for a while
    let watch = matches!(banner, Some(Banner::Info(_)));
    let banner = match banner {
        Some(Banner::Saved) => r#"<p class="banner ok">Saved. The clock has updated.</p>"#.to_string(),
        Some(Banner::Info(msg)) => format!(r#"<p class="banner info">{}</p>"#, html_escape(&msg)),
        Some(Banner::Error(msg)) => format!(r#"<p class="banner error">{}</p>"#, html_escape(&msg)),
        None => String::new(),
    };
    let updates = format!(
        r#"<div id="update-status" data-busy="{busy}" data-watch="{watch}">{html}</div>"#,
        busy = update_busy(status.as_ref()) as u8,
        watch = watch as u8,
        html = render_update_status(status.as_ref()),
    );

    let face_checked = |f: &str| if cfg.clock_face == f { " checked" } else { "" };

    let accent_options: String = ACCENTS
        .iter()
        .map(|(id, name, hex)| {
            format!(
                r#"<label class="swatch"><input type="radio" name="accent" value="{id}" data-hex="{hex}"{c}><span><i style="--c:{hex}"></i>{name}</span></label>"#,
                c = if cfg.accent_hex() == *hex { " checked" } else { "" }
            )
        })
        .collect();

    let background_options: String = BACKGROUNDS
        .iter()
        .map(|(id, name, bg, surface, _)| {
            format!(
                r#"<label><input type="radio" name="background" value="{id}" data-bg="{bg}" data-surface="{surface}"{c}><span><i class="dot" style="--c:{bg}"></i>{name}</span></label>"#,
                c = if cfg.background_hex().0 == *bg { " checked" } else { "" }
            )
        })
        .collect();

    let night_options: String = NIGHT_MODES
        .iter()
        .map(|(id, name)| {
            format!(
                r#"<label><input type="radio" name="night_mode" value="{id}"{c}><span>{name}</span></label>"#,
                c = if cfg.night_mode == *id { " checked" } else { "" }
            )
        })
        .collect();
    let gap_options: String = ALERT_GAPS_MIN
        .iter()
        .map(|gap| {
            let label = match gap {
                0 => "No limit".to_string(),
                1 => "1 minute".to_string(),
                n => format!("{} minutes", n),
            };
            // A gap from a hand-edited config that isn't a listed choice shows as the first
            format!(
                r#"<option value="{gap}"{c}>{label}</option>"#,
                c = if cfg.alert_min_gap_min == *gap { " selected" } else { "" }
            )
        })
        .collect();

    let body = format!(
        r#"{banner}
<form method="POST" action="/settings" class="stack">
  {place_cards}

  <section class="card">
    <h2 class="label">Alerts</h2>
    <input type="hidden" name="has_night_and_alerts" value="1">
    <label class="switch"><input type="checkbox" name="alerts_enabled"{alerts}><span class="track"></span><span>Show new flights full screen</span></label>
    <div class="field"><label class="label" for="alert_max_altitude_ft">Only below (feet)</label>
      <input id="alert_max_altitude_ft" name="alert_max_altitude_ft" type="number" inputmode="numeric" min="{alt_min}" max="{alt_max}" step="500" value="{altitude}" placeholder="Any altitude"></div>
    <div class="field"><label class="label" for="alert_min_gap_min">At most one alert every</label>
      <select id="alert_min_gap_min" name="alert_min_gap_min">{gap_options}</select></div>
    <p class="hint">Flights always show on the radar.</p>
  </section>

  <section class="card">
    <h2 class="label">Night</h2>
    <div class="segmented three">{night_options}</div>
    <div class="row">
      <div class="field"><label class="label" for="night_start">From</label>
        <input id="night_start" name="night_start" type="time" value="{night_start}" required></div>
      <div class="field"><label class="label" for="night_end">Until</label>
        <input id="night_end" name="night_end" type="time" value="{night_end}" required></div>
    </div>
    <label class="switch"><input type="checkbox" name="night_alerts"{night_alerts}><span class="track"></span><span>Alerts at night</span></label>
    <p class="hint">Touch the screen to wake it for a minute.</p>
  </section>

  <section class="card">
    <h2 class="label">Clock</h2>
    <div class="segmented">
      <label><input type="radio" name="clock_face" value="analog"{analog}><span>Analogue</span></label>
      <label><input type="radio" name="clock_face" value="digital"{digital}><span>Digital</span></label>
    </div>
    <label class="switch"><input type="checkbox" name="show_date"{date}><span class="track"></span><span>Show the date</span></label>
  </section>

  <section class="card">
    <h2 class="label">Colour</h2>
    <div class="swatches">{accent_options}</div>
    <h2 class="label">Background</h2>
    <div class="segmented">{background_options}</div>
  </section>

  <section class="card">
    <h2 class="label">Updates</h2>
    {updates}
    <div class="segmented">
      <label><input type="radio" name="update_channel" value="stable"{stable}><span>Stable</span></label>
      <label><input type="radio" name="update_channel" value="beta"{beta}><span>Beta</span></label>
    </div>
    <label class="switch"><input type="checkbox" name="auto_update"{auto}><span class="track"></span><span>Install updates automatically</span></label>
  </section>

  <button type="submit" class="primary">Save</button>
</form>
<div class="row update-actions">
  <form method="POST" action="/update/check"><button type="submit" class="secondary wide">Check now</button></form>
  {update_now}
</div>
<form method="POST" action="/forget"><button type="submit" class="link">Unpair this phone</button></form>
<form method="POST" action="/forget-all" data-confirm="Unpair every phone, including this one? Each will need a new code from the clock."><button type="submit" class="link">Unpair all phones</button></form>
<script>{place_script}</script>
<script>{update_script}</script>
<script>
(function () {{
  // Preview a colour on this page straight away; Save applies it to the clock
  document.querySelectorAll('input[name=accent]').forEach(function (r) {{
    r.addEventListener('change', function () {{
      document.documentElement.style.setProperty('--accent', r.dataset.hex);
    }});
  }});
  document.querySelectorAll('input[name=background]').forEach(function (r) {{
    r.addEventListener('change', function () {{
      var s = document.documentElement.style;
      s.setProperty('--bg', r.dataset.bg);
      s.setProperty('--surface', r.dataset.surface);
    }});
  }});
}})();
</script>"#,
        banner = banner,
        place_cards = render_place_cards(cfg),
        place_script = PLACE_SCRIPT,
        night_options = night_options,
        gap_options = gap_options,
        alerts = if cfg.alerts_enabled { " checked" } else { "" },
        altitude = cfg.alert_max_altitude_ft.map(|ft| ft.to_string()).unwrap_or_default(),
        alt_min = ALERT_ALTITUDE_MIN_FT,
        alt_max = ALERT_ALTITUDE_MAX_FT,
        night_start = html_escape(&cfg.night_start),
        night_end = html_escape(&cfg.night_end),
        night_alerts = if cfg.night_alerts { " checked" } else { "" },
        accent_options = accent_options,
        background_options = background_options,
        analog = face_checked("analog"),
        digital = face_checked("digital"),
        date = if cfg.show_date { " checked" } else { "" },
        updates = updates,
        stable = if cfg.update_channel != "beta" { " checked" } else { "" },
        beta = if cfg.update_channel == "beta" { " checked" } else { "" },
        auto = if cfg.auto_update { " checked" } else { "" },
        // Always on the page, shown when there is something to install
        update_now = format!(
            r#"<form method="POST" action="/update/install" id="update-now"{hidden}><button type="submit" class="primary">Update now</button></form>"#,
            hidden = if can_install(status.as_ref()) { "" } else { " hidden" }
        ),
        update_script = UPDATE_SCRIPT,
    );

    page("Settings", &body, cfg)
}

/// The Location and Facing cards: on the settings page and the setup page.
fn render_place_cards(cfg: &Config) -> String {
    let facing_options: String = FACINGS
        .iter()
        .map(|f| {
            format!(
                r#"<label class="pill"><input type="radio" name="facing_direction" value="{f}"{c}><span>{f}</span></label>"#,
                f = f,
                c = if cfg.facing_direction == *f { " checked" } else { "" }
            )
        })
        .collect();

    // No location yet: empty boxes and no map link
    let coordinate = |v: Option<f64>| v.map(|v| v.to_string()).unwrap_or_default();
    let (lat, lon) = (coordinate(cfg.latitude), coordinate(cfg.longitude));
    let location_note = if cfg.location().is_none() {
        r#"<p class="lead">Set where the clock is to start seeing flights.</p>"#
    } else {
        ""
    };
    let map_link = if cfg.location().is_some() {
        format!("https://www.openstreetmap.org/?mlat={lat}&amp;mlon={lon}#map=13/{lat}/{lon}")
    } else {
        "https://www.openstreetmap.org/".to_string()
    };

    format!(
        r#"<section class="card">
    <h2 class="label">Location</h2>
    {location_note}
    <div class="row">
      <input id="place" placeholder="Postcode, town or address" autocomplete="off" enterkeyhint="search">
      <button type="button" class="secondary" id="lookup">Search</button>
    </div>
    <p class="hint" id="lookup-status">This phone asks postcodes.io (UK postcodes) or OpenStreetMap. The clock only gets the coordinates.</p>
    <div class="results" id="results"></div>
    <div class="row">
      <div class="field"><label class="label" for="latitude">Latitude</label>
        <input id="latitude" name="latitude" inputmode="decimal" value="{lat}" required></div>
      <div class="field"><label class="label" for="longitude">Longitude</label>
        <input id="longitude" name="longitude" inputmode="decimal" value="{lon}" required></div>
    </div>
    <p class="hint"><a class="hint" id="map-link" href="{map_link}" target="_blank" rel="noopener">Check on a map</a> · Search data © <a class="hint" href="https://www.openstreetmap.org/copyright" target="_blank" rel="noopener">OpenStreetMap</a> contributors</p>
    <div class="field"><label class="label" for="radius_nm">Radar radius (nautical miles)</label>
      <input id="radius_nm" name="radius_nm" type="number" min="{rmin}" max="{rmax}" step="0.5" value="{radius}" required></div>
  </section>

  <section class="card">
    <h2 class="label">Facing</h2>
    <p class="hint">Which way the clock faces. Sets the top of the radar.</p>
    <div class="pills">{facing_options}</div>
  </section>"#,
        radius = cfg.radius_nm,
        rmin = RADIUS_MIN_NM,
        rmax = RADIUS_MAX_NM,
    )
}

/// Location search for the place cards. Runs on the phone: a UK postcode
/// goes to postcodes.io, anything else (or a postcode it doesn't know) to
/// OpenStreetMap's Nominatim, once per press of Search as its usage policy
/// asks. Results are put on the page as text, never as HTML.
const PLACE_SCRIPT: &str = r#"
(function () {
  var place = document.getElementById('place');
  var lat = document.getElementById('latitude');
  var lon = document.getElementById('longitude');
  var status = document.getElementById('lookup-status');
  var results = document.getElementById('results');
  var map = document.getElementById('map-link');
  var button = document.getElementById('lookup');
  function updateMap() {
    var a = encodeURIComponent(lat.value.trim()), o = encodeURIComponent(lon.value.trim());
    map.href = 'https://www.openstreetmap.org/?mlat=' + a + '&mlon=' + o + '#map=13/' + a + '/' + o;
  }
  lat.addEventListener('input', updateMap);
  lon.addEventListener('input', updateMap);
  function choose(a, o, name) {
    lat.value = Number(a).toFixed(6);
    lon.value = Number(o).toFixed(6);
    updateMap();
    results.textContent = '';
    status.textContent = 'Found ' + name + '. Save to apply.';
  }
  function postcode(text) {
    return fetch('https://api.postcodes.io/postcodes/' + encodeURIComponent(text))
      .then(function (r) { return r.json(); })
      .then(function (j) {
        if (j.status !== 200 || !j.result) throw new Error();
        choose(j.result.latitude, j.result.longitude, j.result.postcode + ' (' + j.result.admin_district + ')');
      });
  }
  function search(text) {
    return fetch('https://nominatim.openstreetmap.org/search?format=jsonv2&limit=5&q=' + encodeURIComponent(text))
      .then(function (r) { if (!r.ok) throw new Error(); return r.json(); })
      .then(function (list) {
        list = (list || []).filter(function (p) { return isFinite(Number(p.lat)) && isFinite(Number(p.lon)); });
        if (!list.length) { status.textContent = "Couldn't find that. Try a nearby town, or type the coordinates."; return; }
        if (list.length === 1) { choose(list[0].lat, list[0].lon, list[0].display_name); return; }
        status.textContent = 'Which one?';
        results.textContent = '';
        list.forEach(function (p) {
          var b = document.createElement('button');
          b.type = 'button';
          b.textContent = p.display_name;
          b.addEventListener('click', function () { choose(p.lat, p.lon, p.display_name); });
          results.appendChild(b);
        });
      });
  }
  var UK_POSTCODE = /^[A-Za-z]{1,2}[0-9][0-9A-Za-z]?\s*[0-9][A-Za-z]{2}$/;
  function lookup() {
    var text = place.value.trim();
    if (!text) { place.focus(); return; }
    status.textContent = 'Searching for ' + text + '...';
    results.textContent = '';
    button.disabled = true;
    var found = UK_POSTCODE.test(text) ? postcode(text).catch(function () { return search(text); }) : search(text);
    found
      .catch(function () { status.textContent = "Couldn't search just now. Check this phone is online, or type the coordinates."; })
      .then(function () { button.disabled = false; });
  }
  button.addEventListener('click', lookup);
  place.addEventListener('keydown', function (e) { if (e.key === 'Enter') { e.preventDefault(); lookup(); } });
})();
"#;

/// First-run setup: where the clock is and which way it faces. Everything
/// else has a sensible default and lives on the settings page.
fn render_setup(cfg: &Config, error: Option<&str>) -> String {
    let banner = error
        .map(|e| format!(r#"<p class="banner error">{}</p>"#, html_escape(e)))
        .unwrap_or_default();
    let body = format!(
        r#"{banner}
<p class="lead intro">This phone is paired. Two things and the clock can start showing flights.</p>
<form method="POST" action="/setup" class="stack">
  {place_cards}
  <button type="submit" class="primary">Save</button>
</form>
<form method="POST" action="/setup/later"><button type="submit" class="link">Later</button></form>
<script>{place_script}</script>"#,
        place_cards = render_place_cards(cfg),
        place_script = PLACE_SCRIPT,
    );
    page("Setup", &body, cfg)
}

/// Version and last update result, for the Updates card.
fn render_update_status(status: Option<&update::UpdateStatus>) -> String {
    let version = format!("<b>FlightClock {}</b>", html_escape(update::VERSION));
    let Some(status) = status else {
        return format!(r#"<p class="status">{} · <span class="hint">not checked yet</span></p>"#, version);
    };
    let when = ago(status.checked_at);
    let latest = status.latest.as_deref().map(html_escape).unwrap_or_default();
    let line = match status.state.as_str() {
        "up_to_date" => format!(r#"<span class="ok">Up to date</span> · checked {}"#, when),
        "available" => format!(r#"<span class="warn">{} available</span> · checked {}"#, latest, when),
        "installing" => format!(r#"<span class="warn">Installing {}...</span>"#, latest),
        "checking" => "Checking...".to_string(),
        "updated" => format!(r#"<span class="ok">Updated to {}</span> {}"#, latest, when),
        "failed" => format!(r#"<span class="bad">Last update failed</span> · {}"#, when),
        other => html_escape(other),
    };
    let message = if status.message.is_empty() {
        String::new()
    } else {
        format!(r#"<br><span class="hint">{}</span>"#, html_escape(&status.message))
    };
    format!(r#"<p class="status">{}<br>{}{}</p>"#, version, line, message)
}

/// The updater is at work: the page should look again soon.
fn update_busy(status: Option<&update::UpdateStatus>) -> bool {
    status.is_some_and(|s| s.state == "checking" || s.state == "installing")
}

/// There is a release to install (or to try again): show "Update now".
fn can_install(status: Option<&update::UpdateStatus>) -> bool {
    status.is_some_and(|s| s.state == "available" || s.state == "failed")
}

/// What the settings page polls: the Updates card's contents, already
/// escaped HTML, and what to do with it.
fn update_status_json(status: Option<&update::UpdateStatus>) -> String {
    serde_json::json!({
        "html": render_update_status(status),
        "busy": update_busy(status),
        "can_install": can_install(status),
    })
    .to_string()
}

/// Keeps the Updates card current. Every 3 s while the updater is working
/// or for 4 minutes after "Check now" / "Update now"; every 30 s otherwise.
/// A failed request (the clock restarts during an update) is tried again.
/// The page shell swaps in whole new pages after a form post; the old page's
/// loop notices its card has gone and stops.
const UPDATE_SCRIPT: &str = r#"
(function () {
  var box = document.getElementById('update-status');
  var installButton = document.getElementById('update-now');
  if (!box || !window.fetch) return;
  var busy = box.dataset.busy === '1';
  var watchUntil = box.dataset.watch === '1' ? Date.now() + 240000 : 0;
  function soon() { return busy || Date.now() < watchUntil; }
  function poll() {
    if (!document.body.contains(box)) return;
    fetch('/update/status', { credentials: 'same-origin', cache: 'no-store' })
      .then(function (r) { if (!r.ok) throw new Error(); return r.json(); })
      .then(function (s) {
        box.innerHTML = s.html;
        busy = s.busy;
        if (installButton) installButton.hidden = !s.can_install;
      })
      .catch(function () {})
      .then(function () { setTimeout(poll, soon() ? 3000 : 30000); });
  }
  setTimeout(poll, soon() ? 2000 : 30000);
})();
"#;

fn ago(unix_secs: u64) -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let secs = now.saturating_sub(unix_secs);
    match secs {
        0..=59 => "just now".into(),
        60..=3599 => format!("{} min ago", secs / 60),
        3600..=86399 => format!("{} h ago", secs / 3600),
        _ => format!("{} days ago", secs / 86400),
    }
}

/// The brand fonts the page shell loads, by URL path.
pub(crate) fn font(path: &str) -> Option<&'static [u8]> {
    match path {
        "/fonts/display.ttf" => Some(FONT_DISPLAY),
        "/fonts/display-medium.ttf" => Some(FONT_DISPLAY_MEDIUM),
        "/fonts/mono.ttf" => Some(FONT_MONO),
        _ => None,
    }
}

/// The shell every page shares, the WiFi setup portal's included: brand
/// fonts and colours (the owner's accent and background), the wordmark, and
/// the script that sends forms in the background.
pub(crate) fn page(title: &str, body: &str, cfg: &Config) -> String {
    let accent = cfg.accent_hex();
    let (bg, surface, _) = cfg.background_hex();
    format!(
        r##"<!DOCTYPE html>
<html lang="en"><head><meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<meta name="theme-color" content="{bg}">
<title>FlightClock · {title}</title>
<style>
@font-face{{font-family:"Space Grotesk";font-weight:600;src:url(/fonts/display.ttf) format("truetype")}}
@font-face{{font-family:"Space Grotesk";font-weight:500;src:url(/fonts/display-medium.ttf) format("truetype")}}
@font-face{{font-family:"JetBrains Mono";font-weight:500;src:url(/fonts/mono.ttf) format("truetype")}}
:root{{--bg:{bg};--surface:{surface};--fg:#EEF2F6;--soft:#C9D3DE;--dim:#8A9AAD;--line:#2A3646;--accent:{accent};--error:#F87171;--ok:#4ADE80}}
*{{box-sizing:border-box;margin:0;padding:0}}
body{{background:var(--bg);color:var(--fg);font-family:"Space Grotesk",system-ui,sans-serif;font-weight:500;line-height:1.4;padding:24px 16px 48px;max-width:460px;margin:0 auto}}
header{{text-align:center;margin:8px 0 24px}}
.wordmark{{font-size:28px;font-weight:600;letter-spacing:-.5px}}.wordmark span{{color:var(--accent)}}
.eyebrow,.label{{font-family:"JetBrains Mono",ui-monospace,monospace;font-size:12px;font-weight:500;letter-spacing:2px;text-transform:uppercase;color:var(--dim)}}
.eyebrow{{color:var(--accent);margin-top:6px;display:block}}
.stack{{display:flex;flex-direction:column;gap:16px}}
.card{{background:var(--surface);border:1px solid var(--line);border-radius:16px;padding:18px;display:flex;flex-direction:column;gap:12px}}
.lead{{font-size:17px}}
.hint{{font-size:13px;color:var(--dim)}}a.hint{{color:var(--soft)}}
.row{{display:flex;gap:10px;align-items:flex-end}}.row>*{{flex:1;min-width:0}}
.field{{display:flex;flex-direction:column;gap:6px}}
input:not([type=radio]):not([type=checkbox]),select{{width:100%;background:var(--bg);color:var(--fg);border:1px solid var(--line);border-radius:12px;padding:12px 14px;font:inherit;font-size:16px;color-scheme:dark}}
[hidden]{{display:none!important}}
.segmented.three{{grid-template-columns:repeat(3,1fr)}}
input:focus{{outline:2px solid var(--accent);outline-offset:1px;border-color:transparent}}
.intro{{margin-bottom:16px;text-align:center}}
.nets{{display:flex;flex-direction:column;gap:8px}}
.net{{display:flex;align-items:center;gap:10px;min-height:52px;padding:10px 14px;background:var(--bg);border:1px solid var(--line);border-radius:12px;color:var(--fg);text-decoration:none}}
.net:active{{border-color:var(--accent)}}
.net .name{{flex:1;overflow-wrap:anywhere}}
.net svg{{flex:0 0 auto;color:var(--dim)}}.net .on{{fill:var(--fg)}}.net .off{{fill:var(--line)}}
.net .go{{color:var(--dim);font-size:20px}}
.chosen{{font-size:20px;font-weight:600;overflow-wrap:anywhere}}
a.link{{display:block;text-align:center;padding:13px}}
.center{{text-align:center;align-items:center}}
.spinner{{width:40px;height:40px;border:4px solid var(--line);border-top-color:var(--accent);border-radius:50%;animation:spin 1s linear infinite;margin:8px auto}}
@keyframes spin{{to{{transform:rotate(360deg)}}}}
.results{{display:flex;flex-direction:column;gap:8px}}.results:empty{{display:none}}
.results button{{background:var(--bg);color:var(--fg);border:1px solid var(--line);border-radius:12px;text-align:left;font-size:14px;padding:10px 14px}}
.code-input{{font-family:"JetBrains Mono",ui-monospace,monospace!important;font-size:28px!important;letter-spacing:8px;text-align:center}}
button{{font:inherit;font-size:16px;border-radius:999px;padding:13px 20px;cursor:pointer;min-height:48px}}
.primary{{background:var(--accent);color:var(--bg);border:none;font-weight:600;width:100%}}
.secondary{{background:transparent;color:var(--fg);border:1px solid var(--line);flex:0 0 auto}}
.link{{background:none;border:none;color:var(--dim);text-decoration:underline;width:100%;margin-top:8px}}
.banner{{border-radius:12px;padding:12px 14px;margin-bottom:16px;border:1px solid}}
.banner.ok{{color:var(--ok);border-color:#4ADE8066;background:#4ADE8014}}
.banner.error{{color:var(--error);border-color:#F8717166;background:#F8717114}}
.banner.info{{color:var(--accent);border-color:{accent}66;background:{accent}14}}
.update-actions{{margin-top:16px;align-items:stretch}}.update-actions form{{flex:1}}.wide{{width:100%}}
.status{{font-size:15px}}.status b{{font-weight:600}}.status .ok{{color:var(--ok)}}.status .warn{{color:var(--accent)}}.status .bad{{color:var(--error)}}
.pills{{display:grid;grid-template-columns:repeat(4,1fr);gap:8px}}
.pill input,.segmented input{{position:absolute;opacity:0;pointer-events:none}}
.pill span{{display:flex;align-items:center;justify-content:center;min-height:48px;border:1px solid var(--line);border-radius:999px;font-weight:600}}
.pill input:checked+span{{background:var(--accent);border-color:var(--accent);color:var(--bg)}}
.pill input:focus-visible+span,.segmented input:focus-visible+span{{outline:2px solid var(--accent);outline-offset:2px}}
.segmented{{display:grid;grid-template-columns:1fr 1fr;border:1px solid var(--line);border-radius:999px;padding:4px;gap:4px}}
.segmented span{{display:flex;align-items:center;justify-content:center;min-height:42px;border-radius:999px;font-weight:600;color:var(--dim)}}
.segmented input:checked+span{{background:var(--accent);color:var(--bg)}}
.switch{{display:flex;align-items:center;gap:12px;min-height:44px;cursor:pointer}}
.switch input{{position:absolute;opacity:0}}
.track{{width:46px;height:28px;border-radius:999px;border:1px solid var(--line);background:var(--bg);position:relative;flex:0 0 auto}}
.track::after{{content:"";position:absolute;top:3px;left:3px;width:20px;height:20px;border-radius:50%;background:var(--dim);transition:transform .15s}}
.switch input:checked+.track{{background:var(--accent);border-color:var(--accent)}}
.switch input:checked+.track::after{{transform:translateX(18px);background:var(--bg)}}
.switch input:focus-visible+.track{{outline:2px solid var(--accent);outline-offset:2px}}
.swatches{{display:grid;grid-template-columns:repeat(5,1fr);gap:6px}}
.swatch{{cursor:pointer}}.swatch input{{position:absolute;opacity:0;pointer-events:none}}
.swatch span{{display:flex;flex-direction:column;align-items:center;gap:8px;font-size:12px;line-height:1.2;text-align:center;color:var(--dim)}}
.swatch i{{width:44px;height:44px;border-radius:50%;background:var(--c);border:3px solid var(--surface);box-shadow:0 0 0 1px var(--line)}}
.swatch input:checked+span{{color:var(--fg)}}.swatch input:checked+span i{{box-shadow:0 0 0 2px var(--fg)}}
.segmented .dot{{width:14px;height:14px;border-radius:50%;background:var(--c);box-shadow:0 0 0 1px var(--soft);margin-right:8px;display:inline-block}}
.swatch input:focus-visible+span i{{outline:2px solid var(--fg);outline-offset:4px}}
</style></head><body>
<header><div class="wordmark">Flight<span>Clock</span></div><span class="eyebrow">{title}</span></header>
{body}
<script>
// Send forms in the background and swap in the page that comes back.
// Browsers show an "insecure form" prompt for every form posted over plain
// http; background requests don't trigger it. Without JS, forms still work.
document.addEventListener('submit', function (e) {{
  var form = e.target;
  if (form.dataset.confirm && !confirm(form.dataset.confirm)) {{ e.preventDefault(); return; }}
  if (!window.fetch || form.method.toLowerCase() !== 'post') return;
  e.preventDefault();
  var button = form.querySelector('[type=submit]');
  if (button) button.disabled = true;
  fetch(form.action, {{
    method: 'POST',
    body: new URLSearchParams(new FormData(form)),
    credentials: 'same-origin'
  }})
    .then(function (r) {{ return r.text(); }})
    .then(function (html) {{
      document.open();
      document.write(html);
      document.close();
      window.scrollTo(0, 0);
    }})
    .catch(function () {{
      if (button) button.disabled = false;
      alert("Couldn't reach the clock. Check you're on the same WiFi and try again.");
    }});
}});
</script>
</body></html>"##
    )
}

fn respond_html(request: Request, html: String) {
    let ct = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap();
    let nocache = Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap();
    let _ = request.respond(Response::from_string(html).with_header(ct).with_header(nocache));
}

pub(crate) fn respond_json(request: Request, json: String) {
    let ct = Header::from_bytes(&b"Content-Type"[..], &b"application/json"[..]).unwrap();
    let nocache = Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap();
    let _ = request.respond(Response::from_string(json).with_header(ct).with_header(nocache));
}

pub(crate) fn respond_font(request: Request, bytes: &'static [u8]) {
    let ct = Header::from_bytes(&b"Content-Type"[..], &b"font/ttf"[..]).unwrap();
    let cache = Header::from_bytes(&b"Cache-Control"[..], &b"public, max-age=604800"[..]).unwrap();
    let _ = request.respond(Response::from_data(bytes).with_header(ct).with_header(cache));
}

/// Hand a newly paired phone its session cookie and send it to the front page.
fn respond_paired(request: Request, token: &str) {
    let cookie = format!(
        "{}={}; Max-Age={}; Path=/; HttpOnly; SameSite=Strict",
        COOKIE_NAME, token, COOKIE_MAX_AGE_SECS
    );
    let header = Header::from_bytes(&b"Set-Cookie"[..], cookie.as_bytes()).unwrap();
    let location = Header::from_bytes(&b"Location"[..], &b"/"[..]).unwrap();
    let nocache = Header::from_bytes(&b"Cache-Control"[..], &b"no-store"[..]).unwrap();
    let _ = request.respond(Response::empty(303).with_header(header).with_header(location).with_header(nocache));
}

fn redirect(request: Request, to: &str) {
    let location = Header::from_bytes(&b"Location"[..], to.as_bytes()).unwrap();
    let _ = request.respond(Response::empty(303).with_header(location));
}

fn read_form(request: &mut Request) -> Vec<(String, String)> {
    let mut body = Vec::new();
    let _ = request.as_reader().take(MAX_BODY_BYTES).read_to_end(&mut body);
    parse_form(&String::from_utf8_lossy(&body))
}

fn field(form: &[(String, String)], name: &str) -> String {
    form.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()).unwrap_or_default()
}

fn session_token(request: &Request) -> Option<String> {
    request
        .headers()
        .iter()
        .filter(|h| h.field.equiv("Cookie"))
        .flat_map(|h| h.value.as_str().split(';').map(str::trim).map(str::to_string).collect::<Vec<_>>())
        .find_map(|c| c.strip_prefix(&format!("{}=", COOKIE_NAME)).map(str::to_string))
        .filter(|t| !t.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn form(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    fn valid() -> Vec<(String, String)> {
        form(&[
            ("latitude", "51.4700"),
            ("longitude", "-0.4543"),
            ("radius_nm", "8"),
            ("facing_direction", "NE"),
            ("clock_face", "digital"),
            ("show_date", "on"),
            ("update_channel", "beta"),
            ("auto_update", "on"),
        ])
    }

    #[test]
    fn applies_valid_form() {
        let cfg = apply_form(&Config::default(), &valid()).unwrap();
        assert_eq!(cfg.latitude, Some(51.47));
        assert_eq!(cfg.longitude, Some(-0.4543));
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.radius_nm, 8.0);
        assert_eq!(cfg.facing_direction, "NE");
        assert_eq!(cfg.clock_face, "digital");
        assert!(cfg.show_date);
        assert_eq!(cfg.update_channel, "beta");
        assert!(cfg.auto_update);
    }

    #[test]
    fn applies_accent_colour() {
        let mut f = valid();
        f.push(("accent".into(), "lilac".into()));
        let cfg = apply_form(&Config::default(), &f).unwrap();
        assert_eq!(cfg.accent, "lilac");
        assert_eq!(cfg.accent_hex(), "#B9A2FF");
    }

    #[test]
    fn missing_accent_keeps_current() {
        let current = Config { accent: "coral".into(), ..Config::default() };
        assert_eq!(apply_form(&current, &valid()).unwrap().accent, "coral");
    }

    #[test]
    fn rejects_unknown_accent() {
        let mut f = valid();
        f.push(("accent".into(), "#000000".into()));
        assert!(apply_form(&Config::default(), &f).is_err());
    }

    #[test]
    fn page_uses_chosen_accent() {
        let cfg = Config { accent: "lime".into(), ..Config::default() };
        let html = render_settings(&cfg, None);
        assert!(html.contains("--accent:#B4E06A"));
        assert!(!html.contains("--accent:#FFB547"));
        assert!(html.contains(r##"value="lime" data-hex="#B4E06A" checked"##));
    }

    #[test]
    fn applies_background() {
        let mut f = valid();
        f.push(("background".into(), "black".into()));
        assert_eq!(apply_form(&Config::default(), &f).unwrap().background, "black");
        let mut bad = valid();
        bad.push(("background".into(), "white".into()));
        assert!(apply_form(&Config::default(), &bad).is_err());
        let current = Config { background: "black".into(), ..Config::default() };
        assert_eq!(apply_form(&current, &valid()).unwrap().background, "black");
    }

    #[test]
    fn page_uses_chosen_background() {
        let cfg = Config { background: "black".into(), ..Config::default() };
        let html = render_settings(&cfg, None);
        assert!(html.contains("--bg:#000000"));
        assert!(html.contains(r##"<meta name="theme-color" content="#000000">"##));
    }

    #[test]
    fn missing_checkbox_means_date_off() {
        let f: Vec<_> = valid().into_iter().filter(|(k, _)| k != "show_date").collect();
        assert!(!apply_form(&Config::default(), &f).unwrap().show_date);
    }

    #[test]
    fn rejects_out_of_range_values() {
        let mut f = valid();
        f[0].1 = "91".into();
        assert!(apply_form(&Config::default(), &f).is_err());
        let mut f = valid();
        f[2].1 = "0.5".into();
        assert!(apply_form(&Config::default(), &f).is_err());
        let mut f = valid();
        f[3].1 = "UP".into();
        assert!(apply_form(&Config::default(), &f).is_err());
        let mut f = valid();
        f[1].1 = "NaN".into();
        assert!(apply_form(&Config::default(), &f).is_err());
    }

    /// Stands in for the clock: records what it was asked.
    #[derive(Default)]
    struct FakeClock {
        asked: Mutex<Vec<String>>,
    }

    impl Clock for FakeClock {
        fn show_code(&self, code: &str) -> Result<(), String> {
            self.asked.lock().unwrap().push(format!("show {}", code));
            Ok(())
        }
        fn hide_code(&self) {
            self.asked.lock().unwrap().push("hide".into());
        }
        fn save_settings(&self, config: &Config) -> Result<(), String> {
            config.validate()
        }
        fn trigger_update(&self, _install: bool) -> Result<(), String> {
            Ok(())
        }
        fn wifi_connect(&self, _ssid: &str, _password: &str) -> Result<(), String> {
            Ok(())
        }
        fn portal_seen(&self) {}
        fn link_used(&self) {
            self.asked.lock().unwrap().push("link used".into());
        }
    }

    fn web(name: &str) -> SettingsWeb {
        SettingsWeb {
            state: Mutex::new(State::default()),
            clock: Arc::new(FakeClock::default()),
            sessions_path: std::env::temp_dir().join(format!("fc-test-{}-{}.json", name, std::process::id())),
            hostname: "flight-clock".into(),
            link_token: Arc::new(Mutex::new(None)),
        }
    }

    fn wrong_code(code: &str) -> &'static str {
        if code == "000000" { "111111" } else { "000000" }
    }

    #[test]
    fn pairing_code_flow() {
        let web = web("flow");
        let now = Instant::now();
        assert!(matches!(web.check_code("123456", now), CodeResult::NoCode));

        let code = web.start_pairing(now).unwrap();
        assert_eq!(code.len(), 6);
        let wrong = wrong_code(&code);
        for _ in 0..MAX_CODE_ATTEMPTS - 1 {
            assert!(matches!(web.check_code(wrong, now), CodeResult::Wrong));
        }
        assert!(matches!(web.check_code(wrong, now), CodeResult::Locked));
        assert!(matches!(web.check_code(&code, now), CodeResult::NoCode));

        let code = web.start_pairing(now).unwrap();
        match web.check_code(&code, now) {
            CodeResult::Ok(token) => {
                assert_eq!(token.len(), 32);
                assert!(web.state.lock().unwrap().sessions.contains(&token));
            }
            _ => panic!("correct code rejected"),
        }
        let _ = std::fs::remove_file(&web.sessions_path);
    }

    #[test]
    fn code_expires() {
        let web = web("expiry");
        let now = Instant::now();
        let code = web.start_pairing(now).unwrap();
        assert!(matches!(web.check_code(&code, now + CODE_TTL + Duration::from_secs(1)), CodeResult::NoCode));
    }

    #[test]
    fn new_codes_do_not_buy_more_guesses() {
        // The attack: ask for a code, guess five times, ask for another...
        let web = web("guesses");
        let now = Instant::now();
        let mut wrong_guesses = 0;
        let paused = loop {
            let Ok(code) = web.start_pairing(now) else { panic!("starts ran out before guesses") };
            let wrong = wrong_code(&code);
            let mut paused = None;
            for _ in 0..MAX_CODE_ATTEMPTS {
                wrong_guesses += 1;
                match web.check_code(wrong, now) {
                    CodeResult::Paused(wait) => {
                        paused = Some(wait);
                        break;
                    }
                    CodeResult::Wrong | CodeResult::Locked => {}
                    _ => panic!("unexpected result"),
                }
            }
            if let Some(wait) = paused {
                break wait;
            }
        };
        assert_eq!(wrong_guesses, MAX_FAILURES);
        assert_eq!(paused, FAILURE_WINDOW);

        // Paused: no new code, and even the right code is refused
        assert!(web.start_pairing(now).is_err());
        assert!(web.state.lock().unwrap().pairing.is_none());
        let later = now + Duration::from_secs(1800);
        assert_eq!(web.start_pairing(later).unwrap_err(), Duration::from_secs(1800));

        // An hour after the wrong codes, pairing works again
        let after = now + FAILURE_WINDOW;
        let code = web.start_pairing(after).unwrap();
        assert!(matches!(web.check_code(&code, after), CodeResult::Ok(_)));
        let _ = std::fs::remove_file(&web.sessions_path);
    }

    #[test]
    fn code_requests_are_limited() {
        let web = web("starts");
        let now = Instant::now();
        for _ in 0..MAX_STARTS {
            assert!(web.start_pairing(now).is_ok());
        }
        assert_eq!(web.start_pairing(now + Duration::from_secs(60)).unwrap_err(), Duration::from_secs(540));
        assert!(web.start_pairing(now + START_WINDOW).is_ok());
    }

    #[test]
    fn sessions_file_is_private() {
        let web = web("private");
        web.save_sessions(&["abc".to_string()]);
        web.save_sessions(&["abc".to_string(), "def".to_string()]);
        let saved = std::fs::read_to_string(&web.sessions_path).unwrap();
        assert_eq!(saved, r#"["abc","def"]"#);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&web.sessions_path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let _ = std::fs::remove_file(&web.sessions_path);
    }

    #[test]
    fn only_the_clocks_own_addresses_are_served() {
        let ok = |host: &str| host_allowed(Some(host), "flight-clock");
        for host in [
            "flight-clock.local",
            "Flight-Clock.local",
            "flight-clock.local.",
            "flight-clock.local:8080",
            "flight-clock",
            "flight-clock.lan",
            "flight-clock.home.arpa",
            "flight-clock-2.local",
            "192.168.1.50",
            "192.168.4.1:80",
            "[fe80::1]",
            "[fe80::1]:8080",
            "localhost:8080",
        ] {
            assert!(ok(host), "{} should be allowed", host);
        }
        for host in [
            "",
            "evil.example",
            "flight-clock.evil.example",
            "flight-clock.local.evil.example",
            "evil.example:80",
            "flight-clock-evil.local",
            "flight-clock-.local",
            "notflight-clock.local",
            "192.168.1.50.evil.example",
            "[not-an-ip]",
        ] {
            assert!(!ok(host), "{} should be refused", host);
        }
        assert!(!host_allowed(None, "flight-clock"));
        // Unknown hostname: only addresses
        assert!(host_allowed(Some("10.0.0.2"), ""));
        assert!(!host_allowed(Some(".local"), ""));
    }

    #[test]
    fn form_posts_must_come_from_the_clocks_page() {
        let host = Some("flight-clock.local");
        assert!(same_origin(Some("http://flight-clock.local"), host));
        assert!(same_origin(Some("http://Flight-Clock.local"), host));
        assert!(same_origin(None, host));
        assert!(!same_origin(Some("http://evil.example"), host));
        assert!(!same_origin(Some("https://flight-clock.local"), host));
        assert!(!same_origin(Some("null"), host));
        assert!(!same_origin(Some("http://flight-clock.local:8080"), host));
        assert!(same_origin(Some("http://localhost:8080"), Some("localhost:8080")));
    }

    #[test]
    fn page_without_a_location_asks_for_one() {
        let html = render_settings(&Config::default(), None);
        assert!(html.contains("Set where the clock is"));
        assert!(html.contains(r#"name="latitude" inputmode="decimal" value="" required"#));
        assert!(html.contains(r#"id="map-link" href="https://www.openstreetmap.org/" "#));

        let cfg = Config { latitude: Some(51.47), longitude: Some(-0.45), ..Config::default() };
        let html = render_settings(&cfg, None);
        assert!(!html.contains("Set where the clock is"));
        assert!(html.contains(r#"value="51.47""#));
        assert!(html.contains("mlat=51.47&amp;mlon=-0.45"));
    }

    #[test]
    fn waits_are_rounded_up_to_minutes() {
        assert_eq!(minutes(Duration::from_secs(1)), "1 minute");
        assert_eq!(minutes(Duration::from_secs(60)), "1 minute");
        assert_eq!(minutes(Duration::from_secs(61)), "2 minutes");
        assert_eq!(minutes(Duration::from_secs(3600)), "60 minutes");
    }

    #[test]
    fn qr_link_pairs_once() {
        let web = web("link");
        assert!(web.use_link("").is_none());
        assert!(web.use_link("anything").is_none());

        *web.link_token.lock().unwrap() = Some("abcdefghjkmnpqrs".into());
        assert!(web.use_link("").is_none());
        assert!(web.use_link("abcdefghjkmnpqrt").is_none());
        assert!(web.use_link("abcdefghjkmnpqr").is_none());
        // A miss leaves the token in place
        let session = web.use_link("abcdefghjkmnpqrs").expect("the right token pairs");
        assert!(web.state.lock().unwrap().sessions.contains(&session));
        // Used up
        assert!(web.use_link("abcdefghjkmnpqrs").is_none());
        assert!(web.link_token.lock().unwrap().is_none());
        let _ = std::fs::remove_file(&web.sessions_path);
    }

    #[test]
    fn an_empty_token_never_matches() {
        let web = web("empty-link");
        *web.link_token.lock().unwrap() = Some(String::new());
        assert!(web.use_link("").is_none());
    }

    #[test]
    fn setup_is_owed_until_a_location_or_later() {
        assert!(needs_setup(&Config::default()));
        assert!(!needs_setup(&Config { setup_dismissed: true, ..Config::default() }));
        assert!(!needs_setup(&Config { latitude: Some(51.47), longitude: Some(-0.45), ..Config::default() }));
    }

    #[test]
    fn setup_form_changes_only_the_place() {
        let current = Config { accent: "lime".into(), clock_face: "digital".into(), auto_update: false, ..Config::default() };
        let f = form(&[("latitude", "40.6413"), ("longitude", "-73.7781"), ("radius_nm", "10"), ("facing_direction", "SW")]);
        let cfg = apply_place(&current, &f).unwrap();
        assert_eq!(cfg.location().map(|l| (l.lat, l.lon, l.max_radius_nm)), Some((40.6413, -73.7781, 10.0)));
        assert_eq!(cfg.facing_direction, "SW");
        assert_eq!((cfg.accent.as_str(), cfg.clock_face.as_str(), cfg.auto_update), ("lime", "digital", false));
        assert!(cfg.validate().is_ok());

        let missing = form(&[("latitude", ""), ("longitude", ""), ("radius_nm", "10"), ("facing_direction", "SW")]);
        assert!(apply_place(&current, &missing).is_err());
    }

    #[test]
    fn setup_page_has_the_place_fields_and_a_way_out() {
        let html = render_setup(&Config::default(), Some("Latitude must be a number."));
        assert!(html.contains(r#"action="/setup""#));
        assert!(html.contains(r#"action="/setup/later""#));
        assert!(html.contains(r#"name="latitude""#));
        assert!(html.contains(r#"name="facing_direction""#));
        assert!(html.contains("Latitude must be a number."));
        assert!(html.contains("nominatim.openstreetmap.org"));
        // Not the full settings form
        assert!(!html.contains(r#"name="update_channel""#));
    }

    #[test]
    fn settings_page_keeps_the_place_fields() {
        let html = render_settings(&Config::default(), None);
        assert!(html.contains(r#"name="latitude""#));
        assert!(html.contains(r#"name="radius_nm""#));
        assert!(html.contains(r#"name="update_channel""#));
        assert!(html.contains("api.postcodes.io"));
        // The search script is plain JS, not a format string: no doubled braces
        assert!(!PLACE_SCRIPT.contains("{{"));
    }

    #[test]
    fn applies_night_mode_and_alert_filters() {
        let mut f = valid();
        f.extend(form(&[
            ("has_night_and_alerts", "1"),
            ("night_mode", "screen_off"),
            ("night_start", "23:30"),
            ("night_end", "06:15"),
            ("night_alerts", "on"),
            ("alert_max_altitude_ft", "10000"),
            ("alert_min_gap_min", "5"),
        ]));
        let cfg = apply_form(&Config::default(), &f).unwrap();
        assert_eq!((cfg.night_mode.as_str(), cfg.night_start.as_str(), cfg.night_end.as_str()), ("screen_off", "23:30", "06:15"));
        assert!(cfg.night_alerts);
        // The alerts switch wasn't sent: off
        assert!(!cfg.alerts_enabled);
        assert_eq!((cfg.alert_max_altitude_ft, cfg.alert_min_gap_min), (Some(10000), 5));
        assert!(cfg.validate().is_ok());

        // Empty altitude means any
        let i = f.iter().position(|(k, _)| k == "alert_max_altitude_ft").unwrap();
        f[i].1 = "".into();
        assert_eq!(apply_form(&Config::default(), &f).unwrap().alert_max_altitude_ft, None);

        for (name, bad) in [("night_mode", "sleepy"), ("night_start", "25:00"), ("alert_max_altitude_ft", "12"), ("alert_min_gap_min", "7")] {
            let mut g = f.clone();
            let i = g.iter().position(|(k, _)| k == name).unwrap();
            g[i].1 = bad.into();
            assert!(apply_form(&Config::default(), &g).is_err(), "{}={}", name, bad);
        }
    }

    #[test]
    fn an_older_page_leaves_night_and_alerts_alone() {
        let current = Config { night_mode: "dim".into(), alerts_enabled: true, alert_min_gap_min: 10, ..Config::default() };
        let cfg = apply_form(&current, &valid()).unwrap();
        assert_eq!((cfg.night_mode.as_str(), cfg.alerts_enabled, cfg.alert_min_gap_min), ("dim", true, 10));
    }

    #[test]
    fn settings_page_shows_night_and_alert_choices() {
        let cfg = Config { night_mode: "dim".into(), alert_min_gap_min: 5, alert_max_altitude_ft: Some(8000), ..Config::default() };
        let html = render_settings(&cfg, None);
        assert!(html.contains(r#"name="night_mode" value="dim" checked"#));
        assert!(html.contains(r#"<option value="5" selected>5 minutes</option>"#));
        assert!(html.contains(r#"name="alert_max_altitude_ft" type="number" inputmode="numeric" min="500" max="60000" step="500" value="8000""#));
        assert!(html.contains(r#"name="night_start" type="time" value="22:00""#));
    }

    fn status(state: &str) -> update::UpdateStatus {
        update::UpdateStatus {
            state: state.into(),
            current: "0.3.0".into(),
            latest: Some("0.3.1".into()),
            message: String::new(),
            checked_at: 0,
        }
    }

    #[test]
    fn update_status_says_when_to_look_again_and_what_to_offer() {
        for (state, busy, install) in [
            ("checking", true, false),
            ("installing", true, false),
            ("available", false, true),
            ("failed", false, true),
            ("up_to_date", false, false),
            ("updated", false, false),
        ] {
            let s = status(state);
            assert_eq!(update_busy(Some(&s)), busy, "{}", state);
            assert_eq!(can_install(Some(&s)), install, "{}", state);
        }
        assert!(!update_busy(None) && !can_install(None));
    }

    #[test]
    fn update_status_json_carries_the_card() {
        let json: serde_json::Value = serde_json::from_str(&update_status_json(Some(&status("installing")))).unwrap();
        assert_eq!(json["busy"], true);
        assert_eq!(json["can_install"], false);
        assert!(json["html"].as_str().unwrap().contains("Installing 0.3.1"));

        let json: serde_json::Value = serde_json::from_str(&update_status_json(None)).unwrap();
        assert_eq!(json["busy"], false);
        assert!(json["html"].as_str().unwrap().contains("not checked yet"));
    }

    #[test]
    fn update_status_escapes_what_the_updater_wrote() {
        let mut s = status("failed");
        s.message = "<script>alert(1)</script>".into();
        s.latest = Some("<b>x".into());
        let html = render_update_status(Some(&s));
        assert!(!html.contains("<script>") && html.contains("&lt;script&gt;"));
        let odd = render_update_status(Some(&status("<i>odd")));
        assert!(odd.contains("&lt;i&gt;odd"));
    }

    #[test]
    fn settings_page_polls_for_update_status() {
        let html = render_settings(&Config::default(), None);
        assert!(html.contains(r#"<div id="update-status" data-busy="0" data-watch="0">"#));
        assert!(html.contains("/update/status"));
        // "Update now" is on the page either way; hidden unless there's something to install
        assert!(html.contains(r#"id="update-now""#));
        assert!(!UPDATE_SCRIPT.contains("{{"));

        // Just pressed "Check now": look often
        let html = render_settings(&Config::default(), Some(Banner::Info("Checking for updates...".into())));
        assert!(html.contains(r#"data-watch="1""#));
        // An ordinary save doesn't
        let html = render_settings(&Config::default(), Some(Banner::Saved));
        assert!(html.contains(r#"data-watch="0""#));
    }
}
