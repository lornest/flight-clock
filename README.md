# FlightClock

A round clock that shows the aircraft flying overhead. Built for a
Raspberry Pi Zero 2W with a Pimoroni HyperPixel 2.1 Round touch display.

- Analogue or digital clock face
- Five accent colours and two backgrounds (aviator blue or black)
- Alert when a new aircraft comes into range, with its route
- Radar of nearby traffic, oriented to the way the clock faces
- Settings from your phone at `http://<clock>.local`
- Automatic, signed updates

Flight data: [adsb.fi](https://adsb.fi) open data. Routes and airports:
[HexDB](https://hexdb.io) and [adsb.lol](https://adsb.lol).

## Install

You need a Raspberry Pi Zero 2W (or other 64-bit Pi), a HyperPixel 2.1 Round,
and a microSD card.

1. Flash **Raspberry Pi OS Lite (64-bit)** with Raspberry Pi Imager. In its
   settings, set a hostname (e.g. `flight-clock`), your WiFi (2.4 GHz), your
   timezone, and enable SSH.
2. Boot the Pi, connect with SSH, and run:
   ```
   curl -fsSL https://raw.githubusercontent.com/lornest/flight-clock/main/scripts/get.sh | sudo bash
   sudo reboot
   ```
3. The clock shows a QR code. Scan it with your phone (on the same WiFi) and
   say where the clock is and which way it faces: it shows no flights until
   it knows. You can search for a postcode, town or address; your phone asks
   postcodes.io or OpenStreetMap, and the clock only gets the coordinates.
   No camera? Open `http://flight-clock.local` (your hostname + `.local`),
   tap **Show code on the clock**, and enter the code.

No WiFi set up, or moved house? The clock opens a `FlightClock-Setup` hotspot
and shows a QR code for it: scan it to join, then pick your network. The
hotspot has a new random password each time (shown under the code), so the
WiFi password you type isn't sent in the clear.

Options for the installer: `CHANNEL=beta` for pre-releases, `HYPERPIXEL=0` to
skip the display setup (e.g. `... | sudo CHANNEL=beta bash`).

## Using it

- **Double-tap the centre**: settings (WiFi, date, facing direction), and
  **Pair a phone**, a QR code that opens the settings page on your phone.
- **Swipe** (or double-tap left/right): clock / radar.
- **Tap an alert** to dismiss it.
- **Hold the FlightClock logo**: switch analogue / digital.
- **Settings page**: everything above plus location, radar radius, colours,
  updates, alerts (off, only below an altitude, at most one every few
  minutes) and night hours (dim the screen or switch it off, with or without
  alerts; a touch wakes it for a minute).

The first time it runs, the clock walks you through these gestures.

## Updates

Clocks check daily and install new releases automatically (turn this off, or
switch to the beta channel, on the settings page). Every release is signed;
clocks refuse anything not signed with a FlightClock release key
(`keys/release.pub`, or the offline backup `keys/release-backup.pub`). If a new version doesn't stay running, the clock goes
back to the previous one.

## Development

```
cargo test
SLINT_BACKEND=winit-software cargo run          # clock window on your computer
FLIGHT_CLOCK_SCREEN=radar ...                   # start on the radar
FLIGHT_CLOCK_CONFIG=/tmp/fc.json ...            # use a local config file
```
The settings page runs on `http://localhost:8080` locally.

On the Pi, FlightClock is two processes. `flight-clock.service` (root) drives
the screen, WiFi and updates. `flight-clock-web.service` runs the settings
page and WiFi setup portal as the unprivileged `flight-clock-web` user, and
asks the clock for anything else over a Unix socket (`src/ipc.rs`). On a dev
machine the web server runs on a thread of the one process, over the same
socket.

Build for the Pi (arm64 Docker container) and try it on a clock:
```
scripts/cross-build.sh
scripts/deploy-dev.sh user@flight-clock.local
```

### Releasing

GitHub Actions are pinned to commit SHAs in `.github/workflows/`; update the
SHA and the version comment together.

1. Bump `version` in `Cargo.toml` and add a section to `CHANGELOG.md`.
2. Merge to `main`, then tag: `git tag v0.3.0 && git push origin v0.3.0`
   (`v0.3.0-beta.1` for the beta channel).
3. CI builds and tests it and creates a **draft** release.
4. Sign and publish from a machine with the release key in 1Password:
   `scripts/publish-release.sh v0.3.0`. Before signing, it checks the build's
   provenance attestation: that the tarball was built by this repo's release
   workflow from that tag.

If the release key is lost, sign with the backup key instead:
`KEY_FILE=/path/to/backup.key scripts/publish-release.sh v0.3.0`. If it
leaks, use the backup key to sign a release that replaces `keys/release.pub`
(and the key list in `scripts/get.sh`) with a new key.

Changes outside the binary (packages, `config.txt`, systemd units) go in
`migrations/` (see its README).

## Licence

GPL-3.0 (see `LICENSE`). Fonts are under the SIL Open Font License (see
`assets/fonts`). HyperPixel panel setup uses
[Pimoroni's hyperpixel2r](https://github.com/pimoroni/hyperpixel2r) files,
downloaded at install time.
