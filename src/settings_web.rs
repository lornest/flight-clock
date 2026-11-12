//! Settings page on the home network: http://<hostname>.local
//!
//! Served by the same web server as the WiFi setup portal, whenever the setup
//! hotspot is *not* up. Changing settings needs a paired phone:
//!
//! 1. The phone asks for a code; the clock shows a 6-digit code for 2 minutes.
//! 2. Entering it sets a long-lived session cookie (HttpOnly, SameSite=Strict),
//!    so the phone only pairs once. Sessions are kept in a root-only file next
//!    to config.json, so they survive restarts.
//! 3. Five wrong attempts cancel the code.
//!
//! Location can be filled in from a UK postcode: the *phone* looks it up on
//! postcodes.io; the clock itself never contacts it.

use crate::config::{config_path, Config, ACCENTS, BACKGROUNDS};
use crate::update;
use crate::wifi::portal::{html_escape, parse_form};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use tiny_http::{Header, Request, Response};

const CODE_TTL: Duration = Duration::from_secs(120);
const MAX_CODE_ATTEMPTS: u8 = 5;
const MAX_SESSIONS: usize = 20;
const MAX_BODY_BYTES: u64 = 8 * 1024;
const COOKIE_NAME: &str = "fc_session";
const COOKIE_MAX_AGE_SECS: u64 = 365 * 24 * 3600;

const FACINGS: [&str; 8] = ["N", "NE", "E", "SE", "S", "SW", "W", "NW"];
const RADIUS_MIN_NM: f64 = 1.0;
const RADIUS_MAX_NM: f64 = 25.0;

// Brand fonts, served to the phone so the page works without internet
const FONT_DISPLAY: &[u8] = include_bytes!("../assets/fonts/SpaceGrotesk-SemiBold.ttf");
const FONT_DISPLAY_MEDIUM: &[u8] = include_bytes!("../assets/fonts/SpaceGrotesk-Medium.ttf");
const FONT_MONO: &[u8] = include_bytes!("../assets/fonts/JetBrainsMono-Medium.ttf");

/// What the settings page asks the clock's UI to do.
pub enum SettingsEvent {
    /// Show this pairing code on the screen.
    ShowCode(String),
    /// Hide the pairing code (paired, cancelled or too many attempts).
    HideCode,
    /// New settings were saved; apply them.
    Applied(Config),
}

pub struct SettingsWeb {
    state: Mutex<State>,
    notify: Box<dyn Fn(SettingsEvent) + Send + Sync>,
    sessions_path: PathBuf,
}

struct State {
    pairing: Option<Pairing>,
    sessions: Vec<String>,
}

struct Pairing {
    code: String,
    expires: Instant,
    attempts: u8,
}

impl SettingsWeb {
    pub fn new(notify: impl Fn(SettingsEvent) + Send + Sync + 'static) -> Self {
        let sessions_path = PathBuf::from(config_path())
            .parent()
            .map(|d| d.join("web-sessions.json"))
            .unwrap_or_else(|| PathBuf::from("web-sessions.json"));
        let sessions = std::fs::read_to_string(&sessions_path)
            .ok()
            .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
            .unwrap_or_default();

        Self {
            state: Mutex::new(State { pairing: None, sessions }),
            notify: Box::new(notify),
            sessions_path,
        }
    }

    /// The code shown on the clock was dismissed there.
    pub fn cancel_code(&self) {
        self.state.lock().unwrap().pairing = None;
    }

    pub fn handle(&self, mut request: Request) {
        let method = request.method().as_str().to_string();
        let path = request.url().split('?').next().unwrap_or("/").to_string();

        match (method.as_str(), path.as_str()) {
            ("GET", "/fonts/display.ttf") => respond_font(request, FONT_DISPLAY),
            ("GET", "/fonts/display-medium.ttf") => respond_font(request, FONT_DISPLAY_MEDIUM),
            ("GET", "/fonts/mono.ttf") => respond_font(request, FONT_MONO),

            ("GET", "/") => {
                let html = if self.is_authed(&request) {
                    render_settings(&Config::load(None), None)
                } else {
                    self.render_pairing(None)
                };
                respond_html(request, html);
            }

            ("POST", "/pair/start") => {
                let code = self.start_pairing();
                (self.notify)(SettingsEvent::ShowCode(code));
                redirect(request, "/");
            }

            ("POST", "/pair") => {
                let form = read_form(&mut request);
                let entered: String = field(&form, "code").chars().filter(|c| c.is_ascii_digit()).collect();
                match self.check_code(&entered) {
                    CodeResult::Ok(token) => {
                        (self.notify)(SettingsEvent::HideCode);
                        let cookie = format!(
                            "{}={}; Max-Age={}; Path=/; HttpOnly; SameSite=Strict",
                            COOKIE_NAME, token, COOKIE_MAX_AGE_SECS
                        );
                        let header = Header::from_bytes(&b"Set-Cookie"[..], cookie.as_bytes()).unwrap();
                        let location = Header::from_bytes(&b"Location"[..], &b"/"[..]).unwrap();
                        let _ = request.respond(Response::empty(303).with_header(header).with_header(location));
                    }
                    CodeResult::Wrong => {
                        let html = self.render_pairing(Some("That code didn't match. Check the clock and try again."));
                        respond_html(request, html);
                    }
                    CodeResult::Locked => {
                        (self.notify)(SettingsEvent::HideCode);
                        let html = self.render_pairing(Some("Too many wrong codes. Ask the clock for a new one."));
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
                    Ok(updated) => match updated.save(None) {
                        Ok(()) => {
                            (self.notify)(SettingsEvent::Applied(updated.clone()));
                            respond_html(request, render_settings(&updated, Some(Banner::Saved)));
                        }
                        Err(e) => {
                            eprintln!("Settings page: save failed: {}", e);
                            let msg = format!("Couldn't save settings: {}", e);
                            respond_html(request, render_settings(&current, Some(Banner::Error(msg))));
                        }
                    },
                    Err(msg) => respond_html(request, render_settings(&current, Some(Banner::Error(msg)))),
                }
            }

            ("POST", "/update/check") | ("POST", "/update/install") => {
                if !self.is_authed(&request) {
                    return redirect(request, "/");
                }
                let mode = if path == "/update/install" { update::Mode::Now } else { update::Mode::Check };
                let banner = match update::trigger(mode) {
                    Ok(()) if mode == update::Mode::Now => Banner::Info(
                        "Updating. The clock restarts when it's done; this page will show the result.".into(),
                    ),
                    Ok(()) => Banner::Info("Checking for updates...".into()),
                    Err(e) => Banner::Error(format!("Couldn't start the updater: {}", e)),
                };
                respond_html(request, render_settings(&Config::load(None), Some(banner)));
            }

            ("POST", "/forget") => {
                if let Some(token) = session_token(&request) {
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
            Some(token) => self.state.lock().unwrap().sessions.iter().any(|t| *t == token),
            None => false,
        }
    }

    fn start_pairing(&self) -> String {
        let code = format!("{:06}", random_u32() % 1_000_000);
        self.state.lock().unwrap().pairing = Some(Pairing {
            code: code.clone(),
            expires: Instant::now() + CODE_TTL,
            attempts: 0,
        });
        code
    }

    fn check_code(&self, entered: &str) -> CodeResult {
        let mut state = self.state.lock().unwrap();
        let Some(pairing) = state.pairing.as_mut() else {
            return CodeResult::NoCode;
        };
        if Instant::now() > pairing.expires {
            state.pairing = None;
            return CodeResult::NoCode;
        }
        if entered == pairing.code {
            state.pairing = None;
            let token = random_hex(16);
            state.sessions.push(token.clone());
            let excess = state.sessions.len().saturating_sub(MAX_SESSIONS);
            state.sessions.drain(..excess);
            self.save_sessions(&state.sessions);
            return CodeResult::Ok(token);
        }
        pairing.attempts += 1;
        if pairing.attempts >= MAX_CODE_ATTEMPTS {
            state.pairing = None;
            return CodeResult::Locked;
        }
        CodeResult::Wrong
    }

    fn save_sessions(&self, sessions: &[String]) {
        let json = serde_json::to_string(sessions).unwrap_or_else(|_| "[]".into());
        if let Err(e) = std::fs::write(&self.sessions_path, json) {
            eprintln!("Settings page: couldn't save sessions to {}: {}", self.sessions_path.display(), e);
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&self.sessions_path, std::fs::Permissions::from_mode(0o600));
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
</section>"#
            )
        };

        page("Pair", &body, &Config::load(None))
    }
}

enum CodeResult {
    Ok(String),
    Wrong,
    Locked,
    NoCode,
}

enum Banner {
    Saved,
    Info(String),
    Error(String),
}

/// Validate the submitted form and return the updated config.
fn apply_form(current: &Config, form: &[(String, String)]) -> Result<Config, String> {
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
        current.accent.clone()
    } else if ACCENTS.iter().any(|(id, _, _)| *id == accent) {
        accent
    } else {
        return Err("Pick a colour.".into());
    };

    let background = field(form, "background");
    let background = if background.is_empty() {
        current.background.clone()
    } else if BACKGROUNDS.iter().any(|(id, ..)| *id == background) {
        background
    } else {
        return Err("Pick a background.".into());
    };

    let mut updated = current.clone();
    updated.accent = accent;
    updated.background = background;
    updated.update_channel = channel;
    updated.auto_update = form.iter().any(|(k, _)| k == "auto_update");
    updated.latitude = (lat * 1e6).round() / 1e6;
    updated.longitude = (lon * 1e6).round() / 1e6;
    updated.radius_nm = (radius * 10.0).round() / 10.0;
    updated.facing_direction = facing;
    updated.clock_face = face;
    // Unchecked checkboxes aren't submitted
    updated.show_date = form.iter().any(|(k, _)| k == "show_date");
    Ok(updated)
}

fn render_settings(cfg: &Config, banner: Option<Banner>) -> String {
    let banner = match banner {
        Some(Banner::Saved) => r#"<p class="banner ok">Saved. The clock has updated.</p>"#.to_string(),
        Some(Banner::Info(msg)) => format!(r#"<p class="banner info">{}</p>"#, html_escape(&msg)),
        Some(Banner::Error(msg)) => format!(r#"<p class="banner error">{}</p>"#, html_escape(&msg)),
        None => String::new(),
    };
    let updates = render_updates(cfg);

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

    let body = format!(
        r#"{banner}
<form method="POST" action="/settings" class="stack">
  <section class="card">
    <h2 class="label">Location</h2>
    <div class="row">
      <input id="postcode" placeholder="UK postcode" autocapitalize="characters" autocomplete="postal-code">
      <button type="button" class="secondary" id="lookup">Look up</button>
    </div>
    <p class="hint" id="lookup-status">Looks up the postcode on postcodes.io from this phone.</p>
    <div class="row">
      <div class="field"><label class="label" for="latitude">Latitude</label>
        <input id="latitude" name="latitude" inputmode="decimal" value="{lat}" required></div>
      <div class="field"><label class="label" for="longitude">Longitude</label>
        <input id="longitude" name="longitude" inputmode="decimal" value="{lon}" required></div>
    </div>
    <a class="hint" id="map-link" href="https://www.openstreetmap.org/?mlat={lat}&amp;mlon={lon}#map=13/{lat}/{lon}" target="_blank" rel="noopener">Check on a map</a>
    <div class="field"><label class="label" for="radius_nm">Radar radius (nautical miles)</label>
      <input id="radius_nm" name="radius_nm" type="number" min="{rmin}" max="{rmax}" step="0.5" value="{radius}" required></div>
  </section>

  <section class="card">
    <h2 class="label">Facing</h2>
    <p class="hint">Which way the clock faces. Sets the top of the radar.</p>
    <div class="pills">{facing_options}</div>
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
<script>
(function () {{
  var pc = document.getElementById('postcode');
  var lat = document.getElementById('latitude');
  var lon = document.getElementById('longitude');
  var status = document.getElementById('lookup-status');
  var map = document.getElementById('map-link');
  function updateMap() {{
    var a = lat.value.trim(), o = lon.value.trim();
    map.href = 'https://www.openstreetmap.org/?mlat=' + a + '&mlon=' + o + '#map=13/' + a + '/' + o;
  }}
  lat.addEventListener('input', updateMap);
  lon.addEventListener('input', updateMap);
  function lookup() {{
    var code = pc.value.trim();
    if (!code) {{ pc.focus(); return; }}
    status.textContent = 'Looking up ' + code + '...';
    fetch('https://api.postcodes.io/postcodes/' + encodeURIComponent(code))
      .then(function (r) {{ return r.json(); }})
      .then(function (j) {{
        if (j.status !== 200 || !j.result) throw new Error();
        lat.value = j.result.latitude.toFixed(6);
        lon.value = j.result.longitude.toFixed(6);
        updateMap();
        status.textContent = 'Found ' + j.result.postcode + ' (' + j.result.admin_district + '). Save to apply.';
      }})
      .catch(function () {{ status.textContent = "Couldn't find that postcode."; }});
  }}
  document.getElementById('lookup').addEventListener('click', lookup);
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
  pc.addEventListener('keydown', function (e) {{ if (e.key === 'Enter') {{ e.preventDefault(); lookup(); }} }});
}})();
</script>"#,
        banner = banner,
        lat = cfg.latitude,
        lon = cfg.longitude,
        radius = cfg.radius_nm,
        rmin = RADIUS_MIN_NM,
        rmax = RADIUS_MAX_NM,
        facing_options = facing_options,
        accent_options = accent_options,
        background_options = background_options,
        analog = face_checked("analog"),
        digital = face_checked("digital"),
        date = if cfg.show_date { " checked" } else { "" },
        updates = updates,
        stable = if cfg.update_channel != "beta" { " checked" } else { "" },
        beta = if cfg.update_channel == "beta" { " checked" } else { "" },
        auto = if cfg.auto_update { " checked" } else { "" },
        update_now = if update::read_status().is_some_and(|s| s.state == "available" || s.state == "failed") {
            r#"<form method="POST" action="/update/install"><button type="submit" class="primary">Update now</button></form>"#
        } else {
            ""
        },
    );

    page("Settings", &body, cfg)
}

/// Version and last update result, for the Updates card.
fn render_updates(_cfg: &Config) -> String {
    let version = format!("<b>FlightClock {}</b>", html_escape(update::VERSION));
    let Some(status) = update::read_status() else {
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

fn page(title: &str, body: &str, cfg: &Config) -> String {
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
input:not([type=radio]):not([type=checkbox]){{width:100%;background:var(--bg);color:var(--fg);border:1px solid var(--line);border-radius:12px;padding:12px 14px;font:inherit;font-size:16px}}
input:focus{{outline:2px solid var(--accent);outline-offset:1px;border-color:transparent}}
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

fn respond_font(request: Request, bytes: &'static [u8]) {
    let ct = Header::from_bytes(&b"Content-Type"[..], &b"font/ttf"[..]).unwrap();
    let cache = Header::from_bytes(&b"Cache-Control"[..], &b"public, max-age=604800"[..]).unwrap();
    let _ = request.respond(Response::from_data(bytes).with_header(ct).with_header(cache));
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

fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    match std::fs::File::open("/dev/urandom").and_then(|mut f| f.read_exact(&mut buf)) {
        Ok(()) => buf,
        Err(e) => panic!("no randomness source for settings pairing: {}", e),
    }
}

fn random_u32() -> u32 {
    let b = random_bytes(4);
    u32::from_le_bytes([b[0], b[1], b[2], b[3]])
}

fn random_hex(n: usize) -> String {
    random_bytes(n).iter().map(|b| format!("{:02x}", b)).collect()
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
        assert_eq!(cfg.latitude, 51.47);
        assert_eq!(cfg.longitude, -0.4543);
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

    #[test]
    fn pairing_code_flow() {
        let web = SettingsWeb {
            state: Mutex::new(State { pairing: None, sessions: vec![] }),
            notify: Box::new(|_| {}),
            sessions_path: std::env::temp_dir().join("fc-test-sessions.json"),
        };
        assert!(matches!(web.check_code("123456"), CodeResult::NoCode));

        let code = web.start_pairing();
        assert_eq!(code.len(), 6);
        let wrong = if code == "000000" { "111111" } else { "000000" };
        for _ in 0..MAX_CODE_ATTEMPTS - 1 {
            assert!(matches!(web.check_code(wrong), CodeResult::Wrong));
        }
        assert!(matches!(web.check_code(wrong), CodeResult::Locked));
        assert!(matches!(web.check_code(&code), CodeResult::NoCode));

        let code = web.start_pairing();
        match web.check_code(&code) {
            CodeResult::Ok(token) => {
                assert_eq!(token.len(), 32);
                assert!(web.state.lock().unwrap().sessions.contains(&token));
            }
            _ => panic!("correct code rejected"),
        }
    }
}
