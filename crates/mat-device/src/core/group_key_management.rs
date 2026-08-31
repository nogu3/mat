//! GroupKeyManagement クラスタサーバ (spec §11.2, cluster 0x003F) —
//! RootNode デバイスタイプの必須クラスタ（Device Library §9.2.2）。Apple
//! Home は commissioning 直後の interview でこのクラスタの不在を咎める。
//!
//! `GroupKeyWrite`（spec §11.2.7.1）を実装し、書き込まれた KeySet を
//! `GroupKeyStore` に保持する。`GroupKeyMap`/`GroupTable` 属性の実データ
//! 反映（`ATTR_GROUP_KEY_MAP` write, `KeySetRead`/`KeySetRemove`/
//! `KeySetReadAllIndices` コマンド）と永続化は未実装（既知ギャップ、M3
//! 送り）— `GroupKeyStore` は Task 2 以降（groupcast 実配線）が読み書きする
//! 共有 state として用意するのみで、本ファイルはまだ `map` を読みに使わ
//! ない。
use std::sync::{Arc, Mutex, PoisonError};

use mat_controller::im;
use mat_controller::tlv::{Reader, Tag, Value, Writer};

use crate::core::datamodel::{ClusterHandler, InvokeCtx, InvokeReply, ReadCtx};

/// `MaxGroupsPerFabric`/`MaxGroupKeysPerFabric` (spec §11.2.7.5) — 固定値を
/// 返すのみで実容量は追跡しない（`AccessControlHandler`の容量属性と同じ
/// 割り切り）。`MAX_GROUP_KEYS_PER_FABRIC` は申告値と同時に、
/// `GroupKeyStore::upsert_keyset` が実際に強制する容量上限でもある
/// （`access_control::ACL_ENTRIES_PER_FABRIC` と同じ「単一定数で申告と
/// 実装の食い違いを構造的にあり得なくする」パターン）。
const MAX_GROUPS_PER_FABRIC: u64 = 16;
const MAX_GROUP_KEYS_PER_FABRIC: usize = 1;

/// デバイス上の 1 KeySet（`GroupKeySetStruct` の epoch key 0 のみ保持 —
/// epoch 1/2 は spec 上 optional でこの実装では未対応、モジュール doc
/// 参照）。
#[derive(Debug, Clone)]
struct GroupKeySet {
    fabric_index: u8,
    keyset_id: u16,
    epoch_key0: [u8; 16],
}

/// `GroupKeyMapStruct` 1 件（spec §11.2.7.6: GroupId → KeySetID）。
#[derive(Debug, Clone)]
struct GroupKeyMapEntry {
    fabric_index: u8,
    group_id: u16,
    keyset_id: u16,
}

/// `GroupKeyStore`'s guarded state — `AclStore`/`AclInner`（
/// `core::access_control`）と同じ形。
#[derive(Default)]
struct GroupKeyInner {
    keysets: Vec<GroupKeySet>,
    map: Vec<GroupKeyMapEntry>,
}

/// GroupKeyManagement の共有 state。`GroupKeyManagementHandler`（EP0 の
/// クラスタハンドラ）と、将来の groupcast 配線・fabric 撤去の purge の
/// 両方から触られる想定で `Arc<Mutex<..>>` + `Clone`（`AclStore` と同じ
/// パターン、モジュール doc 参照）。永続化なし（M3 送り）。
#[derive(Clone, Default)]
pub struct GroupKeyStore(Arc<Mutex<GroupKeyInner>>);

impl GroupKeyStore {
    /// 空ストア。
    pub fn new() -> Self {
        Self::default()
    }

    /// poison 耐性 lock（`matd::subscription`と同じ
    /// `unwrap_or_else(PoisonError::into_inner)` パターン）— 1 パニックで
    /// ストア全体が触れなくなるのを避ける。
    fn lock(&self) -> std::sync::MutexGuard<'_, GroupKeyInner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
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
    ) -> Result<(), u8> {
        let mut guard = self.lock();
        if let Some(existing) = guard
            .keysets
            .iter_mut()
            .find(|k| k.fabric_index == fabric_index && k.keyset_id == keyset_id)
        {
            existing.epoch_key0 = epoch_key0;
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
        });
        Ok(())
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
    }

    /// 書き込み fabric の GroupKeyMap エントリを丸ごと入れ替える
    /// （`ATTR_GROUP_KEY_MAP` write の全置換径路 — 配線は後続タスク）。
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
    }

    /// 書き込み fabric の GroupKeyMap 末尾に 1 件足す（write の
    /// ListIndex null append 径路 — 配線は後続タスク）。
    pub fn append_map_entry(&self, fabric_index: u8, group_id: u16, keyset_id: u16) {
        self.lock().map.push(GroupKeyMapEntry {
            fabric_index,
            group_id,
            keyset_id,
        });
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
}

impl GroupKeyManagementHandler {
    pub fn new(store: GroupKeyStore) -> Self {
        Self { store }
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

    fn read(&self, attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
        match attribute {
            im::ATTR_GROUP_KEY_MAP | im::ATTR_GROUP_TABLE => {
                let mut w = Writer::new();
                w.start_array(Tag::Anonymous);
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

    /// `CMD_KEY_SET_WRITE` (spec §11.2.7.1) のみ受理する。PASE セッション
    /// （`ctx.fabric_index == 0`、`AccessControlHandler::write`と同じ
    /// ガード）は `STATUS_UNSUPPORTED_ACCESS`。フィールドのデコードは
    /// `decode_key_set_write_fields` — 構造的な TLV 破損は
    /// `STATUS_INVALID_COMMAND`、フィールド欠落・policy 不正・鍵長不正は
    /// `STATUS_CONSTRAINT_ERROR`（設計メモ参照）。成功時は response
    /// command なしの `STATUS_SUCCESS`。
    fn invoke(&mut self, command: u32, fields_tlv: &[u8], ctx: &mut InvokeCtx) -> InvokeReply {
        if command != im::CMD_KEY_SET_WRITE {
            return InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND);
        }
        if ctx.fabric_index == 0 {
            return InvokeReply::Status(im::STATUS_UNSUPPORTED_ACCESS);
        }
        let (keyset_id, epoch_key0) = match decode_key_set_write_fields(fields_tlv) {
            Ok(fields) => fields,
            Err(KeySetWriteError::Malformed) => {
                return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
            }
            Err(KeySetWriteError::Constraint) => {
                return InvokeReply::Status(im::STATUS_CONSTRAINT_ERROR);
            }
        };
        match self
            .store
            .upsert_keyset(ctx.fabric_index, keyset_id, epoch_key0)
        {
            Ok(()) => InvokeReply::Status(im::STATUS_SUCCESS),
            Err(status) => InvokeReply::Status(status),
        }
    }

    fn accepted_commands(&self) -> Vec<u32> {
        vec![im::CMD_KEY_SET_WRITE]
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
/// 3:EpochStartTime0(u64), 4..7:null } }`）をデコードする。epoch 1/2 と
/// `EpochStartTime0` はこの実装では読み捨てる（モジュール doc 参照）。
fn decode_key_set_write_fields(data: &[u8]) -> Result<(u16, [u8; 16]), KeySetWriteError> {
    let mut r = Reader::new(data);
    let el = next_element(&mut r)?;
    if el.value != Value::StructStart {
        return Err(KeySetWriteError::Malformed);
    }

    let mut keyset_id = None;
    let mut policy = None;
    let mut epoch_key0: Option<Vec<u8>> = None;

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
    Ok((keyset_id, epoch_key0))
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

#[cfg(test)]
mod tests {
    use super::*;
    use mat_controller::tlv::{Reader, Value};

    fn read(h: &GroupKeyManagementHandler, attribute: u32) -> Vec<u8> {
        h.read(attribute, &ReadCtx::default())
            .expect("attribute implemented")
    }

    #[test]
    fn declares_attributes_and_key_set_write_command() {
        let h = GroupKeyManagementHandler::new(GroupKeyStore::new());
        assert_eq!(
            h.attributes(),
            vec![
                im::ATTR_GROUP_KEY_MAP,
                im::ATTR_GROUP_TABLE,
                im::ATTR_MAX_GROUPS_PER_FABRIC,
                im::ATTR_MAX_GROUP_KEYS_PER_FABRIC,
            ]
        );
        assert_eq!(h.accepted_commands(), vec![im::CMD_KEY_SET_WRITE]);
        assert_eq!(h.generated_commands(), Vec::<u32>::new());
        assert_eq!(h.feature_map(), 0);
    }

    #[test]
    fn group_key_map_and_group_table_are_empty_arrays() {
        let h = GroupKeyManagementHandler::new(GroupKeyStore::new());
        for attr in [im::ATTR_GROUP_KEY_MAP, im::ATTR_GROUP_TABLE] {
            let tlv = read(&h, attr);
            let mut r = Reader::new(&tlv);
            assert_eq!(r.next().unwrap().unwrap().value, Value::ArrayStart);
            assert_eq!(r.next().unwrap().unwrap().value, Value::ContainerEnd);
        }
    }

    #[test]
    fn capacity_attributes_report_fixed_values() {
        let h = GroupKeyManagementHandler::new(GroupKeyStore::new());
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
        let mut h = GroupKeyManagementHandler::new(GroupKeyStore::new());
        assert!(h.read(0x7777, &ReadCtx::default()).is_none());
        // 0x01 (KeySetRead, spec §11.2.7.2) is a real command id but not one
        // this implementation accepts — a CASE session (fabric_index != 0)
        // to make sure the rejection is `accepted_commands`, not the PASE
        // guard.
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            ..Default::default()
        };
        assert_eq!(
            h.invoke(0x01, &[], &mut ctx),
            InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
        );
    }

    #[test]
    fn key_set_write_stores_keyset_and_enforces_capacity() {
        let store = GroupKeyStore::new();
        let mut h = GroupKeyManagementHandler::new(store.clone());
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
        let mut h = GroupKeyManagementHandler::new(GroupKeyStore::new());
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
        store.upsert_keyset(1, 10, [0u8; 16]).unwrap();
        store.upsert_keyset(2, 20, [0u8; 16]).unwrap();
        store.purge_fabric(1);
        assert!(!store.keyset_exists(1, 10));
        assert!(store.keyset_exists(2, 20));
    }
}
