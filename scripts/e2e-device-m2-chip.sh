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
#   4. `chip-tool interactive start` fed `onoff subscribe on-off <min> <max>
#      1 1` — verifies the Subscribe path (Task 12) end-to-end from the
#      *official* SDK controller: priming report + SubscribeResponse land,
#      and at least one keep-alive report arrives within max_interval. See
#      `verify_subscribe()` below for why this doesn't also drive a
#      change-report through chip-tool.
#   5. SIGTERM `matv`, restart it against the *same* store (fabrics persist;
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
#   MAT_E2E_IFACE            interface `matv`'s mDNS advertiser binds
#                            (default: `eth1` — plain `lo` has no IPv6
#                            link-local address; same default and rationale
#                            as e2e-device-m1.sh).
#   MAT_E2E_TIMEOUT_S        per-chip-tool-command timeout (default: 120;
#                            the whole `pairing` handshake plus Docker start
#                            fits well inside this on a laptop, but the
#                            image pull on a cold host does not, hence the
#                            generous budget). Also used as the hard backstop
#                            for the interactive subscribe container — see
#                            `verify_subscribe()`.
#   MAT_E2E_SUBSCRIBE_WAIT_S how long to watch the interactive subscribe
#                            session for a priming report + keep-alive
#                            before giving up (default: 12).
set -euo pipefail
cd "$(dirname "$0")/.."

IFACE="${MAT_E2E_IFACE:-eth1}"
TIMEOUT_S="${MAT_E2E_TIMEOUT_S:-120}"
SUBSCRIBE_WAIT_S="${MAT_E2E_SUBSCRIBE_WAIT_S:-12}"

# Single source of truth for the digest-pinned image, shared with
# scripts/chip-tool.sh via scripts/chip-tool-image.env (see that file for
# provenance — a spike documented in the m2-chip-tool-probe plan). Sharing
# the env var name means a caller override applies to both the `chip()`
# helper below (via the wrapper) and the interactive-mode `docker run` in
# `verify_subscribe()`.
#
# `verify_subscribe()` calls `docker run` directly instead of going through
# scripts/chip-tool.sh for that one invocation: the wrapper doesn't pass
# `-i` (needed to feed stdin into `interactive start`'s REPL), and it always
# appends `--storage-directory` as the *last* argument — which would land
# after (and so clobber/conflict with) `interactive start`'s own trailing
# arguments rather than sitting among chip-tool's global flags.
. scripts/chip-tool-image.env
CHIP_TOOL_IMAGE="${CHIP_TOOL_IMAGE:-$CHIP_TOOL_IMAGE_DEFAULT}"

PASSCODE=20202021
DISCRIMINATOR=3840
NODE_ID=1
ENDPOINT=2
SUB_MIN_INTERVAL_S=0
SUB_MAX_INTERVAL_S=5

WORKDIR=".e2e-cache/e2e-device-m2-chip.$$"
DEVICE_STORE="$WORKDIR/device-store"
CHIP_STORE="$PWD/$WORKDIR/chip-store"
MATV_CONFIG="$WORKDIR/matv.toml"
DEVICE_STDOUT="$WORKDIR/device.stdout.log"
DEVICE_STDERR="$WORKDIR/device.stderr.log"
CHIP_LOG="$WORKDIR/chip.log"
SUBSCRIBE_LOG="$WORKDIR/chip-subscribe.log"
mkdir -p "$DEVICE_STORE" "$CHIP_STORE"

DEVICE_PID=""
cleanup() {
    stop_device
    # Belt-and-braces: `verify_subscribe` already kills its own container
    # before returning (success or `fail`), but this covers an external
    # interrupt (Ctrl-C, an outer timeout) landing mid-poll. The name is
    # deterministic (`mat-e2e-sub-$$`), so this is a no-op double-kill on the
    # normal path.
    docker kill "mat-e2e-sub-$$" >/dev/null 2>&1 || true
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

[[device]]
id = "e2e-light"
kind = "onoff-light"
name = "E2E Light"
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

# Verifies chip-tool's Subscribe path end-to-end from the *official* SDK
# controller: feeds `onoff subscribe on-off <min> <max> <node> <endpoint>`
# into `chip-tool interactive start`'s stdin, then polls the session's log
# for (a) a priming report + SubscribeResponse and (b) at least one
# keep-alive report arriving within max_interval.
#
# Why `interactive start` and not the plain `chip-tool onoff subscribe on-off
# ...` subcommand: tried the plain form first (it exists — `chip-tool onoff
# subscribe on-off --help` documents it) and confirmed empirically that it
# exits the instant SubscribeResponse lands ("Shutting down the
# commissioner"), before any keep-alive can arrive. `interactive start` is a
# long-lived REPL; the fed command's ReadClient keeps running afterwards,
# which is what a keep-alive check needs.
#
# Why this only checks priming + keep-alive, not a change report: the device
# is sequential (one CASE session at a time — see
# `docs/superpowers/plans/m2-chip-tool-probe.md`), so a second chip-tool
# process toggling on-off would open a second CASE session and evict this
# one, killing the subscription before any change report could arrive.
# `crates/mat-device/tests/subscribe_loop.rs` (Task 12) already covers
# subscribe -> invoke -> change-report by driving both sides itself from a
# single process, so that path doesn't need re-proving here.
#
# Why the container is torn down with an explicit `docker kill --name`
# rather than relying on the fed stdin closing or a `timeout` wrapper alone:
# verified empirically that chip-tool's interactive shell does *not* exit
# when its stdin pipe hits EOF — it just stops accepting new commands while
# the ReadClient event loop keeps running (observed it stay up for minutes
# until force-killed). A `timeout N docker run ...` around it only kills the
# `docker` client process at N seconds, not the container — the container
# itself doesn't receive a signal, and docker was seen to leave it running.
# So cleanup here is unconditional (`|| true`) and independent of the
# `timeout` backstop, which exists only so a stuck/hanging `docker run`
# client can't wedge this function indefinitely.
verify_subscribe() {
    local name="mat-e2e-sub-$$"
    : >"$SUBSCRIBE_LOG"

    # `$!` below only captures the reader end (the `timeout ... docker run`
    # job); the writer subshell (`( printf ...; sleep ... )`) isn't waited on
    # separately. That's fine, not a leak: it does its one write immediately
    # and then just sleeps for at most $SUBSCRIBE_WAIT_S before exiting on
    # its own — bounded by the same constant we already poll against below —
    # and once the container is killed, its `sleep` finishing just hits a
    # closed pipe (SIGPIPE-safe here since there's nothing left to write).
    (
        printf 'onoff subscribe on-off %s %s %s %s\n' \
            "$SUB_MIN_INTERVAL_S" "$SUB_MAX_INTERVAL_S" "$NODE_ID" "$ENDPOINT"
        sleep "$SUBSCRIBE_WAIT_S"
    ) | timeout "${TIMEOUT_S}s" docker run --rm -i --network host --name "$name" \
        -v "$CHIP_STORE":/root/.chip-tool-store \
        -v "$PWD":/workdir -w /workdir \
        "$CHIP_TOOL_IMAGE" interactive start \
        --storage-directory /root/.chip-tool-store \
        >"$SUBSCRIBE_LOG" 2>&1 &
    local bg_pid=$!

    # Polls in 0.5s steps for up to $SUBSCRIBE_WAIT_S. `Refresh
    # LivenessCheckTime for` is logged once when the subscription is
    # established (right after SubscribeResponse) and again after every
    # report chip-tool receives thereafter — dirty or keep-alive alike — so
    # a count >= 2 with no dirty report in between (nothing else writes
    # on-off during this window) means a keep-alive round-tripped.
    local primed="" keepalive_count=0 waited=0
    while ((waited < SUBSCRIBE_WAIT_S * 2)); do
        if [[ -z "$primed" ]] && grep -q "SubscribeResponse is received" "$SUBSCRIBE_LOG" 2>/dev/null \
            && grep -q "OnOff: \(TRUE\|FALSE\)" "$SUBSCRIBE_LOG" 2>/dev/null; then
            primed=1
        fi
        keepalive_count="$(grep -c "Refresh LivenessCheckTime for" "$SUBSCRIBE_LOG" 2>/dev/null || true)"
        if [[ -n "$primed" && "${keepalive_count:-0}" -ge 2 ]]; then
            break
        fi
        sleep 0.5
        waited=$((waited + 1))
    done

    docker kill "$name" >/dev/null 2>&1 || true
    wait "$bg_pid" 2>/dev/null || true
    cat "$SUBSCRIBE_LOG" >>"$CHIP_LOG"

    [[ -n "$primed" ]] \
        || fail "chip-tool interactive subscribe: no priming report / SubscribeResponse observed within ${SUBSCRIBE_WAIT_S}s"
    [[ "${keepalive_count:-0}" -ge 2 ]] \
        || fail "chip-tool interactive subscribe: no keep-alive report observed within ${SUBSCRIBE_WAIT_S}s (LivenessCheckTime refresh count=${keepalive_count:-0}, want >=2)"
    echo "==> subscribe: priming report + keep-alive observed (LivenessCheckTime refreshed x${keepalive_count})" >&2
}

# bridge topology: EP0 PartsList ⊇ {1,2} / EP1(Aggregator) PartsList = [2]
assert_parts_list() {
    # $1=endpoint $2=expected-member
    local out
    out="$(chip descriptor read parts-list "$NODE_ID" "$1")" \
        || fail "descriptor read parts-list ep$1 failed (exit non-zero)"
    grep -Eq "\[[0-9]+\]: $2\b" <<<"$out" \
        || fail "ep$1 parts-list missing endpoint $2: $(grep -E 'PartsList|\[' <<<"$out" | head -5)"
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

echo "==> asserting bridge topology (EP0 PartsList {1,2}, EP1 Aggregator PartsList [2])" >&2
assert_parts_list 0 1
assert_parts_list 0 2
assert_parts_list 1 2
echo "==> topology OK" >&2

assert_toggle_flips "before restart"

echo "==> chip-tool interactive subscribe (min=${SUB_MIN_INTERVAL_S}s max=${SUB_MAX_INTERVAL_S}s, watching up to ${SUBSCRIBE_WAIT_S}s)" >&2
verify_subscribe

echo "==> restarting matv (SIGTERM, same store)" >&2
stop_device
start_device
echo "==> device back up: $(cat "$DEVICE_STDOUT")" >&2

# No re-pairing: chip-tool reuses the fabric/NOC it already stored and must
# re-establish CASE against the restarted device on its new port.
assert_toggle_flips "after restart"

echo "==> PASS: chip-tool commissioned, controlled, subscribed, and re-attached to matv" >&2
