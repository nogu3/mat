# matv groupcast 実受信（監査レーン A フェーズ 2）設計

日付: 2026-09-03 / 対象ブランチ: worktree-matv-groupcast-rx（base: main 19f205b = レーン A フェーズ 1 マージ後）
触る範囲: `crates/mat-device` + `crates/matv`（設定 1 項目 + 統合テスト）+ `scripts/e2e-device-m3.sh`。
mat-core / mat-controller / mat-native / crates/mat / matd には触らない（他レーン並行中）。

## 0. 背景と現状

③（2026-09-01）で matv は KeySetWrite / GroupKeyMap write / AddGroup / ACL Group エントリ
の**受理**まで実装したが、groupcast は受けない。コード確認済みの事実:

- `net/runtime.rs::run` は unicast ソケット 1 本（`config.port`、既定 0=エフェメラル）だけを
  `select!` で読み、`MessageHeader.security_flags` の session type ビット（group=0x01）を
  見ずに `session_id == current` で unicast 扱いする。group datagram は「session id 不一致」
  で落ちる。multicast join は無い（`mat_controller::transport::UdpTransport` に join API は無く、
  `group.rs` のテストは `tokio::net::UdpSocket::join_multicast_v6` を直接使う）。
- `GroupKeyStore`（`core/group_key_management.rs`）は epoch key と GroupKeyMap を**非永続**で
  保持。`ATTR_GROUP_TABLE` は常に空 array。
- `GroupsHandler`（`core/groups.rs`）は bridged endpoint ごとに独立した `members:
  Vec<(fabric, group)>` を持ち非永続。endpoint 横断の「group → endpoints」は誰も知らない。
- `AclStore::check` は `auth_mode == AUTH_MODE_CASE` のエントリしか照合しない。`Subject` に
  group 形は無い。フェーズ 1 で `validate_entry` は Group エントリ（`mat group grant` 形）を
  受理する。
- `mat_controller::im::decode_invoke_request` は CommandPath に endpoint（Context 0）を要求
  するので、group 形（endpoint 無し、`encode_group_invoke_request` が作る形）は
  `Malformed` になる → mat-device 側に group 用デコーダが要る。
- `mat_controller::crypto::open_message(key, datagram, src_node)` は group datagram を
  そのまま復号できる（`group.rs::group_datagram_roundtrips_with_group_header` が実証）。
  nonce は header の source node id を優先するので第 3 引数は使われない。
- 送信側 mat は `mat group invoke -g <gid> -c <cluster> --command <cmd>`（`GroupCommand::Invoke`）
  で `ff35:40:fd<fabric_id>:00<group_id>` の **5540 番**へ、各 egress iface に同一 datagram を
  投げる（SuppressResponse=true、MRP 無し）。
- `scripts/e2e-device-m3.sh` は `mat group provision`（KeySetWrite/map/AddGroup/ACL grant まで
  matv に着地）を検証するが groupcast 実受信は検証しない。

## 1. スコープ

**入る**: (a) GroupKeyStore と Groups membership の永続化、(b) GroupTable 属性の実配線、
(c) 専用 groupcast 受信ソケット + multicast join 差分同期、(d) group session 復号・
GroupKeyMap 検証・membership 検証・リプレイ検査、(e) Group auth mode の ACL 照合、
(f) group InvokeRequest の全 member endpoint へのディスパッチ（応答なし、購読の dirty に流す）、
(g) ユニット/統合テスト + e2e m3 への groupcast ステップ追加。

**入らない**: matv からの groupcast 送出、KeySetRead/KeySetRemove/KeySetReadAllIndices、
GroupKeySecurityPolicy=CacheAndSync、privacy key（security flags P ビット）、group 宛の
Write/Read（spec 上 Invoke のみ）、EventRequests、複数 keyset/fabric（`MAX_GROUP_KEYS_PER_FABRIC=1`
のまま）、IPK ローテーション。mat-controller の変更（必要な物はすべて mat-device 内に置く）。

## 2. 永続化（core + net/store）

### 2.1 GroupKeyStore

- `core/group_key_management.rs` に `pub trait GroupKeyPersist: Send { fn save(&self,
  keysets: &[GroupKeySet], map: &[GroupKeyMapEntry]) -> Result<(), String>; fn load(&self)
  -> Result<(Vec<GroupKeySet>, Vec<GroupKeyMapEntry>), String>; }`（`AclPersist` 同型、
  whole-table save/load）。`GroupKeySet` / `GroupKeyMapEntry` を `pub` + `Serialize/Deserialize`
  に昇格（フィールドは現状のまま: `fabric_index, keyset_id, epoch_key0: [u8;16]` /
  `fabric_index, group_id, keyset_id`）。
- `GroupKeyStore::with_persist(Box<dyn GroupKeyPersist>)`。`upsert_keyset` /
  `replace_fabric_map` / `append_map_entry` / `purge_fabric` の各変異の直後に save
  （`AclStore` と同じ: save 失敗は `tracing::warn` して in-memory は進める）。
- 読み出し API 追加: `pub fn keysets(&self) -> Vec<GroupKeySet>`（全 fabric、復号候補列挙用）。
- `net/store.rs`: `FileGroupKeyStore`（`<store_dir>/group_keys.json`、`{"keysets":[..],
  "map":[..]}` 1 ファイル、`mat_core::fsatomic::write_atomic`）+ `group_key_store_in_dir(dir)`。
  epoch key は平文 JSON（`fabrics.json` が既に op 秘密鍵を平文で持つのと同じ扱い）。
- `device.rs`: `GroupKeyStore::with_persist(Box::new(group_key_store_in_dir(&store_dir)))`。
  「非永続」を書いた doc（group_key_management.rs モジュール doc、device.rs のコメント）を更新。

### 2.2 Groups membership（新モジュール `core/group_membership.rs`）

- `pub struct GroupMembershipStore(Arc<Mutex<Inner>>)`（`AclStore` 同型、`Clone`、
  `mat_controller::sync::locked`）。レコード `pub struct GroupMember { fabric_index: u8,
  group_id: u16, endpoint: u16 }`（`Serialize/Deserialize`）。
- API: `new()` / `with_persist(Box<dyn GroupMembershipPersist>)` /
  `contains(fabric, group, endpoint) -> bool` / `add(fabric, group, endpoint) -> Result<(), u8>`
  （同 endpoint の件数（fabric 横断、従来の `GroupsHandler` と同じ数え方）が
  `GROUP_TABLE_CAPACITY`(16) 以上なら `STATUS_RESOURCE_EXHAUSTED`、既存なら Ok no-op）/ `remove(fabric, group, endpoint) -> bool` /
  `remove_all(fabric, endpoint)` / `groups_for(fabric, endpoint) -> Vec<u16>`（挿入順）/
  `endpoints_for(fabric, group) -> Vec<u16>`（挿入順）/ `groups_by_fabric() -> Vec<(u8, u16)>`
  （重複なし、join 集合と GroupTable 用）/ `purge_fabric(fabric)`。
- `GroupsHandler::new(identify, store: GroupMembershipStore, endpoint: u16)`: 内部 `members`
  を撤去し、全操作を store に委譲（`GetGroupMembership` の capacity 残数は
  `GROUP_TABLE_CAPACITY - groups_for(fabric, endpoint).len()`）。`core/bridge.rs::
  build_bridged_endpoint(kind, name, unique_id, endpoint, membership)` に引数追加、`device.rs`
  は台帳で採番した endpoint を渡す。既存ユニットテストは `GroupMembershipStore::new()` を
  渡して維持。
- 永続化: `net/store.rs` に `FileGroupMembershipStore`（`<store_dir>/groups.json`）+
  `group_membership_in_dir(dir)`。
- `CommissioningServer::set_group_membership_store(store)`: RemoveFabric と fail-safe
  rollback の purge に `AclStore`/`GroupKeyStore` と並べて配線（`commissioning.rs` の
  `purge_fabric` 呼び出し 3 箇所）。

### 2.3 GroupTable 属性

`GroupKeyManagementHandler::new(store, membership)`: `ATTR_GROUP_TABLE` は
`membership.groups_by_fabric()`（fabric_filtered なら `ctx.fabric_index` のみ）から
`GroupInfoMapStruct { 1: GroupId, 2: Endpoints (array u16), 254: FabricIndex }` の array を組む
（GroupName(3) は NameSupport=0 なので省略）。空 array 固定だった doc を更新。

## 3. 受信経路（net）

### 3.1 ソケット

- `DeviceConfig.group_port: u16`（既定 **5540**、0 = エフェメラル=テスト用）。`net/group_rx.rs`
  （新モジュール）の `GroupSocket::bind(port, iface_index) -> io::Result<GroupSocket>`:
  `socket2::Socket::new(Ipv6, Dgram)` → `set_only_v6(true)` + `set_reuse_address(true)` +
  `set_reuse_port(true)`（同ホストの mat/matd が 5540 を掴んでいても共存）→ `bind([::]:port)`
  → `set_nonblocking` → `tokio::net::UdpSocket::from_std`。bind 失敗は **fatal ではない**:
  `Device::new` で `tracing::warn!` して `None`（unicast のみで動く）。
- `Device::group_local_addr() -> Option<SocketAddr>`（テストがエフェメラルポートを知るため）。
  matv の JSON 1 行出力に `group_port` を追加。
- join 管理: `GroupSocket { socket, iface_index, joined: HashSet<Ipv6Addr> }` +
  `sync_joins(&mut self, desired: &HashSet<Ipv6Addr>)`: desired − joined を
  `join_multicast_v6(&addr, iface_index)`、joined − desired を `leave_multicast_v6`。失敗は
  `tracing::warn!`（`lo` は IFF_MULTICAST 無しで join できない — テストはこの経路を通るので
  非致命）。成功した分だけ `joined` に入れる（失敗分は次回また試す）。
- desired 集合は `runtime` が組む: `comm_server.fabrics()` の各 `FabricEntry{fabric_index,
  fabric_id}` × `membership.groups_by_fabric()` の同 fabric の group → `group_multicast_addr
  (fabric_id, group_id)`。**毎ループ末尾（secured message を 1 件 serve した後・fail-safe
  expiry 後・RemoveFabric 後）に `sync_joins` を呼ぶ**（差分計算は HashSet 数個、通知機構不要）。
  起動時にも 1 回（永続化から復元した membership の join）。

### 3.2 分類と復号（`net/group_rx.rs`、純関数中心）

`run` の `select!` に `recv = group_socket.recv_from(&mut gbuf), if group_socket.is_some()`
ブランチを追加。unicast ソケット側は `security_flags & 0x01 != 0`（group session type）の
datagram を「group socket の担当」として debug ログで drop（`IPV6_MULTICAST_ALL` で unicast
ソケットにも届き得るため）。

`serve_group_datagram(buf, from, deps) -> Option<Vec<(u16,u32,u32)>>`（changed パス）:

1. `MessageHeader::decode`。`security_flags & 0x01 == 0`、または `destination` が
   `Destination::Group(gid)` でない、または `source_node_id` が `None` → drop（debug）。
   privacy ビット（0x80）が立っていれば未対応として drop（debug）。
2. 復号候補: `gk_store.keysets()` の各 keyset について fabric（`comm_server.fabrics()` から
   `fabric_index` 一致）の `compressed_fabric_id(&root_public_key, fabric_id)` で
   `operational = derive_ipk_operational(&epoch_key0, &cfid)`、
   `gkh = derive_group_session_id(&operational)`。`gkh == header.session_id` の候補を順に
   `crypto::open_message(&operational, buf, source_node_id)` に通し、最初に成功した
   `(fabric_index, keyset_id, proto, payload)` を採用（GKH 衝突時の試行復号 = spec §4.15.3）。
   全滅 → drop（debug、候補数を出す）。fabric ごとに毎回 KDF 2 回（HKDF-SHA256、µs オーダ、
   keyset は fabric あたり 1 個）— キャッシュは設けない（YAGNI、状態を持たない方が
   RemoveFabric/KeySetWrite の追従が要らない）。
3. GroupKeyMap 検証: `gk_store.map_entries_for(fabric_index)` に `(gid, keyset_id)` が
   無ければ drop（debug）。
4. membership: `membership.endpoints_for(fabric_index, gid)` が空なら drop（debug）。
5. リプレイ検査（`GroupReplayGuard`、runtime 保有の in-memory）: key `(fabric_index,
   source_node_id)` → 最終 `message_counter`。既知で `counter <= last` は drop（debug、
   mat が複数 egress に流す同一 datagram の重複がここで落ちる）。未知 or 大きければ受理し
   更新。エントリ上限 64（超えたら最古を退去、`VecDeque` 順）。spec §4.5.4 の trust-first +
   window の簡略版であることを doc に明記（bitmap window 無し、順序逆転は捨てる側に倒す）。
6. `proto.protocol_id == IM && proto.opcode == INVOKE_REQUEST` 以外は drop（debug）。
7. `decode_group_invoke_request(&payload) -> Result<Vec<GroupInvokeIn{cluster, command,
   fields_tlv}>, ()>`（`core/group_invoke.rs`、mat-controller の
   `decode_request_command_data_ib` の構造を写した独自デコーダ: `{0: SuppressResponse,
   1: TimedRequest, 2: InvokeRequests[ CommandDataIB{0: CommandPath list{1: cluster,
   2: command}(endpoint 無し), 1: fields} ], 255}`。endpoint タグが**あっても**無視する）。
   失敗 → drop（debug）。
8. `node.handle_group_invoke(&endpoints, &invokes, &mut InvokeCtx{fabric_index, subject:
   Subject::group(gid), ..})` → changed パスを返す。runtime はそれを既存の
   `subscription.note_changed(&changed)` に流す（購読中の matd に反映される）。

応答は一切送らない（spec §8.2.5: group 宛 Invoke は SuppressResponse、MRP なし）。

### 3.3 ACL（`core/access_control.rs`）

- `Subject` に `pub group_id: Option<u16>` を追加（`Copy` 維持、`Default`=None）+
  `Subject::group(group_id: u16) -> Self`（node_id 0、cats 空）。
- `AclStore::check`: エントリの `auth_mode` で分岐 — `AUTH_MODE_CASE` は従来どおり
  `subject.group_id.is_none()` のときだけ照合、`AUTH_MODE_GROUP` は `subject.group_id ==
  Some(g)` かつ（`subjects` 空 = wildcard、または `subjects.contains(&u64::from(g))`）。
  privilege / targets の判定は共通。`Subject::matches` は変更しない。
- `mat group grant` が書く `Operate/Group/[gid]/targets null` で OnOff の On/Off/Toggle
  （Operate）が通り、Groups の AddGroup 等（Manage）や Administer 系は通らない。

### 3.4 ディスパッチ（`core/datamodel.rs`）

- `Node::handle_invoke` の「handler 特定 → ACL → invoke → changed に (endpoint, cluster,
  attr) を付けて DataVersion bump」の後半を private `invoke_on_endpoint(&mut self, endpoint,
  cluster, command, fields, ctx) -> Result<(InvokeReply, Vec<(u16,u32,u32)>), u8>` に切り出し、
  既存 `handle_invoke` と新 `pub fn handle_group_invoke(&mut self, endpoints: &[u16],
  invokes: &[GroupInvokeIn], ctx: &mut InvokeCtx) -> Vec<(u16,u32,u32)>` の両方から使う。
  group 版は endpoint × invoke の全組み合わせを回し、UNSUPPORTED_* / ACL 拒否は debug ログのみ
  （応答が無いので）、changed を連結して返す。
- `ctx.subject` は `Subject::group(gid)`、`fabric_index` は復号で確定した fabric。PASE の
  fabric 0 バイパス（`acl_allows`）には掛からない（fabric_index ≥ 1 のみ到達）。

### 3.5 runtime への配線

`run(transport, local_addr, config, node, comm_server, group: GroupRxDeps)` —
`GroupRxDeps { socket: Option<GroupSocket>, gk_store: GroupKeyStore, membership:
GroupMembershipStore }` を `Device` が保持して渡す。`ServeState` は変えない（group 経路は
session を持たないので別関数）。既存の `serve_secured` 群には触らない。

## 4. matv

- `FileConfig.group_port: Option<u16>`（TOML `group_port`、省略時 5540）→ `DeviceConfig.group_port`。
- 起動時 JSON 行に `"group_port": <実ポート or null>`。
- README / docs の matv 節に「groupcast 受信は 5540 固定（REUSEPORT）、`group_port = 0` で
  無効化ではなくエフェメラル」を 1 段落。

## 5. テスト

ユニット（`cargo test -p mat-device --features net`）:
- `group_invoke::decode_group_invoke_request`: `im::encode_group_invoke_request` の出力
  （fields 有/無）を復号、endpoint 付き CommandPath も受理、壊れた TLV は Err。
- `GroupMembershipStore`: add/remove/remove_all/endpoints_for/groups_for/容量/purge、
  persist 往復（`net/store.rs` に tempdir テスト、`acl_store_with_persist_reloads_across_instances`
  と同型）。`GroupKeyStore` の persist 往復も同様。
- `GroupsHandler` 既存テストが store 経由で全部通る + 2 endpoint が同じ group に入ると
  `endpoints_for` が両方返す。
- `ATTR_GROUP_TABLE` の encode（fabric filter 有/無）。
- `AclStore::check`: Group エントリ × `Subject::group` の許可 / CASE subject には不一致 /
  Group subject は CASE エントリに不一致 / subjects 空 wildcard / privilege 不足。
- `Node::handle_group_invoke`: 2 endpoint の Toggle が両方反転し changed が 2 件、ACL 無し
  fabric は無変化、`handle_invoke` 既存テスト無退行。
- `GroupReplayGuard`: 初回受理・同値拒否・後退拒否・前進受理・上限退去。
- `group_rx::serve_group_datagram` を `build_group_datagram`（mat-controller pub）で作った
  datagram に対して: 正常 → changed、GKH 不一致 → None、map 未登録 → None、membership
  無し → None、リプレイ → None、privacy ビット → None。`GroupCredentials{session_id,
  encryption_key}` は pub フィールドで組める。

統合（`tests/group_receive.rs`、`support::commission_directly` の閉ループ）:
1. commission → KeySetWrite(EPOCH) → map write (GID, KEYSET) → AddGroup(BRIDGED_EP) →
   ACL 全置換で admin + `Operate/Group/[GID]/null` を追加（`group_provision.rs` の
   エンコーダ流用）。
2. テスト側で `operational = derive_ipk_operational(&EPOCH, &compressed_fabric_id(
   &rcac.pub_key, FABRIC_ID))`、`GroupCredentials{ session_id: derive_group_session_id(
   &operational), encryption_key: operational }`、`build_group_datagram(&creds,
   ADMIN_NODE_ID, counter, exchange, GID, CLUSTER_ON_OFF, CMD_ON_OFF_ON, None)` を
   **`device.group_local_addr()` へ unicast UDP 送信**（multicast 不要、`lo` で動く。
   header の destination は Group のまま）。
3. CASE で `on-off` を read → true。同 datagram を再送 → 変化なし（Toggle を使って
   「1 回だけ反転」を assert: counter 同値の再送で戻らない、counter+1 の Toggle で戻る）。
4. ACL の Group エントリを外して再送 → 変化なし（ACL enforcement）。
5. `Device` を drop → 同 store_dir で `Device::new` → `read` で GroupKeyMap / GroupTable /
   Groups membership（`GetGroupMembership`）が復元されている（永続化）。
1 テスト = 1 CASE セッション（matv は同時 1 CASE）: 3〜5 は同じセッションで順に行う。
再起動確認だけは新 Device + 新セッション。

e2e（`scripts/e2e-device-m3.sh`、`task e2e:device:m3`）: `mat group provision` の直後・matd
起動の**前**に、`mat --iface "$IFACE" group invoke -g "$GROUP_ID" -c onoff --command on
-e "$DEVICE_EP"`（`GroupCommand::Invoke`: `-g/--group`, `-c/--cluster`, `--command`,
`-e/--endpoint`）→ `mat --iface "$IFACE" read --node "$NODE_ID" --endpoint "$DEVICE_EP"
--cluster onoff --attribute on-off` の `value` が `true` を assert
（groupcast が multicast 経由で matv に届いた実証。同ホスト multicast loopback は
`group_sender_multicast_loops_back_locally` が前提にする挙動）。matv の `group_port` は
既定 5540 のまま（mat/matd は 5540 を bind せず宛先にだけ使うので衝突しない。REUSEPORT は他プロセス共存の保険）。その後 `mat off` で戻してから
既存の matd/listen ステップに続ける。

## 6. エラー/運用

- group 経路は全部「黙って drop + debug ログ」（応答が無い protocol なので）。ログには
  drop 理由・fabric_index・group_id・source node id・counter を出す。
- 5540 bind 失敗（権限・REUSEPORT 非対応）は warn 1 回で unicast のみ継続。join 失敗は
  warn（同じ addr は次回 sync で再試行、ログは 1 addr につき状態変化時のみ）。
- 永続化 save 失敗は warn して in-memory 続行（AclStore と同じ）。load 失敗（壊れた JSON）は
  空から開始 + warn。

## 7. やらないこと（再掲・補足）

- matv からの groupcast 送出、KeySetRead/Remove、CacheAndSync、privacy、group 宛
  Read/Write、bitmap リプレイ窓、複数 keyset。
- mat-controller への join API 追加（mat-device 内で socket2/tokio 直叩き）。
- GroupName の保持（NameSupport=0）。
