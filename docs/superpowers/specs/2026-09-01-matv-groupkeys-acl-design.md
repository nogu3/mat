# matv KeySetWrite + ACL enforcement + matd×matv 回帰テスト — 設計

日付: 2026-09-01 / 対象ブランチ: worktree-matv-subscribe（base: main 5528861）

## 背景と訂正

監査バックログ③（2026-08-31）は「matv の SubscribeRequest が INVALID_ACTION
落ち」を前提にしていたが、これは誤り。Subscribe サーバは 2026-08-16 の
58af574 で実装済み（`net/runtime.rs:1210` で SubscribeRequest を
`Node::handle_im` より先に横取り → `serve_subscribe_request`。priming /
dirty レポート / keep-alive + E2E テスト `tests/subscribe_loop.rs`）。
`core/datamodel.rs` モジュール doc の "still no subscriptions" と
`mat-controller/src/im.rs:205-207` の STATUS_INVALID_ACTION doc は陳腐化した
記述で、本作業で修正する。

実スコープは同枠の残り2点 + ③の本来の動機だった回帰テスト:

1. **KeySetWrite / group-key-map write 未実装** — `mat group provision` が
   matv に通らない。
2. **ACL enforcement 未実装** — `AclStore` は保存のみ。「このリクエストは
   許可か」を問う関数が無く、呼び出し元 subject も IM 層に渡っていない。
3. **matd×matv 統合回帰テスト不在** — 「matd 常駐 Subscribe + `mat listen`
   を仮想デバイスで回帰テスト」に対応するテストが無い。

**スコープ外（別タスク・バックログ記録）**: groupcast 実受信（group session
復号・epoch key 永続化・GroupTable の endpoints 配線）、CAT subject
マッチ、EventRequests。

## 1. KeySetWrite + group-key-map write（`core/group_key_management.rs`）

ゴール: `mat group provision` の4ステップ（KeySetWrite invoke →
group-key-map write → AddGroup → ACL merge write）のうち欠けている 1・2 を
埋め、provision 全体が matv に成功する。

- **`GroupKeyStore`** を新設 — `AclStore` と同型の
  `Arc<Mutex<..>>` 共有 state。保持するもの:
  - keyset: `(fabric_index, keyset_id: u16, epoch_key0: [u8;16])`
  - map: `(fabric_index, group_id: u16, keyset_id: u16)`
  - API: `upsert_keyset` / `keyset_exists` / `replace_fabric_map` /
    `append_map_entry` / `map_entries_for(fabric)` / `purge_fabric`
  - **永続化なし**（`core/groups.rs` の membership と同じ M3 送り。
    groupcast タスクで epoch key 永続化と一緒にやるのが自然）。doc に明記。
  - `CommissioningServer` にも clone を配り、RemoveFabric / fail-safe
    rollback の purge に参加させる（`AclStore` の
    `set_acl_store`/`purge_fabric` と同じ配線。`device.rs` の組み立てと
    `commissioning.rs` の purge 3箇所）。
- **KeySetWrite (cmd 0x00)**: GroupKeySetStruct をデコード
  （`Context(0)`=GroupKeySetID u16 / `Context(1)`=policy(TrustFirst=0 のみ
  受理) / `Context(2)`=EpochKey0 16B 必須 / EpochKey1/2 は null 前提）。
  malformed / 制約違反は `STATUS_CONSTRAINT_ERROR`。fabric ごと upsert、
  新規 keyset id で既に `MAX_GROUP_KEYS_PER_FABRIC=1` に達していれば
  `STATUS_RESOURCE_EXHAUSTED`。PASE（`ctx.fabric_index == 0`）は
  `STATUS_UNSUPPORTED_ACCESS`（access_control の write ガードと同じ裁定）。
  `accepted_commands()` に `CMD_KEY_SET_WRITE` を追加。応答は
  `InvokeReply::Status(SUCCESS)`（KeySetWrite に response command は無い）。
- **`write` オーバーライド（`ATTR_GROUP_KEY_MAP`）**: 全置換
  （対象 fabric のみ retain-replace）+ `list_append` append の両対応
  （`access_control.rs` の write と同パターン）。エントリは
  `GroupKeyMapStruct { Context(1)=group_id, Context(2)=keyset_id }`。
  group_id 0 と、対象 fabric に存在しない keyset の参照は
  `STATUS_CONSTRAINT_ERROR`。PASE は `STATUS_UNSUPPORTED_ACCESS`。成功時
  `ctx.changed.push(ATTR_GROUP_KEY_MAP)`。
- **read**: `ATTR_GROUP_KEY_MAP` は保存内容を返す（`fabric_filtered` を尊重、
  各 struct に `Context(254)`=fabricIndex 付き — access_control の
  `write_acl_entry` と同じ流儀）。**`ATTR_GROUP_TABLE` は空 array のまま**
  （endpoints の cross-cluster 配線は groupcast タスク。mat クライアントは
  読まない）— doc に明記。

## 2. ACL enforcement

- **subject 配線**: `ReadCtx` / `InvokeCtx` に `subject: u64` を追加
  （PASE セッションでは 0。fabric_index==0 が既存の PASE マーカーなので
  auth_mode フィールドは追加しない）。`net/runtime.rs` が
  `SecureSession::peer_node_id()`（session.rs:221）から埋める。
- **判定**: `core/access_control.rs` に追加:
  - `decode_targets(raw: &[u8]) -> Option<Vec<AclTargetDev>>` —
    `TargetStruct { Context(0)=cluster, Context(1)=endpoint,
    Context(2)=device_type }` を意味的にデコード（現状は raw passthrough
    のみ）。デコード不能な targets_raw は「どこにもマッチしない」安全側。
  - privilege 束: Administer(5) ⊃ Manage(4) ⊃ Operate(3) ⊃ View(1)、
    ProxyView(2) ⊃ View(1)。
  - `AclStore::check(fabric_index, subject, required_privilege, endpoint,
    cluster) -> bool` — 対象 fabric のエントリのうち、auth_mode==CASE(2)
    かつ subjects が subject を含む（**空 subjects は wildcard**、CAT は
    未対応 = 完全一致のみ、doc 明記）かつ privilege が required を含意し、
    targets が None（無制限）または いずれかの target が
    (endpoint, cluster) にマッチ（device_type 制約付き target は不一致
    扱い=安全側）するものが 1 件でもあれば許可。
- **必要 privilege は `ClusterHandler` の default メソッド**:
  `read_privilege(attr) -> u8` (default View=1) /
  `write_privilege(attr) -> u8` (default Operate=3) /
  `invoke_privilege(cmd) -> u8` (default Operate=3)。spec に沿った
  オーバーライド: AccessControl 読み書き=Administer、
  GroupKeyManagement KeySetWrite=Administer・map write=Manage、
  OperationalCredentials / GeneralCommissioning /
  AdministratorCommissioning のコマンド=Administer、Identify=Manage、
  BasicInformation NodeLabel/Location write=Manage。実装時に spec 表と照合。
- **enforcement 位置**（`core/datamodel.rs`、`Node` に
  `set_acl_store(AclStore)` を追加）:
  - `read_entries`（single choke point → 通常 read・priming・dirty
    レポート全部に効く）: 具体パスの不許可は per-path
    `STATUS_UNSUPPORTED_ACCESS` (0x7E)、**wildcard 展開は不許可属性を
    黙って落とす**（spec §8.4.2.2 の挙動）。global 属性は View。
  - `handle_write`: per-entry `AttributeStatusIB` に UNSUPPORTED_ACCESS。
  - `handle_invoke`: UNSUPPORTED_ACCESS ステータス応答。
  - **PASE（fabric_index==0）は implicit Administer で常に許可**
    （commissioning が壊れない）。CASE は AddNOC の自動 admin エントリ
    （実装済み `add_case_admin`）で従来フローが通る。
- `access_control.rs:15` 付近の「enforcement 未実装だから実害なし」doc を
  現実に合わせて書き換え。

## 3. matd×matv 回帰テスト

- **mat-device 統合テスト**（`tests/`、既存 `support::commission_directly`
  利用）:
  1. group provision フロー: commissioned CASE セッションで
     KeySetWrite（`im::encode_key_set_write_fields`）→ map write
     （`im::encode_group_key_map_tlv`）→ AddGroup → map read back
     （fabric filter 済み・fabricIndex 付き）。
  2. ACL 拒否: admin エントリを View のみに置換 → toggle が
     UNSUPPORTED_ACCESS、wildcard read で不許可属性が落ちる、具体パス
     read が per-path UNSUPPORTED_ACCESS。
- **新 E2E スクリプト `scripts/e2e-device-m3.sh`** + task
  `e2e:device:m3`（`e2e-device-m1.sh` の流儀踏襲、`MAT_E2E_IFACE`
  デフォルト eth1、`task check` には入れない）:
  matv 起動 → `mat fabric init` + `mat commission` →
  **`mat group provision` 成功 assert**（KeySetWrite 実線検証）→
  同 store で matd 起動 → `matd status` で購読 Established →
  `mat listen --count 1` をバックグラウンドに → `mat onoff toggle`
  （matd 経由）→ listen が on-off イベント JSON を吐くことを assert。
  これが③の動機「matd 常駐 Subscribe + mat listen の仮想デバイス回帰」の
  本体。

## 4. 付随

- 陳腐化 doc 修正: `core/datamodel.rs` モジュール doc（"still no
  subscriptions"）、`mat-controller/src/im.rs:205-207`、
  `group_key_management.rs` モジュール doc（最小実装の申し送り更新）。
- 完了後: メモリ更新（監査③の前提訂正 + groupcast 実受信を新規バック
  ログ化）、`superpowers:finishing-a-development-branch`。

## テスト戦略

TDD（superpowers:test-driven-development）。core 層は純ユニットテスト
（`--no-default-features` で通る配置を維持 — core に tokio/socket/file を
持ち込まない）。runtime 配線は既存の closed-loop テスト群
（`net/runtime.rs` の実ソケットテスト）に追随。コミット毎 `task check` 緑。
