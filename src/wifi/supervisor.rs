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
//! - Back online after the hotspot has been up: reclaim the `.local` name.

use crate::wifi::manager::{Network, WifiManager};
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

#[derive(Debug)]
enum Mode {
    Online { offline_since: Option<Instant> },
    Setup { user_initiated: bool, since: Instant },
}

pub struct Supervisor<F: Fn(WifiUiState), N: Network = WifiManager> {
    wifi: N,
    commands: Receiver<WifiCommand>,
    portal: Arc<Mutex<PortalState>>,
    network_ready: Arc<AtomicBool>,
    notify: F,
    /// The setup hotspot has been up since we were last online.
    hotspot_used: bool,
    /// How long each tick waits for a command; shortened in tests.
    online_check_every: Duration,
    setup_check_every: Duration,
}

impl<F: Fn(WifiUiState)> Supervisor<F> {
    pub fn new(
        commands: Receiver<WifiCommand>,
        portal: Arc<Mutex<PortalState>>,
        network_ready: Arc<AtomicBool>,
        notify: F,
    ) -> Self {
        Supervisor::with_network(WifiManager::new(), commands, portal, network_ready, notify)
    }

    /// Runs forever.
    pub fn run(mut self) {
        if !cfg!(target_os = "linux") {
            return self.run_simulated();
        }

        eprintln!("WiFi backend: {:?}", self.wifi.backend());

        let mut mode = self.start();
        loop {
            mode = self.tick(mode);
        }
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

impl<F: Fn(WifiUiState), N: Network> Supervisor<F, N> {
    pub fn with_network(
        wifi: N,
        commands: Receiver<WifiCommand>,
        portal: Arc<Mutex<PortalState>>,
        network_ready: Arc<AtomicBool>,
        notify: F,
    ) -> Self {
        Self {
            wifi,
            commands,
            portal,
            network_ready,
            notify,
            hotspot_used: false,
            online_check_every: ONLINE_CHECK_EVERY,
            setup_check_every: SETUP_CHECK_EVERY,
        }
    }

    /// Boot: decide the first mode.
    fn start(&mut self) -> Mode {
        if self.wifi.is_connected() {
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
        }
    }

    fn tick(&mut self, mode: Mode) -> Mode {
        match mode {
            Mode::Online { offline_since } => self.tick_online(offline_since),
            Mode::Setup { user_initiated, since } => self.tick_setup(user_initiated, since),
        }
    }

    fn tick_online(&mut self, offline_since: Option<Instant>) -> Mode {
        match self.commands.recv_timeout(self.online_check_every) {
            Ok(WifiCommand::EnterSetup) => return self.enter_setup(true, "", false),
            Ok(_) => {} // stale portal/cancel commands
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => std::thread::sleep(self.online_check_every),
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

        match self.commands.recv_timeout(self.setup_check_every) {
            Ok(WifiCommand::Connect { ssid, password }) => self.try_connect(&ssid, &password, user_initiated),
            Ok(WifiCommand::CancelSetup) if self.wifi.has_saved_network() => self.rejoin_saved(),
            Ok(_) => same,
            Err(RecvTimeoutError::Disconnected) => {
                std::thread::sleep(self.setup_check_every);
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
            self.wifi.reclaim_hostname();
        }
        self.network_ready.store(true, Ordering::Relaxed);
        let ssid = self.wifi.current_ssid().unwrap_or_default();
        eprintln!("Online via {:?}", ssid);
        (self.notify)(WifiUiState::Online { ssid });
        Mode::Online { offline_since: None }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::wifi::manager::ScannedNetwork;
    use std::cell::RefCell;
    use std::rc::Rc;
    use std::sync::mpsc::{channel, Sender};

    /// Scripted WiFi: tests set how each operation behaves and read back
    /// what the supervisor did.
    #[derive(Default)]
    struct FakeState {
        saved: bool,
        connected: bool,
        ap_fails: bool,
        connect_fails: bool,
        reconnect_fails: bool,
        ap_up: bool,
        ap_starts: u32,
        reclaims: u32,
    }

    #[derive(Clone, Default)]
    struct FakeNetwork(Rc<RefCell<FakeState>>);

    impl Network for FakeNetwork {
        fn has_saved_network(&self) -> bool {
            self.0.borrow().saved
        }
        fn is_connected(&self) -> bool {
            self.0.borrow().connected
        }
        fn current_ssid(&self) -> Option<String> {
            self.0.borrow().connected.then(|| "Home".to_string())
        }
        fn scan_networks(&self) -> Vec<ScannedNetwork> {
            vec![ScannedNetwork { ssid: "Home".into(), signal: 80, encrypted: true }]
        }
        fn start_ap(&mut self) -> Result<(), String> {
            let mut s = self.0.borrow_mut();
            if s.ap_fails {
                return Err("no AP mode".into());
            }
            s.ap_up = true;
            s.connected = false;
            s.ap_starts += 1;
            Ok(())
        }
        fn stop_ap(&mut self) {
            self.0.borrow_mut().ap_up = false;
        }
        fn connect(&mut self, _ssid: &str, _password: &str) -> Result<(), String> {
            let mut s = self.0.borrow_mut();
            s.ap_up = false;
            if s.connect_fails {
                return Err("wrong password".into());
            }
            s.connected = true;
            s.saved = true;
            Ok(())
        }
        fn reconnect_saved(&mut self, _timeout: Duration) -> bool {
            let mut s = self.0.borrow_mut();
            s.ap_up = false;
            s.connected = !s.reconnect_fails && s.saved;
            s.connected
        }
        fn wait_connected(&self, _timeout: Duration) -> bool {
            self.0.borrow().connected
        }
        fn reclaim_hostname(&mut self) {
            self.0.borrow_mut().reclaims += 1;
        }
    }

    struct Harness {
        sup: Supervisor<Box<dyn Fn(WifiUiState)>, FakeNetwork>,
        net: Rc<RefCell<FakeState>>,
        tx: Sender<WifiCommand>,
        portal: Arc<Mutex<PortalState>>,
        ready: Arc<AtomicBool>,
        ui: Rc<RefCell<Vec<WifiUiState>>>,
    }

    impl Harness {
        fn new(state: FakeState) -> Self {
            let net = FakeNetwork(Rc::new(RefCell::new(state)));
            let (tx, rx) = channel();
            let portal = Arc::new(Mutex::new(PortalState::default()));
            let ready = Arc::new(AtomicBool::new(false));
            let ui = Rc::new(RefCell::new(Vec::new()));
            let ui_log = ui.clone();
            let notify: Box<dyn Fn(WifiUiState)> = Box::new(move |s| ui_log.borrow_mut().push(s));
            let mut sup = Supervisor::with_network(net.clone(), rx, portal.clone(), ready.clone(), notify);
            sup.online_check_every = Duration::from_millis(1);
            sup.setup_check_every = Duration::from_millis(1);
            Harness { sup, net: net.0, tx, portal, ready, ui }
        }

        fn send(&self, cmd: WifiCommand) {
            self.tx.send(cmd).unwrap();
        }

        fn reclaims(&self) -> u32 {
            self.net.borrow().reclaims
        }

        fn last_ui(&self) -> WifiUiState {
            self.ui.borrow().last().cloned().expect("no UI state sent")
        }
    }

    fn ago(d: Duration) -> Instant {
        Instant::now().checked_sub(d).expect("system uptime shorter than test offset")
    }

    fn connect_cmd() -> WifiCommand {
        WifiCommand::Connect { ssid: "Home".into(), password: "password1".into() }
    }

    fn is_online(m: &Mode) -> bool {
        matches!(m, Mode::Online { .. })
    }

    fn is_setup(m: &Mode) -> bool {
        matches!(m, Mode::Setup { .. })
    }

    // --- Boot ---

    #[test]
    fn boot_already_connected_goes_online_without_hotspot() {
        let mut h = Harness::new(FakeState { saved: true, connected: true, ..Default::default() });
        let m = h.sup.start();
        assert!(is_online(&m));
        assert!(h.ready.load(Ordering::Relaxed));
        assert_eq!(h.net.borrow().ap_starts, 0);
        assert_eq!(h.reclaims(), 0);
    }

    #[test]
    fn boot_without_saved_network_opens_setup() {
        let mut h = Harness::new(FakeState::default());
        let m = h.sup.start();
        assert!(is_setup(&m));
        assert!(h.net.borrow().ap_up);
        assert!(h.portal.lock().unwrap().active);
        assert!(!h.ready.load(Ordering::Relaxed));
        assert!(matches!(h.last_ui(), WifiUiState::Setup { can_cancel: false, is_error: false, .. }));
    }

    #[test]
    fn boot_with_unreachable_saved_network_opens_setup_with_error() {
        let mut h = Harness::new(FakeState { saved: true, ..Default::default() });
        let m = h.sup.start();
        assert!(is_setup(&m));
        assert!(matches!(h.last_ui(), WifiUiState::Setup { can_cancel: true, is_error: true, .. }));
    }

    // --- Hostname reclaim after the hotspot ---

    #[test]
    fn joining_from_setup_reclaims_hostname_once() {
        let mut h = Harness::new(FakeState::default());
        let m = h.sup.start();
        h.send(connect_cmd());
        let m = h.sup.tick(m);
        assert!(is_online(&m));
        assert!(!h.portal.lock().unwrap().active);
        assert_eq!(h.reclaims(), 1);
    }

    #[test]
    fn cancelling_setup_reclaims_hostname() {
        // The 16:24 case: WiFi opened from the clock, then cancelled.
        let mut h = Harness::new(FakeState { saved: true, connected: true, ..Default::default() });
        let m = h.sup.start();
        h.send(WifiCommand::EnterSetup);
        let m = h.sup.tick(m);
        assert!(is_setup(&m));
        h.send(WifiCommand::CancelSetup);
        let m = h.sup.tick(m);
        assert!(is_online(&m));
        assert_eq!(h.reclaims(), 1);
    }

    #[test]
    fn no_reclaim_when_hotspot_never_started() {
        let mut h = Harness::new(FakeState { ap_fails: true, ..Default::default() });
        let m = h.sup.start();
        assert!(matches!(h.last_ui(), WifiUiState::Setup { is_error: true, .. }));
        h.send(connect_cmd());
        let m = h.sup.tick(m);
        assert!(is_online(&m));
        assert_eq!(h.reclaims(), 0);
    }

    #[test]
    fn later_reconnects_do_not_reclaim_again() {
        let mut h = Harness::new(FakeState::default());
        let m = h.sup.start();
        h.send(connect_cmd());
        let mut m = h.sup.tick(m);
        assert_eq!(h.reclaims(), 1);

        // A short WiFi drop and recovery, with no hotspot involved.
        h.net.borrow_mut().connected = false;
        m = h.sup.tick(m);
        assert!(matches!(m, Mode::Online { offline_since: Some(_) }));
        h.net.borrow_mut().connected = true;
        m = h.sup.tick(m);
        assert!(matches!(m, Mode::Online { offline_since: None }));
        assert_eq!(h.reclaims(), 1);
    }

    #[test]
    fn each_hotspot_session_reclaims() {
        let mut h = Harness::new(FakeState { saved: true, connected: true, ..Default::default() });
        let mut m = h.sup.start();
        for expected in 1..=2 {
            h.send(WifiCommand::EnterSetup);
            m = h.sup.tick(m);
            h.send(WifiCommand::CancelSetup);
            m = h.sup.tick(m);
            assert!(is_online(&m));
            assert_eq!(h.reclaims(), expected);
        }
    }

    #[test]
    fn failed_join_stays_in_setup_then_reclaims_once_joined() {
        let mut h = Harness::new(FakeState { connect_fails: true, ..Default::default() });
        let m = h.sup.start();
        h.send(connect_cmd());
        let m = h.sup.tick(m);
        assert!(is_setup(&m));
        assert!(h.portal.lock().unwrap().message.as_deref().unwrap().contains("wrong password"));
        assert_eq!(h.reclaims(), 0);

        h.net.borrow_mut().connect_fails = false;
        h.send(connect_cmd());
        let m = h.sup.tick(m);
        assert!(is_online(&m));
        assert_eq!(h.reclaims(), 1);
    }

    // --- Setup idle retries ---

    #[test]
    fn idle_setup_retries_saved_network_after_five_minutes() {
        let mut h = Harness::new(FakeState { saved: true, ..Default::default() });
        h.sup.start();
        let m = h.sup.tick(Mode::Setup { user_initiated: false, since: ago(Duration::from_secs(301)) });
        assert!(is_online(&m));
        assert_eq!(h.reclaims(), 1);
    }

    #[test]
    fn idle_setup_waits_before_five_minutes() {
        let mut h = Harness::new(FakeState { saved: true, ..Default::default() });
        h.sup.start();
        let m = h.sup.tick(Mode::Setup { user_initiated: false, since: ago(Duration::from_secs(200)) });
        assert!(is_setup(&m));
    }

    #[test]
    fn user_opened_setup_gets_ten_minutes() {
        let mut h = Harness::new(FakeState { saved: true, ..Default::default() });
        h.sup.start();
        let m = h.sup.tick(Mode::Setup { user_initiated: true, since: ago(Duration::from_secs(400)) });
        assert!(is_setup(&m));
        let m = h.sup.tick(Mode::Setup { user_initiated: true, since: ago(Duration::from_secs(601)) });
        assert!(is_online(&m));
    }

    #[test]
    fn setup_stays_up_while_portal_in_use() {
        let mut h = Harness::new(FakeState { saved: true, ..Default::default() });
        h.sup.start();
        h.portal.lock().unwrap().last_request = Some(ago(Duration::from_secs(30)));
        let m = h.sup.tick(Mode::Setup { user_initiated: false, since: ago(Duration::from_secs(900)) });
        assert!(is_setup(&m));
    }

    #[test]
    fn setup_without_saved_network_never_retries() {
        let mut h = Harness::new(FakeState::default());
        h.sup.start();
        let m = h.sup.tick(Mode::Setup { user_initiated: false, since: ago(Duration::from_secs(3600)) });
        assert!(is_setup(&m));
    }

    #[test]
    fn failed_retry_reopens_setup_with_error() {
        let mut h = Harness::new(FakeState { saved: true, reconnect_fails: true, ..Default::default() });
        h.sup.start();
        let m = h.sup.tick(Mode::Setup { user_initiated: false, since: ago(Duration::from_secs(301)) });
        assert!(is_setup(&m));
        assert!(matches!(h.last_ui(), WifiUiState::Setup { is_error: true, .. }));
        assert_eq!(h.reclaims(), 0);
    }

    // --- Losing the network while online ---

    #[test]
    fn brief_outage_stays_online() {
        let mut h = Harness::new(FakeState { saved: true, connected: true, ..Default::default() });
        h.sup.start();
        h.net.borrow_mut().connected = false;
        let m = h.sup.tick(Mode::Online { offline_since: Some(ago(Duration::from_secs(60))) });
        assert!(matches!(m, Mode::Online { offline_since: Some(_) }));
    }

    #[test]
    fn five_minutes_offline_opens_setup() {
        let mut h = Harness::new(FakeState { saved: true, connected: true, ..Default::default() });
        h.sup.start();
        h.net.borrow_mut().connected = false;
        let m = h.sup.tick(Mode::Online { offline_since: Some(ago(Duration::from_secs(301))) });
        assert!(is_setup(&m));
        assert!(!h.ready.load(Ordering::Relaxed));
        assert_eq!(h.portal.lock().unwrap().message.as_deref(), Some("Lost the WiFi connection."));
    }

    #[test]
    fn stale_setup_commands_ignored_while_online() {
        let mut h = Harness::new(FakeState { saved: true, connected: true, ..Default::default() });
        let m = h.sup.start();
        h.send(WifiCommand::CancelSetup);
        let m = h.sup.tick(m);
        h.send(connect_cmd());
        let m = h.sup.tick(m);
        assert!(is_online(&m));
        assert_eq!(h.net.borrow().ap_starts, 0);
    }
}
