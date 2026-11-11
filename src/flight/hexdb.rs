use crate::flight::types::{
    AdsbLolAirport, AdsbLolRouteResponse, Airport, FlightInfo, FlightRoute,
};
use crate::utils::cache::BoundedCache;
use std::sync::{Mutex, OnceLock};

const HEXDB_BASE_URL: &str = "https://hexdb.io/api/v1";
const ADSB_LOL_API_URL: &str = "https://api.adsb.lol/api/0/routeset";
const ADSB_LOL_AIRPORT_URL: &str = "https://api.adsb.lol/api/0/airport";

/// adsb.lol airport lookups we've already made. Airports don't change, so
/// this saves requests and keeps proper city names during adsb.lol outages.
const AIRPORT_CACHE_SIZE: usize = 300;

fn airport_cache() -> &'static Mutex<BoundedCache<Airport>> {
    static CACHE: OnceLock<Mutex<BoundedCache<Airport>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(BoundedCache::new(AIRPORT_CACHE_SIZE)))
}

/// Any 2xx counts as success: adsb.lol's routeset has been seen answering
/// 201. (ureq already turns 4xx/5xx into errors.)
fn is_success(status: u16) -> bool {
    (200..300).contains(&status)
}

/// Fetch flight route from HexDB by ICAO callsign.
fn fetch_flight_route(flight_number: &str) -> Option<FlightRoute> {
    let url = format!("{}/route/icao/{}", HEXDB_BASE_URL, flight_number.trim());

    let response = super::http().get(&url).call().ok()?;
    if !is_success(response.status()) {
        return None;
    }
    response.into_json().ok()
}

/// Fetch airport info from HexDB by ICAO code.
fn fetch_airport_info(icao_code: &str) -> Option<Airport> {
    let url = format!("{}/airport/icao/{}", HEXDB_BASE_URL, icao_code.trim());

    let response = super::http().get(&url).call().ok()?;
    if !is_success(response.status()) {
        return None;
    }
    response.into_json().ok()
}

/// Convert adsb.lol airport format to our Airport type.
fn convert_adsb_lol_airport(a: &AdsbLolAirport) -> Airport {
    Airport {
        country_code: a.countryiso2.clone(),
        region_name: None,
        // adsb.lol's "location" is the city the airport serves
        city: a.location.clone(),
        iata: a.iata.clone(),
        icao: a.icao.clone(),
        airport: a.name.clone(),
        latitude: a.lat,
        longitude: a.lon,
    }
}

/// Fetch richer airport data from adsb.lol by ICAO code.
/// Returns location (city) and name (full airport name) that HexDB doesn't provide.
fn fetch_adsb_lol_airport(icao_code: &str) -> Option<Airport> {
    let icao = icao_code.trim().to_uppercase();
    if let Some(cached) = airport_cache().lock().unwrap().get(&icao) {
        return Some(cached.clone());
    }
    let url = format!("{}/{}", ADSB_LOL_AIRPORT_URL, icao);

    let response = super::http().get(&url)
        .set("User-Agent", "flight-clock-app")
        .call()
        .ok()?;
    if !is_success(response.status()) {
        return None;
    }
    let data: AdsbLolAirport = response.into_json().ok()?;
    let airport = convert_adsb_lol_airport(&data);
    airport_cache().lock().unwrap().insert(icao, airport.clone());
    Some(airport)
}

/// Enrich a HexDB airport with richer data from adsb.lol.
/// Falls back to the original airport if adsb.lol fetch fails.
fn enrich_airport(airport: Airport) -> Airport {
    let icao = match &airport.icao {
        Some(code) if !code.is_empty() => code.clone(),
        _ => return airport,
    };

    match fetch_adsb_lol_airport(&icao) {
        Some(enriched) => enriched,
        None => airport,
    }
}

/// Fallback route lookup using adsb.lol API.
fn fetch_route_from_adsb_lol(callsign: &str) -> Option<FlightInfo> {
    let body = serde_json::json!({
        "planes": [{
            "callsign": callsign.trim(),
            "lat": 0,
            "lng": 0
        }]
    });

    let response = super::http().post(ADSB_LOL_API_URL)
        .set("Accept", "application/json")
        .set("Content-Type", "application/json")
        .send_json(&body)
        .ok()?;

    if !is_success(response.status()) {
        return None;
    }

    let data: Vec<AdsbLolRouteResponse> = response.into_json().ok()?;

    let route_data = data.first()?;
    let airports = route_data.airports.as_ref()?;

    if airports.len() < 2 {
        return None;
    }

    Some(FlightInfo {
        flight: callsign.trim().to_string(),
        origin: Some(convert_adsb_lol_airport(&airports[0])),
        destination: Some(convert_adsb_lol_airport(&airports[1])),
        route: route_data.airport_codes.clone(),
    })
}

/// Get flight info (route + airports) for a given callsign.
/// Tries HexDB first, falls back to adsb.lol.
pub fn get_flight_info(flight_number: &str) -> Option<FlightInfo> {
    let clean = flight_number.trim();
    if clean.is_empty() {
        return None;
    }

    // Try HexDB first
    let route_info = fetch_flight_route(clean);

    match route_info {
        Some(ri) if ri.route.is_some() => {
            let route = ri.route.unwrap();
            let parts: Vec<&str> = route.split('-').collect();

            if parts.len() < 2 {
                return Some(FlightInfo {
                    flight: clean.to_string(),
                    origin: None,
                    destination: None,
                    route: Some(route),
                });
            }

            let origin = fetch_airport_info(parts[0]).map(enrich_airport);
            let destination = fetch_airport_info(parts[1]).map(enrich_airport);

            Some(FlightInfo {
                flight: clean.to_string(),
                origin,
                destination,
                route: Some(route),
            })
        }
        _ => {
            // Fallback to adsb.lol
            let fallback = fetch_route_from_adsb_lol(clean);
            if fallback.is_some() {
                return fallback;
            }

            Some(FlightInfo {
                flight: clean.to_string(),
                origin: None,
                destination: None,
                route: None,
            })
        }
    }
}
