#!/bin/bash
set -euo pipefail

# Deploy a local build to a clock as "releases/dev", for testing.
#   scripts/deploy-dev.sh lorne@flight-clock.local
#
# Builds (scripts/cross-build.sh), copies the binary to the clock and points
# /opt/flight-clock/current at releases/dev. Release updates replace it as
# soon as a newer version is published; to go back to the last release,
# run:  sudo ln -sfn "$(readlink /opt/flight-clock/previous)" /opt/flight-clock/current

HOST="${1:?usage: deploy-dev.sh user@host}"
cd "$(dirname "$0")/.."

./scripts/cross-build.sh
scp target/pi/flight-clock "$HOST:/tmp/flight-clock"
scp systemd/flight-clock-web.service "$HOST:/tmp/flight-clock-web.service"
ssh "$HOST" 'set -e
    sudo mkdir -p /opt/flight-clock/releases/dev
    sudo install -m755 /tmp/flight-clock /opt/flight-clock/releases/dev/flight-clock
    # The web server unit and its user, for clocks installed before the split
    id -u flight-clock-web >/dev/null 2>&1 || sudo useradd --system --user-group --no-create-home \
        --home-dir /nonexistent --shell /usr/sbin/nologin flight-clock-web
    sudo chmod 755 /var/lib/flight-clock
    sudo install -m644 /tmp/flight-clock-web.service /etc/systemd/system/flight-clock-web.service
    sudo systemctl daemon-reload
    sudo systemctl enable flight-clock-web.service
    cur=$(readlink /opt/flight-clock/current || true)
    if [ "$cur" != "releases/dev" ] && [ -n "$cur" ]; then sudo ln -sfn "$cur" /opt/flight-clock/previous; fi
    sudo ln -sfn releases/dev /opt/flight-clock/current
    sudo systemctl restart flight-clock
    sleep 3; systemctl is-active flight-clock flight-clock-web'
