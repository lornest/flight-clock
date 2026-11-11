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
    /// Region from HexDB, e.g. "Scotland". Not a city: never shown as one.
    pub region_name: Option<String>,
    /// City, e.g. "Edinburgh". Only adsb.lol provides this.
    #[serde(default)]
    pub city: Option<String>,
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

    /// The place to show for this airport: the city when we know it, else
    /// the airport's name without "Airport" ("Edinburgh Airport" ->
    /// "Edinburgh"). Never the region: HexDB's region is e.g. "Scotland".
    pub fn location(&self) -> String {
        if let Some(city) = self.city.as_deref().map(str::trim).filter(|c| !c.is_empty()) {
            return city.to_string();
        }
        if let Some(name) = self.airport.as_deref().map(str::trim).filter(|n| !n.is_empty()) {
            let lower = name.to_ascii_lowercase();
            let trimmed = if lower.ends_with(" airport") { &name[..name.len() - " airport".len()] } else { name };
            return trimmed.trim().to_string();
        }
        self.iata
            .as_deref()
            .or(self.icao.as_deref())
            .unwrap_or("Unknown")
            .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::Airport;

    fn airport(city: Option<&str>, region: Option<&str>, name: Option<&str>) -> Airport {
        Airport {
            country_code: Some("GB".into()),
            region_name: region.map(Into::into),
            city: city.map(Into::into),
            iata: Some("EDI".into()),
            icao: Some("EGPH".into()),
            airport: name.map(Into::into),
            latitude: None,
            longitude: None,
        }
    }

    #[test]
    fn location_prefers_city() {
        let a = airport(Some("Edinburgh"), Some("Scotland"), Some("Edinburgh Airport"));
        assert_eq!(a.location(), "Edinburgh");
    }

    #[test]
    fn location_never_uses_region() {
        // HexDB-only data (adsb.lol unavailable)
        let a = airport(None, Some("Scotland"), Some("Edinburgh Airport"));
        assert_eq!(a.location(), "Edinburgh");
        let a = airport(None, Some("England"), Some("London Heathrow Airport"));
        assert_eq!(a.location(), "London Heathrow");
    }

    #[test]
    fn location_falls_back_to_codes() {
        assert_eq!(airport(None, Some("Scotland"), None).location(), "EDI");
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
