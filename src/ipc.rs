//! The link between the clock and its web server.
//!
//! FlightClock runs as two processes:
//!
//! - the **clock** (`flight-clock`, root): screen, touch, WiFi, updates, and
//!   the only writer of config.json;
//! - the **web server** (`flight-clock web`, the unprivileged
//!   `flight-clock-web` user, sandboxed by its systemd unit): the settings
//!   page and the WiFi setup portal. It is the part that parses requests from
//!   the network, so it holds no privileges of its own.
//!
//! They talk over a Unix socket that the clock owns: one JSON message per
//! line. The web server can only *ask*: show a pairing code, save these
//! settings, start an update, join this WiFi. The clock checks every request
//! before acting on it, as it would input from the network.

use crate::config::Config;
use crate::wifi::manager::ScannedNetwork;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The web server's user (created by install.sh / the 0001 migration).
pub const WEB_USER: &str = "flight-clock-web";

const SOCKET_PATH: &str = "/run/flight-clock/web.sock";
/// Longest message accepted from the other side.
const MAX_LINE_BYTES: u64 = 64 * 1024;
/// How long the web server waits for the clock to answer a request.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
const RECONNECT_EVERY: Duration = Duration::from_secs(1);
/// At most one "someone is using the portal" note per this long.
const PORTAL_SEEN_EVERY: Duration = Duration::from_secs(2);

/// Socket path: $FLIGHT_CLOCK_WEB_SOCKET, else /run/flight-clock/web.sock on
/// the Pi and a per-user file in the temp folder on a dev machine.
pub fn socket_path() -> PathBuf {
    if let Some(path) = std::env::var_os("FLIGHT_CLOCK_WEB_SOCKET") {
        return PathBuf::from(path);
    }
    if cfg!(target_os = "linux") {
        PathBuf::from(SOCKET_PATH)
    } else {
        std::env::temp_dir().join("flight-clock-web.sock")
    }
}

/// Web server -> clock.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FromWeb {
    /// Show this pairing code on the screen.
    ShowCode { code: String },
    /// Hide the pairing code (paired, expired or too many attempts).
    HideCode,
    /// Save and apply these settings. Answered with `Reply`.
    SaveSettings { id: u64, config: Config },
    /// Start the updater: install, or only check. Answered with `Reply`.
    TriggerUpdate { id: u64, install: bool },
    /// Credentials submitted through the WiFi setup portal.
    WifiConnect { ssid: String, password: String },
    /// A phone loaded a setup portal page.
    PortalSeen,
    /// A phone paired by scanning the QR code on the clock.
    LinkUsed,
}

/// Clock -> web server.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ToWeb {
    /// WiFi setup state: whether the hotspot is up (serve the portal instead
    /// of the settings page), and what to show on it.
    Portal {
        active: bool,
        networks: Vec<ScannedNetwork>,
        message: Option<String>,
    },
    /// The pairing code was dismissed on the clock.
    CancelCode,
    /// The one-time token in the QR code the clock is showing (None: no QR
    /// code is up). Opening /s?k=<token> pairs that phone without a code:
    /// only someone who can see the screen has it.
    PairLink { token: Option<String> },
    /// Result of a `SaveSettings` or `TriggerUpdate`.
    Reply { id: u64, error: Option<String> },
}

fn write_message<T: Serialize>(stream: &mut UnixStream, message: &T) -> std::io::Result<()> {
    let mut line = serde_json::to_string(message).map_err(std::io::Error::other)?;
    line.push('\n');
    stream.write_all(line.as_bytes())
}

/// Read one line. `Ok(None)` at end of stream; an error for a line longer
/// than `MAX_LINE_BYTES` (the connection is then dropped).
fn read_line(reader: &mut impl BufRead) -> std::io::Result<Option<String>> {
    let mut line = String::new();
    let read = reader.take(MAX_LINE_BYTES + 1).read_line(&mut line)?;
    if read == 0 {
        return Ok(None);
    }
    if !line.ends_with('\n') && read as u64 > MAX_LINE_BYTES {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "message too long"));
    }
    Ok(Some(line))
}

/// Read messages until the stream ends. Lines that don't parse are logged
/// and skipped.
fn read_messages<T: DeserializeOwned>(stream: UnixStream, mut handle: impl FnMut(T)) {
    let mut reader = BufReader::new(stream);
    loop {
        match read_line(&mut reader) {
            Ok(Some(line)) => match serde_json::from_str::<T>(line.trim_end()) {
                Ok(message) => handle(message),
                Err(e) => eprintln!("Clock/web link: skipping a message that didn't parse: {}", e),
            },
            Ok(None) => return,
            Err(e) => {
                eprintln!("Clock/web link: {}", e);
                return;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Clock side
// ---------------------------------------------------------------------------

/// The clock's end: listens for the web server and sends it updates.
pub struct WebLink {
    stream: Mutex<Option<UnixStream>>,
    /// Bumped each time the web server (re)connects.
    connection: AtomicU64,
}

impl WebLink {
    /// Listen on the socket. `handle` runs on the link's thread for every
    /// request; a returned message is sent back as the answer.
    pub fn listen(handle: impl Fn(FromWeb) -> Option<ToWeb> + Send + 'static) -> Arc<Self> {
        Self::listen_at(&socket_path(), handle)
    }

    fn listen_at(path: &Path, handle: impl Fn(FromWeb) -> Option<ToWeb> + Send + 'static) -> Arc<Self> {
        let link = Arc::new(Self { stream: Mutex::new(None), connection: AtomicU64::new(0) });

        let listener = match bind(path) {
            Ok(l) => l,
            Err(e) => {
                eprintln!("Clock/web link: can't listen on {}: {} (no settings page)", path.display(), e);
                return link;
            }
        };

        let accepted = link.clone();
        std::thread::spawn(move || {
            // One web server at a time; a new connection waits for the old one to end.
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let Ok(writer) = stream.try_clone() else { continue };
                *accepted.stream.lock().unwrap() = Some(writer);
                accepted.connection.fetch_add(1, Ordering::Relaxed);
                eprintln!("Web server connected");

                read_messages(stream, |message: FromWeb| {
                    if let Some(answer) = handle(message) {
                        accepted.send(&answer);
                    }
                });

                *accepted.stream.lock().unwrap() = None;
                eprintln!("Web server disconnected");
            }
        });
        link
    }

    /// Send to the web server, if it's connected.
    pub fn send(&self, message: &ToWeb) -> bool {
        let mut stream = self.stream.lock().unwrap();
        let Some(s) = stream.as_mut() else { return false };
        if write_message(s, message).is_err() {
            *stream = None;
            return false;
        }
        true
    }

    /// Changes whenever the web server reconnects (and so needs the current
    /// state sent again).
    pub fn connection(&self) -> u64 {
        self.connection.load(Ordering::Relaxed)
    }
}

/// Create the socket, connectable only by the web server's user (and root).
fn bind(path: &Path) -> std::io::Result<UnixListener> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    // Left over from a previous run
    let _ = std::fs::remove_file(path);
    let listener = UnixListener::bind(path)?;

    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    match user_id(WEB_USER) {
        Some(uid) => std::os::unix::fs::chown(path, Some(uid), None)?,
        // Dev machine: the web server runs as the same user as the clock
        None => eprintln!("Clock/web link: no '{}' user; socket stays with this user", WEB_USER),
    }
    Ok(listener)
}

/// Look a user up in /etc/passwd.
fn user_id(name: &str) -> Option<u32> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    parse_user_id(&passwd, name)
}

fn parse_user_id(passwd: &str, name: &str) -> Option<u32> {
    passwd.lines().find_map(|line| {
        let mut fields = line.split(':');
        (fields.next()? == name).then(|| fields.nth(1)?.parse().ok())?
    })
}

// ---------------------------------------------------------------------------
// Web server side
// ---------------------------------------------------------------------------

/// What the web server can ask of the clock.
pub trait Clock: Send + Sync {
    fn show_code(&self, code: &str) -> Result<(), String>;
    fn hide_code(&self);
    /// Save and apply settings. The clock validates them again.
    fn save_settings(&self, config: &Config) -> Result<(), String>;
    fn trigger_update(&self, install: bool) -> Result<(), String>;
    fn wifi_connect(&self, ssid: &str, password: &str) -> Result<(), String>;
    /// A phone is using the WiFi setup portal.
    fn portal_seen(&self);
    /// A phone paired with the QR code on the clock, which can now come down.
    fn link_used(&self);
}

const NOT_CONNECTED: &str = "The clock isn't answering. Wait a moment and try again.";

/// The web server's end: connects to the clock and keeps reconnecting.
pub struct ClockLink {
    stream: Mutex<Option<UnixStream>>,
    next_id: AtomicU64,
    /// The request waiting for a `Reply`, if any.
    pending: Mutex<Option<(u64, mpsc::Sender<Option<String>>)>>,
    /// One request at a time.
    request_lock: Mutex<()>,
    last_portal_seen: Mutex<Option<Instant>>,
}

impl ClockLink {
    /// Connect in the background. `on_message` gets everything the clock
    /// sends except replies; `on_disconnect` runs when the clock goes away.
    pub fn connect(
        on_message: impl Fn(ToWeb) + Send + 'static,
        on_disconnect: impl Fn() + Send + 'static,
    ) -> Arc<Self> {
        Self::connect_to(socket_path(), on_message, on_disconnect)
    }

    fn connect_to(
        path: PathBuf,
        on_message: impl Fn(ToWeb) + Send + 'static,
        on_disconnect: impl Fn() + Send + 'static,
    ) -> Arc<Self> {
        let link = Arc::new(Self {
            stream: Mutex::new(None),
            next_id: AtomicU64::new(1),
            pending: Mutex::new(None),
            request_lock: Mutex::new(()),
            last_portal_seen: Mutex::new(None),
        });

        let reader = link.clone();
        std::thread::spawn(move || loop {
            let connected = UnixStream::connect(&path).and_then(|s| Ok((s.try_clone()?, s)));
            if let Ok((writer, stream)) = connected {
                *reader.stream.lock().unwrap() = Some(writer);
                eprintln!("Connected to the clock");

                read_messages(stream, |message: ToWeb| match message {
                    ToWeb::Reply { id, error } => {
                        let mut pending = reader.pending.lock().unwrap();
                        if pending.as_ref().is_some_and(|(want, _)| *want == id) {
                            if let Some((_, answer)) = pending.take() {
                                let _ = answer.send(error);
                            }
                        }
                    }
                    other => on_message(other),
                });

                *reader.stream.lock().unwrap() = None;
                // Fail a waiting request now rather than at its timeout
                reader.pending.lock().unwrap().take();
                eprintln!("Lost the clock; reconnecting");
                on_disconnect();
            }
            std::thread::sleep(RECONNECT_EVERY);
        });
        link
    }

    fn send(&self, message: &FromWeb) -> Result<(), String> {
        let mut stream = self.stream.lock().unwrap();
        let Some(s) = stream.as_mut() else {
            return Err(NOT_CONNECTED.into());
        };
        write_message(s, message).map_err(|_| {
            *stream = None;
            NOT_CONNECTED.to_string()
        })
    }

    /// Send a request and wait for the clock's `Reply`.
    fn request(&self, build: impl FnOnce(u64) -> FromWeb) -> Result<(), String> {
        let _one_at_a_time = self.request_lock.lock().unwrap();
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (answer, wait) = mpsc::channel();
        *self.pending.lock().unwrap() = Some((id, answer));

        let result = self.send(&build(id)).and_then(|()| match wait.recv_timeout(REPLY_TIMEOUT) {
            Ok(None) => Ok(()),
            Ok(Some(error)) => Err(error),
            Err(_) => Err(NOT_CONNECTED.to_string()),
        });
        self.pending.lock().unwrap().take();
        result
    }
}

impl Clock for ClockLink {
    fn show_code(&self, code: &str) -> Result<(), String> {
        self.send(&FromWeb::ShowCode { code: code.to_string() })
    }

    fn hide_code(&self) {
        let _ = self.send(&FromWeb::HideCode);
    }

    fn save_settings(&self, config: &Config) -> Result<(), String> {
        self.request(|id| FromWeb::SaveSettings { id, config: config.clone() })
    }

    fn trigger_update(&self, install: bool) -> Result<(), String> {
        self.request(|id| FromWeb::TriggerUpdate { id, install })
    }

    fn wifi_connect(&self, ssid: &str, password: &str) -> Result<(), String> {
        self.send(&FromWeb::WifiConnect { ssid: ssid.to_string(), password: password.to_string() })
    }

    fn portal_seen(&self) {
        let mut last = self.last_portal_seen.lock().unwrap();
        if last.is_some_and(|t| t.elapsed() < PORTAL_SEEN_EVERY) {
            return;
        }
        *last = Some(Instant::now());
        let _ = self.send(&FromWeb::PortalSeen);
    }

    fn link_used(&self) {
        let _ = self.send(&FromWeb::LinkUsed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_socket(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("fc-ipc-{}-{}.sock", name, std::process::id()))
    }

    fn wait_until(what: &str, ready: impl Fn() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready() {
            assert!(Instant::now() < deadline, "timed out waiting for {}", what);
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn messages_use_a_tagged_format() {
        let json = serde_json::to_string(&FromWeb::ShowCode { code: "123456".into() }).unwrap();
        assert_eq!(json, r#"{"type":"show_code","code":"123456"}"#);
        let json = serde_json::to_string(&ToWeb::Reply { id: 7, error: None }).unwrap();
        assert_eq!(json, r#"{"type":"reply","id":7,"error":null}"#);
        assert!(serde_json::from_str::<FromWeb>(r#"{"type":"reboot"}"#).is_err());
    }

    #[test]
    fn overlong_lines_are_refused() {
        let long = "x".repeat(MAX_LINE_BYTES as usize + 10) + "\n";
        assert!(read_line(&mut long.as_bytes()).is_err());
        let fits = "x".repeat(100) + "\nnext\n";
        let mut reader = fits.as_bytes();
        assert_eq!(read_line(&mut reader).unwrap().unwrap().len(), 101);
        assert_eq!(read_line(&mut reader).unwrap().as_deref(), Some("next\n"));
        assert!(read_line(&mut reader).unwrap().is_none());
    }

    #[test]
    fn finds_users_in_passwd() {
        let passwd = "root:x:0:0:root:/root:/bin/bash\nflight-clock-web:x:997:995::/nonexistent:/usr/sbin/nologin\n";
        assert_eq!(parse_user_id(passwd, "flight-clock-web"), Some(997));
        assert_eq!(parse_user_id(passwd, "root"), Some(0));
        assert_eq!(parse_user_id(passwd, "flight-clock"), None);
    }

    #[test]
    fn requests_and_updates_cross_the_socket() {
        let path = test_socket("roundtrip");
        let seen = Arc::new(Mutex::new(Vec::<String>::new()));

        let seen_by_clock = seen.clone();
        let web = WebLink::listen_at(&path, move |message| {
            seen_by_clock.lock().unwrap().push(format!("{:?}", message));
            match message {
                FromWeb::SaveSettings { id, config } => Some(ToWeb::Reply { id, error: config.validate().err() }),
                FromWeb::TriggerUpdate { id, .. } => Some(ToWeb::Reply { id, error: Some("no updater here".into()) }),
                _ => None,
            }
        });

        let from_clock = Arc::new(Mutex::new(Vec::<ToWeb>::new()));
        let inbox = from_clock.clone();
        let clock = ClockLink::connect_to(path.clone(), move |m| inbox.lock().unwrap().push(m), || {});
        wait_until("the web server to connect", || web.connection() == 1);

        // Fire-and-forget
        clock.show_code("123456").unwrap();
        wait_until("the code to arrive", || seen.lock().unwrap().iter().any(|m| m.contains("123456")));

        // Request with an answer, both ways it can go
        assert!(clock.save_settings(&Config::default()).is_ok());
        let bad = Config { radius_nm: 9000.0, ..Config::default() };
        assert!(clock.save_settings(&bad).unwrap_err().contains("Radius"));
        assert_eq!(clock.trigger_update(true).unwrap_err(), "no updater here");

        // Clock -> web
        assert!(web.send(&ToWeb::CancelCode));
        wait_until("the cancel to arrive", || {
            from_clock.lock().unwrap().iter().any(|m| matches!(m, ToWeb::CancelCode))
        });

        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn requests_fail_cleanly_without_a_clock() {
        let clock = ClockLink::connect_to(test_socket("nobody-home"), |_| {}, || {});
        assert_eq!(clock.show_code("123456").unwrap_err(), NOT_CONNECTED);
        assert_eq!(clock.save_settings(&Config::default()).unwrap_err(), NOT_CONNECTED);
    }
}
