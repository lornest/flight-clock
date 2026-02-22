use crate::flight::types::{ADSBResponse, Flight};

const ADSB_BASE_URL: &str = "https://opendata.adsb.fi/api/v2";

/// Fetch flights within a radius of the given coordinates from opendata.adsb.fi.
pub fn fetch_flights_in_radius(
    latitude: f64,
    longitude: f64,
    radius_nm: f64,
) -> Result<Vec<Flight>, String> {
    let url = format!(
        "{}/lat/{}/lon/{}/dist/{}",
        ADSB_BASE_URL, latitude, longitude, radius_nm
    );

    let response = ureq::get(&url)
        .set("Accept", "application/json")
        .call()
        .map_err(|e| format!("ADS-B API request failed: {}", e))?;

    let data: ADSBResponse = response
        .into_json()
        .map_err(|e| format!("Failed to parse ADS-B response: {}", e))?;

    Ok(data.aircraft.unwrap_or_default())
}

/// Filter flights that have valid position data.
pub fn filter_valid_flights(flights: Vec<Flight>) -> Vec<Flight> {
    flights.into_iter().filter(|f| f.is_valid()).collect()
}
