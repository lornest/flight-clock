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
