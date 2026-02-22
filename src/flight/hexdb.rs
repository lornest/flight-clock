use crate::flight::types::{
    AdsbLolAirport, AdsbLolRouteResponse, Airport, FlightInfo, FlightRoute,
};

const HEXDB_BASE_URL: &str = "https://hexdb.io/api/v1";
const ADSB_LOL_API_URL: &str = "https://api.adsb.lol/api/0/routeset";

/// Fetch flight route from HexDB by ICAO callsign.
fn fetch_flight_route(flight_number: &str) -> Option<FlightRoute> {
    let url = format!("{}/route/icao/{}", HEXDB_BASE_URL, flight_number.trim());

    let response = ureq::get(&url).call().ok()?;
    if response.status() != 200 {
        return None;
    }
    response.into_json().ok()
}

/// Fetch airport info from HexDB by ICAO code.
fn fetch_airport_info(icao_code: &str) -> Option<Airport> {
    let url = format!("{}/airport/icao/{}", HEXDB_BASE_URL, icao_code.trim());

    let response = ureq::get(&url).call().ok()?;
    if response.status() != 200 {
        return None;
    }
    response.into_json().ok()
}

/// Convert adsb.lol airport format to our Airport type.
fn convert_adsb_lol_airport(a: &AdsbLolAirport) -> Airport {
    Airport {
        country_code: a.countryiso2.clone(),
        region_name: a.location.clone(),
        iata: a.iata.clone(),
        icao: a.icao.clone(),
        airport: a.name.clone(),
        latitude: a.lat,
        longitude: a.lon,
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

    let response = ureq::post(ADSB_LOL_API_URL)
        .set("Accept", "application/json")
        .set("Content-Type", "application/json")
        .send_json(&body)
        .ok()?;

    if response.status() != 200 {
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

            let origin = fetch_airport_info(parts[0]);
            let destination = fetch_airport_info(parts[1]);

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
