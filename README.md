# FlightClock

A round clock that shows the aircraft flying overhead. Built for a
Raspberry Pi Zero 2W with a Pimoroni HyperPixel 2.1 Round touch display.

- Analogue or digital clock face
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
3. On your phone, open `http://flight-clock.local` (your hostname + `.local`),
   tap **Show code on the clock**, and enter the code. Set your location there.

No WiFi set up, or moved house? The clock opens a `FlightClock-Setup` hotspot:
join it on your phone and pick your network.

Options for the installer: `CHANNEL=beta` for pre-releases, `HYPERPIXEL=0` to
skip the display setup (e.g. `... | sudo CHANNEL=beta bash`).

## Using it

- **Double-tap the centre**: settings (WiFi, date, facing direction).
- **Double-tap left/right**: clock / radar.
- **Hold the FlightClock logo**: switch analogue / digital.
- **Settings page**: everything above plus location, radar radius and updates.

## Updates

Clocks check daily and install new releases automatically (turn this off, or
switch to the beta channel, on the settings page). Every release is signed;
clocks refuse anything not signed with the FlightClock release key
(`keys/release.pub`). If a new version doesn't stay running, the clock goes
back to the previous one.

## Development

```
cargo test
SLINT_BACKEND=winit-software cargo run          # clock window on your computer
FLIGHT_CLOCK_SCREEN=radar ...                   # start on the radar
FLIGHT_CLOCK_CONFIG=/tmp/fc.json ...            # use a local config file
```
The settings page runs on `http://localhost:8080` locally.

Build for the Pi (arm64 Docker container) and try it on a clock:
```
scripts/cross-build.sh
scripts/deploy-dev.sh user@flight-clock.local
```

### Releasing

1. Bump `version` in `Cargo.toml` and add a section to `CHANGELOG.md`.
2. Merge to `main`, then tag: `git tag v0.3.0 && git push origin v0.3.0`
   (`v0.3.0-beta.1` for the beta channel).
3. CI builds and tests it and creates a **draft** release.
4. Sign and publish from a machine with the release key in 1Password:
   `scripts/publish-release.sh v0.3.0`

Changes outside the binary (packages, `config.txt`, systemd units) go in
`migrations/` (see its README).

## Licence

GPL-3.0 (see `LICENSE`). Fonts are under the SIL Open Font License (see
`assets/fonts`). HyperPixel panel setup uses
[Pimoroni's hyperpixel2r](https://github.com/pimoroni/hyperpixel2r) files,
downloaded at install time.
