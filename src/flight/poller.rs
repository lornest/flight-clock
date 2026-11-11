use crate::config::Config;
use crate::flight::adsb::{fetch_flights_in_radius, filter_valid_flights};
use crate::flight::tracker::FlightTracker;
use crate::flight::types::{FlightInfo, SlimFlight};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Snapshot of flight state sent from poller to UI thread.
#[derive(Clone)]
pub struct FlightState {
    pub flights: Vec<SlimFlight>,
    pub new_flights: Vec<String>,
    pub new_flights_with_info: Vec<NewFlightWithInfo>,
    pub flight_count: usize,
    pub has_new_flights: bool,
}

/// What the poller tells the UI.
pub enum PollEvent {
    /// A request has started.
    Fetching,
    Updated(FlightState),
    /// The request failed; message is short enough for the screen.
    Failed(String),
}

#[derive(Clone)]
pub struct NewFlightWithInfo {
    pub hex: String,
    pub callsign: String,
    pub aircraft_type: String,
    pub origin_location: String,
    pub origin_name: String,
    pub destination_location: String,
    pub destination_name: String,
    pub has_route: bool,
}

impl NewFlightWithInfo {
    pub fn from_parts(hex: &str, slim: &SlimFlight, info: Option<&FlightInfo>) -> Self {
        let callsign = if slim.callsign.is_empty() {
            hex.to_uppercase()
        } else {
            slim.callsign.clone()
        };
        let aircraft_type = slim.aircraft_type.clone().unwrap_or_default();

        let route = info.and_then(|fi| Some((fi.origin.as_ref()?, fi.destination.as_ref()?)));
        match route {
            Some((origin, dest)) => Self {
                hex: hex.to_string(),
                callsign,
                aircraft_type,
                origin_location: origin.location().to_string(),
                origin_name: origin.display_name().to_string(),
                destination_location: dest.location().to_string(),
                destination_name: dest.display_name().to_string(),
                has_route: true,
            },
            None => Self {
                hex: hex.to_string(),
                callsign,
                aircraft_type,
                origin_location: String::new(),
                origin_name: String::new(),
                destination_location: String::new(),
                destination_name: String::new(),
                has_route: false,
            },
        }
    }
}

/// Flight polling loop that runs in a background thread.
pub struct Poller {
    config: Config,
    poll_interval_ms: Arc<AtomicU64>,
    network_ready: Arc<AtomicBool>,
}

impl Poller {
    pub fn new(config: Config, poll_interval_ms: Arc<AtomicU64>, network_ready: Arc<AtomicBool>) -> Self {
        Self {
            config,
            poll_interval_ms,
            network_ready,
        }
    }

    /// Run the polling loop forever, reporting each step through `on_event`.
    /// Pauses while the WiFi setup hotspot is up.
    pub fn run<F>(&self, on_event: F)
    where
        F: Fn(PollEvent) + Send + 'static,
    {
        let mut tracker = FlightTracker::new();

        loop {
            if !self.network_ready.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(1));
                continue;
            }

            let started = Instant::now();
            let interval = self.poll_interval_ms.load(Ordering::Relaxed);
            on_event(PollEvent::Fetching);

            match fetch_flights_in_radius(self.config.latitude, self.config.longitude, self.config.radius_nm) {
                Ok(flights) => {
                    let valid = filter_valid_flights(flights);
                    let new_ids = tracker.update_flights(valid);
                    let has_new = !new_ids.is_empty();

                    let new_with_info: Vec<NewFlightWithInfo> = tracker
                        .get_new_flights_with_info()
                        .iter()
                        .map(|(hex, slim, info)| NewFlightWithInfo::from_parts(hex, slim, *info))
                        .collect();

                    on_event(PollEvent::Updated(FlightState {
                        flights: tracker.get_current_flights(),
                        new_flights: new_ids,
                        new_flights_with_info: new_with_info,
                        flight_count: tracker.flight_count(),
                        has_new_flights: has_new,
                    }));
                }
                Err(e) => on_event(PollEvent::Failed(e)),
            }

            self.wait_for_next_poll(started, interval);
        }
    }

    /// Sleep until the poll interval has passed since `started`. Re-reads the
    /// interval while waiting, so switching to the radar or starting an alert
    /// (30s -> 5s) triggers the next fetch straight away instead of after the
    /// rest of the old 30s wait.
    fn wait_for_next_poll(&self, started: Instant, mut interval: u64) {
        loop {
            let elapsed = started.elapsed();
            if elapsed >= Duration::from_millis(interval) {
                return;
            }
            let step = (Duration::from_millis(interval) - elapsed).min(Duration::from_millis(250));
            std::thread::sleep(step);
            interval = self.poll_interval_ms.load(Ordering::Relaxed);
        }
    }
}
