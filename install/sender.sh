#!/usr/bin/env bash
# Install the xone-relay sender on a host with a working xone driver.
set -euo pipefail

cd "$(dirname "$0")"
REPO=$(cd .. && pwd)
BIN=${XONE_RELAY_BIN:-$REPO/dist/xone-relay}

[ "$(id -u)" -eq 0 ] || {
    echo "install: run as root" >&2
    exit 1
}
[ -x "$BIN" ] || {
    echo "install: missing $BIN; run ./build.sh first or set XONE_RELAY_BIN" >&2
    exit 1
}

if ldd "$BIN" 2>&1 | grep -qE 'not a dynamic executable|statically linked'; then
    :
else
    echo "install: $BIN is not a static binary" >&2
    exit 1
fi

if grep -qi 'Name="Microsoft Xbox Controller"' /proc/bus/input/devices; then
    :
else
    echo "install: no Xbox controller found. The sender will wait for one." >&2
fi

install -D -m 0755 "$BIN" /usr/local/libexec/xone-relay
if [ ! -e /etc/xone-relay.conf ]; then
    install -D -m 0644 "$REPO/config/xone-relay.conf" /etc/xone-relay.conf
fi
install -D -m 0644 "$REPO/systemd/xone-relay-sender.service" \
    /etc/systemd/system/xone-relay-sender.service
systemctl daemon-reload
systemctl enable --now xone-relay-sender.service
systemctl --no-pager --lines=0 status xone-relay-sender.service || true
