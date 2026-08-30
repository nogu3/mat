#!/usr/bin/env bash
# M1 self-commissioning E2E — the acceptance gate for
# `.superpowers/sdd/2026-08-15-mat-device-m1/`: the real `mat commission`
# binary, unmodified, must succeed against the real `matv` device host.
#
# Flow: build the workspace (release) -> run `matv` (Task 13's real binary,
# replacing Task 12's `mat-device examples/device_m1` stand-in) in the
# background against a generated `matv.toml`, capturing its single stdout
# JSON line -> `mat fabric init` into a throwaway store -> `mat commission
# --setup-code <qr> --node 1` against the running device, discovered over
# real mDNS -> assert the command's stdout JSON has `"status":"success"`.
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
DEVICE_STDOUT="$WORKDIR/device.stdout.log"
DEVICE_STDERR="$WORKDIR/device.stderr.log"
DEVICE_STORE="$WORKDIR/device-store"
MAT_STORE_DIR="$WORKDIR/mat-store"
MATV_CONFIG="$WORKDIR/matv.toml"
mkdir -p "$DEVICE_STORE" "$MAT_STORE_DIR"

# jq is NOT guaranteed on the host running this script (unlike the
# `-real` E2E scripts under scripts/, which hard-require it) — fall back to
# python3, then to a plain grep/sed extractor for matv's flat single-line
# JSON (no nested objects/arrays in its output, so this is safe).
json_get() {
    local key="$1" json="$2"
    if command -v jq >/dev/null 2>&1; then
        printf '%s' "$json" | jq -r --arg k "$key" '.[$k]'
        return
    fi
    if command -v python3 >/dev/null 2>&1; then
        printf '%s' "$json" | python3 -c "
import json, sys
v = json.load(sys.stdin).get('$key')
print('' if v is None else v)
"
        return
    fi
    printf '%s' "$json" | sed -n "s/.*\"$key\":\"\{0,1\}\([^\",}]*\)\"\{0,1\}.*/\1/p"
}

DEVICE_PID=""
cleanup() {
    if [[ -n "$DEVICE_PID" ]] && kill -0 "$DEVICE_PID" 2>/dev/null; then
        kill "$DEVICE_PID" 2>/dev/null || true
        wait "$DEVICE_PID" 2>/dev/null || true
    fi
}
trap cleanup EXIT

echo "==> building (release)" >&2
cargo build --release -p matterctl
cargo build --release -p matv

cat >"$MATV_CONFIG" <<EOF
passcode = 20202021
discriminator = 3840
vendor_id = 0xFFF1
product_id = 0x8000
port = 0
store = "$DEVICE_STORE"
iface = "$IFACE"

[[device]]
id = "e2e-light"
kind = "onoff-light"
name = "E2E Light"
EOF

echo "==> starting matv (iface=$IFACE, store=$DEVICE_STORE)" >&2
RUST_LOG="${RUST_LOG:-info}" \
    ./target/release/matv --config "$MATV_CONFIG" \
    >"$DEVICE_STDOUT" 2>"$DEVICE_STDERR" &
DEVICE_PID=$!

# matv prints exactly one JSON line to stdout before entering the serve
# loop (mat 流儀: stdout=JSON, ログ=stderr — so DEVICE_STDOUT holds nothing
# but that line once it appears; a bind/attestation/config failure exits
# the process instead, which the `kill -0` below also catches).
DEVICE_JSON=""
for _ in $(seq 1 50); do
    if ! kill -0 "$DEVICE_PID" 2>/dev/null; then
        echo "matv exited early:" >&2
        echo "-- stdout --" >&2
        cat "$DEVICE_STDOUT" >&2
        echo "-- stderr --" >&2
        cat "$DEVICE_STDERR" >&2
        exit 1
    fi
    DEVICE_JSON="$(head -n1 "$DEVICE_STDOUT" 2>/dev/null)" || true
    [[ -n "$DEVICE_JSON" ]] && break
    sleep 0.1
done
if [[ -z "$DEVICE_JSON" ]]; then
    echo "matv never printed its setup-payload JSON line:" >&2
    echo "-- stdout --" >&2
    cat "$DEVICE_STDOUT" >&2
    echo "-- stderr --" >&2
    cat "$DEVICE_STDERR" >&2
    exit 1
fi

QR="$(json_get qr_payload "$DEVICE_JSON")"
STORE_FIELD="$(json_get store "$DEVICE_JSON")"
PAA_DIR="$STORE_FIELD/paa"
if [[ -z "$QR" ]]; then
    echo "matv stdout JSON had no qr_payload: $DEVICE_JSON" >&2
    exit 1
fi
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

STATUS="$(json_get status "$COMMISSION_JSON")"
[[ "$STATUS" == "success" ]]

echo "==> PASS: mat commission reached status=success" >&2
