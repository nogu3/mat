#!/usr/bin/env bash
# [M8c-3] chip-tool 撤去済みのため 0.22.0 以降では動かない（歴史的アーカイブ。
# 動かすなら git tag の 0.21.0 時点を checkout）。現行ハーネスは e2e-m8c3-real.sh。
# Phase 5 M6a 受け入れ: native commissioning ローカル E2E。
# 前提: ./chip-all-clusters-app (task chip:extract:app)。chip-tool は不要。
set -euo pipefail
cd "$(dirname "$0")/.."
APP=${MAT_E2E_APP:-./chip-all-clusters-app}
PASSCODE=20202021
[[ -x "$APP" ]] || { echo "error: $APP なし (task chip:extract:app)"; exit 1; }

WORK=$(mktemp -d)
APP_PID=""
cleanup() { [[ -n "$APP_PID" ]] && kill "$APP_PID" 2>/dev/null || true; rm -rf "$WORK"; }
trap cleanup EXIT

echo "== 1/3 テスト用 PAA 証明書取得 (connectedhomeip v1.4.2.0)"
mkdir -p "$WORK/paa" .e2e-cache
BASE=https://raw.githubusercontent.com/project-chip/connectedhomeip/v1.4.2.0/credentials/development/paa-root-certs
for f in Chip-Test-PAA-FFF1-Cert.der Chip-Test-PAA-NoVID-Cert.der; do
  [[ -f ".e2e-cache/$f" ]] || curl -fsSL "$BASE/$f" -o ".e2e-cache/$f"
  cp ".e2e-cache/$f" "$WORK/paa/"
done

echo "== 2/3 app 起動 (KVS: $WORK/device_kvs)"
"$APP" --KVS "$WORK/device_kvs" >"$WORK/app.log" 2>&1 &
APP_PID=$!
for i in $(seq 1 40); do
  ss -uln 2>/dev/null | grep -q ':5540' && break
  kill -0 "$APP_PID" 2>/dev/null || { echo "app 起動失敗"; cat "$WORK/app.log"; exit 1; }
  sleep 0.25
done

echo "== 3/3 native commissioning ライブテスト"
MAT_E2E_PEER="[::1]:5540" \
MAT_E2E_PASSCODE="$PASSCODE" \
MAT_E2E_PAA_DIR="$WORK/paa" \
  cargo test -p mat-controller --test live_commissioning -- --ignored --nocapture \
  || { echo "テスト失敗 — app ログ:"; cat "$WORK/app.log"; exit 1; }

echo "== e2e:m6 PASS"
