#!/usr/bin/env bash
# M2 gate 1 — chip-tool interop E2E. The acceptance gate for
# `.superpowers/sdd/2026-08-16-mat-device-m2/` Phase A: the *official*
# Matter SDK controller (`chip-tool`, run from a digest-pinned Docker image
# by `scripts/chip-tool.sh`), unmodified, must commission the real `matv`
# device host, control it, and re-attach to it after the device restarts.
#
# Flow:
#   1. build (release) -> run `matv` against a generated `matv.toml`,
#      capturing its single stdout JSON line
#   2. `chip-tool pairing onnetwork-long 1 <passcode> <discriminator>` —
#      real mDNS discovery, PASE, full attestation verification (PAA trust
#      store + Certification Declaration), CSR/NOC, operational CASE
#   3. `chip-tool onoff read on-off 1 1` (baseline) -> `onoff toggle 1 1`
#      -> read again, asserting the value actually flipped
#   4. SIGTERM `matv`, restart it against the *same* store (fabrics persist;
#      the UDP port is re-picked, so chip-tool must re-resolve it over the
#      operational mDNS advert) -> read/toggle/read again, proving the
#      re-CASE path works against a restarted device
#
# Why the workdir lives under the repo (not `mktemp -d`): the chip-tool
# wrapper mounts `$PWD` as the container's `/workdir`, so anything it must
# read — here the PAA trust store `matv` writes — has to sit inside the
# checkout. `/.e2e-cache/` is gitignored. See
# `docs/superpowers/plans/m2-chip-tool-probe.md` ("つまずき" section).
#
# Env:
#   MAT_E2E_IFACE      interface `matv`'s mDNS advertiser binds (default:
#                      `eth1` — plain `lo` has no IPv6 link-local address;
#                      same default and rationale as e2e-device-m1.sh).
#   MAT_E2E_TIMEOUT_S  per-chip-tool-command timeout (default: 120; the
#                      whole `pairing` handshake plus Docker start fits well
#                      inside this on a laptop, but the image pull on a cold
#                      host does not, hence the generous budget).
set -euo pipefail
cd "$(dirname "$0")/.."

IFACE="${MAT_E2E_IFACE:-eth1}"
TIMEOUT_S="${MAT_E2E_TIMEOUT_S:-120}"

PASSCODE=20202021
DISCRIMINATOR=3840
NODE_ID=1
ENDPOINT=1

WORKDIR=".e2e-cache/e2e-device-m2-chip.$$"
DEVICE_STORE="$WORKDIR/device-store"
CHIP_STORE="$PWD/$WORKDIR/chip-store"
MATV_CONFIG="$WORKDIR/matv.toml"
DEVICE_STDOUT="$WORKDIR/device.stdout.log"
DEVICE_STDERR="$WORKDIR/device.stderr.log"
CHIP_LOG="$WORKDIR/chip.log"
mkdir -p "$DEVICE_STORE" "$CHIP_STORE"

DEVICE_PID=""
cleanup() {
    stop_device
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

stop_device() {
    if [[ -n "$DEVICE_PID" ]] && kill -0 "$DEVICE_PID" 2>/dev/null; then
        kill "$DEVICE_PID" 2>/dev/null || true
        wait "$DEVICE_PID" 2>/dev/null || true
    fi
    DEVICE_PID=""
}

fail() {
    echo "FAIL: $*" >&2
    echo "-- matv stderr (tail) --" >&2
    tail -n 40 "$DEVICE_STDERR" >&2 || true
    echo "-- chip-tool (tail) --" >&2
    tail -n 40 "$CHIP_LOG" >&2 || true
    exit 1
}

# Starts (or restarts) matv against $DEVICE_STORE and waits for its single
# stdout JSON line. `port = 0` on purpose: a restart deliberately lands on a
# *different* UDP port, so step 4 really exercises chip-tool re-resolving the
# device through its operational mDNS advert rather than reusing a cached
# address:port from the first session.
start_device() {
    cat >"$MATV_CONFIG" <<EOF
passcode = $PASSCODE
discriminator = $DISCRIMINATOR
vendor_id = 0xFFF1
product_id = 0x8000
port = 0
store = "$DEVICE_STORE"
iface = "$IFACE"
EOF
    : >"$DEVICE_STDOUT"
    RUST_LOG="${RUST_LOG:-info}" \
        ./target/release/matv --config "$MATV_CONFIG" \
        >"$DEVICE_STDOUT" 2>>"$DEVICE_STDERR" &
    DEVICE_PID=$!

    for _ in $(seq 1 100); do
        if ! kill -0 "$DEVICE_PID" 2>/dev/null; then
            fail "matv exited early"
        fi
        [[ -s "$DEVICE_STDOUT" ]] && return 0
        sleep 0.1
    done
    fail "matv never printed its setup-payload JSON line"
}

# Runs one chip-tool command, appending its output to $CHIP_LOG (kept for
# the failure dump) and echoing it so callers can grep the result.
chip() {
    local out
    if ! out="$(CHIP_TOOL_STORE="$CHIP_STORE" timeout "${TIMEOUT_S}s" \
        bash scripts/chip-tool.sh "$@" 2>&1)"; then
        printf '%s\n' "$out" >>"$CHIP_LOG"
        return 1
    fi
    printf '%s\n' "$out" >>"$CHIP_LOG"
    printf '%s\n' "$out"
}

# `chip-tool onoff read on-off` prints `CHIP:TOO:   OnOff: TRUE|FALSE`.
# Echoes `TRUE`/`FALSE`, or fails if the read produced no value at all
# (which is what a broken CASE / unserved attribute looks like: exit 0 with
# a `CHIP:TOO: Error` line and no OnOff report).
read_onoff() {
    local out value
    out="$(chip onoff read on-off "$NODE_ID" "$ENDPOINT")" \
        || fail "chip-tool onoff read failed (exit non-zero)"
    value="$(printf '%s\n' "$out" | sed -n 's/.*OnOff: \(TRUE\|FALSE\).*/\1/p' | tail -n1)"
    [[ -n "$value" ]] || fail "chip-tool onoff read returned no OnOff value"
    printf '%s\n' "$value"
}

# One read -> toggle -> read cycle, asserting the attribute actually
# changed. This is the real "the controller can both command and observe
# this device" check: a toggle that silently no-ops, or a read served from
# a stale/unrelated path, fails here rather than looking green.
#
# The `|| exit 1` on each `$(read_onoff)` is load-bearing, not belt-and-
# braces: `fail` runs inside the command substitution's *subshell*, so its
# `exit 1` only kills that subshell. Without an explicit propagation the
# script would carry on with an empty `before`/`after` — and relying on
# `set -e` alone here is exactly the kind of thing that quietly stops
# working the moment this call moves into a condition or a pipeline.
assert_toggle_flips() {
    local phase="$1" before after
    before="$(read_onoff)" || exit 1
    chip onoff toggle "$NODE_ID" "$ENDPOINT" >/dev/null \
        || fail "$phase: chip-tool onoff toggle failed"
    after="$(read_onoff)" || exit 1
    [[ "$before" != "$after" ]] \
        || fail "$phase: OnOff did not change across toggle (stayed $before)"
    echo "==> $phase: OnOff $before -> $after" >&2
}

echo "==> building (release)" >&2
cargo build --release -p matv

echo "==> starting matv (iface=$IFACE, store=$DEVICE_STORE)" >&2
start_device
echo "==> device up: $(cat "$DEVICE_STDOUT")" >&2

echo "==> chip-tool pairing onnetwork-long (timeout ${TIMEOUT_S}s)" >&2
PAIR_OUT="$(chip pairing onnetwork-long "$NODE_ID" "$PASSCODE" "$DISCRIMINATOR" \
    --paa-trust-store-path "$DEVICE_STORE/paa")" \
    || fail "chip-tool pairing exited non-zero"
# chip-tool exits 0 on some partial failures, so the success line is the
# real assertion, not the exit code alone.
printf '%s\n' "$PAIR_OUT" | grep -q "Device commissioning completed with success" \
    || fail "chip-tool pairing did not report commissioning success"
echo "==> commissioned" >&2

assert_toggle_flips "before restart"

echo "==> restarting matv (SIGTERM, same store)" >&2
stop_device
start_device
echo "==> device back up: $(cat "$DEVICE_STDOUT")" >&2

# No re-pairing: chip-tool reuses the fabric/NOC it already stored and must
# re-establish CASE against the restarted device on its new port.
assert_toggle_flips "after restart"

echo "==> PASS: chip-tool commissioned, controlled, and re-attached to matv" >&2
