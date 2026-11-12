use crate::config::Location;
use crate::flight::adsb::{fetch_flights_in_radius, filter_valid_flights};
use crate::flight::tracker::FlightTracker;
use crate::flight::types::{FlightInfo, SlimFlight};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
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
                origin_location: origin.location(),
                origin_name: short_airport_name(&origin.location(), origin.display_name()),
                destination_location: dest.location(),
                destination_name: short_airport_name(&dest.location(), dest.display_name()),
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
    location: Arc<Mutex<Option<Location>>>,
    poll_interval_ms: Arc<AtomicU64>,
    network_ready: Arc<AtomicBool>,
}

impl Poller {
    pub fn new(
        location: Arc<Mutex<Option<Location>>>,
        poll_interval_ms: Arc<AtomicU64>,
        network_ready: Arc<AtomicBool>,
    ) -> Self {
        Self {
            location,
            poll_interval_ms,
            network_ready,
        }
    }

    /// Run the polling loop forever, reporting each step through `on_event`.
    /// Pauses while the WiFi setup hotspot is up, and until the clock has
    /// been told where it is.
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

            let Some(loc) = *self.location.lock().unwrap() else {
                std::thread::sleep(Duration::from_secs(1));
                continue;
            };

            let started = Instant::now();
            let interval = self.poll_interval_ms.load(Ordering::Relaxed);
            on_event(PollEvent::Fetching);

            match fetch_flights_in_radius(loc.lat, loc.lon, loc.max_radius_nm) {
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

/// Shorten an airport name for the alert card, where the city is already
/// shown on the line above:
/// - drop a trailing "Airport"                    "Oslo Gardermoen Airport" -> "Oslo Gardermoen"
/// - drop the city from the front                 "Oslo Gardermoen" -> "Gardermoen"
/// - nothing left means it just repeats the city  "Edinburgh Airport" -> ""
fn short_airport_name(city: &str, name: &str) -> String {
    let mut short = name.trim();

    if let Some(rest) = strip_suffix_ignore_case(short, "airport") {
        short = rest.trim_end();
    }

    let city = city.trim();
    if !city.is_empty() {
        if short.eq_ignore_ascii_case(city) {
            return String::new();
        }
        if let Some(rest) = strip_prefix_ignore_case(short, city) {
            // Only at a word boundary: "Oslo Gardermoen", not "Oslotown"
            if rest.starts_with(' ') {
                short = rest.trim_start();
            }
        }
    }

    short.to_string()
}

fn strip_suffix_ignore_case<'a>(s: &'a str, suffix: &str) -> Option<&'a str> {
    let cut = s.len().checked_sub(suffix.len())?;
    (s.is_char_boundary(cut) && s[cut..].eq_ignore_ascii_case(suffix)).then(|| &s[..cut])
}

fn strip_prefix_ignore_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let cut = prefix.len();
    (s.len() >= cut && s.is_char_boundary(cut) && s[..cut].eq_ignore_ascii_case(prefix)).then(|| &s[cut..])
}

#[cfg(test)]
mod tests {
    use super::short_airport_name;

    #[test]
    fn drops_trailing_airport() {
        assert_eq!(short_airport_name("Oslo", "Oslo Gardermoen Airport"), "Gardermoen");
        assert_eq!(short_airport_name("Stansted", "London Stansted Airport"), "London Stansted");
    }

    #[test]
    fn drops_leading_city() {
        assert_eq!(short_airport_name("London", "London Heathrow"), "Heathrow");
        assert_eq!(
            short_airport_name("New York", "New York John F Kennedy International Airport"),
            "John F Kennedy International"
        );
    }

    #[test]
    fn empty_when_it_only_repeats_the_city() {
        assert_eq!(short_airport_name("Edinburgh", "Edinburgh Airport"), "");
        assert_eq!(short_airport_name("Paris", "paris"), "");
    }

    #[test]
    fn leaves_other_names_alone() {
        assert_eq!(short_airport_name("Amsterdam", "Schiphol"), "Schiphol");
        assert_eq!(short_airport_name("Oslo", "Oslotown Field"), "Oslotown Field");
        assert_eq!(short_airport_name("", "Airport"), "");
        assert_eq!(short_airport_name("Zürich", "Zürich Airport"), "");
    }
}
