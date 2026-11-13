#!/bin/bash
set -euo pipefail

# flight-clock.service gains systemd sandboxing (see the unit file). Units
# aren't replaced by the updater, so install the new one here. It takes
# effect when the updater restarts the clock, a moment after this runs.
#
# If the update is then rolled back, the previous release runs under the new
# unit: the limits are about what the process may touch, not which release it
# is, and they were checked against 0.3.0.

RELEASE_DIR="${FLIGHT_CLOCK_RELEASE_DIR:?}"

install -m644 "$RELEASE_DIR/systemd/flight-clock.service" /etc/systemd/system/flight-clock.service
systemctl daemon-reload
