//! SVG path data for shapes that have to be rotated at runtime.
//!
//! The software renderer can't rotate elements, so rotated outlines are
//! produced here as path strings for Slint `Path { commands: ... }`.

use crate::flight::types::AircraftKind;

/// An outline as path commands with their points.
type Outline = &'static [(char, &'static [(f64, f64)])];

/// Plane outline from the brand design, nose up, centred on (0, 0),
/// about 26 units across.
const PLANE: Outline = &[
    ('M', &[(0.0, -13.0)]),
    ('C', &[(1.6, -13.0), (2.2, -10.0), (2.2, -7.0)]),
    ('L', &[(2.2, -2.0)]),
    ('L', &[(13.0, 4.0)]),
    ('L', &[(13.0, 7.0)]),
    ('L', &[(2.2, 3.5)]),
    ('L', &[(2.2, 9.0)]),
    ('L', &[(5.5, 11.5)]),
    ('L', &[(5.5, 13.5)]),
    ('L', &[(0.0, 12.0)]),
    ('L', &[(-5.5, 13.5)]),
    ('L', &[(-5.5, 11.5)]),
    ('L', &[(-2.2, 9.0)]),
    ('L', &[(-2.2, 3.5)]),
    ('L', &[(-13.0, 7.0)]),
    ('L', &[(-13.0, 4.0)]),
    ('L', &[(-2.2, -2.0)]),
    ('L', &[(-2.2, -7.0)]),
    ('C', &[(-2.2, -10.0), (-1.6, -13.0), (0.0, -13.0)]),
    ('Z', &[]),
];

/// The plane centred at (cx, cy), nose pointing `heading_deg` clockwise
/// from up, in display coordinates.
pub fn plane(cx: f64, cy: f64, heading_deg: f64) -> String {
    plane_scaled(cx, cy, heading_deg, 1.0)
}

/// As `plane`, scaled (1.0 = 26 units across).
pub fn plane_scaled(cx: f64, cy: f64, heading_deg: f64, scale: f64) -> String {
    outline(PLANE, cx, cy, heading_deg, scale)
}

// Radar and alert icons, one per `AircraftKind`. All nose up, centred on
// (0, 0). Sizes step up with the aircraft: drawn at `ICON_SCALE`, about 20
// display pixels across for a light aircraft to 36 for a wide-body.

/// How much larger than their outlines below the icons are drawn. One number
/// to make every icon bigger or smaller together.
pub const ICON_SCALE: f64 = 1.25;

/// Light aircraft: straight high wing, short nose.
const LIGHT: Outline = &[
    ('M', &[(0.0, -7.0)]),
    ('L', &[(1.2, -5.5)]),
    ('L', &[(1.2, -3.0)]),
    ('L', &[(8.0, -3.0)]),
    ('L', &[(8.0, -0.6)]),
    ('L', &[(1.2, -0.6)]),
    ('L', &[(0.9, 5.0)]),
    ('L', &[(3.2, 5.0)]),
    ('L', &[(3.2, 6.6)]),
    ('L', &[(0.0, 6.2)]),
    ('L', &[(-3.2, 6.6)]),
    ('L', &[(-3.2, 5.0)]),
    ('L', &[(-0.9, 5.0)]),
    ('L', &[(-1.2, -0.6)]),
    ('L', &[(-8.0, -0.6)]),
    ('L', &[(-8.0, -3.0)]),
    ('L', &[(-1.2, -3.0)]),
    ('L', &[(-1.2, -5.5)]),
    ('Z', &[]),
];

/// Business jet or turboprop: slim, wings swept and set back.
const SMALL: Outline = &[
    ('M', &[(0.0, -9.0)]),
    ('C', &[(1.0, -9.0), (1.5, -7.0), (1.5, -5.0)]),
    ('L', &[(1.5, -1.0)]),
    ('L', &[(8.5, 3.5)]),
    ('L', &[(8.5, 5.2)]),
    ('L', &[(1.5, 2.5)]),
    ('L', &[(1.5, 6.5)]),
    ('L', &[(4.0, 8.3)]),
    ('L', &[(4.0, 9.6)]),
    ('L', &[(0.0, 8.6)]),
    ('L', &[(-4.0, 9.6)]),
    ('L', &[(-4.0, 8.3)]),
    ('L', &[(-1.5, 6.5)]),
    ('L', &[(-1.5, 2.5)]),
    ('L', &[(-8.5, 5.2)]),
    ('L', &[(-8.5, 3.5)]),
    ('L', &[(-1.5, -1.0)]),
    ('L', &[(-1.5, -5.0)]),
    ('C', &[(-1.5, -7.0), (-1.0, -9.0), (0.0, -9.0)]),
    ('Z', &[]),
];

/// Single-aisle airliner: the brand plane, at this scale.
const LARGE_SCALE: f64 = 0.8;

/// Wide-body: longer, with a fatter fuselage and a broader wing.
const HEAVY: Outline = &[
    ('M', &[(0.0, -14.5)]),
    ('C', &[(2.2, -14.5), (3.0, -11.0), (3.0, -8.0)]),
    ('L', &[(3.0, -3.0)]),
    ('L', &[(14.5, 4.0)]),
    ('L', &[(14.5, 7.5)]),
    ('L', &[(3.0, 3.5)]),
    ('L', &[(3.0, 9.5)]),
    ('L', &[(6.5, 12.5)]),
    ('L', &[(6.5, 14.8)]),
    ('L', &[(0.0, 13.0)]),
    ('L', &[(-6.5, 14.8)]),
    ('L', &[(-6.5, 12.5)]),
    ('L', &[(-3.0, 9.5)]),
    ('L', &[(-3.0, 3.5)]),
    ('L', &[(-14.5, 7.5)]),
    ('L', &[(-14.5, 4.0)]),
    ('L', &[(-3.0, -3.0)]),
    ('L', &[(-3.0, -8.0)]),
    ('C', &[(-3.0, -11.0), (-2.2, -14.5), (0.0, -14.5)]),
    ('Z', &[]),
];

/// Helicopter: round cabin, tail boom with a tail rotor, and two crossed
/// rotor blades.
const ROTOR: Outline = &[
    // Cabin
    ('M', &[(0.0, -5.0)]),
    ('L', &[(2.8, -3.8)]),
    ('L', &[(4.0, -1.0)]),
    ('L', &[(2.8, 1.8)]),
    ('L', &[(0.0, 3.0)]),
    ('L', &[(-2.8, 1.8)]),
    ('L', &[(-4.0, -1.0)]),
    ('L', &[(-2.8, -3.8)]),
    ('Z', &[]),
    // Tail boom and tail rotor
    ('M', &[(-0.9, 2.0)]),
    ('L', &[(0.9, 2.0)]),
    ('L', &[(0.9, 9.4)]),
    ('L', &[(3.0, 9.4)]),
    ('L', &[(3.0, 11.0)]),
    ('L', &[(-3.0, 11.0)]),
    ('L', &[(-3.0, 9.4)]),
    ('L', &[(-0.9, 9.4)]),
    ('Z', &[]),
    // Rotor blades
    ('M', &[(-8.5, -8.5)]),
    ('L', &[(-7.5, -9.5)]),
    ('L', &[(8.5, 6.5)]),
    ('L', &[(7.5, 7.5)]),
    ('Z', &[]),
    ('M', &[(8.5, -8.5)]),
    ('L', &[(7.5, -9.5)]),
    ('L', &[(-8.5, 6.5)]),
    ('L', &[(-7.5, 7.5)]),
    ('Z', &[]),
];

/// The icon for this kind of aircraft centred at (cx, cy), nose pointing
/// `heading_deg` clockwise from up. Empty for `Unknown`, which is drawn as
/// the plain marker instead.
pub fn aircraft(kind: AircraftKind, cx: f64, cy: f64, heading_deg: f64) -> String {
    aircraft_sized(kind, cx, cy, heading_deg, 1.0)
}

/// As `aircraft`, `times` as large as on the radar (the details screen).
pub fn aircraft_sized(kind: AircraftKind, cx: f64, cy: f64, heading_deg: f64, times: f64) -> String {
    let scale = ICON_SCALE * times;
    match kind {
        AircraftKind::Light => outline(LIGHT, cx, cy, heading_deg, scale),
        AircraftKind::Small => outline(SMALL, cx, cy, heading_deg, scale),
        AircraftKind::Large => outline(PLANE, cx, cy, heading_deg, LARGE_SCALE * scale),
        AircraftKind::Heavy => outline(HEAVY, cx, cy, heading_deg, scale),
        AircraftKind::Rotor => outline(ROTOR, cx, cy, heading_deg, scale),
        AircraftKind::Unknown => String::new(),
    }
}

/// An outline rotated, scaled and moved to (cx, cy), as path commands.
fn outline(shape: Outline, cx: f64, cy: f64, heading_deg: f64, scale: f64) -> String {
    let (s, c) = heading_deg.to_radians().sin_cos();
    let mut out = String::with_capacity(400);
    for (cmd, points) in shape {
        out.push(*cmd);
        for (i, (x, y)) in points.iter().enumerate() {
            if i > 0 {
                out.push(' ');
            }
            let (x, y) = (x * scale, y * scale);
            let px = cx + x * c - y * s;
            let py = cy + x * s + y * c;
            out.push_str(&format!("{:.1} {:.1}", px, py));
        }
    }
    out
}

/// The plane riding the tip of the second hand: at `radius` from the
/// centre at `angle_deg`, flying clockwise (90 degrees ahead of the hand).
pub fn second_hand_plane(angle_deg: f64, radius: f64) -> String {
    let (s, c) = angle_deg.to_radians().sin_cos();
    plane(240.0 + radius * s, 240.0 - radius * c, angle_deg + 90.0)
}

/// Digital face: plane size relative to the base outline (design: 30 across).
const ORBIT_PLANE_SCALE: f64 = 15.0 / 13.0;

/// Digital face progress ring: an arc of radius `radius` from 12 o'clock
/// clockwise through `fraction` of a turn, as a polyline (Path has no
/// dash patterns, and a polyline avoids relying on SVG arc support).
pub fn progress_arc(radius: f64, fraction: f64) -> String {
    let fraction = fraction.clamp(0.0, 1.0);
    if fraction <= 0.0 {
        return String::new();
    }
    // One point per 3 degrees is smooth at this size.
    let steps = ((fraction * 120.0).ceil() as usize).max(1);
    let mut out = String::with_capacity(steps * 14 + 16);
    for i in 0..=steps {
        let a = (fraction * i as f64 / steps as f64 * 360.0).to_radians();
        let (s, c) = a.sin_cos();
        out.push(if i == 0 { 'M' } else { 'L' });
        out.push_str(&format!("{:.1} {:.1}", 240.0 + radius * s, 240.0 - radius * c));
    }
    out
}

/// Digital face: the plane at the end of the progress arc, flying clockwise.
pub fn orbit_plane(radius: f64, fraction: f64) -> String {
    let deg = fraction * 360.0;
    let (s, c) = deg.to_radians().sin_cos();
    plane_scaled(240.0 + radius * s, 240.0 - radius * c, deg + 90.0, ORBIT_PLANE_SCALE)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every number in a path string.
    fn numbers(path: &str) -> Vec<f64> {
        path.split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-'))
            .filter(|t| !t.is_empty())
            .map(|t| t.parse().unwrap())
            .collect()
    }

    /// Width and height of a path's bounding box.
    fn size(path: &str) -> (f64, f64) {
        let n = numbers(path);
        let (xs, ys): (Vec<f64>, Vec<f64>) = n.chunks(2).map(|p| (p[0], p[1])).unzip();
        let span = |v: &[f64]| v.iter().cloned().fold(f64::MIN, f64::max) - v.iter().cloned().fold(f64::MAX, f64::min);
        (span(&xs), span(&ys))
    }

    #[test]
    fn every_kind_but_unknown_has_an_icon() {
        use AircraftKind::*;
        for kind in [Light, Small, Large, Heavy, Rotor] {
            let path = aircraft(kind, 240.0, 240.0, 0.0);
            assert!(path.starts_with('M') && path.ends_with('Z'), "{:?}: {}", kind, path);
            // Coordinate pairs throughout
            assert_eq!(numbers(&path).len() % 2, 0, "{:?}", kind);
        }
        assert_eq!(aircraft(Unknown, 240.0, 240.0, 0.0), "");
    }

    #[test]
    fn icons_get_bigger_with_the_aircraft() {
        use AircraftKind::*;
        let width = |kind| size(&aircraft(kind, 0.0, 0.0, 0.0)).0;
        assert!(width(Light) < width(Small));
        assert!(width(Small) < width(Large));
        assert!(width(Large) < width(Heavy));
        // Readable but not sprawling on a 480 px radar
        for kind in [Light, Small, Large, Heavy, Rotor] {
            let (w, h) = size(&aircraft(kind, 0.0, 0.0, 0.0));
            assert!((18.0..=38.0).contains(&w) && (16.0..=38.0).contains(&h), "{:?}: {}x{}", kind, w, h);
        }
    }

    #[test]
    fn icons_are_symmetrical_and_turn_with_the_heading() {
        use AircraftKind::*;
        for kind in [Light, Small, Large, Heavy] {
            // Nose up: as far left of centre as right
            let n = numbers(&aircraft(kind, 0.0, 0.0, 0.0));
            let xs: Vec<f64> = n.chunks(2).map(|p| p[0]).collect();
            let (min, max) = (xs.iter().cloned().fold(f64::MAX, f64::min), xs.iter().cloned().fold(f64::MIN, f64::max));
            assert!((min + max).abs() < 0.11, "{:?}", kind);
            // The nose is the first point: straight up, then to the right when heading east
            assert!(n[0].abs() < 0.01 && n[1] < 0.0, "{:?}", kind);
            let east = numbers(&aircraft(kind, 0.0, 0.0, 90.0));
            assert!(east[0] > 0.0 && east[1].abs() < 0.11, "{:?}", kind);
            // Turning swaps width and height
            let (w, h) = size(&aircraft(kind, 0.0, 0.0, 0.0));
            let (we, he) = size(&aircraft(kind, 0.0, 0.0, 90.0));
            assert!((w - he).abs() < 0.3 && (h - we).abs() < 0.3, "{:?}", kind);
        }
    }

    #[test]
    fn icons_are_placed_where_asked() {
        let n = numbers(&aircraft(AircraftKind::Light, 100.0, 300.0, 0.0));
        // The light aircraft's nose is 7 units ahead of its centre
        assert_eq!(n[0], 100.0);
        assert!((n[1] - (300.0 - 7.0 * ICON_SCALE)).abs() < 0.06, "{}", n[1]);
    }

    #[test]
    fn plane_unrotated_matches_design() {
        assert!(plane(0.0, 0.0, 0.0).starts_with("M0.0 -13.0C1.6 -13.0 2.2 -10.0 2.2 -7.0L2.2 -2.0"));
    }

    #[test]
    fn progress_arc_quarter_ends_at_three_oclock() {
        let p = progress_arc(220.0, 0.25);
        assert!(p.starts_with("M240.0 20.0"), "{}", p);
        assert!(p.ends_with("L460.0 240.0"), "{}", p);
        assert_eq!(progress_arc(220.0, 0.0), "");
    }

    #[test]
    fn second_hand_plane_at_twelve_points_right() {
        // At 12 o'clock the nose (0,-13) rotated 90 degrees points to +x.
        let p = second_hand_plane(0.0, 186.0);
        assert!(p.starts_with("M253.0 54.0"), "{}", p);
    }
}
