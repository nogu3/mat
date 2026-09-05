#!/usr/bin/env bash
# M4 IPK rotation E2E against the virtual device — pins the *failure* path,
# because `matv` (mat-device) does not accept KeySetWrite on key set 0 yet
# (INVALID_COMMAND) and holds a single epoch key. Flow: build -> matv ->
# `mat fabric init` + `mat commission` -> `mat group provision` ->
# `mat fabric rotate-ipk` (expect exit 4 = device_rejected, stdout body
# status:"pending", node 1 failed with kind device_rejected) -> `mat fabric
# list` shows ipk_rotation_pending:true -> `mat on` and `mat group invoke`
# still work (the controller did NOT switch epochs) -> `mat fabric rotate-ipk
# --abort` (status:"aborted") -> `fabric list` pending:false -> `mat on` again.
#
# When matv learns KeySetWrite(0) (multi-epoch IPK), flip the expectations:
# rotate-ipk exits 0 with status:"rotated", and `mat on` afterwards proves
# CASE with the new IPK.
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

echo "==> mat fabric rotate-ipk (expect pending: matv rejects KeySetWrite(0))" >&2
set +e
ROTATE_JSON="$(MAT_STORE="$MAT_STORE_DIR" ./target/release/mat --iface "$IFACE" fabric rotate-ipk 2>"$WORKDIR/rotate.stderr")"
ROTATE_RC=$?
set -e
echo "$ROTATE_JSON"
cat "$WORKDIR/rotate.stderr" >&2
[[ "$ROTATE_RC" == "4" ]] || { echo "expected exit 4 (device_rejected), got $ROTATE_RC" >&2; exit 1; }
printf '%s' "$ROTATE_JSON" | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert d["status"] == "pending", d
n = d["nodes"][0]
assert n["node_id"] == '"$NODE_ID"' and n["status"] == "failed", d
assert n["error"]["kind"] == "device_rejected", d
'
grep -q '"kind":"device_rejected"' "$WORKDIR/rotate.stderr"
echo "==> PASS: rotate-ipk ended pending with device_rejected on node $NODE_ID" >&2

echo "==> fabric list shows ipk_rotation_pending:true" >&2
LIST_JSON="$(MAT_STORE="$MAT_STORE_DIR" ./target/release/mat fabric list)"
echo "$LIST_JSON" >&2
printf '%s' "$LIST_JSON" | python3 -c '
import json, sys
d = json.load(sys.stdin)
f = [x for x in d["fabrics"] if x["current"]][0]
assert f["ipk_rotation_pending"] is True, d
'

echo "==> controller still on the old IPK: mat on + group invoke keep working" >&2
MAT_STORE="$MAT_STORE_DIR" ./target/release/mat --iface "$IFACE" on --node "$NODE_ID" --endpoint "$DEVICE_EP" >&2
MAT_STORE="$MAT_STORE_DIR" ./target/release/mat --iface "$IFACE" group invoke -g "$GROUP_ID" -c onoff --command off -e "$DEVICE_EP" >&2
sleep 1
READ_JSON="$(MAT_STORE="$MAT_STORE_DIR" ./target/release/mat --iface "$IFACE" read --node "$NODE_ID" --endpoint "$DEVICE_EP" --cluster onoff --attribute on-off)"
[[ "$(json_get value "$READ_JSON")" == "false" ]] || { echo "groupcast off did not land after pending rotation: $READ_JSON" >&2; exit 1; }
echo "==> PASS: unicast + groupcast unaffected by a pending rotation" >&2

echo "==> mat fabric rotate-ipk --abort" >&2
ABORT_JSON="$(MAT_STORE="$MAT_STORE_DIR" ./target/release/mat --iface "$IFACE" fabric rotate-ipk --abort)"
echo "$ABORT_JSON"
[[ "$(json_get status "$ABORT_JSON")" == "aborted" ]]
LIST_JSON="$(MAT_STORE="$MAT_STORE_DIR" ./target/release/mat fabric list)"
printf '%s' "$LIST_JSON" | python3 -c '
import json, sys
d = json.load(sys.stdin)
f = [x for x in d["fabrics"] if x["current"]][0]
assert f["ipk_rotation_pending"] is False, d
'
MAT_STORE="$MAT_STORE_DIR" ./target/release/mat --iface "$IFACE" on --node "$NODE_ID" --endpoint "$DEVICE_EP" >&2
echo "==> PASS: abort cleared pending; node still reachable" >&2
echo "==> ALL PASS (m4: ipk rotation failure path against matv)" >&2
