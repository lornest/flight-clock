# Changelog

Versions follow [semantic versioning](https://semver.org). Each release's
section here becomes its GitHub release notes.

## [0.3.0] - 2026-11-12

### Security
- The settings page and WiFi setup portal now run as their own unprivileged,
  sandboxed process (`flight-clock-web.service`) instead of inside the root
  process. Paired phones stay paired.
- Pairing: ten wrong codes in an hour, or six code requests in ten minutes,
  pause pairing, so asking for code after code no longer gets more guesses.
- The settings page only answers requests addressed to the clock's own name
  or IP address, and only accepts forms sent from its own pages.
- The file of paired phones is created private and replaced whole.
- Callsigns and airport codes are checked before being used in lookups, and
  release version names are limited to safe characters.
- Releases carry a build provenance attestation, checked before signing.
  GitHub Actions are pinned to commit SHAs.
- The WiFi setup hotspot is no longer open. It uses WPA2 with a new random
  password each time, shown on the clock as a QR code to scan, with the
  password underneath.
- Clocks also accept releases signed with a second, offline backup key
  (`keys/release-backup.pub`), so a lost or leaked release key can be replaced.

### Added
- First-run setup: a clock with no location shows a QR code. Scanning it
  pairs the phone and opens a short page for location and facing direction.
  **Later** on the clock or the page puts it off.
- **Pair a phone** on the clock's settings: a QR code that opens the settings
  page already paired. The 6-digit code still works.
- Location search by postcode, town or address, anywhere in the world. The
  phone asks postcodes.io or OpenStreetMap; the clock only gets coordinates.
- **Unpair all phones** on the settings page.
- Night hours: dim the screen or switch it off between two times, with
  alerts off unless you allow them. A touch wakes it for a minute. The
  display's backlight is on/off only, so "dim" darkens the picture.
- Alert filters: turn alerts off, only alert for aircraft below an altitude,
  and limit alerts to one every 1 to 30 minutes.
- Swipe between the clock and the radar. Double-tap still works.
- A short guided tour the first time the clock runs: it asks for each gesture
  in turn (swipe, double-tap the middle, hold the logo) and moves on when you
  do it. SKIP ends it.

### Changed
- Until the clock has the time from the network after a boot, the faces show
  "Setting the time" instead of a time that may be wrong.
- After an update the clock must run for 150 seconds (was 90), longer than
  the watchdog, so a version that starts and then freezes is rolled back.
  The settings page must be running too.
- The WiFi setup pages use the same look as the settings page (fonts, your
  accent colour and background), with drawn icons in place of emoji.
- The radar and splash grid lines and the digital time use the theme's
  colours instead of their own.
- A tap anywhere dismisses an alert; the small close button is gone.
- Instruction text on the clock (pairing, WiFi setup, QR codes, settings) is
  larger.
- No built-in location. A new clock shows no flights until its location is
  set on the settings page. Clocks that already have a location keep it.
- Settings are saved by replacing the file whole, so a power cut can't
  corrupt them.

## [0.2.3] - 2026-11-12

### Changed
- Amber is the default accent again, as it was before 0.2.2. Clocks that
  never picked a colour on the settings page go back to amber. A colour
  chosen on the settings page is kept.

## [0.2.2] - 2026-11-12

### Added
- Accent colours from the design: Amber, Powder blue, Coral, Lime and Lilac,
  chosen on the settings page. The whole clock and the settings page follow
  the choice.
- Background choice: Aviator blue or Black.

### Changed
- Coral is now the default accent. Clocks that never picked a colour switch
  from amber to coral; amber is still available on the settings page.
- Bigger text and rows in the settings menu on the clock.

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
