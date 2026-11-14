use crate::flight::types::{ADSBResponse, Flight};

const ADSB_BASE_URL: &str = "https://opendata.adsb.fi/api/v2";

/// Fetch flights within a radius of the given coordinates from opendata.adsb.fi.
pub fn fetch_flights_in_radius(
    latitude: f64,
    longitude: f64,
    radius_nm: f64,
) -> Result<Vec<Flight>, String> {
    // Dev aid: FLIGHT_CLOCK_ADSB_URL points the clock at another server that
    // answers the same way, e.g. a local one serving made-up aircraft
    // (scripts/demo-feed.py) to see how the radar draws them.
    let base = std::env::var("FLIGHT_CLOCK_ADSB_URL").unwrap_or_else(|_| ADSB_BASE_URL.to_string());
    let url = format!("{}/lat/{}/lon/{}/dist/{}", base.trim_end_matches('/'), latitude, longitude, radius_nm);

    let response = super::http()
        .get(&url)
        .set("Accept", "application/json")
        .call()
        .map_err(|e| {
            eprintln!("ADS-B request failed: {}", e);
            describe_error(&e)
        })?;

    let data: ADSBResponse = response.into_json().map_err(|e| {
        eprintln!("ADS-B response unreadable: {}", e);
        "Unexpected reply from flight data service".to_string()
    })?;

    Ok(data.aircraft.unwrap_or_default())
}

/// Is this an aircraft's ICAO address ("4840d6"): six hex digits? Addresses
/// the feed has made up for aircraft without one start with "~" and can't be
/// looked up. The code goes into a request URL, so nothing else is accepted.
pub fn is_icao_hex(hex: &str) -> bool {
    hex.len() == 6 && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Fetch one aircraft by its ICAO address, wherever it is. `Ok(None)` when
/// the service isn't hearing it (out of coverage, landed and switched off)
/// or the address can't be looked up.
pub fn fetch_flight_by_hex(hex: &str) -> Result<Option<Flight>, String> {
    if !is_icao_hex(hex) {
        return Ok(None);
    }
    let base = std::env::var("FLIGHT_CLOCK_ADSB_URL").unwrap_or_else(|_| ADSB_BASE_URL.to_string());
    let url = format!("{}/hex/{}", base.trim_end_matches('/'), hex.to_ascii_lowercase());

    let response = super::http()
        .get(&url)
        .set("Accept", "application/json")
        .call()
        .map_err(|e| describe_error(&e))?;
    let data: ADSBResponse = response
        .into_json()
        .map_err(|_| "Unexpected reply from flight data service".to_string())?;

    Ok(data
        .aircraft
        .unwrap_or_default()
        .into_iter()
        .find(|f| f.hex.eq_ignore_ascii_case(hex) && f.is_valid()))
}

/// Short message for the radar screen.
fn describe_error(e: &ureq::Error) -> String {
    match e {
        ureq::Error::Status(429, _) => "Flight data rate-limited, retrying".to_string(),
        ureq::Error::Status(code, _) => format!("Flight data service error ({})", code),
        ureq::Error::Transport(t) => match t.kind() {
            ureq::ErrorKind::Dns | ureq::ErrorKind::ConnectionFailed => "No internet connection".to_string(),
            ureq::ErrorKind::Io => "Flight data request timed out".to_string(),
            _ => "Can't reach flight data service".to_string(),
        },
    }
}

/// Filter flights that have valid position data.
pub fn filter_valid_flights(flights: Vec<Flight>) -> Vec<Flight> {
    flights.into_iter().filter(|f| f.is_valid()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_real_icao_addresses_are_looked_up() {
        assert!(is_icao_hex("4840d6"));
        assert!(is_icao_hex("A0B1C2"));
        for bad in ["", "4840d", "4840d6a", "~4840d", "48 0d6", "4840dg", "../../x", "4840d6?x=1"] {
            assert!(!is_icao_hex(bad), "{:?}", bad);
        }
        // Not looked up at all, so no request is made
        assert_eq!(fetch_flight_by_hex("~abc12").map(|f| f.is_some()), Ok(false));
    }
}
