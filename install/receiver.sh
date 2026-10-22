#!/usr/bin/env bash
# Install the xone-relay receiver and its systemd unit on the receiver machine.
set -euo pipefail

cd "$(dirname "$0")"
REPO=$(cd .. && pwd)
BIN=${XONE_RELAY_BIN:-$REPO/dist/xone-relay}

[ "$(id -u)" -eq 0 ] || {
    echo "install: run as root" >&2
    exit 1
}
[ -x "$BIN" ] || {
    echo "install: missing $BIN; run ../build.sh first or set XONE_RELAY_BIN" >&2
    exit 1
}

if ldd "$BIN" 2>&1 | grep -qE 'not a dynamic executable|statically linked'; then
    :
else
    echo "install: $BIN is not a static binary" >&2
    exit 1
fi

install -D -m 0755 "$BIN" /usr/local/libexec/xone-relay
install -D -m 0644 "$REPO/systemd/xone-relay-receiver.service" \
    /etc/systemd/system/xone-relay-receiver.service
if [ ! -e /etc/xone-relay.conf ]; then
    install -D -m 0644 "$REPO/config/xone-relay-receiver.conf" \
        /etc/xone-relay.conf
fi
printf 'uinput\n' > /etc/modules-load.d/xone-relay.conf
# uinput may be built into the kernel.
modprobe uinput || true

systemctl daemon-reload
systemctl enable --now xone-relay-receiver.service
# The status command is a report, so its exit code is not a check.
systemctl --no-pager --lines=0 status xone-relay-receiver.service || true
