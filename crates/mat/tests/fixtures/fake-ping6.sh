#!/bin/sh
# テスト用ダミー ping6。FAKE_PING_LOSS（既定 50）% でロスを報告する。
loss="${FAKE_PING_LOSS:-50}"
echo "PING target 56 data bytes"
echo "3 packets transmitted, 1 received, ${loss}% packet loss, time 2002ms"
if [ "$loss" != "100" ]; then
  echo "rtt min/avg/max/mdev = 90.000/168.000/200.000/40.000 ms"
fi
exit 0
