//! QR codes for the clock's screen: joining the setup hotspot, and opening
//! the settings page already paired.

use qrcodegen::{QrCode, QrCodeEcc};

/// Light border around the code, in modules. Scanners need it to find the code.
const QUIET_ZONE: i32 = 4;

/// A square black-on-white bitmap of `text`, `size` pixels a side: 0 is dark,
/// 255 light. Modules are a whole number of pixels so edges stay sharp; any
/// space left over is extra border. None if the text can't fit in a QR code
/// or the code can't fit in `size`.
pub fn bitmap(text: &str, size: u32) -> Option<Vec<u8>> {
    let qr = QrCode::encode_text(text, QrCodeEcc::Medium).ok()?;
    let modules = qr.size() + 2 * QUIET_ZONE;
    let scale = size as i32 / modules;
    if scale < 1 {
        return None;
    }
    let offset = (size as i32 - qr.size() * scale) / 2;
    let mut pixels = vec![255u8; (size * size) as usize];
    for y in 0..qr.size() {
        for x in 0..qr.size() {
            if !qr.get_module(x, y) {
                continue;
            }
            for py in 0..scale {
                let row = (offset + y * scale + py) as usize * size as usize;
                let start = row + (offset + x * scale) as usize;
                pixels[start..start + scale as usize].fill(0);
            }
        }
    }
    Some(pixels)
}

/// The same, as an image for the UI. An empty image if it can't be made.
pub fn image(text: &str, size: u32) -> slint::Image {
    let Some(pixels) = bitmap(text, size) else {
        eprintln!("Couldn't make a QR code {} px wide for {} characters", size, text.len());
        return slint::Image::default();
    };
    let mut buffer = slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(size, size);
    for (out, v) in buffer.make_mut_slice().iter_mut().zip(pixels) {
        *out = slint::Rgb8Pixel { r: v, g: v, b: v };
    }
    slint::Image::from_rgb8(buffer)
}

/// What a phone's camera needs to join a WPA network.
/// `\ ; , : "` in the name or password are escaped, as the format asks.
pub fn wifi_text(ssid: &str, password: &str) -> String {
    let escape = |s: &str| {
        let mut out = String::new();
        for c in s.chars() {
            if matches!(c, '\\' | ';' | ',' | ':' | '"') {
                out.push('\\');
            }
            out.push(c);
        }
        out
    };
    format!("WIFI:T:WPA;S:{};P:{};;", escape(ssid), escape(password))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitmap_has_a_border_and_finder_patterns() {
        let size = 205;
        let pixels = bitmap(&wifi_text("FlightClock-Setup", "abcd-efgh-jkmn"), size).unwrap();
        assert_eq!(pixels.len(), (size * size) as usize);
        // Light border all the way round
        let at = |x: u32, y: u32| pixels[(y * size + x) as usize];
        for i in 0..size {
            assert_eq!(at(i, 0), 255);
            assert_eq!(at(0, i), 255);
            assert_eq!(at(i, size - 1), 255);
            assert_eq!(at(size - 1, i), 255);
        }
        // The first dark pixel on the diagonal is the corner of the top-left
        // finder pattern, at least a 4-module border in
        let first_dark = (0..size).find(|&i| at(i, i) == 0).unwrap();
        assert!(first_dark >= 4 * 4, "{}", first_dark);
        // Only black and white
        assert!(pixels.iter().all(|&p| p == 0 || p == 255));
    }

    #[test]
    fn the_screen_codes_fit_at_five_pixels_a_module() {
        // Version 4 (33 modules) + border = 41 modules; 5 px each = 205
        for text in [
            wifi_text("FlightClock-Setup", "abcd-efgh-jkmn"),
            "http://flight-clock-kitchen.local/s?k=abcdefghjkmnpqrs".to_string(),
        ] {
            let qr = QrCode::encode_text(&text, QrCodeEcc::Medium).unwrap();
            assert!(qr.size() <= 33, "{} needs {} modules", text, qr.size());
        }
    }

    #[test]
    fn too_small_or_too_long_gives_none() {
        assert!(bitmap("hello", 10).is_none());
        assert!(bitmap(&"x".repeat(5000), 400).is_none());
    }

    #[test]
    fn wifi_text_escapes_special_characters() {
        assert_eq!(wifi_text("Home", "pass"), "WIFI:T:WPA;S:Home;P:pass;;");
        assert_eq!(wifi_text("a;b", r#"p:"\,"#), r#"WIFI:T:WPA;S:a\;b;P:p\:\"\\\,;;"#);
    }
}
