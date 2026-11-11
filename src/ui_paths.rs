//! SVG path data for shapes that have to be rotated at runtime.
//!
//! The software renderer can't rotate elements, so rotated outlines are
//! produced here as path strings for Slint `Path { commands: ... }`.

/// Plane outline from the brand design, nose up, centred on (0, 0),
/// about 26 units across.
const PLANE: &[(char, &[(f64, f64)])] = &[
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
    let (s, c) = heading_deg.to_radians().sin_cos();
    let mut out = String::with_capacity(400);
    for (cmd, points) in PLANE {
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
