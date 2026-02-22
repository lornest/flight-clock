use std::f64::consts::PI;

/// Earth's radius in nautical miles
const EARTH_RADIUS_NM: f64 = 3440.065;

/// Calculate the bearing from one point to another in degrees (0-360).
/// 0 = North, 90 = East, 180 = South, 270 = West
pub fn calculate_bearing(from_lat: f64, from_lon: f64, to_lat: f64, to_lon: f64) -> f64 {
    let phi1 = from_lat.to_radians();
    let phi2 = to_lat.to_radians();
    let delta_lambda = (to_lon - from_lon).to_radians();

    let y = delta_lambda.sin() * phi2.cos();
    let x = phi1.cos() * phi2.sin() - phi1.sin() * phi2.cos() * delta_lambda.cos();

    let theta = y.atan2(x);
    (theta.to_degrees() + 360.0) % 360.0
}

/// Calculate the distance between two coordinates in nautical miles.
/// Uses the Haversine formula.
pub fn calculate_distance(from_lat: f64, from_lon: f64, to_lat: f64, to_lon: f64) -> f64 {
    let phi1 = from_lat.to_radians();
    let phi2 = to_lat.to_radians();
    let delta_phi = (to_lat - from_lat).to_radians();
    let delta_lambda = (to_lon - from_lon).to_radians();

    let a = (delta_phi / 2.0).sin().powi(2)
        + phi1.cos() * phi2.cos() * (delta_lambda / 2.0).sin().powi(2);
    let c = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());

    EARTH_RADIUS_NM * c
}

/// Convert compass direction string to degrees.
/// N = 0, NE = 45, E = 90, SE = 135, S = 180, SW = 225, W = 270, NW = 315
pub fn direction_to_degrees(direction: &str) -> f64 {
    match direction.to_uppercase().as_str() {
        "N" => 0.0,
        "NE" => 45.0,
        "E" => 90.0,
        "SE" => 135.0,
        "S" => 180.0,
        "SW" => 225.0,
        "W" => 270.0,
        "NW" => 315.0,
        _ => 0.0,
    }
}

/// Calculate the rotation angle for positioning a plane icon on screen.
///
/// Returns rotation in degrees where:
/// - 0 = pointing up (12 o'clock)
/// - 90 = pointing right (3 o'clock)
/// - 180 = pointing down (6 o'clock)
/// - 270 = pointing left (9 o'clock)
pub fn calculate_plane_rotation(
    user_lat: f64,
    user_lon: f64,
    plane_lat: f64,
    plane_lon: f64,
    user_facing_direction: &str,
) -> f64 {
    let bearing_to_plane = calculate_bearing(user_lat, user_lon, plane_lat, plane_lon);
    let user_facing_degrees = direction_to_degrees(user_facing_direction);
    let mut relative_bearing = bearing_to_plane - user_facing_degrees;
    if relative_bearing < 0.0 {
        relative_bearing += 360.0;
    }
    relative_bearing
}

/// Convert a polar coordinate (rotation degrees, distance ratio) to screen X/Y
/// relative to center. Uses the same math as the original FlightRadar/FlightAlert.
///
/// - `rotation`: angle in degrees (0 = up/north on screen)
/// - `screen_radius`: distance from center in pixels
///
/// Returns (x, y) offset from center.
pub fn polar_to_screen(rotation: f64, screen_radius: f64) -> (f64, f64) {
    let angle_rad = (rotation - 90.0) * PI / 180.0;
    let x = angle_rad.cos() * screen_radius;
    let y = angle_rad.sin() * screen_radius;
    (x, y)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_direction_to_degrees() {
        assert_eq!(direction_to_degrees("N"), 0.0);
        assert_eq!(direction_to_degrees("E"), 90.0);
        assert_eq!(direction_to_degrees("S"), 180.0);
        assert_eq!(direction_to_degrees("W"), 270.0);
        assert_eq!(direction_to_degrees("NE"), 45.0);
        assert_eq!(direction_to_degrees("sw"), 225.0); // case insensitive
    }

    #[test]
    fn test_calculate_bearing_north() {
        // Point directly north should give ~0 degrees
        let bearing = calculate_bearing(55.0, -3.0, 56.0, -3.0);
        assert!((bearing - 0.0).abs() < 1.0, "Expected ~0, got {}", bearing);
    }

    #[test]
    fn test_calculate_bearing_east() {
        // Point directly east should give ~90 degrees
        let bearing = calculate_bearing(55.0, -3.0, 55.0, -2.0);
        assert!(
            (bearing - 90.0).abs() < 5.0,
            "Expected ~90, got {}",
            bearing
        );
    }

    #[test]
    fn test_calculate_distance_zero() {
        let dist = calculate_distance(55.0, -3.0, 55.0, -3.0);
        assert!(dist.abs() < 0.001, "Expected 0, got {}", dist);
    }

    #[test]
    fn test_calculate_distance_known() {
        // Edinburgh to London is approximately 280 nautical miles
        let dist = calculate_distance(55.95, -3.19, 51.47, -0.45);
        assert!(
            (dist - 280.0).abs() < 20.0,
            "Expected ~280nm, got {}",
            dist
        );
    }

    #[test]
    fn test_polar_to_screen() {
        // Rotation 0 (north/up) at radius 100 should give (0, -100)
        let (x, y) = polar_to_screen(0.0, 100.0);
        assert!(x.abs() < 0.01, "Expected x~0, got {}", x);
        assert!((y + 100.0).abs() < 0.01, "Expected y~-100, got {}", y);
    }
}
