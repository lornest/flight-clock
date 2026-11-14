use crate::config::Location;
use crate::flight::adsb::{fetch_flight_by_hex, fetch_flights_in_radius, filter_valid_flights};
use crate::flight::hexdb::get_flight_info;
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
    /// News of the aircraft whose details are on screen. `flight` is None
    /// when it couldn't be found this time (out of coverage, or the request
    /// failed).
    Followed {
        hex: String,
        flight: Option<(SlimFlight, NewFlightWithInfo)>,
    },
}

/// While an aircraft's details are open: how often to ask about it, and how
/// often to refresh the area once it has flown out of it (the radar isn't on
/// screen, but new arrivals still need noticing so they don't all alert at
/// once afterwards).
const FOLLOW_EVERY_MS: u64 = 5000;
const AREA_WHILE_FOLLOWING: Duration = Duration::from_secs(30);
/// The flight data service asks for no more than one request a second
const MIN_BETWEEN_POLLS: Duration = Duration::from_secs(1);

/// The aircraft being followed, across polls.
struct Followed {
    hex: String,
    /// Its route, kept from when it was in the area or looked up once since
    info: Option<FlightInfo>,
    route_looked_up: bool,
    /// In the last area reply: the area poll covers it, with no extra requests
    in_area: bool,
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
    /// ICAO address of the aircraft whose details are on screen, if any.
    /// It is followed even after it leaves the area.
    follow: Arc<Mutex<Option<String>>>,
    /// The timings above, as fields so tests can shorten them
    follow_every: Duration,
    area_while_following: Duration,
    min_between_polls: Duration,
}

impl Poller {
    pub fn new(
        location: Arc<Mutex<Option<Location>>>,
        poll_interval_ms: Arc<AtomicU64>,
        network_ready: Arc<AtomicBool>,
        follow: Arc<Mutex<Option<String>>>,
    ) -> Self {
        Self {
            location,
            poll_interval_ms,
            network_ready,
            follow,
            follow_every: Duration::from_millis(FOLLOW_EVERY_MS),
            area_while_following: AREA_WHILE_FOLLOWING,
            min_between_polls: MIN_BETWEEN_POLLS,
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
        let mut followed: Option<Followed> = None;
        let mut last_area_poll: Option<Instant> = None;

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

            // Start or stop following as the details screen opens and closes
            let follow_hex = self.follow.lock().unwrap().clone();
            if followed.as_ref().map(|f| &f.hex) != follow_hex.as_ref() {
                followed = follow_hex.clone().map(|hex| Followed {
                    // It was tapped on the radar, so it starts in the area
                    in_area: tracker.has_flight(&hex),
                    hex,
                    info: None,
                    route_looked_up: false,
                });
            }

            // The area: every time normally, and while the followed aircraft
            // is still in it. Once it has left, only every so often.
            let area_due = match &followed {
                Some(f) if !f.in_area => last_area_poll.is_none_or(|t| t.elapsed() >= self.area_while_following),
                _ => true,
            };
            if !area_due {
                if let Some(f) = followed.as_mut() {
                    on_event(Self::follow_outside_area(f));
                }
                self.wait_for_next_poll(started, follow_hex.as_deref());
                continue;
            }

            last_area_poll = Some(started);
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

                    if let Some(f) = followed.as_mut() {
                        match tracker.get_current_flights().into_iter().find(|s| s.hex == f.hex) {
                            Some(slim) => {
                                // Still in the area: this reply covers it
                                f.in_area = true;
                                if let Some(info) = tracker.get_flight_info(&f.hex) {
                                    f.info = Some(info.clone());
                                }
                                let with_info = NewFlightWithInfo::from_parts(&f.hex, &slim, f.info.as_ref());
                                on_event(PollEvent::Followed { hex: f.hex.clone(), flight: Some((slim, with_info)) });
                            }
                            None => {
                                f.in_area = false;
                                on_event(Self::follow_outside_area(f));
                            }
                        }
                    }
                }
                Err(e) => {
                    on_event(PollEvent::Failed(e));
                    if let Some(f) = followed.as_ref() {
                        on_event(PollEvent::Followed { hex: f.hex.clone(), flight: None });
                    }
                }
            }

            self.wait_for_next_poll(started, follow_hex.as_deref());
        }
    }

    /// Ask the flight data service about the followed aircraft by its
    /// address, now that the area reply no longer includes it.
    fn follow_outside_area(followed: &mut Followed) -> PollEvent {
        let flight = fetch_flight_by_hex(&followed.hex)
            .unwrap_or(None)
            .and_then(|f| SlimFlight::from_flight(&f));
        let flight = flight.map(|slim| {
            // The route, if it never arrived while the aircraft was in the area
            if followed.info.is_none() && !followed.route_looked_up && !slim.callsign.is_empty() {
                followed.route_looked_up = true;
                followed.info = get_flight_info(&slim.callsign);
            }
            let with_info = NewFlightWithInfo::from_parts(&followed.hex, &slim, followed.info.as_ref());
            (slim, with_info)
        });
        PollEvent::Followed { hex: followed.hex.clone(), flight }
    }

    /// Sleep until the poll interval has passed since `started`. Re-reads the
    /// interval while waiting, so switching to the radar or starting an alert
    /// (30s -> 5s) triggers the next fetch straight away instead of after the
    /// rest of the old 30s wait. Returns at once when the details screen
    /// opens, closes or moves to another aircraft (`following` is what it was
    /// showing when this poll began), so that shows fresh data without delay.
    fn wait_for_next_poll(&self, started: Instant, following: Option<&str>) {
        loop {
            // ...but never two requests within a second of each other,
            // however fast the screen is opened and closed
            let changed = self.follow.lock().unwrap().as_deref() != following;
            if changed && started.elapsed() >= self.min_between_polls {
                return;
            }
            let interval = match following {
                Some(_) => self.follow_every,
                None => Duration::from_millis(self.poll_interval_ms.load(Ordering::Relaxed)),
            };
            let elapsed = started.elapsed();
            if elapsed >= interval {
                return;
            }
            let step = (interval - elapsed).min(Duration::from_millis(50));
            std::thread::sleep(step);
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
mod follow_tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::mpsc;

    /// A stand-in for the flight data service: answers area requests with
    /// whatever `in_area` holds and single-aircraft requests with `anywhere`,
    /// and records every path asked for.
    struct FakeFeed {
        in_area: Arc<Mutex<Vec<String>>>,
        anywhere: Arc<Mutex<Vec<String>>>,
        asked: Arc<Mutex<Vec<String>>>,
        url: String,
    }

    fn aircraft(hex: &str) -> String {
        // No callsign, so no route lookups go out to real services
        format!(r#"{{"hex":"{}","lat":51.5,"lon":-0.4,"alt_baro":9000,"gs":250,"track":90,"category":"A3"}}"#, hex)
    }

    fn fake_feed() -> FakeFeed {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let list = || Arc::new(Mutex::new(Vec::<String>::new()));
        let (in_area, anywhere, asked) = (list(), list(), list());
        let (a, b, c) = (in_area.clone(), anywhere.clone(), asked.clone());
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let mut request_line = String::new();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                if reader.read_line(&mut request_line).is_err() {
                    continue;
                }
                // Drain the headers
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 2) {
                    line.clear();
                }
                let path = request_line.split_whitespace().nth(1).unwrap_or("").to_string();
                c.lock().unwrap().push(path.clone());
                let list = if path.starts_with("/hex/") {
                    let hex = path.trim_start_matches("/hex/");
                    b.lock().unwrap().iter().filter(|h| h.as_str() == hex).map(|h| aircraft(h)).collect::<Vec<_>>()
                } else {
                    a.lock().unwrap().iter().map(|h| aircraft(h)).collect::<Vec<_>>()
                };
                let body = format!(r#"{{"ac":[{}]}}"#, list.join(","));
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
            }
        });
        FakeFeed { in_area, anywhere, asked, url }
    }

    /// Wait for the next event about the followed aircraft.
    fn next_followed(events: &mpsc::Receiver<(String, bool)>) -> (String, bool) {
        events.recv_timeout(Duration::from_secs(10)).expect("no news of the followed aircraft")
    }

    #[test]
    fn keeps_following_an_aircraft_after_it_leaves_the_area() {
        let feed = fake_feed();
        // The only test that sets this; every request in this test goes to the fake
        std::env::set_var("FLIGHT_CLOCK_ADSB_URL", &feed.url);
        *feed.in_area.lock().unwrap() = vec!["4840d6".into(), "abcdef".into()];
        *feed.anywhere.lock().unwrap() = vec!["4840d6".into()];

        let follow = Arc::new(Mutex::new(None::<String>));
        let location = Arc::new(Mutex::new(Some(Location { lat: 51.47, lon: -0.45, max_radius_nm: 6.0 })));
        let mut poller = Poller::new(location, Arc::new(AtomicU64::new(100)), Arc::new(AtomicBool::new(true)), follow.clone());
        poller.follow_every = Duration::from_millis(100);
        poller.area_while_following = Duration::from_millis(600);
        poller.min_between_polls = Duration::from_millis(20);

        let (tx, events) = mpsc::channel();
        let tx = Mutex::new(tx);
        std::thread::spawn(move || {
            poller.run(move |event| {
                if let PollEvent::Followed { hex, flight } = event {
                    let _ = tx.lock().unwrap().send((hex, flight.is_some()));
                }
            })
        });

        // Opened on the radar while it's in the area: the area reply covers
        // it, and no single-aircraft requests are made
        std::thread::sleep(Duration::from_millis(300));
        *follow.lock().unwrap() = Some("4840d6".into());
        assert_eq!(next_followed(&events), ("4840d6".into(), true));
        assert!(feed.asked.lock().unwrap().iter().all(|p| !p.starts_with("/hex/")));

        // It flies out of the area: followed by its address instead
        *feed.in_area.lock().unwrap() = vec!["abcdef".into()];
        for _ in 0..5 {
            assert_eq!(next_followed(&events), ("4840d6".into(), true));
        }
        let asked = feed.asked.lock().unwrap().clone();
        let lookups = asked.iter().filter(|p| *p == "/hex/4840d6").count();
        assert!(lookups >= 3, "{:?}", asked);
        // ...while the area is only asked about now and then
        let first_lookup = asked.iter().position(|p| p.starts_with("/hex/")).unwrap();
        let area_after = asked[first_lookup..].iter().filter(|p| p.starts_with("/lat/")).count();
        assert!(area_after < lookups, "area {} vs lookups {}: {:?}", area_after, lookups, asked);

        // The service stops hearing it: news arrives, with nothing in it
        feed.anywhere.lock().unwrap().clear();
        let mut lost = false;
        for _ in 0..5 {
            if !next_followed(&events).1 {
                lost = true;
                break;
            }
        }
        assert!(lost, "never reported as lost");

        // Closed: no more single-aircraft requests
        *follow.lock().unwrap() = None;
        std::thread::sleep(Duration::from_millis(400));
        let before = feed.asked.lock().unwrap().iter().filter(|p| p.starts_with("/hex/")).count();
        std::thread::sleep(Duration::from_millis(400));
        let after = feed.asked.lock().unwrap().iter().filter(|p| p.starts_with("/hex/")).count();
        assert_eq!(before, after);
        while events.try_recv().is_ok() {}
    }
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
