use std::process::Command;

const WIFI_CONFIG_PATH: &str = "/etc/flight-clock/wifi.json";
const AP_SSID: &str = "FlightClock-Setup";
const AP_IP: &str = "192.168.4.1";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct WifiCredentials {
    pub ssid: String,
    pub psk: String,
}

#[derive(Debug, Clone)]
pub struct ScannedNetwork {
    pub ssid: String,
    pub signal_strength: i32,
    pub encrypted: bool,
}

pub struct WifiManager {
    ap_active: bool,
}

impl WifiManager {
    pub fn new() -> Self {
        Self { ap_active: false }
    }

    /// Check if we have saved WiFi credentials.
    pub fn has_saved_network(&self) -> bool {
        let path = std::path::Path::new(WIFI_CONFIG_PATH);
        if !path.exists() {
            return false;
        }
        match std::fs::read_to_string(path) {
            Ok(contents) => serde_json::from_str::<WifiCredentials>(&contents).is_ok(),
            Err(_) => false,
        }
    }

    /// Load saved WiFi credentials.
    pub fn load_credentials(&self) -> Option<WifiCredentials> {
        let contents = std::fs::read_to_string(WIFI_CONFIG_PATH).ok()?;
        serde_json::from_str(&contents).ok()
    }

    /// Save WiFi credentials to disk.
    pub fn save_credentials(&self, creds: &WifiCredentials) -> Result<(), String> {
        let path = std::path::Path::new(WIFI_CONFIG_PATH);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create config dir: {}", e))?;
        }

        let json = serde_json::to_string_pretty(creds)
            .map_err(|e| format!("Failed to serialize credentials: {}", e))?;

        std::fs::write(path, &json)
            .map_err(|e| format!("Failed to write credentials: {}", e))?;

        // Set permissions to 600 (owner read/write only)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            std::fs::set_permissions(path, perms)
                .map_err(|e| format!("Failed to set permissions: {}", e))?;
        }

        Ok(())
    }

    /// Start WiFi AP mode for captive portal setup.
    pub fn start_ap_mode(&mut self) -> Result<(), String> {
        eprintln!("Starting AP mode: SSID={}", AP_SSID);

        // Stop wpa_supplicant
        let _ = Command::new("systemctl")
            .args(["stop", "wpa_supplicant"])
            .output();

        // Configure and start hostapd
        let hostapd_conf = format!(
            "interface=wlan0\ndriver=nl80211\nssid={}\nhw_mode=g\nchannel=7\nwmm_enabled=0\nmacaddr_acl=0\nauth_algs=1\nignore_broadcast_ssid=0\n",
            AP_SSID
        );

        std::fs::write("/tmp/hostapd.conf", hostapd_conf)
            .map_err(|e| format!("Failed to write hostapd config: {}", e))?;

        // Configure network interface
        let _ = Command::new("ip")
            .args(["addr", "flush", "dev", "wlan0"])
            .output();
        let _ = Command::new("ip")
            .args(["addr", "add", &format!("{}/24", AP_IP), "dev", "wlan0"])
            .output();
        let _ = Command::new("ip")
            .args(["link", "set", "wlan0", "up"])
            .output();

        // Start hostapd
        let _ = Command::new("hostapd")
            .args(["-B", "/tmp/hostapd.conf"])
            .output();

        // Configure and start dnsmasq for DHCP + DNS redirect
        let dnsmasq_conf = format!(
            "interface=wlan0\ndhcp-range=192.168.4.2,192.168.4.20,255.255.255.0,24h\naddress=/#/{}\n",
            AP_IP
        );

        std::fs::write("/tmp/dnsmasq.conf", dnsmasq_conf)
            .map_err(|e| format!("Failed to write dnsmasq config: {}", e))?;

        let _ = Command::new("dnsmasq")
            .args(["-C", "/tmp/dnsmasq.conf"])
            .output();

        self.ap_active = true;
        Ok(())
    }

    /// Stop AP mode.
    pub fn stop_ap_mode(&mut self) -> Result<(), String> {
        eprintln!("Stopping AP mode");

        let _ = Command::new("killall").args(["hostapd"]).output();
        let _ = Command::new("killall").args(["dnsmasq"]).output();
        let _ = Command::new("ip")
            .args(["addr", "flush", "dev", "wlan0"])
            .output();

        self.ap_active = false;
        Ok(())
    }

    /// Connect to a WiFi network using wpa_supplicant.
    pub fn connect(&mut self, ssid: &str, password: &str) -> Result<bool, String> {
        eprintln!("Connecting to WiFi: {}", ssid);

        // Stop AP mode if active
        if self.ap_active {
            self.stop_ap_mode()?;
        }

        // Generate PSK hash using wpa_passphrase
        let psk_output = Command::new("wpa_passphrase")
            .args([ssid, password])
            .output()
            .map_err(|e| format!("wpa_passphrase failed: {}", e))?;

        let psk_config = String::from_utf8_lossy(&psk_output.stdout).to_string();

        if psk_config.is_empty() {
            return Err("wpa_passphrase produced empty output".to_string());
        }

        // Write wpa_supplicant config
        let wpa_conf = format!(
            "ctrl_interface=DIR=/var/run/wpa_supplicant GROUP=netdev\nupdate_config=1\ncountry=GB\n\n{}",
            psk_config
        );

        std::fs::write("/etc/wpa_supplicant/wpa_supplicant.conf", &wpa_conf)
            .map_err(|e| format!("Failed to write wpa_supplicant config: {}", e))?;

        // Restart networking
        let _ = Command::new("systemctl")
            .args(["restart", "wpa_supplicant"])
            .output();
        let _ = Command::new("systemctl")
            .args(["restart", "dhcpcd"])
            .output();

        // Wait for connection (up to 15 seconds)
        for _ in 0..15 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            if self.is_connected() {
                // Save credentials
                let _ = self.save_credentials(&WifiCredentials {
                    ssid: ssid.to_string(),
                    psk: psk_config.clone(),
                });
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Connect using saved credentials.
    pub fn connect_saved(&mut self) -> Result<bool, String> {
        eprintln!("Connecting with saved credentials");

        // Start wpa_supplicant
        let _ = Command::new("systemctl")
            .args(["start", "wpa_supplicant"])
            .output();
        let _ = Command::new("systemctl")
            .args(["start", "dhcpcd"])
            .output();

        // Wait for connection (up to 15 seconds)
        for _ in 0..15 {
            std::thread::sleep(std::time::Duration::from_secs(1));
            if self.is_connected() {
                return Ok(true);
            }
        }

        Ok(false)
    }

    /// Check if we're connected to WiFi.
    pub fn is_connected(&self) -> bool {
        let output = Command::new("iwgetid")
            .args(["-r"])
            .output()
            .ok();

        match output {
            Some(o) => !String::from_utf8_lossy(&o.stdout).trim().is_empty(),
            None => false,
        }
    }

    /// Scan for available WiFi networks.
    pub fn scan_networks(&self) -> Vec<ScannedNetwork> {
        let output = Command::new("iwlist")
            .args(["wlan0", "scan"])
            .output()
            .ok();

        let raw = match output {
            Some(o) => String::from_utf8_lossy(&o.stdout).to_string(),
            None => return Vec::new(),
        };

        let mut networks: Vec<ScannedNetwork> = Vec::new();
        let mut current_ssid: Option<String> = None;
        let mut current_signal: i32 = 0;
        let mut current_encrypted = false;

        for line in raw.lines() {
            let trimmed = line.trim();

            if trimmed.starts_with("Cell ") {
                // Save previous network
                if let Some(ssid) = current_ssid.take() {
                    if !ssid.is_empty() {
                        networks.push(ScannedNetwork {
                            ssid,
                            signal_strength: current_signal,
                            encrypted: current_encrypted,
                        });
                    }
                }
                current_signal = 0;
                current_encrypted = false;
            }

            if trimmed.starts_with("ESSID:") {
                current_ssid = Some(
                    trimmed
                        .trim_start_matches("ESSID:")
                        .trim_matches('"')
                        .to_string(),
                );
            }

            if trimmed.contains("Signal level=") {
                if let Some(level_str) = trimmed.split("Signal level=").nth(1) {
                    if let Some(val_str) = level_str.split(' ').next() {
                        current_signal = val_str.parse().unwrap_or(0);
                    }
                }
            }

            if trimmed.starts_with("Encryption key:on") {
                current_encrypted = true;
            }
        }

        // Don't forget the last network
        if let Some(ssid) = current_ssid {
            if !ssid.is_empty() {
                networks.push(ScannedNetwork {
                    ssid,
                    signal_strength: current_signal,
                    encrypted: current_encrypted,
                });
            }
        }

        // Sort by signal strength (strongest first)
        networks.sort_by(|a, b| b.signal_strength.cmp(&a.signal_strength));

        // Deduplicate by SSID
        let mut seen = std::collections::HashSet::new();
        networks.retain(|n| seen.insert(n.ssid.clone()));

        networks
    }

    pub fn ap_ssid(&self) -> &str {
        AP_SSID
    }
}
