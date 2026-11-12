#!/bin/bash
set -euo pipefail

# The settings page and WiFi portal move out of the root process into
# flight-clock-web.service, running as the unprivileged flight-clock-web user.
# Runs while the previous release is still current; the new unit starts when
# the updater restarts flight-clock.service.

RELEASE_DIR="${FLIGHT_CLOCK_RELEASE_DIR:?}"
WEB_USER="flight-clock-web"
WEB_STATE="/var/lib/flight-clock-web"

if ! id -u "$WEB_USER" >/dev/null 2>&1; then
    useradd --system --user-group --no-create-home --home-dir /nonexistent \
        --shell /usr/sbin/nologin "$WEB_USER"
fi

# The settings page shows the update status from here; nothing in it is secret
chmod 755 /var/lib/flight-clock

# Keep phones paired: copy their sessions to where the web server keeps them.
# The original stays, in case the update is rolled back.
install -d -m700 -o "$WEB_USER" -g "$WEB_USER" "$WEB_STATE"
if [ -f /etc/flight-clock/web-sessions.json ] && [ ! -f "$WEB_STATE/web-sessions.json" ]; then
    install -m600 -o "$WEB_USER" -g "$WEB_USER" /etc/flight-clock/web-sessions.json "$WEB_STATE/web-sessions.json"
fi

install -m644 "$RELEASE_DIR/systemd/flight-clock-web.service" /etc/systemd/system/flight-clock-web.service
systemctl daemon-reload
systemctl enable flight-clock-web.service >/dev/null 2>&1
