#!/usr/bin/env bash
# Phase 5 M7 受け入れ: one-shot 直経路の native 実行（MAT_IFACE/MAT_FABRIC_INDEX +
# MAT_MATD=0）、native 有効な一時 matd との counter 共有（jump-ahead）、native 対象
# 外 op（describe / diag thread）が chip-tool フォールバックで壊れていないこと、
# の 3 点を jarvis 上で検証する。本番 systemd matd（chip-tool 版, port 9100,
# 既定 socket）には触れない。
#
# 警告: 実行中は本番 matd / 直 chip-tool から同じグループへ group 送信をしないこと
# （group send counter 混在の実機知見 — 以後不達になり matd 再起動でしか回復しない）。
# unicast コマンドは併用してよい。このスクリプトが終わったら Task 9（本番 native 化）
# を速やかに行う前提で実行すること。
#
# 必須 env: MAT_E2E_HOST（ssh 先。repo は public のため既定値を置かない）
#           MAT_E2E_IFACE（native warm session が使う Thread mesh iface 名）
#           MAT_E2E_NODE_ID（unicast 対象の commission 済み node_id）
#           MAT_E2E_GROUP_NODES（対象グループのメンバー node id、カンマ区切り。
#             目視確認の案内表示にのみ使う。repo は public のため既定値を置かない）
# 任意 env: MAT_E2E_GROUP_ID（既定 10）/ MAT_E2E_ENDPOINT（既定 1）
#           MAT_E2E_FABRIC_INDEX（既定 2、jarvis 本番）
#           MAT_E2E_STORE（既定: バイナリ自身のデフォルト解決 = ~/.config/mat 相当。
#             指定時のみ --store を渡す。スペースを含むパスは非対応 — ssh が argv を
#             素朴に空白結合するため）
#           MAT_E2E_SOCKET（既定 /tmp/matd-m7.sock）
#           MAT_E2E_CHIP_TOOL_BIN（chip-tool フォールバックが使うパス。
#             未指定なら ssh 先 PATH 任せ — jarvis の運用に倣う）
set -euo pipefail
cd "$(dirname "$0")/.."

: "${MAT_E2E_HOST:?MAT_E2E_HOST (ssh host) required}"
: "${MAT_E2E_IFACE:?MAT_E2E_IFACE (thread mesh iface on the host) required}"
: "${MAT_E2E_NODE_ID:?MAT_E2E_NODE_ID (unicast target node id) required}"
: "${MAT_E2E_GROUP_NODES:?MAT_E2E_GROUP_NODES (csv node ids, for the visual-check prompt) required}"
GROUP_ID="${MAT_E2E_GROUP_ID:-10}"
ENDPOINT="${MAT_E2E_ENDPOINT:-1}"
FABRIC_INDEX="${MAT_E2E_FABRIC_INDEX:-2}"
SOCKET="${MAT_E2E_SOCKET:-/tmp/matd-m7.sock}"
STORE="${MAT_E2E_STORE:-}"
CHIP_TOOL_BIN="${MAT_E2E_CHIP_TOOL_BIN:-}"
NODE="$MAT_E2E_NODE_ID"
TARGET=aarch64-unknown-linux-musl

confirm() {
  # $1 = 目視確認を促す文面
  echo ""
  echo ">>> $1"
  read -r -p ">>> 確認できたら Enter で続行 (Ctrl-C で中断): " _
}

echo "== 1/5 クロスビルド (mat + matd, $TARGET, rust-lld)"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
export RUSTFLAGS="-C linker-flavor=ld.lld -C link-self-contained=yes"
cargo build --release --target "$TARGET" -p mat -p matd
MAT_BIN="target/$TARGET/release/mat"
MATD_BIN="target/$TARGET/release/matd"
file "$MAT_BIN" | grep -q 'aarch64' || { echo "stale/wrong-arch binary: $MAT_BIN"; exit 1; }
file "$MATD_BIN" | grep -q 'aarch64' || { echo "stale/wrong-arch binary: $MATD_BIN"; exit 1; }
echo "mat:  $MAT_BIN"
echo "matd: $MATD_BIN"

echo "== 2/5 転送 → $MAT_E2E_HOST"
# ssh cat 方式（scp は ssh-agent の状態に左右される、e2e-m4/m5 に倣う）。別名
# (/tmp/mat-m7, /tmp/matd-m7) で置き、本番 /usr/local/bin/{mat,matd} とは衝突させない。
ssh "$MAT_E2E_HOST" 'cat > /tmp/mat-m7 && chmod +x /tmp/mat-m7' < "$MAT_BIN"
ssh "$MAT_E2E_HOST" 'cat > /tmp/matd-m7 && chmod +x /tmp/matd-m7' < "$MATD_BIN"

STORE_ARG=()
[ -n "$STORE" ] && STORE_ARG=(--store "$STORE")

# 直近 run_mat_direct 呼び出しの stderr（ローカル一時ファイル、run_mat_direct 内で
# 上書きされる）。削除は後段の cleanup()（EXIT trap）でまとめて行う。
LAST_STDERR_FILE=$(mktemp)

# ssh 先で一時 mat を直 native 経路（MAT_MATD=0 + MAT_IFACE + MAT_FABRIC_INDEX）で
# 実行する。注意: ssh は argv を素朴に空白結合してリモートへ送るため、各引数は
# 単語内にスペースを含まないこと（env 変数はここでは 1 要素 = 1 argv として渡す
# ので安全。ただし STORE パスにスペースがあると壊れる — 上記コメント参照）。
# stdout は関数の標準出力（呼び出し側で $() 捕捉）、stderr はローカル一時ファイルへ
# （ssh は remote stdout/stderr を別チャンネルのまま転送するため、ローカルの
# `2>file` がそのまま効く）。
run_mat_direct() {
  local envs=(MAT_MATD=0 "MAT_IFACE=$MAT_E2E_IFACE" "MAT_FABRIC_INDEX=$FABRIC_INDEX" RUST_LOG=info)
  [ -n "$CHIP_TOOL_BIN" ] && envs+=("MAT_CHIP_TOOL_BIN=$CHIP_TOOL_BIN")
  ssh "$MAT_E2E_HOST" "${envs[@]}" /tmp/mat-m7 "$@" 2>"$LAST_STDERR_FILE"
}

assert_grep() {
  # $1 = grep パターン, $2 = 対象文字列, $3 = 説明
  if ! printf '%s' "$2" | grep -q -- "$1"; then
    echo "FAIL: $3 — expected pattern '$1' not found in:" >&2
    printf '%s\n' "$2" >&2
    exit 1
  fi
}

assert_no_fallback() {
  if grep -q "falling back" "$LAST_STDERR_FILE"; then
    echo "FAIL: stderr contains 'falling back' — op did not run native:" >&2
    cat "$LAST_STDERR_FILE" >&2
    exit 1
  fi
}

echo "== 3/5 受け入れ1: one-shot 直 native (unicast, node=$NODE)"

OUT=$(run_mat_direct "${STORE_ARG[@]}" read -n "$NODE" -e "$ENDPOINT" -c onoff -a on-off)
echo "$OUT"
assert_grep '"value":' "$OUT" "read onoff (baseline) が value を含まない"
assert_no_fallback

OUT=$(run_mat_direct "${STORE_ARG[@]}" on -n "$NODE" -e "$ENDPOINT")
echo "$OUT"
assert_grep '"status":"success"' "$OUT" "on が status:success を返さない"
assert_no_fallback

OUT=$(run_mat_direct "${STORE_ARG[@]}" read -n "$NODE" -e "$ENDPOINT" -c onoff -a on-off)
echo "$OUT"
assert_grep '"value":true' "$OUT" "on 後の read が value:true を返さない"
assert_no_fallback

OUT=$(run_mat_direct "${STORE_ARG[@]}" off -n "$NODE" -e "$ENDPOINT")
echo "$OUT"
assert_grep '"status":"success"' "$OUT" "off が status:success を返さない"
assert_no_fallback

OUT=$(run_mat_direct "${STORE_ARG[@]}" read -n "$NODE" -e "$ENDPOINT" -c onoff -a on-off)
echo "$OUT"
assert_grep '"value":false' "$OUT" "off 後の read が value:false を返さない"
assert_no_fallback

OUT=$(run_mat_direct "${STORE_ARG[@]}" on -n "$NODE" -e "$ENDPOINT")
echo "$OUT"
assert_grep '"status":"success"' "$OUT" "color 前提の on が失敗"
assert_no_fallback

OUT=$(run_mat_direct "${STORE_ARG[@]}" color -n "$NODE" -e "$ENDPOINT" --name red)
echo "$OUT"
assert_grep '"status":"success"' "$OUT" "color が status:success を返さない"
assert_no_fallback
confirm "node $NODE が赤に変わっていることを目視確認"

OUT=$(run_mat_direct "${STORE_ARG[@]}" color-temp -n "$NODE" -e "$ENDPOINT" --kelvin 2700)
echo "$OUT"
assert_grep '"status":"success"' "$OUT" "color-temp が status:success を返さない"
assert_no_fallback
confirm "node $NODE が暖色(2700K)に変わっていることを目視確認"

echo "-- group 送信 (対象メンバー: $MAT_E2E_GROUP_NODES)"
OUT=$(run_mat_direct "${STORE_ARG[@]}" group invoke -g "$GROUP_ID" -c onoff --command off -e "$ENDPOINT")
echo "$OUT"
assert_grep '"status":"sent"' "$OUT" "group invoke off が status:sent を返さない"
assert_no_fallback
confirm "グループ($GROUP_ID)全メンバー ($MAT_E2E_GROUP_NODES) が消灯していることを目視確認 (N/N)"

OUT=$(run_mat_direct "${STORE_ARG[@]}" group invoke -g "$GROUP_ID" -c onoff --command on -e "$ENDPOINT")
echo "$OUT"
assert_grep '"status":"sent"' "$OUT" "group invoke on が status:sent を返さない"
assert_no_fallback
confirm "グループ($GROUP_ID)全メンバー ($MAT_E2E_GROUP_NODES) が点灯していることを目視確認 (N/N)"

OUT=$(run_mat_direct "${STORE_ARG[@]}" group color-temp -g "$GROUP_ID" --kelvin 2700 -e "$ENDPOINT")
echo "$OUT"
assert_grep '"status":"sent"' "$OUT" "group color-temp が status:sent を返さない"
assert_no_fallback
confirm "グループ($GROUP_ID)全メンバー ($MAT_E2E_GROUP_NODES) が暖色(2700K)になっていることを目視確認 (N/N)"

echo "== 受け入れ1 PASS (one-shot 直 native)"

start_matd() {
  echo "== 一時 matd を起動 (socket $SOCKET, ws port 9110 — 本番 9100/既定 socket とは別)"
  ssh "$MAT_E2E_HOST" \
    MAT_E2E_SOCKET="$SOCKET" \
    MAT_E2E_IFACE="$MAT_E2E_IFACE" \
    MAT_E2E_FABRIC_INDEX="$FABRIC_INDEX" \
    MAT_E2E_STORE="$STORE" \
    MAT_E2E_CHIP_TOOL_BIN="$CHIP_TOOL_BIN" \
    'bash -s' <<'EOF'
set -euo pipefail
rm -f "$MAT_E2E_SOCKET"
ARGS=(--iface "$MAT_E2E_IFACE" --fabric-index "$MAT_E2E_FABRIC_INDEX" \
      --socket "$MAT_E2E_SOCKET" --port 9110)
[ -n "$MAT_E2E_STORE" ] && ARGS+=(--store "$MAT_E2E_STORE")
[ -n "$MAT_E2E_CHIP_TOOL_BIN" ] && export MAT_CHIP_TOOL_BIN="$MAT_E2E_CHIP_TOOL_BIN"
export RUST_LOG=info
nohup /tmp/matd-m7 "${ARGS[@]}" >/tmp/matd-m7.log 2>&1 &
echo $! > /tmp/matd-m7.pid
disown
EOF

  for i in $(seq 1 20); do
    ssh "$MAT_E2E_HOST" "test -S '$SOCKET'" && break
    ssh "$MAT_E2E_HOST" "kill -0 \$(cat /tmp/matd-m7.pid 2>/dev/null) 2>/dev/null" \
      || { echo "matd 起動失敗"; ssh "$MAT_E2E_HOST" 'tail -n 60 /tmp/matd-m7.log' || true; exit 1; }
    sleep 0.5
  done

  ssh "$MAT_E2E_HOST" 'grep -q "native backend enabled" /tmp/matd-m7.log' \
    || { echo "FAIL: matd ログに 'native backend enabled' が無い（chip-tool フォールバックのまま起動）" >&2
         ssh "$MAT_E2E_HOST" 'tail -n 60 /tmp/matd-m7.log' || true
         exit 1; }
}

cleanup() {
  echo "== cleanup: 一時 matd 停止 + ssh 先の一時ファイル削除 ($MAT_E2E_HOST) =="
  ssh "$MAT_E2E_HOST" "/tmp/matd-m7 stop --socket '$SOCKET'" 2>/dev/null || true
  ssh "$MAT_E2E_HOST" "kill \"\$(cat /tmp/matd-m7.pid 2>/dev/null)\" 2>/dev/null" || true
  ssh "$MAT_E2E_HOST" \
    "rm -f /tmp/mat-m7 /tmp/matd-m7 /tmp/matd-m7.pid /tmp/matd-m7.log '$SOCKET'" \
    || true
  rm -f "$LAST_STDERR_FILE"
}
trap cleanup EXIT

echo "== 4/5 受け入れ2: 一時 matd と one-shot 直経路の counter 共有 (jump-ahead)"
start_matd

# one-shot 直経路（受け入れ1 のグループ送信）が進めた counter を、一時 matd が
# 別プロセスとして跨いで単調増加を保つことを実証する。matd 経由で off/on。
OUT=$(ssh "$MAT_E2E_HOST" "/tmp/mat-m7 --matd '$SOCKET' group invoke -g '$GROUP_ID' -c onoff --command off -e '$ENDPOINT'")
echo "$OUT"
assert_grep '"status":"sent"' "$OUT" "matd 経由 group invoke off が status:sent を返さない"
confirm "グループ($GROUP_ID)全メンバー ($MAT_E2E_GROUP_NODES) が消灯していることを目視確認 (N/N) — matd 経由"

OUT=$(ssh "$MAT_E2E_HOST" "/tmp/mat-m7 --matd '$SOCKET' group invoke -g '$GROUP_ID' -c onoff --command on -e '$ENDPOINT'")
echo "$OUT"
assert_grep '"status":"sent"' "$OUT" "matd 経由 group invoke on が status:sent を返さない"
confirm "グループ($GROUP_ID)全メンバー ($MAT_E2E_GROUP_NODES) が点灯していることを目視確認 (N/N) — matd 経由"

echo "-- native groupcast 送信ログ (counter jump-ahead の目視用; 直経路分+matd分の両方が"
echo "   単調増加した counter で並ぶはず)"
ssh "$MAT_E2E_HOST" 'grep "groupcast sent" /tmp/matd-m7.log' || true

ssh "$MAT_E2E_HOST" "/tmp/matd-m7 stop --socket '$SOCKET'"
echo "== 受け入れ2 PASS (counter 共有)"

echo "== 5/5 受け入れ3: フォールバック（native 対象外 op）"
OUT=$(run_mat_direct "${STORE_ARG[@]}" describe -n "$NODE")
echo "$OUT"
assert_grep '"node_id"' "$OUT" "describe が期待どおりの JSON を返さない"

OUT=$(run_mat_direct "${STORE_ARG[@]}" diag thread -n "$NODE")
echo "$OUT"
assert_grep '"channel"' "$OUT" "diag thread が期待どおりの JSON を返さない"

echo "== 受け入れ3 PASS (フォールバック op は chip-tool 経由で健全)"

echo "== e2e:m7 PASS"
