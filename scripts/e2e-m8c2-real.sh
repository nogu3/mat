#!/usr/bin/env bash
# Phase 5 M8c-2 受け入れ: `mat group provision` のコントローラ側 group state
# 書込（groupsettings add-group/add-keysets/bind-keyset 相当）が MAT_IFACE
# 設定時に chip-tool を一切 spawn せず native KVS 直書き（mat-controller::
# group_settings）だけで完走すること、native で書いた KVS を実 chip-tool が
# 読めること（groupsettings show-groups/show-keysets、compat の主検証）、
# `mat diag node --deep` の operational/thread 部分が native（mat-native::
# ops）で完走すること、MAT_IFACE 未設定時は従来どおり chip-tool 経路が
# 健全であることを jarvis 上・実機の living_lights メンバー 2 台（既定
# node 8/9）を使い捨てグループ 99 に入れて検証する。
#
# 骨格は scripts/e2e-m8a-real.sh / e2e-m8b-real.sh / e2e-m8c1-real.sh を流用
# （trap 後始末・stderr への PASS/FAIL・positive marker 二重チェック・musl
# rust-lld クロスビルド・ssh -n の作法）。M8c-1 と異なり BLE 機能は使わない
# ため素の musl クロスビルドで足りる（gnu/cross 不要）。
#
# native 実行の実アサーションは m8a/m8b/m8c1 と同じ二重チェック方式:
# (1) 外部バイナリを実在させない（MAT_CHIP_TOOL_BIN=/nonexistent/...）ことで
#     chip-tool spawn があれば即 exit 12（child_not_found）になる、
# (2) 加えて positive marker を stderr から直接 grep する ——
#     "group provision controller state written (native kvs)"
#     （mat-native::group_settings::write_group_provision, KVS 書込側）
#     "group provision executed (native direct)"
#     （mat/src/native_direct.rs, provision 全体の完走側）
#     "diag node executed (native)"
#     （mat/src/native_direct.rs::diag_im_with_engine, operational+thread 部分）
# assert_no_fallback（"falling back" の不在）と組み合わせる（M8a Task11 の
# 教訓 — marker 不在のまま警告ゼロで静かに fallback する回帰を検出するため）。
#
# 検証項目（brief 通し番号）:
#   1. 準備: musl クロスビルド → scp → matd 停止（trap で復帰）→ KVS backup。
#   2. native group provision（chip-tool spawn ゼロ）: 両 positive marker +
#      status:provisioned + note(restart 案内)。
#   3. native groupcast: toggle → 各ノード on-off 反転を native read で確認
#      → 逆 toggle で復元。
#   4. --rebind 再実行が成功（Duplicate にならない）、--rebind 無しの再実行が
#      "use --rebind" 誘導の detail で失敗（exit 1 = ErrorKind::Other）。
#   5. chip-tool 互換: (a) 実 chip-tool groupsettings show-groups/show-keysets
#      に group 99 / keyset 99 が現れる（mat が書いた KVS を実 chip-tool が
#      読めた証明、主検証・FAIL 対象）。単発実行を先に試し、失敗したら
#      interactive echo パイプにフォールバック（brief の両対応指示）。
#      (b) groupcast 互換 best-effort: <store>/native_group_counter の値+4096
#      を LE u32/base64 で g/gdc に書き戻し、MAT_IFACE 未設定の実 chip-tool
#      経路で group invoke → 目視確認（WARN 許容、(a) が主検証）。
#   6. diag node --deep native: marker + verdict/checks.mdns。MAT_IFACE 未設定
#      + 実 chip-tool でも従来どおり成功。
#   7. 後片付け（best-effort、失敗は WARN）+ matd 復帰 + living_lights(g=10)
#      無傷確認（matd 経由 off→on、目視）。
#
# ★judgment: chip-tool の groupsettings 系サブコマンドは単発（非 interactive）
# で確実に動く根拠がある —— mat 自身の chip-tool フォールバック経路
# （crates/mat/src/commands/group.rs::provision_controller_state）が
# `chip-tool groupsettings add-group <name> <id> --storage-directory <dir>`
# を単発 Command::output() で呼んでおり（crates/mat/src/runner.rs::
# ChipTool::run）、これは実機で動作実績のある経路。show-groups/show-keysets
# も同じ `groupsettings` コマンド族なので単発で動くと想定できるが、念のため
# 失敗時は interactive echo パイプへフォールバックする（brief の両対応指示、
# 未検証環境での保険）。
#
# ★judgment: 検証5(b) の目視確認は自動判定できない（点滅の有無は人間にしか
# 見えない）ため、MAT_E2E_ASSUME_YES=1 では既定で WARN（"確認できず"）に倒す
# — m8c1 の confirm_yn（危険操作は既定スキップ）と同じ「安全側に倒す」判断。
#
# 必須 env: MAT_E2E_HOST（ssh 先。repo は public のため既定値を置かない）
# 任意 env: MAT_E2E_IFACE（既定 eth0）
#           MAT_E2E_FABRIC_INDEX（既定 2、jarvis 本番）
#           MAT_E2E_TEST_NODES（既定 "8 9"、living_lights の2台を使い捨て
#             グループにも入れる。空白区切り — --nodes にそのまま渡すため）
#           MAT_E2E_GROUP_ID（既定 99）
#           MAT_E2E_KEYSET_ID（既定 99）
#           MAT_E2E_ENDPOINT（既定 1）
#           MAT_E2E_STORE（既定: バイナリ自身のデフォルト解決 = jarvis の
#             本番ストア相当。指定時のみ --store を渡す）
#           MAT_E2E_CHIP_TOOL_BIN（検証5・6 で使う実 chip-tool のパス。
#             未指定なら ssh 先 PATH 任せ）
#           MAT_E2E_ASSUME_YES=1（目視確認プロンプトを自動化。検証5(b) の
#             点滅確認は既定で WARN 側に倒す — 上記 judgment 参照）
# ローカル要件: jq（JSON 抽出・比較に使う。ローカル側で実行する）
#   ssh 先（jarvis）には python3 が必要（検証5(b) の g/gdc エンコードに使用、
#   未搭載なら該当部分のみ WARN でスキップ）。
set -euo pipefail
cd "$(dirname "$0")/.."

: "${MAT_E2E_HOST:?MAT_E2E_HOST (ssh host) required}"
command -v jq >/dev/null 2>&1 || { echo "jq が必要です（JSON 抽出・比較に使用）" >&2; exit 1; }

IFACE="${MAT_E2E_IFACE:-eth0}"
FABRIC_INDEX="${MAT_E2E_FABRIC_INDEX:-2}"
TEST_NODES="${MAT_E2E_TEST_NODES:-8 9}"
GROUP="${MAT_E2E_GROUP_ID:-99}"
KEYSET="${MAT_E2E_KEYSET_ID:-99}"
ENDPOINT="${MAT_E2E_ENDPOINT:-1}"
STORE="${MAT_E2E_STORE:-}"
CHIP_TOOL_BIN="${MAT_E2E_CHIP_TOOL_BIN:-}"
# shellcheck disable=SC2206 # TEST_NODES は意図的に空白分割する（--nodes にそのまま渡す）
NODE_ARR=($TEST_NODES)
TARGET=aarch64-unknown-linux-musl
REMOTE_BIN=/tmp/mat-e2e-0.21.0
GROUP_NAME=e2e-m8c2

confirm() {
  # $1 = 目視確認を促す文面。Enter で続行、Ctrl-C で中断（m8a/m8b/m8c1 同様）。
  echo ""
  echo ">>> $1"
  if [ "${MAT_E2E_ASSUME_YES:-0}" = "1" ]; then
    echo ">>> (MAT_E2E_ASSUME_YES=1: 自動確認で続行)"
    return
  fi
  read -r -p ">>> 確認できたら Enter で続行 (Ctrl-C で中断): " _
}

# $1 = 実行してよいか問う文面（y/Y のみ成功、他はスキップ）。目視のみで自動
# 判定できない検証5(b)の点滅確認に使う。ASSUME_YES=1 は安全側の WARN に倒す
# （上記 judgment 参照 — 危険操作ではないが自動 PASS 判定を避ける）。
confirm_blink_yn() {
  echo ""
  echo ">>> $1"
  if [ "${MAT_E2E_ASSUME_YES:-0}" = "1" ]; then
    echo ">>> (MAT_E2E_ASSUME_YES=1: 目視確認不可のため WARN 側に倒す)"
    return 1
  fi
  local ans
  read -r -p ">>> 点滅しましたか？ [y/N]: " ans
  case "$ans" in
    y|Y) return 0 ;;
    *) return 1 ;;
  esac
}

echo "== 1/7 クロスビルド (mat, $TARGET, rust-lld) — matd は不要（provision/invoke/diag は直経路）"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
export RUSTFLAGS="-C linker-flavor=ld.lld -C link-self-contained=yes"
cargo build --release --target "$TARGET" -p mat
MAT_BIN="target/$TARGET/release/mat"
file "$MAT_BIN" | grep -q 'aarch64' || { echo "FAIL: stale/wrong-arch binary: $MAT_BIN" >&2; exit 1; }
echo "mat: $MAT_BIN"

# 直近呼び出しの stderr（ローカル一時ファイル、呼び出しのたびに上書きされる）。
LAST_STDERR_FILE=$(mktemp)

cleanup() {
  echo "== cleanup: ssh 先の一時バイナリ削除 + matd 復帰 ($MAT_E2E_HOST) =="
  ssh -n "$MAT_E2E_HOST" "rm -f '$REMOTE_BIN'" || true
  # trap で必ず matd を起動状態に戻す（既に動いていれば no-op）。
  ssh -n "$MAT_E2E_HOST" "sudo systemctl start matd" || true
  if [ -n "${REMOTE_STORE:-}" ]; then
    echo "== KVS バックアップの案内 =="
    echo "  jarvis 上の $REMOTE_STORE/chip_tool_config.ini.bak-m8c2 に検証前の状態を保存しています。"
    echo "  問題があれば手動で復元してください（本ハーネスは自動 restore しません）:"
    echo "    ssh $MAT_E2E_HOST \"cp '$REMOTE_STORE/chip_tool_config.ini.bak-m8c2' '$REMOTE_STORE/chip_tool_config.ini'\""
  fi
  rm -f "$LAST_STDERR_FILE"
}
trap cleanup EXIT

echo "== 2/7 転送 → $MAT_E2E_HOST ($REMOTE_BIN, 本番 /usr/local/bin/mat とは別)"
# brief の指示どおり scp（当環境は ssh-agent 経由で scp が通る実績あり —
# メモリ「jarvis への scp」参照）。
scp "$MAT_BIN" "$MAT_E2E_HOST:$REMOTE_BIN"
ssh -n "$MAT_E2E_HOST" "chmod +x '$REMOTE_BIN'"

STORE_ARG=()
[ -n "$STORE" ] && STORE_ARG=(--store "$STORE")

echo "== 3/7 store 解決 + matd 停止 + KVS backup"
if [ -n "$STORE" ]; then
  REMOTE_STORE="$STORE"
else
  # mat-core::store の優先順位（MAT_STORE > XDG_CONFIG_HOME/mat > ~/.config/mat）
  # をリモートシェルに解決させる。
  REMOTE_STORE=$(ssh -n "$MAT_E2E_HOST" 'echo "${MAT_STORE:-${XDG_CONFIG_HOME:-$HOME/.config}/mat}"')
fi
echo "store = $REMOTE_STORE"

echo "sudo systemctl stop matd (直経路検証中は flock/warm chip-tool を退避、trap で必ず復帰)"
ssh -n "$MAT_E2E_HOST" "sudo systemctl stop matd"

echo "KVS backup: $REMOTE_STORE/chip_tool_config.ini -> .bak-m8c2 (上書きしない -n 付き)"
ssh -n "$MAT_E2E_HOST" "cp -n '$REMOTE_STORE/chip_tool_config.ini' '$REMOTE_STORE/chip_tool_config.ini.bak-m8c2'" \
  || echo "WARN: KVS backup が失敗（既に .bak-m8c2 が存在？ 上書きはしていません）" >&2

# ---- runner 群 ----
# 検証2〜4・6(native側): native 直経路の純 native 実証。chip-tool 不在。
run_native() {
  local envs=(MAT_MATD=0 MAT_LOG=info "MAT_IFACE=$IFACE" "MAT_FABRIC_INDEX=$FABRIC_INDEX" \
    MAT_CHIP_TOOL_BIN=/nonexistent/mat-e2e-m8c2-chip-tool)
  ssh -n "$MAT_E2E_HOST" "${envs[@]}" "$REMOTE_BIN" "$@" 2>"$LAST_STDERR_FILE"
}

# 検証5(b)・6(chip-tool側): MAT_IFACE 未設定、実 chip-tool。
run_chip() {
  local envs=(MAT_MATD=0 MAT_LOG=info)
  [ -n "$CHIP_TOOL_BIN" ] && envs+=("MAT_CHIP_TOOL_BIN=$CHIP_TOOL_BIN")
  ssh -n "$MAT_E2E_HOST" "${envs[@]}" "$REMOTE_BIN" "$@" 2>"$LAST_STDERR_FILE"
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

# positive 実証（M8a Task11 の教訓 — marker 不在のまま静かに fallback する
# 回帰を assert_no_fallback だけでは検出できないための二重チェック）。
# $1 = grep パターン, $2 = 説明（省略可）
assert_native_marker() {
  if ! grep -q -- "$1" "$LAST_STDERR_FILE"; then
    echo "FAIL: ${2:-op} — stderr に native 実行の positive marker '$1' が無い（native で走った実証なし）:" >&2
    cat "$LAST_STDERR_FILE" >&2
    exit 1
  fi
}

echo "== 4/7 検証2: native group provision（chip-tool spawn ゼロ、group=$GROUP keyset=$KEYSET nodes=$TEST_NODES）"
# shellcheck disable=SC2086 # TEST_NODES は意図的に空白展開する（--nodes にそのまま渡す）
OUT=$(run_native "${STORE_ARG[@]}" group provision -g "$GROUP" --nodes $TEST_NODES \
  --keyset-id "$KEYSET" --name "$GROUP_NAME")
echo "$OUT"
assert_no_fallback "group provision (native)"
assert_native_marker "group provision controller state written (native kvs)" "group provision (native kvs write)"
assert_native_marker "group provision executed (native direct)" "group provision (native direct 完走)"
assert_grep '"status":"provisioned"' "$OUT" "group provision が status:provisioned を返さない"
assert_grep '"note":".*restart' "$OUT" "group provision の note に restart 案内が無い"
echo "PASS: 検証2 native group provision（KVS書込+完走の両marker、status:provisioned+note、fallback不在）" >&2

echo "== 5/7 検証3: native groupcast（toggle → 反転確認 → 逆toggle）"
declare -A BEFORE
for n in "${NODE_ARR[@]}"; do
  OUT=$(run_native "${STORE_ARG[@]}" read -n "$n" -e "$ENDPOINT" -c onoff -a on-off)
  echo "$OUT"
  assert_no_fallback "read on-off baseline (node=$n)"
  BEFORE[$n]=$(printf '%s' "$OUT" | jq -r '.value')
  echo "node $n baseline on-off = ${BEFORE[$n]}"
done

OUT=$(run_native "${STORE_ARG[@]}" group invoke -g "$GROUP" -c onoff --command toggle -e "$ENDPOINT")
echo "$OUT"
assert_no_fallback "group invoke toggle (1st)"
assert_native_marker "groupcast sent (native)" "group invoke toggle (1st)"
assert_grep '"status":"sent"' "$OUT" "group invoke toggle が status:sent を返さない"
confirm "グループ($GROUP) 全メンバー ($TEST_NODES) の点灯状態が反転したことを目視確認してください"

for n in "${NODE_ARR[@]}"; do
  OUT=$(run_native "${STORE_ARG[@]}" read -n "$n" -e "$ENDPOINT" -c onoff -a on-off)
  echo "$OUT"
  assert_no_fallback "read on-off after toggle (node=$n)"
  AFTER=$(printf '%s' "$OUT" | jq -r '.value')
  if [ "$AFTER" = "${BEFORE[$n]}" ]; then
    echo "FAIL: node $n の on-off が toggle 後も反転していない (baseline=${BEFORE[$n]}, after=$AFTER)" >&2
    exit 1
  fi
done
echo "PASS: 検証3(前半) 全ノード on-off 反転確認 (native read)" >&2

OUT=$(run_native "${STORE_ARG[@]}" group invoke -g "$GROUP" -c onoff --command toggle -e "$ENDPOINT")
echo "$OUT"
assert_no_fallback "group invoke toggle (2nd, revert)"
assert_native_marker "groupcast sent (native)" "group invoke toggle (2nd, revert)"
assert_grep '"status":"sent"' "$OUT" "group invoke toggle（復元）が status:sent を返さない"

for n in "${NODE_ARR[@]}"; do
  OUT=$(run_native "${STORE_ARG[@]}" read -n "$n" -e "$ENDPOINT" -c onoff -a on-off)
  echo "$OUT"
  assert_no_fallback "read on-off after revert (node=$n)"
  AFTER=$(printf '%s' "$OUT" | jq -r '.value')
  if [ "$AFTER" != "${BEFORE[$n]}" ]; then
    echo "FAIL: node $n の on-off が復元 toggle 後に baseline と一致しない (baseline=${BEFORE[$n]}, after=$AFTER)" >&2
    exit 1
  fi
done
echo "PASS: 検証3 native groupcast（toggle→反転確認→逆toggleで復元、両方 native marker + fallback不在）" >&2

echo "== 6/7 検証4: --rebind 再実行 + rebind無し再実行の失敗確認"
# shellcheck disable=SC2086
OUT=$(run_native "${STORE_ARG[@]}" group provision -g "$GROUP" --nodes $TEST_NODES \
  --keyset-id "$KEYSET" --name "$GROUP_NAME" --rebind)
echo "$OUT"
assert_no_fallback "group provision --rebind"
assert_native_marker "group provision controller state written (native kvs)" "group provision --rebind (native kvs write)"
assert_native_marker "group provision executed (native direct)" "group provision --rebind (native direct 完走)"
assert_grep '"status":"provisioned"' "$OUT" "group provision --rebind が status:provisioned を返さない（Duplicate になった？）"
echo "PASS: 検証4(前半) --rebind 再実行が成功（Duplicate にならない）" >&2

set +e
# shellcheck disable=SC2086
OUT=$(run_native "${STORE_ARG[@]}" group provision -g "$GROUP" --nodes $TEST_NODES \
  --keyset-id "$KEYSET" --name "$GROUP_NAME")
RC=$?
set -e
if [ "$RC" -eq 0 ]; then
  echo "FAIL: --rebind 無しの再実行が成功してしまった（Duplicate 検出が効いていない）: $OUT" >&2
  exit 1
fi
if [ "$RC" -ne 1 ]; then
  echo "FAIL: --rebind 無し再実行の exit code が 1 (ErrorKind::Other) でない (got $RC)" >&2
  cat "$LAST_STDERR_FILE" >&2
  exit 1
fi
STDERR_OUT=$(cat "$LAST_STDERR_FILE")
assert_grep '"kind":"other"' "$STDERR_OUT" "--rebind 無し再実行の stderr に error.kind:other が無い"
assert_grep 'use --rebind' "$STDERR_OUT" "--rebind 無し再実行の stderr に '--rebind' 誘導の detail が無い"
echo "PASS: 検証4 --rebind 無し再実行は exit 1 + 'use --rebind' 誘導の detail で失敗" >&2

echo "== 7/7 検証5: chip-tool 互換"

CHIP_BIN="${CHIP_TOOL_BIN:-chip-tool}"

# 検証5(a): show-groups/show-keysets（主検証・FAIL対象）。単発を先に試し、
# 失敗（非ゼロ終了）なら interactive echo パイプへフォールバック（brief の
# 両対応指示 — 上記 judgment 参照、mat 自身の chip-tool フォールバック経路が
# groupsettings を単発で使っている実績があるため単発が主）。
chip_groupsettings_show() {
  # $1 = show-groups | show-keysets
  local sub=$1 rc=0 out
  out=$(ssh -n "$MAT_E2E_HOST" "$CHIP_BIN" groupsettings "$sub" --storage-directory "$REMOTE_STORE" 2>&1) || rc=$?
  if [ "$rc" -ne 0 ]; then
    echo "WARN: chip-tool groupsettings $sub の単発実行が失敗(exit $rc) — interactive echo パイプにフォールバック" >&2
    # -n を使わず、printf の出力をそのまま ssh のリモート stdin へパイプする
    # （interactive モードはコマンドを stdin から読むため、ここは意図的に
    # ローカル stdin を消費させる — 他の呼び出しはすべて -n で防御している）。
    out=$(printf 'groupsettings %s\nquit\n' "$sub" \
      | ssh "$MAT_E2E_HOST" "$CHIP_BIN" interactive start --storage-directory "$REMOTE_STORE" 2>&1) || true
  fi
  printf '%s' "$out"
}

SHOW_GROUPS_OUT=$(chip_groupsettings_show show-groups)
echo "$SHOW_GROUPS_OUT"
assert_grep "$GROUP_NAME" "$SHOW_GROUPS_OUT" \
  "chip-tool groupsettings show-groups に group 名 '$GROUP_NAME'（=mat が native KVS に書いた group）が現れない"

SHOW_KEYSETS_OUT=$(chip_groupsettings_show show-keysets)
echo "$SHOW_KEYSETS_OUT"
# assert_grep は BRE（grep -q、-E 無し）なので、数字境界を要る ERE alternation
# はここだけ直接 grep -Eq で書く（"9" のような短い ID が別の数の部分文字列に
# 誤マッチしないよう境界を要求する）。
if ! printf '%s' "$SHOW_KEYSETS_OUT" | grep -Eq "(^|[^0-9])$KEYSET([^0-9]|\$)"; then
  echo "FAIL: chip-tool groupsettings show-keysets に keyset $KEYSET（=mat が native KVS に書いた keyset）が現れない:" >&2
  printf '%s\n' "$SHOW_KEYSETS_OUT" >&2
  exit 1
fi
echo "PASS: 検証5(a) 実 chip-tool が native KVS 書込を読めた（groupsettings show-groups/show-keysets に group/keyset 確認）" >&2

# 検証5(b): groupcast 互換 best-effort（WARN 許容）。
echo "-- 検証5(b): g/gdc を native_group_counter+4096 で書き換えて chip-tool 経路 group invoke（best-effort）"
# remote cat の失敗（ファイル未作成等）を "|| true" で吸収する — set -e +
# pipefail 下で素の command substitution はパイプライン内の非ゼロ終了を
# そのままスクリプト全体の異常終了にしてしまうため（次の正規表現チェックで
# WARN として拾いたいだけで FAIL にはしない）。
NATIVE_COUNTER=$(ssh -n "$MAT_E2E_HOST" "cat '$REMOTE_STORE/native_group_counter' 2>/dev/null || true" | tr -d '[:space:]')
if ! [[ "$NATIVE_COUNTER" =~ ^[0-9]+$ ]]; then
  echo "WARN: 検証5(b) スキップ — $REMOTE_STORE/native_group_counter が読めない/数値でない (got '$NATIVE_COUNTER')" >&2
elif ! ssh -n "$MAT_E2E_HOST" "grep -q '^g/gdc=' '$REMOTE_STORE/chip_tool_config.ini'"; then
  echo "WARN: 検証5(b) スキップ — $REMOTE_STORE/chip_tool_config.ini に g/gdc 行が無い" >&2
elif ! ssh -n "$MAT_E2E_HOST" "test -f '$REMOTE_STORE/chip_tool_config.ini.bak-m8c2'"; then
  echo "WARN: 検証5(b) スキップ — KVS バックアップが実在しない; sed による g/gdc 書換は実行しません（(a) が主検証）" >&2
else
  NEW_GDC=$((NATIVE_COUNTER + 4096))
  GDC_B64=$(ssh -n "$MAT_E2E_HOST" "python3 -c \"import base64,struct;print(base64.b64encode(struct.pack('<I', $NEW_GDC)).decode())\"" 2>/dev/null) || GDC_B64=""
  if [ -z "$GDC_B64" ]; then
    echo "WARN: 検証5(b) スキップ — リモートの python3 で g/gdc エンコードに失敗（未搭載？）" >&2
  else
    echo "native_group_counter=$NATIVE_COUNTER → g/gdc を $NEW_GDC (base64=$GDC_B64) へ書き換え（chip-tool・matd 停止中）"
    if ! ssh -n "$MAT_E2E_HOST" "sed -i 's|^g/gdc=.*|g/gdc=$GDC_B64|' '$REMOTE_STORE/chip_tool_config.ini'"; then
      echo "WARN: 検証5(b) スキップ — g/gdc の sed 書き換えが失敗（(a) が主検証のため WARN 継続）" >&2
    else
      OUT=$(run_chip "${STORE_ARG[@]}" group invoke -g "$GROUP" -c onoff --command toggle -e "$ENDPOINT" 2>&1) \
        && CHIP_INVOKE_RC=0 || CHIP_INVOKE_RC=$?
      echo "$OUT"
      if [ "$CHIP_INVOKE_RC" -ne 0 ]; then
        echo "WARN: 検証5(b) chip-tool 経路 group invoke が失敗（exit $CHIP_INVOKE_RC）— (a) が主検証のため WARN 継続" >&2
      elif confirm_blink_yn "グループ($GROUP) が chip-tool 経路の group invoke で点滅しましたか？"; then
        echo "PASS: 検証5(b) chip-tool 経路 groupcast 互換（g/gdc調整後に点滅確認）" >&2
        # 復元 toggle（見た目を戻す。best-effort）
        run_chip "${STORE_ARG[@]}" group invoke -g "$GROUP" -c onoff --command toggle -e "$ENDPOINT" >/dev/null 2>&1 || true
      else
        echo "WARN: 検証5(b) 点滅未確認（(a) が主検証のため互換自体は証明済み、WARN 継続）" >&2
      fi
    fi
  fi
fi

echo "== 検証6: diag node --deep native + MAT_IFACE未設定chip-tool経路の健全性（node=${NODE_ARR[0]}）"
NODE0="${NODE_ARR[0]}"
DIAG_NATIVE_OUT=$(run_native "${STORE_ARG[@]}" diag node -n "$NODE0" --deep)
echo "$DIAG_NATIVE_OUT"
assert_no_fallback "diag node --deep (native operational/thread部分)"
assert_native_marker "diag node executed (native)" "diag node --deep (native)"
assert_grep '"verdict"' "$DIAG_NATIVE_OUT" "diag node --deep（native）が verdict を含まない"
HAS_MDNS=$(printf '%s' "$DIAG_NATIVE_OUT" | jq -e '.checks | has("mdns")' 2>/dev/null) || HAS_MDNS=false
if [ "$HAS_MDNS" != "true" ]; then
  echo "FAIL: diag node --deep（native）の checks.mdns が無い" >&2
  exit 1
fi
echo "PASS: 検証6(前半) diag node --deep native（marker + verdict + checks.mdns、fallback不在）" >&2

DIAG_CHIP_OUT=$(run_chip "${STORE_ARG[@]}" diag node -n "$NODE0" --deep)
echo "$DIAG_CHIP_OUT"
assert_grep '"verdict"' "$DIAG_CHIP_OUT" "diag node --deep（MAT_IFACE未設定/chip-tool経路）が verdict を含まない"
echo "PASS: 検証6 diag node --deep（MAT_IFACE未設定でも従来どおり成功）" >&2

echo "== 後片付け（best-effort、失敗は WARN）"
for n in "${NODE_ARR[@]}"; do
  run_native "${STORE_ARG[@]}" invoke -n "$n" -e "$ENDPOINT" -c groups --command remove-group "$GROUP" >/dev/null 2>&1 \
    && echo "device-side remove-group OK (node=$n)" \
    || echo "WARN: device-side groups remove-group が失敗 (node=$n, best-effort)" >&2
done
ssh -n "$MAT_E2E_HOST" "$CHIP_BIN" groupsettings remove-group "$GROUP" --storage-directory "$REMOTE_STORE" >/dev/null 2>&1 \
  && echo "controller-side groupsettings remove-group OK" \
  || echo "WARN: controller-side groupsettings remove-group が失敗 (best-effort)" >&2
ssh -n "$MAT_E2E_HOST" "$CHIP_BIN" groupsettings remove-keyset "$KEYSET" --storage-directory "$REMOTE_STORE" >/dev/null 2>&1 \
  && echo "controller-side groupsettings remove-keyset OK" \
  || echo "WARN: controller-side groupsettings remove-keyset が失敗 (best-effort)" >&2

echo "sudo systemctl restart matd（group state 再読込 + living_lights 検証のため起動）"
ssh -n "$MAT_E2E_HOST" "sudo systemctl restart matd"
for i in $(seq 1 10); do
  ssh -n "$MAT_E2E_HOST" "sudo systemctl is-active --quiet matd" && break
  sleep 1
done

echo "living_lights (group 10) 無傷確認（matd 経由 off→on）"
ssh -n "$MAT_E2E_HOST" "$REMOTE_BIN" "${STORE_ARG[@]}" group invoke -g 10 -c onoff --command off >/dev/null
confirm "living_lights (group 10) 全メンバーが消灯したことを目視確認してください"
ssh -n "$MAT_E2E_HOST" "$REMOTE_BIN" "${STORE_ARG[@]}" group invoke -g 10 -c onoff --command on >/dev/null
confirm "living_lights (group 10) 全メンバーが点灯したことを目視確認してください"
echo "PASS: 検証7 後片付け + living_lights 無傷確認（matd 経由 off/on）" >&2

echo "== e2e:m8c2:real PASS（検証1〜7 GREEN。検証5(b) は best-effort — 上記ログの WARN/PASS 参照）"
