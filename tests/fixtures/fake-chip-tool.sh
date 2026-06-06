#!/bin/sh
# テスト用ダミー chip-tool。実 chip-tool 不要で discover / commission の
# 統合テストを回すために、固定のログ風テキストを吐く。
#
# 挙動は環境変数で制御:
#   FAKE_CHIP_MODE = success(既定) | timeout | reject
# mat は末尾に `--storage-directory <path>` を付けるが、ここでは無視する。

sub="$1"

case "$sub" in
  discover)
    cat <<'EOF'
[1717][CHIP:DIS] Discovered commissionable/commissioner node:
[1717][CHIP:DIS] 	Hostname: B827EBA8C9F0
[1717][CHIP:DIS] 	IP Address #1: 192.0.2.10
[1717][CHIP:DIS] 	Port: 5540
[1717][CHIP:DIS] 	Long Discriminator: 3840
[1717][CHIP:DIS] 	Vendor ID: 65521
[1717][CHIP:DIS] 	Product ID: 32769
EOF
    exit 0
    ;;
  pairing)
    case "${FAKE_CHIP_MODE:-success}" in
      success)
        echo "[1656][CHIP:CTL] Successfully finished commissioning, deviceId=1"
        echo "[1656][CHIP:TOO] Device commissioning completed with success"
        exit 0
        ;;
      timeout)
        echo "[1656][CHIP:DMG] CHIP Error 0x00000032: Timeout" >&2
        exit 1
        ;;
      reject)
        echo "[1656][CHIP:TOO] Received Command Response Status status 0x81 (Failure)"
        exit 1
        ;;
    esac
    ;;
  *)
    echo "fake-chip-tool: unhandled subcommand: $sub" >&2
    exit 2
    ;;
esac
