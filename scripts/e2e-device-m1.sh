#!/usr/bin/env bash
# M1 self-commissioning E2E — the acceptance gate for
# `.superpowers/sdd/2026-08-15-mat-device-m1/`: the real `mat commission`
# binary, unmodified, must succeed against `mat-device`'s runtime.
#
# Flow: build the workspace (release) -> run `examples/device_m1` (Task 12
# stand-in for Task 13's real `matv` binary) in the background, capturing
# its printed QR payload and PAA directory -> `mat fabric init` into a
# throwaway store -> `mat commission --setup-code <qr> --node 1` against
# the running device, discovered over real mDNS -> assert the command's
# stdout JSON has `"status":"success"`.
#
# Env:
#   MAT_E2E_IFACE   interface both the device's mDNS advertiser and `mat
#                    commission`'s discovery use (default: `eth1`, chosen
#                    because plain `lo` lacks an IPv6 link-local address —
#                    see discover_live.rs's doc comment; on real hardware
#                    with exactly one usable NIC this can usually be left
#                    unset and `mat`'s own --iface autodetect would work,
#                    but this harness passes it explicitly either way to
#                    stay deterministic across hosts).
#   MAT_E2E_TIMEOUT_S  seconds to wait for `mat commission` (default: 30).
set -euo pipefail
cd "$(dirname "$0")/.."

IFACE="${MAT_E2E_IFACE:-eth1}"
TIMEOUT_S="${MAT_E2E_TIMEOUT_S:-30}"

WORKDIR="$(mktemp -d)"
DEVICE_LOG="$WORKDIR/device.log"
DEVICE_STORE="$WORKDIR/device-store"
MAT_STORE_DIR="$WORKDIR/mat-store"
mkdir -p "$DEVICE_STORE" "$MAT_STORE_DIR"

DEVICE_PID=""
cleanup() {
    if [[ -n "$DEVICE_PID" ]] && kill -0 "$DEVICE_PID" 2>/dev/null; then
        kill "$DEVICE_PID" 2>/dev/null || true
        wait "$DEVICE_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

echo "==> building (release)" >&2
cargo build --release -p mat
cargo build --release -p mat-device --example device_m1

echo "==> starting device (iface=$IFACE, store=$DEVICE_STORE)" >&2
MAT_DEVICE_STORE="$DEVICE_STORE" \
MAT_DEVICE_IFACE="$IFACE" \
MAT_DEVICE_PORT=0 \
RUST_LOG="${RUST_LOG:-info}" \
    ./target/release/examples/device_m1 >"$DEVICE_LOG" 2>&1 &
DEVICE_PID=$!

# Wait for the device to print its QR line (it prints before entering the
# serve loop, so this also confirms the socket bound and mDNS came up far
# enough to log — a bind/attestation failure exits the process instead,
# which the `kill -0` below would then also catch).
QR=""
for _ in $(seq 1 50); do
    if ! kill -0 "$DEVICE_PID" 2>/dev/null; then
        echo "device_m1 exited early:" >&2
        cat "$DEVICE_LOG" >&2
        exit 1
    fi
    QR="$(grep -m1 '^qr=' "$DEVICE_LOG" 2>/dev/null | sed 's/^qr=//')" || true
    [[ -n "$QR" ]] && break
    sleep 0.1
done
if [[ -z "$QR" ]]; then
    echo "device_m1 never printed a QR payload:" >&2
    cat "$DEVICE_LOG" >&2
    exit 1
fi
PAA_DIR="$(grep -m1 '^paa_dir=' "$DEVICE_LOG" | sed 's/^paa_dir=//')"
echo "==> device up: qr=$QR paa_dir=$PAA_DIR" >&2

echo "==> mat fabric init (store=$MAT_STORE_DIR)" >&2
MAT_STORE="$MAT_STORE_DIR" ./target/release/mat fabric init >&2

echo "==> mat commission (timeout ${TIMEOUT_S}s)" >&2
COMMISSION_JSON="$(
    MAT_STORE="$MAT_STORE_DIR" \
    MAT_PAA_TRUST_STORE="$PAA_DIR" \
        timeout "${TIMEOUT_S}s" ./target/release/mat --iface "$IFACE" commission \
            --setup-code "$QR" --node 1
)"
echo "$COMMISSION_JSON"

if ! command -v jq >/dev/null 2>&1; then
    echo "$COMMISSION_JSON" | grep -q '"status":"success"'
else
    STATUS="$(echo "$COMMISSION_JSON" | jq -r '.status')"
    [[ "$STATUS" == "success" ]]
fi

echo "==> PASS: mat commission reached status=success" >&2
