# Migrations

One-time setup steps that ship with a release, for changes outside the binary
(packages, `config.txt`, systemd units...). Shell scripts named
`NNNN-short-description.sh`, run in order, as root, by the updater before it
switches to the new release. Each runs at most once per clock; applied names
are recorded in `/var/lib/flight-clock/migrations-applied`.

- Make scripts idempotent anyway (a failed run is retried on the next update).
- A non-zero exit stops the update; the clock stays on its current version.
- `$FLIGHT_CLOCK_RELEASE_DIR` is the new release's folder.
- Fresh installs mark every migration in the release as applied, since
  `install.sh` already sets things up from scratch: keep `install.sh` in step.
