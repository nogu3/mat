#!/usr/bin/env bash
# [M8c-3] chip-tool 撤去済みのため 0.22.0 以降では動かない（歴史的アーカイブ。
# 動かすなら git tag の 0.21.0 時点を checkout）。現行ハーネスは e2e-m8c3-real.sh。
# Phase 5 M3 受け入れ: jarvis 相乗り live E2E。aarch64-musl クロスビルド →
# 転送 → コントローラ実機上で実行（KVS とデバイスは実機側にあるため）。
# 必須 env: MAT_E2E_HOST（ssh 先。repo は public のため既定値を置かない）
#           MAT_E2E_NODE_ID（対象 device node id。同上）
# 任意 env: MAT_E2E_KVS_DIR（既定 ~/.config/mat）
#           MAT_E2E_IFACE（既定: リモートの default route の iface）
#           MAT_E2E_FABRIC_INDEX（既定 1）/ MAT_E2E_ENDPOINT（既定 1）
#           MAT_E2E_ISSUER_INDEX（既定 0）/ MAT_E2E_PEER（mDNS バイパス）
set -euo pipefail
cd "$(dirname "$0")/.."
: "${MAT_E2E_HOST:?MAT_E2E_HOST (ssh host) required}"
: "${MAT_E2E_NODE_ID:?MAT_E2E_NODE_ID (device node id) required}"

echo "== 1/3 クロスビルド (aarch64-unknown-linux-musl, rust-lld)"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
export RUSTFLAGS="-C linker-flavor=ld.lld -C link-self-contained=yes"
cargo test -p mat-controller --test live_jarvis --release \
  --target aarch64-unknown-linux-musl --no-run
BIN=$(ls -t target/aarch64-unknown-linux-musl/release/deps/live_jarvis-* \
  | grep -v '\.d$' | head -1)
file "$BIN" | grep -q 'aarch64' || { echo "stale/wrong-arch binary: $BIN"; exit 1; }
echo "binary: $BIN"

echo "== 2/3 転送 → $MAT_E2E_HOST"
# scp は ssh-agent の状態に左右されるため、確実な ssh cat 方式で送る
ssh "$MAT_E2E_HOST" 'cat > /tmp/live_jarvis && chmod +x /tmp/live_jarvis' < "$BIN"

echo "== 3/3 実機で実行"
ssh "$MAT_E2E_HOST" \
  MAT_E2E_NODE_ID="$MAT_E2E_NODE_ID" \
  MAT_E2E_FABRIC_INDEX="${MAT_E2E_FABRIC_INDEX:-1}" \
  MAT_E2E_ENDPOINT="${MAT_E2E_ENDPOINT:-1}" \
  MAT_E2E_ISSUER_INDEX="${MAT_E2E_ISSUER_INDEX:-0}" \
  MAT_E2E_KVS_DIR="${MAT_E2E_KVS_DIR:-}" \
  MAT_E2E_IFACE="${MAT_E2E_IFACE:-}" \
  MAT_E2E_PEER="${MAT_E2E_PEER:-}" \
  'bash -s' <<'EOF'
set -euo pipefail
[ -n "${MAT_E2E_KVS_DIR}" ] || MAT_E2E_KVS_DIR="$HOME/.config/mat"
if [ -z "${MAT_E2E_IFACE}" ] && [ -z "${MAT_E2E_PEER}" ]; then
  MAT_E2E_IFACE=$(ip route show default | sed -n 's/.* dev \([^ ]*\).*/\1/p' | head -1)
  echo "auto-detected iface: ${MAT_E2E_IFACE}"
fi
export MAT_E2E_KVS_DIR
[ -n "${MAT_E2E_IFACE}" ] && export MAT_E2E_IFACE || unset MAT_E2E_IFACE
[ -n "${MAT_E2E_PEER}" ] && export MAT_E2E_PEER || unset MAT_E2E_PEER
exec /tmp/live_jarvis --ignored --nocapture
EOF

echo "== e2e:m3 PASS"
