#!/usr/bin/env bash
# M4 IPK rotation E2E against the virtual device — success path first, then
# the pending/abort path. `matv` (mat-device) accepts KeySetWrite on key set 0
# (multi-epoch IPK, persisted in group_keys.json) and answers CASE for every
# epoch it holds. Flow: build -> matv -> `mat fabric init` + `mat commission`
# -> `mat group provision` -> `mat fabric rotate-ipk` (exit 0, status:"rotated")
# -> `fabric list` pending:false -> `mat on` (CASE with the NEW IPK) + `mat
# group invoke` (groupcast keys untouched) -> matv restarted (proves the
# rotated IPK survived a restart) -> `mat on` again -> matv stopped -> `mat
# fabric rotate-ipk` (expect non-zero, status:"pending", node 1 failed) ->
# `fabric list` pending:true -> `--abort` (status:"aborted") -> pending:false
# -> matv restarted -> `mat on` still works (the controller never switched).
#
# Env:
#   MAT_E2E_IFACE     interface both matv's mDNS advertiser and mat's
#                      discovery use (default: `eth1` — same rationale as
#                      e2e-device-m1.sh).
#   MAT_E2E_TIMEOUT_S  seconds budgeted for `mat commission` (default: 30).
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

DEVICE_PID=""
cleanup() {
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
[[ "$(json_get status "$GROUP_JSON")" == "provisioned" ]]

assert_pending() {
    local expected="$1" json
    json="$(MAT_STORE="$MAT_STORE_DIR" ./target/release/mat fabric list)"
    echo "$json" >&2
    printf '%s' "$json" | python3 -c '
import json, sys
d = json.load(sys.stdin)
f = [x for x in d["fabrics"] if x["current"]][0]
assert f["ipk_rotation_pending"] is '"$expected"', d
'
}

# (Re)start matv on the same store and wait for its setup-payload line —
# the device keeps its fabric + rotated IPK (group_keys.json) across restarts.
start_matv() {
    : >"$DEVICE_STDOUT"
    RUST_LOG="${RUST_LOG:-info}" \
        ./target/release/matv --config "$MATV_CONFIG" \
        >"$DEVICE_STDOUT" 2>>"$DEVICE_STDERR" &
    DEVICE_PID=$!
    for _ in $(seq 1 50); do
        [[ -n "$(head -n1 "$DEVICE_STDOUT" 2>/dev/null)" ]] && return 0
        kill -0 "$DEVICE_PID" 2>/dev/null || break
        sleep 0.1
    done
    echo "matv did not come back:" >&2
    cat "$DEVICE_STDERR" >&2
    exit 1
}

stop_matv() {
    kill "$DEVICE_PID" 2>/dev/null || true
    wait "$DEVICE_PID" 2>/dev/null || true
    DEVICE_PID=""
}

echo "==> mat fabric rotate-ipk (expect rotated: matv accepts KeySetWrite(0))" >&2
ROTATE_JSON="$(MAT_STORE="$MAT_STORE_DIR" ./target/release/mat --iface "$IFACE" fabric rotate-ipk)"
echo "$ROTATE_JSON"
printf '%s' "$ROTATE_JSON" | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert d["status"] == "rotated", d
n = d["nodes"][0]
assert n["node_id"] == '"$NODE_ID"' and n["status"] == "ok", d
'
assert_pending False
echo "==> PASS: rotate-ipk committed" >&2

echo "==> CASE with the new IPK (mat on) + groupcast keys untouched (group invoke)" >&2
MAT_STORE="$MAT_STORE_DIR" ./target/release/mat --iface "$IFACE" on --node "$NODE_ID" --endpoint "$DEVICE_EP" >&2
MAT_STORE="$MAT_STORE_DIR" ./target/release/mat --iface "$IFACE" group invoke -g "$GROUP_ID" -c onoff --command off -e "$DEVICE_EP" >&2
sleep 1
READ_JSON="$(MAT_STORE="$MAT_STORE_DIR" ./target/release/mat --iface "$IFACE" read --node "$NODE_ID" --endpoint "$DEVICE_EP" --cluster onoff --attribute on-off)"
[[ "$(json_get value "$READ_JSON")" == "false" ]] || { echo "groupcast off did not land after rotation: $READ_JSON" >&2; exit 1; }
echo "==> PASS: unicast on the new IPK + groupcast after rotation" >&2

echo "==> restart matv: the rotated IPK must survive (group_keys.json)" >&2
stop_matv
start_matv
MAT_STORE="$MAT_STORE_DIR" ./target/release/mat --iface "$IFACE" on --node "$NODE_ID" --endpoint "$DEVICE_EP" >&2
echo "==> PASS: CASE with the rotated IPK after a device restart" >&2

echo "==> stop matv, rotate again (expect pending: node unreachable)" >&2
stop_matv
set +e
ROTATE_JSON="$(MAT_STORE="$MAT_STORE_DIR" MAT_OP_TIMEOUT_MS=8000 ./target/release/mat --iface "$IFACE" fabric rotate-ipk 2>"$WORKDIR/rotate.stderr")"
ROTATE_RC=$?
set -e
echo "$ROTATE_JSON"
cat "$WORKDIR/rotate.stderr" >&2
[[ "$ROTATE_RC" != "0" ]] || { echo "expected a non-zero exit for a pending rotation" >&2; exit 1; }
printf '%s' "$ROTATE_JSON" | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert d["status"] == "pending", d
n = d["nodes"][0]
assert n["node_id"] == '"$NODE_ID"' and n["status"] == "failed", d
'
assert_pending True
echo "==> PASS: rotate-ipk ended pending with node $NODE_ID failed" >&2

echo "==> mat fabric rotate-ipk --abort" >&2
ABORT_JSON="$(MAT_STORE="$MAT_STORE_DIR" ./target/release/mat --iface "$IFACE" fabric rotate-ipk --abort)"
echo "$ABORT_JSON"
[[ "$(json_get status "$ABORT_JSON")" == "aborted" ]]
assert_pending False

echo "==> restart matv: the controller never switched, so the node is still reachable" >&2
start_matv
MAT_STORE="$MAT_STORE_DIR" ./target/release/mat --iface "$IFACE" on --node "$NODE_ID" --endpoint "$DEVICE_EP" >&2
echo "==> PASS: abort cleared pending; node still reachable" >&2
echo "==> ALL PASS (m4: ipk rotation success + pending/abort paths against matv)" >&2
