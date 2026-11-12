//! WiFi state machine: decides when the clock is online and when it hosts
//! the setup hotspot.
//!
//! - Boot: if the OS has a saved network, wait for it. A device that was
//!   already set up stays on its network and never shows setup.
//! - No saved network, or it can't be reached: start the setup hotspot.
//! - The user can open setup any time from the settings screen, and cancel
//!   back to the saved network.
//! - Setup open with a saved network and nobody using the portal: retry the
//!   saved network (after 5 min, or 10 if the user opened setup), so a router
//!   reboot or a walk-away doesn't leave the clock stuck in setup.
//! - Online but the network disappears for several minutes: start setup.

use crate::wifi::manager::WifiManager;
use crate::wifi::portal::PortalState;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const BOOT_CONNECT_WAIT: Duration = Duration::from_secs(45);
const RECONNECT_WAIT: Duration = Duration::from_secs(30);
const OFFLINE_BEFORE_SETUP: Duration = Duration::from_secs(5 * 60);
const SETUP_RETRY_SAVED_EVERY: Duration = Duration::from_secs(5 * 60);
const ONLINE_CHECK_EVERY: Duration = Duration::from_secs(10);
const SETUP_CHECK_EVERY: Duration = Duration::from_secs(5);

pub enum WifiCommand {
    /// User asked to change WiFi from the settings screen.
    EnterSetup,
    /// User backed out of setup; go back to the saved network.
    CancelSetup,
    /// Credentials submitted through the portal.
    Connect { ssid: String, password: String },
}

/// What the screen should show.
#[derive(Debug, Clone)]
pub enum WifiUiState {
    /// Working on it (boot, or rejoining after cancel).
    Connecting(String),
    Online { ssid: String },
    Setup {
        status: String,
        is_error: bool,
        can_cancel: bool,
    },
}

enum Mode {
    Online { offline_since: Option<Instant> },
    Setup { user_initiated: bool, since: Instant },
}

pub struct Supervisor<F: Fn(WifiUiState)> {
    wifi: WifiManager,
    commands: Receiver<WifiCommand>,
    portal: Arc<Mutex<PortalState>>,
    network_ready: Arc<AtomicBool>,
    notify: F,
    /// The setup hotspot has been up since we were last online.
    hotspot_used: bool,
}

impl<F: Fn(WifiUiState)> Supervisor<F> {
    pub fn new(
        commands: Receiver<WifiCommand>,
        portal: Arc<Mutex<PortalState>>,
        network_ready: Arc<AtomicBool>,
        notify: F,
    ) -> Self {
        Self {
            wifi: WifiManager::new(),
            commands,
            portal,
            network_ready,
            notify,
            hotspot_used: false,
        }
    }

    /// Runs forever.
    pub fn run(mut self) {
        if !cfg!(target_os = "linux") {
            return self.run_simulated();
        }

        eprintln!("WiFi backend: {:?}", self.wifi.backend());

        let mut mode = if self.wifi.is_connected() {
            self.go_online()
        } else if self.wifi.has_saved_network() {
            (self.notify)(WifiUiState::Connecting("Connecting to WiFi...".into()));
            if self.wifi.wait_connected(BOOT_CONNECT_WAIT) {
                self.go_online()
            } else {
                self.enter_setup(false, "Couldn't connect to your saved WiFi.", true)
            }
        } else {
            self.enter_setup(false, "", false)
        };

        loop {
            mode = match mode {
                Mode::Online { offline_since } => self.tick_online(offline_since),
                Mode::Setup { user_initiated, since } => self.tick_setup(user_initiated, since),
            };
        }
    }

    fn tick_online(&mut self, offline_since: Option<Instant>) -> Mode {
        match self.commands.recv_timeout(ONLINE_CHECK_EVERY) {
            Ok(WifiCommand::EnterSetup) => return self.enter_setup(true, "", false),
            Ok(_) => {} // stale portal/cancel commands
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => std::thread::sleep(ONLINE_CHECK_EVERY),
        }

        if self.wifi.is_connected() {
            return Mode::Online { offline_since: None };
        }

        let since = offline_since.unwrap_or_else(Instant::now);
        if since.elapsed() >= OFFLINE_BEFORE_SETUP {
            self.enter_setup(false, "Lost the WiFi connection.", true)
        } else {
            Mode::Online { offline_since: Some(since) }
        }
    }

    fn tick_setup(&mut self, user_initiated: bool, since: Instant) -> Mode {
        let same = Mode::Setup { user_initiated, since };

        match self.commands.recv_timeout(SETUP_CHECK_EVERY) {
            Ok(WifiCommand::Connect { ssid, password }) => self.try_connect(&ssid, &password, user_initiated),
            Ok(WifiCommand::CancelSetup) if self.wifi.has_saved_network() => self.rejoin_saved(),
            Ok(_) => same,
            Err(RecvTimeoutError::Disconnected) => {
                std::thread::sleep(SETUP_CHECK_EVERY);
                same
            }
            Err(RecvTimeoutError::Timeout) => {
                // Give someone who opened setup on purpose longer before
                // giving up on them and going back to the saved network.
                let idle_limit = if user_initiated {
                    SETUP_RETRY_SAVED_EVERY * 2
                } else {
                    SETUP_RETRY_SAVED_EVERY
                };
                let portal_idle = self
                    .portal
                    .lock()
                    .unwrap()
                    .last_request
                    .map_or(true, |t| t.elapsed() >= idle_limit);

                if portal_idle
                    && since.elapsed() >= idle_limit
                    && self.wifi.has_saved_network()
                {
                    eprintln!("Setup idle; retrying saved WiFi");
                    self.rejoin_saved()
                } else {
                    same
                }
            }
        }
    }

    fn try_connect(&mut self, ssid: &str, password: &str, user_initiated: bool) -> Mode {
        (self.notify)(WifiUiState::Setup {
            status: format!("Connecting to {}...", ssid),
            is_error: false,
            can_cancel: false,
        });

        match self.wifi.connect(ssid, password) {
            Ok(()) => self.go_online(),
            Err(e) => {
                eprintln!("Joining {:?} failed: {}", ssid, e);
                let msg = format!("Couldn't connect to {}: {}.", ssid, e);
                self.enter_setup(user_initiated, &msg, true)
            }
        }
    }

    fn rejoin_saved(&mut self) -> Mode {
        self.portal.lock().unwrap().active = false;
        (self.notify)(WifiUiState::Connecting("Reconnecting to saved WiFi...".into()));

        if self.wifi.reconnect_saved(RECONNECT_WAIT) {
            self.go_online()
        } else {
            self.enter_setup(false, "Couldn't reconnect to your saved WiFi.", true)
        }
    }

    fn enter_setup(&mut self, user_initiated: bool, message: &str, is_error: bool) -> Mode {
        self.network_ready.store(false, Ordering::Relaxed);
        let can_cancel = self.wifi.has_saved_network();

        (self.notify)(WifiUiState::Setup {
            status: "Starting setup network...".into(),
            is_error: false,
            can_cancel: false,
        });

        // Scan before the hotspot comes up; the radio can't scan in AP mode.
        self.wifi.stop_ap();
        let networks = self.wifi.scan_networks();
        {
            let mut portal = self.portal.lock().unwrap();
            portal.networks = networks;
            portal.message = (!message.is_empty()).then(|| message.to_string());
            portal.last_request = None;
            portal.active = true;
        }

        let started = self.wifi.start_ap();
        self.hotspot_used |= started.is_ok();
        let (status, is_error) = match started {
            Ok(()) if message.is_empty() => ("Waiting for setup...".to_string(), false),
            Ok(()) => (message.to_string(), is_error),
            Err(e) => {
                eprintln!("Setup hotspot failed: {}", e);
                (format!("Couldn't start the setup network: {}", e), true)
            }
        };
        (self.notify)(WifiUiState::Setup { status, is_error, can_cancel });

        Mode::Setup { user_initiated, since: Instant::now() }
    }

    fn go_online(&mut self) -> Mode {
        self.wifi.stop_ap();
        {
            let mut portal = self.portal.lock().unwrap();
            portal.active = false;
            portal.message = None;
        }
        if std::mem::take(&mut self.hotspot_used) {
            reclaim_hostname();
        }
        self.network_ready.store(true, Ordering::Relaxed);
        let ssid = self.wifi.current_ssid().unwrap_or_default();
        eprintln!("Online via {:?}", ssid);
        (self.notify)(WifiUiState::Online { ssid });
        Mode::Online { offline_since: None }
    }

    /// Non-Linux dev builds: no WiFi control, but the setup screen can still
    /// be opened and cancelled to check the UI.
    fn run_simulated(self) {
        self.network_ready.store(true, Ordering::Relaxed);
        (self.notify)(WifiUiState::Online { ssid: "Dev machine".into() });

        while let Ok(cmd) = self.commands.recv() {
            match cmd {
                WifiCommand::EnterSetup => {
                    self.network_ready.store(false, Ordering::Relaxed);
                    (self.notify)(WifiUiState::Setup {
                        status: "Simulated: WiFi control only works on the Pi.".into(),
                        is_error: false,
                        can_cancel: true,
                    });
                }
                WifiCommand::CancelSetup | WifiCommand::Connect { .. } => {
                    self.network_ready.store(true, Ordering::Relaxed);
                    (self.notify)(WifiUiState::Online { ssid: "Dev machine".into() });
                }
            }
        }
    }
}

/// Restart avahi after a hotspot session. When wlan0 drops and comes back,
/// avahi can mistake its own stale announcement for another device, rename
/// the clock to `<host>-2.local` and stop answering on the usual name.
/// A restart makes it announce the real hostname again. `try-restart` does
/// nothing if avahi isn't running.
fn reclaim_hostname() {
    match std::process::Command::new("systemctl")
        .args(["try-restart", "avahi-daemon.service"])
        .status()
    {
        Ok(s) if s.success() => eprintln!("Restarted avahi to reclaim the .local name"),
        Ok(s) => eprintln!("avahi restart exited with {}", s),
        Err(e) => eprintln!("avahi restart failed: {}", e),
    }
}
