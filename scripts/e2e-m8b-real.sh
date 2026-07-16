#!/usr/bin/env bash
# Phase 5 M8b 受け入れ: `mat discover` / `mat discover --probe` / `mat diag node
# --deep` の mDNS プローブ（commissionable 探索・到達性判定）が MAT_IFACE 設定時
# に chip-tool / avahi-browse を一切 spawn せず native browse（mat-controller
# ::dnssd）だけで完走すること、出力 JSON スキーマが chip-tool 経路と一致する
# こと、MAT_IFACE 未設定時のフォールバックが健全であることを jarvis 上で検証
# する（本番 systemd matd, port 9100 には触れない — discover/diag は常に直経路
# で matd 非対応 op のため、そもそも matd を経由しない）。
#
# 骨格は scripts/e2e-m8a-real.sh を流用（trap 後始末 / stderr への PASS/FAIL /
# 実ノード ID を環境変数で注入しハードコードしない / cross-build → ssh cat 転送
# → リモート実行の house style）。m8a と異なり本ハーネスは matd・group counter
# ・on-level 書き込みに一切触れない（discover/probe/diag --deep は read-only）
# ため、matd 起動やクリーンアップの on-level 復元は不要（m8a より単純）。
#
# native 実行の実アサーションは「外部バイナリを実在させない」方式:
# MAT_CHIP_TOOL_BIN=/nonexistent/... と MAT_AVAHI_BROWSE_BIN=/nonexistent/...
# を立てて走らせる。フォールバックが起きれば discover の commissionable 経路は
# 即 exit 12（chip-tool 不在 = child_not_found）、probe（mdns）経路は
# reachable:null（probe 内部で拾って None を返すだけでコマンド自体は失敗しない
# ため）になるので、コマンドが成功し reachable:true が返ること自体が純 native
# の証明になる（marker grep より強い）。加えて positive marker（`discover
# executed (native browse)` / `probe executed (native browse)`、`MAT_LOG=info`
# 必須 — info レベルなので既定の warn フィルタでは出ない）も grep する。
#
# 検証項目（brief 通し番号）:
#   1. native discover + probe（外部バイナリ無効化）: 全 commissioned ノードが
#      reachable:true + address 非null、positive marker 2件、fallback 不在。
#   2. commissionable 検出（玄関ライト等、discriminator 付き）。0件は WARN+
#      人力確認（FAIL にしない — 玄関ライトが広告を止めている場合があるため）。
#   3. diag node --deep native。
#   4. MAT_IFACE 無し（実 chip-tool + 実 avahi）との出力構造一致。
#   5. フォールバック健全性（MAT_IFACE に存在しない iface 名を与える）。
#
# ★judgment: 検証3（diag node --deep）は MAT_AVAHI_BROWSE_BIN のみを無効化し
# MAT_CHIP_TOOL_BIN は実バイナリのまま使う。`mat diag node` は mDNS プローブ
# 以外に descriptor read（operational チェック）と threadnetworkdiagnostics
# read（thread チェック、neighbor-table/routing-role）を常に chip-tool 経由で
# 行う（M8b は「discover の commissionable 探索」と「probe の mDNS 部分」だけを
# native 化する設計で、diag node の IM 部分は M8c まで chip-tool のまま
# — 2026-07-17 design doc 参照）。そのため diag node --deep で chip-tool まで
# 無効化すると、mDNS が native で動いているかどうかに関係なく operational
# チェックの descriptor read が child_not_found (exit 12) で即死し、本来見たい
# 「mDNS だけが native で動いたか」を検証できなくなる。avahi のみを殺せば、
# native mDNS が失敗して avahi へフォールバックした場合にのみ確実に失敗する
# （probe::mdns の Err はコマンド全体を落とさず checks.mdns を欠落させるだけ
# なので、advertised_any_fabric/advertised_self_fabric の true 判定が失敗の
# 実アサーションになる）。
#
# ★judgment: MAT_MATD=0 を全呼び出しに belt-and-braces で付与する。実際には
# discover/diag は matd_client::to_op が Err を返す「matd 非対応 op」なので
# MAT_MATD の値に関わらず（自動検出モードでも）常に直経路へ落ちる
# （crates/mat/src/matd_client.rs 参照）。実害はないが m8a との一貫性のため
# 明示する。
#
# ★judgment: MAT_E2E_NODE（diag --deep の対象単一ノード）を別の必須環境変数
# にはせず、MAT_E2E_NODES（CSV）の先頭要素から導出する。brief は「MAT_E2E_NODE
# 等の必須環境変数」と一括りに書いており、discover/probe と diag の対象ノード
# 群は本来同じ commissioned ノード集合を指すため、要求する必須変数を1つに絞り
# 「self-contained で簡潔に」という指示に沿わせた。
#
# 必須 env: MAT_E2E_HOST（ssh 先。repo は public のため既定値を置かない）
#           MAT_E2E_NODES（commissioned node_id のカンマ区切り、例 "5,7,8"。
#             検証1 の reachable 判定と検証3 の対象ノード（先頭要素）に使う。
#             repo は public のため既定値を置かない）
# 任意 env: MAT_E2E_IFACE（既定 eth0）
#           MAT_E2E_FABRIC_INDEX（既定 2、jarvis 本番。discover/probe/diag の
#             native mDNS browse 自体はエンジン（fabric/NOC/KVS）を構築しない
#             ため実際には未使用だが、CLI のグローバル引数として無害に渡す）
#           MAT_E2E_STORE（既定: バイナリ自身のデフォルト解決 = ~/.config/mat
#             相当。指定時のみ --store を渡す）
#           MAT_E2E_CHIP_TOOL_BIN（検証4・5 で使う実 chip-tool のパス。
#             未指定なら ssh 先 PATH 任せ）
#           MAT_E2E_AVAHI_BROWSE_BIN（検証4・5 で使う実 avahi-browse のパス。
#             未指定なら ssh 先 PATH 任せ）
# ローカル要件: jq（出力の JSON 比較・抽出に使う。ローカル側で実行する。
#   ssh 先（jarvis）には不要）
set -euo pipefail
cd "$(dirname "$0")/.."

: "${MAT_E2E_HOST:?MAT_E2E_HOST (ssh host) required}"
: "${MAT_E2E_NODES:?MAT_E2E_NODES (comma-separated commissioned node ids, e.g. \"5,7,8\") required}"
command -v jq >/dev/null 2>&1 || { echo "jq が必要です（discover/diag の JSON 比較・抽出に使用）" >&2; exit 1; }

IFACE="${MAT_E2E_IFACE:-eth0}"
FABRIC_INDEX="${MAT_E2E_FABRIC_INDEX:-2}"
STORE="${MAT_E2E_STORE:-}"
CHIP_TOOL_BIN="${MAT_E2E_CHIP_TOOL_BIN:-}"
AVAHI_BROWSE_BIN="${MAT_E2E_AVAHI_BROWSE_BIN:-}"
IFS=',' read -r -a NODE_ARR <<< "$MAT_E2E_NODES"
NODE="${NODE_ARR[0]}"
TARGET=aarch64-unknown-linux-musl

confirm() {
  # $1 = 目視確認を促す文面
  echo ""
  echo ">>> $1"
  read -r -p ">>> 確認できたら Enter で続行 (Ctrl-C で中断): " _
}

echo "== 1/6 クロスビルド (mat, $TARGET, rust-lld) — matd は不要（discover/diag は常に直経路）"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
export RUSTFLAGS="-C linker-flavor=ld.lld -C link-self-contained=yes"
cargo build --release --target "$TARGET" -p mat
MAT_BIN="target/$TARGET/release/mat"
file "$MAT_BIN" | grep -q 'aarch64' || { echo "stale/wrong-arch binary: $MAT_BIN"; exit 1; }
echo "mat: $MAT_BIN"

# 直近呼び出しの stderr（ローカル一時ファイル、呼び出しのたびに上書きされる）。
LAST_STDERR_FILE=$(mktemp)

cleanup() {
  echo "== cleanup: ssh 先の一時バイナリ削除 ($MAT_E2E_HOST) =="
  ssh "$MAT_E2E_HOST" "rm -f /tmp/mat-m8b" || true
  rm -f "$LAST_STDERR_FILE"
}
trap cleanup EXIT

echo "== 2/6 転送 → $MAT_E2E_HOST"
# ssh cat 方式（scp は ssh-agent の状態に左右される、既存 e2e-*-real.sh に倣う）。
# 別名 (/tmp/mat-m8b) で置き、本番 /usr/local/bin/mat とは衝突させない。
ssh "$MAT_E2E_HOST" 'cat > /tmp/mat-m8b && chmod +x /tmp/mat-m8b' < "$MAT_BIN"

STORE_ARG=()
[ -n "$STORE" ] && STORE_ARG=(--store "$STORE")

# ---- runner 群 ----
# 共通: MAT_MATD=0（belt-and-braces、上のコメント参照）+ MAT_LOG=info（positive
# marker を info レベルで確実に出す）。stdout は関数の標準出力（呼び出し側で
# $() 捕捉）、stderr はローカル一時ファイルへ。

# 検証1: native discover+probe の純 native 実証。chip-tool・avahi-browse とも
# 存在しないパスに向ける。
run_native_full() {
  ssh "$MAT_E2E_HOST" \
    MAT_MATD=0 MAT_LOG=info "MAT_IFACE=$IFACE" "MAT_FABRIC_INDEX=$FABRIC_INDEX" \
    MAT_CHIP_TOOL_BIN=/nonexistent/mat-e2e-m8b-chip-tool \
    MAT_AVAHI_BROWSE_BIN=/nonexistent/mat-e2e-m8b-avahi-browse \
    /tmp/mat-m8b "$@" 2>"$LAST_STDERR_FILE"
}

# 検証3: diag node --deep の native mDNS 実証。avahi-browse のみ無効化
# （chip-tool は実バイナリ — 上の judgment 参照）。
run_native_diag() {
  local envs=(MAT_MATD=0 MAT_LOG=info "MAT_IFACE=$IFACE" "MAT_FABRIC_INDEX=$FABRIC_INDEX" \
    MAT_AVAHI_BROWSE_BIN=/nonexistent/mat-e2e-m8b-avahi-browse)
  [ -n "$CHIP_TOOL_BIN" ] && envs+=("MAT_CHIP_TOOL_BIN=$CHIP_TOOL_BIN")
  ssh "$MAT_E2E_HOST" "${envs[@]}" /tmp/mat-m8b "$@" 2>"$LAST_STDERR_FILE"
}

# 検証4: MAT_IFACE 未設定、実 chip-tool + 実 avahi-browse（比較対象）。
run_chip() {
  local envs=(MAT_MATD=0 MAT_LOG=info)
  [ -n "$CHIP_TOOL_BIN" ] && envs+=("MAT_CHIP_TOOL_BIN=$CHIP_TOOL_BIN")
  [ -n "$AVAHI_BROWSE_BIN" ] && envs+=("MAT_AVAHI_BROWSE_BIN=$AVAHI_BROWSE_BIN")
  ssh "$MAT_E2E_HOST" "${envs[@]}" /tmp/mat-m8b "$@" 2>"$LAST_STDERR_FILE"
}

# 検証5: 存在しない iface 名 → native browse が IO エラーで即 fallback。
run_fallback() {
  local envs=(MAT_MATD=0 MAT_LOG=info MAT_IFACE=mat-e2e-bogus-iface)
  [ -n "$CHIP_TOOL_BIN" ] && envs+=("MAT_CHIP_TOOL_BIN=$CHIP_TOOL_BIN")
  [ -n "$AVAHI_BROWSE_BIN" ] && envs+=("MAT_AVAHI_BROWSE_BIN=$AVAHI_BROWSE_BIN")
  ssh "$MAT_E2E_HOST" "${envs[@]}" /tmp/mat-m8b "$@" 2>"$LAST_STDERR_FILE"
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

# positive 実証: probe.rs::native / discover.rs::native_commissionables が
# 成功時に出す "... executed (native browse)" を直接 grep する
# （assert_no_fallback だけでは、warn ゼロのまま静かに fallback するような
# 将来的な回帰を検出できないための二重チェック。M8a Task11 の教訓を踏襲）。
# $1 = grep パターン, $2 = 説明（省略可）
assert_native_marker() {
  if ! grep -q -- "$1" "$LAST_STDERR_FILE"; then
    echo "FAIL: ${2:-op} — stderr に native 実行の positive marker '$1' が無い（native で走った実証なし）:" >&2
    cat "$LAST_STDERR_FILE" >&2
    exit 1
  fi
}

echo "== 3/6 検証1: native discover + probe（外部バイナリ無効化、node群=$MAT_E2E_NODES）"
NATIVE_OUT=$(run_native_full "${STORE_ARG[@]}" discover --probe)
echo "$NATIVE_OUT"
assert_no_fallback "discover --probe (native, 外部バイナリ無効化)"
assert_native_marker "discover executed (native browse)" "discover --probe (native)"
assert_native_marker "probe executed (native browse)" "discover --probe (native)"

for n in "${NODE_ARR[@]}"; do
  ENTRY=$(printf '%s' "$NATIVE_OUT" | jq -c --argjson n "$n" \
    '.devices[] | select(.state=="commissioned" and .node_id==$n)')
  if [ -z "$ENTRY" ]; then
    echo "FAIL: node $n が commissioned として discover --probe 出力に無い" >&2
    printf '%s\n' "$NATIVE_OUT" >&2
    exit 1
  fi
  REACHABLE=$(printf '%s' "$ENTRY" | jq -r '.reachable')
  ADDRESS=$(printf '%s' "$ENTRY" | jq -r '.address')
  if [ "$REACHABLE" != "true" ]; then
    echo "FAIL: node $n の reachable が true でない (got $REACHABLE)" >&2
    printf '%s\n' "$ENTRY" >&2
    exit 1
  fi
  if [ "$ADDRESS" = "null" ] || [ -z "$ADDRESS" ]; then
    echo "FAIL: node $n の address が null" >&2
    printf '%s\n' "$ENTRY" >&2
    exit 1
  fi
done
echo "PASS: 検証1 native discover+probe（全ノード reachable:true+address非null、marker2件、fallback不在）" >&2

echo "== 4/6 検証2: commissionable 検出（玄関ライト等）"
COMMISSIONABLE_COUNT=$(printf '%s' "$NATIVE_OUT" | \
  jq '[.devices[] | select(.state=="commissionable")] | length')
if [ "$COMMISSIONABLE_COUNT" -eq 0 ]; then
  echo "WARN: commissionable なデバイスが0件（玄関ライトが commissioning 広告を止めている可能性）" >&2
  confirm "玄関ライト（fabric無し）が現在 commissioning 広告を出していない状況で問題ないか確認してください"
else
  HAS_DISC=$(printf '%s' "$NATIVE_OUT" | jq \
    '[.devices[] | select(.state=="commissionable" and has("discriminator"))] | length > 0')
  if [ "$HAS_DISC" != "true" ]; then
    echo "FAIL: commissionable エントリに discriminator を持つものが1件も無い" >&2
    exit 1
  fi
  echo "PASS: 検証2 commissionable 検出 ($COMMISSIONABLE_COUNT 件, discriminator 付き含む)" >&2
fi

echo "== 5/6 検証3: diag node --deep native（avahi のみ無効化、node=$NODE）"
DIAG_OUT=$(run_native_diag "${STORE_ARG[@]}" diag node --node "$NODE" --deep)
echo "$DIAG_OUT"
if grep -q "falling back to avahi-browse" "$LAST_STDERR_FILE"; then
  echo "FAIL: diag node --deep で avahi-browse へフォールバックしている（native mDNS が動いていない）" >&2
  cat "$LAST_STDERR_FILE" >&2
  exit 1
fi
assert_native_marker "probe executed (native browse)" "diag node --deep (native mdns)"

ANY_FABRIC=$(printf '%s' "$DIAG_OUT" | jq -r '.checks.mdns.advertised_any_fabric')
SELF_FABRIC=$(printf '%s' "$DIAG_OUT" | jq -r '.checks.mdns.advertised_self_fabric')
if [ "$ANY_FABRIC" != "true" ]; then
  echo "FAIL: checks.mdns.advertised_any_fabric が true でない (got $ANY_FABRIC)" >&2
  printf '%s\n' "$DIAG_OUT" >&2
  exit 1
fi
if [ "$SELF_FABRIC" != "true" ]; then
  echo "FAIL: checks.mdns.advertised_self_fabric が true でない (got $SELF_FABRIC) — jarvis は自 fabric 広告ありのはず" >&2
  printf '%s\n' "$DIAG_OUT" >&2
  exit 1
fi
echo "PASS: 検証3 diag node --deep native（advertised_any_fabric/advertised_self_fabric = true）" >&2

echo "== 6/6 検証4・5: chip-tool 経路との構造一致 + フォールバック健全性"

echo "-- 検証4: MAT_IFACE 無し（実 chip-tool + 実 avahi-browse）"
CHIP_OUT=$(run_chip "${STORE_ARG[@]}" discover --probe)
echo "$CHIP_OUT"

NATIVE_KEYS_UNION=$(printf '%s' "$NATIVE_OUT" | \
  jq -S '[.devices[] | select(.state=="commissioned") | keys] | unique')
CHIP_KEYS_UNION=$(printf '%s' "$CHIP_OUT" | \
  jq -S '[.devices[] | select(.state=="commissioned") | keys] | unique')
if [ "$NATIVE_KEYS_UNION" != "$CHIP_KEYS_UNION" ]; then
  echo "FAIL: commissioned エントリのキー集合が native/chip-tool で一致しない" >&2
  echo "native:    $NATIVE_KEYS_UNION" >&2
  echo "chip-tool: $CHIP_KEYS_UNION" >&2
  exit 1
fi

NATIVE_REACHABLE_NODES=$(printf '%s' "$NATIVE_OUT" | \
  jq -S '[.devices[] | select(.state=="commissioned" and .reachable==true) | .node_id] | sort')
CHIP_REACHABLE_NODES=$(printf '%s' "$CHIP_OUT" | \
  jq -S '[.devices[] | select(.state=="commissioned" and .reachable==true) | .node_id] | sort')
if [ "$NATIVE_REACHABLE_NODES" != "$CHIP_REACHABLE_NODES" ]; then
  echo "FAIL: reachable:true の node 集合が native/chip-tool で一致しない（commissionable 件数差は許容だが、これは許容しない）" >&2
  echo "native:    $NATIVE_REACHABLE_NODES" >&2
  echo "chip-tool: $CHIP_REACHABLE_NODES" >&2
  exit 1
fi
echo "PASS: 検証4 構造一致（commissioned キー集合一致、reachable:true node集合一致: $NATIVE_REACHABLE_NODES）" >&2

echo "-- 検証5: フォールバック健全性（MAT_IFACE=mat-e2e-bogus-iface、実 chip-tool + 実 avahi-browse）"
FALLBACK_OUT=$(run_fallback "${STORE_ARG[@]}" discover --probe)
echo "$FALLBACK_OUT"
FALLBACK_STDERR=$(cat "$LAST_STDERR_FILE")
assert_grep "falling back to chip-tool" "$FALLBACK_STDERR" \
  "フォールバック健全性: stderr に 'falling back to chip-tool' が無い"
assert_grep "falling back to avahi-browse" "$FALLBACK_STDERR" \
  "フォールバック健全性: stderr に 'falling back to avahi-browse' が無い"

FALLBACK_KEYS_UNION=$(printf '%s' "$FALLBACK_OUT" | \
  jq -S '[.devices[] | select(.state=="commissioned") | keys] | unique')
if [ "$FALLBACK_KEYS_UNION" != "$CHIP_KEYS_UNION" ]; then
  echo "FAIL: フォールバック経路の commissioned キー集合が chip-tool 経路（検証4）と一致しない" >&2
  echo "fallback:  $FALLBACK_KEYS_UNION" >&2
  echo "chip-tool: $CHIP_KEYS_UNION" >&2
  exit 1
fi
echo "PASS: 検証5 フォールバック健全性（'falling back to chip-tool'+'falling back to avahi-browse' 両方stderrに出現、出力構造がchip-tool経路と一致）" >&2

echo "== e2e:m8b:real PASS（検証1〜5 全項目 GREEN）"
