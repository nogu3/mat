#!/usr/bin/env bash
# Phase 5 M6a 受け入れ: 実機（本番 fabric）に対する native commissioning。
# 本番 fabric の Nanoleaf に native open-window → 使い捨て第二 fabric へ
# native commission（実 _matterc browse + 本物 DAC の厳格 attestation）→
# 制御 → RemoveFabric 撤収 → 本番 fabric 無傷を確認。
# aarch64-musl クロスビルド → 転送 → コントローラ実機上で実行（KVS・PAA
# ストア・対象デバイスは実機側にあるため）。
# 必須 env: MAT_E2E_HOST（ssh 先。repo は public のため既定値を置かない）
#           MAT_E2E_NODE_ID（対象 device node id。同上）
# 任意 env: MAT_E2E_KVS_DIR（既定 ~/.config/mat）
#           MAT_E2E_PAA_DIR（既定 ~/.config/mat/paa-trust-store。本番 PAA ストア）
#           MAT_E2E_IFACE（既定: リモートの default route の iface。jarvis は eth0）
#           MAT_E2E_FABRIC_INDEX（既定 1 — ダミー。jarvis の実測値は 2、
#             呼び出し時に明示指定すること）
#           MAT_E2E_ISSUER_INDEX（既定 0）
set -euo pipefail
cd "$(dirname "$0")/.."
: "${MAT_E2E_HOST:?MAT_E2E_HOST (ssh host) required}"
: "${MAT_E2E_NODE_ID:?MAT_E2E_NODE_ID (device node id) required}"

echo "== 1/3 クロスビルド (aarch64-unknown-linux-musl, rust-lld)"
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=rust-lld
export RUSTFLAGS="-C linker-flavor=ld.lld -C link-self-contained=yes"
cargo test -p mat-controller --test live_commission_real --release \
  --target aarch64-unknown-linux-musl --no-run
BIN=$(ls -t target/aarch64-unknown-linux-musl/release/deps/live_commission_real-* \
  | grep -v '\.d$' | head -1)
file "$BIN" | grep -q 'aarch64' || { echo "stale/wrong-arch binary: $BIN"; exit 1; }
echo "binary: $BIN"

echo "== 2/3 転送 → $MAT_E2E_HOST"
# scp は ssh-agent の状態に左右されるため、確実な ssh cat 方式で送る
ssh "$MAT_E2E_HOST" 'cat > /tmp/live_commission_real && chmod +x /tmp/live_commission_real' < "$BIN"

echo "== 3/3 実機で実行（本番 fabric・本番 matd に影響なし）"
ssh "$MAT_E2E_HOST" \
  MAT_E2E_NODE_ID="$MAT_E2E_NODE_ID" \
  MAT_E2E_FABRIC_INDEX="${MAT_E2E_FABRIC_INDEX:-1}" \
  MAT_E2E_ISSUER_INDEX="${MAT_E2E_ISSUER_INDEX:-0}" \
  MAT_E2E_KVS_DIR="${MAT_E2E_KVS_DIR:-}" \
  MAT_E2E_PAA_DIR="${MAT_E2E_PAA_DIR:-}" \
  MAT_E2E_IFACE="${MAT_E2E_IFACE:-}" \
  'bash -s' <<'EOF'
set -euo pipefail
[ -n "${MAT_E2E_KVS_DIR}" ] || MAT_E2E_KVS_DIR="$HOME/.config/mat"
[ -n "${MAT_E2E_PAA_DIR}" ] || MAT_E2E_PAA_DIR="$HOME/.config/mat/paa-trust-store"
if [ -z "${MAT_E2E_IFACE}" ]; then
  MAT_E2E_IFACE=$(ip route show default | sed -n 's/.* dev \([^ ]*\).*/\1/p' | head -1)
  echo "auto-detected iface: ${MAT_E2E_IFACE}"
fi
export MAT_E2E_KVS_DIR MAT_E2E_PAA_DIR MAT_E2E_IFACE
exec /tmp/live_commission_real --ignored --nocapture
EOF

echo "== e2e:m6:real PASS"
