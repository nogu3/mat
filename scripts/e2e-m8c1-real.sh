#!/usr/bin/env bash
# Phase 5 M8c-1 受け入れ: `mat commission` の native 直経路（MAT_IFACE 設定時、
# mDNS→BLE 自動フォールバック）が chip-tool を spawn せず完走すること
# （on-network / BLE+Thread 両経路）、native 経路で commission したノードへの
# 制御（on/off）が通ること、MAT_IFACE 未設定時の chip-tool 経路が引き続き
# 健全であることを、jarvis 上・実機の玄関ライト（現在 fabric 無し、BLE
# commissionable）で検証する。matd には触れない（commission は恒久的に matd
# 対象外 — crates/mat/src/cli.rs の `matd` フラグの doc コメント参照。native
# 直経路のみが対象）。
#
# ビルドが従来のハーネス（e2e-m7/m8a/m8b-real.sh の rust-lld musl 経路）と
# **異なる**: BLE feature は bluer → libdbus（C 依存）を使うため musl では
# 組めない（M8c-1 design doc のビルド検証スパイクの結論）。M6b で確立済みの
# `cross`（docker）+ aarch64-unknown-linux-gnu + arm64 libdbus（Cross.toml
# 参照）でビルドし、jarvis（Debian 13, glibc）向けの動的リンクバイナリを
# scp する。`file` で aarch64 動的リンク（NOT musl 静的）であることを確認する
# ——ここを取り違えると常態化した musl 経路の stale バイナリを誤って転送し、
# BLE feature 無し（＝BLE 経路が常に "no BLE support" で Unavailable）のまま
# 検証2 が静かに壊れる。
#
# setup code（QR ペイロード `MT:...` または 11/21桁 manual code）はハーネス
# 引数・環境変数にせず、実行時プロンプトで人力入力する（デバイス毎に異なり、
# repo は public のためコミット不可 — brief の明示指定）。**QR ペイロードを
# 推奨**: manual code は BLE 経路を持たない（mat-native::commission::commission
# 参照 — manual code で mDNS 空振りだと即 Unavailable、BLE を試さない）ため、
# 検証2（mDNS に居ないはずの玄関ライトを BLE 経由で拾う）には QR が必須。
# 本ハーネスはこの理由からプロンプトで QR を明示的に案内する。
#
# 検証項目（brief 通し番号）:
#   1. ビルド: cross build（gnu + features ble）→ scp → file で動的リンク確認。
#   2. native BLE+Thread commission: mDNS 空振り→BLE 経由で玄関ライトを
#      jarvis 本番 fabric へ commission。chip-tool 不在
#      （MAT_CHIP_TOOL_BIN=/nonexistent/...）でも成功 = 純 native の実証。
#      stderr の positive marker "commission executed (native ble-thread)" も
#      grep（marker だけでなく、chip-tool 不在下での成功そのものが二重の
#      実証になる — M8a Task11 の教訓を踏襲）。台帳記録（node_id 採番）は
#      stdout の JSON から確認。
#   3. 制御確認: 検証2 で得た新 node_id に対する `mat on` / `mat off`
#      （native 直経路）。玄関ライトが実際に点滅する — 目視確認込み。
#   4. on-network 経路（任意・人力判断）: RemoveFabric（device 側の
#      current-fabric-index を native read で取得し、native invoke で
#      operationalcredentials/remove-fabric へ渡す）→ 同一 setup code で
#      on-network 再 commission（"commission executed (native on-network)"
#      marker）。玄関ライトの運用状況次第で危険性があるため WARN + 人力
#      確認で実施可否を選べる（デフォルトはスキップ側 — M8b 同様「状態依存の
#      検証は FAIL にしない」流儀）。
#   5. フォールバック健全性: (a) chip-tool 経路の生存確認（MAT_IFACE 未設定 +
#      無害な `mat discover`）。(b) bogus iface + MAT_CHIP_TOOL_BIN=/nonexistent
#      で exit 12 + stderr に "falling back to chip-tool" — フォールバック
#      境界が機能していることの実証。setup code は well-known な chip-tool
#      testvector（"MT:-24J0AFN00KA0648G00" — connectedhomeip all-clusters-app
#      の公開テストペイロード、mat-controller::setup_code のユニットテストにも
#      使われている。実デバイスの資格情報ではない）を使う。iface_index の
#      解決が setup code の parse 直後・PASE 開始より前に失敗するため、この
#      経路は玄関ライト（や他のどのデバイスにも）に一切ワイヤ接触しない。
#
# ★judgment: 検証5(b) は「bogus iface + 実 chip-tool + 実 QR」にしない
# （brief の instructions 通り）。native が iface 解決で Unavailable になり
# chip-tool へフォールバックした場合、実 chip-tool は実際に discovery を
# 開始し、実デバイスへ二重 commission を試みかねない（本番運用中の他ノード
# を巻き込むリスク）。bogus iface + 不在 chip-tool binary の組が「フォール
# バック境界が機能していること」を実証しつつ、どのデバイスにも一切触れない
# 最も安全な検証方法。
#
# ★judgment: 検証4 の fabric-index は `MAT_E2E_FABRIC_INDEX`（jarvis コント
# ローラ側の fabric テーブル index、既定 2）を流用せず、対象デバイス自身の
# `current-fabric-index` 属性を native read で読み直す。RemoveFabric の
# 引数はデバイス側のローカルな fabric-index であり、コントローラ側の
# fabric テーブル index と数値が一致する保証がないため。
#
# ★judgment: 検証4 は成功するまで最大 3 回、3 秒間隔でリトライする
# （RemoveFabric 直後にデバイスが commissioning window を再オープンするまで
# 数秒かかる実装が一般的なため）。3 回失敗しても FAIL にせず WARN — デバイス
# 状態依存（M8b 同様の流儀）。
#
# 必須 env: MAT_E2E_HOST（ssh 先。repo は public のため既定値を置かない）
# 任意 env: MAT_E2E_IFACE（既定 eth0）
#           MAT_E2E_FABRIC_INDEX（既定 2、jarvis 本番の controller 側 fabric
#             テーブル index）
#           MAT_E2E_STORE（既定: バイナリ自身のデフォルト解決 = jarvis の
#             本番ストア相当。指定時のみ --store を渡す）
#           MAT_E2E_CHIP_TOOL_BIN（検証5(a) の chip-tool 生存確認で使う実
#             chip-tool のパス。未指定なら ssh 先 PATH 任せ）
#           MAT_E2E_ASSUME_YES=1（確認プロンプトを自動化。検証4 は危険側の
#             操作のため、この場合は既定でスキップ側に倒す — 下記 confirm_yn
#             実装参照）
#           MAT_E2E_SETUP_CODE（玄関ライトの QR ペイロード "MT:..."。非対話
#             実行用。未設定なら実行時プロンプト。repo にはコミットしない）
#           MAT_E2E_THREAD_DATASET（active operational dataset の hex。network
#             key を含む秘匿値 — repo にコミットしない。BR ホストの
#             `ot-ctl dataset active -x` か HA OTBR の Web UI/SSH アドオンから）
# ローカル要件: cross（docker 経由の aarch64-unknown-linux-gnu クロスビルド。
#   Cross.toml が arm64 libdbus の pre-build を設定済み）、jq（JSON 抽出に
#   使う。ローカル側で実行する。ssh 先（jarvis）には不要）
set -euo pipefail
cd "$(dirname "$0")/.."

: "${MAT_E2E_HOST:?MAT_E2E_HOST (ssh host) required}"
command -v jq >/dev/null 2>&1 || { echo "jq が必要です（commission/read の JSON 抽出に使用）" >&2; exit 1; }
command -v cross >/dev/null 2>&1 || { echo "cross が必要です（BLE feature は libdbus C 依存のため musl 直ビルド不可 — Cross.toml 参照）" >&2; exit 1; }

IFACE="${MAT_E2E_IFACE:-eth0}"
FABRIC_INDEX="${MAT_E2E_FABRIC_INDEX:-2}"
STORE="${MAT_E2E_STORE:-}"
CHIP_TOOL_BIN="${MAT_E2E_CHIP_TOOL_BIN:-}"
TARGET=aarch64-unknown-linux-gnu
REMOTE_BIN=/tmp/mat-m8c1

# connectedhomeip all-clusters-app の公開テストペイロード（VID 0xFFF1 / PID
# 0x8001 / discriminator 3840 / passcode 20202021）。mat-controller::setup_code
# のユニットテストにも使われている既知の定数で、実デバイスの資格情報では
# ない。検証5(b) のフォールバック境界確認専用（bogus iface で iface_index
# 解決に失敗する = PASE 開始前 = このコードが実際に解釈されることはない）。
DUMMY_QR="MT:-24J0AFN00KA0648G00"

confirm() {
  # $1 = 目視確認を促す文面。Enter で続行、Ctrl-C で中断（M8a/M8b 同様）。
  echo ""
  echo ">>> $1"
  if [ "${MAT_E2E_ASSUME_YES:-0}" = "1" ]; then
    echo ">>> (MAT_E2E_ASSUME_YES=1: 自動確認で続行)"
    return
  fi
  read -r -p ">>> 確認できたら Enter で続行 (Ctrl-C で中断): " _
}

confirm_yn() {
  # $1 = 実行してよいか問う文面。y/Y のみ実行（戻り値0）、それ以外は
  # スキップ（戻り値1）。MAT_E2E_ASSUME_YES=1 でも既定はスキップ側 ——
  # 検証4（RemoveFabric を伴う）はデバイス状態に応じて危険側になり得るため、
  # 「自動確認 = 危険操作も自動実行」にはしない（他の confirm() とは非対称、
  # 本関数固有の判断）。
  echo ""
  echo ">>> $1"
  if [ "${MAT_E2E_ASSUME_YES:-0}" = "1" ]; then
    echo ">>> (MAT_E2E_ASSUME_YES=1: 危険操作のため既定でスキップ)"
    return 1
  fi
  local ans
  read -r -p ">>> 実行しますか？ [y/N]: " ans
  case "$ans" in
    y|Y) return 0 ;;
    *) return 1 ;;
  esac
}

echo "== 1/6 cross build (mat, $TARGET, features ble — docker 経由、Cross.toml の arm64 libdbus pre-build 使用)"
cross build --release --target "$TARGET" -p mat --features ble
MAT_BIN="target/$TARGET/release/mat"
FILE_OUT=$(file "$MAT_BIN")
echo "$FILE_OUT"
printf '%s' "$FILE_OUT" | grep -q 'aarch64' || { echo "FAIL: aarch64 バイナリでない: $FILE_OUT" >&2; exit 1; }
printf '%s' "$FILE_OUT" | grep -q 'dynamically linked' || {
  echo "FAIL: 動的リンクでない（musl 静的ビルドの取り違え？ BLE feature 無しの stale バイナリの疑い）: $FILE_OUT" >&2
  exit 1
}

# 直近呼び出しの stderr（ローカル一時ファイル、呼び出しのたびに上書きされる）。
LAST_STDERR_FILE=$(mktemp)

cleanup() {
  echo "== cleanup: ssh 先の一時バイナリ削除 ($MAT_E2E_HOST) =="
  ssh -n "$MAT_E2E_HOST" "rm -f $REMOTE_BIN" || true
  rm -f "$LAST_STDERR_FILE"
}
trap cleanup EXIT

echo "== 2/6 転送 → $MAT_E2E_HOST"
# ssh cat 方式（scp は ssh-agent の状態に左右される、既存 e2e-*-real.sh に倣う）。
# 別名 (/tmp/mat-m8c1) で置き、本番 /usr/local/bin/mat とは衝突させない。
ssh "$MAT_E2E_HOST" "cat > $REMOTE_BIN && chmod +x $REMOTE_BIN" < "$MAT_BIN"

STORE_ARG=()
[ -n "$STORE" ] && STORE_ARG=(--store "$STORE")

# ---- runner 群 ----
# 共通: MAT_MATD=0（commission は恒久的に matd 対象外だが、on/off・invoke・
# read は matd 対応 op のため、production matd が動いていても本ハーネスの
# 直経路検証を確実に踏ませる belt-and-braces）+ MAT_LOG=info（positive
# marker を info レベルで確実に出す）。

# 検証2: native BLE+Thread commission の純 native 実証。chip-tool 不在。
run_native_ble() {
  ssh -n "$MAT_E2E_HOST" \
    MAT_MATD=0 MAT_LOG=info "MAT_IFACE=$IFACE" "MAT_FABRIC_INDEX=$FABRIC_INDEX" \
    "MAT_THREAD_DATASET=$THREAD_DATASET" \
    MAT_CHIP_TOOL_BIN=/nonexistent/mat-e2e-m8c1-chip-tool \
    "$REMOTE_BIN" "$@" 2>"$LAST_STDERR_FILE"
}

# 検証3・4: native 直経路（on/off・read・invoke・on-network commission）の
# 純 native 実証。chip-tool 不在（thread-dataset は不要 — BLE 経路を試させ
# ないことで on-network 経路の判定を汚さない）。
run_native() {
  ssh -n "$MAT_E2E_HOST" \
    MAT_MATD=0 MAT_LOG=info "MAT_IFACE=$IFACE" "MAT_FABRIC_INDEX=$FABRIC_INDEX" \
    MAT_CHIP_TOOL_BIN=/nonexistent/mat-e2e-m8c1-chip-tool \
    "$REMOTE_BIN" "$@" 2>"$LAST_STDERR_FILE"
}

# 検証5(a): MAT_IFACE 未設定、実 chip-tool（生存確認）。
run_chip() {
  local envs=(MAT_MATD=0 MAT_LOG=info)
  [ -n "$CHIP_TOOL_BIN" ] && envs+=("MAT_CHIP_TOOL_BIN=$CHIP_TOOL_BIN")
  ssh -n "$MAT_E2E_HOST" "${envs[@]}" "$REMOTE_BIN" "$@" 2>"$LAST_STDERR_FILE"
}

# 検証5(b): 存在しない iface 名 + chip-tool 不在 → フォールバック境界の実証
# （iface_index 解決失敗 = ワイヤ未接触、どのデバイスにも触れない）。
run_fallback() {
  ssh -n "$MAT_E2E_HOST" \
    MAT_MATD=0 MAT_LOG=info MAT_IFACE=mat-e2e-m8c1-bogus-iface "MAT_FABRIC_INDEX=$FABRIC_INDEX" \
    MAT_CHIP_TOOL_BIN=/nonexistent/mat-e2e-m8c1-chip-tool \
    "$REMOTE_BIN" "$@" 2>"$LAST_STDERR_FILE"
}

assert_no_fallback() {
  # $1 = 説明（省略可）
  if grep -q "falling back" "$LAST_STDERR_FILE"; then
    echo "FAIL: ${1:-op} — stderr contains 'falling back' — op did not run native:" >&2
    cat "$LAST_STDERR_FILE" >&2
    exit 1
  fi
}

# positive 実証: commission.rs / native_direct.rs が成功時に出す
# "... executed (native ...)" を直接 grep する（assert_no_fallback だけでは、
# 警告ゼロのまま静かに fallback するような将来的な回帰を検出できないための
# 二重チェック。M8a Task11 の教訓を踏襲）。
# $1 = grep パターン, $2 = 説明（省略可）
assert_native_marker() {
  if ! grep -q -- "$1" "$LAST_STDERR_FILE"; then
    echo "FAIL: ${2:-op} — stderr に native 実行の positive marker '$1' が無い（native で走った実証なし）:" >&2
    cat "$LAST_STDERR_FILE" >&2
    exit 1
  fi
}

echo "== 3/6 検証2: Thread dataset 取得 → native BLE+Thread commission（玄関ライト）"
# jarvis は border router では**ない**（BR 群は LAN 上の別デバイス）ため
# ot-ctl は使えない。M6b と同じく MAT_E2E_THREAD_DATASET（active operational
# dataset の hex、network key を含む秘匿値 — repo にコミットしない）で注入する。
# 取得例: BR ホストで `sudo ot-ctl dataset active -x`、または HA の Thread
# 統合（OTBR アドオン）から。
THREAD_DATASET=$(printf '%s' "${MAT_E2E_THREAD_DATASET:-}" | tr -d '\r\n ')
if ! printf '%s' "$THREAD_DATASET" | grep -Eq '^[0-9a-fA-F]+$'; then
  echo "FAIL: MAT_E2E_THREAD_DATASET (hex の Thread active operational dataset) が必要" >&2
  exit 1
fi
echo "Thread dataset OK (${#THREAD_DATASET} hex chars)"

echo ""
echo ">>> 玄関ライトの setup code を QR ペイロード（\"MT:...\"）で指定してください。"
echo ">>> manual code（11/21桁）は BLE 経路が無いため本検証には使えません。"
echo ">>> デバイス印字の QR は repo にコミットしません（env/プロンプト入力のみ）。"
# MAT_E2E_SETUP_CODE（env 注入）を優先。未設定なら対話プロンプト。
# 非対話実行（バックグラウンド + パイプ）では read が途中の ssh に stdin を
# 食われて空振りするため、その場合は env で渡すこと。
SETUP_CODE="${MAT_E2E_SETUP_CODE:-}"
if [ -z "$SETUP_CODE" ]; then
  read -r -p ">>> setup code: " SETUP_CODE
fi
case "$SETUP_CODE" in
  MT:*) : ;;
  *) echo "FAIL: setup code は QR ペイロード（MT:...）である必要があります: '$SETUP_CODE'" >&2; exit 1 ;;
esac

COMMISSION_OUT=$(run_native_ble "${STORE_ARG[@]}" commission --target "ble-thread" --setup-code "$SETUP_CODE")
echo "$COMMISSION_OUT"
assert_no_fallback "commission (native ble+thread)"
assert_native_marker "commission executed (native ble-thread)" "commission (native ble+thread)"

NEW_NODE=$(printf '%s' "$COMMISSION_OUT" | jq -r '.node_id')
STATUS=$(printf '%s' "$COMMISSION_OUT" | jq -r '.status')
if [ -z "$NEW_NODE" ] || [ "$NEW_NODE" = "null" ] || [ "$STATUS" != "success" ]; then
  echo "FAIL: commission 出力に node_id/status:success が無い" >&2
  exit 1
fi
echo "PASS: 検証2 native BLE+Thread commission（node_id=$NEW_NODE、chip-tool 不在下で成功、marker確認、fallback不在）" >&2

echo "== 4/6 検証3: 制御確認 mat on/off（native, node=$NEW_NODE）"
run_native "${STORE_ARG[@]}" on -n "$NEW_NODE" >/dev/null
assert_no_fallback "on (native, node=$NEW_NODE)"
confirm "玄関ライトが点灯したことを目視確認してください"

run_native "${STORE_ARG[@]}" off -n "$NEW_NODE" >/dev/null
assert_no_fallback "off (native, node=$NEW_NODE)"
confirm "玄関ライトが消灯したことを目視確認してください"
echo "PASS: 検証3 制御確認（on/off 両方 native、chip-tool 不在下で成功、fallback不在）" >&2

echo "== 5/6 検証4（任意）: on-network 再commission（RemoveFabric → 同一 setup code で再commission）"
echo "WARN: この検証は玄関ライトを一時的に fabric-less（未commission）状態に戻します。失敗すると再commission待ちの状態が残ります。" >&2
if confirm_yn "検証4（RemoveFabric → on-network 再commission）を実行しますか"; then
  echo "-- デバイス自身の current-fabric-index を native read で取得"
  FABRIC_IDX_OUT=$(run_native "${STORE_ARG[@]}" read -n "$NEW_NODE" -e 0 -c operationalcredentials -a current-fabric-index)
  echo "$FABRIC_IDX_OUT"
  assert_no_fallback "read current-fabric-index (native, node=$NEW_NODE)"
  assert_native_marker "read executed (native direct)" "read current-fabric-index (native, node=$NEW_NODE)"
  DEVICE_FABRIC_IDX=$(printf '%s' "$FABRIC_IDX_OUT" | jq -r '.value')
  if [ -z "$DEVICE_FABRIC_IDX" ] || [ "$DEVICE_FABRIC_IDX" = "null" ]; then
    echo "FAIL: current-fabric-index の読み出しに失敗" >&2
    exit 1
  fi
  echo "device 側 fabric-index = $DEVICE_FABRIC_IDX"

  echo "-- RemoveFabric（native invoke, fabric-index=$DEVICE_FABRIC_IDX）"
  run_native "${STORE_ARG[@]}" invoke -n "$NEW_NODE" -e 0 -c operationalcredentials --command remove-fabric "$DEVICE_FABRIC_IDX" >/dev/null
  assert_no_fallback "invoke remove-fabric (native, node=$NEW_NODE)"
  assert_native_marker "invoke executed (native direct)" "invoke remove-fabric (native, node=$NEW_NODE)"
  echo "RemoveFabric 完了 — 玄関ライトは fabric-less（Thread 残留）のはず"

  echo "-- 同一 setup code で on-network 再commission（commissioning window 再オープン待ちのため最大3回リトライ）"
  ONNET_OK=0
  for i in 1 2 3; do
    if ONNET_OUT=$(run_native "${STORE_ARG[@]}" commission --target "on-network-recommission" --setup-code "$SETUP_CODE" --node "$NEW_NODE"); then
      ONNET_OK=1
      break
    fi
    echo "retry $i/3: on-network 再commission 失敗、3秒後に再試行します" >&2
    sleep 3
  done

  if [ "$ONNET_OK" = "1" ]; then
    echo "$ONNET_OUT"
    assert_no_fallback "commission (native on-network, retry)"
    assert_native_marker "commission executed (native on-network)" "commission (native on-network, retry)"
    echo "PASS: 検証4 on-network 再commission（node_id=$NEW_NODE 継続、marker確認、fallback不在）" >&2
  else
    echo "WARN: 検証4 on-network 再commission が3回とも失敗 — 玄関ライトの commissioning window 再オープンが間に合わなかった可能性（デバイス状態依存、FAILにしない）。玄関ライトは fabric-less のままの可能性があるため、後で改めて手動 commission してください。" >&2
  fi
else
  echo "SKIP: 検証4（人力判断によりスキップ）" >&2
fi

echo "== 6/6 検証5: フォールバック健全性"

echo "-- 検証5(a): chip-tool 経路の生存確認（MAT_IFACE 未設定、無害な discover）"
CHIP_LIVE_OUT=$(run_chip "${STORE_ARG[@]}" discover)
echo "$CHIP_LIVE_OUT"
printf '%s' "$CHIP_LIVE_OUT" | jq -e 'has("devices")' >/dev/null || {
  echo "FAIL: chip-tool 経路の discover が devices を含む JSON を返さない" >&2
  exit 1
}
echo "PASS: 検証5(a) chip-tool 経路生存確認（discover が構造化 JSON を返す）" >&2

echo "-- 検証5(b): フォールバック境界（bogus iface + chip-tool 不在、dummy QR — どのデバイスにも触れない）"
FALLBACK_EXIT=0
run_fallback commission --target "bogus" --setup-code "$DUMMY_QR" >/dev/null || FALLBACK_EXIT=$?
if [ "$FALLBACK_EXIT" -ne 12 ]; then
  echo "FAIL: フォールバック健全性 — 期待 exit 12（child_not_found）、実際 $FALLBACK_EXIT" >&2
  cat "$LAST_STDERR_FILE" >&2
  exit 1
fi
grep -q "falling back to chip-tool" "$LAST_STDERR_FILE" || {
  echo "FAIL: フォールバック健全性 — stderr に 'falling back to chip-tool' が無い:" >&2
  cat "$LAST_STDERR_FILE" >&2
  exit 1
}
echo "PASS: 検証5(b) フォールバック境界（exit 12 + 'falling back to chip-tool'、ワイヤ未接触）" >&2

echo "== e2e:m8c1:real PASS（検証1〜3・5 GREEN。検証4は上記参照 — 任意/デバイス状態依存）"
