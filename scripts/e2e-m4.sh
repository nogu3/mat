#!/usr/bin/env bash
# [M8c-3] chip-tool 撤去済みのため 0.22.0 以降では動かない（歴史的アーカイブ。
# 動かすなら git tag の 0.21.0 時点を checkout）。現行ハーネスは e2e-m8c3-real.sh。
# Phase 5 M4 受け入れ: native 有効な matd を jarvis で（本番 matd とは別の socket/ws
# ポートで）起動し、unix socket 越しにホットパス往復 / warm 再利用 / describe の
# chip-tool フォールバックを検証する。本番 systemd matd（chip-tool 版, port 9100,
# 既定 socket）には触れない。
# 必須 env: MAT_E2E_HOST（ssh 先。repo は public のため既定値を置かない）
#           MAT_E2E_NODE_ID（対象 device node id。同上）
#           MAT_E2E_IFACE（native warm session が使う Thread mesh iface 名）
# 任意 env: MAT_E2E_ENDPOINT（既定 1）/ MAT_E2E_FABRIC_INDEX（既定 2、jarvis 本番）
#           MAT_E2E_STORE（既定: matd 自身のデフォルト解決 = ~/.config/mat 相当。
#             指定時のみ --store を渡す）
#           MAT_E2E_SOCKET（既定 /tmp/matd-m4.sock）
#           MAT_E2E_CHIP_TOOL_BIN（describe フォールバックが使う chip-tool のパス。
#             未指定なら ssh 先 PATH 任せ — jarvis の運用に倣う）
set -euo pipefail
cd "$(dirname "$0")/.."

: "${MAT_E2E_HOST:?MAT_E2E_HOST (ssh host) required}"
: "${MAT_E2E_NODE_ID:?MAT_E2E_NODE_ID (device node id) required}"
: "${MAT_E2E_IFACE:?MAT_E2E_IFACE (thread mesh iface on the host) required}"
ENDPOINT="${MAT_E2E_ENDPOINT:-1}"
FABRIC_INDEX="${MAT_E2E_FABRIC_INDEX:-2}"
SOCKET="${MAT_E2E_SOCKET:-/tmp/matd-m4.sock}"
STORE="${MAT_E2E_STORE:-}"
CHIP_TOOL_BIN="${MAT_E2E_CHIP_TOOL_BIN:-}"
TARGET=aarch64-unknown-linux-musl

echo "== 1/4 クロスビルド (matd + live test, $TARGET, rust-lld)"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
export RUSTFLAGS="-C linker-flavor=ld.lld -C link-self-contained=yes"
cargo build --release --target "$TARGET" -p matd
MATD_BIN="target/$TARGET/release/matd"
file "$MATD_BIN" | grep -q 'aarch64' || { echo "stale/wrong-arch binary: $MATD_BIN"; exit 1; }

cargo test -p mat-controller --test live_matd_native --release \
  --target "$TARGET" --no-run
TESTBIN=$(ls -t "target/$TARGET/release/deps/live_matd_native-"* \
  | grep -v '\.d$' | head -1)
file "$TESTBIN" | grep -q 'aarch64' || { echo "stale/wrong-arch binary: $TESTBIN"; exit 1; }
echo "matd: $MATD_BIN"
echo "live test: $TESTBIN"

echo "== 2/4 転送 → $MAT_E2E_HOST"
# scp は ssh-agent の状態に左右されるため、e2e-m3 に倣い確実な ssh cat 方式で送る。
# 別名 (/tmp/matd-m4, /tmp/live_matd_native) で置き、本番 /tmp/matd 等とは衝突させない。
ssh "$MAT_E2E_HOST" 'cat > /tmp/matd-m4 && chmod +x /tmp/matd-m4' < "$MATD_BIN"
ssh "$MAT_E2E_HOST" 'cat > /tmp/live_matd_native && chmod +x /tmp/live_matd_native' < "$TESTBIN"

echo "== 3/4 native matd を起動 (socket $SOCKET, ws port 9110 — 本番 9100/既定 socket とは別)"
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
ARGS=(--socket "$MAT_E2E_SOCKET" --port 9110 --fabric-index "$MAT_E2E_FABRIC_INDEX")
[ -n "$MAT_E2E_STORE" ] && ARGS+=(--store "$MAT_E2E_STORE")
export MAT_MATD_IFACE="$MAT_E2E_IFACE"
[ -n "$MAT_E2E_CHIP_TOOL_BIN" ] && export MAT_CHIP_TOOL_BIN="$MAT_E2E_CHIP_TOOL_BIN"
export RUST_LOG=info
nohup /tmp/matd-m4 "${ARGS[@]}" >/tmp/matd-m4.log 2>&1 &
echo $! > /tmp/matd-m4.pid
disown
EOF

cleanup() {
  echo "== cleanup: stopping native matd on $MAT_E2E_HOST =="
  ssh "$MAT_E2E_HOST" \
    'kill "$(cat /tmp/matd-m4.pid 2>/dev/null)" 2>/dev/null; rm -f /tmp/matd-m4.pid '"$SOCKET"'' \
    || true
}
trap cleanup EXIT

# matd の socket が bind されるまで待つ（起動失敗を早期検出）。
for i in $(seq 1 20); do
  ssh "$MAT_E2E_HOST" "test -S '$SOCKET'" && break
  ssh "$MAT_E2E_HOST" "kill -0 \$(cat /tmp/matd-m4.pid 2>/dev/null) 2>/dev/null" \
    || { echo "matd 起動失敗"; ssh "$MAT_E2E_HOST" 'tail -n 60 /tmp/matd-m4.log' || true; exit 1; }
  sleep 0.5
done

echo "== 4/4 ライブテスト実行"
ssh "$MAT_E2E_HOST" \
  MAT_E2E_SOCKET="$SOCKET" \
  MAT_E2E_NODE_ID="$MAT_E2E_NODE_ID" \
  MAT_E2E_ENDPOINT="$ENDPOINT" \
  'exec /tmp/live_matd_native --ignored --nocapture matd_native_hotpath_roundtrip'

echo "== e2e:m4 PASS"
