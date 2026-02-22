use crate::flight::hexdb::get_flight_info;
use crate::flight::types::{Flight, FlightInfo, SlimFlight};
use crate::utils::cache::BoundedCache;
use std::collections::{HashMap, HashSet};

const MAX_FLIGHT_INFO_ENTRIES: usize = 50;
const BATCH_SIZE: usize = 3;

/// FlightTracker maintains the state of currently visible flights
/// and resolves route information for newly detected aircraft.
pub struct FlightTracker {
    current_flights: HashMap<String, Flight>,
    new_flights: Vec<String>,
    flight_info: BoundedCache<FlightInfo>,
    last_update: u64,
}

impl FlightTracker {
    pub fn new() -> Self {
        Self {
            current_flights: HashMap::new(),
            new_flights: Vec::new(),
            flight_info: BoundedCache::new(MAX_FLIGHT_INFO_ENTRIES),
            last_update: 0,
        }
    }

    /// Update the tracker with a new set of flights.
    /// Returns hex codes of newly detected flights.
    pub fn update_flights(&mut self, flights: Vec<Flight>) -> Vec<String> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64;

        let previous_ids: HashSet<String> = self.current_flights.keys().cloned().collect();
        let mut new_flight_ids: Vec<String> = Vec::new();

        self.current_flights.clear();

        for flight in flights {
            if !flight.hex.is_empty() {
                let hex = flight.hex.clone();
                if !previous_ids.contains(&hex) {
                    new_flight_ids.push(hex.clone());
                }
                self.current_flights.insert(hex, flight);
            }
        }

        // Fetch flight info for new flights in batches
        if !new_flight_ids.is_empty() {
            for batch in new_flight_ids.chunks(BATCH_SIZE) {
                for hex_code in batch {
                    if let Some(flight) = self.current_flights.get(hex_code) {
                        if let Some(callsign) = flight.callsign() {
                            if !callsign.is_empty() {
                                if let Some(info) = get_flight_info(callsign) {
                                    self.flight_info.insert(hex_code.clone(), info);
                                }
                            }
                        }
                    }
                }
            }
        }

        // Evict old flight info entries
        let current_hex_codes: HashSet<String> = self.current_flights.keys().cloned().collect();
        self.flight_info.evict_keeping(&current_hex_codes);

        self.new_flights = new_flight_ids.clone();
        self.last_update = now;

        new_flight_ids
    }

    /// Get all current flights as SlimFlight for UI display.
    pub fn get_current_flights(&self) -> Vec<SlimFlight> {
        self.current_flights
            .values()
            .filter_map(SlimFlight::from_flight)
            .collect()
    }

    /// Get hex codes of newly detected flights.
    pub fn get_new_flights(&self) -> &[String] {
        &self.new_flights
    }

    /// Clear the new flights list.
    pub fn clear_new_flights(&mut self) {
        self.new_flights.clear();
    }

    /// Get flight info for a specific hex code.
    pub fn get_flight_info(&self, hex_code: &str) -> Option<&FlightInfo> {
        self.flight_info.get(hex_code)
    }

    /// Get the number of currently tracked flights.
    pub fn flight_count(&self) -> usize {
        self.current_flights.len()
    }

    /// Get new flights with their info.
    pub fn get_new_flights_with_info(&self) -> Vec<(String, SlimFlight, Option<&FlightInfo>)> {
        self.new_flights
            .iter()
            .filter_map(|hex| {
                let flight = self.current_flights.get(hex)?;
                let slim = SlimFlight::from_flight(flight)?;
                let info = self.flight_info.get(hex);
                Some((hex.clone(), slim, info))
            })
            .collect()
    }

    /// Get all current flights with their cached info.
    pub fn get_all_flights_with_info(&self) -> Vec<(String, SlimFlight, Option<&FlightInfo>)> {
        self.current_flights
            .values()
            .filter_map(|flight| {
                let slim = SlimFlight::from_flight(flight)?;
                let info = self.flight_info.get(&flight.hex);
                Some((flight.hex.clone(), slim, info))
            })
            .collect()
    }
}
