use crate::config::Config;
use crate::flight::adsb::{fetch_flights_in_radius, filter_valid_flights};
use crate::flight::tracker::FlightTracker;
use crate::flight::types::{FlightInfo, SlimFlight};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// Snapshot of flight state sent from poller to UI thread.
#[derive(Clone)]
pub struct FlightState {
    pub flights: Vec<SlimFlight>,
    pub new_flights: Vec<String>,
    pub new_flights_with_info: Vec<NewFlightWithInfo>,
    pub all_flights_with_info: Vec<NewFlightWithInfo>,
    pub flight_count: usize,
    pub has_new_flights: bool,
}

#[derive(Clone)]
pub struct NewFlightWithInfo {
    pub hex: String,
    pub callsign: String,
    pub origin_location: String,
    pub origin_name: String,
    pub destination_location: String,
    pub destination_name: String,
    pub has_route: bool,
}

impl NewFlightWithInfo {
    pub fn from_parts(hex: &str, slim: &SlimFlight, info: Option<&FlightInfo>) -> Self {
        match info {
            Some(fi) => {
                let has_route = fi.origin.is_some() && fi.destination.is_some();
                Self {
                    hex: hex.to_string(),
                    callsign: slim.callsign.clone(),
                    origin_location: fi.origin.as_ref().map(|a| a.location().to_string()).unwrap_or_default(),
                    origin_name: fi.origin.as_ref().map(|a| a.display_name().to_string()).unwrap_or_default(),
                    destination_location: fi.destination.as_ref().map(|a| a.location().to_string()).unwrap_or_default(),
                    destination_name: fi.destination.as_ref().map(|a| a.display_name().to_string()).unwrap_or_default(),
                    has_route,
                }
            }
            None => Self {
                hex: hex.to_string(),
                callsign: slim.callsign.clone(),
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
}

impl Poller {
    pub fn new(config: Config, poll_interval_ms: Arc<AtomicU64>) -> Self {
        Self {
            config,
            poll_interval_ms,
        }
    }

    /// Run the polling loop. Calls `on_update` with new state after each successful poll.
    /// This blocks the current thread indefinitely.
    pub fn run<F>(&self, on_update: F)
    where
        F: Fn(FlightState) + Send + 'static,
    {
        let mut tracker = FlightTracker::new();

        loop {
            let interval = self.poll_interval_ms.load(Ordering::Relaxed);

            match fetch_flights_in_radius(
                self.config.latitude,
                self.config.longitude,
                self.config.radius_nm,
            ) {
                Ok(flights) => {
                    let valid = filter_valid_flights(flights);
                    let new_ids = tracker.update_flights(valid);
                    let has_new = !new_ids.is_empty();

                    let new_with_info: Vec<NewFlightWithInfo> = tracker
                        .get_new_flights_with_info()
                        .iter()
                        .map(|(hex, slim, info)| NewFlightWithInfo::from_parts(hex, slim, *info))
                        .collect();

                    let all_with_info: Vec<NewFlightWithInfo> = tracker
                        .get_all_flights_with_info()
                        .iter()
                        .map(|(hex, slim, info)| NewFlightWithInfo::from_parts(hex, slim, *info))
                        .collect();

                    let state = FlightState {
                        flights: tracker.get_current_flights(),
                        new_flights: new_ids,
                        new_flights_with_info: new_with_info,
                        all_flights_with_info: all_with_info,
                        flight_count: tracker.flight_count(),
                        has_new_flights: has_new,
                    };

                    on_update(state);
                }
                Err(e) => {
                    eprintln!("Flight poll error: {}", e);
                }
            }

            std::thread::sleep(Duration::from_millis(interval));
        }
    }
}
