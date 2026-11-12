//! Low-level WiFi control.
//!
//! Two backends are supported, picked at runtime:
//! - **NetworkManager** (Raspberry Pi OS Bookworm and later): everything goes
//!   through `nmcli`. The setup hotspot is an NM "shared" connection, and NM's
//!   own dnsmasq hands out addresses and answers DNS for the captive portal.
//! - **wpa_supplicant + dhcpcd** (older Raspberry Pi OS): hostapd and dnsmasq
//!   are started by hand for the hotspot, and credentials are written to
//!   wpa_supplicant.conf.
//!
//! Saved networks live in the OS's own network config, so a device that was
//! already online before this app was installed is recognised as configured.

use std::process::{Command, Output};
use std::time::{Duration, Instant};

pub const AP_SSID: &str = "FlightClock-Setup";
pub const AP_IP: &str = "192.168.4.1";

const IFACE: &str = "wlan0";

// NetworkManager backend
const NM_AP_CONNECTION: &str = "flightclock-setup";
const NM_CAPTIVE_DNS_CONF: &str = "/etc/NetworkManager/dnsmasq-shared.d/flightclock-captive.conf";
const NM_WIFI_TYPE: &str = "802-11-wireless";

// wpa_supplicant backend
const WPA_CONF_PATH: &str = "/etc/wpa_supplicant/wpa_supplicant.conf";
const LEGACY_HOSTAPD_CONF: &str = "/tmp/flightclock-hostapd.conf";
const LEGACY_DNSMASQ_CONF: &str = "/tmp/flightclock-dnsmasq.conf";
const LEGACY_DNSMASQ_PID: &str = "/run/flightclock-dnsmasq.pid";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    NetworkManager,
    WpaSupplicant,
}

#[derive(Debug, Clone)]
pub struct ScannedNetwork {
    pub ssid: String,
    /// Signal quality, 0-100.
    pub signal: u8,
    pub encrypted: bool,
}

pub struct WifiManager {
    backend: Backend,
    ap_active: bool,
}

impl WifiManager {
    pub fn new() -> Self {
        let nm_running = Command::new("systemctl")
            .args(["is-active", "--quiet", "NetworkManager"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        let nmcli_present = Command::new("nmcli").arg("--version").output().is_ok();

        let backend = if nm_running && nmcli_present {
            Backend::NetworkManager
        } else {
            Backend::WpaSupplicant
        };

        Self {
            backend,
            ap_active: false,
        }
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    pub fn ap_active(&self) -> bool {
        self.ap_active
    }

    /// Whether the OS has at least one saved WiFi network to join.
    pub fn has_saved_network(&self) -> bool {
        match self.backend {
            Backend::NetworkManager => !nm_client_profiles().is_empty(),
            Backend::WpaSupplicant => std::fs::read_to_string(WPA_CONF_PATH)
                .map(|c| c.contains("network={"))
                .unwrap_or(false),
        }
    }

    /// Whether we're joined to a WiFi network as a client (not hosting the setup hotspot).
    pub fn is_connected(&self) -> bool {
        if self.ap_active {
            return false;
        }
        match self.backend {
            Backend::NetworkManager => nm_active_client_uuid().is_some(),
            Backend::WpaSupplicant => legacy_current_ssid().is_some(),
        }
    }

    /// SSID of the network we're currently joined to, if any.
    pub fn current_ssid(&self) -> Option<String> {
        if self.ap_active {
            return None;
        }
        match self.backend {
            Backend::NetworkManager => nm_active_client_uuid().and_then(|uuid| nm_profile_ssid(&uuid)),
            Backend::WpaSupplicant => legacy_current_ssid(),
        }
    }

    /// Scan for nearby networks, strongest first, one entry per SSID.
    /// Must be called while the hotspot is down: the radio can't scan in AP mode.
    pub fn scan_networks(&self) -> Vec<ScannedNetwork> {
        let mut networks = match self.backend {
            Backend::NetworkManager => nm_scan(),
            Backend::WpaSupplicant => legacy_scan(),
        };

        networks.retain(|n| !n.ssid.is_empty() && n.ssid != AP_SSID);
        networks.sort_by(|a, b| b.signal.cmp(&a.signal));
        let mut seen = std::collections::HashSet::new();
        networks.retain(|n| seen.insert(n.ssid.clone()));
        networks
    }

    /// Bring up the open "FlightClock-Setup" hotspot at AP_IP, with DNS pointing
    /// every name at us so phones pop up the captive portal.
    pub fn start_ap(&mut self) -> Result<(), String> {
        eprintln!("Starting setup hotspot {} ({:?})", AP_SSID, self.backend);
        let result = match self.backend {
            Backend::NetworkManager => nm_start_ap(),
            Backend::WpaSupplicant => legacy_start_ap(),
        };
        // Even on partial failure some pieces may be up; stop_ap cleans them.
        self.ap_active = true;
        result
    }

    pub fn stop_ap(&mut self) {
        if !self.ap_active {
            return;
        }
        eprintln!("Stopping setup hotspot");
        match self.backend {
            Backend::NetworkManager => {
                let _ = run("nmcli", &["connection", "delete", "id", NM_AP_CONNECTION]);
            }
            Backend::WpaSupplicant => legacy_stop_ap(),
        }
        self.ap_active = false;
    }

    /// Join `ssid`. Blocks until connected or failed. On failure the previous
    /// saved networks are left as they were, and the error says why.
    pub fn connect(&mut self, ssid: &str, password: &str) -> Result<(), String> {
        if !password.is_empty() && !valid_wpa_password(password) {
            return Err("the password must be 8-63 characters".to_string());
        }

        self.stop_ap();
        eprintln!("Joining WiFi network {:?}", ssid);

        match self.backend {
            Backend::NetworkManager => nm_connect(ssid, password),
            Backend::WpaSupplicant => legacy_connect(ssid, password, self),
        }
    }

    /// Try to rejoin a saved network. Returns true once connected.
    pub fn reconnect_saved(&mut self, timeout: Duration) -> bool {
        self.stop_ap();
        match self.backend {
            Backend::NetworkManager => {
                let secs = timeout.as_secs().max(1).to_string();
                let _ = run("nmcli", &["--wait", &secs, "device", "connect", IFACE]);
            }
            Backend::WpaSupplicant => legacy_restart_client(),
        }
        self.wait_connected(timeout)
    }

    pub fn wait_connected(&self, timeout: Duration) -> bool {
        let start = Instant::now();
        loop {
            if self.is_connected() {
                return true;
            }
            if start.elapsed() >= timeout {
                return false;
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

fn valid_wpa_password(password: &str) -> bool {
    let len = password.len();
    (8..=63).contains(&len) || (len == 64 && password.chars().all(|c| c.is_ascii_hexdigit()))
}

// ---------------------------------------------------------------------------
// Process helpers
// ---------------------------------------------------------------------------

fn run(cmd: &str, args: &[&str]) -> Result<Output, String> {
    Command::new(cmd)
        .args(args)
        .output()
        .map_err(|e| format!("{}: {}", cmd, e))
}

/// Run a command and return stdout, or the first line of its error output.
fn run_ok(cmd: &str, args: &[&str]) -> Result<String, String> {
    let out = run(cmd, args)?;
    let stdout = String::from_utf8_lossy(&out.stdout).to_string();
    if out.status.success() {
        return Ok(stdout);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    let msg = stderr
        .lines()
        .chain(stdout.lines())
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("command failed")
        .to_string();
    Err(msg)
}

fn stdout_of(cmd: &str, args: &[&str]) -> String {
    run(cmd, args)
        .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// NetworkManager backend
// ---------------------------------------------------------------------------

/// Split one line of `nmcli -t` output. Fields are `:`-separated, with literal
/// `:` and `\` escaped as `\:` and `\\`.
fn split_terse(line: &str) -> Vec<String> {
    let mut fields = vec![String::new()];
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                if let Some(next) = chars.next() {
                    fields.last_mut().unwrap().push(next);
                }
            }
            ':' => fields.push(String::new()),
            _ => fields.last_mut().unwrap().push(c),
        }
    }
    fields
}

/// UUIDs of saved WiFi client profiles (excluding our setup hotspot).
fn nm_client_profiles() -> Vec<String> {
    stdout_of("nmcli", &["-t", "-f", "UUID,TYPE,NAME", "connection", "show"])
        .lines()
        .filter_map(|line| {
            let f = split_terse(line);
            (f.len() >= 3 && f[1] == NM_WIFI_TYPE && f[2] != NM_AP_CONNECTION).then(|| f[0].clone())
        })
        .collect()
}

fn nm_profile_ssid(uuid: &str) -> Option<String> {
    let ssid = stdout_of("nmcli", &["-g", "802-11-wireless.ssid", "connection", "show", "uuid", uuid]);
    let ssid = ssid.trim_end_matches('\n').to_string();
    (!ssid.is_empty()).then_some(ssid)
}

/// UUID of the client connection active on wlan0, if it's fully connected.
fn nm_active_client_uuid() -> Option<String> {
    let out = stdout_of(
        "nmcli",
        &["-g", "GENERAL.STATE,GENERAL.CONNECTION,GENERAL.CON-UUID", "device", "show", IFACE],
    );
    let mut lines = out.lines();
    let state = lines.next()?.trim();
    let name = lines.next()?.trim();
    let uuid = lines.next()?.trim();

    let connected = state.starts_with("100");
    (connected && name != NM_AP_CONNECTION && !uuid.is_empty()).then(|| uuid.to_string())
}

fn nm_scan() -> Vec<ScannedNetwork> {
    stdout_of(
        "nmcli",
        &["-t", "-f", "SSID,SIGNAL,SECURITY", "device", "wifi", "list", "ifname", IFACE, "--rescan", "yes"],
    )
    .lines()
    .filter_map(|line| {
        let f = split_terse(line);
        if f.len() < 3 {
            return None;
        }
        let security = f[2].trim();
        Some(ScannedNetwork {
            ssid: f[0].clone(),
            signal: f[1].trim().parse().unwrap_or(0),
            encrypted: !security.is_empty() && security != "--",
        })
    })
    .collect()
}

fn nm_start_ap() -> Result<(), String> {
    // NM's dnsmasq for "shared" connections reads extra config from this dir.
    // Answering every DNS lookup with our IP makes phones show the portal.
    if let Some(dir) = std::path::Path::new(NM_CAPTIVE_DNS_CONF).parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("creating {}: {}", dir.display(), e))?;
    }
    std::fs::write(
        NM_CAPTIVE_DNS_CONF,
        format!(
            "# Written by flight-clock: point all DNS from setup-hotspot clients at the setup page\naddress=/#/{}\n",
            AP_IP
        ),
    )
    .map_err(|e| format!("writing {}: {}", NM_CAPTIVE_DNS_CONF, e))?;

    let _ = run("nmcli", &["connection", "delete", "id", NM_AP_CONNECTION]);

    let ap_addr = format!("{}/24", AP_IP);
    run_ok(
        "nmcli",
        &[
            "connection", "add",
            "type", "wifi",
            "ifname", IFACE,
            "con-name", NM_AP_CONNECTION,
            "autoconnect", "no",
            "ssid", AP_SSID,
            "802-11-wireless.mode", "ap",
            "802-11-wireless.band", "bg",
            "ipv4.method", "shared",
            "ipv4.addresses", &ap_addr,
            "ipv6.method", "disabled",
        ],
    )?;
    run_ok("nmcli", &["--wait", "30", "connection", "up", "id", NM_AP_CONNECTION])?;
    Ok(())
}

fn nm_profiles_for_ssid(ssid: &str) -> Vec<String> {
    nm_client_profiles()
        .into_iter()
        .filter(|uuid| nm_profile_ssid(uuid).as_deref() == Some(ssid))
        .collect()
}

fn nm_connect(ssid: &str, password: &str) -> Result<(), String> {
    let existing = nm_profiles_for_ssid(ssid);

    // The radio was just in AP mode; give it a fresh scan so the SSID is known.
    let _ = run("nmcli", &["device", "wifi", "rescan", "ifname", IFACE]);
    std::thread::sleep(Duration::from_secs(4));

    let mut args = vec!["--wait", "45", "device", "wifi", "connect", ssid];
    if !password.is_empty() {
        args.extend(["password", password]);
    }
    args.extend(["ifname", IFACE]);

    match run_ok("nmcli", &args) {
        Ok(_) => {
            // Drop older profiles for the same SSID (e.g. an old password) so
            // NM doesn't try them on the next boot.
            let active = nm_active_client_uuid();
            for uuid in existing {
                if Some(&uuid) != active.as_ref() {
                    let _ = run("nmcli", &["connection", "delete", "uuid", &uuid]);
                }
            }
            Ok(())
        }
        Err(raw) => {
            eprintln!("nmcli connect failed: {}", raw);
            // Remove any half-made profile this attempt created, keep the old ones.
            for uuid in nm_profiles_for_ssid(ssid) {
                if !existing.contains(&uuid) {
                    let _ = run("nmcli", &["connection", "delete", "uuid", &uuid]);
                }
            }
            Err(friendly_nm_error(&raw))
        }
    }
}

fn friendly_nm_error(raw: &str) -> String {
    let lower = raw.to_lowercase();
    if lower.contains("secrets were required") || lower.contains("no secrets") || lower.contains("802-1x") {
        "wrong password".to_string()
    } else if lower.contains("psk") && lower.contains("invalid") {
        "the password must be 8-63 characters".to_string()
    } else if lower.contains("no network with ssid") {
        "network not found - is it in range?".to_string()
    } else if lower.contains("timeout") || lower.contains("timed out") {
        "timed out".to_string()
    } else {
        raw.trim_start_matches("Error: ").to_string()
    }
}

// ---------------------------------------------------------------------------
// wpa_supplicant + dhcpcd backend (older Raspberry Pi OS)
// ---------------------------------------------------------------------------

fn legacy_current_ssid() -> Option<String> {
    let ssid = stdout_of("iwgetid", &["-r"]).trim().to_string();
    (!ssid.is_empty()).then_some(ssid)
}

fn legacy_scan() -> Vec<ScannedNetwork> {
    let raw = stdout_of("iwlist", &[IFACE, "scan"]);
    let mut networks = Vec::new();
    let mut ssid: Option<String> = None;
    let mut signal_dbm = -100;
    let mut encrypted = false;

    let mut flush = |ssid: &mut Option<String>, dbm: i32, enc: bool| {
        if let Some(s) = ssid.take() {
            networks.push(ScannedNetwork {
                ssid: s,
                signal: (2 * (dbm + 100)).clamp(0, 100) as u8,
                encrypted: enc,
            });
        }
    };

    for line in raw.lines().map(str::trim) {
        if line.starts_with("Cell ") {
            flush(&mut ssid, signal_dbm, encrypted);
            signal_dbm = -100;
            encrypted = false;
        }
        if let Some(rest) = line.strip_prefix("ESSID:") {
            ssid = Some(rest.trim_matches('"').to_string());
        }
        if let Some(level) = line.split("Signal level=").nth(1) {
            signal_dbm = level.split(' ').next().and_then(|v| v.parse().ok()).unwrap_or(-100);
        }
        if line.starts_with("Encryption key:on") {
            encrypted = true;
        }
    }
    flush(&mut ssid, signal_dbm, encrypted);
    networks
}

fn legacy_start_ap() -> Result<(), String> {
    let _ = run("systemctl", &["stop", "wpa_supplicant"]);
    let _ = run("killall", &["wpa_supplicant"]);

    std::fs::write(
        LEGACY_HOSTAPD_CONF,
        format!(
            "interface={}\ndriver=nl80211\nssid={}\nhw_mode=g\nchannel=7\nwmm_enabled=0\nmacaddr_acl=0\nauth_algs=1\nignore_broadcast_ssid=0\n",
            IFACE, AP_SSID
        ),
    )
    .map_err(|e| format!("writing hostapd config: {}", e))?;

    std::fs::write(
        LEGACY_DNSMASQ_CONF,
        format!(
            "interface={iface}\nbind-interfaces\nlisten-address={ip}\ndhcp-range=192.168.4.2,192.168.4.20,255.255.255.0,1h\naddress=/#/{ip}\n",
            iface = IFACE,
            ip = AP_IP
        ),
    )
    .map_err(|e| format!("writing dnsmasq config: {}", e))?;

    let addr = format!("{}/24", AP_IP);
    let _ = run("ip", &["addr", "flush", "dev", IFACE]);
    let _ = run("ip", &["addr", "add", &addr, "dev", IFACE]);
    let _ = run("ip", &["link", "set", IFACE, "up"]);

    run_ok("hostapd", &["-B", LEGACY_HOSTAPD_CONF])?;
    let pid_arg = format!("--pid-file={}", LEGACY_DNSMASQ_PID);
    run_ok("dnsmasq", &["-C", LEGACY_DNSMASQ_CONF, &pid_arg])?;
    Ok(())
}

fn legacy_stop_ap() {
    let _ = run("killall", &["hostapd"]);
    if let Ok(pid) = std::fs::read_to_string(LEGACY_DNSMASQ_PID) {
        let _ = run("kill", &[pid.trim()]);
        let _ = std::fs::remove_file(LEGACY_DNSMASQ_PID);
    }
    let _ = run("ip", &["addr", "flush", "dev", IFACE]);
}

fn legacy_restart_client() {
    let _ = run("systemctl", &["restart", "wpa_supplicant"]);
    let _ = run("systemctl", &["restart", "dhcpcd"]);
}

fn hex_encode(s: &str) -> String {
    s.bytes().map(|b| format!("{:02x}", b)).collect()
}

fn legacy_connect(ssid: &str, password: &str, mgr: &WifiManager) -> Result<(), String> {
    let previous = std::fs::read_to_string(WPA_CONF_PATH).ok();
    let country = previous
        .as_deref()
        .and_then(|c| c.lines().find_map(|l| l.trim().strip_prefix("country=").map(str::to_string)))
        .unwrap_or_else(|| "GB".to_string());

    let network_block = if password.is_empty() {
        format!("network={{\n\tssid={}\n\tkey_mgmt=NONE\n}}\n", hex_encode(ssid))
    } else {
        let block = run_ok("wpa_passphrase", &[ssid, password])?;
        // Don't keep the plaintext password comment wpa_passphrase adds.
        block
            .lines()
            .filter(|l| !l.trim_start().starts_with("#psk="))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n"
    };

    let conf = format!(
        "ctrl_interface=DIR=/var/run/wpa_supplicant GROUP=netdev\nupdate_config=1\ncountry={}\n\n{}",
        country, network_block
    );
    std::fs::write(WPA_CONF_PATH, conf).map_err(|e| format!("writing wpa_supplicant.conf: {}", e))?;

    legacy_restart_client();
    if mgr.wait_connected(Duration::from_secs(30)) {
        return Ok(());
    }

    // Put the old config back so a typo doesn't lose the working network.
    if let Some(prev) = previous {
        let _ = std::fs::write(WPA_CONF_PATH, prev);
    }
    Err("couldn't join - check the password".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_terse_handles_escapes() {
        assert_eq!(split_terse("a:b:c"), vec!["a", "b", "c"]);
        assert_eq!(split_terse(r"My\:Net:80:WPA2"), vec!["My:Net", "80", "WPA2"]);
        assert_eq!(split_terse(r"back\\slash::"), vec![r"back\slash", "", ""]);
    }

    #[test]
    fn password_validation() {
        assert!(valid_wpa_password("12345678"));
        assert!(!valid_wpa_password("1234567"));
        assert!(valid_wpa_password(&"a".repeat(63)));
        assert!(!valid_wpa_password(&"g".repeat(64)));
        assert!(valid_wpa_password(&"f".repeat(64)));
    }

    #[test]
    fn friendly_errors() {
        assert_eq!(
            friendly_nm_error("Error: Connection activation failed: Secrets were required, but not provided."),
            "wrong password"
        );
        assert_eq!(
            friendly_nm_error("Error: No network with SSID 'Nope' found."),
            "network not found - is it in range?"
        );
    }
}

/// The WiFi and host operations the supervisor needs. `WifiManager` is the
/// real implementation; tests use a fake.
pub trait Network {
    fn has_saved_network(&self) -> bool;
    fn is_connected(&self) -> bool;
    fn current_ssid(&self) -> Option<String>;
    fn scan_networks(&self) -> Vec<ScannedNetwork>;
    fn start_ap(&mut self) -> Result<(), String>;
    fn stop_ap(&mut self);
    fn connect(&mut self, ssid: &str, password: &str) -> Result<(), String>;
    fn reconnect_saved(&mut self, timeout: Duration) -> bool;
    fn wait_connected(&self, timeout: Duration) -> bool;
    /// Make mDNS announce the real hostname again after the hotspot.
    fn reclaim_hostname(&mut self);
}

impl Network for WifiManager {
    fn has_saved_network(&self) -> bool {
        WifiManager::has_saved_network(self)
    }
    fn is_connected(&self) -> bool {
        WifiManager::is_connected(self)
    }
    fn current_ssid(&self) -> Option<String> {
        WifiManager::current_ssid(self)
    }
    fn scan_networks(&self) -> Vec<ScannedNetwork> {
        WifiManager::scan_networks(self)
    }
    fn start_ap(&mut self) -> Result<(), String> {
        WifiManager::start_ap(self)
    }
    fn stop_ap(&mut self) {
        WifiManager::stop_ap(self)
    }
    fn connect(&mut self, ssid: &str, password: &str) -> Result<(), String> {
        WifiManager::connect(self, ssid, password)
    }
    fn reconnect_saved(&mut self, timeout: Duration) -> bool {
        WifiManager::reconnect_saved(self, timeout)
    }
    fn wait_connected(&self, timeout: Duration) -> bool {
        WifiManager::wait_connected(self, timeout)
    }

    /// When wlan0 drops for the hotspot and comes back, avahi can mistake its
    /// own stale announcement for another device, rename the clock to
    /// `<host>-2.local` and stop answering on the usual name. A restart makes
    /// it announce the real hostname again. `try-restart` does nothing if
    /// avahi isn't running.
    fn reclaim_hostname(&mut self) {
        match Command::new("systemctl")
            .args(["try-restart", "avahi-daemon.service"])
            .status()
        {
            Ok(s) if s.success() => eprintln!("Restarted avahi to reclaim the .local name"),
            Ok(s) => eprintln!("avahi restart exited with {}", s),
            Err(e) => eprintln!("avahi restart failed: {}", e),
        }
    }
}
