//! GroupKeyManagement クラスタサーバ (spec §11.2, cluster 0x003F) —
//! RootNode デバイスタイプの必須クラスタ（Device Library §9.2.2）。Apple
//! Home は commissioning 直後の interview でこのクラスタの不在を咎める。
//!
//! `KeySetWrite`（spec §11.2.7.1）と `ATTR_GROUP_KEY_MAP` の write/read
//! （spec §11.2.7.6、全置換 + `ListIndex` null append の両径路）を実装し、
//! `GroupKeyStore` に保持する。`ATTR_GROUP_TABLE` は
//! `core::group_membership::GroupMembershipStore`（Groups クラスタ側の
//! エンドポイント紐付け帳簿）から派生する読み取り専用ビュー。
//! `KeySetRead`（§11.2.8.2、EpochKey は null で返す）/ `KeySetRemove`（§11.2.8.4、
//! GroupKeyMap の参照行もカスケード削除）/ `KeySetReadAllIndices`（§11.2.8.5）も
//! 実装済み。IPK = keyset 0 は `GroupKeyStore` には持たず（`FabricEntry` 側）、
//! Read/ReadAllIndices では常在の仮想 keyset として応答し、Remove は
//! INVALID_COMMAND で拒む。keyset 0 は書き込みでも `GroupKeyStore` に入らない
//! — `KeySetWrite(0)`（IPK rotation）は同じく INVALID_COMMAND で拒むので、
//! `GroupKeyStore` が keyset 0 を保持している状態はこの実装ではサポート外。
//! 永続化は `with_persist` で `<store_dir>/group_keys.json`
//! （`net::store::FileGroupKeyStore`）に行う。
use std::sync::{Arc, Mutex};

use mat_controller::im;
use mat_controller::sync::locked;
use mat_controller::tlv::{Reader, Tag, Value, Writer};
use serde::{Deserialize, Serialize};

use crate::core::datamodel::{ClusterHandler, InvokeCtx, InvokeReply, ReadCtx};
use crate::core::group_membership::GroupMembershipStore;

/// `MaxGroupsPerFabric`/`MaxGroupKeysPerFabric` (spec §11.2.7.5) — 固定値を
/// 返すのみで実容量は追跡しない（`AccessControlHandler`の容量属性と同じ
/// 割り切り）。`MAX_GROUP_KEYS_PER_FABRIC` は申告値と同時に、
/// `GroupKeyStore::upsert_keyset` が実際に強制する容量上限でもある
/// （`access_control::ACL_ENTRIES_PER_FABRIC` と同じ「単一定数で申告と
/// 実装の食い違いを構造的にあり得なくする」パターン）。
const MAX_GROUPS_PER_FABRIC: u64 = 16;
const MAX_GROUP_KEYS_PER_FABRIC: usize = 1;

/// GroupKeyManagement のコマンド id（spec §11.2.8）。`mat_controller::im` は
/// `CMD_KEY_SET_WRITE` しか持たず、im.rs は他レーンの編集領域なのでここで
/// 局所定義する（`mat-native/src/ops.rs` の `CMD_KEY_SET_REMOVE` と同じ裁定）。
pub const CMD_KEY_SET_READ: u32 = 0x01;
pub const RESP_KEY_SET_READ: u32 = 0x02;
pub const CMD_KEY_SET_REMOVE: u32 = 0x03;
pub const CMD_KEY_SET_READ_ALL_INDICES: u32 = 0x04;
pub const RESP_KEY_SET_READ_ALL_INDICES: u32 = 0x05;
/// IPK の KeySet id（spec §11.2.6.2）。`GroupKeyStore` には持たず
/// （`FabricEntry.ipk_operational` 側）、KeySetRead/ReadAllIndices は仮想的に
/// 常在として応答し、KeySetRemove は INVALID_COMMAND で拒む。
pub const IPK_KEY_SET_ID: u16 = 0;

/// デバイス上の 1 KeySet（epoch key 0 のみ保持 — epoch 1/2 は spec 上
/// optional でこの実装では未対応、モジュール doc 参照）。`Debug` は鍵を
/// 伏せる（`FabricEntry`/`GroupCredentials` と同じ方針）。
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupKeySet {
    pub fabric_index: u8,
    pub keyset_id: u16,
    pub epoch_key0: [u8; 16],
    /// KeySetWrite の `EpochStartTime0`（spec §11.2.6.2、epoch-us）。
    /// v1.31.0 以前の `group_keys.json` には無いので `default` = 0 で読む。
    /// KeySetRead が返す以外の用途は無い（epoch 1/2 非対応なので鍵選択に
    /// 使わない）。
    #[serde(default)]
    pub epoch_start_time0: u64,
}

impl std::fmt::Debug for GroupKeySet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GroupKeySet")
            .field("fabric_index", &self.fabric_index)
            .field("keyset_id", &self.keyset_id)
            .field("epoch_key0", &"[REDACTED]")
            .field("epoch_start_time0", &self.epoch_start_time0)
            .finish()
    }
}

/// `GroupKeyMapStruct` 1 件（spec §11.2.7.6: GroupId → KeySetID）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupKeyMapEntry {
    pub fabric_index: u8,
    pub group_id: u16,
    pub keyset_id: u16,
}

/// 永続化境界（`core::access_control::AclPersist` と同型: whole-table
/// save/load、具象は `net::store::FileGroupKeyStore`）。
pub trait GroupKeyPersist: Send {
    fn save(&self, keysets: &[GroupKeySet], map: &[GroupKeyMapEntry]) -> Result<(), String>;
    fn load(&self) -> Result<(Vec<GroupKeySet>, Vec<GroupKeyMapEntry>), String>;
}

/// `GroupKeyStore`'s guarded state — `AclStore`/`AclInner`（
/// `core::access_control`）と同じ形。
#[derive(Default)]
struct GroupKeyInner {
    keysets: Vec<GroupKeySet>,
    map: Vec<GroupKeyMapEntry>,
    persist: Option<Box<dyn GroupKeyPersist>>,
}

/// GroupKeyManagement の共有 state。`GroupKeyManagementHandler`（EP0 の
/// クラスタハンドラ）と、`CommissioningServer`（fabric 撤去の purge、
/// `core::commissioning::set_group_key_store`）・将来の groupcast 配線の
/// 両方から触られる想定で `Arc<Mutex<..>>` + `Clone`（`AclStore` と同じ
/// パターン、モジュール doc 参照）。永続化は任意（`new` は非永続、
/// `with_persist` で `<store_dir>/group_keys.json` に永続化）。
#[derive(Clone, Default)]
pub struct GroupKeyStore(Arc<Mutex<GroupKeyInner>>);

impl GroupKeyStore {
    /// 空ストア。
    pub fn new() -> Self {
        Self::default()
    }

    /// `persist` から復元して開始する。load 失敗（壊れた JSON 等）は warn
    /// して空から始める — 起動を止めない。
    pub fn with_persist(persist: Box<dyn GroupKeyPersist>) -> Self {
        let (keysets, map) = match persist.load() {
            Ok(state) => state,
            Err(e) => {
                tracing::warn!(error = %e, "group key store: load failed; starting empty");
                (Vec::new(), Vec::new())
            }
        };
        Self(Arc::new(Mutex::new(GroupKeyInner {
            keysets,
            map,
            persist: Some(persist),
        })))
    }

    /// 全 fabric の KeySet（groupcast 受信の復号候補列挙用）。
    pub fn keysets(&self) -> Vec<GroupKeySet> {
        self.lock().keysets.clone()
    }

    /// poison 耐性 lock（`mat_controller::sync::locked` — `AclStore` と同じ
    /// 全クレート共通ヘルパ）— 1 パニックでストア全体が触れなくなるのを
    /// 避ける。
    fn lock(&self) -> std::sync::MutexGuard<'_, GroupKeyInner> {
        locked(&self.0)
    }

    /// save 失敗は warn して in-memory を進める（`AclStore::save` と同じ理由:
    /// 直前の正当な状態が残るだけで安全性は落ちない）。
    fn save(guard: &GroupKeyInner) {
        if let Some(persist) = &guard.persist {
            if let Err(e) = persist.save(&guard.keysets, &guard.map) {
                tracing::warn!(error = %e, "group key store: save failed; keeping in-memory state");
            }
        }
    }

    /// KeySetWrite (spec §11.2.7.1) の実処理: 同 `(fabric_index,
    /// keyset_id)` が既にあれば epoch key を置換（upsert）。無ければ
    /// fabric 内 keyset 数が `MAX_GROUP_KEYS_PER_FABRIC` 未満のときだけ
    /// 追加、上限に達していれば `STATUS_RESOURCE_EXHAUSTED`。
    pub fn upsert_keyset(
        &self,
        fabric_index: u8,
        keyset_id: u16,
        epoch_key0: [u8; 16],
        epoch_start_time0: u64,
    ) -> Result<(), u8> {
        let mut guard = self.lock();
        if let Some(existing) = guard
            .keysets
            .iter_mut()
            .find(|k| k.fabric_index == fabric_index && k.keyset_id == keyset_id)
        {
            existing.epoch_key0 = epoch_key0;
            existing.epoch_start_time0 = epoch_start_time0;
            Self::save(&guard);
            return Ok(());
        }
        let count = guard
            .keysets
            .iter()
            .filter(|k| k.fabric_index == fabric_index)
            .count();
        if count >= MAX_GROUP_KEYS_PER_FABRIC {
            return Err(im::STATUS_RESOURCE_EXHAUSTED);
        }
        guard.keysets.push(GroupKeySet {
            fabric_index,
            keyset_id,
            epoch_key0,
            epoch_start_time0,
        });
        Self::save(&guard);
        Ok(())
    }

    /// KeySetRemove (spec §11.2.8.4) の実処理: `(fabric_index, keyset_id)` の
    /// KeySet を落とし、同 fabric の GroupKeyMap でその keyset を参照する行も
    /// 落とす（手順 4 のカスケード）。返り値は map が変化したか。無ければ
    /// `STATUS_NOT_FOUND`。lock 1 回・save 1 回。
    pub fn remove_keyset(&self, fabric_index: u8, keyset_id: u16) -> Result<bool, u8> {
        let mut guard = self.lock();
        let before = guard.keysets.len();
        guard
            .keysets
            .retain(|k| !(k.fabric_index == fabric_index && k.keyset_id == keyset_id));
        if guard.keysets.len() == before {
            return Err(im::STATUS_NOT_FOUND);
        }
        let map_before = guard.map.len();
        guard
            .map
            .retain(|m| !(m.fabric_index == fabric_index && m.keyset_id == keyset_id));
        let map_changed = guard.map.len() != map_before;
        Self::save(&guard);
        Ok(map_changed)
    }

    /// accessing fabric の KeySet id 一覧（挿入順）— KeySetReadAllIndices 用。
    pub fn keyset_ids_for(&self, fabric_index: u8) -> Vec<u16> {
        self.lock()
            .keysets
            .iter()
            .filter(|k| k.fabric_index == fabric_index)
            .map(|k| k.keyset_id)
            .collect()
    }

    /// `(fabric_index, keyset_id)` の KeySet のコピー — KeySetRead 用
    /// （応答は鍵を返さないので、呼び出し側は `epoch_start_time0` だけ使う）。
    pub fn find_keyset(&self, fabric_index: u8, keyset_id: u16) -> Option<GroupKeySet> {
        self.lock()
            .keysets
            .iter()
            .find(|k| k.fabric_index == fabric_index && k.keyset_id == keyset_id)
            .cloned()
    }

    pub fn keyset_exists(&self, fabric_index: u8, keyset_id: u16) -> bool {
        self.lock()
            .keysets
            .iter()
            .any(|k| k.fabric_index == fabric_index && k.keyset_id == keyset_id)
    }

    /// fabric 撤去時の purge（RemoveFabric / fail-safe rollback 共用、
    /// `AclStore::purge_fabric` と同じ用途）。keyset・map の両方から
    /// 当該 fabric のエントリを落とす。
    pub fn purge_fabric(&self, fabric_index: u8) {
        let mut guard = self.lock();
        guard.keysets.retain(|k| k.fabric_index != fabric_index);
        guard.map.retain(|m| m.fabric_index != fabric_index);
        Self::save(&guard);
    }

    /// 書き込み fabric の GroupKeyMap エントリを丸ごと入れ替える
    /// （`ATTR_GROUP_KEY_MAP` write の全置換径路）。
    pub fn replace_fabric_map(&self, fabric_index: u8, entries: Vec<(u16, u16)>) {
        let mut guard = self.lock();
        guard.map.retain(|m| m.fabric_index != fabric_index);
        guard.map.extend(
            entries
                .into_iter()
                .map(|(group_id, keyset_id)| GroupKeyMapEntry {
                    fabric_index,
                    group_id,
                    keyset_id,
                }),
        );
        Self::save(&guard);
    }

    /// 書き込み fabric の GroupKeyMap 末尾に 1 件足す（write の
    /// ListIndex null append 径路）。
    pub fn append_map_entry(&self, fabric_index: u8, group_id: u16, keyset_id: u16) {
        let mut guard = self.lock();
        guard.map.push(GroupKeyMapEntry {
            fabric_index,
            group_id,
            keyset_id,
        });
        Self::save(&guard);
    }

    pub fn map_entries_for(&self, fabric_index: u8) -> Vec<(u16, u16)> {
        self.lock()
            .map
            .iter()
            .filter(|m| m.fabric_index == fabric_index)
            .map(|m| (m.group_id, m.keyset_id))
            .collect()
    }

    /// 全 fabric 分の `(fabric_index, group_id, keyset_id)` — groupcast
    /// 送出側が「この GroupId 宛の keyset はどれか」を fabric 横断で引く
    /// ための読み口（後続タスク用）。
    pub fn all_map_entries(&self) -> Vec<(u8, u16, u16)> {
        self.lock()
            .map
            .iter()
            .map(|m| (m.fabric_index, m.group_id, m.keyset_id))
            .collect()
    }
}

#[derive(Default)]
pub struct GroupKeyManagementHandler {
    store: GroupKeyStore,
    membership: GroupMembershipStore,
}

impl GroupKeyManagementHandler {
    pub fn new(store: GroupKeyStore, membership: GroupMembershipStore) -> Self {
        Self { store, membership }
    }

    /// `write` の共通制約チェック（設計メモ参照）: `group_id == 0`（無効
    /// GroupId）または `fabric_index` に `keyset_id` の KeySet が無ければ
    /// `STATUS_CONSTRAINT_ERROR`。
    fn check_map_entry(&self, fabric_index: u8, entry: &(u16, u16)) -> Result<(), u8> {
        let (group_id, keyset_id) = *entry;
        if group_id == 0 || !self.store.keyset_exists(fabric_index, keyset_id) {
            return Err(im::STATUS_CONSTRAINT_ERROR);
        }
        Ok(())
    }
}

impl ClusterHandler for GroupKeyManagementHandler {
    fn cluster_id(&self) -> u32 {
        im::CLUSTER_GROUP_KEY_MANAGEMENT
    }

    /// ClusterRevision (spec §7.13): Group Key Management cluster spec
    /// revision 2 (Matter 1.4).
    fn revision(&self) -> u16 {
        2
    }

    fn attributes(&self) -> Vec<u32> {
        vec![
            im::ATTR_GROUP_KEY_MAP,
            im::ATTR_GROUP_TABLE,
            im::ATTR_MAX_GROUPS_PER_FABRIC,
            im::ATTR_MAX_GROUP_KEYS_PER_FABRIC,
        ]
    }

    /// `ATTR_GROUP_KEY_MAP`（spec §11.2.7.6）と `ATTR_GROUP_TABLE`（spec
    /// §11.2.7.7）はどちらも `ctx.fabric_filtered` を尊重する fabric-scoped
    /// list（`AccessControlHandler::read`の `ATTR_ACL`と同じ扱い）: filtered
    /// なら `ctx.fabric_index` 分のみ、unfiltered なら全 fabric 分。
    /// `ATTR_GROUP_TABLE` は `self.membership` から派生する（モジュール
    /// doc）。
    fn read(&self, attribute: u32, ctx: &ReadCtx) -> Option<Vec<u8>> {
        match attribute {
            im::ATTR_GROUP_KEY_MAP => {
                let entries: Vec<(u8, u16, u16)> = if ctx.fabric_filtered {
                    self.store
                        .map_entries_for(ctx.fabric_index)
                        .into_iter()
                        .map(|(group_id, keyset_id)| (ctx.fabric_index, group_id, keyset_id))
                        .collect()
                } else {
                    self.store.all_map_entries()
                };
                Some(encode_group_key_map(&entries))
            }
            im::ATTR_GROUP_TABLE => {
                // spec §11.2.7.7 GroupInfoMapStruct {1: GroupId, 2: Endpoints,
                // 254: FabricIndex} — GroupName(3) は NameSupport=0 なので省略。
                let mut w = Writer::new();
                w.start_array(Tag::Anonymous);
                for (fabric_index, group_id) in self.membership.groups_by_fabric() {
                    if ctx.fabric_filtered && fabric_index != ctx.fabric_index {
                        continue;
                    }
                    w.start_struct(Tag::Anonymous);
                    w.put_uint(Tag::Context(1), u64::from(group_id));
                    w.start_array(Tag::Context(2));
                    for endpoint in self.membership.endpoints_for(fabric_index, group_id) {
                        w.put_uint(Tag::Anonymous, u64::from(endpoint));
                    }
                    w.end_container();
                    w.put_uint(Tag::Context(254), u64::from(fabric_index));
                    w.end_container();
                }
                w.end_container();
                Some(w.finish())
            }
            im::ATTR_MAX_GROUPS_PER_FABRIC => {
                let mut w = Writer::new();
                w.put_uint(Tag::Anonymous, MAX_GROUPS_PER_FABRIC);
                Some(w.finish())
            }
            im::ATTR_MAX_GROUP_KEYS_PER_FABRIC => {
                let mut w = Writer::new();
                w.put_uint(Tag::Anonymous, MAX_GROUP_KEYS_PER_FABRIC as u64);
                Some(w.finish())
            }
            _ => None,
        }
    }

    /// KeySet 系 4 コマンドを受理する: `CMD_KEY_SET_WRITE`（spec §11.2.7.1）/
    /// `CMD_KEY_SET_READ`（§11.2.8.2）/ `CMD_KEY_SET_REMOVE`（§11.2.8.4）/
    /// `CMD_KEY_SET_READ_ALL_INDICES`（§11.2.8.5）。それ以外は
    /// `STATUS_UNSUPPORTED_COMMAND`。PASE セッション（`ctx.fabric_index ==
    /// 0`、`AccessControlHandler::write`と同じガード）は
    /// `STATUS_UNSUPPORTED_ACCESS`。KeySetWrite のフィールドデコードは
    /// `decode_key_set_write_fields` — 構造的な TLV 破損は
    /// `STATUS_INVALID_COMMAND`、フィールド欠落・policy 不正・鍵長不正は
    /// `STATUS_CONSTRAINT_ERROR`（設計メモ参照）。`keyset_id ==
    /// IPK_KEY_SET_ID`（IPK rotation、この実装では未対応）も
    /// `STATUS_INVALID_COMMAND`（`KeySetRemove(0)` と同じ裁定、モジュール
    /// doc 参照）。成功時は response command なしの `STATUS_SUCCESS`。
    /// KeySetRead/Remove のフィールドデコードは `decode_key_set_id` —
    /// 形不正・id 欠落は `STATUS_INVALID_COMMAND`。
    fn invoke(&mut self, command: u32, fields_tlv: &[u8], ctx: &mut InvokeCtx) -> InvokeReply {
        if !matches!(
            command,
            im::CMD_KEY_SET_WRITE
                | CMD_KEY_SET_READ
                | CMD_KEY_SET_REMOVE
                | CMD_KEY_SET_READ_ALL_INDICES
        ) {
            return InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND);
        }
        if ctx.fabric_index == 0 {
            return InvokeReply::Status(im::STATUS_UNSUPPORTED_ACCESS);
        }
        match command {
            im::CMD_KEY_SET_WRITE => {
                let (keyset_id, epoch_key0, epoch_start_time0) =
                    match decode_key_set_write_fields(fields_tlv) {
                        Ok(fields) => fields,
                        Err(KeySetWriteError::Malformed) => {
                            return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
                        }
                        Err(KeySetWriteError::Constraint) => {
                            return InvokeReply::Status(im::STATUS_CONSTRAINT_ERROR);
                        }
                    };
                if keyset_id == IPK_KEY_SET_ID {
                    return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
                }
                match self.store.upsert_keyset(
                    ctx.fabric_index,
                    keyset_id,
                    epoch_key0,
                    epoch_start_time0,
                ) {
                    Ok(()) => InvokeReply::Status(im::STATUS_SUCCESS),
                    Err(status) => InvokeReply::Status(status),
                }
            }
            CMD_KEY_SET_READ => {
                let Some(keyset_id) = decode_key_set_id(fields_tlv) else {
                    return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
                };
                let epoch_start_time0 = if keyset_id == IPK_KEY_SET_ID {
                    0
                } else {
                    match self.store.find_keyset(ctx.fabric_index, keyset_id) {
                        Some(ks) => ks.epoch_start_time0,
                        None => return InvokeReply::Status(im::STATUS_NOT_FOUND),
                    }
                };
                InvokeReply::Data {
                    response_command: RESP_KEY_SET_READ,
                    fields_tlv: encode_key_set_read_response(keyset_id, epoch_start_time0),
                }
            }
            CMD_KEY_SET_REMOVE => {
                let Some(keyset_id) = decode_key_set_id(fields_tlv) else {
                    return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
                };
                if keyset_id == IPK_KEY_SET_ID {
                    return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
                }
                match self.store.remove_keyset(ctx.fabric_index, keyset_id) {
                    Ok(true) => {
                        ctx.changed.push(im::ATTR_GROUP_KEY_MAP);
                        InvokeReply::Status(im::STATUS_SUCCESS)
                    }
                    Ok(false) => InvokeReply::Status(im::STATUS_SUCCESS),
                    Err(status) => InvokeReply::Status(status),
                }
            }
            CMD_KEY_SET_READ_ALL_INDICES => {
                // 引数は空 struct（読まない）。
                let mut ids = vec![IPK_KEY_SET_ID];
                ids.extend(self.store.keyset_ids_for(ctx.fabric_index));
                let mut w = Writer::new();
                w.start_struct(Tag::Anonymous);
                w.start_array(Tag::Context(0));
                for id in ids {
                    w.put_uint(Tag::Anonymous, u64::from(id));
                }
                w.end_container();
                w.end_container();
                InvokeReply::Data {
                    response_command: RESP_KEY_SET_READ_ALL_INDICES,
                    fields_tlv: w.finish(),
                }
            }
            _ => unreachable!("guarded by the matches! above"),
        }
    }

    fn accepted_commands(&self) -> Vec<u32> {
        vec![
            im::CMD_KEY_SET_WRITE,
            CMD_KEY_SET_READ,
            CMD_KEY_SET_REMOVE,
            CMD_KEY_SET_READ_ALL_INDICES,
        ]
    }

    fn generated_commands(&self) -> Vec<u32> {
        vec![RESP_KEY_SET_READ, RESP_KEY_SET_READ_ALL_INDICES]
    }

    /// write の対象は `ATTR_GROUP_KEY_MAP` のみ（`ATTR_GROUP_TABLE` は
    /// spec 上も read-only）。パターンは `AccessControlHandler::write`
    /// （access_control.rs）を踏襲: (1) `attribute` ルーティング外は
    /// `STATUS_UNSUPPORTED_WRITE`、(2) `ctx.fabric_index == 0`（PASE）は
    /// `STATUS_UNSUPPORTED_ACCESS`、(3) `list_append` で「全置換」/「1 件
    /// append」を切り替え、どちらも各エントリの `group_id == 0`（無効
    /// GroupId、spec §11.2.7.6）または書き込み fabric に
    /// `keyset_id` の KeySet が無い（`GroupKeyStore::keyset_exists`）場合は
    /// `STATUS_CONSTRAINT_ERROR` — 存在しない keyset への参照を弾く。wire
    /// 形の `fabricIndex`（254）は無視し、常に `ctx.fabric_index` を使う
    /// （`decode_group_key_map_entries`/`decode_single_group_key_map_entry`
    /// はそもそも 254 を読まない — クライアント実装 `im.rs:1373` の
    /// コメントどおり、書く側もこのフィールドを送らない）。
    fn write(
        &mut self,
        attribute: u32,
        data_tlv: &[u8],
        list_append: bool,
        ctx: &mut InvokeCtx,
    ) -> Result<(), u8> {
        if attribute != im::ATTR_GROUP_KEY_MAP {
            return Err(im::STATUS_UNSUPPORTED_WRITE);
        }
        if ctx.fabric_index == 0 {
            return Err(im::STATUS_UNSUPPORTED_ACCESS);
        }
        if list_append {
            let Some(entry) = decode_single_group_key_map_entry(data_tlv) else {
                return Err(im::STATUS_CONSTRAINT_ERROR);
            };
            self.check_map_entry(ctx.fabric_index, &entry)?;
            self.store
                .append_map_entry(ctx.fabric_index, entry.0, entry.1);
        } else {
            let Some(entries) = decode_group_key_map_entries(data_tlv) else {
                return Err(im::STATUS_CONSTRAINT_ERROR);
            };
            for entry in &entries {
                self.check_map_entry(ctx.fabric_index, entry)?;
            }
            self.store.replace_fabric_map(ctx.fabric_index, entries);
        }
        ctx.changed.push(im::ATTR_GROUP_KEY_MAP);
        Ok(())
    }

    /// spec §11.2.5 のアクセス表: KeySet 系コマンド（この実装が受理する
    /// `CMD_KEY_SET_WRITE`/`CMD_KEY_SET_READ`/`CMD_KEY_SET_REMOVE`/
    /// `CMD_KEY_SET_READ_ALL_INDICES` の 4 つ全部）は Administer — group key
    /// は fabric の共有秘密そのものなので、Operate 権限の controller には
    /// 書かせない（Read も同様: 鍵は返さないが keyset の存在自体を隠す）。
    fn invoke_privilege(&self, _command: u32) -> u8 {
        crate::core::access_control::PRIVILEGE_ADMINISTER
    }

    /// `ATTR_GROUP_KEY_MAP` の write は Manage（read は View のまま =
    /// trait default）。書ける属性はこれだけ。
    fn write_privilege(&self, _attribute: u32) -> u8 {
        crate::core::access_control::PRIVILEGE_MANAGE
    }
}

/// `decode_key_set_write_fields`の失敗理由。TLV ストリーム自体が壊れて
/// いる／期待した shape（struct/struct のネスト）と食い違うのが
/// `Malformed`、shape は正しいが必須フィールド欠落・値制約違反なのが
/// `Constraint` — `invoke` がこの区別をそのまま IM ステータスに割り付ける
/// （設計メモ参照）。
enum KeySetWriteError {
    Malformed,
    Constraint,
}

/// KeySetWrite の CommandFields（クライアント実装
/// `mat_controller::im::encode_key_set_write_fields`が wire 形の正 —
/// `struct{ Context(0): GroupKeySetStruct{ 0:GroupKeySetID(u16),
/// 1:GroupKeySecurityPolicy(u8, TrustFirst=0), 2:EpochKey0(16B octstr),
/// 3:EpochStartTime0(u64), 4..7:null } }`）をデコードする。epoch 1/2 は
/// この実装では未対応で読み捨てるが、`EpochStartTime0` は保存して
/// KeySetRead が返す。無ければ 0（spec 上は必須だが互換のため緩く）。
fn decode_key_set_write_fields(data: &[u8]) -> Result<(u16, [u8; 16], u64), KeySetWriteError> {
    let mut r = Reader::new(data);
    let el = next_element(&mut r)?;
    if el.value != Value::StructStart {
        return Err(KeySetWriteError::Malformed);
    }

    let mut keyset_id = None;
    let mut policy = None;
    let mut epoch_key0: Option<Vec<u8>> = None;
    let mut epoch_start_time0 = None;

    loop {
        let el = next_element(&mut r)?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::StructStart) => loop {
                let inner = next_element(&mut r)?;
                match (inner.tag, inner.value) {
                    (_, Value::ContainerEnd) => break,
                    (Tag::Context(0), Value::Uint(v)) => keyset_id = u16::try_from(v).ok(),
                    (Tag::Context(1), Value::Uint(v)) => policy = u8::try_from(v).ok(),
                    (Tag::Context(2), Value::Bytes(b)) => epoch_key0 = Some(b.to_vec()),
                    (Tag::Context(3), Value::Uint(v)) => epoch_start_time0 = Some(v),
                    (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                        mat_controller::tlv::skip_container(&mut r)
                            .map_err(|_| KeySetWriteError::Malformed)?;
                    }
                    _ => {}
                }
            },
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                mat_controller::tlv::skip_container(&mut r)
                    .map_err(|_| KeySetWriteError::Malformed)?;
            }
            _ => {}
        }
    }

    let keyset_id = keyset_id.ok_or(KeySetWriteError::Constraint)?;
    let policy = policy.ok_or(KeySetWriteError::Constraint)?;
    let epoch_key0 = epoch_key0.ok_or(KeySetWriteError::Constraint)?;
    if policy != 0 {
        return Err(KeySetWriteError::Constraint);
    }
    let epoch_key0: [u8; 16] = epoch_key0
        .try_into()
        .map_err(|_| KeySetWriteError::Constraint)?;
    let epoch_start_time0 = epoch_start_time0.unwrap_or(0);
    Ok((keyset_id, epoch_key0, epoch_start_time0))
}

/// `Reader::next()` の `Result<Option<Element>, TlvError>` を「読めない・
/// もう要素がない」のどちらも `Malformed` に畳む — KeySetWrite のフィールド
/// 走査はいつも次の要素があることを前提にしている（`StructStart`は必ず
/// 対応する `ContainerEnd` で閉じる wire 形なので、途中で尽きるのは
/// 破損データのみ）。
fn next_element<'a>(
    r: &mut Reader<'a>,
) -> Result<mat_controller::tlv::Element<'a>, KeySetWriteError> {
    r.next()
        .map_err(|_| KeySetWriteError::Malformed)?
        .ok_or(KeySetWriteError::Malformed)
}

/// `ATTR_GROUP_KEY_MAP` の read: `(fabric_index, group_id, keyset_id)`
/// 列を `array[GroupKeyMapStruct{ Context(1)=GroupId, Context(2)=
/// GroupKeySetID, Context(254)=FabricIndex }]` に符号化する（fabricIndex
/// 出力の流儀は `access_control.rs::write_acl_entry` の Context(254) と
/// 同じ）。
fn encode_group_key_map(entries: &[(u8, u16, u16)]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    for (fabric_index, group_id, keyset_id) in entries {
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(1), u64::from(*group_id));
        w.put_uint(Tag::Context(2), u64::from(*keyset_id));
        w.put_uint(Tag::Context(254), u64::from(*fabric_index));
        w.end_container();
    }
    w.end_container();
    w.finish()
}

/// `ATTR_GROUP_KEY_MAP` write の全置換径路: `data_tlv` は
/// `array[GroupKeyMapStruct{1:GroupId(u16), 2:GroupKeySetID(u16)}]`
/// （wire 形はクライアント実装
/// `mat_controller::im::encode_group_key_map_tlv` が正 — `fabricIndex`
/// (254) は送られてこない前提で、来ても `decode_group_key_map_entry_body`
/// が無視する）。構造が array/struct のネストと食い違う、または必須
/// フィールド（GroupId/GroupKeySetID のどちらか）が欠けていれば `None`
/// — `write` はこれを一律 `STATUS_CONSTRAINT_ERROR` に畳む
/// （`decode_acl_entries`/`AccessControlHandler::write`と同じ裁定）。
fn decode_group_key_map_entries(data_tlv: &[u8]) -> Option<Vec<(u16, u16)>> {
    let mut r = Reader::new(data_tlv);
    let el = r.next().ok()??;
    if el.value != Value::ArrayStart {
        return None;
    }
    let mut entries = Vec::new();
    loop {
        let el = r.next().ok()??;
        match el.value {
            Value::ContainerEnd => break,
            Value::StructStart => entries.push(decode_group_key_map_entry_body(&mut r)?),
            _ => return None,
        }
    }
    Some(entries)
}

/// write の `ListIndex` null append 径路: `data_tlv` は単一の
/// `GroupKeyMapStruct`（array に包まれない）。
fn decode_single_group_key_map_entry(data_tlv: &[u8]) -> Option<(u16, u16)> {
    let mut r = Reader::new(data_tlv);
    let el = r.next().ok()??;
    if el.value != Value::StructStart {
        return None;
    }
    decode_group_key_map_entry_body(&mut r)
}

/// `GroupKeyMapStruct` 1 件分のフィールド列を読む。呼び出し側がその
/// `StructStart` を消費済みであることが前提。`Context(1)`=GroupId,
/// `Context(2)`=GroupKeySetID 以外（`fabricIndex`含む）は読み捨てる。
fn decode_group_key_map_entry_body(r: &mut Reader) -> Option<(u16, u16)> {
    let mut group_id = None;
    let mut keyset_id = None;
    loop {
        let el = r.next().ok()??;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::Uint(v)) => group_id = u16::try_from(v).ok(),
            (Tag::Context(2), Value::Uint(v)) => keyset_id = u16::try_from(v).ok(),
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                mat_controller::tlv::skip_container(r).ok()?;
            }
            _ => {}
        }
    }
    Some((group_id?, keyset_id?))
}

/// KeySetRead / KeySetRemove の `{0: GroupKeySetID}`。`groups.rs::decode_group_id`
/// と同じ形（先頭 struct の Context(0) uint、ネストは読み飛ばす）。形不正・
/// id 欠落は `None` → 呼び出し側は `STATUS_INVALID_COMMAND`。
fn decode_key_set_id(fields_tlv: &[u8]) -> Option<u16> {
    let mut r = Reader::new(fields_tlv);
    match r.next() {
        Ok(Some(el)) if el.value == Value::StructStart => {}
        _ => return None,
    }
    let mut keyset_id = None;
    loop {
        match r.next() {
            Ok(Some(el)) => match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(0), Value::Uint(v)) => keyset_id = u16::try_from(v).ok(),
                (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                    mat_controller::tlv::skip_container(&mut r).ok()?;
                }
                _ => {}
            },
            _ => return None,
        }
    }
    keyset_id
}

/// `KeySetReadResponse {0: GroupKeySetStruct}`（spec §11.2.8.3）。EpochKey0/1/2
/// は**必ず null**（鍵素材は返さない）、policy は TrustFirst(0) 固定（他は
/// KeySetWrite で拒む）、epoch 1/2 の StartTime も null（未対応）。
fn encode_key_set_read_response(keyset_id: u16, epoch_start_time0: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.start_struct(Tag::Context(0));
    w.put_uint(Tag::Context(0), u64::from(keyset_id));
    w.put_uint(Tag::Context(1), 0);
    w.put_null(Tag::Context(2));
    w.put_uint(Tag::Context(3), epoch_start_time0);
    w.put_null(Tag::Context(4));
    w.put_null(Tag::Context(5));
    w.put_null(Tag::Context(6));
    w.put_null(Tag::Context(7));
    w.end_container();
    w.end_container();
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mat_controller::tlv::{Reader, Value};

    fn read(h: &GroupKeyManagementHandler, attribute: u32) -> Vec<u8> {
        h.read(attribute, &ReadCtx::default())
            .expect("attribute implemented")
    }

    /// `GroupTable` の array を `(fabric_index, group_id, endpoints)` に戻す。
    fn decode_group_table(tlv: &[u8]) -> Vec<(u8, u16, Vec<u16>)> {
        let mut r = Reader::new(tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::ArrayStart);
        let mut out = Vec::new();
        loop {
            let el = r.next().unwrap().unwrap();
            match el.value {
                Value::ContainerEnd => break,
                Value::StructStart => {
                    let (mut fabric, mut group, mut eps) = (0u8, 0u16, Vec::new());
                    loop {
                        let e = r.next().unwrap().unwrap();
                        match (e.tag, e.value) {
                            (_, Value::ContainerEnd) => break,
                            (Tag::Context(1), Value::Uint(v)) => group = v as u16,
                            (Tag::Context(2), Value::ArrayStart) => loop {
                                let x = r.next().unwrap().unwrap();
                                match x.value {
                                    Value::ContainerEnd => break,
                                    Value::Uint(v) => eps.push(v as u16),
                                    other => panic!("unexpected {other:?}"),
                                }
                            },
                            (Tag::Context(254), Value::Uint(v)) => fabric = v as u8,
                            _ => {}
                        }
                    }
                    out.push((fabric, group, eps));
                }
                other => panic!("unexpected {other:?}"),
            }
        }
        out
    }

    #[test]
    fn group_table_is_derived_from_membership_and_fabric_filtered() {
        let membership = crate::core::group_membership::GroupMembershipStore::new();
        membership.add(1, 10, 2).unwrap();
        membership.add(1, 10, 3).unwrap();
        membership.add(2, 20, 2).unwrap();
        let h = GroupKeyManagementHandler::new(GroupKeyStore::new(), membership);
        let filtered = h
            .read(
                im::ATTR_GROUP_TABLE,
                &ReadCtx {
                    fabric_index: 1,
                    fabric_filtered: true,
                    ..ReadCtx::default()
                },
            )
            .unwrap();
        assert_eq!(decode_group_table(&filtered), vec![(1, 10, vec![2, 3])]);
        let all = h
            .read(im::ATTR_GROUP_TABLE, &ReadCtx::unfiltered(1))
            .unwrap();
        assert_eq!(
            decode_group_table(&all),
            vec![(1, 10, vec![2, 3]), (2, 20, vec![2])]
        );
    }

    #[test]
    fn declares_attributes_and_key_set_write_command() {
        let h = GroupKeyManagementHandler::new(GroupKeyStore::new(), GroupMembershipStore::new());
        assert_eq!(
            h.attributes(),
            vec![
                im::ATTR_GROUP_KEY_MAP,
                im::ATTR_GROUP_TABLE,
                im::ATTR_MAX_GROUPS_PER_FABRIC,
                im::ATTR_MAX_GROUP_KEYS_PER_FABRIC,
            ]
        );
        assert_eq!(h.feature_map(), 0);
    }

    #[test]
    fn declares_all_key_set_commands() {
        let h = GroupKeyManagementHandler::new(GroupKeyStore::new(), GroupMembershipStore::new());
        assert_eq!(
            h.accepted_commands(),
            vec![
                im::CMD_KEY_SET_WRITE,
                CMD_KEY_SET_READ,
                CMD_KEY_SET_REMOVE,
                CMD_KEY_SET_READ_ALL_INDICES
            ]
        );
        assert_eq!(
            h.generated_commands(),
            vec![RESP_KEY_SET_READ, RESP_KEY_SET_READ_ALL_INDICES]
        );
    }

    fn write_keyset(h: &mut GroupKeyManagementHandler, fabric: u8, id: u16) -> InvokeCtx {
        let mut ctx = InvokeCtx {
            fabric_index: fabric,
            ..Default::default()
        };
        let ks = mat_controller::im::encode_key_set_write_fields(id, &[9u8; 16]);
        assert_eq!(
            h.invoke(im::CMD_KEY_SET_WRITE, &ks, &mut ctx),
            InvokeReply::Status(im::STATUS_SUCCESS)
        );
        ctx
    }

    fn key_set_id_fields(id: u16) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), u64::from(id));
        w.end_container();
        w.finish()
    }

    /// `KeySetReadResponse {0: GroupKeySetStruct}` を `(id, policy,
    /// start_time0)` に戻す。EpochKey0/1/2 (2/4/6) が null 以外なら `None`
    /// — 鍵素材が漏れたら失敗させる。
    fn decode_key_set_read_response(fields: &[u8]) -> Option<(u16, u8, Option<u64>)> {
        let mut r = Reader::new(fields);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let el = r.next().unwrap().unwrap();
        assert_eq!((el.tag, el.value), (Tag::Context(0), Value::StructStart));
        let (mut id, mut policy, mut start) = (None, None, None);
        loop {
            let el = r.next().unwrap().unwrap();
            match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(0), Value::Uint(v)) => id = Some(v as u16),
                (Tag::Context(1), Value::Uint(v)) => policy = Some(v as u8),
                (Tag::Context(3), Value::Uint(v)) => start = Some(v),
                (Tag::Context(2 | 4 | 6), Value::Null) => {}
                (Tag::Context(5 | 7), Value::Null) => {}
                (Tag::Context(2 | 4 | 6), _) => return None,
                other => panic!("unexpected field {other:?}"),
            }
        }
        Some((id?, policy?, start))
    }

    #[test]
    fn key_set_read_returns_metadata_but_never_the_key() {
        let store = GroupKeyStore::new();
        let mut h = GroupKeyManagementHandler::new(store.clone(), GroupMembershipStore::new());
        let mut ctx = write_keyset(&mut h, 1, 42);
        let reply = h.invoke(CMD_KEY_SET_READ, &key_set_id_fields(42), &mut ctx);
        let InvokeReply::Data {
            response_command,
            fields_tlv,
        } = reply
        else {
            panic!("expected data reply, got {reply:?}");
        };
        assert_eq!(response_command, RESP_KEY_SET_READ);
        let start = store.find_keyset(1, 42).unwrap().epoch_start_time0;
        assert_eq!(
            decode_key_set_read_response(&fields_tlv),
            Some((42, 0, Some(start)))
        );
        // 鍵バイト列が応答に一切含まれない
        assert!(!fields_tlv.windows(16).any(|w| w == [9u8; 16]));
    }

    #[test]
    fn key_set_read_of_ipk_is_virtual_and_unknown_is_not_found() {
        let mut h =
            GroupKeyManagementHandler::new(GroupKeyStore::new(), GroupMembershipStore::new());
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            ..Default::default()
        };
        let InvokeReply::Data { fields_tlv, .. } =
            h.invoke(CMD_KEY_SET_READ, &key_set_id_fields(0), &mut ctx)
        else {
            panic!("IPK keyset 0 must always be readable");
        };
        assert_eq!(
            decode_key_set_read_response(&fields_tlv),
            Some((0, 0, Some(0)))
        );
        assert_eq!(
            h.invoke(CMD_KEY_SET_READ, &key_set_id_fields(42), &mut ctx),
            InvokeReply::Status(im::STATUS_NOT_FOUND)
        );
        // 他 fabric の keyset は見えない
        write_keyset(&mut h, 2, 42);
        assert_eq!(
            h.invoke(CMD_KEY_SET_READ, &key_set_id_fields(42), &mut ctx),
            InvokeReply::Status(im::STATUS_NOT_FOUND)
        );
    }

    #[test]
    fn key_set_commands_reject_pase_and_malformed_fields() {
        let mut h =
            GroupKeyManagementHandler::new(GroupKeyStore::new(), GroupMembershipStore::new());
        let mut pase = InvokeCtx::default();
        for cmd in [
            CMD_KEY_SET_READ,
            CMD_KEY_SET_REMOVE,
            CMD_KEY_SET_READ_ALL_INDICES,
        ] {
            assert_eq!(
                h.invoke(cmd, &key_set_id_fields(1), &mut pase),
                InvokeReply::Status(im::STATUS_UNSUPPORTED_ACCESS),
                "cmd {cmd:#x}"
            );
        }
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            ..Default::default()
        };
        for cmd in [CMD_KEY_SET_READ, CMD_KEY_SET_REMOVE] {
            assert_eq!(
                h.invoke(cmd, &[0xFF, 0x00], &mut ctx),
                InvokeReply::Status(im::STATUS_INVALID_COMMAND),
                "cmd {cmd:#x}"
            );
            // id 欠落（空 struct）
            let mut w = Writer::new();
            w.start_struct(Tag::Anonymous);
            w.end_container();
            assert_eq!(
                h.invoke(cmd, &w.finish(), &mut ctx),
                InvokeReply::Status(im::STATUS_INVALID_COMMAND),
                "cmd {cmd:#x}"
            );
        }
    }

    #[test]
    fn key_set_remove_cascades_to_map_and_marks_the_attribute_changed() {
        let store = GroupKeyStore::new();
        let mut h = GroupKeyManagementHandler::new(store.clone(), GroupMembershipStore::new());
        let mut ctx = write_keyset(&mut h, 1, 42);
        let map = mat_controller::im::encode_group_key_map_tlv(&[(0x000A, 42)]);
        h.write(im::ATTR_GROUP_KEY_MAP, &map, false, &mut ctx)
            .unwrap();
        ctx.changed.clear();

        assert_eq!(
            h.invoke(CMD_KEY_SET_REMOVE, &key_set_id_fields(42), &mut ctx),
            InvokeReply::Status(im::STATUS_SUCCESS)
        );
        assert!(!store.keyset_exists(1, 42));
        assert!(store.map_entries_for(1).is_empty());
        assert_eq!(ctx.changed, vec![im::ATTR_GROUP_KEY_MAP]);

        // 参照の無い keyset の削除は changed を積まない
        let mut ctx = write_keyset(&mut h, 1, 43);
        ctx.changed.clear();
        assert_eq!(
            h.invoke(CMD_KEY_SET_REMOVE, &key_set_id_fields(43), &mut ctx),
            InvokeReply::Status(im::STATUS_SUCCESS)
        );
        assert!(ctx.changed.is_empty());
    }

    #[test]
    fn key_set_remove_rejects_ipk_unknown_and_other_fabric() {
        let store = GroupKeyStore::new();
        let mut h = GroupKeyManagementHandler::new(store.clone(), GroupMembershipStore::new());
        let mut ctx = write_keyset(&mut h, 1, 42);
        assert_eq!(
            h.invoke(CMD_KEY_SET_REMOVE, &key_set_id_fields(0), &mut ctx),
            InvokeReply::Status(im::STATUS_INVALID_COMMAND)
        );
        assert_eq!(
            h.invoke(CMD_KEY_SET_REMOVE, &key_set_id_fields(99), &mut ctx),
            InvokeReply::Status(im::STATUS_NOT_FOUND)
        );
        let mut ctx2 = InvokeCtx {
            fabric_index: 2,
            ..Default::default()
        };
        assert_eq!(
            h.invoke(CMD_KEY_SET_REMOVE, &key_set_id_fields(42), &mut ctx2),
            InvokeReply::Status(im::STATUS_NOT_FOUND)
        );
        assert!(store.keyset_exists(1, 42));
    }

    #[test]
    fn key_set_write_rejects_ipk_keyset_zero() {
        let store = GroupKeyStore::new();
        let mut h = GroupKeyManagementHandler::new(store.clone(), GroupMembershipStore::new());
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            ..Default::default()
        };
        let fields = mat_controller::im::encode_key_set_write_fields(0, &[9u8; 16]);
        assert_eq!(
            h.invoke(im::CMD_KEY_SET_WRITE, &fields, &mut ctx),
            InvokeReply::Status(im::STATUS_INVALID_COMMAND)
        );
        assert!(!store.keyset_exists(1, 0));
        assert!(store.keyset_ids_for(1).is_empty());
    }

    fn decode_u16_list_response(fields: &[u8]) -> Vec<u16> {
        let mut r = Reader::new(fields);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let el = r.next().unwrap().unwrap();
        assert_eq!((el.tag, el.value), (Tag::Context(0), Value::ArrayStart));
        let mut out = Vec::new();
        loop {
            match r.next().unwrap().unwrap().value {
                Value::ContainerEnd => break,
                Value::Uint(v) => out.push(v as u16),
                other => panic!("unexpected {other:?}"),
            }
        }
        out
    }

    #[test]
    fn key_set_read_all_indices_lists_ipk_plus_this_fabrics_keysets() {
        let mut h =
            GroupKeyManagementHandler::new(GroupKeyStore::new(), GroupMembershipStore::new());
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            ..Default::default()
        };
        let InvokeReply::Data {
            response_command,
            fields_tlv,
        } = h.invoke(CMD_KEY_SET_READ_ALL_INDICES, &[], &mut ctx)
        else {
            panic!("expected data reply");
        };
        assert_eq!(response_command, RESP_KEY_SET_READ_ALL_INDICES);
        assert_eq!(decode_u16_list_response(&fields_tlv), vec![0]);

        write_keyset(&mut h, 1, 42);
        write_keyset(&mut h, 2, 43);
        let InvokeReply::Data { fields_tlv, .. } =
            h.invoke(CMD_KEY_SET_READ_ALL_INDICES, &[], &mut ctx)
        else {
            panic!("expected data reply");
        };
        assert_eq!(decode_u16_list_response(&fields_tlv), vec![0, 42]);
    }

    #[test]
    fn group_key_map_and_group_table_are_empty_arrays() {
        let h = GroupKeyManagementHandler::new(GroupKeyStore::new(), GroupMembershipStore::new());
        for attr in [im::ATTR_GROUP_KEY_MAP, im::ATTR_GROUP_TABLE] {
            let tlv = read(&h, attr);
            let mut r = Reader::new(&tlv);
            assert_eq!(r.next().unwrap().unwrap().value, Value::ArrayStart);
            assert_eq!(r.next().unwrap().unwrap().value, Value::ContainerEnd);
        }
    }

    #[test]
    fn capacity_attributes_report_fixed_values() {
        let h = GroupKeyManagementHandler::new(GroupKeyStore::new(), GroupMembershipStore::new());
        // Literal brief values (not `MAX_GROUPS_PER_FABRIC`/
        // `MAX_GROUP_KEYS_PER_FABRIC`) so this test still catches a wrong
        // constant value.
        let tlv = read(&h, im::ATTR_MAX_GROUPS_PER_FABRIC);
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(16));

        let tlv = read(&h, im::ATTR_MAX_GROUP_KEYS_PER_FABRIC);
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(1));
    }

    #[test]
    fn unknown_attribute_and_unknown_command_are_rejected() {
        let mut h =
            GroupKeyManagementHandler::new(GroupKeyStore::new(), GroupMembershipStore::new());
        assert!(h.read(0x7777, &ReadCtx::default()).is_none());
        // 0x77 is a command id with no assignment in this cluster — a CASE
        // session (fabric_index != 0) to make sure the rejection is
        // `accepted_commands`, not the PASE guard.
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            ..Default::default()
        };
        assert_eq!(
            h.invoke(0x77, &[], &mut ctx),
            InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
        );
    }

    #[test]
    fn key_set_write_stores_keyset_and_enforces_capacity() {
        let store = GroupKeyStore::new();
        let mut h = GroupKeyManagementHandler::new(store.clone(), GroupMembershipStore::new());
        let fields = mat_controller::im::encode_key_set_write_fields(0x01AA, &[0x11; 16]);
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            ..Default::default()
        };
        assert_eq!(
            h.invoke(im::CMD_KEY_SET_WRITE, &fields, &mut ctx),
            InvokeReply::Status(im::STATUS_SUCCESS)
        );
        assert!(store.keyset_exists(1, 0x01AA));
        // 同一 id への再 write は upsert（容量エラーにならない）
        assert_eq!(
            h.invoke(im::CMD_KEY_SET_WRITE, &fields, &mut ctx),
            InvokeReply::Status(im::STATUS_SUCCESS)
        );
        // 別 id は容量 1 超過
        let fields2 = mat_controller::im::encode_key_set_write_fields(0x01AB, &[0x22; 16]);
        assert_eq!(
            h.invoke(im::CMD_KEY_SET_WRITE, &fields2, &mut ctx),
            InvokeReply::Status(im::STATUS_RESOURCE_EXHAUSTED)
        );
        // 別 fabric は独立
        let mut ctx2 = InvokeCtx {
            fabric_index: 2,
            ..Default::default()
        };
        assert_eq!(
            h.invoke(im::CMD_KEY_SET_WRITE, &fields2, &mut ctx2),
            InvokeReply::Status(im::STATUS_SUCCESS)
        );
    }

    #[test]
    fn key_set_write_rejects_pase_and_malformed() {
        let mut h =
            GroupKeyManagementHandler::new(GroupKeyStore::new(), GroupMembershipStore::new());
        let fields = mat_controller::im::encode_key_set_write_fields(1, &[0u8; 16]);
        let mut pase = InvokeCtx::default(); // fabric_index 0 = PASE
        assert_eq!(
            h.invoke(im::CMD_KEY_SET_WRITE, &fields, &mut pase),
            InvokeReply::Status(im::STATUS_UNSUPPORTED_ACCESS)
        );
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            ..Default::default()
        };
        assert!(matches!(
            h.invoke(im::CMD_KEY_SET_WRITE, &[0xFF, 0x00], &mut ctx),
            InvokeReply::Status(_)
        ));
    }

    #[test]
    fn purge_fabric_drops_that_fabrics_keysets_only() {
        let store = GroupKeyStore::new();
        store.upsert_keyset(1, 10, [0u8; 16], 0).unwrap();
        store.upsert_keyset(2, 20, [0u8; 16], 0).unwrap();
        store.purge_fabric(1);
        assert!(!store.keyset_exists(1, 10));
        assert!(store.keyset_exists(2, 20));
    }

    /// group-key-map write の全置換 + 存在しない keyset 参照の拒否 +
    /// fabric_filtered read（自 fabric のみ・`Context(254)` 付き）を
    /// 一通り確認する（brief Step 1 のテスト）。
    #[test]
    fn group_key_map_write_replace_append_and_fabric_filtered_read() {
        let store = GroupKeyStore::new();
        let mut h = GroupKeyManagementHandler::new(store.clone(), GroupMembershipStore::new());
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            ..Default::default()
        };
        store.upsert_keyset(1, 7, [9u8; 16], 0).unwrap();

        // 全置換 write
        let data = mat_controller::im::encode_group_key_map_tlv(&[(0x000A, 7)]);
        h.write(im::ATTR_GROUP_KEY_MAP, &data, false, &mut ctx)
            .unwrap();
        assert_eq!(ctx.changed, vec![im::ATTR_GROUP_KEY_MAP]);
        assert_eq!(store.map_entries_for(1), vec![(0x000A, 7)]);

        // 存在しない keyset 参照は CONSTRAINT_ERROR（store は変化しない）
        let bad = mat_controller::im::encode_group_key_map_tlv(&[(0x000B, 99)]);
        assert_eq!(
            h.write(im::ATTR_GROUP_KEY_MAP, &bad, false, &mut ctx),
            Err(im::STATUS_CONSTRAINT_ERROR)
        );
        assert_eq!(store.map_entries_for(1), vec![(0x000A, 7)]);

        // fabric_filtered read は自 fabric のみ・fabricIndex(254) 付き
        let read_ctx = ReadCtx {
            fabric_index: 1,
            ..ReadCtx::default()
        };
        let tlv = h.read(im::ATTR_GROUP_KEY_MAP, &read_ctx).unwrap();
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::ArrayStart);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let mut group_id = None;
        let mut keyset_id = None;
        let mut fabric_index = None;
        loop {
            let el = r.next().unwrap().unwrap();
            match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(1), Value::Uint(v)) => group_id = Some(v),
                (Tag::Context(2), Value::Uint(v)) => keyset_id = Some(v),
                (Tag::Context(254), Value::Uint(v)) => fabric_index = Some(v),
                other => panic!("unexpected field {other:?}"),
            }
        }
        assert_eq!(group_id, Some(0x000A));
        assert_eq!(keyset_id, Some(7));
        assert_eq!(fabric_index, Some(1));
        assert_eq!(r.next().unwrap().unwrap().value, Value::ContainerEnd); // array end
        assert!(r.next().unwrap().is_none());
    }

    /// `list_append=true` は単一 struct の追加径路。
    #[test]
    fn group_key_map_write_list_append_adds_one_entry() {
        let store = GroupKeyStore::new();
        let mut h = GroupKeyManagementHandler::new(store.clone(), GroupMembershipStore::new());
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            ..Default::default()
        };
        store.upsert_keyset(1, 7, [9u8; 16], 0).unwrap();

        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(1), 0x000A);
        w.put_uint(Tag::Context(2), 7);
        w.end_container();
        let single = w.finish();

        h.write(im::ATTR_GROUP_KEY_MAP, &single, true, &mut ctx)
            .unwrap();
        assert_eq!(store.map_entries_for(1), vec![(0x000A, 7)]);
    }

    /// `group_id == 0`（無効 GroupId）は keyset の有無に関わらず
    /// CONSTRAINT_ERROR。
    #[test]
    fn group_key_map_write_rejects_group_id_zero() {
        let store = GroupKeyStore::new();
        let mut h = GroupKeyManagementHandler::new(store.clone(), GroupMembershipStore::new());
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            ..Default::default()
        };
        store.upsert_keyset(1, 7, [9u8; 16], 0).unwrap();

        let data = mat_controller::im::encode_group_key_map_tlv(&[(0, 7)]);
        assert_eq!(
            h.write(im::ATTR_GROUP_KEY_MAP, &data, false, &mut ctx),
            Err(im::STATUS_CONSTRAINT_ERROR)
        );
    }

    /// `keyset_exists` は accessing fabric でスコープされる（他 fabric の
    /// keyset は「存在しない」）— fabric 1 に書いた keyset_id を fabric 2 の
    /// group-key-map から参照しようとすると `STATUS_CONSTRAINT_ERROR` に
    /// なることのピン留め。
    #[test]
    fn group_key_map_write_rejects_other_fabrics_keyset() {
        let store = GroupKeyStore::new();
        let mut h = GroupKeyManagementHandler::new(store.clone(), GroupMembershipStore::new());
        store.upsert_keyset(1, 7, [9u8; 16], 0).unwrap();
        assert!(store.keyset_exists(1, 7));

        let mut ctx2 = InvokeCtx {
            fabric_index: 2,
            ..Default::default()
        };
        let data = mat_controller::im::encode_group_key_map_tlv(&[(0x000A, 7)]);
        assert_eq!(
            h.write(im::ATTR_GROUP_KEY_MAP, &data, false, &mut ctx2),
            Err(im::STATUS_CONSTRAINT_ERROR)
        );
        assert!(store.map_entries_for(2).is_empty());
    }

    /// `ATTR_GROUP_TABLE` は read-only — write は `STATUS_UNSUPPORTED_WRITE`。
    /// PASE セッション（fabric_index 0）は `ATTR_GROUP_KEY_MAP` write でも
    /// `STATUS_UNSUPPORTED_ACCESS`。
    #[test]
    fn write_rejects_unsupported_attribute_and_pase() {
        let mut h =
            GroupKeyManagementHandler::new(GroupKeyStore::new(), GroupMembershipStore::new());
        let data = mat_controller::im::encode_group_key_map_tlv(&[]);
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            ..Default::default()
        };
        assert_eq!(
            h.write(im::ATTR_GROUP_TABLE, &data, false, &mut ctx),
            Err(im::STATUS_UNSUPPORTED_WRITE)
        );

        let mut pase = InvokeCtx::default(); // fabric_index 0 = PASE
        assert_eq!(
            h.write(im::ATTR_GROUP_KEY_MAP, &data, false, &mut pase),
            Err(im::STATUS_UNSUPPORTED_ACCESS)
        );
    }

    /// unfiltered read（`IsFabricFiltered=false`）は fabric をまたいで
    /// 全エントリを返す。
    #[test]
    fn group_key_map_read_unfiltered_returns_all_fabrics() {
        let store = GroupKeyStore::new();
        let h = GroupKeyManagementHandler::new(store.clone(), GroupMembershipStore::new());
        store.upsert_keyset(1, 7, [0u8; 16], 0).unwrap();
        store.upsert_keyset(2, 8, [0u8; 16], 0).unwrap();
        store.replace_fabric_map(1, vec![(0x000A, 7)]);
        store.replace_fabric_map(2, vec![(0x000B, 8)]);

        let tlv = h
            .read(im::ATTR_GROUP_KEY_MAP, &ReadCtx::unfiltered(0))
            .unwrap();
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::ArrayStart);
        let mut seen = Vec::new();
        loop {
            let el = r.next().unwrap().unwrap();
            if el.value == Value::ContainerEnd {
                break;
            }
            assert_eq!(el.value, Value::StructStart);
            let mut group_id = None;
            let mut keyset_id = None;
            let mut fabric_index = None;
            loop {
                let el = r.next().unwrap().unwrap();
                match (el.tag, el.value) {
                    (_, Value::ContainerEnd) => break,
                    (Tag::Context(1), Value::Uint(v)) => group_id = Some(v),
                    (Tag::Context(2), Value::Uint(v)) => keyset_id = Some(v),
                    (Tag::Context(254), Value::Uint(v)) => fabric_index = Some(v),
                    other => panic!("unexpected field {other:?}"),
                }
            }
            seen.push((fabric_index.unwrap(), group_id.unwrap(), keyset_id.unwrap()));
        }
        seen.sort();
        assert_eq!(seen, vec![(1, 0x000A, 7), (2, 0x000B, 8)]);
    }

    struct MemPersist(std::sync::Arc<std::sync::Mutex<(Vec<GroupKeySet>, Vec<GroupKeyMapEntry>)>>);
    impl GroupKeyPersist for MemPersist {
        fn save(&self, keysets: &[GroupKeySet], map: &[GroupKeyMapEntry]) -> Result<(), String> {
            *self.0.lock().unwrap() = (keysets.to_vec(), map.to_vec());
            Ok(())
        }
        fn load(&self) -> Result<(Vec<GroupKeySet>, Vec<GroupKeyMapEntry>), String> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    /// 変異 4 種（upsert / replace map / append map / purge）が毎回 save され、
    /// 別インスタンスが load で同じ状態に戻る。
    #[test]
    fn mutations_persist_and_reload_in_a_new_instance() {
        let cell = std::sync::Arc::new(std::sync::Mutex::new((Vec::new(), Vec::new())));
        {
            let store = GroupKeyStore::with_persist(Box::new(MemPersist(cell.clone())));
            store.upsert_keyset(1, 42, [7u8; 16], 0).unwrap();
            store.replace_fabric_map(1, vec![(10, 42)]);
            store.append_map_entry(1, 11, 42);
            store.upsert_keyset(2, 9, [1u8; 16], 0).unwrap();
            store.purge_fabric(2);
            store.upsert_keyset(3, 5, [2u8; 16], 0).unwrap();
            store.replace_fabric_map(3, vec![(30, 5)]);
            store.remove_keyset(3, 5).unwrap();
        }
        let store2 = GroupKeyStore::with_persist(Box::new(MemPersist(cell)));
        assert_eq!(
            store2.keysets(),
            vec![GroupKeySet {
                fabric_index: 1,
                keyset_id: 42,
                epoch_key0: [7u8; 16],
                epoch_start_time0: 0
            }]
        );
        assert_eq!(store2.map_entries_for(1), vec![(10, 42), (11, 42)]);
        assert!(store2.map_entries_for(2).is_empty());
        assert!(!store2.keyset_exists(3, 5));
        assert!(store2.map_entries_for(3).is_empty());
    }

    struct FailingPersist;
    impl GroupKeyPersist for FailingPersist {
        fn save(&self, _: &[GroupKeySet], _: &[GroupKeyMapEntry]) -> Result<(), String> {
            Err("disk full".into())
        }
        fn load(&self) -> Result<(Vec<GroupKeySet>, Vec<GroupKeyMapEntry>), String> {
            Err("corrupt".into())
        }
    }

    /// load 失敗は空から開始、save 失敗は in-memory を進める（AclStore と同じ）。
    #[test]
    fn persist_failures_do_not_block_the_store() {
        let store = GroupKeyStore::with_persist(Box::new(FailingPersist));
        assert!(store.keysets().is_empty());
        store.upsert_keyset(1, 42, [7u8; 16], 0).unwrap();
        assert!(store.keyset_exists(1, 42));
    }

    #[test]
    fn keyset_debug_redacts_the_epoch_key() {
        let s = format!(
            "{:?}",
            GroupKeySet {
                fabric_index: 1,
                keyset_id: 42,
                epoch_key0: [0xAB; 16],
                epoch_start_time0: 0
            }
        );
        // Note: a plain `!s.contains("ab")` would false-positive on the
        // `fabric_index` field label itself (unrelated to the redacted
        // key), so this checks only for a decimal leak of the 0xAB byte
        // value (171) — the epoch key is fully redacted either way (no
        // digits of it appear in `s` at all).
        assert!(s.contains("REDACTED") && !s.contains("171"), "{s}");
    }

    #[test]
    fn remove_keyset_drops_keyset_and_referencing_map_rows_of_that_fabric_only() {
        let store = GroupKeyStore::new();
        store.upsert_keyset(1, 42, [7u8; 16], 0).unwrap();
        store.replace_fabric_map(1, vec![(10, 42), (11, 42)]);
        store.upsert_keyset(2, 42, [8u8; 16], 0).unwrap();
        store.replace_fabric_map(2, vec![(20, 42)]);

        assert_eq!(
            store.remove_keyset(1, 42),
            Ok(true),
            "map rows referenced it"
        );
        assert!(!store.keyset_exists(1, 42));
        assert!(store.map_entries_for(1).is_empty());
        // 他 fabric は無傷
        assert!(store.keyset_exists(2, 42));
        assert_eq!(store.map_entries_for(2), vec![(20, 42)]);
        // 2 回目は NOT_FOUND
        assert_eq!(store.remove_keyset(1, 42), Err(im::STATUS_NOT_FOUND));
    }

    #[test]
    fn remove_keyset_reports_no_map_change_when_unreferenced() {
        let store = GroupKeyStore::new();
        store.upsert_keyset(1, 42, [7u8; 16], 0).unwrap();
        assert_eq!(store.remove_keyset(1, 42), Ok(false));
        assert!(store.keysets().is_empty());
    }

    #[test]
    fn keyset_ids_and_find_are_fabric_scoped_and_keep_start_time() {
        let store = GroupKeyStore::new();
        store
            .upsert_keyset(1, 42, [7u8; 16], 1_700_000_000_000)
            .unwrap();
        store.upsert_keyset(2, 43, [8u8; 16], 5).unwrap();
        assert_eq!(store.keyset_ids_for(1), vec![42]);
        assert_eq!(store.keyset_ids_for(2), vec![43]);
        assert!(store.keyset_ids_for(3).is_empty());
        let ks = store.find_keyset(1, 42).unwrap();
        assert_eq!(ks.epoch_start_time0, 1_700_000_000_000);
        assert!(store.find_keyset(2, 42).is_none());
        // upsert は start time も置換する
        store.upsert_keyset(1, 42, [9u8; 16], 77).unwrap();
        assert_eq!(store.find_keyset(1, 42).unwrap().epoch_start_time0, 77);
    }

    /// `epoch_start_time0` を持たない旧 `group_keys.json`（v1.31.0 以前）は
    /// 0 として読める（`#[serde(default)]`）。
    #[test]
    fn keyset_json_without_start_time_loads_as_zero() {
        let json =
            r#"{"fabric_index":1,"keyset_id":42,"epoch_key0":[7,7,7,7,7,7,7,7,7,7,7,7,7,7,7,7]}"#;
        let ks: GroupKeySet = serde_json::from_str(json).unwrap();
        assert_eq!(ks.epoch_start_time0, 0);
        assert_eq!(ks.keyset_id, 42);
    }

    #[test]
    fn key_set_write_stores_epoch_start_time0() {
        let store = GroupKeyStore::new();
        let mut h = GroupKeyManagementHandler::new(store.clone(), GroupMembershipStore::new());
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            ..Default::default()
        };
        // encode_key_set_write_fields は Context(3)=EpochStartTime0 を書く
        // （値はクライアント実装が決める）— ここでは「読み捨てず保存する」
        // ことだけを、手組みの TLV で pin する。
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(0));
        w.put_uint(Tag::Context(0), 0x01AA);
        w.put_uint(Tag::Context(1), 0);
        w.put_bytes(Tag::Context(2), &[0x11; 16]);
        w.put_uint(Tag::Context(3), 123_456);
        w.put_null(Tag::Context(4));
        w.put_null(Tag::Context(5));
        w.put_null(Tag::Context(6));
        w.put_null(Tag::Context(7));
        w.end_container();
        w.end_container();
        assert_eq!(
            h.invoke(im::CMD_KEY_SET_WRITE, &w.finish(), &mut ctx),
            InvokeReply::Status(im::STATUS_SUCCESS)
        );
        assert_eq!(
            store.find_keyset(1, 0x01AA).unwrap().epoch_start_time0,
            123_456
        );
    }
}
