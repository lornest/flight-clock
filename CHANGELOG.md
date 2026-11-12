# Changelog

Versions follow [semantic versioning](https://semver.org). Each release's
section here becomes its GitHub release notes.

## [0.2.1] - 2026-11-12

### Fixed
- The clock could stop answering at `flight-clock.local` (becoming
  `flight-clock-2.local`) after the WiFi setup hotspot had been opened. It now
  reclaims its name when it comes back online.

## [0.2.0] - 2026-11-11

First release with signed updates.

### Added
- New look: night-sky palette with amber accent, Space Grotesk, JetBrains Mono
  and B612 fonts, launch screen, redesigned radar and alert.
- Digital clock face. Hold the FlightClock logo to switch faces.
- Settings page at `http://<clock>.local`: location (by UK postcode or
  coordinates), radar radius, facing direction, clock face, date, updates.
  Phones pair once with a code shown on the clock.
- Settings menu on the clock (double-tap the centre): WiFi, Date, Facing.
- WiFi setup hotspot with captive portal, on first use or when the saved
  network is unreachable; change WiFi from the clock.
- Automatic updates from signed GitHub releases, with rollback if a new
  version doesn't start. Stable and beta channels.
- One-line installer (`scripts/get.sh`) and HyperPixel 2.1 Round display and
  touch setup (`scripts/setup-hyperpixel2r.sh`).

### Changed
- Rewritten in Rust with Slint, drawing straight to the display (replaces the
  Next.js app in a browser kiosk): about 30 MB of memory.

### Fixed
- Callsigns no longer flip to the aircraft's hex code when the feed drops them.
- Airport fallback no longer shows a region ("Scotland") as the city.
