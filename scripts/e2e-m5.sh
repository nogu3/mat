#!/usr/bin/env bash
# [M8c-3] chip-tool 撤去済みのため 0.22.0 以降では動かない（歴史的アーカイブ。
# 動かすなら git tag の 0.21.0 時点を checkout）。現行ハーネスは e2e-m8c3-real.sh。
# Phase 5 M5 受け入れ: native 有効な matd を jarvis で（本番 matd とは別の socket/ws
# ポートで）起動し、unix socket 越しに groupcast の N/N 配達（off/on/color-temp）と
# matd 再起動後の jump-ahead 配達を検証する。本番 systemd matd（chip-tool 版,
# port 9100, 既定 socket）には触れない。
#
# 警告: 実行中は本番 matd / 直 chip-tool から同じグループへ group 送信をしないこと
# （group send counter 混在の実機知見 — 以後不達になり matd 再起動でしか回復しない）。
# unicast コマンドは併用してよい。
#
# 必須 env: MAT_E2E_HOST（ssh 先。repo は public のため既定値を置かない）
#           MAT_E2E_IFACE（native warm session が使う Thread mesh iface 名）
#           MAT_E2E_GROUP_NODES（対象グループのメンバー node id、カンマ区切り。
#             repo は public のため既定値を置かない）
# 任意 env: MAT_E2E_GROUP_ID（既定 10）/ MAT_E2E_ENDPOINT（既定 1）
#           MAT_E2E_FABRIC_INDEX（既定 2、jarvis 本番）
#           MAT_E2E_STORE（既定: matd 自身のデフォルト解決 = ~/.config/mat 相当。
#             指定時のみ --store を渡す）
#           MAT_E2E_SOCKET（既定 /tmp/matd-m5.sock）
#           MAT_E2E_CHIP_TOOL_BIN（chip-tool フォールバックが使うパス。
#             未指定なら ssh 先 PATH 任せ — jarvis の運用に倣う）
set -euo pipefail
cd "$(dirname "$0")/.."

: "${MAT_E2E_HOST:?MAT_E2E_HOST (ssh host) required}"
: "${MAT_E2E_IFACE:?MAT_E2E_IFACE (thread mesh iface on the host) required}"
: "${MAT_E2E_GROUP_NODES:?MAT_E2E_GROUP_NODES (csv node ids) required}"
GROUP_ID="${MAT_E2E_GROUP_ID:-10}"
ENDPOINT="${MAT_E2E_ENDPOINT:-1}"
FABRIC_INDEX="${MAT_E2E_FABRIC_INDEX:-2}"
SOCKET="${MAT_E2E_SOCKET:-/tmp/matd-m5.sock}"
STORE="${MAT_E2E_STORE:-}"
CHIP_TOOL_BIN="${MAT_E2E_CHIP_TOOL_BIN:-}"
TARGET=aarch64-unknown-linux-musl

echo "== 1/5 クロスビルド (matd + live test, $TARGET, rust-lld)"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
export RUSTFLAGS="-C linker-flavor=ld.lld -C link-self-contained=yes"
cargo build --release --target "$TARGET" -p matd
MATD_BIN="target/$TARGET/release/matd"
file "$MATD_BIN" | grep -q 'aarch64' || { echo "stale/wrong-arch binary: $MATD_BIN"; exit 1; }

cargo test -p mat-controller --test live_matd_group --release \
  --target "$TARGET" --no-run
TESTBIN=$(ls -t "target/$TARGET/release/deps/live_matd_group-"* \
  | grep -v '\.d$' | head -1)
file "$TESTBIN" | grep -q 'aarch64' || { echo "stale/wrong-arch binary: $TESTBIN"; exit 1; }
echo "matd: $MATD_BIN"
echo "live test: $TESTBIN"

echo "== 2/5 転送 → $MAT_E2E_HOST"
# scp は ssh-agent の状態に左右されるため、e2e-m4 に倣い確実な ssh cat 方式で送る。
# 別名 (/tmp/matd-m5, /tmp/live_matd_group) で置き、本番 /tmp/matd 等とは衝突させない。
ssh "$MAT_E2E_HOST" 'cat > /tmp/matd-m5 && chmod +x /tmp/matd-m5' < "$MATD_BIN"
ssh "$MAT_E2E_HOST" 'cat > /tmp/live_matd_group && chmod +x /tmp/live_matd_group' < "$TESTBIN"

start_matd() {
  echo "== native matd を起動 (socket $SOCKET, ws port 9111 — 本番 9100/既定 socket とは別)"
  # MAT_MATD_IFACE は matd の --iface と同じ env（main.rs 参照）。fabric-index は
  # jarvis 本番なら 2。--store は未指定なら渡さず、matd 自身のデフォルト解決
  # （MAT_STORE / XDG_CONFIG_HOME / ~/.config/mat）に任せる。
  # nohup + disown で ssh セッション終了後も生き続けさせる（このテストの間だけ）。
  ssh "$MAT_E2E_HOST" \
    MAT_E2E_SOCKET="$SOCKET" \
    MAT_E2E_IFACE="$MAT_E2E_IFACE" \
    MAT_E2E_FABRIC_INDEX="$FABRIC_INDEX" \
    MAT_E2E_STORE="$STORE" \
    MAT_E2E_CHIP_TOOL_BIN="$CHIP_TOOL_BIN" \
    'bash -s' <<'EOF'
set -euo pipefail
rm -f "$MAT_E2E_SOCKET"
ARGS=(--socket "$MAT_E2E_SOCKET" --port 9111 --fabric-index "$MAT_E2E_FABRIC_INDEX")
[ -n "$MAT_E2E_STORE" ] && ARGS+=(--store "$MAT_E2E_STORE")
export MAT_MATD_IFACE="$MAT_E2E_IFACE"
[ -n "$MAT_E2E_CHIP_TOOL_BIN" ] && export MAT_CHIP_TOOL_BIN="$MAT_E2E_CHIP_TOOL_BIN"
export RUST_LOG=info
# 追記（>>）: restart 時の再呼び出しでログを消さない（roundtrip フェーズの
# counter 基準値を残し、末尾の jump-ahead 比較で使う）。初回の空ログ化は
# 呼び出し側（最初の start_matd 呼び出し前）で rm -f 済み。
nohup /tmp/matd-m5 "${ARGS[@]}" >>/tmp/matd-m5.log 2>&1 &
echo $! > /tmp/matd-m5.pid
disown
EOF

  # matd の socket が bind されるまで待つ（起動失敗を早期検出）。
  for i in $(seq 1 20); do
    ssh "$MAT_E2E_HOST" "test -S '$SOCKET'" && break
    ssh "$MAT_E2E_HOST" "kill -0 \$(cat /tmp/matd-m5.pid 2>/dev/null) 2>/dev/null" \
      || { echo "matd 起動失敗"; ssh "$MAT_E2E_HOST" 'tail -n 60 /tmp/matd-m5.log' || true; exit 1; }
    sleep 0.5
  done
}

cleanup() {
  echo "== cleanup: stopping native matd on $MAT_E2E_HOST =="
  ssh "$MAT_E2E_HOST" \
    'kill "$(cat /tmp/matd-m5.pid 2>/dev/null)" 2>/dev/null; rm -f /tmp/matd-m5.pid '"$SOCKET"'' \
    || true
}
trap cleanup EXIT

echo "== 3/5 native matd を起動"
# 初回だけログを空にする（start_matd 自体は >> 追記なので、restart を跨いで
# roundtrip フェーズの counter 基準値が残る）。
ssh "$MAT_E2E_HOST" 'rm -f /tmp/matd-m5.log'
start_matd

echo "== 4/5 ライブテスト (roundtrip)"
# 注意: MAT_E2E_GROUP_NODES は ssh 経由でそのまま argv に渡るため、csv にスペースを
# 含めないこと（ssh はリモートコマンドをスペース区切りで再結合する）。
ssh "$MAT_E2E_HOST" \
  MAT_E2E_SOCKET="$SOCKET" \
  MAT_E2E_GROUP_ID="$GROUP_ID" \
  MAT_E2E_GROUP_NODES="$MAT_E2E_GROUP_NODES" \
  MAT_E2E_ENDPOINT="$ENDPOINT" \
  'exec /tmp/live_matd_group --ignored --nocapture matd_group_roundtrip'

# 最終レビュー指摘: native が無効（フォールバック先の chip-tool のみ）でも
# roundtrip/after_restart テスト自体は全 op が chip-tool 経由で "sent" を返し
# 通ってしまう — silent full fallback を検出できない。restart 前後の
# ログ行数を記録しておき、末尾で「native 送信ログが restart 前後の両方に
# 実在するか」と「counter が単調増加か」を実アサーションにする。
PRE_RESTART_LINES=$(ssh "$MAT_E2E_HOST" 'wc -l < /tmp/matd-m5.log' | tr -d '[:space:]')
PRE_RESTART_LINES="${PRE_RESTART_LINES:-0}"

echo "== 5/5 matd 再起動 → jump-ahead 配達検証"
ssh "$MAT_E2E_HOST" 'kill "$(cat /tmp/matd-m5.pid)" 2>/dev/null; sleep 1' || true
start_matd
ssh "$MAT_E2E_HOST" \
  MAT_E2E_SOCKET="$SOCKET" \
  MAT_E2E_GROUP_ID="$GROUP_ID" \
  MAT_E2E_GROUP_NODES="$MAT_E2E_GROUP_NODES" \
  MAT_E2E_ENDPOINT="$ENDPOINT" \
  'exec /tmp/live_matd_group --ignored --nocapture matd_group_after_restart'

echo "== counter 履歴（native groupcast 送信ログ; ログ形式は"
echo "   tracing::info!(group_id, counter, \"groupcast sent (native)\") の rendering）"
FULL_LOG=$(ssh "$MAT_E2E_HOST" 'cat /tmp/matd-m5.log')
printf '%s\n' "$FULL_LOG" | grep "groupcast sent" || true   # 目視確認用の人間可読出力

BEFORE_COUNT=$(printf '%s\n' "$FULL_LOG" | head -n "$PRE_RESTART_LINES" \
  | grep -c "groupcast sent (native)" || true)
AFTER_COUNT=$(printf '%s\n' "$FULL_LOG" | tail -n "+$((PRE_RESTART_LINES + 1))" \
  | grep -c "groupcast sent (native)" || true)

if [ "${BEFORE_COUNT:-0}" -lt 1 ]; then
  echo "FAIL: no 'groupcast sent (native)' log line found BEFORE the restart." >&2
  echo "      native groupcast may be disabled; this run would have silently" >&2
  echo "      gone through chip-tool for every op and still printed PASS without" >&2
  echo "      this check." >&2
  exit 1
fi
if [ "${AFTER_COUNT:-0}" -lt 1 ]; then
  echo "FAIL: no 'groupcast sent (native)' log line found AFTER the restart." >&2
  echo "      jump-ahead re-init or native itself may be broken post-restart." >&2
  exit 1
fi

COUNTERS=$(printf '%s\n' "$FULL_LOG" \
  | grep "groupcast sent (native)" \
  | sed -nE 's/.*counter=([0-9]+).*/\1/p')
PREV=""
while IFS= read -r c; do
  [ -z "$c" ] && continue
  if [ -n "$PREV" ] && [ "$c" -le "$PREV" ]; then
    echo "FAIL: groupcast counter not strictly increasing across the run" \
      "($PREV -> $c) — possible counter reuse/desync." >&2
    exit 1
  fi
  PREV="$c"
done <<EOF
$COUNTERS
EOF

echo "counters observed (chronological, native sends only, before+after restart):"
printf '%s' "$COUNTERS" | tr '\n' ' '
echo
echo "== e2e:m5 PASS"
