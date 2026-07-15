#!/usr/bin/env bash
# M6b 実機 E2E — jarvis 上で実行する（BLE は WSL では動かない）。
# 事前:
#   1) sudo ot-ctl dataset active -x  → MAT_E2E_THREAD_DATASET
#   2) 対象（玄関ライト）を工場リセットし、印字の passcode/discriminator を控える
#   3) bluetoothctl power on / preflight: cargo run --features ble --example ble-scan
set -euo pipefail
cd "$(dirname "$0")/.."
: "${MAT_E2E_BLE_PASSCODE:?device setup passcode}"
: "${MAT_E2E_BLE_DISCRIMINATOR:?12-bit discriminator}"
: "${MAT_E2E_THREAD_DATASET:?ot-ctl dataset active -x}"
: "${MAT_E2E_IFACE:?e.g. eth0}"
: "${MAT_E2E_PAA_DIR:?<store>/paa-trust-store}"
: "${MAT_E2E_NODE_ID:=200}"
export MAT_E2E_NODE_ID
exec cargo test -p mat-controller --features ble --test live_commission_ble \
  -- --ignored --nocapture
