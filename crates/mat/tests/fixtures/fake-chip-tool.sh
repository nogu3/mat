#!/bin/sh
# テスト用ダミー chip-tool。実 chip-tool 不要で discover / commission /
# read / write / invoke / describe / open-window の統合テストを回すため、固定の
# ログ風テキストを吐く。
#
# 挙動は環境変数で制御:
#   FAKE_CHIP_MODE = success(既定) | timeout | reject
# mat は末尾に `--storage-directory <path>` を付けるが、ここでは無視する。

mode="${FAKE_CHIP_MODE:-success}"

# テスト検証用: 受け取った全引数を記録（PAA フラグ受け渡し等の確認に使う）。
if [ -n "$FAKE_CHIP_ARGS_FILE" ]; then
  echo "$*" > "$FAKE_CHIP_ARGS_FILE"
fi

# 自 fabric CFID のダミー出力。第1候補 = operational discovery のインスタンス名
# `<CFID>-<NodeId>`、第2候補 = `Compressed FabricId 0x...` 行。テストで個別に抑止可能。
#   FAKE_CHIP_NO_DIS_CFID=1 → インスタンス名行のみ抑止（第2候補の回帰テスト用）
#   FAKE_CHIP_NO_CFID=1     → 両方抑止（cfid_unavailable のテスト用）
if [ -z "$FAKE_CHIP_NO_CFID" ]; then
  if [ -z "$FAKE_CHIP_NO_DIS_CFID" ]; then
    echo "[DIS] OperationalSessionSetup[1:0000000000000005]: resolved instance 00AABB1122CC3344-0000000000000005._matter._tcp.local." >&2
  fi
  echo "[FP] Compressed FabricId 0x00AABB1122CC3344, FabricId 0x1" >&2
fi

# read/write/invoke/describe 共通の失敗注入。success 以外なら該当ログを吐いて非 0 終了。
emit_failure() {
  case "$mode" in
    timeout)
      echo "[1656][CHIP:DMG] CHIP Error 0x00000032: Timeout" >&2
      exit 1
      ;;
    reject)
      echo "[1656][CHIP:TOO] Received Command Response Status status 0x81 (Failure)"
      exit 1
      ;;
  esac
}

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
    # open-commissioning-window は同じ `pairing` サブコマンド。発行コードを吐く。
    if [ "$2" = "open-commissioning-window" ]; then
      emit_failure
      cat <<'EOF'
[1656][CHIP:CTL] Manual pairing code: [36217551492]
[1656][CHIP:SVR] SetupQRCode: [MT:-24J0AFN00KA0648G00]
EOF
      exit 0
    fi
    case "$mode" in
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
  groupsettings)
    # コントローラ側 group state（ローカル操作）。add-group / add-keysets /
    # bind-keyset。ネットワーク不要なので timeout/reject 注入はしない。
    echo "[1656][CHIP:TOO] $2 ok"
    exit 0
    ;;
  groupkeymanagement)
    # provision のデバイス書き込み: key-set-write / write group-key-map。
    emit_failure
    echo "[1656][CHIP:DMG] AttributeStatusIB ="
    echo "[1656][CHIP:DMG]   status = 0x00 (SUCCESS),"
    exit 0
    ;;
  groups)
    # provision の AddGroup（Groups クラスタ）。成功 status 行を吐く。
    emit_failure
    echo "[1656][CHIP:DMG] Received Command Response Status for Endpoint=0x1 Cluster=0x0000_0004 Command=0x0000_0000 Status=0x0 (SUCCESS)"
    exit 0
    ;;
  descriptor)
    # `descriptor read <list> <node> <ep> ...`
    emit_failure
    list="$3"
    ep="$5"
    case "$list" in
      parts-list)
        # エンドポイント 0 の子: 1 つ（ep 1）。
        cat <<'EOF'
[1717][CHIP:TOO]   PartsList: 1 entries
[1717][CHIP:TOO]     [1]: 1
EOF
        ;;
      server-list)
        if [ "$ep" = "0" ]; then
          cat <<'EOF'
[1717][CHIP:TOO]   ServerList: 2 entries
[1717][CHIP:TOO]     [1]: 29
[1717][CHIP:TOO]     [2]: 31
EOF
        else
          cat <<'EOF'
[1717][CHIP:TOO]   ServerList: 2 entries
[1717][CHIP:TOO]     [1]: 6
[1717][CHIP:TOO]     [2]: 8
EOF
        fi
        ;;
    esac
    exit 0
    ;;
  threadnetworkdiagnostics)
    # Thread Network Diagnostics (cluster 53)。`... read <attr> <node> <ep>`。
    # mat diag thread のスナップショット用。スカラは DMG の Data 行、リスト属性は
    # TOO レイヤの `[i]: { ... }` 形で吐く。
    emit_failure
    attr="$3"
    # 部分結果テスト用: 指定属性だけ拒否させる（間欠不通の機器を模す）。
    if [ -n "$FAKE_THREAD_FAIL_ATTR" ] && [ "$attr" = "$FAKE_THREAD_FAIL_ATTR" ]; then
      echo "[1656][CHIP:TOO] Received Command Response Status status 0x81 (Failure)"
      exit 1
    fi
    case "$attr" in
      # 値・整形は jarvis node 5 の実機出力に合わせる（routing-role 5=Router、
      # 文字列に長さ注釈、neighbor は `Lqi`、ExtAddress は10進、route は `PathCost`）。
      routing-role)     echo "[1656][CHIP:DMG] Data = 5 (Router)," ;;
      network-name)     echo '[1656][CHIP:DMG] Data = "ha-thread-6562" (14 chars),' ;;
      extended-pan-id)  echo "[1656][CHIP:DMG] Data = 14789548233599576168 (unsigned)," ;;
      pan-id)           echo "[1656][CHIP:DMG] Data = 25954 (unsigned)," ;;
      partition-id)     echo "[1656][CHIP:DMG] Data = 597971536 (unsigned)," ;;
      channel)          echo "[1656][CHIP:DMG] Data = 15 (unsigned)," ;;
      neighbor-table)
        cat <<'EOF'
[1656][CHIP:TOO]   NeighborTable: 2 entries
[1656][CHIP:TOO]     [1]: {
[1656][CHIP:TOO]       Age: 21
[1656][CHIP:TOO]       ExtAddress: 7110405590318074745
[1656][CHIP:TOO]       Rloc16: 38912
[1656][CHIP:TOO]       Lqi: 3
[1656][CHIP:TOO]       AverageRssi: -65
[1656][CHIP:TOO]       LastRssi: -67
[1656][CHIP:TOO]       FrameErrorRate: 56
[1656][CHIP:TOO]       RxOnWhenIdle: true
[1656][CHIP:TOO]       IsChild: false
[1656][CHIP:TOO]      }
[1656][CHIP:TOO]     [2]: {
[1656][CHIP:TOO]       Age: 5
[1656][CHIP:TOO]       ExtAddress: 4768252830523895510
[1656][CHIP:TOO]       Rloc16: 13312
[1656][CHIP:TOO]       Lqi: 1
[1656][CHIP:TOO]       AverageRssi: -95
[1656][CHIP:TOO]       LastRssi: -94
[1656][CHIP:TOO]       FrameErrorRate: 0
[1656][CHIP:TOO]       RxOnWhenIdle: true
[1656][CHIP:TOO]       IsChild: false
[1656][CHIP:TOO]      }
EOF
        ;;
      route-table)
        cat <<'EOF'
[1656][CHIP:TOO]   RouteTable: 1 entries
[1656][CHIP:TOO]     [1]: {
[1656][CHIP:TOO]       Age: 32
[1656][CHIP:TOO]       ExtAddress: 7110405590318074745
[1656][CHIP:TOO]       Rloc16: 38912
[1656][CHIP:TOO]       RouterId: 38
[1656][CHIP:TOO]       NextHop: 45
[1656][CHIP:TOO]       PathCost: 1
[1656][CHIP:TOO]       LQIIn: 3
[1656][CHIP:TOO]       LQIOut: 3
[1656][CHIP:TOO]       LinkEstablished: true
[1656][CHIP:TOO]       Allocated: true
[1656][CHIP:TOO]      }
EOF
        ;;
    esac
    exit 0
    ;;
  *)
    # クラスタ名がサブコマンド位置に来る: read / write / invoke。
    op="$2"
    emit_failure
    case "$op" in
      read)
        # `<cluster> read <attribute> <node> <ep>`。固定で bool 値を返す。
        echo "[1656][CHIP:DMG] ReportDataMessage ="
        echo "[1656][CHIP:DMG]   AttributeReportIBs ="
        echo "[1656][CHIP:DMG]     Data = true,"
        exit 0
        ;;
      write)
        echo "[1656][CHIP:DMG] AttributeStatusIB ="
        echo "[1656][CHIP:DMG]   status = 0x00 (SUCCESS),"
        exit 0
        ;;
      *)
        # invoke（on/off 含む）: `<cluster> <command> <node> <ep>`。
        echo "[1656][CHIP:DMG] Received Command Response Status for Endpoint=0x1 Cluster=0x0000_0006 Command=0x0000_0001 Status=0x0 (SUCCESS)"
        exit 0
        ;;
    esac
    ;;
esac
