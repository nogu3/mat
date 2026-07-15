#!/usr/bin/env bash
# Phase 5 M2b 受け入れ: 使い捨て fabric でコミッション → 自己発行 NOC で CASE + IM。
# 前提: ./chip-all-clusters-app と ./chip-tool（task chip:extract:app / chip:extract）。
set -euo pipefail
cd "$(dirname "$0")/.."
APP=${MAT_E2E_APP:-./chip-all-clusters-app}
CHIP_TOOL=${MAT_CHIP_TOOL_BIN:-./chip-tool}
NODE_ID=0x12344321
PASSCODE=20202021
[[ -x "$APP" ]] || { echo "error: $APP なし (task chip:extract:app)"; exit 1; }
[[ -x "$CHIP_TOOL" ]] || { echo "error: $CHIP_TOOL なし (task chip:extract)"; exit 1; }

WORK=$(mktemp -d)
APP_PID=""
cleanup() { [[ -n "$APP_PID" ]] && kill "$APP_PID" 2>/dev/null || true; rm -rf "$WORK"; }
trap cleanup EXIT

echo "== 1/3 app 起動 (KVS: $WORK/device_kvs)"
"$APP" --KVS "$WORK/device_kvs" >"$WORK/app.log" 2>&1 &
APP_PID=$!
for i in $(seq 1 40); do
  ss -uln 2>/dev/null | grep -q ':5540' && break
  kill -0 "$APP_PID" 2>/dev/null || { echo "app 起動失敗"; cat "$WORK/app.log"; exit 1; }
  sleep 0.25
done

echo "== 2/3 chip-tool でコミッション (device node $NODE_ID)"
"$CHIP_TOOL" pairing already-discovered "$NODE_ID" "$PASSCODE" ::1 5540 \
  --storage-directory "$WORK" >"$WORK/pairing.log" 2>&1 \
  || { echo "pairing 失敗"; tail -40 "$WORK/pairing.log"; exit 1; }
grep -qi "commissioning completed with success" "$WORK/pairing.log" \
  || { echo "コミッション成功ログ無し"; tail -40 "$WORK/pairing.log"; exit 1; }

echo "== 3/3 self-issued CASE + IM ライブテスト"
# controller node id は chip-tool 既定 112233。デバイス側はその node id に admin。
MAT_E2E_KVS_DIR="$WORK" \
MAT_E2E_NODE_ID="$NODE_ID" \
MAT_E2E_PEER="[::1]:5540" \
  cargo test -p mat-controller --test live_case_im -- --ignored --nocapture

echo "== e2e:m2 PASS"
