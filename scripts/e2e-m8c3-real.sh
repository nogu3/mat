#!/usr/bin/env bash
# Phase 5 M8c-3 実機 E2E ゲート 1（native 既定化、Stage 1）。
#
# Stage 1 のゴールは「`MAT_IFACE` / `MAT_MATD_IFACE` を明示 unset しても、
# jarvis 実運用 fabric で全 op が native 経路で完走し、chip-tool フォール
# バックが一度も発火しないこと」（design: docs/superpowers/specs/
# 2026-07-17-phase5-m8c3-native-default-design.md 設計3・ゲート1 参照）。
# chip-tool フォールバックのコードはこの段階ではまだ生きている（Stage 2 で
# 撤去）ため、ゲート1が FAIL しても本番影響なく撤退できる。
#
# 本ハーネスは STAGE 環境変数（既定 1）で二相に分岐する:
#   STAGE=1（既定）— 本 Task（Task7）が実装するゲート1の検証一式。
#   STAGE=2        — chip-tool 完全撤去後の最終受け入れ（Task13 で実装、
#                     本 Task では未実装スタブ）。
#
# 骨格は scripts/e2e-m8c2-real.sh / e2e-m8c1-real.sh / e2e-m8a-real.sh を流用
# （trap 後始末・stderr への PASS/FAIL・positive marker 二重チェック・musl
# rust-lld クロスビルド・`ssh -n` の作法・KVS バックアップの実在ガード）。
# 本 Task と異なる新規パターン:
#   - 直経路呼び出しはすべて `env -u MAT_IFACE -u MAT_MATD_IFACE` を明示付与
#     する（m8c1/m8c2 は逆に `MAT_IFACE=$IFACE` を明示注入していた — 本
#     ゲートの主題そのものが「未設定でも native」なので、リモートログイン
#     シェルが偶然どちらかを export していても揺らがないよう確実に外す）。
#   - matd も同様に `MAT_MATD_IFACE` を外した一時インスタンスを
#     `nohup` で起動する（e2e-m8a-real.sh の start_matd() パターンを流用、
#     本番 systemd matd の port/socket とは別にして衝突を避ける）。
#   - epoch 採用永続の実測（検証5）は m8c1 の「RemoveFabric → 同一 setup
#     code で on-network 再commission」パターンを再利用する。epoch キー
#     は fabric 単位（`mat/f/<idx>/ipk-epoch`、ノード単位ではない）なので、
#     使い捨てノード1台に対してこのサイクルを2回回せば「初回 adopt」と
#     「2回目は読み出しのみ（marker 不在）」の両方を1台で検証できる —
#     2台目の実機を用意する必要がない。
#     ★judgment: KVS に既に epoch キーがある状態（前回実行の再実行等）でも
#     壊れないよう、実行前の存在有無で期待値を切り替える（無ければ1回目で
#     adopt marker 必須・2回目は不在、既にあれば1回目から不在）— brief の
#     文言はそのまま「新規は1回目で adopt」を検証するが、ゲート再実行への
#     耐性として存在確認を先に読み、分岐で判定する（brief にない追加の
#     安全策 — 詳細は report 参照）。
#   - epoch キーの値は秘匿材料（IPK 導出の種）なので、存在確認は必ず
#     `grep -q`（値を一切 stdout/stderr に出さない）で行う。KVS の値行を
#     `cat` や `echo` で出力する処理はこのハーネスには存在しない。
#
# 検証項目（brief の STAGE=1 通し番号、実装箇所は下の "== N/7" コメント参照）:
#   1. 準備: musl クロスビルド（mat + matd、BLE 不要 — commission は
#      on-network のみ）→ scp → matd 停止（trap で必ず復帰）→ KVS backup。
#   2. env 未設定 native スイープ（直経路）: discover / read / write /
#      invoke / describe / diag thread / diag node --deep / open-window /
#      group provision（使い捨て group 99、--rebind 含む）/ group invoke。
#      各実行の stderr に "iface auto-selected (native default)"。
#   3. matd 経路: matd を MAT_MATD_IFACE 無しで起動
#      （"iface auto-selected (matd native default)" 確認）→
#      read / write / group invoke が matd 経由で成功。
#   4. フォールバック発火ゼロ: 全ログ結合 + "falling back" 0件
#      （assert_no_fallback）。直経路呼び出しは全て MAT_CHIP_TOOL_BIN=
#      /nonexistent/... を伴う（spawn があれば exit 12 で即 FAIL する
#      二重チェック — M8a Task11 以来の作法）。
#   5. epoch 採用永続: 使い捨てノードで RemoveFabric→on-network 再commission
#      を2回実施。1回目で "ipk epoch adopted (kvs)" + INI にキー出現、
#      2回目で marker 不在（読み出し経路）を確認。危険操作
#      （デバイスを一時的に fabric-less にする）なので confirm_yn ゲート
#      （既定はスキップ側 — m8c1 の verification4 と同じ判断）。
#   6. iface 一意選択の実測: jarvis 上の marker が eth0（tailscale0 でない）。
#   7. 後始末: 使い捨て group 99 の controller/device 両側除去（best-effort）、
#      matd 復帰。KVS backup は「実在ガード付きで手動復元できる」案内のみ
#      （m8c2 と同じ方針 — epoch 採用永続はこのゲートの目的そのものであり
#      自動 restore で消してしまうと Stage 1 のゴールを自ら破壊するため、
#      検証5専用の使い捨てノードの後始末以外は自動 restore しない）。
#
# 必須 env: MAT_E2E_HOST（ssh 先。repo は public のため既定値を置かない）
#           MAT_E2E_NODE（read/write/invoke/describe/diag/open-window
#             スイープ対象の commission 済み node_id。単一ノード）
#           MAT_E2E_GROUP_NODES（使い捨て group 99 provision 対象の
#             commission 済み node_id を空白区切りで、1つ以上）
# 任意 env: MAT_E2E_FABRIC_INDEX（既定 2、jarvis 本番の controller 側 fabric
#             テーブル index — epoch キー名 `mat/f/<idx>/ipk-epoch` の
#             <idx> にもこの値を使う。CLI 既定値は 1 だが jarvis の実値は
#             2 — m8c1/m8c2 ハーネスと同じ既定）
#           MAT_E2E_GROUP_ID（既定 99） / MAT_E2E_KEYSET_ID（既定 99）
#           MAT_E2E_ENDPOINT（既定 1）
#           MAT_E2E_STORE（既定: バイナリ自身のデフォルト解決）
#           MAT_E2E_CHIP_TOOL_BIN（後始末の実 chip-tool groupsettings 呼出に
#             使うパス。未指定なら ssh 先 PATH 任せ）
#           MAT_E2E_SOCKET（一時 matd の unix socket、既定
#             /tmp/matd-e2e-m8c3.sock、本番 matd とは別）
#           MAT_E2E_MATD_PORT（一時 matd の ws port、既定 9115、本番 9100 /
#             e2e-m8a の 9112 と衝突しない値）
#           MAT_E2E_ASSUME_YES=1（確認プロンプトを自動化。検証5は危険操作の
#             ため、この場合は既定でスキップ側に倒す — confirm_yn 実装参照）
#           MAT_E2E_EPOCH_NODE（検証5用の使い捨てノード。confirm_yn で
#             検証5の実施を選んだ場合のみ必須 — 未設定ならその場でエラー）
#           MAT_E2E_EPOCH_SETUP_CODE（検証5対象ノードの QR/manual setup
#             code。未設定なら実行時プロンプト。repo にはコミットしない）
# ローカル要件: jq（JSON 抽出に使用）
# STAGE=2 の要件は Task13 で追記する（現時点はスタブ、下記 stage2_main 参照）。
set -euo pipefail
cd "$(dirname "$0")/.."

STAGE="${STAGE:-1}"

# ---------------------------------------------------------------------------
# 共有ヘルパ（STAGE=1/2 双方から呼べるよう、副作用のある処理は関数化して
# ここには置かない。env 必須チェック・ビルド・KVS backup は stage1_main 内）。
# ---------------------------------------------------------------------------

confirm() {
  # $1 = 目視確認を促す文面。Enter で続行、Ctrl-C で中断（m8a/m8c1/m8c2 同様）。
  echo ""
  echo ">>> $1"
  if [ "${MAT_E2E_ASSUME_YES:-0}" = "1" ]; then
    echo ">>> (MAT_E2E_ASSUME_YES=1: 自動確認で続行)"
    return
  fi
  read -r -p ">>> 確認できたら Enter で続行 (Ctrl-C で中断): " _
}

confirm_yn() {
  # $1 = 実行してよいか問う文面。y/Y のみ実行（戻り値0）、他はスキップ
  # （戻り値1）。MAT_E2E_ASSUME_YES=1 でも既定はスキップ側 —
  # 検証5（RemoveFabric を伴う危険操作）はデバイス状態に応じて危険側に
  # なり得るため「自動確認 = 危険操作も自動実行」にはしない
  # （m8c1 の confirm_yn と同じ判断、非対称な安全側デフォルト）。
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

assert_grep() {
  # $1 = grep パターン, $2 = 対象文字列, $3 = 説明
  if ! printf '%s' "$2" | grep -q -- "$1"; then
    echo "FAIL: $3 — expected pattern '$1' not found in:" >&2
    printf '%s\n' "$2" >&2
    exit 1
  fi
}

assert_no_fallback_in() {
  # $1 = 対象ログファイル, $2 = 説明（省略可）
  if grep -q "falling back" "$1"; then
    echo "FAIL: ${2:-op} — stderr contains 'falling back' — op did not run native:" >&2
    cat "$1" >&2
    exit 1
  fi
}

assert_no_fallback() {
  # $1 = 説明（省略可）。直近呼び出し（$LAST_STDERR_FILE）に対する簡便版。
  assert_no_fallback_in "$LAST_STDERR_FILE" "${1:-op}"
}

# positive 実証（M8a Task11 の教訓 — marker 不在のまま静かに fallback する
# 回帰を assert_no_fallback だけでは検出できないための二重チェック）。
# $1 = grep パターン, $2 = 説明（省略可）, $3 = 対象ファイル（省略時 $LAST_STDERR_FILE）
assert_native_marker() {
  local file=${3:-$LAST_STDERR_FILE}
  if ! grep -q -- "$1" "$file"; then
    echo "FAIL: ${2:-op} — stderr に native 実行の positive marker '$1' が無い（native で走った実証なし）:" >&2
    cat "$file" >&2
    exit 1
  fi
}

# 逆方向 assert（検証5・2回目の adopt marker 不在確認用）。
# $1 = grep パターン, $2 = 説明, $3 = 対象ファイル
assert_marker_absent() {
  local file=${3:-$LAST_STDERR_FILE}
  if grep -q -- "$1" "$file"; then
    echo "FAIL: ${2:-op} — marker '$1' が出現してはいけない箇所で出現した:" >&2
    cat "$file" >&2
    exit 1
  fi
}

# ---------------------------------------------------------------------------
# STAGE=2（Task13 で実装。撤去後の最終受け入れ — chip-tool を PATH から
# 外した環境での再検証・`mat fabric init` 実機検証・deploy 成果物
# （aarch64-gnu + ble）検証。design の「実機 E2E ゲート2」参照）。
# ---------------------------------------------------------------------------
stage2_main() {
  echo "STAGE=2 not implemented until Task 13" >&2
  exit 1
}

# ---------------------------------------------------------------------------
# STAGE=1 本体
# ---------------------------------------------------------------------------
stage1_main() {
  : "${MAT_E2E_HOST:?MAT_E2E_HOST (ssh host) required}"
  : "${MAT_E2E_NODE:?MAT_E2E_NODE (commissioned node id for the read/write/invoke/describe/diag/open-window sweep) required}"
  : "${MAT_E2E_GROUP_NODES:?MAT_E2E_GROUP_NODES (space-separated commissioned node ids for the throwaway group 99 provision) required}"
  command -v jq >/dev/null 2>&1 || { echo "jq が必要です（JSON 抽出に使用）" >&2; exit 1; }

  # 注意: このうち REMOTE_MAT_BIN / REMOTE_MATD_BIN / REMOTE_MATD_LOG /
  # REMOTE_MATD_PID / SOCKET / REMOTE_STORE / MATD_STARTED / COMBINED_LOG は
  # 意図的に `local` を付けない（下の cleanup() は EXIT trap で発火し、
  # stage1_main が正常終了して戻った**後**の trap 発火では関数ローカル変数は
  # 既にスコープ外になるため、trap から参照する状態はグローバルにする必要が
  # ある — m8c1/m8c2 が関数分割せずフラットに書いていた理由でもある）。
  local FABRIC_INDEX GROUP KEYSET ENDPOINT STORE CHIP_TOOL_BIN
  local NODE GROUP_NODES MATD_PORT
  FABRIC_INDEX="${MAT_E2E_FABRIC_INDEX:-2}"
  GROUP="${MAT_E2E_GROUP_ID:-99}"
  KEYSET="${MAT_E2E_KEYSET_ID:-99}"
  ENDPOINT="${MAT_E2E_ENDPOINT:-1}"
  STORE="${MAT_E2E_STORE:-}"
  CHIP_TOOL_BIN="${MAT_E2E_CHIP_TOOL_BIN:-}"
  NODE="$MAT_E2E_NODE"
  GROUP_NODES="$MAT_E2E_GROUP_NODES"
  SOCKET="${MAT_E2E_SOCKET:-/tmp/matd-e2e-m8c3.sock}"
  MATD_PORT="${MAT_E2E_MATD_PORT:-9115}"
  # shellcheck disable=SC2206 # GROUP_NODES は意図的に空白分割する（--nodes にそのまま渡す）
  local GROUP_NODE_ARR=($GROUP_NODES)
  local TARGET=aarch64-unknown-linux-musl
  REMOTE_MAT_BIN=/tmp/mat-e2e-m8c3
  REMOTE_MATD_BIN=/tmp/matd-e2e-m8c3
  REMOTE_MATD_LOG=/tmp/matd-e2e-m8c3.log
  REMOTE_MATD_PID=/tmp/matd-e2e-m8c3.pid
  local GROUP_NAME=e2e-m8c3
  local EPOCH_KEY_PATTERN="^mat/f/${FABRIC_INDEX}/ipk-epoch="

  echo "== 準備1/3: クロスビルド (mat + matd, $TARGET, rust-lld) — BLE 不要（commission は on-network のみ）"
  export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
  export RUSTFLAGS="-C linker-flavor=ld.lld -C link-self-contained=yes"
  cargo build --release --target "$TARGET" -p mat -p matd
  local MAT_BIN="target/$TARGET/release/mat"
  local MATD_BIN="target/$TARGET/release/matd"
  file "$MAT_BIN" | grep -q 'aarch64' || { echo "FAIL: stale/wrong-arch binary: $MAT_BIN" >&2; exit 1; }
  file "$MATD_BIN" | grep -q 'aarch64' || { echo "FAIL: stale/wrong-arch binary: $MATD_BIN" >&2; exit 1; }
  echo "mat: $MAT_BIN / matd: $MATD_BIN"

  # 直近呼び出しの stderr（ローカル一時ファイル、呼び出しのたびに上書き）+
  # 全実行ログ結合（検証4の "falling back" 0件チェック用、追記のみ）。
  LAST_STDERR_FILE=$(mktemp)
  COMBINED_LOG=$(mktemp)
  REMOTE_STORE=""
  MATD_STARTED=0

  cleanup() {
    echo "== cleanup: 一時 matd 停止 + ssh 先の一時バイナリ削除 + 本番 matd 復帰 ($MAT_E2E_HOST) =="
    if [ "$MATD_STARTED" = "1" ]; then
      ssh -n "$MAT_E2E_HOST" "'$REMOTE_MATD_BIN' stop --socket '$SOCKET'" 2>/dev/null || true
      ssh -n "$MAT_E2E_HOST" "kill \"\$(cat '$REMOTE_MATD_PID' 2>/dev/null)\" 2>/dev/null" || true
    fi
    ssh -n "$MAT_E2E_HOST" "rm -f '$REMOTE_MAT_BIN' '$REMOTE_MATD_BIN' '$REMOTE_MATD_PID' '$REMOTE_MATD_LOG' '$SOCKET'" || true
    # trap で必ず本番 matd を起動状態に戻す（既に動いていれば no-op）。
    ssh -n "$MAT_E2E_HOST" "sudo systemctl start matd" || true
    if [ -n "$REMOTE_STORE" ]; then
      echo "== KVS バックアップの案内 =="
      echo "  jarvis 上の $REMOTE_STORE/chip_tool_config.ini.bak-m8c3 に検証前の状態を保存しています。"
      echo "  epoch 採用永続はこのゲートの目的そのものなので自動 restore はしません"
      echo "  （restore すると Stage 1 のゴールを自ら消してしまう）。問題があれば内容を"
      echo "  見比べたうえで手動判断してください:"
      echo "    ssh $MAT_E2E_HOST \"diff '$REMOTE_STORE/chip_tool_config.ini.bak-m8c3' '$REMOTE_STORE/chip_tool_config.ini'\""
    fi
    rm -f "$LAST_STDERR_FILE" "$COMBINED_LOG"
  }
  trap cleanup EXIT

  echo "== 準備2/3: 転送 → $MAT_E2E_HOST ($REMOTE_MAT_BIN / $REMOTE_MATD_BIN, 本番 /usr/local/bin/{mat,matd} とは別)"
  scp "$MAT_BIN" "$MAT_E2E_HOST:$REMOTE_MAT_BIN"
  scp "$MATD_BIN" "$MAT_E2E_HOST:$REMOTE_MATD_BIN"
  ssh -n "$MAT_E2E_HOST" "chmod +x '$REMOTE_MAT_BIN' '$REMOTE_MATD_BIN'"

  local STORE_ARG=()
  [ -n "$STORE" ] && STORE_ARG=(--store "$STORE")

  echo "== 準備3/3: store 解決 + 本番 matd 停止 + KVS backup"
  if [ -n "$STORE" ]; then
    REMOTE_STORE="$STORE"
  else
    REMOTE_STORE=$(ssh -n "$MAT_E2E_HOST" 'echo "${MAT_STORE:-${XDG_CONFIG_HOME:-$HOME/.config}/mat}"')
  fi
  echo "store = $REMOTE_STORE"

  echo "sudo systemctl stop matd（本番 systemd matd は MAT_MATD_IFACE 設定済みのため、未設定検証は一時インスタンスで行う。trap で必ず復帰）"
  ssh -n "$MAT_E2E_HOST" "sudo systemctl stop matd"

  echo "KVS backup: $REMOTE_STORE/chip_tool_config.ini -> .bak-m8c3（上書きしない -n 付き）"
  ssh -n "$MAT_E2E_HOST" "cp -n '$REMOTE_STORE/chip_tool_config.ini' '$REMOTE_STORE/chip_tool_config.ini.bak-m8c3'" \
    || echo "WARN: KVS backup が失敗（既に .bak-m8c3 が存在？ 上書きはしていません）" >&2

  # ---- runner 群 ----
  # 検証2・5: MAT_IFACE / MAT_MATD_IFACE を明示 unset（env -u）した直経路。
  # `env -u` はリモートログインシェルが偶然どちらかを export していても
  # 確実に外す（brief の主題そのものへの環境汚染を避ける）。加えて
  # MAT_CHIP_TOOL_BIN=/nonexistent で「fallback すれば即 exit 12」の
  # 二重チェックを常に効かせる（M8a Task11 以来の作法）。
  # ★注意: ssh の終了コードを必ず明示的に捕まえてから return する
  # （`cmd; cat ...` のように末尾コマンドを追加すると、関数の戻り値が最後の
  # `cat`（ほぼ常に成功）にすり替わり、if 条件や retry ループでの失敗判定が
  # 常に成功扱いになる — 検証5のリトライループ実装時に見つけて修正した）。
  run_native_default() {
    local rc=0
    ssh -n "$MAT_E2E_HOST" env -u MAT_IFACE -u MAT_MATD_IFACE \
      MAT_MATD=0 MAT_LOG=info "MAT_FABRIC_INDEX=$FABRIC_INDEX" \
      MAT_CHIP_TOOL_BIN=/nonexistent/mat-e2e-m8c3-chip-tool \
      "$REMOTE_MAT_BIN" "$@" 2>"$LAST_STDERR_FILE" || rc=$?
    cat "$LAST_STDERR_FILE" >> "$COMBINED_LOG"
    return "$rc"
  }

  # 検証3: matd 経由（--matd で強制、接続失敗はフォールバック無しのハード
  # エラー）。matd 側の native/fallback 実証はソケットではなく
  # $REMOTE_MATD_LOG を見る（クライアント呼び出し自体の stderr は薄い）。
  run_matd() {
    local rc=0
    ssh -n "$MAT_E2E_HOST" "$REMOTE_MAT_BIN" --matd "$SOCKET" "$@" 2>"$LAST_STDERR_FILE" || rc=$?
    cat "$LAST_STDERR_FILE" >> "$COMBINED_LOG"
    return "$rc"
  }

  # KVS の mat-epoch キーの有無だけを判定する（値は絶対に出力しない — 秘匿
  # 材料。grep -q のみ使用、-c であっても値行を晒さないよう常に -q で統一）。
  kvs_has_epoch_key() {
    ssh -n "$MAT_E2E_HOST" "grep -q '$EPOCH_KEY_PATTERN' '$REMOTE_STORE/chip_tool_config.ini'"
  }

  echo "== 検証2/7: env 未設定 native スイープ（直経路、node=$NODE, group=$GROUP keyset=$KEYSET, group-nodes=$GROUP_NODES）"

  echo "-- discover"
  local DISCOVER_OUT
  DISCOVER_OUT=$(run_native_default discover)
  echo "$DISCOVER_OUT"
  assert_no_fallback "discover (native default)"
  assert_native_marker "iface auto-selected (native default)" "discover (native default)"
  assert_grep '"devices"' "$DISCOVER_OUT" "discover が devices を含まない"
  # 検証6用に、この呼び出しの marker 行を保存しておく（jarvis の eth0/tailscale0
  # 実測は最初の呼び出しでも以降の呼び出しでも同じ結果になるはずだが、専用の
  # 呼び出しを増やさずここで済ませる）。
  local IFACE_CHECK_LOG
  IFACE_CHECK_LOG=$(mktemp)
  cp "$LAST_STDERR_FILE" "$IFACE_CHECK_LOG"

  echo "-- read (baseline, onoff on-off)"
  local READ_OUT
  READ_OUT=$(run_native_default "${STORE_ARG[@]}" read -n "$NODE" -e "$ENDPOINT" -c onoff -a on-off)
  echo "$READ_OUT"
  assert_no_fallback "read (native default)"
  assert_native_marker "iface auto-selected (native default)" "read (native default)"
  assert_native_marker "read executed (native direct)" "read (native default)"

  echo "-- write (levelcontrol on-level=77 → read back → revert to null)"
  local WRITE_OUT
  WRITE_OUT=$(run_native_default "${STORE_ARG[@]}" write -n "$NODE" -e "$ENDPOINT" -c levelcontrol -a on-level --value 77)
  echo "$WRITE_OUT"
  assert_no_fallback "write on-level=77 (native default)"
  assert_native_marker "iface auto-selected (native default)" "write on-level=77 (native default)"
  assert_native_marker "write executed (native direct)" "write on-level=77 (native default)"
  assert_grep '"status":"success"' "$WRITE_OUT" "write on-level=77 が status:success を返さない"

  READ_OUT=$(run_native_default "${STORE_ARG[@]}" read -n "$NODE" -e "$ENDPOINT" -c levelcontrol -a on-level)
  echo "$READ_OUT"
  assert_no_fallback "read on-level after write (native default)"
  assert_grep '"value":77' "$READ_OUT" "write 直後の read on-level が value:77 を返さない"

  WRITE_OUT=$(run_native_default "${STORE_ARG[@]}" write -n "$NODE" -e "$ENDPOINT" -c levelcontrol -a on-level --value null)
  echo "$WRITE_OUT"
  assert_no_fallback "write on-level=null revert (native default)"
  assert_grep '"status":"success"' "$WRITE_OUT" "write on-level=null（後始末）が status:success を返さない"
  echo "PASS: write 検証（native default, 反映+revert確認）" >&2

  echo "-- invoke (onoff toggle → 逆toggle)"
  local INVOKE_OUT
  INVOKE_OUT=$(run_native_default "${STORE_ARG[@]}" invoke -n "$NODE" -e "$ENDPOINT" -c onoff --command toggle)
  echo "$INVOKE_OUT"
  assert_no_fallback "invoke toggle (native default)"
  assert_native_marker "iface auto-selected (native default)" "invoke toggle (native default)"
  assert_native_marker "invoke executed (native direct)" "invoke toggle (native default)"
  assert_grep '"status":"success"' "$INVOKE_OUT" "invoke toggle が status:success を返さない"

  INVOKE_OUT=$(run_native_default "${STORE_ARG[@]}" invoke -n "$NODE" -e "$ENDPOINT" -c onoff --command toggle)
  echo "$INVOKE_OUT"
  assert_no_fallback "invoke toggle revert (native default)"
  assert_grep '"status":"success"' "$INVOKE_OUT" "invoke toggle（復元）が status:success を返さない"
  confirm "node $NODE の点灯状態が2回の toggle で元に戻ったことを目視確認してください（任意）"
  echo "PASS: invoke 検証（native default, toggle往復）" >&2

  echo "-- describe"
  local DESCRIBE_OUT
  DESCRIBE_OUT=$(run_native_default "${STORE_ARG[@]}" describe -n "$NODE")
  echo "$DESCRIBE_OUT"
  assert_no_fallback "describe (native default)"
  assert_native_marker "iface auto-selected (native default)" "describe (native default)"
  assert_native_marker "describe executed (native direct)" "describe (native default)"
  assert_grep '"endpoints"' "$DESCRIBE_OUT" "describe が endpoints を含まない"

  echo "-- diag thread"
  local DIAG_THREAD_OUT
  DIAG_THREAD_OUT=$(run_native_default "${STORE_ARG[@]}" diag thread -n "$NODE")
  echo "$DIAG_THREAD_OUT"
  assert_no_fallback "diag thread (native default)"
  assert_native_marker "iface auto-selected (native default)" "diag thread (native default)"
  assert_native_marker "diag thread executed (native direct)" "diag thread (native default)"

  echo "-- diag node --deep"
  local DIAG_NODE_OUT
  DIAG_NODE_OUT=$(run_native_default "${STORE_ARG[@]}" diag node -n "$NODE" --deep)
  echo "$DIAG_NODE_OUT"
  assert_no_fallback "diag node --deep (native default)"
  assert_native_marker "iface auto-selected (native default)" "diag node --deep (native default)"
  assert_native_marker "diag node executed (native)" "diag node --deep (native default)"
  assert_grep '"verdict"' "$DIAG_NODE_OUT" "diag node --deep が verdict を含まない"

  echo "-- open-window"
  local OW_OUT
  OW_OUT=$(run_native_default "${STORE_ARG[@]}" open-window -n "$NODE")
  echo "$OW_OUT"
  assert_no_fallback "open-window (native default)"
  assert_native_marker "iface auto-selected (native default)" "open-window (native default)"
  assert_native_marker "open-window executed (native direct)" "open-window (native default)"
  assert_grep '"qr_payload"' "$OW_OUT" "open-window が qr_payload を含まない"

  echo "-- group provision（使い捨て group=$GROUP keyset=$KEYSET nodes=$GROUP_NODES）"
  local GP_OUT
  # shellcheck disable=SC2086 # GROUP_NODES は意図的に空白展開する（--nodes にそのまま渡す）
  GP_OUT=$(run_native_default "${STORE_ARG[@]}" group provision -g "$GROUP" --nodes $GROUP_NODES \
    --keyset-id "$KEYSET" --name "$GROUP_NAME")
  echo "$GP_OUT"
  assert_no_fallback "group provision (native default)"
  assert_native_marker "iface auto-selected (native default)" "group provision (native default)"
  assert_native_marker "group provision controller state written (native kvs)" "group provision (native default, kvs write)"
  assert_native_marker "group provision executed (native direct)" "group provision (native default, 完走)"
  assert_grep '"status":"provisioned"' "$GP_OUT" "group provision が status:provisioned を返さない"
  assert_grep '"note":".*restart' "$GP_OUT" "group provision の note に restart 案内が無い"

  echo "-- group provision --rebind（再実行、Duplicate にならないこと）"
  # shellcheck disable=SC2086
  GP_OUT=$(run_native_default "${STORE_ARG[@]}" group provision -g "$GROUP" --nodes $GROUP_NODES \
    --keyset-id "$KEYSET" --name "$GROUP_NAME" --rebind)
  echo "$GP_OUT"
  assert_no_fallback "group provision --rebind (native default)"
  assert_native_marker "group provision controller state written (native kvs)" "group provision --rebind (native default, kvs write)"
  assert_native_marker "group provision executed (native direct)" "group provision --rebind (native default, 完走)"
  assert_grep '"status":"provisioned"' "$GP_OUT" "group provision --rebind が status:provisioned を返さない"
  echo "PASS: group provision 検証（native default, 通常+--rebind 両方）" >&2

  echo "-- group invoke（toggle → 逆toggle）"
  local GI_OUT
  GI_OUT=$(run_native_default "${STORE_ARG[@]}" group invoke -g "$GROUP" -c onoff --command toggle -e "$ENDPOINT")
  echo "$GI_OUT"
  assert_no_fallback "group invoke toggle (native default)"
  assert_native_marker "iface auto-selected (native default)" "group invoke toggle (native default)"
  assert_native_marker "groupcast sent (native)" "group invoke toggle (native default)"
  assert_grep '"status":"sent"' "$GI_OUT" "group invoke toggle が status:sent を返さない"

  GI_OUT=$(run_native_default "${STORE_ARG[@]}" group invoke -g "$GROUP" -c onoff --command toggle -e "$ENDPOINT")
  echo "$GI_OUT"
  assert_no_fallback "group invoke toggle revert (native default)"
  assert_grep '"status":"sent"' "$GI_OUT" "group invoke toggle（復元）が status:sent を返さない"
  confirm "group $GROUP メンバー ($GROUP_NODES) が2回の toggle で元に戻ったことを目視確認してください（任意）"
  echo "PASS: 検証2 env 未設定 native スイープ全項目 GREEN（discover/read/write/invoke/describe/diag thread/diag node --deep/open-window/group provision(+rebind)/group invoke、全て iface auto-selected (native default) + 各 positive marker + fallback 不在）" >&2

  echo "== 検証3/7: matd 経路（MAT_MATD_IFACE 無しで一時起動 → read/write/group invoke）"
  ssh -n "$MAT_E2E_HOST" "rm -f '$SOCKET'"
  # 注意: この呼び出しは heredoc でリモート stdin にスクリプトを渡すため
  # `ssh -n`（ローカル stdin を /dev/null にリダイレクトし、heredoc の中身を
  # 握りつぶす）を使わない — e2e-m8a-real.sh の start_matd() と同じ理由
  # （`-n` と heredoc は併用できない）。値は `VAR=val` で一旦リモート環境変数へ
  # 渡し、heredoc 自体はクォート付き（`<<'REMOTE_SCRIPT'`）でローカル展開させ
  # ない（値にシェルメタ文字があっても安全 — m8a と同じ方式）。
  # MAT_MATD_IFACE は渡す変数リストに含めない（未設定を保証）うえ、リモート
  # 側スクリプト冒頭でも明示 unset する（ログインシェルの export 汚染対策）。
  ssh "$MAT_E2E_HOST" \
    MAT_E2E_SOCKET="$SOCKET" \
    MAT_E2E_MATD_PORT="$MATD_PORT" \
    MAT_E2E_FABRIC_INDEX="$FABRIC_INDEX" \
    MAT_E2E_STORE="$STORE" \
    MAT_E2E_CHIP_TOOL_BIN="$CHIP_TOOL_BIN" \
    MAT_E2E_MATD_BIN="$REMOTE_MATD_BIN" \
    MAT_E2E_MATD_LOG="$REMOTE_MATD_LOG" \
    MAT_E2E_MATD_PID="$REMOTE_MATD_PID" \
    'bash -s' <<'REMOTE_SCRIPT'
set -euo pipefail
unset MAT_MATD_IFACE
ARGS=(--fabric-index "$MAT_E2E_FABRIC_INDEX" --socket "$MAT_E2E_SOCKET" --port "$MAT_E2E_MATD_PORT")
[ -n "$MAT_E2E_STORE" ] && ARGS+=(--store "$MAT_E2E_STORE")
[ -n "$MAT_E2E_CHIP_TOOL_BIN" ] && export MAT_CHIP_TOOL_BIN="$MAT_E2E_CHIP_TOOL_BIN"
export RUST_LOG=info
nohup "$MAT_E2E_MATD_BIN" "${ARGS[@]}" >"$MAT_E2E_MATD_LOG" 2>&1 &
echo $! > "$MAT_E2E_MATD_PID"
disown
REMOTE_SCRIPT
  MATD_STARTED=1

  local i
  for i in $(seq 1 20); do
    ssh -n "$MAT_E2E_HOST" "test -S '$SOCKET'" && break
    ssh -n "$MAT_E2E_HOST" "kill -0 \"\$(cat '$REMOTE_MATD_PID' 2>/dev/null)\" 2>/dev/null" \
      || { echo "FAIL: 一時 matd 起動失敗" >&2; ssh -n "$MAT_E2E_HOST" "tail -n 80 '$REMOTE_MATD_LOG'" || true; exit 1; }
    sleep 0.5
  done
  ssh -n "$MAT_E2E_HOST" "test -S '$SOCKET'" || { echo "FAIL: 一時 matd の socket が既定時間内に現れない" >&2; ssh -n "$MAT_E2E_HOST" "tail -n 80 '$REMOTE_MATD_LOG'" || true; exit 1; }

  ssh -n "$MAT_E2E_HOST" "grep -q 'iface auto-selected (matd native default)' '$REMOTE_MATD_LOG'" \
    || { echo "FAIL: matd ログに 'iface auto-selected (matd native default)' が無い（MAT_MATD_IFACE 未設定の自動検出が走っていない）" >&2
         ssh -n "$MAT_E2E_HOST" "tail -n 80 '$REMOTE_MATD_LOG'" || true
         exit 1; }
  ssh -n "$MAT_E2E_HOST" "grep -q 'native backend enabled' '$REMOTE_MATD_LOG'" \
    || { echo "FAIL: matd ログに 'native backend enabled' が無い（自動検出後の native backend 構築に失敗？）" >&2
         ssh -n "$MAT_E2E_HOST" "tail -n 80 '$REMOTE_MATD_LOG'" || true
         exit 1; }
  echo "PASS: 一時 matd が MAT_MATD_IFACE 無しで起動し iface auto-selected (matd native default) を確認" >&2

  READ_OUT=$(run_matd "${STORE_ARG[@]}" read -n "$NODE" -e "$ENDPOINT" -c onoff -a on-off)
  echo "$READ_OUT"
  assert_grep '"value"' "$READ_OUT" "matd 経由 read が value を含まない"

  WRITE_OUT=$(run_matd "${STORE_ARG[@]}" write -n "$NODE" -e "$ENDPOINT" -c levelcontrol -a on-level --value 77)
  echo "$WRITE_OUT"
  assert_grep '"status":"success"' "$WRITE_OUT" "matd 経由 write on-level=77 が status:success を返さない"
  READ_OUT=$(run_matd "${STORE_ARG[@]}" read -n "$NODE" -e "$ENDPOINT" -c levelcontrol -a on-level)
  echo "$READ_OUT"
  assert_grep '"value":77' "$READ_OUT" "matd 経由 write 直後の read on-level が value:77 を返さない"
  WRITE_OUT=$(run_matd "${STORE_ARG[@]}" write -n "$NODE" -e "$ENDPOINT" -c levelcontrol -a on-level --value null)
  echo "$WRITE_OUT"
  assert_grep '"status":"success"' "$WRITE_OUT" "matd 経由 write on-level=null（後始末）が status:success を返さない"

  GI_OUT=$(run_matd "${STORE_ARG[@]}" group invoke -g "$GROUP" -c onoff --command toggle -e "$ENDPOINT")
  echo "$GI_OUT"
  assert_grep '"status":"sent"' "$GI_OUT" "matd 経由 group invoke toggle が status:sent を返さない"
  GI_OUT=$(run_matd "${STORE_ARG[@]}" group invoke -g "$GROUP" -c onoff --command toggle -e "$ENDPOINT")
  echo "$GI_OUT"
  assert_grep '"status":"sent"' "$GI_OUT" "matd 経由 group invoke toggle（復元）が status:sent を返さない"

  local MATD_FULL_LOG
  MATD_FULL_LOG=$(ssh -n "$MAT_E2E_HOST" "cat '$REMOTE_MATD_LOG'")
  if printf '%s\n' "$MATD_FULL_LOG" | grep -q "chip-tool ws raw response"; then
    echo "FAIL: matd ログに 'chip-tool ws raw response' が出ている — read/write/group invoke のいずれかが native を通らず chip-tool 経由で実行された可能性" >&2
    printf '%s\n' "$MATD_FULL_LOG" >&2
    exit 1
  fi
  ssh -n "$MAT_E2E_HOST" "cat '$REMOTE_MATD_LOG'" >> "$COMBINED_LOG"

  ssh -n "$MAT_E2E_HOST" "'$REMOTE_MATD_BIN' stop --socket '$SOCKET'"
  MATD_STARTED=0
  echo "PASS: 検証3 matd 経路（MAT_MATD_IFACE 無し起動 + read/write/group invoke 成功、chip-tool traffic 無し）" >&2

  echo "== 検証4/7: フォールバック発火ゼロ（全ログ結合）"
  if grep -q "falling back" "$COMBINED_LOG"; then
    echo "FAIL: 検証2・3 の結合ログに 'falling back' が出現している" >&2
    grep "falling back" "$COMBINED_LOG" >&2
    exit 1
  fi
  echo "PASS: 検証4 フォールバック発火ゼロ（結合ログ 0 件 + 各呼び出し MAT_CHIP_TOOL_BIN=/nonexistent 二重チェック済み）" >&2

  echo "== 検証6/7: iface 一意選択の実測（jarvis で eth0、tailscale0 でないこと）"
  local IFACE_LINE
  IFACE_LINE=$(grep "iface auto-selected (native default)" "$IFACE_CHECK_LOG" || true)
  rm -f "$IFACE_CHECK_LOG"
  if [ -z "$IFACE_LINE" ]; then
    echo "FAIL: 検証6 — iface auto-selected (native default) のログ行が見つからない" >&2
    exit 1
  fi
  # Strip ANSI escape sequences from structured field output (tracing emits ANSI between field name/value)
  IFACE_LINE=$(printf '%s' "$IFACE_LINE" | sed 's/\x1b\[[0-9;]*m//g')
  if ! printf '%s' "$IFACE_LINE" | grep -Eq 'iface[=:]"?eth0"?'; then
    echo "FAIL: 検証6 — 選択された iface が eth0 でない: $IFACE_LINE" >&2
    exit 1
  fi
  if printf '%s' "$IFACE_LINE" | grep -q 'tailscale0'; then
    echo "FAIL: 検証6 — tailscale0 が候補選択に紛れ込んでいる: $IFACE_LINE" >&2
    exit 1
  fi
  echo "PASS: 検証6 iface 一意選択実測（jarvis で eth0 一意選択、tailscale0 が選ばれない）" >&2

  echo "== 検証5/7: epoch 採用永続（使い捨てノードで RemoveFabric→on-network 再commission を2回。検証6の後に実行 — 危険操作なので安価な検証を先に済ませる）"
  echo "WARN: この検証は対象ノードを一時的に fabric-less（未commission）状態にします。実行判断は下記プロンプトで。" >&2
  if confirm_yn "検証5（epoch 採用永続の実測、RemoveFabric を伴う）を実行しますか"; then
    : "${MAT_E2E_EPOCH_NODE:?検証5を実行するには MAT_E2E_EPOCH_NODE (使い捨てノードの node_id) が必要}"
    local EPOCH_NODE=$MAT_E2E_EPOCH_NODE
    echo ""
    echo ">>> 検証5対象ノード（node=$EPOCH_NODE）の setup code を QR ペイロード（\"MT:...\"）または"
    echo ">>> 11/21桁の manual code で指定してください。デバイス印字のコードは repo にコミットしません。"
    local EPOCH_SETUP_CODE="${MAT_E2E_EPOCH_SETUP_CODE:-}"
    if [ -z "$EPOCH_SETUP_CODE" ]; then
      read -r -p ">>> setup code: " EPOCH_SETUP_CODE
    fi

    local KEY_PRESENT_BEFORE=0
    if kvs_has_epoch_key; then
      KEY_PRESENT_BEFORE=1
      echo "epoch キーは実行前から存在済み（再実行 or 別経路での先行 adopt）— 1回目を『読み出しのみ』期待に切り替えます（★judgment 参照）"
    else
      echo "epoch キーは実行前は不在 — 1回目で adopt が起きる想定"
    fi

    epoch_remove_and_recommission() {
      # $1 = ラベル（ログ用）
      local label=$1
      echo "-- [$label] current-fabric-index を native read で取得（node=$EPOCH_NODE）"
      local FI_OUT
      FI_OUT=$(run_native_default "${STORE_ARG[@]}" read -n "$EPOCH_NODE" -e 0 -c operationalcredentials -a current-fabric-index)
      echo "$FI_OUT"
      assert_no_fallback "[$label] read current-fabric-index"
      local DEV_FI
      DEV_FI=$(printf '%s' "$FI_OUT" | jq -r '.value')
      if [ -z "$DEV_FI" ] || [ "$DEV_FI" = "null" ]; then
        echo "FAIL: [$label] current-fabric-index の読み出しに失敗" >&2
        exit 1
      fi

      echo "-- [$label] RemoveFabric（native invoke, fabric-index=$DEV_FI）"
      run_native_default "${STORE_ARG[@]}" invoke -n "$EPOCH_NODE" -e 0 -c operationalcredentials --command remove-fabric "$DEV_FI" >/dev/null
      assert_no_fallback "[$label] invoke remove-fabric"

      echo "-- [$label] 同一 setup code で on-network 再commission（commissioning window 再オープン待ちのため最大3回リトライ）"
      local ok=0 attempt
      for attempt in 1 2 3; do
        if run_native_default "${STORE_ARG[@]}" commission --target "on-network-epoch-probe" \
          --setup-code "$EPOCH_SETUP_CODE" --node "$EPOCH_NODE" >/dev/null; then
          ok=1
          break
        fi
        echo "retry $attempt/3: [$label] on-network 再commission 失敗、3秒後に再試行します" >&2
        sleep 3
      done
      if [ "$ok" != "1" ]; then
        echo "FAIL: [$label] on-network 再commission が3回とも失敗した — node $EPOCH_NODE は fabric-less のまま残っている可能性があります。手動で再commissionしてください。最後の試行の stderr:" >&2
        cat "$LAST_STDERR_FILE" >&2
        exit 1
      fi
      assert_no_fallback "[$label] commission on-network-epoch-probe"
      assert_native_marker "commission executed (native on-network)" "[$label] commission on-network-epoch-probe"
    }

    epoch_remove_and_recommission "1回目"
    if [ "$KEY_PRESENT_BEFORE" = "1" ]; then
      assert_marker_absent "ipk epoch adopted (kvs)" "1回目（既に adopt 済みのはずなので出てはいけない）"
    else
      assert_native_marker "ipk epoch adopted (kvs)" "1回目（初回 adopt が起きるはず）"
      kvs_has_epoch_key || { echo "FAIL: 1回目の commission 後も INI に mat/f/$FABRIC_INDEX/ipk-epoch が現れない" >&2; exit 1; }
    fi
    echo "PASS: 検証5 1回目（epoch 解決 + INI 永続確認）" >&2

    epoch_remove_and_recommission "2回目"
    assert_marker_absent "ipk epoch adopted (kvs)" "2回目（既に永続済みのため adopt marker は出てはいけない、読み出し経路のはず）"
    echo "PASS: 検証5 2回目（adopt marker 不在 = 読み出し経路であることを確認）" >&2

    echo "-- 検証5 後片付け（best-effort）: node $EPOCH_NODE を再度 RemoveFabric（fabric-less のまま残す）"
    local FI_OUT2 DEV_FI2
    if FI_OUT2=$(run_native_default "${STORE_ARG[@]}" read -n "$EPOCH_NODE" -e 0 -c operationalcredentials -a current-fabric-index 2>/dev/null); then
      DEV_FI2=$(printf '%s' "$FI_OUT2" | jq -r '.value' 2>/dev/null) || DEV_FI2=""
      if [ -n "$DEV_FI2" ] && [ "$DEV_FI2" != "null" ]; then
        run_native_default "${STORE_ARG[@]}" invoke -n "$EPOCH_NODE" -e 0 -c operationalcredentials --command remove-fabric "$DEV_FI2" >/dev/null 2>&1 \
          && echo "node $EPOCH_NODE を RemoveFabric しました（fabric-less で終了 — 元の運用に戻すには手動で再commissionしてください）" \
          || echo "WARN: 後始末の RemoveFabric が失敗（best-effort、node $EPOCH_NODE の状態を手動確認してください）" >&2
      fi
    else
      echo "WARN: 後始末の current-fabric-index 読み出しが失敗（best-effort、node $EPOCH_NODE の状態を手動確認してください）" >&2
    fi
    echo ">>> NOTE: node $EPOCH_NODE (検証5使い捨てノード) は検証終了時点で fabric-less です。再利用するなら手動で再commissionしてください。" >&2
  else
    echo "SKIP: 検証5（epoch 採用永続の実測、人力判断によりスキップ）— このゲート実行では未検証です" >&2
  fi

  echo "== 検証7/7: 後片付け（使い捨て group $GROUP、best-effort）"
  local n
  for n in "${GROUP_NODE_ARR[@]}"; do
    run_native_default "${STORE_ARG[@]}" invoke -n "$n" -e "$ENDPOINT" -c groups --command remove-group "$GROUP" >/dev/null 2>&1 \
      && echo "device-side remove-group OK (node=$n)" \
      || echo "WARN: device-side groups remove-group が失敗 (node=$n, best-effort)" >&2
  done
  local CHIP_BIN="${CHIP_TOOL_BIN:-chip-tool}"
  ssh -n "$MAT_E2E_HOST" "$CHIP_BIN" groupsettings remove-group "$GROUP" --storage-directory "$REMOTE_STORE" >/dev/null 2>&1 \
    && echo "controller-side groupsettings remove-group OK" \
    || echo "WARN: controller-side groupsettings remove-group が失敗 (best-effort)" >&2
  ssh -n "$MAT_E2E_HOST" "$CHIP_BIN" groupsettings remove-keyset "$KEYSET" --storage-directory "$REMOTE_STORE" >/dev/null 2>&1 \
    && echo "controller-side groupsettings remove-keyset OK" \
    || echo "WARN: controller-side groupsettings remove-keyset が失敗 (best-effort)" >&2

  echo "sudo systemctl restart matd（本番 native group state 再読込のため起動 — trap の cleanup でも start するが、後続の目視確認のためここで明示的に起動する）"
  ssh -n "$MAT_E2E_HOST" "sudo systemctl restart matd"
  for i in $(seq 1 10); do
    ssh -n "$MAT_E2E_HOST" "sudo systemctl is-active --quiet matd" && break
    sleep 1
  done
  echo "PASS: 検証7 後片付け（使い捨て group $GROUP 除去 best-effort、本番 matd 復帰）" >&2

  echo "== e2e:m8c3:real STAGE=1 PASS（検証1〜7 GREEN。検証5は上記ログの PASS/SKIP を確認）"
}

case "$STAGE" in
  1) stage1_main ;;
  2) stage2_main ;;
  *) echo "FAIL: unknown STAGE='$STAGE' (expected 1 or 2)" >&2; exit 2 ;;
esac
