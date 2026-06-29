#!/bin/sh
# テスト用ダミー avahi-browse。FAKE_AVAHI_OUT のパス内容をそのまま吐く。
# 未指定なら「該当ノードの広告なし」を模す（他 fabric の無関係ノードのみ）。
if [ -n "$FAKE_AVAHI_OUT" ] && [ -f "$FAKE_AVAHI_OUT" ]; then
  cat "$FAKE_AVAHI_OUT"
else
  echo "+   eth0 IPv6 0011223344556677-00000000000000FF   _matter._tcp   local"
fi
exit 0
