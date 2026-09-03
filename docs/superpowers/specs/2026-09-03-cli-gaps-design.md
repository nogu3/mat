# CLI 欠けの補完（監査バックログ レーン B）— 設計 spec（2026-09-03）

## 目的

2026-08-31 監査「足りない機能 3: CLI の欠け」を一括で埋める。対象は 6 件:

1. `mat unpair`（RemoveFabric + 台帳削除）
2. `mat group list` / `mat group remove`
3. `mat fabric list`
4. cluster wildcard read の CLI 露出（`mat read` の `--attribute` 省略）
5. `mat listen --reconnect`
6. `--timed`（invoke / write の timed 上書き）

いずれも新しい Matter プロトコル機能は要らない。エンコーダ・デバイス側応答（matv）・
Engine API は揃っており、欠けているのは CLI / op 配管 / ローカル台帳の削除経路。

## 制約（並列レーンとの衝突回避）

- **触らない**: `mat-core/src/ids.rs` / `ids_gen.rs`（レーン C）、`mat-controller/src/im.rs`
  （レーン C）、`mat-controller/src/{case,session,x509}*`（レーン D）。
  - `--timed` は ids.rs を触らず mat-native op 側で `def.timed || override` として実装。
    ids.rs:314 付近の「CLI フラグは未提供」コメント 1 行だけは **最後の Task** で、
    main を fetch して C の float 対応がマージ済みか確認 → rebase 後に直す。
  - group 系デバイス op の新規定数（`CMD_REMOVE_GROUP` 等）は im.rs に足さず
    `mat-native/src/ops.rs` に局所定義する（`CLUSTER_DESCRIPTOR` と同じ扱い）。
- 触ってよい: `crates/mat/src/*`、`mat-native/src/{op,ops,runner,lib}.rs`、`matd/src/{protocol,server,subscription}.rs`、
  `mat-core/src/{store,alias,body}.rs`、`mat-controller/src/{group_settings,kvs}.rs`（読み出し/削除の追加のみ）。
- CLAUDE.md「op → TLV → body lives once」: 新 op は `NodeOpKind` + `run_node_op` に 1 arm。
  エンコードの二重実装禁止。CLI（cli.rs / resolve.rs / device_op::classify / matd_client::to_op）
  と wire（protocol::Op / server::to_device_op）は写像だけ。

## 1. `mat unpair`

```
mat unpair --node <N|ALIAS> [--force]
```

**経路**: 直経路のみ（`commission` と対称。`--matd` 明示は exit 2、自動発見はスキップ）。
台帳（`nodes.json`）の書き手は `mat` だけという設計ルール 4 を守る。

**手順**（`crates/mat/src/commands/unpair.rs` 新設、`fabric.rs` / `commission.rs` と同じ dedicated 系）:

1. `Store::open` → `require_node(node_id)`（未 commission = exit 11、`--force` でも同じ）。
2. デバイス側: `OneShotRunner` で `NodeOp { node_id, kind: NodeOpKind::RemoveFabric }` を実行。
   `run_node_op` の 1 arm:
   - `conn.read_json(0, 0x003E, 0x0005)`（OperationalCredentials / CurrentFabricIndex）→ u8。
   - `conn.invoke_for_data(0, 0x003E, 0x0A, Some(commissioning::encode_remove_fabric(idx)), false)`
     → 応答フィールド TLV を `decode_noc_response` で復号し、`statusCode` が 0 以外なら
     `device_rejected`。`NodeConn::invoke` は `()` しか返さないので、**`NodeConn::invoke_for_data(...)
     -> Result<Vec<u8>, MatError>`** を新設する（`Engine` 実装は既存 `SecureSession::invoke_for_data`
     へ委譲、`FakeConn` は `with_invoke_response(ep, cluster, cmd, tlv)` でスクリプト可能にする）。
     データ応答を持つ RemoveGroup（手順 2 節参照）も同じメソッドを使う。
   - body: `body::unpair_device(node_id, fabric_index)` = `{"removed": true, "fabric_index": idx}`。
3. 台帳側（デバイス成功時、または `--force`）:
   - `Store::remove_node(node_id) -> Result<bool, MatError>`（新設、`upsert_node` の隣。
     存在しなければ `Ok(false)`、`save_ledger` は atomic write）。
   - `AliasBook::remove_node(node_id, store_root) -> Result<Vec<String>, MatError>`（新設）:
     `nodes` から値 == node_id のエントリ全部、`endpoints` から外側キーが「その alias 名」
     または「node_id の数字文字列」のものを削除し、消した node alias 名を返す。
     aliases.toml が無ければ何もしない（absent-file 規律）。書き込みは atomic。
4. 出力（stdout 1 行）:

```json
{"timestamp":"...","node_id":24,"aliases_removed":["toilet_light"],
 "device":{"removed":true,"fabric_index":2},
 "ledger":{"removed":true}}
```

`--force` でデバイス側が失敗したとき:

```json
{"timestamp":"...","node_id":15,"aliases_removed":[],
 "device":{"removed":false,"error":{"kind":"unreachable","detail":"..."}},
 "ledger":{"removed":true}}
```

exit 0。`--force` 無しでデバイス側が失敗した場合は通常の typed error（3/4/5/6）で exit、
台帳は触らない。

**node_id の再利用はしない**: `commission::next_node_id` は max+1 のまま（stale SRP レコード
で同一 node_id の再 commission が CASE 必敗になる罠、メモリ ble-recommission-traps）。

**matd 側**（`matd/src/subscription.rs::spawn_subscription_manager`）: 現状は「台帳は増える一方
（削除 API 無し）」を前提に `subscribed: HashSet<u64>` を持つ。これを
`HashMap<u64, JoinHandle<()>>` にし、rescan で台帳から消えた node_id の購読ループを
`abort()` → `SubHealth::forget(node_id)`（`status` / `pending` / `touched` / `values` の該当
node を除去する新 API）→ `tracing::info!(node_id, "ledger rescan: node removed; unsubscribed")`。
`matd status` の `nodes` からも消える。abort は「購読ループの cancel-safe 性」に依存しない
（ループは Notify/sleep で待つだけで、abort で壊れる複合不変条件は無い — 実装時に
`node_subscription_loop` の await 点を確認して doc コメントに残す）。

## 2. `mat group list` / `mat group remove`

### `group list`

```
mat group list
```

ローカル KVS のみ（ネット不要）。`fabric init` と同じくローカルのみで完結し、
`--matd` は無視される（iface 解決・route dispatch より前に main.rs が早期
dispatch する）。
`mat-controller::group_settings::read_groups(main_ini, fabric_index) -> Result<GroupTable, GroupSettingsError>`
を新設: `f/<idx>/g` の FabricData から GroupData チェーン（`first_group` / `group_count`）、
KeyMap チェーン（`scan_map` 流用）、KeySet チェーン（`first_keyset` / `keyset_count` / `keyset_next`）
を歩く。読み取りは `KvsTxn::open`（flock 共有で良いなら read 用の非ロック経路を検討、無ければ
open→drop で可）。

```rust
pub struct GroupTable { pub groups: Vec<GroupRow>, pub keysets: Vec<KeysetRow> }
pub struct GroupRow  { pub group_id: u16, pub name: String, pub keyset_id: Option<u16> }
pub struct KeysetRow { pub keyset_id: u16, pub bound_groups: Vec<u16> }
```

鍵素材（epoch key / operational key / GKH）は構造体に載せない（`SelfIssueMaterials` の
redaction 規律と同じ）。出力:

```json
{"timestamp":"...","fabric_index":2,
 "groups":[{"group_id":1,"name":"desk","keyset_id":42}],
 "keysets":[{"keyset_id":42,"bound_groups":[1]}]}
```

`f/<idx>/g` が無い（一度も provision していない）場合は空配列で exit 0。チェーン破損は
`store_parse`（既存 `Corrupt` の写像）。

### `group remove`

```
mat group remove --group <ID|ALIAS> --nodes <N|ALIAS>... [--endpoint EP]   (EP 既定 1)
```

直経路のみ（`grant` と同じ理由: 稀な修復系でバージョンスキューに安全）。`--nodes` 必須
（コントローラ KVS はメンバー一覧を持たない）。

**デバイス側**（`mat-native/src/ops.rs::remove_group_node(conn, p) -> Result<RemoveGroupNodeReport, MatError>`、
provision の逆順、各ステップは `provision_step_err` と同型のプレフィクス付き）:

1. ACL: `read_json(0, ACL, ACL)` → `AclEntry` 群から `auth_mode == Group && subjects == [group_id]`
   を除去 → 変化があれば write。**read 失敗時は write しない**（`ensure_group_acl` と同じ安全則:
   admin エントリを失うとデバイスが文鎮化する）。
2. `RemoveGroup`（Groups 0x0004 / cmd 0x03、フィールド `{0: groupID}`）を `p.endpoint` に
   `invoke_for_data` → RemoveGroupResponse `{0: status, 1: groupID}` を復号。status 0 = 成功、
   NOT_FOUND (0x8B) = 未登録なので `group_removed: false` として続行（冪等）、それ以外は `device_rejected`。
3. `group-key-map` read → 当該 group_id の行を除去 → write（read-merge-write、`parse_group_key_map` /
   `encode_group_key_map_tlv` 流用）。
4. 書き戻した map にその keyset_id を参照する行が残っていなければ `KeySetRemove`（0x003F / cmd 0x03、
   `{0: groupKeySetID}`）。keyset_id は手順 3 の read 結果から取る（引数にしない）。

コントローラ側で keyset を共有している別 group があるかは無関係 — デバイスごとに「そのデバイス
の map に残っているか」で判断する。

runner: `runner::remove_group(r, engine, p) -> Result<Value, MatError>`（`provision` と同型:
node 失敗は `node {id}: {detail}` で即停止、**コントローラ KVS はその場合触らない**）。全ノード成功後に
`group_settings::remove_group(main_ini, fabric_index, group_id) -> Result<RemoveOutcome, _>`
（新設、1 トランザクション）: GroupData をチェーンから外す（前ノードの `next` をつなぎ替え、
`first_group` / `group_count` 更新）、KeyMap から group_id の行を外す（rebind の unbind 経路を関数化して流用）、
その keyset を参照する KeyMap 行が無くなれば KeySetData もチェーンから外す。`FabricList` は触らない。

出力:

```json
{"timestamp":"...","group_id":1,"endpoint":1,
 "nodes":[{"node_id":5,"acl_removed":true,"group_removed":true,"keymap_removed":true,"keyset_removed":true}],
 "controller":{"group_removed":true,"keyset_removed":true}}
```

## 3. `mat fabric list`

```
mat fabric list
```

ローカルのみ。`chip_tool_config.ini` の default セクションから `f/<n>/n` キーを列挙
（`kvs.rs` に `list_fabric_indices(main_ini) -> Result<Vec<u8>, KvsError>` を新設、行頭 `f/` の
キーを正規表現なしで手パース）。各 index について:

- `fabric_id` / `admin_node_id`: `read_self_issue_materials` は alpha.ini（CA 鍵）も要求するので
  使わず、`f/<idx>/n` の NOC subject だけを読む軽量関数 `read_noc_identity(main_ini, idx) -> (node_id, fabric_id)`
  を切り出す（既存の subject パーサを共用）。
- `compressed_fabric_id`: `f/<idx>/r` の RCAC 公開鍵 + fabric_id → `fabric::compressed_fabric_id`、16 桁大文字 hex
  （`fabric init` の出力と同形）。
- `ipk_epoch`: `read_mat_ipk_epoch` が `Some` → `"mat"`、`None` → `"chip-tool"`（chip-tool 固定 epoch を
  採用する経路、README Backend 節）。
- `current`: `--fabric-index`（既定 1 / `MAT_FABRIC_INDEX`）と一致。

```json
{"timestamp":"...","store":"/home/user/.config/mat",
 "fabrics":[{"fabric_index":1,"fabric_id":1,"admin_node_id":112233,
             "compressed_fabric_id":"ABCDEF0123456789","ipk_epoch":"mat","current":true}]}
```

INI が無い = `store_missing`（exit 10、`fabric init` 案内付き）。`fabric show` は作らない
（list の 1 行に全情報が載る）。

## 4. cluster wildcard read

```
mat read --node <N|ALIAS> --cluster <NAME|ID> [--endpoint EP]      # --attribute 省略
```

- cli.rs: `Read.attribute: Option<String>`。resolve.rs は機械的に通す。
- `NodeOpKind::ReadCluster { endpoint, cluster_in: String, cluster: u32 }`（新 arm）。
  `NodeOpKind::read_cluster(endpoint, cluster_in)` は `ids::resolve_cluster` で名前→ID（未知名は
  `unresolved_op`）。`run_node_op`: `conn.read_cluster(endpoint, cluster)` →
  `body::read_cluster_success(node_id, endpoint, cluster_in, rows)`:

```json
{"timestamp":"...","node_id":24,"endpoint":1,"cluster":"onoff",
 "attributes":{"on-off":true,"global-scene-control":true,"on-time":0,"65533":5}}
```

  キーは chip-tool 属性名（`ids::find_cluster(cluster).attrs` から逆引き）、未知 ID は 10 進文字列。
  list/struct 値もそのまま JSON（`merge_reports` が chunk / list_append を吸収済み — 単一属性 read の
  「巨大 list はチャンク拒否」制限の回避策になる旨を docs に書く）。デバイスが持たない属性は
  返らない（wildcard は per-attribute status を返さない — ops.rs:64 の既知意味論）。
- device_op::classify: `attribute: None` → `ReadCluster`、`Some` → 従来 `Read`。
- matd wire: `protocol::Op::Read.attribute` を `#[serde(default)] Option<String>` に。
  `server::to_device_op` は `None` → `ReadCluster`。`matd_client::to_op` は省略時に
  `attribute` キーを出さない。**version skew**: 新 mat → 旧 matd は matd 側で
  `missing field attribute` の parse_error（exit 1）になる。docs に明記（`--matd` 経路で
  `MAT_MATD=0` を案内）。
- op ログ（`Op::log_path`）は `cluster/*` 表記。

## 5. `mat listen --reconnect`

```
mat listen ... --reconnect
```

- 既定（フラグ無し）の挙動は不変（接続失敗 / EOF / 非 JSON 行 = exit 13）。
- `--reconnect`: 上記 3 種の喪失で **再接続**する。backoff は 1s → 2s → 4s → … → 30s 上限、
  接続成功でリセット。stderr に `tracing::warn!(attempt, backoff_s, "matd lost; reconnecting")`、
  再接続成功で `info!`。初回接続の失敗も同じループで待つ（matd より先に起動する consumer 向け）。
- `--count` は再接続を跨いで累積、`--timeout-ms` は全体を束ねる（deadline は 1 本、再接続の
  待ち時間も含む）。deadline 到達時の exit は既存 `finish_on_timeout`（0 件 = 3、1 件以上 = 0）。
- stdout にマーカー行は**出さない**（1 行 1 イベントの契約。matd 再起動後の初回バーストは既存の
  `priming: true` / `recovered: true` 意味論で consumer が扱える）。
- 実装: `run_listen_stream` を「1 接続分」として残し、外側に `run_listen_reconnecting(sockets, op, count, timeout_ms)`
  を足す。`received` カウンタとdeadline は外側が持ち、内側に `&mut` で渡す。
- docs 修正: commands.md の「ack 行を出力する」記述を実装（読み捨て）に合わせる。

## 6. `--timed`

```
mat invoke ... --timed
mat write  ... --timed
```

- 意味: **true への上書きのみ**。テーブルが `timed: true` の名前付きコマンド / `timed_write: true` の
  属性は常に timed（フラグで解除不可）。数値 ID・テーブル false のものは `--timed` で timed 化。
- 実装: `NodeOpKind::invoke(..., timed_override: bool)` / `NodeOpKind::write(..., timed_override: bool)` で
  `timed: class_timed || timed_override`。ids.rs は不変。`Invoke.timed` / `Write.timed` の型は既存の bool のまま。
- wire: `protocol::Op::Write` / `Op::Invoke` に `#[serde(default)] timed: bool`（旧 mat → 新 matd 互換）。
  `matd_client::to_op` は `--timed` のときだけ `"timed": true` を出す。旧 matd は未知フィールドを無視するので
  上書きが**効かない**（version skew、docs に明記）。
- group invoke には付けない（groupcast に timed request は無い、op.rs:290 の既存判断）。

## 7. コマンド分類の更新（`device_op` / docs Routing 節）

| op | 経路 |
|---|---|
| `unpair` | direct-only（dedicated、`--matd` 明示 = exit 2） |
| `group list` | direct-only（ローカル KVS、dedicated） |
| `group remove` | direct-only（`grant` と同じ） |
| `fabric list` | direct-only（ローカル、`fabric init` と同じ早期 dispatch） |
| `read`（wildcard） | matd 対応（`Op::Read` 拡張） |
| `listen --reconnect` | matd-only（従来どおり） |
| `--timed` | matd 対応（wire 拡張） |

## 8. エラー

新しい `kind` は追加しない。`unpair --force` のデバイス側失敗は出力 JSON 内の `device.error` に
既存 kind をそのまま入れる。`group remove` の NOT_FOUND は `group_removed: false` で吸収。

## 9. テスト

- **mat-core**: `store::remove_node`（存在/非存在、atomic 保存後の再読込）、`alias::remove_node`
  （node alias + 配下 endpoints + 数字キー endpoints、ファイル無し = no-op、他 alias は残る）、
  `body::*` の JSON 形（キー名、`attributes` の名前/数値キー混在）。
- **mat-native op**: `FakeConn` で `RemoveFabric`（read → invoke の順と TLV、status≠0 = device_rejected）、
  `ReadCluster`（`with_cluster` 既存ヘルパ）、`invoke/write` の `timed_override`（table false + override =
  timed true / table true + override false = true）。`ops::remove_group_node` の 4 ステップ
  （ACL read 失敗で write しない、NOT_FOUND 続行、keyset 参照残りで KeySetRemove を撃たない）。
- **mat-controller group_settings**: 既存 `tmp_ini` テスト群に `read_groups`（空 / 1 group / 共有 keyset）、
  `remove_group`（先頭 / 中間 / 末尾ノード、keyset 共有時は KeySet を残す、未登録 group = エラー）。
  `kvs::list_fabric_indices` / `read_noc_identity`。
- **mat CLI (integration.rs)**: `unpair` / `group list` / `group remove` / `fabric list` の
  `--matd` 明示 = exit 2、store 無し = 10、未 commission = 11、`read` の `--attribute` 省略が
  clap を通る、`--timed` が clap を通る。`native_direct.rs` 単体テストで unpair の成功 JSON 形
  （`--force` の失敗形含む）。
- **matd**: protocol round-trip（`attribute` 省略 / `timed` 省略・明示）、`to_device_op` の
  `ReadCluster` 写像、integration.rs で wildcard read の応答形、subscription.rs で台帳から
  node を消した rescan 後に status から消える（既存の rescan 追加テストの逆）。
- **listen (crates/mat/tests/listen.rs)**: fake matd が EOF → `--reconnect` で再接続して count 到達 exit 0、
  フラグ無しは exit 13 のまま、deadline が再接続待ちも束ねる。
- **e2e (matv)**: `scripts/e2e-device-m1.sh` の末尾に `mat unpair --node 1` → `nodes.json` から消える +
  matv 側ログに fabric 削除 → 再 commission 成功、を追加。`group remove` は `e2e-device-m3.sh`
  （provision 済み）の末尾に remove → `group list` が空、を追加。

## 10. 実機検証（マージ前、hogar-matd コンテナ内、他セッションと同時実行しない）

`task check` 緑後、x86_64 musl 静的 `mat` / `matd` を `docker cp` で hogar-matd コンテナへ（本番
バイナリは置換しない、`/tmp` 配下）:

- 直経路: `read --node 23 --cluster onoff`（wildcard）、`fabric list`、`group list`（読み取りのみ）。
- matd 経由（本番 matd は 1.30.0 のまま）: `read --node 24 --cluster onoff --attribute on-off`
  で無回帰、wildcard は旧 matd の skew 経路（parse_error）を確認、`MAT_MATD=0` で直経路成功。
- `listen --reconnect --count 1 --timeout-ms 30000` で 1 件受信 exit 0。
- **本番 fabric に `unpair` / `group remove` は撃たない**（matv + fresh store の e2e で担保）。
- 後始末: コピーしたバイナリを削除。

## 11. docs

- `docs/commands.md`: `unpair` 節（Discover and commissioning の直後）、`group list` / `group remove`
  （Groupcast 節）、`fabric list`（First-fabric bootstrap 節）、`read` wildcard 形（State operations）、
  `listen --reconnect`（Listen 節 + ack 行の記述修正）、`--timed`（State operations の invoke/write）、
  Routing 節の direct-only / matd 対応リストと version skew 注記。
- `README.md` Quickstart に `mat unpair --node 5` を 1 行。
- `CLAUDE.md` Backend 節の direct-only op 列挙に `unpair` / `group list` / `group remove` / `fabric list` を追加。
- `ARCHITECTURE.md` にレーン B 完了の記録 1 段落。
