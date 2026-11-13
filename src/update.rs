//! Self-update from signed GitHub releases.
//!
//! Runs as its own systemd unit, `flight-clock-update@<mode>.service`, so it
//! survives restarting the clock:
//!   scheduled  daily timer: installs if auto-update is on, else only checks
//!   now        "Update now" on the settings page: installs
//!   check      "Check now" on the settings page: only checks
//!
//! Layout:
//!   /opt/flight-clock/releases/<version>/   binary + scripts + migrations
//!   /opt/flight-clock/current  -> releases/<version>
//!   /opt/flight-clock/previous -> releases/<older version>
//!   /opt/flight-clock/flight-clock -> current/flight-clock   (systemd runs this)
//!   /var/lib/flight-clock/      status, applied migrations, failed versions
//!
//! Safety:
//! - Releases must be signed with one of the keys in keys/ (minisign):
//!   release.pub normally, release-backup.pub if that key is lost or leaked.
//!   The signed trusted comment must be exactly "flight-clock <version> aarch64",
//!   so a validly signed but different file can't be substituted.
//! - After switching, the clock must stay up for HEALTH_WAIT without a
//!   restart, or the previous version is restored and the new one marked bad.
//!   HEALTH_WAIT is longer than the unit's WatchdogSec, so a build that
//!   starts and then hangs is restarted by the watchdog inside the wait and
//!   counted as bad. The web server must be running at the end of it too.

use crate::config::Config;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

pub const REPO: &str = "lornest/flight-clock";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const ARCH: &str = "aarch64";
/// A release is accepted if any one of these signed it. The backup key's
/// secret half is kept offline, apart from the main key: it can sign a release
/// if the main key is lost, or one that replaces the main key if it leaks.
const PUBLIC_KEYS: &[&str] = &[
    include_str!("../keys/release.pub"),
    include_str!("../keys/release-backup.pub"),
];
const SERVICE: &str = "flight-clock";
const WEB_SERVICE: &str = "flight-clock-web";

const MAX_DOWNLOAD_BYTES: u64 = 64 * 1024 * 1024;
const HEALTH_SETTLE: Duration = Duration::from_secs(15);
/// Must exceed WatchdogSec in systemd/flight-clock.service (a test checks):
/// a hung build only shows up as a restart once the watchdog has fired.
const HEALTH_WAIT: Duration = Duration::from_secs(150);
/// How long the web server gets to come back if it's mid-restart when checked
const WEB_RETRY_WAIT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Scheduled,
    Now,
    Check,
}

impl Mode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "scheduled" => Some(Mode::Scheduled),
            "now" => Some(Mode::Now),
            "check" => Some(Mode::Check),
            _ => None,
        }
    }
}

/// Written to the state dir after every run; shown on the settings page.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct UpdateStatus {
    /// "checking", "up_to_date", "available", "installing", "updated", "failed"
    pub state: String,
    pub current: String,
    pub latest: Option<String>,
    pub message: String,
    /// Unix seconds
    pub checked_at: u64,
}

fn install_dir() -> PathBuf {
    std::env::var_os("FLIGHT_CLOCK_INSTALL_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/opt/flight-clock"))
}

fn state_dir() -> PathBuf {
    std::env::var_os("FLIGHT_CLOCK_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/var/lib/flight-clock"))
}

pub fn read_status() -> Option<UpdateStatus> {
    let text = std::fs::read_to_string(state_dir().join("update-status.json")).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_status(state: &str, latest: Option<&Version>, message: &str) {
    let status = UpdateStatus {
        state: state.to_string(),
        current: VERSION.to_string(),
        latest: latest.map(|v| v.to_string()),
        message: message.to_string(),
        checked_at: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
    };
    let dir = state_dir();
    let _ = std::fs::create_dir_all(&dir);
    if let Ok(json) = serde_json::to_string_pretty(&status) {
        let tmp = dir.join("update-status.json.tmp");
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, dir.join("update-status.json"));
        }
    }
    eprintln!("update: {}{}", state, if message.is_empty() { String::new() } else { format!(" - {}", message) });
}

/// Ask systemd to run the updater in the background (from the settings page).
pub fn trigger(mode: Mode) -> Result<(), String> {
    let unit = match mode {
        Mode::Now => "flight-clock-update@now.service",
        Mode::Check => "flight-clock-update@check.service",
        Mode::Scheduled => "flight-clock-update@scheduled.service",
    };
    let out = Command::new("systemctl")
        .args(["start", "--no-block", unit])
        .output()
        .map_err(|e| format!("couldn't run systemctl: {}", e))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}

/// Entry point for `flight-clock update <mode>`.
pub fn run(mode: Mode) -> Result<(), String> {
    let cfg = Config::load(None);
    let beta = cfg.update_channel == "beta";
    let current = Version::parse(VERSION).ok_or("can't parse own version")?;

    write_status("checking", None, "");
    let release = match find_latest(beta) {
        Ok(r) => r,
        Err(e) => {
            write_status("failed", None, &format!("Couldn't check for updates: {}", e));
            return Err(e);
        }
    };

    let Some(release) = release.filter(|r| r.version > current) else {
        write_status("up_to_date", None, "");
        return Ok(());
    };

    let bad = read_lines(&state_dir().join("bad-versions"));
    let previously_failed = bad.contains(&release.version.to_string());

    let install = match mode {
        Mode::Now => true,
        Mode::Check => false,
        Mode::Scheduled => cfg.auto_update && !previously_failed,
    };
    if !install {
        let note = if previously_failed { "This version failed to start before." } else { "" };
        write_status("available", Some(&release.version), note);
        return Ok(());
    }

    write_status("installing", Some(&release.version), "");
    match install_release(&release) {
        Ok(()) => {
            // Report as the new version: this process is still the old binary
            let status = UpdateStatus {
                state: "updated".into(),
                current: release.version.to_string(),
                latest: Some(release.version.to_string()),
                message: format!("Updated from {}", VERSION),
                checked_at: SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
            };
            if let Ok(json) = serde_json::to_string_pretty(&status) {
                let _ = std::fs::write(state_dir().join("update-status.json"), json);
            }
            eprintln!("update: installed {}", release.version);
            Ok(())
        }
        Err(e) => {
            write_status("failed", Some(&release.version), &e);
            Err(e)
        }
    }
}

// ---------------------------------------------------------------------------
// Finding releases
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Release {
    version: Version,
    tarball_url: String,
    signature_url: String,
}

#[derive(Deserialize)]
struct GhRelease {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    assets: Vec<GhAsset>,
}

#[derive(Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

fn agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(15))
        .timeout(Duration::from_secs(300))
        .user_agent(&format!("flight-clock-updater/{}", VERSION))
        .build()
}

fn find_latest(include_beta: bool) -> Result<Option<Release>, String> {
    let url = format!("https://api.github.com/repos/{}/releases?per_page=30", REPO);
    let releases: Vec<GhRelease> = agent()
        .get(&url)
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(|e| format!("GitHub: {}", e))?
        .into_json()
        .map_err(|e| format!("GitHub response: {}", e))?;
    Ok(pick_release(releases, include_beta))
}

fn pick_release(releases: Vec<GhRelease>, include_beta: bool) -> Option<Release> {
    releases
        .into_iter()
        .filter(|r| !r.draft && (include_beta || !r.prerelease))
        .filter_map(|r| {
            let version = Version::parse(r.tag_name.trim_start_matches('v'))?;
            if version.pre.is_some() && !include_beta {
                return None;
            }
            let tarball_name = tarball_name(&version);
            let tarball = r.assets.iter().find(|a| a.name == tarball_name)?;
            let signature = r.assets.iter().find(|a| a.name == format!("{}.minisig", tarball_name))?;
            Some(Release {
                version,
                tarball_url: tarball.browser_download_url.clone(),
                signature_url: signature.browser_download_url.clone(),
            })
        })
        .max_by(|a, b| a.version.cmp(&b.version))
}

fn tarball_name(version: &Version) -> String {
    format!("flight-clock-{}-{}.tar.gz", version, ARCH)
}

fn download(url: &str) -> Result<Vec<u8>, String> {
    let response = agent().get(url).call().map_err(|e| format!("download failed: {}", e))?;
    let mut bytes = Vec::new();
    response
        .into_reader()
        .take(MAX_DOWNLOAD_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| format!("download failed: {}", e))?;
    if bytes.len() as u64 > MAX_DOWNLOAD_BYTES {
        return Err("download too large".into());
    }
    Ok(bytes)
}

/// Check the tarball against the release keys and the expected trusted comment.
fn verify(tarball: &[u8], signature_text: &str, version: &Version) -> Result<(), String> {
    verify_with_any(PUBLIC_KEYS, tarball, signature_text, version)
}

/// Ok if any one of the keys accepts it; otherwise the first key's error.
fn verify_with_any(keys: &[&str], tarball: &[u8], signature_text: &str, version: &Version) -> Result<(), String> {
    let mut first_error = None;
    for key in keys {
        match verify_with(key, tarball, signature_text, version) {
            Ok(()) => return Ok(()),
            Err(e) => first_error = first_error.or(Some(e)),
        }
    }
    Err(first_error.unwrap_or_else(|| "no release keys".into()))
}

fn verify_with(public_key: &str, tarball: &[u8], signature_text: &str, version: &Version) -> Result<(), String> {
    let key = minisign_verify::PublicKey::decode(public_key.trim()).map_err(|e| format!("bad public key: {}", e))?;
    let signature =
        minisign_verify::Signature::decode(signature_text.trim()).map_err(|e| format!("bad signature file: {}", e))?;
    key.verify(tarball, &signature, false)
        .map_err(|e| format!("signature check failed: {}", e))?;
    let expected = format!("flight-clock {} {}", version, ARCH);
    if signature.trusted_comment().trim() != expected {
        return Err(format!(
            "signature is for '{}', expected '{}'",
            signature.trusted_comment().trim(),
            expected
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Installing
// ---------------------------------------------------------------------------

fn install_release(release: &Release) -> Result<(), String> {
    let version = release.version.to_string();
    let root = install_dir();
    let releases = root.join("releases");
    let state = state_dir();
    std::fs::create_dir_all(&releases).map_err(|e| format!("creating {}: {}", releases.display(), e))?;
    std::fs::create_dir_all(&state).map_err(|e| format!("creating {}: {}", state.display(), e))?;

    // Download and verify
    let tarball = download(&release.tarball_url)?;
    let signature = download(&release.signature_url)?;
    verify(&tarball, &String::from_utf8_lossy(&signature), &release.version)?;

    // Unpack into a staging dir, then move into place
    let tar_path = state.join("download.tar.gz");
    std::fs::write(&tar_path, &tarball).map_err(|e| format!("saving download: {}", e))?;
    let staging = releases.join(format!("{}.staging", version));
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| format!("creating staging dir: {}", e))?;
    run_cmd(
        "tar",
        &["-xzf", path_str(&tar_path)?, "-C", path_str(&staging)?, "--strip-components=1", "--no-same-owner"],
    )?;
    let _ = std::fs::remove_file(&tar_path);
    if !staging.join("flight-clock").is_file() {
        return Err("release archive has no flight-clock binary".into());
    }

    let target = releases.join(&version);
    let current = std::fs::read_link(root.join("current")).ok();
    if current.as_deref().map(|c| resolve(&root, c)) == Some(target.clone()) {
        return Err(format!("{} is already the current version", version));
    }
    let _ = std::fs::remove_dir_all(&target);
    std::fs::rename(&staging, &target).map_err(|e| format!("installing release: {}", e))?;

    // One-time setup steps shipped with the release
    run_migrations(&target)?;

    // Switch, restart, and make sure it stays up
    let previous = current.map(|c| resolve(&root, &c));
    switch_link(&root.join("current"), &target)?;
    if let Some(prev) = &previous {
        switch_link(&root.join("previous"), prev)?;
    }
    ensure_binary_link(&root)?;

    if let Err(e) = restart_and_check() {
        eprintln!("update: {} unhealthy ({}), rolling back", version, e);
        append_line(&state.join("bad-versions"), &version);
        match &previous {
            Some(prev) => {
                switch_link(&root.join("current"), prev)?;
                let _ = run_cmd("systemctl", &["restart", SERVICE]);
                return Err(format!("{} didn't start properly ({}); went back to the previous version", version, e));
            }
            None => return Err(format!("{} didn't start properly ({}), and there's no previous version", version, e)),
        }
    }

    prune_releases(&releases, &target, previous.as_deref());
    Ok(())
}

/// Run migrations/NNNN-*.sh from the release that haven't been run before.
fn run_migrations(release_dir: &Path) -> Result<(), String> {
    let dir = release_dir.join("migrations");
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Ok(());
    };
    let mut scripts: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "sh"))
        .collect();
    scripts.sort();

    let applied_path = state_dir().join("migrations-applied");
    let applied = read_lines(&applied_path);
    for script in scripts {
        let name = script.file_name().unwrap_or_default().to_string_lossy().to_string();
        if applied.contains(&name) {
            continue;
        }
        eprintln!("update: running migration {}", name);
        let out = Command::new("bash")
            .arg(&script)
            .current_dir(release_dir)
            .env("FLIGHT_CLOCK_RELEASE_DIR", release_dir)
            .output()
            .map_err(|e| format!("migration {}: {}", name, e))?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!("migration {} failed: {}", name, err.lines().last().unwrap_or("").trim()));
        }
        append_line(&applied_path, &name);
    }
    Ok(())
}

/// Restart the clock and require it to stay up (same process) for HEALTH_WAIT.
fn restart_and_check() -> Result<(), String> {
    run_cmd("systemctl", &["restart", SERVICE])?;
    std::thread::sleep(HEALTH_SETTLE);
    let pid = main_pid().ok_or("not running after restart")?;
    std::thread::sleep(HEALTH_WAIT - HEALTH_SETTLE);
    let active = Command::new("systemctl")
        .args(["is-active", "--quiet", SERVICE])
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if !active {
        return Err("stopped".into());
    }
    if main_pid() != Some(pid) {
        return Err("restarted on its own".into());
    }
    // The settings page is part of the release too. Only where its unit is
    // installed and enabled (it restarts with the clock, see the unit file).
    if is_enabled(WEB_SERVICE) && !is_active(WEB_SERVICE) {
        std::thread::sleep(WEB_RETRY_WAIT);
        if !is_active(WEB_SERVICE) {
            return Err("the settings page isn't running".into());
        }
    }
    Ok(())
}

fn is_active(service: &str) -> bool {
    Command::new("systemctl")
        .args(["is-active", "--quiet", service])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn is_enabled(service: &str) -> bool {
    Command::new("systemctl")
        .args(["is-enabled", "--quiet", service])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn main_pid() -> Option<u32> {
    let out = Command::new("systemctl")
        .args(["show", "-p", "MainPID", "--value", SERVICE])
        .output()
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok().filter(|p| *p != 0)
}

/// Keep current, previous and any dev build; remove older releases.
fn prune_releases(releases: &Path, current: &Path, previous: Option<&Path>) {
    let Ok(entries) = std::fs::read_dir(releases) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        if path == current || Some(path.as_path()) == previous || name == "dev" {
            continue;
        }
        let _ = std::fs::remove_dir_all(&path);
    }
}

/// Point `link` at `target`, atomically (new symlink, then rename over).
fn switch_link(link: &Path, target: &Path) -> Result<(), String> {
    let tmp = link.with_extension("new");
    let _ = std::fs::remove_file(&tmp);
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, &tmp).map_err(|e| format!("symlink {}: {}", tmp.display(), e))?;
    std::fs::rename(&tmp, link).map_err(|e| format!("switching {}: {}", link.display(), e))
}

/// /opt/flight-clock/flight-clock -> current/flight-clock (what systemd runs).
fn ensure_binary_link(root: &Path) -> Result<(), String> {
    let link = root.join("flight-clock");
    let target = PathBuf::from("current/flight-clock");
    if std::fs::read_link(&link).ok().as_deref() == Some(target.as_path()) {
        return Ok(());
    }
    switch_link(&link, &target)
}

fn resolve(root: &Path, link_target: &Path) -> PathBuf {
    if link_target.is_absolute() {
        link_target.to_path_buf()
    } else {
        root.join(link_target)
    }
}

fn run_cmd(cmd: &str, args: &[&str]) -> Result<(), String> {
    let out = Command::new(cmd).args(args).output().map_err(|e| format!("{}: {}", cmd, e))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!("{} failed: {}", cmd, String::from_utf8_lossy(&out.stderr).trim()))
    }
}

fn path_str(p: &Path) -> Result<&str, String> {
    p.to_str().ok_or_else(|| format!("non-UTF-8 path {}", p.display()))
}

fn read_lines(path: &Path) -> Vec<String> {
    std::fs::read_to_string(path)
        .map(|s| s.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect())
        .unwrap_or_default()
}

fn append_line(path: &Path, line: &str) {
    let mut lines = read_lines(path);
    if !lines.iter().any(|l| l == line) {
        lines.push(line.to_string());
        let _ = std::fs::write(path, lines.join("\n") + "\n");
    }
}

// ---------------------------------------------------------------------------
// Versions: MAJOR.MINOR.PATCH with an optional -pre (e.g. 0.3.0-beta.1)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    pre: Option<String>,
}

impl Version {
    pub fn parse(s: &str) -> Option<Self> {
        let (core, pre) = match s.split_once('-') {
            // The version becomes a file and folder name, so the pre-release
            // part is limited to semver's characters, with no empty or
            // dot-only pieces (no "..", no "/").
            Some((c, p)) if is_pre_release(p) => (c, Some(p.to_string())),
            Some(_) => return None,
            None => (s, None),
        };
        let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
        let v = Version {
            major: parts.next()??,
            minor: parts.next()??,
            patch: parts.next()??,
            pre,
        };
        parts.next().is_none().then_some(v)
    }
}

fn is_pre_release(pre: &str) -> bool {
    pre.len() <= 32
        && pre.split('.').all(|part| {
            !part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        })
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)?;
        if let Some(pre) = &self.pre {
            write!(f, "-{}", pre)?;
        }
        Ok(())
    }
}

impl Ord for Version {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (&self.pre, &other.pre) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater, // 1.0.0 > 1.0.0-beta
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => compare_pre(a, b),
            })
    }
}

impl PartialOrd for Version {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Dot-separated identifiers; numeric ones compare as numbers.
fn compare_pre(a: &str, b: &str) -> Ordering {
    let mut ai = a.split('.');
    let mut bi = b.split('.');
    loop {
        match (ai.next(), bi.next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) => {
                let ord = match (x.parse::<u64>(), y.parse::<u64>()) {
                    (Ok(nx), Ok(ny)) => nx.cmp(&ny),
                    (Ok(_), Err(_)) => Ordering::Less,
                    (Err(_), Ok(_)) => Ordering::Greater,
                    _ => x.cmp(y),
                };
                if ord != Ordering::Equal {
                    return ord;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(s: &str) -> Version {
        Version::parse(s).unwrap()
    }

    #[test]
    fn version_ordering() {
        assert!(v("0.2.0") > v("0.1.9"));
        assert!(v("1.0.0") > v("1.0.0-beta.2"));
        assert!(v("1.0.0-beta.10") > v("1.0.0-beta.2"));
        assert!(v("1.0.0-rc.1") > v("1.0.0-beta.9"));
        assert_eq!(v("0.2.0").to_string(), "0.2.0");
        assert!(Version::parse("1.2").is_none());
        assert!(Version::parse("1.2.3.4").is_none());
        assert!(Version::parse("v1.2.3").is_none());
    }

    #[test]
    fn pre_release_is_limited_to_safe_characters() {
        assert_eq!(v("0.3.0-beta.1").to_string(), "0.3.0-beta.1");
        assert_eq!(v("0.3.0-rc-2").to_string(), "0.3.0-rc-2");
        for bad in ["1.0.0-", "1.0.0-../../etc", "1.0.0-a/b", "1.0.0-a..b", "1.0.0-.", "1.0.0-a b", "1.0.0-a\nb", "1.0.0-é"] {
            assert!(Version::parse(bad).is_none(), "{:?}", bad);
        }
    }

    #[test]
    fn health_wait_outlasts_the_watchdog() {
        let unit = include_str!("../systemd/flight-clock.service");
        let watchdog: u64 = unit
            .lines()
            .find_map(|l| l.trim().strip_prefix("WatchdogSec="))
            .expect("the unit sets WatchdogSec")
            .trim()
            .parse()
            .expect("WatchdogSec in plain seconds");
        // Watchdog fires, systemd restarts the service: allow 20 s for that to show
        assert!(HEALTH_WAIT.as_secs() >= watchdog + 20, "{} vs {}", HEALTH_WAIT.as_secs(), watchdog);
        assert!(HEALTH_WAIT > HEALTH_SETTLE);
    }

    /// The value(s) of a directive in the clock's unit file.
    fn unit_values(key: &str) -> Vec<&'static str> {
        include_str!("../systemd/flight-clock.service")
            .lines()
            .filter_map(|l| l.trim().strip_prefix(key)?.strip_prefix('='))
            .map(str::trim)
            .collect()
    }

    #[test]
    fn clock_service_is_sandboxed() {
        for key in [
            "NoNewPrivileges",
            "ProtectHome",
            "PrivateTmp",
            "ProtectKernelModules",
            "ProtectKernelLogs",
            "ProtectControlGroups",
            "RestrictNamespaces",
            "RestrictSUIDSGID",
            "LockPersonality",
            "MemoryDenyWriteExecute",
        ] {
            assert_eq!(unit_values(key), ["yes"], "{}", key);
        }
        assert_eq!(unit_values("ProtectSystem"), ["full"]);
        assert_eq!(unit_values("SystemCallFilter"), ["@system-service"]);

        // The powers that would undo the rest stay out
        let caps = unit_values("CapabilityBoundingSet").join(" ");
        assert!(!caps.is_empty());
        for cap in ["CAP_SYS_ADMIN", "CAP_SYS_MODULE", "CAP_SYS_PTRACE", "CAP_SYS_BOOT", "CAP_SYS_RAWIO", "CAP_SYS_TIME"] {
            assert!(!caps.contains(cap), "{}", cap);
        }
        // ...and the ones the clock relies on stay in
        for cap in ["CAP_CHOWN", "CAP_NET_ADMIN", "CAP_NET_BIND_SERVICE"] {
            assert!(caps.contains(cap), "{}", cap);
        }
    }

    #[test]
    fn sandbox_leaves_the_clock_what_it_needs() {
        // Each of these would take the display, touch or backlight away:
        // the app opens /dev/dri and /dev/input itself, and writes to /sys.
        // ProtectClock implies a device allow-list, so it counts too.
        for key in ["PrivateDevices", "DeviceAllow", "DevicePolicy", "ProtectClock", "ProtectKernelTunables", "ProtectProc"] {
            assert!(unit_values(key).is_empty(), "{} would break the clock", key);
        }
        // Everything it writes: settings, and the WiFi setup files
        let writable = unit_values("ReadWritePaths").join(" ");
        for path in ["/etc/flight-clock", "/etc/NetworkManager/dnsmasq-shared.d", "/etc/wpa_supplicant"] {
            assert!(writable.contains(path), "{}", path);
        }
        // "strict" would also make /var/lib/flight-clock and /run read-only
        assert_ne!(unit_values("ProtectSystem"), ["strict"]);
        // WiFi and input device handling need netlink; the web socket and D-Bus need unix
        let families = unit_values("RestrictAddressFamilies").join(" ");
        for family in ["AF_UNIX", "AF_INET", "AF_INET6", "AF_NETLINK"] {
            assert!(families.contains(family), "{}", family);
        }
        // Still root, still watched
        assert_eq!(unit_values("User"), ["root"]);
        assert_eq!(unit_values("WatchdogSec"), ["120"]);
    }

    #[test]
    fn the_hardened_unit_reaches_existing_clocks() {
        // Units aren't replaced by the updater; a migration has to install it
        let migration = include_str!("../migrations/0002-harden-clock-service.sh");
        assert!(migration.contains("systemd/flight-clock.service"));
        assert!(migration.contains("/etc/systemd/system/flight-clock.service"));
        assert!(migration.contains("systemctl daemon-reload"));
        // Fresh installs get it from install.sh
        assert!(include_str!("../scripts/install.sh").contains("flight-clock.service"));
    }

    #[test]
    fn own_version_parses() {
        assert!(Version::parse(VERSION).is_some());
    }

    #[test]
    fn public_keys_parse_and_differ() {
        for key in PUBLIC_KEYS {
            assert!(minisign_verify::PublicKey::decode(key.trim()).is_ok());
        }
        assert_eq!(PUBLIC_KEYS.len(), 2);
        assert_ne!(PUBLIC_KEYS[0].lines().last(), PUBLIC_KEYS[1].lines().last());
    }

    #[test]
    fn installer_trusts_the_same_keys() {
        // scripts/get.sh carries its own copy of the keys
        let get_sh = include_str!("../scripts/get.sh");
        for key in PUBLIC_KEYS {
            let line = key.lines().last().unwrap().trim();
            assert!(get_sh.contains(line), "scripts/get.sh is missing key {}", line);
        }
    }

    #[test]
    fn any_listed_key_is_enough() {
        let real = PUBLIC_KEYS[0];
        assert!(verify_with_any(&[real, TEST_KEY], TEST_PAYLOAD, TEST_SIG, &v("9.9.9")).is_ok());
        assert!(verify_with_any(&[TEST_KEY, real], TEST_PAYLOAD, TEST_SIG, &v("9.9.9")).is_ok());
        assert!(verify_with_any(&[real], TEST_PAYLOAD, TEST_SIG, &v("9.9.9")).is_err());
        assert!(verify_with_any(&[], TEST_PAYLOAD, TEST_SIG, &v("9.9.9")).is_err());
        // Right key, wrong version: still refused
        assert!(verify_with_any(&[real, TEST_KEY], TEST_PAYLOAD, TEST_SIG, &v("9.9.8")).is_err());
    }

    fn gh(tag: &str, prerelease: bool, draft: bool, with_sig: bool) -> GhRelease {
        let ver = tag.trim_start_matches('v');
        let mut assets = vec![GhAsset {
            name: format!("flight-clock-{}-aarch64.tar.gz", ver),
            browser_download_url: format!("https://example/{}.tar.gz", ver),
        }];
        if with_sig {
            assets.push(GhAsset {
                name: format!("flight-clock-{}-aarch64.tar.gz.minisig", ver),
                browser_download_url: format!("https://example/{}.minisig", ver),
            });
        }
        GhRelease { tag_name: tag.into(), draft, prerelease, assets }
    }

    #[test]
    fn picks_newest_published_signed_release() {
        let releases = vec![
            gh("v0.2.0", false, false, true),
            gh("v0.3.0-beta.1", true, false, true),
            gh("v0.4.0", false, true, true),  // draft
            gh("v0.2.5", false, false, false), // unsigned
        ];
        let stable = pick_release(releases, false).unwrap();
        assert_eq!(stable.version.to_string(), "0.2.0");

        let releases = vec![gh("v0.2.0", false, false, true), gh("v0.3.0-beta.1", true, false, true)];
        assert_eq!(pick_release(releases, true).unwrap().version.to_string(), "0.3.0-beta.1");
    }

    // Fixture signed with a throwaway test key (not the release key)
    const TEST_KEY: &str = include_str!("../tests/fixtures/test-key.pub");
    const TEST_PAYLOAD: &[u8] = include_bytes!("../tests/fixtures/payload.txt");
    const TEST_SIG: &str = include_str!("../tests/fixtures/payload.txt.minisig");

    #[test]
    fn accepts_valid_signature_for_expected_version() {
        assert!(verify_with(TEST_KEY, TEST_PAYLOAD, TEST_SIG, &v("9.9.9")).is_ok());
    }

    #[test]
    fn rejects_signature_for_another_version() {
        // Validly signed, but the trusted comment names 9.9.9
        let err = verify_with(TEST_KEY, TEST_PAYLOAD, TEST_SIG, &v("9.9.8")).unwrap_err();
        assert!(err.contains("expected"), "{}", err);
    }

    #[test]
    fn rejects_tampered_payload_and_wrong_key() {
        assert!(verify_with(TEST_KEY, b"tampered", TEST_SIG, &v("9.9.9")).is_err());
        // The real release key didn't sign the fixture
        assert!(verify(TEST_PAYLOAD, TEST_SIG, &v("9.9.9")).is_err());
    }

    #[test]
    fn rejects_bad_signatures() {
        let sig = "untrusted comment: x\nRUR2EfaRD2IfiQAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA=\ntrusted comment: flight-clock 0.2.0 aarch64\nAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA==";
        assert!(verify(b"hello", sig, &v("0.2.0")).is_err());
        assert!(verify(b"hello", "not a signature", &v("0.2.0")).is_err());
    }
}
