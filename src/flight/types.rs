use serde::{Deserialize, Serialize};

/// Raw aircraft data from ADS-B API response
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Flight {
    pub hex: String,
    #[serde(rename = "type")]
    pub msg_type: Option<String>,
    pub flight: Option<String>,
    pub r: Option<String>,
    pub t: Option<String>,
    pub desc: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub alt_baro: Option<AltBaro>,
    pub alt_geom: Option<i32>,
    pub gs: Option<f64>,
    pub track: Option<f64>,
    pub baro_rate: Option<i32>,
    pub geom_rate: Option<i32>,
    pub squawk: Option<String>,
    pub emergency: Option<String>,
    pub category: Option<String>,
    pub seen: Option<f64>,
    pub seen_pos: Option<f64>,
    pub dst: Option<f64>,
    pub dir: Option<f64>,
}

/// alt_baro can be a number or the string "ground"
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AltBaro {
    Altitude(i32),
    Ground(String),
}

impl AltBaro {
    pub fn as_feet(&self) -> Option<i32> {
        match self {
            AltBaro::Altitude(alt) => Some(*alt),
            AltBaro::Ground(_) => None,
        }
    }
}

impl Flight {
    /// Returns trimmed callsign if present
    pub fn callsign(&self) -> Option<&str> {
        self.flight.as_deref().map(|s| s.trim())
    }

    /// Checks if this flight has valid position data
    pub fn is_valid(&self) -> bool {
        if self.hex.is_empty() {
            return false;
        }
        match (self.lat, self.lon) {
            (Some(lat), Some(lon)) => lat.abs() <= 90.0 && lon.abs() <= 180.0,
            _ => false,
        }
    }
}

/// ADS-B API response envelope
#[derive(Debug, Clone, Deserialize)]
pub struct ADSBResponse {
    pub now: Option<f64>,
    #[serde(rename = "ac", alias = "aircraft")]
    pub aircraft: Option<Vec<Flight>>,
    #[serde(rename = "resultCount")]
    pub result_count: Option<u32>,
}

/// Route info from HexDB
#[derive(Debug, Clone, Deserialize)]
pub struct FlightRoute {
    pub route: Option<String>,
}

/// Airport info
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Airport {
    pub country_code: Option<String>,
    pub region_name: Option<String>,
    pub iata: Option<String>,
    pub icao: Option<String>,
    pub airport: Option<String>,
    pub latitude: Option<f64>,
    pub longitude: Option<f64>,
}

impl Airport {
    pub fn display_name(&self) -> &str {
        self.airport
            .as_deref()
            .or(self.iata.as_deref())
            .or(self.icao.as_deref())
            .unwrap_or("Unknown")
    }

    pub fn location(&self) -> &str {
        self.region_name
            .as_deref()
            .or(self.country_code.as_deref())
            .unwrap_or("Unknown")
    }
}

/// Resolved flight info with origin/destination
#[derive(Debug, Clone)]
pub struct FlightInfo {
    pub flight: String,
    pub origin: Option<Airport>,
    pub destination: Option<Airport>,
    pub route: Option<String>,
}

/// adsb.lol API types (fallback route lookup)
#[derive(Debug, Clone, Deserialize)]
pub struct AdsbLolAirport {
    pub countryiso2: Option<String>,
    pub iata: Option<String>,
    pub icao: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub location: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AdsbLolRouteResponse {
    pub airport_codes: Option<String>,
    #[serde(rename = "_airports")]
    pub airports: Option<Vec<AdsbLolAirport>>,
    pub callsign: Option<String>,
}

/// Slim flight data sent to the UI (minimal fields)
#[derive(Debug, Clone)]
pub struct SlimFlight {
    pub hex: String,
    pub callsign: String,
    pub lat: f64,
    pub lon: f64,
    pub alt_baro: Option<i32>,
    pub gs: Option<f64>,
    pub track: Option<f64>,
    pub baro_rate: Option<i32>,
    pub aircraft_type: Option<String>,
}

impl SlimFlight {
    pub fn from_flight(f: &Flight) -> Option<Self> {
        Some(Self {
            hex: f.hex.clone(),
            callsign: f.callsign().unwrap_or("").to_string(),
            lat: f.lat?,
            lon: f.lon?,
            alt_baro: f.alt_baro.as_ref().and_then(|a| a.as_feet()),
            gs: f.gs,
            track: f.track,
            baro_rate: f.baro_rate,
            aircraft_type: f.t.clone().or(f.msg_type.clone()),
        })
    }
}
