#!/usr/bin/env bash
# Phase 5 M8a 受け入れ: 汎用 Interaction Model（read/write/invoke 汎用形）と
# describe / diag thread / open-window / group provision・grant・invoke 汎用形が
# native 直経路（MAT_IFACE + MAT_FABRIC_INDEX）と matd 経由（MAT_MATD_IFACE 相当、
# 本スクリプトでは matd 起動時に --iface/--fabric-index で明示指定）の両方で
# chip-tool を経由せず動くこと、未対応型（list 型属性）への write が即
# parse_error で拒否されること、MAT_IFACE 未設定時は全コマンドが従来どおり
# chip-tool 経由で健全に動くこと、の 11 項目を jarvis 上で検証する。
# 骨格は scripts/e2e-m7.sh を流用（PATH 経由の実 chip-tool には触れない別名
# 一時バイナリ、trap 後始末、stderr への PASS/FAIL、"falling back" grep による
# native 実行の実アサーション）。本番 systemd matd（chip-tool 版, port 9100,
# 既定 socket）には触れない。
#
# native 実行の検出方式（M8a Task11 レビュー指摘対応）: 単に "falling back" が
# 無いことだけでは、classify_strict が None を返す経路（ids テーブル回帰等）で
# 警告ゼロのまま chip-tool にフォールスルーする false-pass を検出できない
# （chip-tool 経由でも同じ正解が返るため）。そのため二重チェックにする —
# (1) positive marker: native_direct.rs::run_op が成功時に出す
# "<op> executed (native direct)" ログを直接 grep（read/write/invoke/describe/
# diag/open-window/grant/provision）、(2) 従来通り "falling back" の不在
# （assert_no_fallback）。
#
# 検証項目（brief 通し番号、各アサーション実装箇所は下の "== N/11" コメント参照）:
#   1. read 汎用      2. write（read 読み返し + null 後始末）
#   3. 未対応型拒否    4. invoke 汎用
#   5. describe（chip-tool 経路と JSON 構造一致）
#   6. diag thread（chip-tool 経路と主要キー一致）
#   7. open-window     8. group grant 冪等
#   9. group provision --rebind 再実行（目視 N/N）
#  10. matd 経由（1/2/4/5 相当が native）
#  11. MAT_IFACE 未設定時のフォールバック健全性（1〜9 相当）
#
# 警告: 実行中は本番 matd / 直 chip-tool から同じグループへ group 送信をしない
# こと（group send counter 混在の実機知見 — 以後不達になり matd 再起動でしか
# 回復しない）。unicast コマンドは併用してよい。
#
# 前提（2026-07-16 実機 E2E の知見）: 本番 matd（native 有効）が group counter の
# flock を保持している間、one-shot native の group 送信は設計どおり chip-tool へ
# フォールバックし検証 9 が FAIL する。**実行前に `sudo systemctl restart matd`
# で flock を解放しておくこと**（matd は group op を受けるまで lazy なので
# 再起動後に group 送信さえしなければ保持しない）。また E2E 中の chip-tool
# spawn 群が g/gdc を進めるため、本番 matd の in-memory counter が窓の下に
# 落ちて group 送信が silent drop になり得る — E2E 完了後にもう一度 matd を
# 再起動すること（rebind 後の再読込も兼ねる）。
#
# 必須 env: MAT_E2E_HOST（ssh 先。repo は public のため既定値を置かない）
#           MAT_E2E_IFACE（native warm session が使う Thread mesh iface 名）
#           MAT_E2E_NODE（unicast 対象の commission 済み node_id。単一ノード。
#             repo は public のため既定値を置かない）
#           MAT_E2E_GROUP_NODES（group provision/grant 対象の commission 済み
#             node_id を空白区切りで（`--nodes` にそのまま渡すため CSV ではなく
#             空白区切り）。repo は public のため既定値を置かない）
# 任意 env: MAT_E2E_GROUP（既定 10、jarvis の living_lights）
#           MAT_E2E_KEYSET（既定 60、jarvis の living_lights keyset）
#           MAT_E2E_ENDPOINT（既定 1）
#           MAT_E2E_FABRIC_INDEX（既定 2、jarvis 本番）
#           MAT_E2E_STORE（既定: バイナリ自身のデフォルト解決 = ~/.config/mat 相当。
#             指定時のみ --store を渡す。スペースを含むパスは非対応 — ssh が argv を
#             素朴に空白結合するため）
#           MAT_E2E_SOCKET（既定 /tmp/matd-m8a.sock）
#           MAT_E2E_CHIP_TOOL_BIN（chip-tool フォールバックが使うパス。
#             未指定なら ssh 先 PATH 任せ — jarvis の運用に倣う）
# ローカル要件: jq（describe / diag / open-window / group grant の JSON 比較・
#   抽出に使う。ローカル側 = 本スクリプトを起動するマシンで実行する。ssh 先
#   （jarvis）には不要）
set -euo pipefail
cd "$(dirname "$0")/.."

: "${MAT_E2E_HOST:?MAT_E2E_HOST (ssh host) required}"
: "${MAT_E2E_IFACE:?MAT_E2E_IFACE (thread mesh iface on the host) required}"
: "${MAT_E2E_NODE:?MAT_E2E_NODE (unicast target node id) required}"
: "${MAT_E2E_GROUP_NODES:?MAT_E2E_GROUP_NODES (space-separated node ids, for group provision/grant) required}"
command -v jq >/dev/null 2>&1 || { echo "jq が必要です（describe/diag/open-window/group grant の JSON 比較に使用）" >&2; exit 1; }

GROUP="${MAT_E2E_GROUP:-10}"
KEYSET="${MAT_E2E_KEYSET:-60}"
ENDPOINT="${MAT_E2E_ENDPOINT:-1}"
FABRIC_INDEX="${MAT_E2E_FABRIC_INDEX:-2}"
SOCKET="${MAT_E2E_SOCKET:-/tmp/matd-m8a.sock}"
STORE="${MAT_E2E_STORE:-}"
CHIP_TOOL_BIN="${MAT_E2E_CHIP_TOOL_BIN:-}"
NODE="$MAT_E2E_NODE"
GROUP_NODES="$MAT_E2E_GROUP_NODES"
TARGET=aarch64-unknown-linux-musl

confirm() {
  # $1 = 目視確認を促す文面
  echo ""
  echo ">>> $1"
  read -r -p ">>> 確認できたら Enter で続行 (Ctrl-C で中断): " _
}

echo "== 1/6 クロスビルド (mat + matd, $TARGET, rust-lld)"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
export RUSTFLAGS="-C linker-flavor=ld.lld -C link-self-contained=yes"
cargo build --release --target "$TARGET" -p mat -p matd
MAT_BIN="target/$TARGET/release/mat"
MATD_BIN="target/$TARGET/release/matd"
file "$MAT_BIN" | grep -q 'aarch64' || { echo "stale/wrong-arch binary: $MAT_BIN"; exit 1; }
file "$MATD_BIN" | grep -q 'aarch64' || { echo "stale/wrong-arch binary: $MATD_BIN"; exit 1; }
echo "mat:  $MAT_BIN"
echo "matd: $MATD_BIN"

# 直近 run_mat_direct / run_mat_chip 呼び出しの stderr（ローカル一時ファイル、
# 呼び出しのたびに上書きされる）。削除は cleanup()（EXIT trap）でまとめて行う。
LAST_STDERR_FILE=$(mktemp)

# e2e-m7 のレビュー指摘を踏襲: cleanup()/trap は転送より前（このすぐ下）で
# 早期登録する。cleanup() は全操作が `|| true` で防御的なので、対象がまだ
# 存在しなくても no-op で安全。
cleanup() {
  # on-level を best-effort で null に復元（do_write_cycle の null write と同一
  # コマンドを流用。途中失敗・中断で on-level=128 が残ったままにならないよう
  # trap のたびに試みる）。必須環境変数 guard（60〜63行目）より前に trap が
  # 発火する将来的な変更に備え、$NODE ではなく ${MAT_E2E_NODE:-} を直接
  # 空チェックしてから使う（set -u 下で未定義変数参照により cleanup 自体が
  # 落ちるのを防ぐ）。
  if [ -n "${MAT_E2E_HOST:-}" ] && [ -n "${MAT_E2E_NODE:-}" ]; then
    echo "== cleanup: on-level を null へ復元 (best-effort, node=${MAT_E2E_NODE}) =="
    local cleanup_envs=(MAT_MATD=0 MAT_LOG=info)
    [ -n "${MAT_E2E_IFACE:-}" ] && cleanup_envs+=("MAT_IFACE=${MAT_E2E_IFACE}")
    [ -n "${FABRIC_INDEX:-}" ] && cleanup_envs+=("MAT_FABRIC_INDEX=${FABRIC_INDEX}")
    local cleanup_store_arg=()
    [ -n "${STORE:-}" ] && cleanup_store_arg=(--store "$STORE")
    ssh "$MAT_E2E_HOST" "${cleanup_envs[@]}" /tmp/mat-m8a "${cleanup_store_arg[@]}" \
      write -n "$MAT_E2E_NODE" -e "${ENDPOINT:-1}" -c levelcontrol -a on-level --value null \
      >/dev/null 2>&1 || true
  fi

  echo "== cleanup: 一時 matd 停止 + ssh 先の一時ファイル削除 ($MAT_E2E_HOST) =="
  ssh "$MAT_E2E_HOST" "/tmp/matd-m8a stop --socket '$SOCKET'" 2>/dev/null || true
  ssh "$MAT_E2E_HOST" "kill \"\$(cat /tmp/matd-m8a.pid 2>/dev/null)\" 2>/dev/null" || true
  ssh "$MAT_E2E_HOST" \
    "rm -f /tmp/mat-m8a /tmp/matd-m8a /tmp/matd-m8a.pid /tmp/matd-m8a.log '$SOCKET'" \
    || true
  rm -f "$LAST_STDERR_FILE"
}
trap cleanup EXIT

echo "== 2/6 転送 → $MAT_E2E_HOST"
# ssh cat 方式（scp は ssh-agent の状態に左右される、e2e-m4/m5/m7 に倣う）。
# 別名 (/tmp/mat-m8a, /tmp/matd-m8a) で置き、本番 /usr/local/bin/{mat,matd} とは
# 衝突させない。
ssh "$MAT_E2E_HOST" 'cat > /tmp/mat-m8a && chmod +x /tmp/mat-m8a' < "$MAT_BIN"
ssh "$MAT_E2E_HOST" 'cat > /tmp/matd-m8a && chmod +x /tmp/matd-m8a' < "$MATD_BIN"

STORE_ARG=()
[ -n "$STORE" ] && STORE_ARG=(--store "$STORE")

# 直経路・native（MAT_MATD=0 + MAT_IFACE + MAT_FABRIC_INDEX）。stdout は関数の
# 標準出力（呼び出し側で $() 捕捉）、stderr はローカル一時ファイルへ（ssh は
# remote stdout/stderr を別チャンネルのまま転送するため、ローカルの `2>file`
# がそのまま効く）。ssh は argv を素朴に空白結合してリモートへ送るため、各引数は
# 単語内にスペースを含まないこと。
run_mat_direct() {
  # MAT_LOG=info を明示（mat の init_tracing は MAT_LOG を RUST_LOG より優先する
  # ので、mat 自身のログレベルを他プロセスの RUST_LOG 設定から独立させる —
  # native 実行の positive marker ログ（"executed (native direct)"）を確実に
  # info レベルで出すため。M8a Task11 レビュー指摘対応）。
  local envs=(MAT_MATD=0 "MAT_IFACE=$MAT_E2E_IFACE" "MAT_FABRIC_INDEX=$FABRIC_INDEX" MAT_LOG=info)
  [ -n "$CHIP_TOOL_BIN" ] && envs+=("MAT_CHIP_TOOL_BIN=$CHIP_TOOL_BIN")
  ssh "$MAT_E2E_HOST" "${envs[@]}" /tmp/mat-m8a "$@" 2>"$LAST_STDERR_FILE"
}

# 直経路・chip-tool フォールバック（MAT_IFACE 未設定）。検証11（フォールバック
# 健全性）専用。
run_mat_chip() {
  local envs=(MAT_MATD=0 MAT_LOG=info)
  [ -n "$CHIP_TOOL_BIN" ] && envs+=("MAT_CHIP_TOOL_BIN=$CHIP_TOOL_BIN")
  ssh "$MAT_E2E_HOST" "${envs[@]}" /tmp/mat-m8a "$@" 2>"$LAST_STDERR_FILE"
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
  # $1 = 説明（省略可）
  if grep -q "falling back" "$LAST_STDERR_FILE"; then
    echo "FAIL: ${1:-op} — stderr contains 'falling back' — op did not run native:" >&2
    cat "$LAST_STDERR_FILE" >&2
    exit 1
  fi
}

# positive 実証: native_direct.rs::run_op（または mat-native/src/group.rs）が
# 成功時に出す "... executed (native direct)" / "groupcast sent (native)" を
# 直接 grep する。assert_no_fallback（"falling back" の不在）だけでは
# classify_strict が None を返す経路（ids テーブル回帰等）で警告ゼロのまま
# chip-tool にフォールスルーする false-pass を検出できないための二重チェック
# （M8a Task11 レビュー指摘対応）。
# $1 = grep パターン, $2 = 説明（省略可）
assert_native_marker() {
  if ! grep -q -- "$1" "$LAST_STDERR_FILE"; then
    echo "FAIL: ${2:-op} — stderr に native 実行の positive marker '$1' が無い（native で走った実証なし）:" >&2
    cat "$LAST_STDERR_FILE" >&2
    exit 1
  fi
}

# ---- write の read/read-back/null-cleanup サイクル（検証2・検証11共有） ----
# $1 = runner 関数名（run_mat_direct / run_mat_chip）
# $2 = "yes" なら assert_no_fallback も行う（native 経路のみ）
do_write_cycle() {
  local runner=$1 native=$2
  OUT=$("$runner" "${STORE_ARG[@]}" write -n "$NODE" -e "$ENDPOINT" -c levelcontrol -a on-level --value 128)
  echo "$OUT"
  assert_grep '"status":"success"' "$OUT" "write levelcontrol on-level=128 が status:success を返さない"
  if [ "$native" = yes ]; then
    assert_no_fallback "write on-level=128"
    assert_native_marker "write executed (native direct)" "write on-level=128"
  fi

  OUT=$("$runner" "${STORE_ARG[@]}" read -n "$NODE" -e "$ENDPOINT" -c levelcontrol -a on-level)
  echo "$OUT"
  assert_grep '"value":128' "$OUT" "write 直後の read on-level が value:128 を返さない"
  if [ "$native" = yes ]; then
    assert_no_fallback "read on-level (post-write)"
    assert_native_marker "read executed (native direct)" "read on-level (post-write)"
  fi

  OUT=$("$runner" "${STORE_ARG[@]}" write -n "$NODE" -e "$ENDPOINT" -c levelcontrol -a on-level --value null)
  echo "$OUT"
  assert_grep '"status":"success"' "$OUT" "write levelcontrol on-level=null（後始末）が status:success を返さない"
  if [ "$native" = yes ]; then
    assert_no_fallback "write on-level=null (cleanup)"
    assert_native_marker "write executed (native direct)" "write on-level=null (cleanup)"
  fi

  OUT=$("$runner" "${STORE_ARG[@]}" read -n "$NODE" -e "$ENDPOINT" -c levelcontrol -a on-level)
  echo "$OUT"
  assert_grep '"value":null' "$OUT" "後始末後の read on-level が value:null を返さない"
  if [ "$native" = yes ]; then
    assert_no_fallback "read on-level (post-cleanup)"
    assert_native_marker "read executed (native direct)" "read on-level (post-cleanup)"
  fi
}

# ---- invoke move-to-level + current-level 読み返し（検証4・検証11共有） ----
do_invoke_cycle() {
  local runner=$1 native=$2
  OUT=$("$runner" "${STORE_ARG[@]}" invoke -n "$NODE" -e "$ENDPOINT" -c levelcontrol --command move-to-level 200 0 0 0)
  echo "$OUT"
  assert_grep '"status":"success"' "$OUT" "invoke levelcontrol move-to-level が status:success を返さない"
  if [ "$native" = yes ]; then
    assert_no_fallback "invoke move-to-level"
    assert_native_marker "invoke executed (native direct)" "invoke move-to-level"
  fi

  OUT=$("$runner" "${STORE_ARG[@]}" read -n "$NODE" -e "$ENDPOINT" -c levelcontrol -a current-level)
  echo "$OUT"
  assert_grep '"value":200' "$OUT" "move-to-level 後の read current-level が value:200 を返さない"
  if [ "$native" = yes ]; then
    assert_no_fallback "read current-level (post-invoke)"
    assert_native_marker "read executed (native direct)" "read current-level (post-invoke)"
  fi
}

# ---- open-window（検証7・検証11共有） ----
do_open_window() {
  local runner=$1 native=$2
  OUT=$("$runner" "${STORE_ARG[@]}" open-window -n "$NODE" --timeout 180)
  echo "$OUT"
  local manual qr
  manual=$(printf '%s' "$OUT" | jq -r '.manual_code')
  qr=$(printf '%s' "$OUT" | jq -r '.qr_payload')
  [[ "$manual" =~ ^[0-9]{11}$ ]] \
    || { echo "FAIL: open-window の manual_code が11桁数字でない: '$manual'" >&2; exit 1; }
  [[ "$qr" == MT:* ]] \
    || { echo "FAIL: open-window の qr_payload が 'MT:' で始まらない: '$qr'" >&2; exit 1; }
  if [ "$native" = yes ]; then
    assert_no_fallback "open-window"
    assert_native_marker "open-window executed (native direct)" "open-window"
  fi
}

# ---- group grant 冪等性（検証8・検証11共有） ----
do_group_grant_idempotent() {
  local runner=$1 native=$2
  OUT=$("$runner" "${STORE_ARG[@]}" group grant -g "$GROUP" --nodes $GROUP_NODES)
  echo "$OUT"
  assert_grep '"status":"granted"' "$OUT" "group grant（1回目）が status:granted を返さない"
  if [ "$native" = yes ]; then
    assert_no_fallback "group grant (1st)"
    assert_native_marker "group grant executed (native direct)" "group grant (1st)"
  fi

  OUT=$("$runner" "${STORE_ARG[@]}" group grant -g "$GROUP" --nodes $GROUP_NODES)
  echo "$OUT"
  assert_grep '"status":"granted"' "$OUT" "group grant（2回目）が status:granted を返さない"
  if [ "$native" = yes ]; then
    assert_no_fallback "group grant (2nd)"
    assert_native_marker "group grant executed (native direct)" "group grant (2nd)"
  fi

  local updated_len unchanged_len expected_len
  updated_len=$(printf '%s' "$OUT" | jq '.updated | length')
  unchanged_len=$(printf '%s' "$OUT" | jq '.unchanged | length')
  # shellcheck disable=SC2086 # GROUP_NODES は意図的に空白展開する（wc -w に単語数を渡す）
  expected_len=$(printf '%s' "$GROUP_NODES" | wc -w)
  if [ "$updated_len" -ne 0 ] || [ "$unchanged_len" -ne "$expected_len" ]; then
    echo "FAIL: group grant 2回目が全ノード unchanged にならない" \
      "(updated=$updated_len, unchanged=$unchanged_len, expected=$expected_len)" >&2
    printf '%s\n' "$OUT" >&2
    exit 1
  fi
}

# ---- group provision --rebind 再実行 + 目視 N/N（検証9・検証11共有） ----
do_group_provision_rebind() {
  local runner=$1 native=$2 label=$3
  # shellcheck disable=SC2086
  OUT=$("$runner" "${STORE_ARG[@]}" group provision -g "$GROUP" --nodes $GROUP_NODES \
    --keyset-id "$KEYSET" --rebind)
  echo "$OUT"
  assert_grep '"status":"provisioned"' "$OUT" "group provision --rebind が status:provisioned を返さない"
  if [ "$native" = yes ]; then
    assert_no_fallback "group provision --rebind"
    assert_native_marker "group provision executed (native direct)" "group provision --rebind"
  fi

  OUT=$("$runner" "${STORE_ARG[@]}" group invoke -g "$GROUP" -c onoff --command off -e "$ENDPOINT")
  echo "$OUT"
  assert_grep '"status":"sent"' "$OUT" "group invoke off が status:sent を返さない"
  if [ "$native" = yes ]; then
    assert_no_fallback "group invoke off"
    assert_native_marker "groupcast sent (native)" "group invoke off"
  fi
  confirm "[$label] グループ($GROUP) 全メンバー ($GROUP_NODES) が消灯していることを目視確認 (N/N)"

  OUT=$("$runner" "${STORE_ARG[@]}" group invoke -g "$GROUP" -c onoff --command on -e "$ENDPOINT")
  echo "$OUT"
  assert_grep '"status":"sent"' "$OUT" "group invoke on が status:sent を返さない"
  if [ "$native" = yes ]; then
    assert_no_fallback "group invoke on"
    assert_native_marker "groupcast sent (native)" "group invoke on"
  fi
  confirm "[$label] グループ($GROUP) 全メンバー ($GROUP_NODES) が点灯していることを目視確認 (N/N)"
}

echo "== 3/6 検証1〜9: 直経路 native（node=$NODE, group=$GROUP）"

echo "-- 1/11 read 汎用"
OUT=$(run_mat_direct "${STORE_ARG[@]}" read -n "$NODE" -e "$ENDPOINT" -c levelcontrol -a current-level)
echo "$OUT"
assert_grep '"value":' "$OUT" "read levelcontrol current-level が value を含まない"
assert_no_fallback "read current-level (baseline)"
assert_native_marker "read executed (native direct)" "read current-level (baseline)"

echo "-- 2/11 write（read 読み返し + null 後始末）"
do_write_cycle run_mat_direct yes

echo "-- 3/11 未対応型（list 型）write の拒否"
set +e
OUT=$(run_mat_direct "${STORE_ARG[@]}" write -n "$NODE" -e 0 -c accesscontrol -a acl --value '[]')
RC=$?
set -e
if [ "$RC" -ne 1 ]; then
  echo "FAIL: 未対応型 write の exit code が 1 でない (got $RC)" >&2
  cat "$LAST_STDERR_FILE" >&2
  exit 1
fi
STDERR_OUT=$(cat "$LAST_STDERR_FILE")
assert_grep '"kind":"parse_error"' "$STDERR_OUT" "未対応型 write の stderr に error.kind:parse_error が無い"
echo "PASS: write accesscontrol/acl (list型) は exit 1 + parse_error" >&2

echo "-- 4/11 invoke 汎用"
do_invoke_cycle run_mat_direct yes

echo "-- 5/11 describe（chip-tool 経路と JSON 構造一致）"
OUT=$(run_mat_direct "${STORE_ARG[@]}" describe -n "$NODE")
echo "$OUT"
assert_grep '"endpoints"' "$OUT" "describe（native）が endpoints を含まない"
assert_no_fallback "describe (native)"
assert_native_marker "describe executed (native direct)" "describe (native)"
DESCRIBE_NATIVE="$OUT"

DESCRIBE_CHIP=$(run_mat_chip "${STORE_ARG[@]}" describe -n "$NODE")
echo "$DESCRIBE_CHIP"
assert_grep '"endpoints"' "$DESCRIBE_CHIP" "describe（chip-tool）が endpoints を含まない"

DESCRIBE_NATIVE_SHAPE=$(printf '%s' "$DESCRIBE_NATIVE" \
  | jq -S '.endpoints | map({endpoint, clusters: (.clusters | sort)}) | sort_by(.endpoint)')
DESCRIBE_CHIP_SHAPE=$(printf '%s' "$DESCRIBE_CHIP" \
  | jq -S '.endpoints | map({endpoint, clusters: (.clusters | sort)}) | sort_by(.endpoint)')
if [ "$DESCRIBE_NATIVE_SHAPE" != "$DESCRIBE_CHIP_SHAPE" ]; then
  echo "FAIL: describe の native/chip-tool で endpoints/clusters 構造が一致しない" >&2
  echo "native:" >&2; printf '%s\n' "$DESCRIBE_NATIVE_SHAPE" >&2
  echo "chip-tool:" >&2; printf '%s\n' "$DESCRIBE_CHIP_SHAPE" >&2
  exit 1
fi
echo "PASS: describe native/chip-tool 構造一致（ep0 含む $(printf '%s' "$DESCRIBE_NATIVE_SHAPE" | jq 'length') endpoints）" >&2

echo "-- 6/11 diag thread（chip-tool 経路と主要キー一致）"
OUT=$(run_mat_direct "${STORE_ARG[@]}" diag thread -n "$NODE")
echo "$OUT"
assert_grep '"thread"' "$OUT" "diag thread（native）が thread を含まない"
assert_no_fallback "diag thread (native)"
assert_native_marker "diag thread executed (native direct)" "diag thread (native)"
DIAG_NATIVE="$OUT"

NT_IS_ARRAY=$(printf '%s' "$DIAG_NATIVE" | jq '.thread.neighbor_table | type == "array"')
[ "$NT_IS_ARRAY" = "true" ] \
  || { echo "FAIL: diag thread（native）の thread.neighbor_table が配列でない" >&2; exit 1; }
NT_LEN=$(printf '%s' "$DIAG_NATIVE" | jq '.thread.neighbor_table | length')
if [ "$NT_LEN" -gt 0 ]; then
  HAS_LQI=$(printf '%s' "$DIAG_NATIVE" | jq '.thread.neighbor_table[0] | has("Lqi")')
  [ "$HAS_LQI" = "true" ] \
    || { echo "FAIL: diag thread（native）の neighbor_table 要素に Lqi キーが無い" >&2; exit 1; }
fi

DIAG_CHIP=$(run_mat_chip "${STORE_ARG[@]}" diag thread -n "$NODE")
echo "$DIAG_CHIP"
assert_grep '"thread"' "$DIAG_CHIP" "diag thread（chip-tool）が thread を含まない"

DIAG_NATIVE_KEYS=$(printf '%s' "$DIAG_NATIVE" | jq -S '.thread | keys')
DIAG_CHIP_KEYS=$(printf '%s' "$DIAG_CHIP" | jq -S '.thread | keys')
if [ "$DIAG_NATIVE_KEYS" != "$DIAG_CHIP_KEYS" ]; then
  echo "FAIL: diag thread の native/chip-tool で thread オブジェクトの主要キーが一致しない" >&2
  echo "native:    $DIAG_NATIVE_KEYS" >&2
  echo "chip-tool: $DIAG_CHIP_KEYS" >&2
  exit 1
fi
echo "PASS: diag thread native/chip-tool 主要キー一致 ($DIAG_NATIVE_KEYS)" >&2

echo "-- 7/11 open-window"
do_open_window run_mat_direct yes

echo "-- 8/11 group grant 冪等"
do_group_grant_idempotent run_mat_direct yes

echo "-- 9/11 group provision --rebind 再実行 + 目視 N/N"
do_group_provision_rebind run_mat_direct yes native

echo "== 検証1〜9 PASS（直経路 native）"

start_matd() {
  echo "== 一時 matd を起動 (socket $SOCKET, ws port 9112 — 本番 9100/既定 socket とは別)"
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
      --socket "$MAT_E2E_SOCKET" --port 9112)
[ -n "$MAT_E2E_STORE" ] && ARGS+=(--store "$MAT_E2E_STORE")
[ -n "$MAT_E2E_CHIP_TOOL_BIN" ] && export MAT_CHIP_TOOL_BIN="$MAT_E2E_CHIP_TOOL_BIN"
# debug: matd::backend の "chip-tool ws raw response" ログ（chip-tool 実トラフィック
# の実アサーション用マーカー、下の検証10参照）を拾うため matd クレートは debug。
export RUST_LOG=matd=debug,info
nohup /tmp/matd-m8a "${ARGS[@]}" >/tmp/matd-m8a.log 2>&1 &
echo $! > /tmp/matd-m8a.pid
disown
EOF

  for i in $(seq 1 20); do
    ssh "$MAT_E2E_HOST" "test -S '$SOCKET'" && break
    ssh "$MAT_E2E_HOST" "kill -0 \$(cat /tmp/matd-m8a.pid 2>/dev/null) 2>/dev/null" \
      || { echo "matd 起動失敗"; ssh "$MAT_E2E_HOST" 'tail -n 60 /tmp/matd-m8a.log' || true; exit 1; }
    sleep 0.5
  done

  ssh "$MAT_E2E_HOST" 'grep -q "native backend enabled" /tmp/matd-m8a.log' \
    || { echo "FAIL: matd ログに 'native backend enabled' が無い（chip-tool フォールバックのまま起動）" >&2
         ssh "$MAT_E2E_HOST" 'tail -n 60 /tmp/matd-m8a.log' || true
         exit 1; }
}

echo "== 4/6 検証10: matd 経由（read/write/invoke/describe 相当が native）"
start_matd

# matd 経由呼び出し（--matd 経由なので env プレフィックス不要 — mat 自身は
# リモートで完結する）。start_matd() がログを `>` で毎回 truncate して起動する
# ため、以降で読む /tmp/matd-m8a.log はこのフェーズの呼び出しのみを含む。
OUT=$(ssh "$MAT_E2E_HOST" "/tmp/mat-m8a --matd '$SOCKET' read -n '$NODE' -e '$ENDPOINT' -c levelcontrol -a current-level")
echo "$OUT"
assert_grep '"value":' "$OUT" "matd 経由 read current-level が value を含まない"

OUT=$(ssh "$MAT_E2E_HOST" "/tmp/mat-m8a --matd '$SOCKET' write -n '$NODE' -e '$ENDPOINT' -c levelcontrol -a on-level --value 128")
echo "$OUT"
assert_grep '"status":"success"' "$OUT" "matd 経由 write on-level=128 が status:success を返さない"

OUT=$(ssh "$MAT_E2E_HOST" "/tmp/mat-m8a --matd '$SOCKET' read -n '$NODE' -e '$ENDPOINT' -c levelcontrol -a on-level")
echo "$OUT"
assert_grep '"value":128' "$OUT" "matd 経由 write 直後の read on-level が value:128 を返さない"

OUT=$(ssh "$MAT_E2E_HOST" "/tmp/mat-m8a --matd '$SOCKET' write -n '$NODE' -e '$ENDPOINT' -c levelcontrol -a on-level --value null")
echo "$OUT"
assert_grep '"status":"success"' "$OUT" "matd 経由 write on-level=null（後始末）が status:success を返さない"

OUT=$(ssh "$MAT_E2E_HOST" "/tmp/mat-m8a --matd '$SOCKET' read -n '$NODE' -e '$ENDPOINT' -c levelcontrol -a on-level")
echo "$OUT"
assert_grep '"value":null' "$OUT" "matd 経由 後始末後の read on-level が value:null を返さない"

OUT=$(ssh "$MAT_E2E_HOST" "/tmp/mat-m8a --matd '$SOCKET' invoke -n '$NODE' -e '$ENDPOINT' -c levelcontrol --command move-to-level 200 0 0 0")
echo "$OUT"
assert_grep '"status":"success"' "$OUT" "matd 経由 invoke move-to-level が status:success を返さない"

OUT=$(ssh "$MAT_E2E_HOST" "/tmp/mat-m8a --matd '$SOCKET' read -n '$NODE' -e '$ENDPOINT' -c levelcontrol -a current-level")
echo "$OUT"
assert_grep '"value":200' "$OUT" "matd 経由 move-to-level 後の read current-level が value:200 を返さない"

OUT=$(ssh "$MAT_E2E_HOST" "/tmp/mat-m8a --matd '$SOCKET' describe -n '$NODE'")
echo "$OUT"
assert_grep '"endpoints"' "$OUT" "matd 経由 describe が endpoints を含まない"

echo "-- matd ログ (chip-tool 実トラフィックが無いことの実アサーション)"
# matd は起動時に必ず chip-tool を spawn する（backend.rs::spawn_child、"spawning
# chip-tool interactive server"）ので「spawn の有無」は native/fallback の証拠に
# ならない。実際に ws 経由で chip-tool へコマンドをやり取りしたかどうかは
# backend.rs::exchange() の `tracing::debug!(%text, "chip-tool ws raw response")`
# にのみ現れる（is_native_hotpath 経由の native op はこの関数を一切呼ばない）。
# start_matd() が RUST_LOG=matd=debug でこのログを有効化しているので、この
# フェーズ（read/write/invoke/describe のみ）でこの行が1件も無いことを確認する。
FULL_LOG=$(ssh "$MAT_E2E_HOST" 'cat /tmp/matd-m8a.log')
if printf '%s\n' "$FULL_LOG" | grep -q "chip-tool ws raw response"; then
  echo "FAIL: matd ログに 'chip-tool ws raw response' が出ている — このフェーズの" >&2
  echo "      read/write/invoke/describe のいずれかが native を通らず chip-tool 経由で" >&2
  echo "      実行された可能性。" >&2
  printf '%s\n' "$FULL_LOG" >&2
  exit 1
fi
if printf '%s\n' "$FULL_LOG" | grep -q "falling back"; then
  echo "FAIL: matd ログに 'falling back' が出ている" >&2
  printf '%s\n' "$FULL_LOG" >&2
  exit 1
fi

ssh "$MAT_E2E_HOST" "/tmp/matd-m8a stop --socket '$SOCKET'"
echo "== 検証10 PASS（matd 経由 read/write/invoke/describe が native, chip-tool traffic 無し）"

echo "== 5/6 検証11: フォールバック健全性（MAT_IFACE 未設定で検証1・2・4〜9相当）"

echo "-- 1/11(fallback) read 汎用"
OUT=$(run_mat_chip "${STORE_ARG[@]}" read -n "$NODE" -e "$ENDPOINT" -c levelcontrol -a current-level)
echo "$OUT"
assert_grep '"value":' "$OUT" "(fallback) read levelcontrol current-level が value を含まない"

echo "-- 2/11(fallback) write（read 読み返し + null 後始末）"
do_write_cycle run_mat_chip no

echo "-- 4/11(fallback) invoke 汎用"
do_invoke_cycle run_mat_chip no

echo "-- 5/11(fallback) describe"
OUT=$(run_mat_chip "${STORE_ARG[@]}" describe -n "$NODE")
echo "$OUT"
assert_grep '"endpoints"' "$OUT" "(fallback) describe が endpoints を含まない"

echo "-- 6/11(fallback) diag thread"
OUT=$(run_mat_chip "${STORE_ARG[@]}" diag thread -n "$NODE")
echo "$OUT"
assert_grep '"thread"' "$OUT" "(fallback) diag thread が thread を含まない"

echo "-- 7/11(fallback) open-window"
do_open_window run_mat_chip no

echo "-- 8/11(fallback) group grant 冪等"
do_group_grant_idempotent run_mat_chip no

echo "-- 9/11(fallback) group provision --rebind 再実行 + 目視 N/N"
do_group_provision_rebind run_mat_chip no fallback

echo "== 検証11 PASS（MAT_IFACE 未設定でも従来どおり成功）"

echo "== 6/6 e2e:m8a:real PASS（検証1〜11 全項目 GREEN）"
