#!/usr/bin/env bash
# Phase 5 M8c-3 実機 E2E（native 既定化 + chip-tool 撤去、最終受け入れ）。
#
# 本ハーネスは STAGE 環境変数（既定 1）で二相に分岐する:
#   STAGE=1（既定）— ゲート1: `MAT_IFACE` / `MAT_MATD_IFACE` を明示 unset
#                     しても jarvis 実運用 fabric で全 op が native 経路で
#                     完走し、chip-tool フォールバックが一度も発火しない
#                     こと（design: docs/superpowers/specs/
#                     2026-07-17-phase5-m8c3-native-default-design.md
#                     設計3・ゲート1）。chip-tool フォールバックのコードは
#                     このハーネスが書かれた時点ではまだ生きていたため、
#                     ゲート1が FAIL しても本番影響なく撤退できる、という
#                     前提で作られた（Task7）。
#   STAGE=2        — ゲート2（最終受け入れ、Task13）: chip-tool 完全撤去後
#                     の状態で、(1) chip-tool を PATH から外した環境での
#                     op スイープ再実行、(2) `mat fabric init` の実機検証
#                     （別 store・実運用 fabric 無傷）、(3) deploy 成果物
#                     （aarch64-gnu + BLE）の BLE feature 実証、(4) `task
#                     check` + `task docker:build`。
#
# 骨格は scripts/e2e-m8c2-real.sh / e2e-m8c1-real.sh / e2e-m8a-real.sh を流用
# （trap 後始末・stderr への PASS/FAIL・positive marker 二重チェック・musl
# rust-lld クロスビルド・`ssh -n` の作法・KVS バックアップの実在ガード）。
#
# STAGE=1/2 で共有する部分は関数化してある（confirm/confirm_yn/assert_* は
# 元から共有、Task13 で run_native_default/run_matd/start_temp_matd/op_sweep
# も top-level 関数に引き上げて共有にした — 詳細は各関数のコメント参照）。
#
# ★Task13 での既知バグ修正（後続タスクのレビューで判明、ゲート1実装時点の
# 状態からのズレ）:
#   1. STAGE=1 の一時 matd 起動が `--port` を渡していた — Task10 で matd
#      から `--port`/`--connect`/`--idle-timeout` が削除済みのため無効な
#      フラグだった（現行 matd の CLI は `--socket`/`--iface`/
#      `--fabric-index`/`--issuer-index`/`--store` のみ、
#      crates/matd/src/main.rs 参照）。start_temp_matd() で修正し、
#      `MAT_E2E_MATD_PORT` の env プラミングも削除した。
#   2. `MAT_CHIP_TOOL_BIN=/nonexistent/...` の注入（M8a Task11 以来の
#      「fallback が起きれば exit 12 で即死する」二重チェック）は、
#      chip-tool の spawn コード自体が M8c-3 で撤去された今は意味を失った
#      （exit 12 は構造的に不可能 — crates/mat-core/src/error.rs の
#      `ChildNotFound` variant のコメント参照）。run_native_default() から
#      削除した。代わりの実証は (a) positive marker（"... executed
#      (native ...)" 系）と (b) 全ログでの "falling back" 0件の二重
#      チェックのまま — こちらは chip-tool 云々とは独立に有効
#      （"falling back" という文字列自体は matd 自動検出フォールバック
#      ["matd not reachable, falling back to direct native backend",
#      crates/mat/src/matd_client.rs] にも使われているが、本ハーネスの
#      呼び出しは全て MAT_MATD=0 / --matd で強制ルートを使うためこの経路
#      には一度も入らない — 0件は変わらず有効な実証）。STAGE=2 では
#      これに加え、配布バイナリそのものに "falling back to chip-tool"
#      という厳密文字列が存在しないことも直接 grep する（suspenders —
#      `grep -rn "falling back to chip-tool" crates/` はこのリポジトリで
#      0件、実在するのは上記の別文言のみ）。
#   3. 検証5（epoch 採用永続、RemoveFabric→同一 setup code で on-network
#      再commission）のパターンは NL68 実機（玄関ライト）では構造的に
#      不成立と判明した（gate1 の知見 — NL68 は RemoveFabric 後に mDNS
#      commissioning window を二度と開かない。復旧には工場リセット→BLE
#      が要る）。STAGE=1 は歴史的にそのまま残す（対象ノードが NL68 でな
#      ければ動く可能性があるため、かつ既に書かれた検証を無断で書き換え
#      ない）が、STAGE=2 はこのパターンを一切再利用しない —
#      「別 store への cross-fabric commission（multi-admin join）」+
#      「新 fabric だけを RemoveFabric」という別パターンを使う（元
#      fabric は一度も RemoveFabric しない、gate1 の教訓を踏まえた設計）。
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
#           MAT_E2E_CHIP_TOOL_BIN（使い捨て group の controller-side
#             groupsettings 後始末専用 — これは mat/matd 自身の chip-tool
#             呼び出しとは無関係の、ハーネス自前のメンテユーティリティ
#             呼び出し。未指定なら ssh 先 PATH の `chip-tool` 任せ、
#             見つからなければ WARN のみ best-effort）
#           MAT_E2E_SOCKET（一時 matd の unix socket。STAGE=1 既定
#             /tmp/matd-e2e-m8c3.sock、STAGE=2 既定
#             /tmp/matd-e2e-m8c3-s2.sock — 本番 matd とは別）
#           MAT_E2E_ASSUME_YES=1（確認プロンプトを自動化。危険操作
#             （STAGE=1 検証5、STAGE=2 の cross-fabric commission）は
#             この場合も既定でスキップ側に倒す — confirm_yn 実装参照）
#           MAT_E2E_EPOCH_NODE / MAT_E2E_EPOCH_SETUP_CODE（STAGE=1 検証5
#             専用。confirm_yn で実施を選んだ場合のみ必須）
# ローカル要件: jq（JSON 抽出に使用）、cross（STAGE=2 のビルドに使用）
set -euo pipefail
cd "$(dirname "$0")/.."

STAGE="${STAGE:-1}"

# STAGE=2 が chip-tool を PATH から外した環境で op スイープを再実行する際に
# 使う env オーバーライド（`env` コマンドへ渡す VAR=val の配列）。STAGE=1 は
# 空のまま（既定の PATH を変えない）。op_sweep() / run_native_default() /
# run_matd() が共通で参照する。
EXTRA_ENV=()
# start_temp_matd() が一時 matd プロセスへ渡す PATH 上書き値（空文字列なら
# 上書きしない）。EXTRA_ENV とは別の小さな仕組み — matd は `env` 経由の ssh
# ではなく heredoc スクリプトで nohup 起動するため（`bash -s` の中で export
# する）、配列を丸ごと渡すより単一の文字列の方が heredoc 側の実装が単純。
MATD_PATH_OVERRIDE=""

# ---------------------------------------------------------------------------
# 共有ヘルパ（STAGE=1/2 双方から呼べるよう top-level に置く）。
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
  # 危険操作（デバイスを一時的に fabric-less / 複数 fabric にする）は
  # 「自動確認 = 危険操作も自動実行」にはしない（m8c1 の confirm_yn と
  # 同じ判断、非対称な安全側デフォルト）。
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

# 逆方向 assert（STAGE=1 検証5・2回目の adopt marker 不在確認用）。
# $1 = grep パターン, $2 = 説明, $3 = 対象ファイル
assert_marker_absent() {
  local file=${3:-$LAST_STDERR_FILE}
  if grep -q -- "$1" "$file"; then
    echo "FAIL: ${2:-op} — marker '$1' が出現してはいけない箇所で出現した:" >&2
    cat "$file" >&2
    exit 1
  fi
}

# 使い捨て group $GROUP の controller/device 両側除去（best-effort、失敗は
# WARN のみ — 一度も provision していない fresh KVS でも失敗しないことが
# 前提）。検証7の後片付けと、op_sweep() 内の group provision 直前の
# pre-clean の両方から呼ばれる共有ロジック（fix3: 前回 run が中断すると
# group 99 の controller state が KVS に残ったままになり、非 --rebind の
# provision が「use --rebind」で必ず失敗する再実行耐性バグへの対策）。
# controller-side の groupsettings 呼び出しは実 chip-tool バイナリを使う
# （mat/matd 自身の内部 chip-tool 呼び出しとは無関係 — mat には
# `group remove` 相当の native op が無いため、このハーネス自前の後始末
# ユーティリティとして chip-tool を使い続けている。STAGE=2 のように
# chip-tool を PATH から外した環境ではこのステップは WARN で失敗するのが
# 期待動作 — best-effort なので全体は FAIL にしない）。
# GROUP_NODE_ARR / ENDPOINT / STORE_ARG / GROUP / KEYSET / CHIP_TOOL_BIN /
# REMOTE_STORE と run_native_default() を bash の動的スコープ経由で参照
# するため、呼び出し元がこれらを設定した後にのみ呼び出し可能。
cleanup_disposable_group() {
  local n
  for n in "${GROUP_NODE_ARR[@]}"; do
    run_native_default "${STORE_ARG[@]}" invoke -n "$n" -e "$ENDPOINT" -c groups --command remove-group "$GROUP" >/dev/null 2>&1 \
      && echo "device-side remove-group OK (node=$n)" \
      || echo "WARN: device-side groups remove-group が失敗 (node=$n, best-effort)" >&2
  done
  local CHIP_BIN="${CHIP_TOOL_BIN:-chip-tool}"
  ssh -n "$MAT_E2E_HOST" "$CHIP_BIN" groupsettings remove-group "$GROUP" --storage-directory "$REMOTE_STORE" >/dev/null 2>&1 \
    && echo "controller-side groupsettings remove-group OK" \
    || echo "WARN: controller-side groupsettings remove-group が失敗 (best-effort — chip-tool 不在環境では想定内)" >&2
  ssh -n "$MAT_E2E_HOST" "$CHIP_BIN" groupsettings remove-keyset "$KEYSET" --storage-directory "$REMOTE_STORE" >/dev/null 2>&1 \
    && echo "controller-side groupsettings remove-keyset OK" \
    || echo "WARN: controller-side groupsettings remove-keyset が失敗 (best-effort — chip-tool 不在環境では想定内)" >&2
}

# 共通 ssh 実行 + リトライ + ログ集約。$@ = 実行する ssh コマンド一式。
# stdout はそのまま呼び出し元へ通す（呼び出し元が `OUT=$(run_xxx ...)` で
# 捕まえる）。stderr は $LAST_STDERR_FILE へ（呼び出しのたびに上書き）+
# $COMBINED_LOG へ追記。Transport級の一過性失敗（rc=5/141/255）は1回だけ
# 3秒待ってリトライする（fix5: 一過性 Thread mesh / SSH 失敗への耐性）。
# ★注意: 戻り値は必ずこの関数自身の最終 rc（呼び出し元の `cmd; cat ...` の
# ような書き方で戻り値が別コマンドにすり替わるバグを避けるため、
# ここに一元化した）。
_run_ssh_capture() {
  local rc=0
  local attempt=1
  while true; do
    rc=0
    "$@" 2>"$LAST_STDERR_FILE" || rc=$?
    cat "$LAST_STDERR_FILE" >> "$COMBINED_LOG"
    if [ "$rc" != 0 ] && [ "$attempt" = "1" ]; then
      case "$rc" in
        5|141|255)
          echo "WARN: transient rc=$rc, retrying once in 3s: $*" >&2
          sleep 3
          attempt=2
          continue
          ;;
      esac
    fi
    break
  done
  if [ "$rc" != 0 ]; then
    echo "FAIL: remote command failed (rc=$rc): $*" >&2
    cat "$LAST_STDERR_FILE" >&2
  fi
  return "$rc"
}

# 直経路、env 未設定（MAT_IFACE / MAT_MATD_IFACE を明示 unset）。`env -u` は
# リモートログインシェルが偶然どちらかを export していても確実に外す
# （本ゲートの主題そのものへの環境汚染を避ける）。EXTRA_ENV（既定空配列）を
# 挟むことで STAGE=2 の「chip-tool を PATH から外す」を注入できる。
# 前提（グローバル）: MAT_E2E_HOST, REMOTE_MAT_BIN, FABRIC_INDEX,
# LAST_STDERR_FILE, COMBINED_LOG, EXTRA_ENV。
run_native_default() {
  _run_ssh_capture ssh -n "$MAT_E2E_HOST" env -u MAT_IFACE -u MAT_MATD_IFACE \
    MAT_MATD=0 MAT_LOG=info "MAT_FABRIC_INDEX=$FABRIC_INDEX" "${EXTRA_ENV[@]}" \
    "$REMOTE_MAT_BIN" "$@"
}

# matd 経由（--matd で強制、接続失敗はフォールバック無しのハードエラー）。
# 前提（グローバル）: MAT_E2E_HOST, REMOTE_MAT_BIN, SOCKET, LAST_STDERR_FILE,
# COMBINED_LOG, EXTRA_ENV。
run_matd() {
  _run_ssh_capture ssh -n "$MAT_E2E_HOST" env "${EXTRA_ENV[@]}" \
    "$REMOTE_MAT_BIN" --matd "$SOCKET" "$@"
}

# STAGE=2 の cross-fabric commission 検証専用: 直経路・別 store
# （$FRESH_STORE）・MAT_FABRIC_INDEX も明示 unset（fresh store 側の
# fabric-index は `mat fabric init` が使った CLI 既定値 1 に委ねる —
# 本番 fabric の FABRIC_INDEX=2 を誤って引き継がないため）。
# 前提（グローバル）: MAT_E2E_HOST, REMOTE_MAT_BIN, FRESH_STORE,
# LAST_STDERR_FILE, COMBINED_LOG, EXTRA_ENV。
run_native_fresh() {
  _run_ssh_capture ssh -n "$MAT_E2E_HOST" env -u MAT_IFACE -u MAT_MATD_IFACE -u MAT_FABRIC_INDEX \
    MAT_MATD=0 MAT_LOG=info "${EXTRA_ENV[@]}" \
    "$REMOTE_MAT_BIN" --store "$FRESH_STORE" "$@"
}

# 一時 matd を MAT_MATD_IFACE 無しで起動する（STAGE=1/2 共有、Task13 で
# 関数化。現行 matd CLI（crates/matd/src/main.rs）は `--socket` /
# `--iface` / `--fabric-index` / `--issuer-index` / `--store` のみ —
# `--port` / `--connect` / `--idle-timeout` は Task10 で削除済みなので渡さない。
# 起動待ち + positive marker 確認（"iface auto-selected (matd native
# default)" + "native backend enabled"）まで行い、失敗時は exit 1。
# 前提（グローバル）: MAT_E2E_HOST, REMOTE_MATD_BIN, REMOTE_MATD_LOG,
# REMOTE_MATD_PID, SOCKET, FABRIC_INDEX, STORE, MATD_PATH_OVERRIDE。
# 副作用: グローバル MATD_STARTED=1（呼び出し元の cleanup trap が参照する
# ため `local` にしない）。
#
# 注意: ssh の heredoc 転送には `ssh -n`（ローカル stdin を /dev/null に
# リダイレクト）を使わない — `-n` と heredoc は併用できない
# （e2e-m8a-real.sh の start_matd() と同じ理由）。値は `VAR=val` で一旦
# リモート環境変数へ渡し、heredoc 自体はクォート付き（`<<'REMOTE_SCRIPT'`）
# でローカル展開させない（値にシェルメタ文字があっても安全）。
# MAT_MATD_IFACE は渡す変数リストに含めない（未設定を保証）うえ、リモート
# 側スクリプト冒頭でも明示 unset する（ログインシェルの export 汚染対策）。
start_temp_matd() {
  ssh -n "$MAT_E2E_HOST" "rm -f '$SOCKET'"
  ssh "$MAT_E2E_HOST" \
    MAT_E2E_SOCKET="$SOCKET" \
    MAT_E2E_FABRIC_INDEX="$FABRIC_INDEX" \
    MAT_E2E_STORE="$STORE" \
    MAT_E2E_MATD_BIN="$REMOTE_MATD_BIN" \
    MAT_E2E_MATD_LOG="$REMOTE_MATD_LOG" \
    MAT_E2E_MATD_PID="$REMOTE_MATD_PID" \
    MAT_E2E_MATD_PATH_OVERRIDE="$MATD_PATH_OVERRIDE" \
    'bash -s' <<'REMOTE_SCRIPT'
set -euo pipefail
unset MAT_MATD_IFACE
ARGS=(--fabric-index "$MAT_E2E_FABRIC_INDEX" --socket "$MAT_E2E_SOCKET")
[ -n "$MAT_E2E_STORE" ] && ARGS+=(--store "$MAT_E2E_STORE")
export RUST_LOG=info
[ -n "$MAT_E2E_MATD_PATH_OVERRIDE" ] && export PATH="$MAT_E2E_MATD_PATH_OVERRIDE"
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
}

# STAGE=1/2 共有 op スイープ: env 未設定 native 直経路（discover / read /
# write / invoke / describe / diag thread / diag node --deep / open-window /
# group provision(+--rebind) / group invoke）→ 一時 matd 経由（read / write /
# group invoke）→ 全ログ結合での "falling back" 0件確認。$1 = ログ用ラベル
# （例 "STAGE=1" / "STAGE=2 (chip-tool absent)"）。
# 前提（グローバル、呼び出し元が呼び出し前に設定）: MAT_E2E_HOST,
# REMOTE_MAT_BIN, REMOTE_MATD_BIN, REMOTE_MATD_LOG, REMOTE_MATD_PID, SOCKET,
# FABRIC_INDEX, GROUP, KEYSET, ENDPOINT, STORE, STORE_ARG(配列), NODE,
# GROUP_NODES, GROUP_NODE_ARR(配列), LAST_STDERR_FILE, COMBINED_LOG,
# EXTRA_ENV(配列), MATD_PATH_OVERRIDE。
# 副作用: グローバル MATD_STARTED（呼び出し元 cleanup trap 用）、
# グローバル IFACE_CHECK_LOG（discover 呼び出しの stderr コピー — STAGE=1
# の検証6・iface 一意選択実測が使う。呼び出し元が rm する）。
op_sweep() {
  local label=$1
  local GROUP_NAME=e2e-m8c3

  echo "== [$label] env 未設定 native スイープ（直経路、node=$NODE, group=$GROUP keyset=$KEYSET, group-nodes=$GROUP_NODES）"

  echo "-- [$label] discover"
  local DISCOVER_OUT
  DISCOVER_OUT=$(run_native_default discover)
  echo "$DISCOVER_OUT"
  assert_no_fallback "discover (native default)"
  assert_native_marker "iface auto-selected (native default)" "discover (native default)"
  assert_grep '"devices"' "$DISCOVER_OUT" "discover が devices を含まない"
  IFACE_CHECK_LOG=$(mktemp)
  cp "$LAST_STDERR_FILE" "$IFACE_CHECK_LOG"

  echo "-- [$label] read (baseline, onoff on-off)"
  local READ_OUT
  READ_OUT=$(run_native_default "${STORE_ARG[@]}" read -n "$NODE" -e "$ENDPOINT" -c onoff -a on-off)
  echo "$READ_OUT"
  assert_no_fallback "read (native default)"
  assert_native_marker "iface auto-selected (native default)" "read (native default)"
  assert_native_marker "read executed (native direct)" "read (native default)"

  echo "-- [$label] write (levelcontrol on-level=77 → read back → revert to null)"
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
  echo "PASS: [$label] write 検証（native default, 反映+revert確認）" >&2

  echo "-- [$label] invoke (onoff toggle → 逆toggle)"
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
  confirm "[$label] node $NODE の点灯状態が2回の toggle で元に戻ったことを目視確認してください（任意）"
  echo "PASS: [$label] invoke 検証（native default, toggle往復）" >&2

  echo "-- [$label] describe"
  local DESCRIBE_OUT
  DESCRIBE_OUT=$(run_native_default "${STORE_ARG[@]}" describe -n "$NODE")
  echo "$DESCRIBE_OUT"
  assert_no_fallback "describe (native default)"
  assert_native_marker "iface auto-selected (native default)" "describe (native default)"
  assert_native_marker "describe executed (native direct)" "describe (native default)"
  assert_grep '"endpoints"' "$DESCRIBE_OUT" "describe が endpoints を含まない"

  echo "-- [$label] diag thread"
  local DIAG_THREAD_OUT
  DIAG_THREAD_OUT=$(run_native_default "${STORE_ARG[@]}" diag thread -n "$NODE")
  echo "$DIAG_THREAD_OUT"
  assert_no_fallback "diag thread (native default)"
  assert_native_marker "iface auto-selected (native default)" "diag thread (native default)"
  assert_native_marker "diag thread executed (native direct)" "diag thread (native default)"

  echo "-- [$label] diag node --deep"
  local DIAG_NODE_OUT
  DIAG_NODE_OUT=$(run_native_default "${STORE_ARG[@]}" diag node -n "$NODE" --deep)
  echo "$DIAG_NODE_OUT"
  assert_no_fallback "diag node --deep (native default)"
  assert_native_marker "iface auto-selected (native default)" "diag node --deep (native default)"
  assert_native_marker "diag node executed (native)" "diag node --deep (native default)"
  assert_grep '"verdict"' "$DIAG_NODE_OUT" "diag node --deep が verdict を含まない"

  echo "-- [$label] open-window"
  local OW_OUT
  OW_OUT=$(run_native_default "${STORE_ARG[@]}" open-window -n "$NODE")
  echo "$OW_OUT"
  assert_no_fallback "open-window (native default)"
  assert_native_marker "iface auto-selected (native default)" "open-window (native default)"
  assert_native_marker "open-window executed (native direct)" "open-window (native default)"
  assert_grep '"qr_payload"' "$OW_OUT" "open-window が qr_payload を含まない"

  echo "-- [$label] group provision の pre-clean（前回 run の残留 state を best-effort 除去、再実行耐性）"
  cleanup_disposable_group

  echo "-- [$label] group provision（使い捨て group=$GROUP keyset=$KEYSET nodes=$GROUP_NODES）"
  local GP_OUT GP_RC
  GP_RC=0
  # shellcheck disable=SC2086 # GROUP_NODES は意図的に空白展開する（--nodes にそのまま渡す）
  GP_OUT=$(run_native_default "${STORE_ARG[@]}" group provision -g "$GROUP" --nodes $GROUP_NODES \
    --keyset-id "$KEYSET" --name "$GROUP_NAME") || GP_RC=$?
  if [ "$GP_RC" != 0 ]; then
    echo "FAIL: [$label] group provision (native default) が exit $GP_RC で失敗した（pre-clean 後でもこれが起きる場合は再実行耐性の問題ではなく実際の provision 失敗）:" >&2
    cat "$LAST_STDERR_FILE" >&2
    exit 1
  fi
  echo "$GP_OUT"
  assert_no_fallback "group provision (native default)"
  assert_native_marker "iface auto-selected (native default)" "group provision (native default)"
  assert_native_marker "group provision controller state written (native kvs)" "group provision (native default, kvs write)"
  assert_native_marker "group provision executed (native direct)" "group provision (native default, 完走)"
  assert_grep '"status":"provisioned"' "$GP_OUT" "group provision が status:provisioned を返さない"
  assert_grep '"note":".*restart' "$GP_OUT" "group provision の note に restart 案内が無い"

  echo "-- [$label] group provision --rebind（再実行、Duplicate にならないこと）"
  GP_RC=0
  # shellcheck disable=SC2086
  GP_OUT=$(run_native_default "${STORE_ARG[@]}" group provision -g "$GROUP" --nodes $GROUP_NODES \
    --keyset-id "$KEYSET" --name "$GROUP_NAME" --rebind) || GP_RC=$?
  if [ "$GP_RC" != 0 ]; then
    echo "FAIL: [$label] group provision --rebind (native default) が exit $GP_RC で失敗した:" >&2
    cat "$LAST_STDERR_FILE" >&2
    exit 1
  fi
  echo "$GP_OUT"
  assert_no_fallback "group provision --rebind (native default)"
  assert_native_marker "group provision controller state written (native kvs)" "group provision --rebind (native default, kvs write)"
  assert_native_marker "group provision executed (native direct)" "group provision --rebind (native default, 完走)"
  assert_grep '"status":"provisioned"' "$GP_OUT" "group provision --rebind が status:provisioned を返さない"
  echo "PASS: [$label] group provision 検証（native default, pre-clean+通常+--rebind 全て）" >&2

  echo "-- [$label] group invoke（toggle → 逆toggle）"
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
  confirm "[$label] group $GROUP メンバー ($GROUP_NODES) が2回の toggle で元に戻ったことを目視確認してください（任意）"
  echo "PASS: [$label] env 未設定 native スイープ全項目 GREEN（discover/read/write/invoke/describe/diag thread/diag node --deep/open-window/group provision(+rebind)/group invoke、全て iface auto-selected (native default) + 各 positive marker + fallback 不在）" >&2

  echo "== [$label] matd 経路（MAT_MATD_IFACE 無しで一時起動 → read/write/group invoke）"
  start_temp_matd

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
    echo "FAIL: [$label] matd ログに 'chip-tool ws raw response' が出ている — read/write/group invoke のいずれかが native を通らず chip-tool 経由で実行された可能性" >&2
    printf '%s\n' "$MATD_FULL_LOG" >&2
    exit 1
  fi
  ssh -n "$MAT_E2E_HOST" "cat '$REMOTE_MATD_LOG'" >> "$COMBINED_LOG"

  ssh -n "$MAT_E2E_HOST" "'$REMOTE_MATD_BIN' stop --socket '$SOCKET'"
  MATD_STARTED=0
  echo "PASS: [$label] matd 経路（MAT_MATD_IFACE 無し起動 + read/write/group invoke 成功、chip-tool traffic 無し）" >&2

  echo "== [$label] フォールバック発火ゼロ（全ログ結合）"
  if grep -q "falling back" "$COMBINED_LOG"; then
    echo "FAIL: [$label] 結合ログに 'falling back' が出現している" >&2
    grep "falling back" "$COMBINED_LOG" >&2
    exit 1
  fi
  echo "PASS: [$label] フォールバック発火ゼロ（結合ログ 0 件）" >&2
}

# ---------------------------------------------------------------------------
# STAGE=2（Task13）— chip-tool 完全撤去後の最終受け入れ。
# design の「実機 E2E ゲート2」/ brief task-13-brief.md 準拠:
#   1. 準備: aarch64-gnu + BLE クロスビルド（`task dist:arm64` 相当）→ scp
#      （/tmp、本番 /usr/local/bin とは別）→ `file`/`--version` で成果物
#      確認 → 本番 matd 停止（trap で復帰）→ KVS backup。
#   2. chip-tool を PATH から外した環境（EXTRA_ENV=(PATH=/usr/bin:/bin)）で
#      op_sweep() を再実行、全合格 + 配布バイナリに
#      "falling back to chip-tool" という厳密文字列が無いことを直接 grep
#      （suspenders）。
#   3a. `mat fabric init` 実機検証（安全・デバイス非接触）: 別 store
#      （mktemp -d）で init → JSON 確認 → KVS ファイル実在 → epoch キー
#      存在確認（値は非出力）→ 再 init 拒否（exit 1, kind:other）。
#   3b. cross-fabric commission 検証（危険操作、confirm_yn ゲート）: 実
#      fabric 側から対象ノードへ open-window → fresh store から on-network
#      commission（multi-admin join）→ 新 fabric で read 成功 → 新 fabric
#      だけを RemoveFabric（元 fabric は一度も触らない — gate1 で判明した
#      NL68 の RemoveFabric→再commission 不成立パターンを再利用しない）→
#      元 fabric からの read が引き続き成功することを確認。
#   4. BLE feature 実証: 配布バイナリの ldd（libdbus 動的リンク）+ バイナリ
#      内の BLE 専用コード痕跡（"bluez-session"）を確認。ライブ BLE
#      commission は M8c-1 ゲート1で実機2回実証済みのため再実施しない
#      （玄関ライトの再 factory reset を避ける — spec の WARN + 人力確認
#      エスケープ）。
#   5. `task check`（ローカル）+ `task docker:build`。
# ---------------------------------------------------------------------------
stage2_main() {
  : "${MAT_E2E_HOST:?MAT_E2E_HOST (ssh host) required}"
  : "${MAT_E2E_NODE:?MAT_E2E_NODE (commissioned node id for the read/write/invoke/describe/diag/open-window sweep) required}"
  : "${MAT_E2E_GROUP_NODES:?MAT_E2E_GROUP_NODES (space-separated commissioned node ids for the throwaway group 99 provision) required}"
  command -v jq >/dev/null 2>&1 || { echo "jq が必要です（JSON 抽出に使用）" >&2; exit 1; }
  command -v cross >/dev/null 2>&1 || { echo "cross が必要です（aarch64-gnu + BLE クロスビルドに使用）" >&2; exit 1; }

  # cleanup trap から参照するため意図的に `local` を付けない（STAGE=1 と
  # 同じ理由 — stage2_main 冒頭のコメント参照）。
  local FABRIC_INDEX GROUP KEYSET ENDPOINT STORE CHIP_TOOL_BIN
  local NODE GROUP_NODES
  FABRIC_INDEX="${MAT_E2E_FABRIC_INDEX:-2}"
  GROUP="${MAT_E2E_GROUP_ID:-99}"
  KEYSET="${MAT_E2E_KEYSET_ID:-99}"
  ENDPOINT="${MAT_E2E_ENDPOINT:-1}"
  STORE="${MAT_E2E_STORE:-}"
  CHIP_TOOL_BIN="${MAT_E2E_CHIP_TOOL_BIN:-}"
  NODE="$MAT_E2E_NODE"
  GROUP_NODES="$MAT_E2E_GROUP_NODES"
  SOCKET="${MAT_E2E_SOCKET:-/tmp/matd-e2e-m8c3-s2.sock}"
  # shellcheck disable=SC2206 # GROUP_NODES は意図的に空白分割する（--nodes にそのまま渡す）
  local GROUP_NODE_ARR=($GROUP_NODES)
  local TARGET=aarch64-unknown-linux-gnu
  REMOTE_MAT_BIN=/tmp/mat-e2e-m8c3-s2
  REMOTE_MATD_BIN=/tmp/matd-e2e-m8c3-s2
  REMOTE_MATD_LOG=/tmp/matd-e2e-m8c3-s2.log
  REMOTE_MATD_PID=/tmp/matd-e2e-m8c3-s2.pid
  FRESH_STORE=""

  echo "== 準備1/5: aarch64-gnu + BLE クロスビルド（task dist:arm64 相当、Cross.toml の arm64 libdbus pre-build 使用）"
  cross build --release --target "$TARGET" --features ble -p mat -p matd
  local MAT_BIN="target/$TARGET/release/mat"
  local MATD_BIN="target/$TARGET/release/matd"

  local FILE_OUT
  FILE_OUT=$(file "$MAT_BIN")
  echo "$FILE_OUT"
  printf '%s' "$FILE_OUT" | grep -q 'aarch64' || { echo "FAIL: mat が aarch64 バイナリでない: $FILE_OUT" >&2; exit 1; }
  printf '%s' "$FILE_OUT" | grep -q 'dynamically linked' || { echo "FAIL: mat が動的リンクでない（stale musl バイナリの取り違え？ BLE feature 無しの疑い）: $FILE_OUT" >&2; exit 1; }
  FILE_OUT=$(file "$MATD_BIN")
  echo "$FILE_OUT"
  printf '%s' "$FILE_OUT" | grep -q 'aarch64' || { echo "FAIL: matd が aarch64 バイナリでない: $FILE_OUT" >&2; exit 1; }
  printf '%s' "$FILE_OUT" | grep -q 'dynamically linked' || { echo "FAIL: matd が動的リンクでない: $FILE_OUT" >&2; exit 1; }

  if grep -q "falling back to chip-tool" "$MAT_BIN"; then
    echo "FAIL: 配布バイナリ (mat) に 'falling back to chip-tool' 文字列が残っている（chip-tool 撤去漏れ）" >&2
    exit 1
  fi
  echo "PASS: 配布バイナリに 'falling back to chip-tool' 文字列が無い（撤去済みの直接実証、suspenders — belt は後述の op スイープ結合ログでの 'falling back' 0件確認）"

  LAST_STDERR_FILE=$(mktemp)
  COMBINED_LOG=$(mktemp)
  REMOTE_STORE=""
  MATD_STARTED=0

  cleanup() {
    echo "== cleanup: 一時 matd 停止 + ssh 先の一時バイナリ削除 + fresh store 削除 + 本番 matd 復帰 ($MAT_E2E_HOST) =="
    if [ "$MATD_STARTED" = "1" ]; then
      ssh -n "$MAT_E2E_HOST" "'$REMOTE_MATD_BIN' stop --socket '$SOCKET'" 2>/dev/null || true
      ssh -n "$MAT_E2E_HOST" "kill \"\$(cat '$REMOTE_MATD_PID' 2>/dev/null)\" 2>/dev/null" || true
    fi
    if [ -n "$FRESH_STORE" ]; then
      ssh -n "$MAT_E2E_HOST" "rm -rf '$FRESH_STORE'" || true
    fi
    ssh -n "$MAT_E2E_HOST" "rm -f '$REMOTE_MAT_BIN' '$REMOTE_MATD_BIN' '$REMOTE_MATD_PID' '$REMOTE_MATD_LOG' '$SOCKET'" || true
    ssh -n "$MAT_E2E_HOST" "sudo systemctl start matd" || true
    if [ -n "$REMOTE_STORE" ]; then
      echo "== KVS バックアップの案内 =="
      echo "  jarvis 上の $REMOTE_STORE/chip_tool_config.ini.bak-m8c3-s2 に検証前の状態を保存しています。"
      echo "  問題があれば内容を見比べたうえで手動判断してください:"
      echo "    ssh $MAT_E2E_HOST \"diff '$REMOTE_STORE/chip_tool_config.ini.bak-m8c3-s2' '$REMOTE_STORE/chip_tool_config.ini'\""
    fi
    rm -f "$LAST_STDERR_FILE" "$COMBINED_LOG" "${IFACE_CHECK_LOG:-}"
  }
  trap cleanup EXIT

  echo "== 準備2/5: 転送 → $MAT_E2E_HOST ($REMOTE_MAT_BIN / $REMOTE_MATD_BIN, 本番 /usr/local/bin/{mat,matd} とは別)"
  scp "$MAT_BIN" "$MAT_E2E_HOST:$REMOTE_MAT_BIN"
  scp "$MATD_BIN" "$MAT_E2E_HOST:$REMOTE_MATD_BIN"
  ssh -n "$MAT_E2E_HOST" "chmod +x '$REMOTE_MAT_BIN' '$REMOTE_MATD_BIN'"

  local VER_OUT
  VER_OUT=$(ssh -n "$MAT_E2E_HOST" "'$REMOTE_MAT_BIN' --version")
  echo "$VER_OUT"
  printf '%s' "$VER_OUT" | grep -q '0\.22\.0' || { echo "FAIL: リモート mat --version が 0.22.0 でない: $VER_OUT" >&2; exit 1; }
  VER_OUT=$(ssh -n "$MAT_E2E_HOST" "'$REMOTE_MATD_BIN' --version")
  echo "$VER_OUT"
  printf '%s' "$VER_OUT" | grep -q '0\.22\.0' || { echo "FAIL: リモート matd --version が 0.22.0 でない: $VER_OUT" >&2; exit 1; }
  echo "PASS: 配布成果物確認（aarch64 動的リンク + --version 0.22.0、mat/matd 両方）"

  local STORE_ARG=()
  [ -n "$STORE" ] && STORE_ARG=(--store "$STORE")

  echo "== 準備3/5: store 解決 + 本番 matd 停止 + KVS backup"
  if [ -n "$STORE" ]; then
    REMOTE_STORE="$STORE"
  else
    REMOTE_STORE=$(ssh -n "$MAT_E2E_HOST" 'echo "${MAT_STORE:-${XDG_CONFIG_HOME:-$HOME/.config}/mat}"')
  fi
  echo "store = $REMOTE_STORE"

  echo "sudo systemctl stop matd（trap で必ず復帰）"
  ssh -n "$MAT_E2E_HOST" "sudo systemctl stop matd"

  echo "KVS backup: $REMOTE_STORE/chip_tool_config.ini -> .bak-m8c3-s2（上書きしない -n 付き）"
  ssh -n "$MAT_E2E_HOST" "cp -n '$REMOTE_STORE/chip_tool_config.ini' '$REMOTE_STORE/chip_tool_config.ini.bak-m8c3-s2'" \
    || echo "WARN: KVS backup が失敗（既に .bak-m8c3-s2 が存在？ 上書きはしていません）" >&2

  echo "== 検証1/5: chip-tool を PATH から外した環境（PATH=/usr/bin:/bin）で op スイープ再実行"
  EXTRA_ENV=(PATH=/usr/bin:/bin)
  MATD_PATH_OVERRIDE=/usr/bin:/bin
  op_sweep "STAGE=2 (chip-tool absent)"
  EXTRA_ENV=()
  MATD_PATH_OVERRIDE=""
  rm -f "$IFACE_CHECK_LOG"
  echo "PASS: 検証1 chip-tool 不在環境（PATH=/usr/bin:/bin）での op スイープ全合格 + 配布バイナリに 'falling back to chip-tool' 文字列無し（belt+suspenders 実証）"

  echo "== 検証2/5: mat fabric init 実機検証 3a（安全・デバイス非接触部分、実運用 fabric は一度も触らない）"
  FRESH_STORE=$(ssh -n "$MAT_E2E_HOST" 'mktemp -d')
  echo "fresh store = $FRESH_STORE"

  local FI_JSON
  FI_JSON=$(run_native_fresh fabric init) || { echo "FAIL: fabric init (fresh store) が失敗した" >&2; exit 1; }
  echo "$FI_JSON"
  assert_no_fallback "fabric init (fresh store)"
  assert_native_marker "fabric bootstrap written (native kvs)" "fabric init (fresh store)"
  assert_grep '"fabric_id"' "$FI_JSON" "fabric init が fabric_id を含まない"
  assert_grep '"fabric_index"' "$FI_JSON" "fabric init が fabric_index を含まない"
  assert_grep '"compressed_fabric_id"' "$FI_JSON" "fabric init が compressed_fabric_id を含まない"
  assert_grep '"admin_node_id"' "$FI_JSON" "fabric init が admin_node_id を含まない"

  ssh -n "$MAT_E2E_HOST" "test -f '$FRESH_STORE/chip_tool_config.ini'" \
    || { echo "FAIL: fresh store に chip_tool_config.ini が無い" >&2; exit 1; }
  ssh -n "$MAT_E2E_HOST" "test -f '$FRESH_STORE/chip_tool_config.alpha.ini'" \
    || { echo "FAIL: fresh store に chip_tool_config.alpha.ini が無い" >&2; exit 1; }
  # epoch キーの値は秘匿材料（IPK 導出の種）— 存在確認は grep -q のみ、
  # 値は一切 stdout/stderr に出さない（fabric-index はここでは CLI 既定値 1）。
  ssh -n "$MAT_E2E_HOST" "grep -q '^mat/f/1/ipk-epoch=' '$FRESH_STORE/chip_tool_config.ini'" \
    || { echo "FAIL: fresh store の KVS に mat/f/1/ipk-epoch が無い（値は出力しない）" >&2; exit 1; }
  echo "PASS: fabric init 新規 KVS 生成（JSON + KVS ファイル実在 + epoch キー確認、値は非出力）"

  echo "-- 再 init 拒否（exit 1, kind:other を期待 — 以下の 'FAIL: remote command failed' 行は想定内、拒否そのものの実証）"
  local REINIT_RC=0
  run_native_fresh fabric init >/dev/null || REINIT_RC=$?
  if [ "$REINIT_RC" != 1 ]; then
    echo "FAIL: 再 init が exit 1 でない (rc=$REINIT_RC) — AlreadyExists の分類が変わった可能性" >&2
    cat "$LAST_STDERR_FILE" >&2
    exit 1
  fi
  assert_grep '"kind":"other"' "$(cat "$LAST_STDERR_FILE")" "再 init エラーが kind:other を返さない"
  echo "PASS: 検証2 (3a) mat fabric init 実機検証（新規 KVS 生成 + 再 init 拒否、exit 1 / kind:other）" >&2

  echo "== 検証3/5: cross-fabric commission 実機検証 3b（危険操作: 対象ノードへの multi-admin join、元 fabric には一度も触れない）"
  echo "WARN: node $NODE を一時的に第2 fabric（fresh store）へ join させます。実行判断は下記プロンプトで。" >&2
  if confirm_yn "検証3（cross-fabric commission、node=$NODE への open-window+commission+新fabricのRemoveFabricを伴う）を実行しますか"; then
    echo "-- open-window（元 fabric、node=$NODE、STORE_ARG=${STORE_ARG[*]:-<default>}）"
    local OW_OUT OW_MANUAL_CODE
    OW_OUT=$(run_native_default "${STORE_ARG[@]}" open-window -n "$NODE") \
      || { echo "FAIL: open-window (cross-fabric prep) が失敗した" >&2; exit 1; }
    echo "$OW_OUT"
    assert_no_fallback "open-window (cross-fabric prep)"
    OW_MANUAL_CODE=$(printf '%s' "$OW_OUT" | jq -r '.manual_code')
    if [ -z "$OW_MANUAL_CODE" ] || [ "$OW_MANUAL_CODE" = "null" ]; then
      echo "FAIL: open-window の manual_code が取得できない" >&2
      exit 1
    fi

    echo "-- fresh store から on-network commission（manual code、commissioning window 再オープン待ちのため最大3回リトライ）"
    local NEW_NODE_JSON ok attempt
    ok=0
    for attempt in 1 2 3; do
      if NEW_NODE_JSON=$(run_native_fresh commission --target "m8c3-cross-fabric-probe" --setup-code "$OW_MANUAL_CODE"); then
        ok=1
        break
      fi
      echo "retry $attempt/3: cross-fabric commission 失敗、3秒後に再試行します" >&2
      sleep 3
    done
    if [ "$ok" != "1" ]; then
      echo "FAIL: cross-fabric commission が3回とも失敗した。最後の試行の stderr:" >&2
      cat "$LAST_STDERR_FILE" >&2
      exit 1
    fi
    echo "$NEW_NODE_JSON"
    assert_no_fallback "commission (cross-fabric, fresh store)"
    assert_native_marker "commission executed (native on-network)" "commission (cross-fabric, fresh store)"
    assert_grep '"status":"success"' "$NEW_NODE_JSON" "cross-fabric commission が status:success を返さない"
    local NEW_NODE_ID
    NEW_NODE_ID=$(printf '%s' "$NEW_NODE_JSON" | jq -r '.node_id')
    if [ -z "$NEW_NODE_ID" ] || [ "$NEW_NODE_ID" = "null" ]; then
      echo "FAIL: cross-fabric commission の node_id が取得できない" >&2
      exit 1
    fi
    echo "new fabric 上の node_id = $NEW_NODE_ID"

    echo "-- 新 fabric で read（node=$NEW_NODE_ID）"
    local NEW_READ_OUT
    NEW_READ_OUT=$(run_native_fresh read -n "$NEW_NODE_ID" -e "$ENDPOINT" -c onoff -a on-off) \
      || { echo "FAIL: 新 fabric での read が失敗した" >&2; exit 1; }
    echo "$NEW_READ_OUT"
    assert_grep '"value"' "$NEW_READ_OUT" "新 fabric read が value を含まない"
    echo "PASS: cross-fabric commission → 新 fabric read 成功" >&2

    echo "-- 後片付け: 新 fabric の current-fabric-index 取得 + RemoveFabric（元 fabric には触れない）"
    local NEW_FI_OUT NEW_DEV_FI
    NEW_FI_OUT=$(run_native_fresh read -n "$NEW_NODE_ID" -e 0 -c operationalcredentials -a current-fabric-index) \
      || { echo "FAIL: 新 fabric の current-fabric-index 読み出しが失敗した — node $NODE に第2 fabric が残留している可能性、手動確認してください" >&2; exit 1; }
    NEW_DEV_FI=$(printf '%s' "$NEW_FI_OUT" | jq -r '.value')
    if [ -z "$NEW_DEV_FI" ] || [ "$NEW_DEV_FI" = "null" ]; then
      echo "FAIL: 新 fabric の current-fabric-index が取得できない — node $NODE に第2 fabric が残留している可能性、手動確認してください" >&2
      exit 1
    fi
    run_native_fresh invoke -n "$NEW_NODE_ID" -e 0 -c operationalcredentials --command remove-fabric "$NEW_DEV_FI" >/dev/null \
      || { echo "FAIL: 新 fabric の RemoveFabric が失敗した — node $NODE に第2 fabric が残留している可能性。手動で groupsettings/操作をご確認ください" >&2; exit 1; }
    echo "PASS: 新 fabric を RemoveFabric（元 fabric は無傷のはず）" >&2

    echo "-- 検証: 元 fabric（$REMOTE_STORE）からの read が引き続き成功すること"
    local FINAL_READ_OUT
    FINAL_READ_OUT=$(run_native_default "${STORE_ARG[@]}" read -n "$NODE" -e "$ENDPOINT" -c onoff -a on-off) \
      || { echo "FAIL: 元 fabric からの read が RemoveFabric 後に失敗した — 元 fabric に影響が出た可能性、手動確認してください" >&2; exit 1; }
    echo "$FINAL_READ_OUT"
    assert_grep '"value"' "$FINAL_READ_OUT" "元 fabric read が value を含まない"
    echo "PASS: 検証3 (3b) cross-fabric commission（open-window → commission → read → 新fabricのみRemoveFabric → 元fabric無傷確認）" >&2
  else
    echo "SKIP: 検証3 (3b) cross-fabric commission（人力判断によりスキップ）— このゲート実行では未検証です" >&2
  fi

  ssh -n "$MAT_E2E_HOST" "rm -rf '$FRESH_STORE'" || true
  FRESH_STORE=""

  echo "== 検証4/5: BLE feature 実証（配布バイナリの静的痕跡。ライブ BLE commission は M8c-1 ゲート1で実機2回実証済みのため再実施しない）"
  local LDD_OUT
  LDD_OUT=$(ssh -n "$MAT_E2E_HOST" "ldd '$REMOTE_MAT_BIN'" 2>&1 || true)
  echo "$LDD_OUT"
  if ! printf '%s' "$LDD_OUT" | grep -q 'libdbus-1\.so'; then
    echo "FAIL: 配布バイナリが libdbus（bluer/BLE 依存）に動的リンクされていない — ble feature 抜けの疑い" >&2
    exit 1
  fi
  local BLE_STR_COUNT
  BLE_STR_COUNT=$(ssh -n "$MAT_E2E_HOST" "grep -ac 'bluez-session' '$REMOTE_MAT_BIN'" || true)
  if [ "${BLE_STR_COUNT:-0}" -lt 1 ] 2>/dev/null; then
    echo "FAIL: 配布バイナリに BLE commission 経路のコード痕跡（'bluez-session'）が無い — ble feature 抜けの疑い" >&2
    exit 1
  fi
  echo "PASS: 配布バイナリは BLE feature 込み（libdbus 動的リンク + BLE 専用コード痕跡 'bluez-session' 確認）"
  echo "NOTE: BLE+Thread commission のライブ実証は M8c-1 実機ゲート1で2回実施・記録済み（玄関ライト、gnu+ble ビルド）。本ゲートでは対象デバイスの再 factory reset を避けるため再実施しない（design の WARN + 人力確認エスケープを適用 — 実施したい場合は e2e-m8c1-real.sh の検証2 相当を手動で流す）。" >&2

  echo "== 検証5/5: task check（ローカル）+ Docker イメージビルド"
  task check
  task docker:build
  echo "PASS: 検証5 task check 全通過 + docker:build 成功" >&2

  echo "== 後片付け（使い捨て group $GROUP、best-effort）"
  # 注意: cleanup_disposable_group の controller-side 部分は chip-tool を ssh 先
  # PATH で探すため、STAGE=2 の PATH=/usr/bin:/bin 環境下では失敗・WARN-skip が
  # 想定内です。デバイス側の native remove-group が実効部分で、controller-side の
  # KVS groupsettings 残留は次回 provision の pre-clean で処理されます（m8c1/m8c2 参照）。
  cleanup_disposable_group

  echo "== e2e:m8c3:real STAGE=2 PASS（検証1〜5 GREEN。検証3 は上記ログの PASS/SKIP を確認、検証4 は静的実証 — ライブ BLE は gate1 記録参照）"
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
  # ある）。同じ理由で run_native_default() / run_matd() / start_temp_matd() /
  # op_sweep() は top-level 関数にしてある（STAGE=2 とも共有 — Task13）。
  local FABRIC_INDEX GROUP KEYSET ENDPOINT STORE CHIP_TOOL_BIN
  local NODE GROUP_NODES
  FABRIC_INDEX="${MAT_E2E_FABRIC_INDEX:-2}"
  GROUP="${MAT_E2E_GROUP_ID:-99}"
  KEYSET="${MAT_E2E_KEYSET_ID:-99}"
  ENDPOINT="${MAT_E2E_ENDPOINT:-1}"
  STORE="${MAT_E2E_STORE:-}"
  CHIP_TOOL_BIN="${MAT_E2E_CHIP_TOOL_BIN:-}"
  NODE="$MAT_E2E_NODE"
  GROUP_NODES="$MAT_E2E_GROUP_NODES"
  SOCKET="${MAT_E2E_SOCKET:-/tmp/matd-e2e-m8c3.sock}"
  # shellcheck disable=SC2206 # GROUP_NODES は意図的に空白分割する（--nodes にそのまま渡す）
  local GROUP_NODE_ARR=($GROUP_NODES)
  local TARGET=aarch64-unknown-linux-musl
  REMOTE_MAT_BIN=/tmp/mat-e2e-m8c3
  REMOTE_MATD_BIN=/tmp/matd-e2e-m8c3
  REMOTE_MATD_LOG=/tmp/matd-e2e-m8c3.log
  REMOTE_MATD_PID=/tmp/matd-e2e-m8c3.pid
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
    rm -f "$LAST_STDERR_FILE" "$COMBINED_LOG" "${IFACE_CHECK_LOG:-}"
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

  # KVS の mat-epoch キーの有無だけを判定する（値は絶対に出力しない — 秘匿
  # 材料。grep -q のみ使用、-c であっても値行を晒さないよう常に -q で統一）。
  kvs_has_epoch_key() {
    ssh -n "$MAT_E2E_HOST" "grep -q '$EPOCH_KEY_PATTERN' '$REMOTE_STORE/chip_tool_config.ini'"
  }

  echo "== 検証2〜4/7: env 未設定 native スイープ + matd 経路 + フォールバック発火ゼロ（共有 op_sweep）"
  op_sweep "STAGE=1"

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
  cleanup_disposable_group

  echo "sudo systemctl restart matd（本番 native group state 再読込のため起動 — trap の cleanup でも start するが、後続の目視確認のためここで明示的に起動する）"
  ssh -n "$MAT_E2E_HOST" "sudo systemctl restart matd"
  # shellcheck disable=SC2034 # i はカウンタとしてのみ使う（10回上限のポーリングループ）
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
