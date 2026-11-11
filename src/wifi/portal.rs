//! The app's web server: WiFi setup portal and settings page.
//!
//! Runs for the life of the app. While the setup hotspot is up
//! (`PortalState::active`) it is the captive portal for joining WiFi. The rest
//! of the time it serves the settings page (src/settings_web.rs), which needs
//! a paired phone; the WiFi setup pages are never reachable from the LAN.

use crate::settings_web::SettingsWeb;
use crate::wifi::manager::{ScannedNetwork, AP_IP, AP_SSID};
use crate::wifi::supervisor::WifiCommand;
use std::io::Read;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tiny_http::{Header, Request, Response, Server};

const MAX_BODY_BYTES: u64 = 8 * 1024;

/// State shared between the WiFi supervisor and the portal.
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
    commands: Sender<WifiCommand>,
    settings: Arc<SettingsWeb>,
    port: u16,
}

impl Portal {
    pub fn new(
        state: Arc<Mutex<PortalState>>,
        commands: Sender<WifiCommand>,
        settings: Arc<SettingsWeb>,
        port: u16,
    ) -> Self {
        Self {
            state,
            commands,
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
            let active = {
                let mut state = self.state.lock().unwrap();
                if state.active {
                    state.last_request = Some(Instant::now());
                }
                state.active
            };
            if !active {
                self.settings.handle(request);
                continue;
            }

            let method = request.method().as_str().to_string();
            let path = request.url().split('?').next().unwrap_or("/").to_string();

            match (method.as_str(), path.as_str()) {
                ("POST", "/connect") => self.handle_connect(request),
                ("GET", "/") => {
                    let html = self.render_setup_page();
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
        let field = |name: &str| form.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone());

        // A typed-in name wins over the radio list (hidden or out-of-scan networks).
        let ssid = field("ssid_other")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .or_else(|| field("ssid").filter(|s| !s.is_empty()));

        let Some(ssid) = ssid else {
            self.state.lock().unwrap().message = Some("Pick a network, or type its name.".to_string());
            let html = self.render_setup_page();
            respond_html(request, html);
            return;
        };
        let password = field("password").unwrap_or_default();

        respond_html(request, render_connecting_page(&ssid));

        // The supervisor drops the hotspot to try the network, so the phone
        // will disconnect; if it fails, the hotspot returns with the error.
        let _ = self.commands.send(WifiCommand::Connect { ssid, password });
    }

    fn render_setup_page(&self) -> String {
        let (networks, message) = {
            let state = self.state.lock().unwrap();
            (state.networks.clone(), state.message.clone())
        };

        let mut options = String::new();
        for net in &networks {
            let bars = match net.signal {
                75..=100 => "████",
                50..=74 => "███░",
                25..=49 => "██░░",
                _ => "█░░░",
            };
            let lock = if net.encrypted { "🔒" } else { "" };
            options.push_str(&format!(
                r#"<label class="net"><input type="radio" name="ssid" value="{v}"><span class="name">{v}</span><span class="lock">{lock}</span><span class="signal">{bars}</span></label>"#,
                v = html_escape(&net.ssid),
            ));
        }
        if networks.is_empty() {
            options.push_str(r#"<p class="hint">No networks found. Type the name below.</p>"#);
        }

        let banner = message
            .map(|m| format!(r#"<div class="banner">{}</div>"#, html_escape(&m)))
            .unwrap_or_default();

        page(
            "FlightClock WiFi",
            &format!(
                r#"<h1>FlightClock WiFi</h1>
{banner}
<form method="POST" action="/connect">
<div class="nets">{options}</div>
<label class="lbl" for="other">Or type a network name</label>
<input type="text" id="other" name="ssid_other" autocapitalize="none" autocorrect="off" placeholder="Network name">
<label class="lbl" for="pw">Password</label>
<input type="password" id="pw" name="password" placeholder="Leave blank for open networks">
<button type="submit">Connect</button>
</form>"#
            ),
        )
    }
}

fn respond_html(request: Request, html: String) {
    let header = Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap();
    let _ = request.respond(Response::from_string(html).with_header(header));
}

fn render_connecting_page(ssid: &str) -> String {
    page(
        "Connecting",
        &format!(
            r#"<div class="center"><div class="spinner"></div>
<h1>Joining {ssid}</h1>
<p>Your phone will now drop off <b>{ap}</b>. That's expected.</p>
<p>Watch the clock's screen. If it can't connect, <b>{ap}</b> will come back. Rejoin it to see what went wrong.</p></div>"#,
            ssid = html_escape(ssid),
            ap = AP_SSID
        ),
    )
}

fn page(title: &str, body: &str) -> String {
    format!(
        r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>{title}</title>
<style>
*{{margin:0;padding:0;box-sizing:border-box}}
body{{font-family:-apple-system,system-ui,sans-serif;background:#1a1a2e;color:#fff;padding:20px;max-width:420px;margin:0 auto;line-height:1.4}}
h1{{text-align:center;margin:16px 0;font-size:24px}}
p{{color:#bbb;margin:12px 0;text-align:center}}
.banner{{background:#7f1d1d;border:1px solid #f87171;border-radius:8px;padding:12px;margin:12px 0}}
.nets{{margin:16px 0;max-height:320px;overflow-y:auto}}
.net{{display:flex;align-items:center;gap:8px;padding:12px;border:1px solid #333;border-radius:8px;margin:8px 0;cursor:pointer}}
.net:has(input:checked){{border-color:#60a5fa;background:#60a5fa22}}
.net .name{{flex:1;font-size:16px;overflow-wrap:anywhere}}
.net .signal{{font-family:monospace;font-size:12px;color:#888}}
.hint{{color:#888}}
label.lbl{{display:block;margin:14px 0 4px;font-size:14px;color:#aaa}}
input[type=text],input[type=password]{{width:100%;padding:12px;border-radius:8px;border:1px solid #333;background:#111;color:#fff;font-size:16px}}
button{{width:100%;padding:14px;border:none;border-radius:8px;background:#3b82f6;color:#fff;font-size:18px;font-weight:600;margin-top:20px}}
.center{{text-align:center;padding-top:40px}}
.spinner{{width:40px;height:40px;border:4px solid #333;border-top-color:#60a5fa;border-radius:50%;animation:spin 1s linear infinite;margin:20px auto}}
@keyframes spin{{to{{transform:rotate(360deg)}}}}
</style></head><body>
{body}
</body></html>"#
    )
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
    fn parse_form_fields() {
        let f = parse_form("ssid=Home&ssid_other=&password=a%3Db");
        assert_eq!(f[0], ("ssid".into(), "Home".into()));
        assert_eq!(f[1], ("ssid_other".into(), "".into()));
        assert_eq!(f[2], ("password".into(), "a=b".into()));
    }
}
