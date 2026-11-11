//! systemd service notifications (sd_notify) without extra dependencies.
//!
//! The unit sets `WatchdogSec=`, so systemd restarts the app unless it hears
//! `WATCHDOG=1` regularly. We ping from a timer on the UI event loop, so a
//! hung UI stops the pings and gets restarted.
//!
//! No-op when `NOTIFY_SOCKET` isn't set (dev machines, manual runs).

use std::time::Duration;

/// Send one notification message, e.g. "READY=1" or "WATCHDOG=1".
pub fn notify(message: &str) {
    let Some(path) = std::env::var_os("NOTIFY_SOCKET") else {
        return;
    };
    if let Err(e) = send(&path, message) {
        eprintln!("sd_notify {:?} failed: {}", message, e);
    }
}

#[cfg(unix)]
fn send(path: &std::ffi::OsStr, message: &str) -> std::io::Result<()> {
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::net::UnixDatagram;

    let socket = UnixDatagram::unbound()?;
    let bytes = path.as_bytes();

    // A leading '@' means a Linux abstract-namespace socket.
    if let Some(name) = bytes.strip_prefix(b"@") {
        #[cfg(target_os = "linux")]
        {
            use std::os::linux::net::SocketAddrExt;
            let addr = std::os::unix::net::SocketAddr::from_abstract_name(name)?;
            socket.send_to_addr(message.as_bytes(), &addr)?;
            return Ok(());
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = name;
            return Ok(());
        }
    }

    socket.send_to(message.as_bytes(), path)?;
    Ok(())
}

#[cfg(not(unix))]
fn send(_path: &std::ffi::OsStr, _message: &str) -> std::io::Result<()> {
    Ok(())
}

/// How often to ping: half of systemd's watchdog interval (`WATCHDOG_USEC`),
/// or None if the watchdog isn't enabled for this process.
pub fn ping_interval() -> Option<Duration> {
    let usec: u64 = std::env::var("WATCHDOG_USEC").ok()?.parse().ok()?;
    if usec == 0 {
        return None;
    }
    Some(Duration::from_micros(usec / 2))
}
