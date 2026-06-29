#!/bin/sh
# テスト用ダミー avahi-browse。
#
# 優先順位:
#   1. FAKE_AVAHI_OUT のパス内容をそのまま吐く（既存互換）。
#   2. FAKE_AVAHI_ADDR が設定されていれば、その IP アドレスを持つ resolved ブロックを生成。
#      FAKE_AVAHI_FABRIC で fabric を指定可（既定 0011223344556677）。
#   3. いずれも未設定なら「該当ノードの広告なし」を模す（他 fabric の無関係ノードのみ）。
if [ -n "$FAKE_AVAHI_OUT" ] && [ -f "$FAKE_AVAHI_OUT" ]; then
  cat "$FAKE_AVAHI_OUT"
elif [ -n "$FAKE_AVAHI_ADDR" ]; then
  FAKE_AVAHI_FABRIC="${FAKE_AVAHI_FABRIC:-0011223344556677}"
  echo "+   eth0 IPv6 ${FAKE_AVAHI_FABRIC}-0000000000000005   _matter._tcp   local"
  echo "=   eth0 IPv6 ${FAKE_AVAHI_FABRIC}-0000000000000005   _matter._tcp   local"
  echo "   hostname = [dummy.local]"
  echo "   address = [${FAKE_AVAHI_ADDR}]"
  echo "   port = [5540]"
else
  echo "+   eth0 IPv6 0011223344556677-00000000000000FF   _matter._tcp   local"
fi
exit 0
