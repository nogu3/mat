#!/usr/bin/env bash
# M3 matd x matv regression E2E — the real motivation behind audit backlog
# item ③ (2026-08-31): "matd resident Subscribe + `mat listen` against a
# virtual device", run with the real `matd` and `mat` binaries against the
# real `matv` device host (no mocks on either side of the socket).
#
# Flow: build the workspace (release) -> run `matv` (EP1=Aggregator per
# `mat-device`'s bridge topology, then one endpoint per `[[device]]` in
# declaration order: EP2=the onoff light, EP3=a Generic Switch, EP4=a
# contact sensor; `--stdin-control` with stdin on a fifo the script holds
# open, so the two event-emitting devices can be stimulated on demand) in
# the background -> `mat fabric init` + `mat commission` into a throwaway
# store (same as e2e-device-m1.sh) -> `mat describe`, asserting the
# endpoint ledger (light on $DEVICE_EP, switch/booleanstate endpoints read
# off the wire rather than hard-coded) -> `mat group provision` against the
# commissioned node, asserting `status:"provisioned"` (the KeySetWrite +
# group-key-map write + AddGroup + ACL write sequence actually lands on
# matv) -> `mat group list`, asserting the provisioned group shows up in the
# controller kvs -> `mat group remove` (asserting all four removal steps
# landed on matv and no groups or non-IPK keysets remain in the controller
# kvs — keyset 0 itself may or may not be visible in that chain) ->
# `mat group provision` again -> write `<store>/subscriptions.toml` with
# `events = ["switch", "booleanstate"]` and *no* `clusters` key (attributes
# stay full wildcard; this also exercises matd accepting an events-only
# config) -> start `matd` against the same store, poll
# `matd status` until its
# resident wildcard Subscribe to node 1 reaches `state:"established"` ->
# start `mat listen --count 1` in the background -> `mat on` (routed through
# matd) -> assert the backgrounded `mat listen` received an onoff on-off=true
# event before its budget ran out -> three event-subscription legs:
#
#   leg A: `mat listen --cluster switch --event --count 2` + a short press on
#          the Generic Switch -> `initial-press` then `short-release`, in
#          ascending EventNumber, `priming:false`, no `attribute` key.
#   leg B: `mat listen --cluster booleanstate --count 2` + a contact-sensor
#          close -> both shapes for the one transition: the attribute line
#          (`state-value` = true) and the event line (`state-change` with
#          `data.state-value` = true), in either order.
#   leg C: EventMin recovery. A direct-path op with *no* `MAT_MATD_SOCKET`
#          (so no `node_touched` hint) evicts matd's subscribe session
#          without matd noticing -> the contact sensor opens during that
#          blind window (nobody is subscribed) -> a `mat listen` client
#          attaches -> a second direct-path op *with* `MAT_MATD_SOCKET`
#          sends the hint, matd re-subscribes with `EventMin = last + 1`,
#          and the blind-window event comes back inside the priming payload
#          as `priming:false` (matd re-checks the number itself rather than
#          trusting the device's EventFilters). matd's "subscription
#          established" log line must carry `event_min = Some(..)` for that
#          attempt.
#
# There is deliberately no "restart matd" leg: a fresh matd has no last
# EventNumber, so it subscribes without EventMin and every priming event is
# `priming:true` by design — nothing to assert about recovery there.
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
#                      30). Keep it well under ~90 s: leg C's blind window
#                      only holds while it stays shorter than matd's silence
#                      deadline (max_interval 60 s + slack), otherwise matd
#                      re-subscribes on its own mid-window and the recovery
#                      event is delivered to nobody.
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
# events 脚（A/B/C）はそれぞれ別の `mat listen` を起こすので、行の混線を避ける
# ために脚ごとにログを分ける。
LISTEN_A_STDOUT="$WORKDIR/listen-a.stdout.log"
LISTEN_A_STDERR="$WORKDIR/listen-a.stderr.log"
LISTEN_B_STDOUT="$WORKDIR/listen-b.stdout.log"
LISTEN_B_STDERR="$WORKDIR/listen-b.stderr.log"
LISTEN_C_STDOUT="$WORKDIR/listen-c.stdout.log"
LISTEN_C_STDERR="$WORKDIR/listen-c.stderr.log"
# matv の `--stdin-control` へ刺激（ボタン押下 / 接点開閉）を流す名前付きパイプ。
MATV_STDIN="$WORKDIR/matv.stdin"
SUBSCRIPTIONS_TOML="$MAT_STORE_DIR/subscriptions.toml"
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

# matd の node $NODE_ID への常駐 Subscribe が established になるまで待つ
# （budget TIMEOUT_S 秒）。起動直後と、直経路 op が matv の唯一 session を奪った
# 後の再確立の両方で使う。「落ちてから戻った」ことの証明にはならない点に注意
# （matd は無音 deadline まで established を報告し続ける）— 張り直しの実証は
# matd stderr の "subscription transport bound" の増加を数える（回転後の段）。
wait_matd_established() {
    local why="$1"
    echo "==> waiting for matd's resident Subscribe to node $NODE_ID ($why; established, budget ${TIMEOUT_S}s)" >&2
    local established="" status_json="" state deadline=$((SECONDS + TIMEOUT_S))
    while ((SECONDS < deadline)); do
        if ! kill -0 "$MATD_PID" 2>/dev/null; then
            echo "matd exited while waiting for the subscription:" >&2
            cat "$MATD_STDERR" >&2
            exit 1
        fi
        status_json="$(./target/release/matd status --socket "$MATD_SOCK" 2>/dev/null)" || true
        state="$(matd_node_state "$status_json" "$NODE_ID")"
        if [[ "$state" == "established" ]]; then
            established=1
            break
        fi
        sleep 0.3
    done
    if [[ -z "$established" ]]; then
        echo "matd status (last seen): $status_json" >&2
        echo "timed out waiting for matd's subscription to node $NODE_ID to reach established ($why)" >&2
        exit 1
    fi
    echo "==> matd subscription to node $NODE_ID: established ($why)" >&2
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
    # fifo の書き手（`exec 3<>` で開きっぱなしにしている fd）を閉じる。まだ
    # 開いていない段（mkfifo より前の失敗）で呼ばれても害が無いよう握り潰す。
    exec 3>&- 2>/dev/null || true
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

# 宣言順 = endpoint 採番順。light を先頭に残すことで EP2 = light（既存脚の
# DEVICE_EP）が動かない。以降の 2 台は events 脚（イベント購読）用で、
# どちらも --stdin-control の刺激で動く。
# （この heredoc は unquoted なので、コメントにもバッククォート・$ を書かない。）
[[device]]
id = "btn"
kind = "switch"
name = "E2E Button"

[[device]]
id = "door"
kind = "contact-sensor"
name = "E2E Door"
EOF

# 刺激の投入口。`3<>`（read/write）で開くのは、`3>`（write only）だと読み手が
# 現れるまで open がブロックしてしまうため — この fd はスクリプトが最後まで
# 握り続けるので matv の stdin に EOF が来ず、`--stdin-control` のフックが
# 途中で畳まれることもない（cleanup で閉じる）。
mkfifo "$MATV_STDIN"
exec 3<>"$MATV_STDIN"

echo "==> starting matv (iface=$IFACE, store=$DEVICE_STORE, stdin-control)" >&2
RUST_LOG="${RUST_LOG:-info}" \
    ./target/release/matv --config "$MATV_CONFIG" --stdin-control \
    <"$MATV_STDIN" >"$DEVICE_STDOUT" 2>"$DEVICE_STDERR" &
DEVICE_PID=$!

# fifo に刺激 1 行を書き、matv が `applied` 行を stdout に返すまで待つ
# （投入の着地を確認してから購読側の assert に進むため）。
send_stimulus() {
    local line="$1" before after deadline
    before=$(grep -c '"applied"' "$DEVICE_STDOUT" || true)
    printf '%s\n' "$line" >&3
    deadline=$((SECONDS + TIMEOUT_S))
    while ((SECONDS < deadline)); do
        after=$(grep -c '"applied"' "$DEVICE_STDOUT" || true)
        if ((after > before)); then
            echo "==> stimulus applied: $line" >&2
            return 0
        fi
        sleep 0.1
    done
    echo "matv never applied the stimulus $line (budget ${TIMEOUT_S}s):" >&2
    echo "-- matv stdout --" >&2; tail -n 20 "$DEVICE_STDOUT" >&2
    echo "-- matv stderr --" >&2; tail -n 40 "$DEVICE_STDERR" >&2
    exit 1
}

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

# endpoint 採番は `[[device]]` の宣言順という規約だが、決め打ちせずに
# `mat describe` の server-list から引く（規約が変わったら数値ではなく
# ここが落ちる）。cluster 6 = onoff、59 = switch、69 = booleanstate。
echo "==> mat describe (endpoint ledger: light / btn / door)" >&2
DESCRIBE_JSON="$(
    MAT_STORE="$MAT_STORE_DIR" \
        ./target/release/mat --iface "$IFACE" describe --node "$NODE_ID"
)"
echo "$DESCRIBE_JSON" >&2
ENDPOINTS="$(printf '%s' "$DESCRIBE_JSON" | python3 -c '
import json, sys
d = json.load(sys.stdin)
want = [(6, "light"), (59, "btn"), (69, "door")]
found = {}
for ep in d["endpoints"]:
    for cluster_id, name in want:
        if cluster_id in ep["clusters"]:
            assert name not in found, ("two endpoints claim " + name, d)
            found[name] = ep["endpoint"]
missing = [name for _, name in want if name not in found]
assert not missing, ("no endpoint serves " + ",".join(missing), d)
print(found["light"], found["btn"], found["door"])
')"
read -r LIGHT_EP BTN_EP DOOR_EP <<<"$ENDPOINTS"
[[ "$LIGHT_EP" == "$DEVICE_EP" ]] || {
    echo "the onoff light moved off endpoint $DEVICE_EP (describe says $LIGHT_EP): $DESCRIBE_JSON" >&2
    exit 1
}
echo "==> PASS: endpoints — light=$LIGHT_EP btn=$BTN_EP door=$DOOR_EP" >&2

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

# 撤収 4 ステップ（ACL Group エントリ除去 → RemoveGroup → group-key-map 除去 →
# 未参照 keyset の KeySetRemove）が全部 matv に着地することを assert する。
# matv の KeySetRemove は 2026-09-05 に実装（それ以前は IM 0x81 で保留していた）。
echo "==> mat group remove (group=$GROUP_ID, node=$NODE_ID, endpoint=$DEVICE_EP)" >&2
REMOVE_JSON="$(
    MAT_STORE="$MAT_STORE_DIR" \
        ./target/release/mat --iface "$IFACE" group remove \
            --group "$GROUP_ID" --nodes "$NODE_ID" --endpoint "$DEVICE_EP"
)"
echo "$REMOVE_JSON"
[[ "$(json_get status "$REMOVE_JSON")" == "removed" ]]
printf '%s' "$REMOVE_JSON" | python3 -c '
import json, sys
d = json.load(sys.stdin)
n = d["nodes"][0]
assert n["node_id"] == '"$NODE_ID"', d
for k in ("acl_removed", "group_removed", "keymap_removed", "keyset_removed"):
    assert n[k] is True, (k, d)
assert d["controller"]["group_removed"] is True, d
'
echo "==> PASS: mat group remove reached status=removed (ACL / RemoveGroup / group-key-map / KeySetRemove all landed on matv)" >&2

echo "==> mat group list after remove (controller kvs: no groups, no non-IPK keysets remain (the IPK itself may or may not be visible in this chain))" >&2
LIST_JSON="$(MAT_STORE="$MAT_STORE_DIR" ./target/release/mat group list)"
echo "$LIST_JSON" >&2
printf '%s' "$LIST_JSON" | python3 -c '
import json, sys
d = json.load(sys.stdin)
assert d["groups"] == [], d
assert all(k["keyset_id"] == 0 for k in d["keysets"]), d
'

# 以降の groupcast / matd / listen 脚のために provision し直す。
echo "==> mat group provision again (group=$GROUP_ID)" >&2
GROUP_JSON="$(
    MAT_STORE="$MAT_STORE_DIR" \
        ./target/release/mat --iface "$IFACE" group provision \
            --group "$GROUP_ID" --nodes "$NODE_ID" --endpoint "$DEVICE_EP" --name e2e-group
)"
echo "$GROUP_JSON"
[[ "$(json_get status "$GROUP_JSON")" == "provisioned" ]]
echo "==> PASS: re-provision after remove reached status=provisioned" >&2

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

# イベント購読の範囲設定。`clusters` キーを **書かない** = 属性は full wildcard
# のまま（既存の onoff listen 脚は無改変で通る）。`events` だけの config を matd
# が受理することの実走確認も兼ねる。
cat >"$SUBSCRIPTIONS_TOML" <<'EOF'
events = ["switch", "booleanstate"]
EOF

echo "==> starting matd (store=$MAT_STORE_DIR, iface=$IFACE, socket=$MATD_SOCK, subscriptions.toml: events-only)" >&2
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

wait_matd_established "start-up"

# `reloads.count` を status JSON から取る（jq 無し環境向けに python3 でも）。
matd_reload_count() {
    printf '%s' "$1" | python3 -c '
import json, sys
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(0)
print((d.get("reloads") or {}).get("count", ""))
'
}

echo "==> matd reload (same store: expect ipk=unchanged, reload_count=1, subscription untouched)" >&2
RELOAD_JSON="$(./target/release/matd reload --socket "$MATD_SOCK")"
echo "$RELOAD_JSON"
# json_get は jq なら `true`、python3 フォールバックなら `True` を出す — 両方を受ける。
[[ "$(json_get reloaded "$RELOAD_JSON")" =~ ^[Tt]rue$ ]] || { echo "reload not acked: $RELOAD_JSON" >&2; exit 1; }
[[ "$(json_get ipk "$RELOAD_JSON")" == "unchanged" ]] || { echo "ipk should be unchanged: $RELOAD_JSON" >&2; exit 1; }
[[ "$(json_get reload_count "$RELOAD_JSON")" == "1" ]] || { echo "reload_count should be 1: $RELOAD_JSON" >&2; exit 1; }
STATUS_JSON="$(./target/release/matd status --socket "$MATD_SOCK")"
[[ "$(matd_reload_count "$STATUS_JSON")" == "1" ]] || { echo "status.reloads.count should be 1: $STATUS_JSON" >&2; exit 1; }
[[ "$(matd_node_state "$STATUS_JSON" "$NODE_ID")" == "established" ]] || { echo "reload must not touch the subscription: $STATUS_JSON" >&2; exit 1; }
echo "==> PASS: matd reload (unchanged) kept the subscription up" >&2

# 回転前の「購読用 CASE が成立した回数」。回転後に 1 本増えることが、reload 済み
# の新 IPK で張り直せた証拠になる（rotate-ipk は node_touched を撃たないので、
# この時点から回転完了までに増えることは無い）。
SUBS_BEFORE=$(grep -c "subscription transport bound" "$MATD_STDERR" || true)

echo "==> mat fabric rotate-ipk (direct path; matv accepts KeySetWrite(0)) — expect matd_reload=reloaded" >&2
ROTATE_JSON="$(MAT_STORE="$MAT_STORE_DIR" MAT_MATD_SOCKET="$MATD_SOCK" ./target/release/mat --iface "$IFACE" fabric rotate-ipk)"
echo "$ROTATE_JSON"
[[ "$(json_get status "$ROTATE_JSON")" == "rotated" ]] || { echo "rotate-ipk did not commit: $ROTATE_JSON" >&2; exit 1; }
[[ "$(json_get matd_reload "$ROTATE_JSON")" == "reloaded" ]] || { echo "matd_reload should be reloaded: $ROTATE_JSON" >&2; exit 1; }
STATUS_JSON="$(./target/release/matd status --socket "$MATD_SOCK")"
[[ "$(matd_reload_count "$STATUS_JSON")" == "2" ]] || { echo "status.reloads.count should be 2 after rotate: $STATUS_JSON" >&2; exit 1; }
echo "==> PASS: rotate-ipk committed and matd reloaded the new IPK (count=2)" >&2

# rotate の proof CASE が matv の唯一 session を奪うので購読は一度落ちる。ただ
# matd がそれを知るのは無音 deadline なので、直後の `matd status` は established
# のままで、購読の張り直しの証拠にならない（warm op session 経由の `mat on` も
# 同じ）。直経路 op を 1 本撃って node_touched ヒントを送り、matd に即時の
# 再購読をさせ、"subscription transport bound"（= 購読専用 CASE の新規成立）が
# 1 本増えることを確かめる。この CASE は reload 済みの新 IPK で張られる。
echo "==> direct-path op (node_touched hint) to make matd resubscribe now (subscription CASEs so far: $SUBS_BEFORE)" >&2
MAT_STORE="$MAT_STORE_DIR" MAT_MATD=0 MAT_MATD_SOCKET="$MATD_SOCK" \
    ./target/release/mat --iface "$IFACE" on --node "$NODE_ID" --endpoint "$DEVICE_EP" >&2
SUBS_AFTER="$SUBS_BEFORE"
SUBS_DEADLINE=$((SECONDS + TIMEOUT_S))
while ((SECONDS < SUBS_DEADLINE)); do
    SUBS_AFTER=$(grep -c "subscription transport bound" "$MATD_STDERR" || true)
    if ((SUBS_AFTER > SUBS_BEFORE)); then
        break
    fi
    sleep 0.3
done
if ! ((SUBS_AFTER > SUBS_BEFORE)); then
    echo "matd never established a fresh subscription CASE after the rotation" >&2
    echo "(\"subscription transport bound\" count stuck at $SUBS_BEFORE, budget ${TIMEOUT_S}s)" >&2
    tail -n 80 "$MATD_STDERR" >&2
    exit 1
fi
echo "==> subscription CASEs after the rotation: $SUBS_AFTER (was $SUBS_BEFORE)" >&2
echo "==> PASS: matd re-established its subscription after the rotation (fresh CASE on the reloaded IPK)" >&2

echo "==> mat on via matd after the rotation (matd must establish with the new IPK)" >&2
ON_JSON="$(MAT_STORE="$MAT_STORE_DIR" ./target/release/mat on --node "$NODE_ID" --endpoint "$DEVICE_EP" --matd "$MATD_SOCK")"
echo "$ON_JSON" >&2
MAT_STORE="$MAT_STORE_DIR" ./target/release/mat off --node "$NODE_ID" --endpoint "$DEVICE_EP" --matd "$MATD_SOCK" >&2
echo "==> PASS: unicast through matd after the rotation" >&2
wait_matd_established "after post-rotate ops"

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

# ---------------------------------------------------------------------------
# events legs — matv の刺激 → matd の常駐 Subscribe（EventRequests 付き） →
# `mat listen --event`。上の onoff 脚と同じ matd / 同じ購読を使い回す。
# ---------------------------------------------------------------------------

# `mat listen` が matd の event bus に繋がるまで待つ（上の onoff 脚と同じ
# 「matd 側から観測する」やり方 — ack 行は出力されないため）。脚を跨いで
# 使うので、開始前の "listen client attached" 件数からの増加で判定する。
wait_listen_attached() {
    local before="$1" pid="$2" out="$3" err="$4" after deadline
    deadline=$((SECONDS + TIMEOUT_S))
    while ((SECONDS < deadline)); do
        if ! kill -0 "$pid" 2>/dev/null; then
            echo "mat listen exited before attaching to matd:" >&2
            echo "-- stdout --" >&2; cat "$out" >&2
            echo "-- stderr --" >&2; cat "$err" >&2
            exit 1
        fi
        after=$(grep -c "listen client attached" "$MATD_STDERR" || true)
        if ((after > before)); then
            echo "==> mat listen attached (attached clients so far: $after)" >&2
            return 0
        fi
        sleep 0.1
    done
    echo "mat listen never attached to matd (\"listen client attached\" stuck at $before, budget ${TIMEOUT_S}s):" >&2
    tail -n 60 "$MATD_STDERR" >&2
    exit 1
}

# JSON 形状の assert が落ちたときの診断出力。cleanup が WORKDIR を無条件に
# 消すので、ここで matd / matv のログ末尾を出しておかないとイベント購読が
# 壊れたときの手がかりが永久に失われる（他の失敗経路と同じ扱いに揃える）。
fail_leg() {
    local leg="$1" out="$2" err="$3"
    echo "==> FAIL: events $leg — JSON shape assertion failed (python traceback above)" >&2
    echo "-- mat listen stdout --" >&2; cat "$out" >&2 || true
    echo "-- mat listen stderr --" >&2; tail -n 40 "$err" >&2 || true
    echo "-- matd stderr tail --" >&2; tail -n 40 "$MATD_STDERR" >&2 || true
    echo "-- matv stderr tail --" >&2; tail -n 40 "$DEVICE_STDERR" >&2 || true
    exit 1
}

# 背景の `mat listen`（$LISTEN_PID）の終了を待ち、stdout を出して 0 終了を確かめる。
finish_listen() {
    local out="$1" err="$2" code=0
    echo "==> waiting for mat listen (pid $LISTEN_PID) to finish (budget ${LISTEN_TIMEOUT_MS}ms)" >&2
    wait "$LISTEN_PID" || code=$?
    LISTEN_PID=""
    echo "-- mat listen stdout --" >&2
    cat "$out" >&2
    if ((code != 0)); then
        echo "mat listen exited $code:" >&2
        echo "-- stderr --" >&2; cat "$err" >&2
        echo "-- matd stderr tail --" >&2; tail -n 60 "$MATD_STDERR" >&2
        exit 1
    fi
}

echo "==> events leg A: mat listen --cluster switch --event --count 2 (short press on btn, endpoint $BTN_EP)" >&2
ATTACH_BEFORE=$(grep -c "listen client attached" "$MATD_STDERR" || true)
MAT_STORE="$MAT_STORE_DIR" \
    ./target/release/mat listen \
        --node "$NODE_ID" --cluster switch --event \
        --count 2 --timeout-ms "$LISTEN_TIMEOUT_MS" \
        --matd "$MATD_SOCK" \
        >"$LISTEN_A_STDOUT" 2>"$LISTEN_A_STDERR" &
LISTEN_PID=$!
wait_listen_attached "$ATTACH_BEFORE" "$LISTEN_PID" "$LISTEN_A_STDOUT" "$LISTEN_A_STDERR"
send_stimulus '{"device":"btn","press":"short"}'
finish_listen "$LISTEN_A_STDOUT" "$LISTEN_A_STDERR"
if ! python3 - "$LISTEN_A_STDOUT" "$NODE_ID" "$BTN_EP" <<'PY'
import json, sys
path, node_id, endpoint = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
lines = [json.loads(l) for l in open(path) if l.strip()]
assert len(lines) == 2, lines
first, second = lines
for e in lines:
    assert e["node_id"] == node_id, e
    assert e["endpoint"] == endpoint, e
    assert e["cluster"] == "switch", e
    assert e["priority"] == "info", e
    assert e["priming"] is False, e
    # イベント行に属性側のキーは付かない（欠落回収は EventNumber で行う）。
    assert "attribute" not in e, e
    assert "recovered" not in e, e
assert first["event"] == "initial-press", first
assert first["data"] == {"new-position": 1}, first
assert second["event"] == "short-release", second
assert second["data"] == {"previous-position": 1}, second
assert second["event_number"] > first["event_number"], lines
PY
then
    fail_leg "leg A" "$LISTEN_A_STDOUT" "$LISTEN_A_STDERR"
fi
echo "==> PASS: leg A — switch initial-press + short-release in ascending EventNumber (priming:false)" >&2

echo "==> events leg B: mat listen --cluster booleanstate --count 2 (door closes, endpoint $DOOR_EP)" >&2
ATTACH_BEFORE=$(grep -c "listen client attached" "$MATD_STDERR" || true)
MAT_STORE="$MAT_STORE_DIR" \
    ./target/release/mat listen \
        --node "$NODE_ID" --cluster booleanstate \
        --count 2 --timeout-ms "$LISTEN_TIMEOUT_MS" \
        --matd "$MATD_SOCK" \
        >"$LISTEN_B_STDOUT" 2>"$LISTEN_B_STDERR" &
LISTEN_PID=$!
wait_listen_attached "$ATTACH_BEFORE" "$LISTEN_PID" "$LISTEN_B_STDOUT" "$LISTEN_B_STDERR"
send_stimulus '{"device":"door","state":true}'
finish_listen "$LISTEN_B_STDOUT" "$LISTEN_B_STDERR"
if ! python3 - "$LISTEN_B_STDOUT" "$NODE_ID" "$DOOR_EP" <<'PY'
import json, sys
path, node_id, endpoint = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
lines = [json.loads(l) for l in open(path) if l.strip()]
assert len(lines) == 2, lines
# 1 つの遷移が属性行とイベント行の両方で届く（順不同）。
attrs = [e for e in lines if "attribute" in e]
events = [e for e in lines if "event" in e]
assert len(attrs) == 1 and len(events) == 1, lines
a, ev = attrs[0], events[0]
for e in lines:
    assert e["node_id"] == node_id, e
    assert e["endpoint"] == endpoint, e
    assert e["cluster"] == "booleanstate", e
assert a["attribute"] == "state-value", a
assert a["value"] is True, a
assert ev["event"] == "state-change", ev
assert ev["data"] == {"state-value": True}, ev
assert ev["priming"] is False, ev
PY
then
    fail_leg "leg B" "$LISTEN_B_STDOUT" "$LISTEN_B_STDERR"
fi
echo "==> PASS: leg B — one booleanstate transition delivered as both an attribute line and a state-change event line" >&2

# 脚 C: EventMin 回収。matd が「購読が死んだ」ことを知らないまま実イベントが
# 起きる盲目窓を作り、再購読時の EventFilters（EventMin = last + 1）でそれが
# 回収されること（= priming の全量返しではなく実イベント）を確かめる。
echo "==> events leg C: EventMin recovery across a blind window" >&2
EVENTMIN_BEFORE=$(grep -cE "event_min ?= ?Some\(" "$MATD_STDERR" || true)
SUBS_BEFORE=$(grep -c "subscription transport bound" "$MATD_STDERR" || true)

# MAT_MATD_SOCKET を **渡さない** 直経路 op。matv の唯一の CASE セッション
# （= matd の購読）を奪うが、node_touched ヒントは飛ばないので matd は気づか
# ない（無音 deadline は max_interval + slack ＝ ずっと先）。ここから盲目窓。
echo "==> evicting matd's subscribe session silently (direct-path op, no node_touched hint)" >&2
MAT_STORE="$MAT_STORE_DIR" MAT_MATD=0 \
    ./target/release/mat --iface "$IFACE" on --node "$NODE_ID" --endpoint "$DEVICE_EP" >&2

# 盲目窓の中で起きる実イベント（購読者はゼロ、matv のイベントログにだけ残る）。
send_stimulus '{"device":"door","state":false}'

# 回収先の listen を先に繋いでおく（再購読の priming より後に繋ぐと取り逃す）。
ATTACH_BEFORE=$(grep -c "listen client attached" "$MATD_STDERR" || true)
MAT_STORE="$MAT_STORE_DIR" \
    ./target/release/mat listen \
        --node "$NODE_ID" --cluster booleanstate --event state-change \
        --count 1 --timeout-ms "$LISTEN_TIMEOUT_MS" \
        --matd "$MATD_SOCK" \
        >"$LISTEN_C_STDOUT" 2>"$LISTEN_C_STDERR" &
LISTEN_PID=$!
wait_listen_attached "$ATTACH_BEFORE" "$LISTEN_PID" "$LISTEN_C_STDOUT" "$LISTEN_C_STDERR"

# ここで初めて matd に「セッションが塗り替えられた」と教える（MAT_MATD_SOCKET
# 付き = node_touched ヒント、rotate 脚と同じ撃ち方）。この op 自身のセッション
# は mat の終了で閉じるので、matd の再購読を後から奪うものはもう無い。
echo "==> direct-path op *with* MAT_MATD_SOCKET (node_touched hint) — matd must resubscribe with EventMin" >&2
MAT_STORE="$MAT_STORE_DIR" MAT_MATD=0 MAT_MATD_SOCKET="$MATD_SOCK" \
    ./target/release/mat --iface "$IFACE" on --node "$NODE_ID" --endpoint "$DEVICE_EP" >&2

finish_listen "$LISTEN_C_STDOUT" "$LISTEN_C_STDERR"
if ! python3 - "$LISTEN_C_STDOUT" "$NODE_ID" "$DOOR_EP" <<'PY'
import json, sys
path, node_id, endpoint = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
lines = [json.loads(l) for l in open(path) if l.strip()]
assert len(lines) == 1, lines
ev = lines[0]
assert ev["node_id"] == node_id, ev
assert ev["endpoint"] == endpoint, ev
assert ev["cluster"] == "booleanstate", ev
assert ev["event"] == "state-change", ev
assert ev["data"] == {"state-value": False}, ev
# 盲目窓中の実イベントとして回収された証拠（priming の全量返しなら true）。
assert ev["priming"] is False, ev
assert "attribute" not in ev, ev
PY
then
    fail_leg "leg C" "$LISTEN_C_STDOUT" "$LISTEN_C_STDERR"
fi

SUBS_AFTER=$(grep -c "subscription transport bound" "$MATD_STDERR" || true)
if ! ((SUBS_AFTER > SUBS_BEFORE)); then
    echo "matd never established a fresh subscription CASE for the recovery leg (stuck at $SUBS_BEFORE)" >&2
    tail -n 80 "$MATD_STDERR" >&2
    exit 1
fi
EVENTMIN_AFTER=$(grep -cE "event_min ?= ?Some\(" "$MATD_STDERR" || true)
if ! ((EVENTMIN_AFTER > EVENTMIN_BEFORE)); then
    echo "matd's \"subscription established\" line never carried event_min = Some(..) (stuck at $EVENTMIN_BEFORE)" >&2
    grep "subscription established" "$MATD_STDERR" >&2 || true
    exit 1
fi
grep -E "event_min ?= ?Some\(" "$MATD_STDERR" | tail -n1 >&2
echo "==> PASS: leg C — the blind-window state-change(false) came back through EventMin recovery as priming:false (subscription CASEs $SUBS_BEFORE -> $SUBS_AFTER)" >&2
