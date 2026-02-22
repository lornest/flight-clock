use crate::wifi::manager::WifiManager;
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Response, Server};

/// Captive portal HTTP server that serves WiFi setup pages.
pub struct Portal {
    wifi: Arc<Mutex<WifiManager>>,
    /// Set to true when WiFi connection succeeds, signaling main thread to transition.
    pub connected: Arc<std::sync::atomic::AtomicBool>,
}

impl Portal {
    pub fn new(wifi: Arc<Mutex<WifiManager>>, connected: Arc<std::sync::atomic::AtomicBool>) -> Self {
        Self { wifi, connected }
    }

    /// Start the HTTP server on port 80. Blocks indefinitely.
    pub fn serve(&self) {
        let server = match Server::http("0.0.0.0:80") {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Failed to start portal server: {}", e);
                return;
            }
        };

        eprintln!("Captive portal running on port 80");

        for request in server.incoming_requests() {
            let url = request.url().to_string();
            let method = request.method().as_str().to_string();

            eprintln!("Portal request: {} {}", method, url);

            match (method.as_str(), url.as_str()) {
                ("GET", "/") | ("GET", "/generate_204") | ("GET", "/hotspot-detect.html")
                | ("GET", "/connecttest.txt") | ("GET", "/redirect")
                | ("GET", "/ncsi.txt") | ("GET", "/success.txt") => {
                    // All captive portal detection endpoints serve the setup page
                    let networks = {
                        let wifi = self.wifi.lock().unwrap();
                        wifi.scan_networks()
                    };
                    let html = self.render_setup_page(&networks);
                    let header = Header::from_bytes(
                        "Content-Type".as_bytes(),
                        "text/html; charset=utf-8".as_bytes(),
                    )
                    .unwrap();
                    let response = Response::from_string(html).with_header(header);
                    let _ = request.respond(response);
                }
                ("POST", "/connect") => {
                    self.handle_connect(request);
                }
                ("GET", "/success") => {
                    let html = self.render_success_page();
                    let header = Header::from_bytes(
                        "Content-Type".as_bytes(),
                        "text/html; charset=utf-8".as_bytes(),
                    )
                    .unwrap();
                    let response = Response::from_string(html).with_header(header);
                    let _ = request.respond(response);
                }
                _ => {
                    // Redirect everything else to setup page (captive portal behavior)
                    let header = Header::from_bytes(
                        "Location".as_bytes(),
                        "http://192.168.4.1/".as_bytes(),
                    )
                    .unwrap();
                    let response =
                        Response::from_string("Redirecting...").with_status_code(302).with_header(header);
                    let _ = request.respond(response);
                }
            }
        }
    }

    fn handle_connect(&self, mut request: tiny_http::Request) {
        // Read POST body
        let mut body = String::new();
        if let Err(e) = request.as_reader().read_to_string(&mut body) {
            eprintln!("Failed to read request body: {}", e);
            let response = Response::from_string("Bad request").with_status_code(400);
            let _ = request.respond(response);
            return;
        }

        // Parse form data (ssid=...&password=...)
        let params: std::collections::HashMap<String, String> = body
            .split('&')
            .filter_map(|pair| {
                let mut parts = pair.splitn(2, '=');
                let key = parts.next()?.to_string();
                let value = parts.next().unwrap_or("").to_string();
                Some((
                    url_decode(&key),
                    url_decode(&value),
                ))
            })
            .collect();

        let ssid = match params.get("ssid") {
            Some(s) if !s.is_empty() => s.clone(),
            _ => {
                let html = self.render_error_page("Please select a WiFi network");
                let header = Header::from_bytes(
                    "Content-Type".as_bytes(),
                    "text/html; charset=utf-8".as_bytes(),
                )
                .unwrap();
                let response = Response::from_string(html).with_header(header);
                let _ = request.respond(response);
                return;
            }
        };

        let password = params.get("password").cloned().unwrap_or_default();

        // Send "connecting" response
        let html = self.render_connecting_page(&ssid);
        let header = Header::from_bytes(
            "Content-Type".as_bytes(),
            "text/html; charset=utf-8".as_bytes(),
        )
        .unwrap();
        let response = Response::from_string(html).with_header(header);
        let _ = request.respond(response);

        // Attempt connection
        let mut wifi = self.wifi.lock().unwrap();
        match wifi.connect(&ssid, &password) {
            Ok(true) => {
                eprintln!("WiFi connection successful!");
                self.connected
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            Ok(false) => {
                eprintln!("WiFi connection failed (timeout)");
                // AP mode will be resumed by the main thread
            }
            Err(e) => {
                eprintln!("WiFi connection error: {}", e);
            }
        }
    }

    fn render_setup_page(&self, networks: &[crate::wifi::manager::ScannedNetwork]) -> String {
        let mut network_options = String::new();
        for net in networks {
            let signal_bars = match net.signal_strength {
                s if s > -50 => "████",
                s if s > -60 => "███░",
                s if s > -70 => "██░░",
                _ => "█░░░",
            };
            network_options.push_str(&format!(
                r#"<label class="net"><input type="radio" name="ssid" value="{}"><span class="name">{}</span><span class="signal">{}</span></label>"#,
                html_escape(&net.ssid),
                html_escape(&net.ssid),
                signal_bars,
            ));
        }

        format!(
            r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>FlightClock Setup</title>
<style>
*{{margin:0;padding:0;box-sizing:border-box}}
body{{font-family:-apple-system,sans-serif;background:#1a1a2e;color:#fff;padding:20px;max-width:400px;margin:0 auto}}
h1{{text-align:center;margin:20px 0;font-size:24px}}
.nets{{margin:16px 0;max-height:300px;overflow-y:auto}}
.net{{display:flex;align-items:center;padding:12px;border:1px solid #333;border-radius:8px;margin:8px 0;cursor:pointer}}
.net:hover,.net:has(input:checked){{border-color:#60a5fa;background:#60a5fa22}}
.net input{{margin-right:12px}}
.net .name{{flex:1;font-size:16px}}
.net .signal{{font-family:monospace;font-size:12px;color:#888}}
label.lbl{{display:block;margin:12px 0 4px;font-size:14px;color:#aaa}}
input[type=password]{{width:100%;padding:12px;border-radius:8px;border:1px solid #333;background:#111;color:#fff;font-size:16px}}
button{{width:100%;padding:14px;border:none;border-radius:8px;background:#3b82f6;color:#fff;font-size:18px;font-weight:600;cursor:pointer;margin-top:16px}}
button:hover{{background:#2563eb}}
.sub{{text-align:center;color:#666;margin-top:20px;font-size:12px}}
</style></head><body>
<h1>FlightClock Setup</h1>
<form method="POST" action="/connect">
<div class="nets">{}</div>
<label class="lbl">Password</label>
<input type="password" name="password" placeholder="Enter WiFi password">
<button type="submit">Connect</button>
</form>
<p class="sub">FlightClock WiFi Setup</p>
</body></html>"#,
            network_options
        )
    }

    fn render_connecting_page(&self, ssid: &str) -> String {
        format!(
            r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Connecting...</title>
<style>
body{{font-family:-apple-system,sans-serif;background:#1a1a2e;color:#fff;display:flex;align-items:center;justify-content:center;min-height:100vh;text-align:center}}
.msg{{font-size:20px;margin:20px}}
.spinner{{width:40px;height:40px;border:4px solid #333;border-top:4px solid #60a5fa;border-radius:50%;animation:spin 1s linear infinite;margin:20px auto}}
@keyframes spin{{to{{transform:rotate(360deg)}}}}
</style>
<meta http-equiv="refresh" content="20;url=/"></head><body>
<div><div class="spinner"></div><div class="msg">Connecting to {}</div><p style="color:#888">This may take up to 15 seconds...</p></div>
</body></html>"#,
            html_escape(ssid)
        )
    }

    fn render_success_page(&self) -> String {
        r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Connected!</title>
<style>
body{font-family:-apple-system,sans-serif;background:#1a1a2e;color:#fff;display:flex;align-items:center;justify-content:center;min-height:100vh;text-align:center}
.check{font-size:60px;margin:20px}
.msg{font-size:24px;margin:20px}
</style></head><body>
<div><div class="check">✓</div><div class="msg">Connected!</div><p style="color:#888">FlightClock is now starting up.</p></div>
</body></html>"#.to_string()
    }

    fn render_error_page(&self, message: &str) -> String {
        format!(
            r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1">
<title>Error</title>
<style>
body{{font-family:-apple-system,sans-serif;background:#1a1a2e;color:#fff;display:flex;align-items:center;justify-content:center;min-height:100vh;text-align:center}}
.err{{color:#f87171;font-size:20px;margin:20px}}
a{{color:#60a5fa}}
</style>
<meta http-equiv="refresh" content="3;url=/"></head><body>
<div><div class="err">{}</div><a href="/">Try again</a></div>
</body></html>"#,
            html_escape(message)
        )
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn url_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.bytes();

    while let Some(b) = chars.next() {
        match b {
            b'+' => result.push(' '),
            b'%' => {
                let hi = chars.next().unwrap_or(b'0');
                let lo = chars.next().unwrap_or(b'0');
                let hex = [hi, lo];
                if let Ok(hex_str) = std::str::from_utf8(&hex) {
                    if let Ok(val) = u8::from_str_radix(hex_str, 16) {
                        result.push(val as char);
                    }
                }
            }
            _ => result.push(b as char),
        }
    }

    result
}
