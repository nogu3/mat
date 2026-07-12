#!/usr/bin/env bash
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
nohup /tmp/matd-m5 "${ARGS[@]}" >/tmp/matd-m5.log 2>&1 &
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
start_matd

echo "== 4/5 ライブテスト (roundtrip)"
ssh "$MAT_E2E_HOST" \
  MAT_E2E_SOCKET="$SOCKET" \
  MAT_E2E_GROUP_ID="$GROUP_ID" \
  MAT_E2E_GROUP_NODES="$MAT_E2E_GROUP_NODES" \
  MAT_E2E_ENDPOINT="$ENDPOINT" \
  'exec /tmp/live_matd_group --ignored --nocapture matd_group_roundtrip'

echo "== 5/5 matd 再起動 → jump-ahead 配達検証"
ssh "$MAT_E2E_HOST" 'kill "$(cat /tmp/matd-m5.pid)" && sleep 1'
start_matd
ssh "$MAT_E2E_HOST" \
  MAT_E2E_SOCKET="$SOCKET" \
  MAT_E2E_GROUP_ID="$GROUP_ID" \
  MAT_E2E_GROUP_NODES="$MAT_E2E_GROUP_NODES" \
  MAT_E2E_ENDPOINT="$ENDPOINT" \
  'exec /tmp/live_matd_group --ignored --nocapture matd_group_after_restart'

echo "== counter 履歴（jump-ahead の目視確認用）"
ssh "$MAT_E2E_HOST" 'grep "groupcast sent" /tmp/matd-m5.log || true'
echo "== e2e:m5 PASS"
