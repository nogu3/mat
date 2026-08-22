# Apple Home interview 通過（IM Write + AccessControl + root 適合性）設計

## 問題

Apple Home（iPhone ホームアプリ + Apple TV ハブ）で matv のペアリングが
「commissioning 完走 → 直後に RemoveFabric で撤収」になる（2026-08-22 実測 2 回、
`~/matv-apple/matv.stderr.log`）。attestation は通っており（Echo の Amazon 側凍結とは別問題）、
落ちるのは commissioning 後の interview フェーズ。

## 証拠と根本原因

実測ログ（2 回とも同一）:

1. PASE → attestation → AddNOC → CASE → CommissioningComplete 全て成功
2. iPhone が **IM WriteRequest (opcode 0x06)** を送信 → matv は Write 未実装
   （M2 スコープ外）なので **StatusResponse で拒否**
3. iPhone は全属性 Subscribe と Fabrics 読みの後、**RemoveFabric** して離脱

この Write は Apple の流儀で必須の **AccessControl(0x001F) への ACL エントリ書き込み**
（commissioner=iPhone とは別ノードのホームハブ Apple TV に管理権限を与える）。
これが通らない = ハブが制御不能 = ペアリング失敗として綺麗に撤収する。
chip-tool / HA（matter-server）は commissioner = 制御者（同一ノード）なので
AddNOC の自動 ACL だけで足り、露見しなかった。

第 1 試行では iPhone が root の NetworkCommissioning(0x0031) /
GeneralDiagnostics(0x0033) / TimeSync(0x0038) / ICD(0x0046) も明示 read しており
（全て UNSUPPORTED_CLUSTER 応答）、root node の必須クラスタ欠落も interview 不合格の
リスク要因。参照実装（Apple 通過実績のある matter.js）は root に AccessControl /
NetworkCommissioning / GeneralDiagnostics / GroupKeyManagement を全て持つ。

なお第 1 段の修正（EP1 Identify/Groups + AcceptedCommandList/GeneratedCommandList
実申告、e57cc95）だけでは症状が変わらないことを実機で確認済み（仮説反証の記録）。

## 設計

### 1. IM Write 経路（device 側）

- `mat-controller::im` に device 側の `decode_write_request` /
  `encode_write_response` を追加（controller 側 `encode_write_request_tlv` /
  `decode_write_response` の鏡像。wire 形は既存 encoder が正）
- `ClusterHandler` に `write(attribute, data_tlv, ctx) -> Result<(), u8>` を追加
  （デフォルト `Err(STATUS_UNSUPPORTED_WRITE=0x88)`）。Data は read と同じ
  「Anonymous タグの完全な TLV 要素 1 個」規約に正規化して渡す
- `Node::handle_write` が AttributeStatusIB 列の WriteResponse を返し、
  変更 path は `ImOutcome::changed` に載せる（購読レポートは既存機構で無償）
- スコープ: untimed write のみ（Apple の ACL write は untimed）。
  suppress_response は無視して常に応答

### 2. AccessControl クラスタ (0x001F, EP0)

- 新規 `core/access_control.rs`。エントリは
  `{privilege, auth_mode, subjects: Vec<u64>, targets(raw TLV passthrough), fabric_index}`
- ストアは `Arc<Mutex<Vec<Entry>>>`（in-memory、永続化は M3 送り）を
  AccessControlHandler と CommissioningServer で共有:
  - **AddNOC 成功時に自動 admin エントリ**（spec §11.17.6.8: privilege=Administer(5),
    authMode=CASE(2), subjects=[CaseAdminSubject]）を追加
  - fabric 撤去（RemoveFabric / fail-safe rollback）でその fabric のエントリを purge
- write は「書き込み fabric のエントリ全置換」（ACL write は全置換が仕様）。
  read は読み手 fabric のエントリのみ返す（fabric_index=0 = PASE は空）
- 付随必須属性: SubjectsPerAccessControlEntry(2)=4, TargetsPerAccessControlEntry(3)=3,
  AccessControlEntriesPerFabric(4)=4
- **ACL の enforcement（認可チェック）はスコープ外**（単一家庭 LAN、M3 で検討）

### 3. root 必須クラスタの最小実装（EP0）

- **NetworkCommissioning (0x0031)**: FeatureMap=Ethernet(0x04)。
  MaxNetworks=1, Networks=[{networkID=iface名, connected=true}],
  InterfaceEnabled=true, LastNetworkingStatus/LastNetworkID/LastConnectErrorValue=null。
  Ethernet はコマンド無し。per-cluster FeatureMap のため `ClusterHandler::feature_map()`
  （デフォルト 0）を追加
- **GeneralDiagnostics (0x0033)**: NetworkInterfaces=[{name=iface, isOperational=true,
  hardwareAddress=6B zero, IPv4/IPv6=[], type=Ethernet(2)}], RebootCount=0,
  UpTime=起動からの秒, TestEventTriggersEnabled=false。
  TestEventTrigger コマンドは常に CONSTRAINT_ERROR（enable key 不一致の spec 挙動）
- **GroupKeyManagement (0x003F)**: 属性のみ（GroupKeyMap=[], GroupTable=[],
  MaxGroupsPerFabric=16, MaxGroupKeysPerFabric=1）。KeySetWrite 系コマンドは未実装の
  既知ギャップとして残す（Apple のペアリングでは呼ばれない実測/コミュニティ知見）

### 4. BasicInformation の必須属性充足

追加: NodeLabel(5, **writable**・in-memory), Location(6)="XX", HardwareVersion(7)=1,
HardwareVersionString(8)="matv", SoftwareVersion(9)=1, SoftwareVersionString(10)=
crate version, CapabilityMinima(19)={CaseSessionsPerFabric:3, SubscriptionsPerFabric:3},
UniqueID(18)=store に永続化した乱数 hex（`store/unique_id`、初回起動時に生成）,
SpecificationVersion(21)=0x01040000, MaxPathsPerInvoke(22)=1。
NodeLabel は Apple が書きに来ることがあるため write 対応（32 文字上限）。

## 受け入れ基準

- 自動: workspace 全テスト + clippy + fmt / chip-tool フルゲート
  （`MAT_E2E_IFACE=eth0 task e2e:device:m2-chip`）/ mat ゲート（`e2e:device:m1`）PASS
- 実機（人間チェックポイント）: iPhone ホームアプリで matv を追加 →
  **部屋割り当てまで進み、ホームアプリのタイルから OnOff がトグルできる**
  （matv ログで Apple TV（ハブ）からの CASE + OnOff invoke を確認）

## スコープ外（M3 送り）

- ACL / NodeLabel / Groups テーブルの永続化（現状 in-memory、再起動で消える）
- ACL enforcement（認可判定）
- GroupKeyManagement のコマンド群（KeySetWrite 等）
- Timed write、chunked write、FabricFiltered=false の read
