#!/usr/bin/env bash
# [M8c-3] chip-tool 撤去済みのため 0.22.0 以降では動かない（歴史的アーカイブ。
# 動かすなら git tag の 0.21.0 時点を checkout）。現行ハーネスは e2e-m8c3-real.sh。
# Phase 5 M7 受け入れ: one-shot 直経路の native 実行（MAT_IFACE/MAT_FABRIC_INDEX +
# MAT_MATD=0）、native 有効な一時 matd との counter 共有（jump-ahead）、native 対象
# 外 op（describe / diag thread / write）が chip-tool フォールバックで壊れていないこと、
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

# 直近 run_mat_direct 呼び出しの stderr（ローカル一時ファイル、run_mat_direct 内で
# 上書きされる）。削除は cleanup()（EXIT trap）でまとめて行う。
LAST_STDERR_FILE=$(mktemp)

# レビュー指摘: cleanup()/trap は元々フェーズ3完了後（受け入れ1 の全コマンド
# 実行後）に登録されており、それより前（このすぐ下の転送失敗・受け入れ1 の
# 途中失敗等）でスクリプトが落ちるとリモート一時ファイル（/tmp/mat-m7,
# /tmp/matd-m7 等）とローカル mktemp（$LAST_STDERR_FILE）が残った。cleanup() は
# 全操作が `|| true` で防御的（対象がまだ存在しなくても no-op で安全）なので、
# 転送より前に早期登録しても問題ない — ここへ移動する。
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

echo "== 2/5 転送 → $MAT_E2E_HOST"
# ssh cat 方式（scp は ssh-agent の状態に左右される、e2e-m4/m5 に倣う）。別名
# (/tmp/mat-m7, /tmp/matd-m7) で置き、本番 /usr/local/bin/{mat,matd} とは衝突させない。
ssh "$MAT_E2E_HOST" 'cat > /tmp/mat-m7 && chmod +x /tmp/mat-m7' < "$MAT_BIN"
ssh "$MAT_E2E_HOST" 'cat > /tmp/matd-m7 && chmod +x /tmp/matd-m7' < "$MATD_BIN"

STORE_ARG=()
[ -n "$STORE" ] && STORE_ARG=(--store "$STORE")

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

# spec カバレッジ漏れ（brief 段階の記録漏れ）: GroupColor native op（`group color`）
# が受け入れ1 の検証対象から抜けていた。unicast 版 `color`（152行目）は既にある
# ので、group 版も native ホットパス（native_direct.rs の GroupCommand::Color
# 分岐）を通ることを同様に検証する。直後の group color-temp（暖色 2700K）で
# 通常状態に戻す既存の流れをそのまま使う。
OUT=$(run_mat_direct "${STORE_ARG[@]}" group color -g "$GROUP_ID" --name red -e "$ENDPOINT")
echo "$OUT"
assert_grep '"status":"sent"' "$OUT" "group color が status:sent を返さない"
assert_no_fallback
confirm "グループ($GROUP_ID)全メンバー ($MAT_E2E_GROUP_NODES) が赤色になっていることを目視確認 (N/N)"

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

echo "== 4/5 受け入れ2: 一時 matd と one-shot 直経路の counter 共有 (jump-ahead)"
start_matd

# 注意（ssh クォーティング方式の混在）: run_mat_direct() は env-assignment +
# argv 要素を素の配列としてそのまま ssh に渡す（引数にスペースを含まない前提で
# 安全 — 上の「実装上の注意」参照）。対してこのフェーズは --matd 経由（matd
# server と通信する mat 自身がリモートで完結するため env プレフィックスが不要）
# で、1 本のシェル文字列をシングルクオートで組み立て、変数だけ '"$VAR"' で
# 外側に出して展開する古典的な方式。どちらも正しいが由来が違う（前者は
# run_mat_direct 導入時に argv 分割の罠を踏んで配列方式に倒した実装、後者は
# e2e-m5.sh の start_matd() 由来のシェル埋め込み方式をそのまま踏襲）。
# 両方式が同一ファイルに混在するのは意図的 — 統一の必要が生じたら別レビューで。
OUT=$(ssh "$MAT_E2E_HOST" "/tmp/mat-m7 --matd '$SOCKET' group invoke -g '$GROUP_ID' -c onoff --command off -e '$ENDPOINT'")
echo "$OUT"
assert_grep '"status":"sent"' "$OUT" "matd 経由 group invoke off が status:sent を返さない"
confirm "グループ($GROUP_ID)全メンバー ($MAT_E2E_GROUP_NODES) が消灯していることを目視確認 (N/N) — matd 経由"

OUT=$(ssh "$MAT_E2E_HOST" "/tmp/mat-m7 --matd '$SOCKET' group invoke -g '$GROUP_ID' -c onoff --command on -e '$ENDPOINT'")
echo "$OUT"
assert_grep '"status":"sent"' "$OUT" "matd 経由 group invoke on が status:sent を返さない"
confirm "グループ($GROUP_ID)全メンバー ($MAT_E2E_GROUP_NODES) が点灯していることを目視確認 (N/N) — matd 経由"

echo "-- native groupcast 送信ログ (counter jump-ahead の実アサーション)"
# レビュー指摘: 上の2回の group invoke は "status":"sent" しか見ていない —
# matd が per-op で chip-tool にフォールバックしても同じ "sent" を返すため、
# それだけでは native 経路を通った証明にならない（偽陽性防止できていなかった）。
# start_matd() はログを `>` で毎回truncateして起動するため（上の start_matd()
# 参照）、ここで読む /tmp/matd-m7.log はこのフェーズの2回の group invoke の
# 出力のみを含む。ログ行形式は mat-native の
# `tracing::info!(group_id, counter, "groupcast sent (native)")`
# （crates/mat-native/src/group.rs）のレンダリング。
FULL_LOG=$(ssh "$MAT_E2E_HOST" 'cat /tmp/matd-m7.log')
printf '%s\n' "$FULL_LOG" | grep "groupcast sent" || true   # 目視確認用の人間可読出力

if printf '%s\n' "$FULL_LOG" | grep -q "falling back"; then
  echo "FAIL: matd ログに 'falling back' が出ている — このフェーズの group invoke の" >&2
  echo "      どちらか（または両方）が native を通らず chip-tool にフォールバックした。" >&2
  echo "      (crates/matd/src/server.rs の 'native group send unavailable; falling" >&2
  echo "      back to chip-tool' 警告に対応)" >&2
  printf '%s\n' "$FULL_LOG" >&2
  exit 1
fi

NATIVE_SEND_COUNT=$(printf '%s\n' "$FULL_LOG" | grep -c "groupcast sent (native)" || true)
if [ "${NATIVE_SEND_COUNT:-0}" -lt 2 ]; then
  echo "FAIL: matd ログに 'groupcast sent (native)' が2回未満（off/on 各1回のはず、" \
    "found ${NATIVE_SEND_COUNT:-0}) — matd が native 経由で送っていない可能性。" >&2
  printf '%s\n' "$FULL_LOG" >&2
  exit 1
fi

# counter の単調増加（e2e-m5.sh の抽出方法を流用）。この2回だけでなく、受け入れ1
# の直経路分と合わせて counter が飛び先行(jump-ahead)していることの直接証拠には
# ならない（直経路分のログは /tmp/mat-m7 側の別プロセスの stderr にあり、この
# ログファイルには乗らない）が、matd 側だけで見ても単調増加していることは
# 「counter が別プロセス跨ぎで巻き戻っていない」ことの実アサーションになる。
COUNTERS=$(printf '%s\n' "$FULL_LOG" \
  | grep "groupcast sent (native)" \
  | sed -nE 's/.*counter=([0-9]+).*/\1/p')
PREV=""
while IFS= read -r c; do
  [ -z "$c" ] && continue
  if [ -n "$PREV" ] && [ "$c" -le "$PREV" ]; then
    echo "FAIL: groupcast counter not strictly increasing across phase 4" \
      "($PREV -> $c) — possible counter reuse/desync." >&2
    exit 1
  fi
  PREV="$c"
done <<EOF
$COUNTERS
EOF
echo "counters observed (phase 4, native sends only, chronological):"
printf '%s' "$COUNTERS" | tr '\n' ' '
echo

ssh "$MAT_E2E_HOST" "/tmp/matd-m7 stop --socket '$SOCKET'"
echo "== 受け入れ2 PASS (counter 共有; native 経由 >=2 件 + no-fallback + counter 単調増加を実アサーション)"

echo "== 5/5 受け入れ3: フォールバック（native 対象外 op）"
OUT=$(run_mat_direct "${STORE_ARG[@]}" describe -n "$NODE")
echo "$OUT"
assert_grep '"node_id"' "$OUT" "describe が期待どおりの JSON を返さない"

OUT=$(run_mat_direct "${STORE_ARG[@]}" diag thread -n "$NODE")
echo "$OUT"
assert_grep '"channel"' "$OUT" "diag thread が期待どおりの JSON を返さない"

# spec カバレッジ漏れ（M7 spec 受け入れ基準3 原文: "native 対象外 op（describe /
# diag / write）が chip-tool 経由で従来どおり成功"）: write が抜けていた。
# levelcontrol の on-level を選ぶ（write.rs のユニットテスト
# write_reports_success と同じ組。on-level は「電源投入時に復元するレベル」の
# 設定値で、書いても現在の明るさ・点灯状態には実害が無い — invoke 系で state を
# 動かす onoff/current-level とは非対称に、write は状態を持つ属性ではなく
# 「次回オン時の初期値」を書き換えるだけ）。write は native ホットパス対象外
# （native_direct.rs の NativeOp に Write 相当の分岐が無い = classify が
# そもそも native/フォールバックの判定に入らず None を返す）なので、
# 「falling back が出ない」ことは native を通った証明にならず無意味 —
# ここでは成功 JSON のみをアサーションする。
OUT=$(run_mat_direct "${STORE_ARG[@]}" write -n "$NODE" -e "$ENDPOINT" -c levelcontrol -a on-level --value 128)
echo "$OUT"
assert_grep '"status":"success"' "$OUT" "write が status:success を返さない"

echo "== 受け入れ3 PASS (フォールバック op は chip-tool 経由で健全)"

echo "== e2e:m7 PASS"
