//! The web server process: `flight-clock web`.
//!
//! Serves the settings page and the WiFi setup portal as the unprivileged
//! `flight-clock-web` user (systemd/flight-clock-web.service). Everything
//! that needs privileges is asked of the clock over the link in src/ipc.rs.

use crate::ipc::{ClockLink, ToWeb};
use crate::settings_web::SettingsWeb;
use crate::wifi::portal::{Portal, PortalState};
use std::sync::{Arc, Mutex, OnceLock};

/// Serve until the listening socket fails. Returns only on error.
pub fn run(port: u16) {
    let portal_state = Arc::new(Mutex::new(PortalState::default()));
    // Filled in once the settings page exists; the link needs to reach it
    // and it needs the link.
    let settings_slot: Arc<OnceLock<Arc<SettingsWeb>>> = Arc::new(OnceLock::new());

    // The token in the QR code on the clock's screen, if one is showing
    let link_token: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    let clock = {
        let link = link_token.clone();
        let link_on_disconnect = link_token.clone();
        let portal = portal_state.clone();
        let settings = settings_slot.clone();
        let portal_on_disconnect = portal_state.clone();
        let settings_on_disconnect = settings_slot.clone();
        ClockLink::connect(
            move |message| match message {
                ToWeb::Portal { active, networks, message } => {
                    let mut state = portal.lock().unwrap();
                    state.active = active;
                    state.networks = networks;
                    state.message = message;
                }
                ToWeb::CancelCode => {
                    if let Some(settings) = settings.get() {
                        settings.cancel_code();
                    }
                }
                ToWeb::PairLink { token } => *link.lock().unwrap() = token,
                ToWeb::Reply { .. } => {}
            },
            // The clock restarted: its screen no longer shows a code or QR
            // code, and it will say again whether the hotspot is up.
            move || {
                portal_on_disconnect.lock().unwrap().active = false;
                if let Some(settings) = settings_on_disconnect.get() {
                    settings.cancel_code();
                }
                *link_on_disconnect.lock().unwrap() = None;
            },
        )
    };

    let settings = Arc::new(SettingsWeb::new(clock.clone(), link_token));
    let _ = settings_slot.set(settings.clone());

    Portal::new(portal_state, clock, settings, port).serve();
}
