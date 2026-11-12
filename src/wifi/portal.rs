//! The web server: WiFi setup portal and settings page.
//!
//! Runs in the web server process (`flight-clock web`, see src/ipc.rs). While
//! the setup hotspot is up (`PortalState::active`) it is the captive portal
//! for joining WiFi. The rest of the time it serves the settings page
//! (src/settings_web.rs), which needs a paired phone; the WiFi setup pages
//! are never reachable from the LAN.

use crate::config::Config;
use crate::ipc::Clock;
use crate::settings_web::{self, SettingsWeb};
use crate::wifi::manager::{ScannedNetwork, AP_IP, AP_SSID};
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tiny_http::{Header, Request, Response, Server};

const MAX_BODY_BYTES: u64 = 8 * 1024;

/// WiFi setup state. The clock's WiFi supervisor owns it; the web server
/// process keeps a copy, sent over whenever it changes.
#[derive(Default)]
pub struct PortalState {
    /// True while the setup hotspot is up and the portal should answer.
    pub active: bool,
    /// Networks found by the scan made just before the hotspot came up.
    pub networks: Vec<ScannedNetwork>,
    /// Message for the top of the setup page, e.g. why the last attempt failed.
    pub message: Option<String>,
    /// When a phone last loaded a page; used to avoid tearing the hotspot
    /// down under someone who's mid-setup.
    pub last_request: Option<Instant>,
}

pub struct Portal {
    state: Arc<Mutex<PortalState>>,
    clock: Arc<dyn Clock>,
    settings: Arc<SettingsWeb>,
    port: u16,
}

impl Portal {
    pub fn new(
        state: Arc<Mutex<PortalState>>,
        clock: Arc<dyn Clock>,
        settings: Arc<SettingsWeb>,
        port: u16,
    ) -> Self {
        Self {
            state,
            clock,
            settings,
            port,
        }
    }

    /// Serve forever.
    pub fn serve(&self) {
        let server = match Server::http(("0.0.0.0", self.port)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Failed to start web server on port {}: {}", self.port, e);
                return;
            }
        };
        eprintln!("Web server listening on port {}", self.port);

        for request in server.incoming_requests() {
            let active = self.state.lock().unwrap().active;
            if !active {
                self.settings.handle(request);
                continue;
            }
            // Tell the clock someone is mid-setup, so it keeps the hotspot up
            self.clock.portal_seen();

            let method = request.method().as_str().to_string();
            let (path, query) = match request.url().split_once('?') {
                Some((path, query)) => (path.to_string(), query.to_string()),
                None => (request.url().to_string(), String::new()),
            };

            // Setup pages live at the hotspot address. Phones probing for a
            // captive portal ask for other hosts and get redirected there.
            let at_portal = request
                .headers()
                .iter()
                .find(|h| h.field.equiv("Host"))
                .is_some_and(|h| is_portal_host(h.value.as_str()));

            match (method.as_str(), path.as_str()) {
                // The brand fonts the pages load
                ("GET", font_path) if settings_web::font(font_path).is_some() => {
                    settings_web::respond_font(request, settings_web::font(font_path).unwrap_or_default())
                }
                ("POST", "/connect") if at_portal => self.handle_connect(request),
                // Step 1: choose a network
                ("GET", "/") => {
                    let html = self.render_setup_page();
                    respond_html(request, html);
                }
                // Step 2: its password
                ("GET", "/join") if at_portal => {
                    let ssid = form_field(&parse_form(&query), "ssid").trim().to_string();
                    let html = if ssid.is_empty() {
                        self.state.lock().unwrap().message = Some("Pick a network, or type its name.".to_string());
                        self.render_setup_page()
                    } else {
                        render_password_page(&ssid, self.is_encrypted(&ssid), None)
                    };
                    respond_html(request, html);
                }
                _ => {
                    // Captive-portal probes (Apple, Android, Windows) and anything
                    // else: send them to the setup page.
                    let location = format!("http://{}/", AP_IP);
                    let header = Header::from_bytes(&b"Location"[..], location.as_bytes()).unwrap();
                    let _ = request.respond(
                        Response::from_string("Redirecting...").with_status_code(302).with_header(header),
                    );
                }
            }
        }
    }

    fn handle_connect(&self, mut request: Request) {
        let mut body = Vec::new();
        if request.as_reader().take(MAX_BODY_BYTES).read_to_end(&mut body).is_err() {
            let _ = request.respond(Response::from_string("Bad request").with_status_code(400));
            return;
        }
        let form = parse_form(&String::from_utf8_lossy(&body));
        let ssid = form_field(&form, "ssid").trim().to_string();
        let password = form_field(&form, "password");

        if ssid.is_empty() {
            self.state.lock().unwrap().message = Some("Pick a network, or type its name.".to_string());
            let html = self.render_setup_page();
            respond_html(request, html);
            return;
        }
        let encrypted = self.is_encrypted(&ssid);
        if encrypted == Some(true) && password.is_empty() {
            respond_html(request, render_password_page(&ssid, encrypted, Some("This network needs a password.")));
            return;
        }

        // The clock drops the hotspot to try the network, so the phone will
        // disconnect; if it fails, the hotspot returns with the error.
        match self.clock.wifi_connect(&ssid, &password) {
            Ok(()) => respond_html(request, render_connecting_page(&ssid)),
            Err(e) => respond_html(request, render_password_page(&ssid, encrypted, Some(&e))),
        }
    }

    /// Whether a network from the scan needs a password. None for a name
    /// that was typed in and isn't in the scan (hidden, or out of range then).
    fn is_encrypted(&self, ssid: &str) -> Option<bool> {
        self.state.lock().unwrap().networks.iter().find(|n| n.ssid == ssid).map(|n| n.encrypted)
    }

    fn render_setup_page(&self) -> String {
        let (networks, message) = {
            let state = self.state.lock().unwrap();
            (state.networks.clone(), state.message.clone())
        };
        render_networks_page(&networks, message.as_deref())
    }
}

/// Step 1: the networks the clock can see, each a link to step 2, and a box
/// for a name it can't see.
fn render_networks_page(networks: &[ScannedNetwork], message: Option<&str>) -> String {
    let mut list = String::new();
    for net in networks {
        let lock = if net.encrypted { LOCK_ICON } else { "" };
        list.push_str(&format!(
            r#"<a class="net" href="/join?ssid={href}"><span class="name">{name}</span>{lock}{bars}<span class="go">›</span></a>"#,
            href = url_encode(&net.ssid),
            name = html_escape(&net.ssid),
            bars = signal_icon(net.signal),
        ));
    }
    if networks.is_empty() {
        list.push_str(r#"<p class="hint">The clock didn't find any networks. Type the name below.</p>"#);
    }

    let banner = message
        .map(|m| format!(r#"<p class="banner error">{}</p>"#, html_escape(m)))
        .unwrap_or_default();

    page(
        "WiFi · Step 1 of 2",
        &format!(
            r#"{banner}
<div class="stack">
<section class="card">
  <p class="lead">Choose your WiFi network.</p>
  <div class="nets">{list}</div>
</section>
<section class="card">
  <form method="GET" action="/join" class="stack">
    <div class="field"><label class="label" for="other">Not listed? Type its name</label>
      <input type="text" id="other" name="ssid" autocapitalize="none" autocorrect="off" spellcheck="false" maxlength="32" placeholder="Network name" required></div>
    <button type="submit" class="secondary">Next</button>
  </form>
</section>
</div>"#
        ),
    )
}

/// A padlock, for networks that need a password.
const LOCK_ICON: &str = r#"<svg width="16" height="16" viewBox="0 0 16 16" role="img" aria-label="Needs a password"><path fill="currentColor" d="M4 7V5a4 4 0 0 1 8 0v2h1v7H3V7zm2 0h4V5a2 2 0 0 0-4 0z"/></svg>"#;

/// Four bars, as many lit as the signal (0-100) deserves.
fn signal_icon(signal: u8) -> String {
    let lit = match signal {
        75..=100 => 4,
        50..=74 => 3,
        25..=49 => 2,
        _ => 1,
    };
    let bars: String = (0..4)
        .map(|i| {
            let height = 3 * (i + 1);
            format!(
                r#"<rect x="{x}" y="{y}" width="3" height="{height}" rx="1" class="{class}"/>"#,
                x = i * 5,
                y = 12 - height,
                class = if i < lit { "on" } else { "off" },
            )
        })
        .collect();
    format!(r#"<svg width="18" height="12" viewBox="0 0 18 12" role="img" aria-label="Signal {lit} of 4">{bars}</svg>"#)
}

/// Step 2: the password for the chosen network. A network the scan saw as
/// open gets no password box, only a button to join. For a typed-in name the
/// clock can't tell, so the box is there and may be left empty.
fn render_password_page(ssid: &str, encrypted: Option<bool>, error: Option<&str>) -> String {
    let banner = error
        .map(|m| format!(r#"<p class="banner error">{}</p>"#, html_escape(m)))
        .unwrap_or_default();
    let name = html_escape(ssid);

    let (fields, button) = match encrypted {
        Some(false) => (r#"<p class="lead">This network has no password.</p>"#.to_string(), "Join"),
        known => (
            format!(
                r#"<div class="field"><label class="label" for="pw">WiFi password</label>
      <input type="password" id="pw" name="password" autocapitalize="none" autocorrect="off" spellcheck="false" maxlength="63" autofocus{required}></div>
    <label class="switch"><input type="checkbox" id="show"><span class="track"></span><span>Show password</span></label>
    {hint}
    <script>document.getElementById('show').addEventListener('change',function(){{document.getElementById('pw').type=this.checked?'text':'password'}});</script>"#,
                required = if known == Some(true) { r#" minlength="8" required"# } else { "" },
                hint = if known.is_none() { r#"<p class="hint">Leave it empty if the network has no password.</p>"# } else { "" },
            ),
            "Connect",
        ),
    };

    page(
        "WiFi · Step 2 of 2",
        &format!(
            r#"{banner}
<div class="stack">
<section class="card">
  <span class="label">Network</span>
  <p class="chosen">{name}</p>
  <form method="POST" action="/connect" class="stack">
    <input type="hidden" name="ssid" value="{name}">
    {fields}
    <button type="submit" class="primary">{button}</button>
  </form>
</section>
<a class="link hint" href="/">Choose a different network</a>
</div>"#
        ),
    )
}

fn form_field(form: &[(String, String)], name: &str) -> String {
    form.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone()).unwrap_or_default()
}

/// Percent-encode for a URL query value (everything but letters, digits and - _ . ~).
fn url_encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// "192.168.4.1" or "192.168.4.1:80".
fn is_portal_host(host: &str) -> bool {
    let host = host.trim();
    host == AP_IP || host.strip_prefix(AP_IP).is_some_and(|rest| rest.starts_with(':'))
}

fn respond_html(request: Request, html: String) {
    let header = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap();
    let _ = request.respond(Response::from_string(html).with_header(header));
}

fn render_connecting_page(ssid: &str) -> String {
    page(
        "WiFi · Connecting",
        &format!(
            r#"<section class="card center"><div class="spinner"></div>
<p class="lead">Joining <b>{ssid}</b></p>
<p class="hint">Your phone will now drop off <b>{ap}</b>. That's expected.</p>
<p class="hint">Watch the clock's screen. If it can't connect, <b>{ap}</b> will come back. Rejoin it to see what went wrong.</p></section>"#,
            ssid = html_escape(ssid),
            ap = AP_SSID
        ),
    )
}

/// The same shell as the settings page: brand fonts, the owner's colours.
fn page(title: &str, body: &str) -> String {
    settings_web::page(title, body, &Config::load(None))
}

pub(crate) fn parse_form(body: &str) -> Vec<(String, String)> {
    body.split('&')
        .filter(|p| !p.is_empty())
        .map(|pair| {
            let mut parts = pair.splitn(2, '=');
            let key = url_decode(parts.next().unwrap_or(""));
            let value = url_decode(parts.next().unwrap_or(""));
            (key, value)
        })
        .collect()
}

pub(crate) fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}

/// Decode application/x-www-form-urlencoded text. Works on bytes so
/// multi-byte UTF-8 (e.g. "Café", "£" in a password) survives.
fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).ok();
                match hex.and_then(|h| u8::from_str_radix(h, 16).ok()) {
                    Some(b) => {
                        out.push(b);
                        i += 2;
                    }
                    None => out.push(b'%'),
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_decode_handles_utf8_and_plus() {
        assert_eq!(url_decode("Caf%C3%A9+Net"), "Café Net");
        assert_eq!(url_decode("p%C2%A3ss%26word"), "p£ss&word");
        assert_eq!(url_decode("100%"), "100%");
        assert_eq!(url_decode("%zz"), "%zz");
    }

    #[test]
    fn setup_forms_only_at_the_hotspot_address() {
        assert!(is_portal_host("192.168.4.1"));
        assert!(is_portal_host("192.168.4.1:80"));
        assert!(!is_portal_host("captive.apple.com"));
        assert!(!is_portal_host("192.168.4.10"));
        assert!(!is_portal_host("192.168.4.1.evil.example"));
    }

    fn net(ssid: &str, encrypted: bool) -> ScannedNetwork {
        ScannedNetwork { ssid: ssid.into(), signal: 80, encrypted }
    }

    #[test]
    fn step_one_links_each_network_to_step_two() {
        let html = render_networks_page(&[net("Home", true), net("Café & <Bar>", false)], Some("Wrong password."));
        assert!(html.contains(r#"href="/join?ssid=Home""#));
        assert!(html.contains(r#"href="/join?ssid=Caf%C3%A9%20%26%20%3CBar%3E""#));
        assert!(html.contains("Café &amp; &lt;Bar&gt;"));
        assert!(html.contains("Wrong password."));
        // No password box on this step
        assert!(!html.contains(r#"type="password""#));
        assert!(html.contains(r#"action="/join""#));
    }

    #[test]
    fn links_survive_a_round_trip() {
        let ssid = "Café & <Bar> 100%+";
        let form = parse_form(&format!("ssid={}", url_encode(ssid)));
        assert_eq!(form_field(&form, "ssid"), ssid);
    }

    #[test]
    fn step_two_asks_for_a_password_only_when_needed() {
        let secured = render_password_page("Home", Some(true), None);
        assert!(secured.contains(r#"type="password""#) && secured.contains("required"));
        assert!(secured.contains(r#"name="ssid" value="Home""#));

        let open = render_password_page("Cafe", Some(false), None);
        assert!(!open.contains(r#"type="password""#));
        assert!(open.contains("no password"));

        // Typed in: can't tell, so the box is optional
        let unknown = render_password_page("Hidden", None, Some("This network needs a password."));
        assert!(unknown.contains(r#"type="password""#) && !unknown.contains(" required"));
        assert!(unknown.contains("Leave it empty"));
        assert!(unknown.contains("This network needs a password."));

        let odd = render_password_page(r#"a"><script>"#, Some(true), None);
        assert!(!odd.contains(r#"a"><script>"#) && odd.contains("a&quot;&gt;&lt;script&gt;"));
    }

    #[test]
    fn pages_use_the_brand_shell_not_their_own_styles() {
        for html in [
            render_networks_page(&[net("Home", true)], None),
            render_password_page("Home", Some(true), None),
            render_connecting_page("Home"),
        ] {
            assert!(html.contains("Flight<span>Clock</span>"));
            assert!(html.contains("/fonts/display.ttf"));
            // The old portal's colours and emoji
            for old in ["#1a1a2e", "#3b82f6", "🔒", "█"] {
                assert!(!html.contains(old), "{}", old);
            }
        }
        assert!(settings_web::font("/fonts/mono.ttf").is_some());
        assert!(settings_web::font("/fonts/../etc/passwd").is_none());
    }

    #[test]
    fn signal_bars_follow_the_strength() {
        assert_eq!(signal_icon(90).matches(r#"class="on""#).count(), 4);
        assert_eq!(signal_icon(60).matches(r#"class="on""#).count(), 3);
        assert_eq!(signal_icon(30).matches(r#"class="on""#).count(), 2);
        assert_eq!(signal_icon(5).matches(r#"class="on""#).count(), 1);
        assert!(render_networks_page(&[net("Open", false)], None).matches("Needs a password").count() == 0);
        assert!(render_networks_page(&[net("Home", true)], None).contains("Needs a password"));
    }

    #[test]
    fn parse_form_fields() {
        let f = parse_form("ssid=Home&ssid_other=&password=a%3Db");
        assert_eq!(f[0], ("ssid".into(), "Home".into()));
        assert_eq!(f[1], ("ssid_other".into(), "".into()));
        assert_eq!(f[2], ("password".into(), "a=b".into()));
    }
}
