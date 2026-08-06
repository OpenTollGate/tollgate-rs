#!/bin/sh
# Remove a TollGate installation from macOS.
#
# Leaves the config alone unless --purge is given: it holds the node's identity,
# and every voucher a peer is holding is a claim on that identity.
set -e

PURGE=0
[ "${1:-}" = "--purge" ] && PURGE=1

if [ "$(id -u)" -ne 0 ]; then
    echo "Run with sudo." >&2
    exit 1
fi

launchctl bootout system /Library/LaunchDaemons/com.tollgate.daemon.plist 2>/dev/null || true
rm -f /Library/LaunchDaemons/com.tollgate.daemon.plist
rm -f /usr/local/bin/tollgated /usr/local/bin/tolltop
rm -rf /usr/local/var/log/tollgate
rm -f /usr/local/var/run/tollgate.sock
pkgutil --forget com.tollgate.pkg 2>/dev/null || true

if [ "$PURGE" -eq 1 ]; then
    rm -rf /usr/local/etc/tollgate
    echo "Removed, including the node's identity."
else
    echo "Removed. /usr/local/etc/tollgate kept — it holds this node's identity."
    echo "Pass --purge to delete it too."
fi
