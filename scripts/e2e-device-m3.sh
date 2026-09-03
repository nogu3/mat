#!/usr/bin/env bash
# M3 matd x matv regression E2E — the real motivation behind audit backlog
# item ③ (2026-08-31): "matd resident Subscribe + `mat listen` against a
# virtual device", run with the real `matd` and `mat` binaries against the
# real `matv` device host (no mocks on either side of the socket).
#
# Flow: build the workspace (release) -> run `matv` (single onoff-light
# device, EP1=Aggregator/EP2=the light per `mat-device`'s bridge topology) in
# the background -> `mat fabric init` + `mat commission` into a throwaway
# store (same as e2e-device-m1.sh) -> `mat group provision` against the
# commissioned node, asserting `status:"provisioned"` (the KeySetWrite +
# group-key-map write + AddGroup + ACL write sequence actually lands on
# matv) -> `mat group list`, asserting the provisioned group shows up in the
# controller kvs (`mat group remove` is held back — see the comment at that
# step: matv has no `KeySetRemove`) -> start `matd` against the same store, poll `matd status` until its
# resident wildcard Subscribe to node 1 reaches `state:"established"` ->
# start `mat listen --count 1` in the background -> `mat on` (routed through
# matd) -> assert the backgrounded `mat listen` received an onoff on-off=true
# event before its budget ran out.
#
# Why the toggle might show up as `recovered:true` rather than a live dirty
# report: `matd` holds the resident Subscribe on a *dedicated* CASE session,
# separate from the warm session it opens to run `mat on` (see
# `matd::native::NativeBackend::establish_subscription`'s doc comment). Real
# Matter devices serve several concurrent CASE sessions, but `matv`'s device
# loop (`mat-device::net::runtime::run`) serves exactly one at a time by
# design (M1 scope) — a new CASE session there evicts whatever session (and
# subscription) came before it. So `mat on`'s own session evicts matd's
# Subscribe session, runs the command, and matd's Subscribe loop then
# reconnects (5s initial backoff) and finds the value changed since it last
# saw it — exactly the `recovered: true` path documented in
# docs/commands.md#listen-device-originated-events ("A transition that
# matd's own op caused during the blind window also comes back as
# recovered: true"). Either delivery shape is a pass here; this script only
# asserts on the event's cluster/attribute/value, not its priming/recovered
# flags, matching that doc's own guidance that a consumer should key off the
# value.
#
# Env:
#   MAT_E2E_IFACE     interface both matv's mDNS advertiser and mat/matd's
#                      discovery use (default: `eth1` — same rationale as
#                      e2e-device-m1.sh).
#   MAT_E2E_TIMEOUT_S  seconds budgeted for `mat commission`, for matd's
#                      subscription to node 1 to reach `established`, and
#                      (in ms) for `mat listen`'s receive window (default:
#                      30).
set -euo pipefail
cd "$(dirname "$0")/.."

IFACE="${MAT_E2E_IFACE:-eth1}"
TIMEOUT_S="${MAT_E2E_TIMEOUT_S:-30}"

GROUP_ID=10
NODE_ID=1
DEVICE_EP=2 # the bridged onoff-light endpoint (EP1 is the Aggregator)

WORKDIR="$(mktemp -d)"
DEVICE_STDOUT="$WORKDIR/device.stdout.log"
DEVICE_STDERR="$WORKDIR/device.stderr.log"
DEVICE_STORE="$WORKDIR/device-store"
MAT_STORE_DIR="$WORKDIR/mat-store"
MATV_CONFIG="$WORKDIR/matv.toml"
MATD_SOCK="$WORKDIR/matd.sock"
MATD_STDOUT="$WORKDIR/matd.stdout.log"
MATD_STDERR="$WORKDIR/matd.stderr.log"
LISTEN_STDOUT="$WORKDIR/listen.stdout.log"
LISTEN_STDERR="$WORKDIR/listen.stderr.log"
mkdir -p "$DEVICE_STORE" "$MAT_STORE_DIR"

# jq is NOT guaranteed on the host running this script — see
# e2e-device-m1.sh's json_get, copied verbatim here for flat top-level
# fields (matv/mat's single-line JSON has none of the nested arrays `matd
# status` does).
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

# `matd status`'s JSON nests the per-node subscription state
# (`.nodes[].state`), which the sed fallback above can't reach — this
# script requires jq or python3 for that one lookup (both are otherwise
# ubiquitous; the sed path above stays for parity with e2e-device-m1.sh's
# flat lookups).
matd_node_state() {
    local json="$1" node_id="$2"
    if command -v jq >/dev/null 2>&1; then
        printf '%s' "$json" | jq -r --argjson n "$node_id" \
            '(.nodes // [])[] | select(.node_id == $n) | .state' 2>/dev/null | head -n1
        return
    fi
    printf '%s' "$json" | python3 -c "
import json, sys
node_id = $node_id
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
for n in d.get('nodes') or []:
    if n.get('node_id') == node_id:
        print(n.get('state', ''))
        break
"
}

DEVICE_PID=""
MATD_PID=""
LISTEN_PID=""
cleanup() {
    # matd -> matv order (brief's step 9): tear down the daemon's warm/
    # subscribe sessions before the device they talk to disappears.
    if [[ -n "$LISTEN_PID" ]] && kill -0 "$LISTEN_PID" 2>/dev/null; then
        kill "$LISTEN_PID" 2>/dev/null || true
        wait "$LISTEN_PID" 2>/dev/null || true
    fi
    if [[ -n "$MATD_PID" ]] && kill -0 "$MATD_PID" 2>/dev/null; then
        kill "$MATD_PID" 2>/dev/null || true
        wait "$MATD_PID" 2>/dev/null || true
    fi
    if [[ -n "$DEVICE_PID" ]] && kill -0 "$DEVICE_PID" 2>/dev/null; then
        kill "$DEVICE_PID" 2>/dev/null || true
        wait "$DEVICE_PID" 2>/dev/null || true
    fi
    rm -rf "$WORKDIR"
}
trap cleanup EXIT

echo "==> building (release)" >&2
cargo build --release -p matterctl -p matd -p matv

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
# loop (mat 流儀: stdout=JSON, ログ=stderr).
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
            --setup-code "$QR" --node "$NODE_ID"
)"
echo "$COMMISSION_JSON"
STATUS="$(json_get status "$COMMISSION_JSON")"
[[ "$STATUS" == "success" ]]
echo "==> commissioned (node=$NODE_ID)" >&2

echo "==> mat group provision (group=$GROUP_ID, node=$NODE_ID, endpoint=$DEVICE_EP)" >&2
GROUP_JSON="$(
    MAT_STORE="$MAT_STORE_DIR" \
        ./target/release/mat --iface "$IFACE" group provision \
            --group "$GROUP_ID" --nodes "$NODE_ID" --endpoint "$DEVICE_EP" --name e2e-group
)"
echo "$GROUP_JSON"
GROUP_STATUS="$(json_get status "$GROUP_JSON")"
[[ "$GROUP_STATUS" == "provisioned" ]]
echo "==> PASS: mat group provision reached status=provisioned (KeySetWrite + group-key-map + AddGroup + ACL all landed on matv)" >&2

echo "==> mat group list (controller kvs)" >&2
LIST_JSON="$(MAT_STORE="$MAT_STORE_DIR" ./target/release/mat group list)"
echo "$LIST_JSON" >&2
printf '%s' "$LIST_JSON" | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert [g["group_id"] for g in d["groups"]] == ['"$GROUP_ID"'], d
'

# `mat group remove` はここでは撃てない: matv の GroupKeyManagement は
# `KeySetWrite` しか実装しておらず（`crates/mat-device/src/core/
# group_key_management.rs` のモジュール doc に「`KeySetRead`/`KeySetRemove`/
# `KeySetReadAllIndices` コマンドと永続化は未実装（既知ギャップ）」と明記）、
# 撤収 4 ステップの最後 `KeySetRemove` が IM status 0x81
# (UNSUPPORTED_COMMAND) で弾かれる:
#   {"error":{"detail":"node 1: remove step 'key-set-remove' failed: native:
#    interaction model error: device rejected command: IM status 0x81",
#    "kind":"device_rejected"}}
# デバイス側の既知ギャップであってコントローラ側 `group remove` の不具合では
# ないため、ここでアサーションを緩めるのではなくステップごと保留する。matv に
# `KeySetRemove` が実装されたら `mat group remove --group $GROUP_ID --nodes
# $NODE_ID --endpoint $DEVICE_EP` → `status:"removed"` → `group list` が
# `groups`/`keysets` とも空、の順で足し直すこと（そのときは以降の matd/listen
# 脚のために provision し直す必要がある）。

echo "==> mat group invoke (multicast) — group=$GROUP_ID cluster=onoff command=on endpoint=$DEVICE_EP" >&2
MAT_STORE="$MAT_STORE_DIR" \
    ./target/release/mat --iface "$IFACE" group invoke -g "$GROUP_ID" -c onoff --command on -e "$DEVICE_EP" >&2
sleep 1
READ_JSON="$(
    MAT_STORE="$MAT_STORE_DIR" \
        ./target/release/mat --iface "$IFACE" read --node "$NODE_ID" --endpoint "$DEVICE_EP" --cluster onoff --attribute on-off
)"
echo "$READ_JSON"
[[ "$(json_get value "$READ_JSON")" == "true" ]] || {
    echo "groupcast did not reach matv: on-off is not true after mat group invoke on: $READ_JSON" >&2
    echo "-- matv stderr tail --" >&2; tail -n 40 "$DEVICE_STDERR" >&2
    exit 1
}
echo "==> PASS: groupcast on reached matv over multicast (on-off=true)" >&2
MAT_STORE="$MAT_STORE_DIR" ./target/release/mat --iface "$IFACE" off --node "$NODE_ID" --endpoint "$DEVICE_EP" >&2

echo "==> starting matd (store=$MAT_STORE_DIR, iface=$IFACE, socket=$MATD_SOCK)" >&2
RUST_LOG="${RUST_LOG:-info}" \
    ./target/release/matd --store "$MAT_STORE_DIR" --iface "$IFACE" --socket "$MATD_SOCK" \
    >"$MATD_STDOUT" 2>"$MATD_STDERR" &
MATD_PID=$!

echo "==> waiting for matd's socket to come up" >&2
MATD_UP=""
for _ in $(seq 1 50); do
    if ! kill -0 "$MATD_PID" 2>/dev/null; then
        echo "matd exited early:" >&2
        echo "-- stdout --" >&2
        cat "$MATD_STDOUT" >&2
        echo "-- stderr --" >&2
        cat "$MATD_STDERR" >&2
        exit 1
    fi
    if ./target/release/matd status --socket "$MATD_SOCK" >/dev/null 2>&1; then
        MATD_UP=1
        break
    fi
    sleep 0.1
done
if [[ -z "$MATD_UP" ]]; then
    echo "matd never answered on $MATD_SOCK:" >&2
    cat "$MATD_STDERR" >&2
    exit 1
fi

echo "==> waiting for matd's resident Subscribe to node $NODE_ID (established, budget ${TIMEOUT_S}s)" >&2
ESTABLISHED=""
STATUS_JSON=""
DEADLINE=$((SECONDS + TIMEOUT_S))
while ((SECONDS < DEADLINE)); do
    if ! kill -0 "$MATD_PID" 2>/dev/null; then
        echo "matd exited while waiting for the subscription:" >&2
        cat "$MATD_STDERR" >&2
        exit 1
    fi
    STATUS_JSON="$(./target/release/matd status --socket "$MATD_SOCK" 2>/dev/null)" || true
    STATE="$(matd_node_state "$STATUS_JSON" "$NODE_ID")"
    if [[ "$STATE" == "established" ]]; then
        ESTABLISHED=1
        break
    fi
    sleep 0.3
done
if [[ -z "$ESTABLISHED" ]]; then
    echo "matd status (last seen): $STATUS_JSON" >&2
    echo "timed out waiting for matd's subscription to node $NODE_ID to reach established" >&2
    exit 1
fi
echo "==> matd subscription to node $NODE_ID: established" >&2

LISTEN_TIMEOUT_MS=$((TIMEOUT_S * 1000))
echo "==> starting mat listen (matd=$MATD_SOCK, node=$NODE_ID, cluster=onoff, count=1, timeout=${LISTEN_TIMEOUT_MS}ms)" >&2
MAT_STORE="$MAT_STORE_DIR" \
    ./target/release/mat listen \
        --node "$NODE_ID" --endpoint "$DEVICE_EP" --cluster onoff --attribute on-off \
        --count 1 --timeout-ms "$LISTEN_TIMEOUT_MS" \
        --matd "$MATD_SOCK" \
        >"$LISTEN_STDOUT" 2>"$LISTEN_STDERR" &
LISTEN_PID=$!

echo "==> waiting for mat listen to attach" >&2
# `mat listen`'s stdout stays empty until the first *event* line — the
# `{"listening":true}` ack matd sends is read and discarded internally
# (`crates/mat/src/matd_client.rs`'s `cmd_listen`: "ack 行 ... は出力せず
# 読み捨てる"), never printed. So attachment is observed from matd's own
# side instead: matd subscribes this client to its event bus *before*
# sending that ack ("ack より先に subscribe" in `crates/matd/src/server.rs`),
# then logs `listen client attached` at info level — which is what
# `RUST_LOG=info` above is for.
ACK=""
for _ in $(seq 1 100); do
    if ! kill -0 "$LISTEN_PID" 2>/dev/null; then
        echo "mat listen exited before attaching to matd:" >&2
        echo "-- stdout --" >&2
        cat "$LISTEN_STDOUT" >&2
        echo "-- stderr --" >&2
        cat "$LISTEN_STDERR" >&2
        exit 1
    fi
    if grep -q "listen client attached" "$MATD_STDERR" 2>/dev/null; then
        ACK=1
        break
    fi
    sleep 0.1
done
if [[ -z "$ACK" ]]; then
    echo "mat listen never attached to matd (no \"listen client attached\" in matd's log):" >&2
    cat "$MATD_STDERR" >&2
    exit 1
fi
echo "==> mat listen attached" >&2

echo "==> mat on (routed through matd) — node=$NODE_ID endpoint=$DEVICE_EP" >&2
ON_JSON="$(
    MAT_STORE="$MAT_STORE_DIR" \
        ./target/release/mat on --node "$NODE_ID" --endpoint "$DEVICE_EP" --matd "$MATD_SOCK"
)"
echo "$ON_JSON" >&2

echo "==> waiting for mat listen (pid $LISTEN_PID) to finish (budget ${LISTEN_TIMEOUT_MS}ms)" >&2
LISTEN_EXIT=0
wait "$LISTEN_PID" || LISTEN_EXIT=$?
LISTEN_PID=""

echo "-- mat listen stdout --" >&2
cat "$LISTEN_STDOUT" >&2
if [[ "$LISTEN_EXIT" -ne 0 ]]; then
    echo "mat listen exited $LISTEN_EXIT:" >&2
    echo "-- stderr --" >&2
    cat "$LISTEN_STDERR" >&2
    exit 1
fi

# The ack is never printed (see the attach-wait comment above) — with
# --count 1, this is the one event line `mat listen` prints before exiting.
EVENT_LINE="$(head -n1 "$LISTEN_STDOUT" 2>/dev/null)"
if [[ -z "$EVENT_LINE" ]]; then
    echo "mat listen exited 0 but printed no event line" >&2
    exit 1
fi

EVT_CLUSTER="$(json_get cluster "$EVENT_LINE")"
EVT_ATTR="$(json_get attribute "$EVENT_LINE")"
EVT_VALUE="$(json_get value "$EVENT_LINE")"
EVT_NODE="$(json_get node_id "$EVENT_LINE")"
if [[ "$EVT_NODE" != "$NODE_ID" || "$EVT_CLUSTER" != "onoff" || "$EVT_ATTR" != "on-off" || "$EVT_VALUE" != "true" ]]; then
    echo "unexpected mat listen event: $EVENT_LINE" >&2
    exit 1
fi

echo "==> PASS: matd's resident Subscribe delivered the on-off=true event through mat listen: $EVENT_LINE" >&2
