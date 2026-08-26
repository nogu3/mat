//! AccessControl クラスタサーバ (spec §11.1, cluster 0x001F) — Apple Home
//! の interview がコミッショニング直後に必ず読み書きする、fabric スコープ
//! の ACL 帳簿。読みは commissioner 自身の admin エントリの確認、書きは
//! Apple がユーザーロール／自動化用に追加エントリを push する経路。
//!
//! `AclStore` は `AccessControlHandler`（EP0 のクラスタハンドラ）と
//! `CommissioningServer`（AddNOC の自動 admin エントリ、RemoveFabric /
//! fail-safe rollback の purge）の両方から触られる共有state —
//! `Arc<Mutex<..>>` で包み、`Clone` で配線先に配る（`FabricStore` 等
//! 既存の共有 state と同じ形）。永続化は M3 送り: in-memory のみで、
//! 再起動で消える。
use std::sync::{Arc, Mutex};

use mat_controller::im;
use mat_controller::tlv::{Reader, Tag, Value, Writer};

use crate::core::datamodel::{ClusterHandler, InvokeCtx, InvokeReply, ReadCtx};

/// AddNOC (spec §11.17.6.8) が自動発行する admin エントリの
/// privilege/auth_mode 固定値 — Administer(5) / CASE(2)。
const CASE_ADMIN_PRIVILEGE: u8 = 5;
const CASE_ADMIN_AUTH_MODE: u8 = 2;

/// `SubjectsPerAccessControlEntry`/`TargetsPerAccessControlEntry`/
/// `AccessControlEntriesPerFabric` (spec §11.1.5) — 固定値を返すのみで
/// 実容量は追跡しない（M2/M3 のどの経路も限界に近づかない）。
const ACL_SUBJECTS_PER_ENTRY: u64 = 4;
const ACL_TARGETS_PER_ENTRY: u64 = 3;
const ACL_ENTRIES_PER_FABRIC: u64 = 4;

/// デバイス上の 1 ACL エントリ (`AccessControlEntryStruct`, spec
/// §11.1.7.1)。`targets_raw` は Context(4) の値要素をそのまま
/// `Tag::Anonymous` に再タグして保持した raw TLV（`None` = null =
/// ターゲット制限なし）— `TargetStruct` の各フィールド
/// (cluster/endpoint/device_type のいずれも optional/null 可) を
/// mat-device 側で意味的にデコードする必要がないための passthrough。
#[derive(Debug, Clone, PartialEq)]
pub struct AclDeviceEntry {
    pub privilege: u8,
    pub auth_mode: u8,
    pub subjects: Vec<u64>,
    pub targets_raw: Option<Vec<u8>>,
    pub fabric_index: u8,
}

/// in-memory ACL ストア（永続化は M3 送り）。fabric をまたいだ 1 本の
/// `Vec` — エントリ数がたかだか数十のオーダーなので、read/write のたびに
/// 対象 fabric だけ filter/retain する素朴な実装で十分（`FabricStore`と
/// 同格の単純さ）。
#[derive(Clone, Default)]
pub struct AclStore(Arc<Mutex<Vec<AclDeviceEntry>>>);

impl AclStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// AddNOC (spec §11.17.6.8) の自動 admin エントリ: 新規 fabric の
    /// CASE admin subject に Administer 権限、ターゲット制限なしのエントリ
    /// を 1 件追加する（このエントリを ACL に自動で持たない device は、
    /// コミッショナ自身が二度と自分の書いた ACL を読み書きできなくなる）。
    pub fn add_case_admin(&self, fabric_index: u8, case_admin_subject: u64) {
        self.lock().push(AclDeviceEntry {
            privilege: CASE_ADMIN_PRIVILEGE,
            auth_mode: CASE_ADMIN_AUTH_MODE,
            subjects: vec![case_admin_subject],
            targets_raw: None,
            fabric_index,
        });
    }

    /// fabric 撤去時の purge（RemoveFabric / fail-safe rollback 共用）。
    pub fn purge_fabric(&self, fabric_index: u8) {
        self.lock().retain(|e| e.fabric_index != fabric_index);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<AclDeviceEntry>> {
        self.0.lock().expect("acl store mutex poisoned")
    }

    fn entries_for(&self, fabric_index: u8) -> Vec<AclDeviceEntry> {
        self.lock()
            .iter()
            .filter(|e| e.fabric_index == fabric_index)
            .cloned()
            .collect()
    }

    /// 書き込み fabric のエントリを丸ごと入れ替える（write の全置換径路）。
    fn replace_fabric(&self, fabric_index: u8, entries: Vec<AclDeviceEntry>) {
        let mut guard = self.lock();
        guard.retain(|e| e.fabric_index != fabric_index);
        guard.extend(entries);
    }

    /// 書き込み fabric の末尾に 1 件足す（write の ListIndex null append
    /// 径路）。
    fn append_to_fabric(&self, entry: AclDeviceEntry) {
        self.lock().push(entry);
    }
}

pub struct AccessControlHandler {
    store: AclStore,
}

impl AccessControlHandler {
    pub fn new(store: AclStore) -> Self {
        Self { store }
    }
}

impl ClusterHandler for AccessControlHandler {
    fn cluster_id(&self) -> u32 {
        im::CLUSTER_ACCESS_CONTROL
    }

    /// ClusterRevision (spec §7.13): Access Control cluster spec revision 2
    /// (Matter 1.4).
    fn revision(&self) -> u16 {
        2
    }

    fn attributes(&self) -> Vec<u32> {
        vec![
            im::ATTR_ACL,
            im::ATTR_ACL_SUBJECTS_PER_ENTRY,
            im::ATTR_ACL_TARGETS_PER_ENTRY,
            im::ATTR_ACL_ENTRIES_PER_FABRIC,
        ]
    }

    fn read(&self, attribute: u32, ctx: &ReadCtx) -> Option<Vec<u8>> {
        match attribute {
            // fabric-scoped (spec §11.1.5.1): 0 (PASE — no fabric yet) は
            // 何とも一致しないので自然に空リストになる。
            im::ATTR_ACL => Some(encode_acl_entries(
                &self.store.entries_for(ctx.fabric_index),
            )),
            im::ATTR_ACL_SUBJECTS_PER_ENTRY => Some(uint_value(ACL_SUBJECTS_PER_ENTRY)),
            im::ATTR_ACL_TARGETS_PER_ENTRY => Some(uint_value(ACL_TARGETS_PER_ENTRY)),
            im::ATTR_ACL_ENTRIES_PER_FABRIC => Some(uint_value(ACL_ENTRIES_PER_FABRIC)),
            _ => None,
        }
    }

    fn invoke(&mut self, _command: u32, _fields_tlv: &[u8], _ctx: &mut InvokeCtx) -> InvokeReply {
        // ReviewFabricRestrictions (spec §11.1.7.6.1) は ARL feature 限定
        // — feature_map()=0 のこの実装には accepted command が無い。
        InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
    }

    /// write の対象は `ATTR_ACL` のみ。`list_append` で「エントリ全置換」
    /// と「1 件 append」を切り替える — どちらも decode 失敗は
    /// `STATUS_CONSTRAINT_ERROR`、書き込み fabric のエントリの
    /// `fabric_index` フィールドは無視して `ctx.fabric_index` で上書きする
    /// （commissioner が書く値を信用しない）。
    fn write(
        &mut self,
        attribute: u32,
        data_tlv: &[u8],
        list_append: bool,
        ctx: &mut InvokeCtx,
    ) -> Result<(), u8> {
        if attribute != im::ATTR_ACL {
            return Err(im::STATUS_UNSUPPORTED_WRITE);
        }
        if list_append {
            let Some(entry) = decode_single_acl_entry(data_tlv) else {
                return Err(im::STATUS_CONSTRAINT_ERROR);
            };
            self.store.append_to_fabric(AclDeviceEntry {
                fabric_index: ctx.fabric_index,
                ..entry
            });
        } else {
            let Some(entries) = decode_acl_entries(data_tlv) else {
                return Err(im::STATUS_CONSTRAINT_ERROR);
            };
            let entries = entries
                .into_iter()
                .map(|e| AclDeviceEntry {
                    fabric_index: ctx.fabric_index,
                    ..e
                })
                .collect();
            self.store.replace_fabric(ctx.fabric_index, entries);
        }
        ctx.changed.push(im::ATTR_ACL);
        Ok(())
    }

    fn feature_map(&self) -> u32 {
        0
    }
}

/// Encodes a scalar as one standalone, `Tag::Anonymous`-tagged TLV element
/// (the `ClusterHandler::read` contract) — same convention as
/// `datamodel::uint_value`, duplicated locally since that one is private to
/// `datamodel`.
fn uint_value(v: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.put_uint(Tag::Anonymous, v);
    w.finish()
}

/// `AccessControlEntryStruct` 列を array の Data TLV へ (spec §11.1.7.1,
/// wire 形は `mat-native::ops::encode_acl_entries_tlv` と完全一致させる):
/// 各要素 struct に `Context(1)=privilege, Context(2)=auth_mode,
/// Context(3)=subjects(array), Context(4)=targets(array|null),
/// Context(254)=fabric_index`。
fn encode_acl_entries(entries: &[AclDeviceEntry]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    for e in entries {
        write_acl_entry(&mut w, e);
    }
    w.end_container();
    w.finish()
}

fn write_acl_entry(w: &mut Writer, entry: &AclDeviceEntry) {
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(1), u64::from(entry.privilege));
    w.put_uint(Tag::Context(2), u64::from(entry.auth_mode));
    w.start_array(Tag::Context(3));
    for s in &entry.subjects {
        w.put_uint(Tag::Anonymous, *s);
    }
    w.end_container();
    match &entry.targets_raw {
        None => w.put_null(Tag::Context(4)),
        Some(raw) => w.put_raw_element(Tag::Context(4), raw),
    }
    w.put_uint(Tag::Context(254), u64::from(entry.fabric_index));
    w.end_container();
}

/// write の全置換径路: `data_tlv` は `AccessControlEntryStruct` の array。
fn decode_acl_entries(data_tlv: &[u8]) -> Option<Vec<AclDeviceEntry>> {
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
            Value::StructStart => entries.push(decode_acl_entry_body(&mut r)?),
            _ => return None,
        }
    }
    Some(entries)
}

/// write の ListIndex null append 径路: `data_tlv` は単一の
/// `AccessControlEntryStruct`（array に包まれない）。
fn decode_single_acl_entry(data_tlv: &[u8]) -> Option<AclDeviceEntry> {
    let mut r = Reader::new(data_tlv);
    let el = r.next().ok()??;
    if el.value != Value::StructStart {
        return None;
    }
    decode_acl_entry_body(&mut r)
}

/// `AccessControlEntryStruct` 1 件分のフィールド列を読む。呼び出し側が
/// その `StructStart` を消費済みであることが前提。`Context(254)` は
/// write 径路では呼び出し側が上書きするため実質無視されるが、
/// read 結果を検証するテストヘルパ（`decode_entries_for_test`）はこの値を
/// そのまま使う。
fn decode_acl_entry_body(r: &mut Reader) -> Option<AclDeviceEntry> {
    let mut privilege = None;
    let mut auth_mode = None;
    let mut subjects = Vec::new();
    let mut targets_raw = None;
    let mut fabric_index = 0u8;
    loop {
        let el = r.next().ok()??;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::Uint(v)) => privilege = u8::try_from(v).ok(),
            (Tag::Context(2), Value::Uint(v)) => auth_mode = u8::try_from(v).ok(),
            (Tag::Context(3), Value::Null) => subjects = Vec::new(),
            (Tag::Context(3), Value::ArrayStart) => loop {
                let inner = r.next().ok()??;
                match inner.value {
                    Value::ContainerEnd => break,
                    Value::Uint(v) => subjects.push(v),
                    _ => return None,
                }
            },
            (Tag::Context(4), Value::Null) => targets_raw = None,
            (Tag::Context(4), Value::ArrayStart) => {
                let mut w = Writer::new();
                copy_element(&mut w, r, Tag::Anonymous, Value::ArrayStart)?;
                targets_raw = Some(w.finish());
            }
            (Tag::Context(254), Value::Uint(v)) => fabric_index = u8::try_from(v).ok()?,
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(r)?;
            }
            _ => {}
        }
    }
    Some(AclDeviceEntry {
        privilege: privilege?,
        auth_mode: auth_mode?,
        subjects,
        targets_raw,
        fabric_index,
    })
}

/// `StructStart`/`ArrayStart`/`ListStart` を既に消費した位置から、対応する
/// `ContainerEnd` まで読み飛ばす（未知のネストしたフィールドを黙って
/// 無視するため — 現状 ACL entry には該当しないが、`case.rs` 等既存の
/// decoder と同じ防御を踏襲）。
fn skip_container(r: &mut Reader) -> Option<()> {
    let mut depth = 1usize;
    while depth > 0 {
        let el = r.next().ok()??;
        match el.value {
            Value::StructStart | Value::ArrayStart | Value::ListStart => depth += 1,
            Value::ContainerEnd => depth -= 1,
            _ => {}
        }
    }
    Some(())
}

/// `mat_controller::tlv::copy_value`/`copy_container` の
/// mat-device 内版（あちらは `pub(crate)` で mat-controller 限定）。
/// `Writer::put_raw_element` が使う deep-copy と同じロジックを、Reader を
/// 読み進めながらその場で行う必要がある箇所（Context(4) の値要素を
/// `Tag::Anonymous` に再タグして丸ごと退避する）向け。
fn copy_element(w: &mut Writer, r: &mut Reader, tag: Tag, value: Value) -> Option<()> {
    match value {
        Value::Int(v) => w.put_int(tag, v),
        Value::Uint(v) => w.put_uint(tag, v),
        Value::Bool(v) => w.put_bool(tag, v),
        Value::F32(v) => w.put_f32(tag, v),
        Value::F64(v) => w.put_f64(tag, v),
        Value::Utf8(v) => w.put_str(tag, v),
        Value::Bytes(v) => w.put_bytes(tag, v),
        Value::Null => w.put_null(tag),
        Value::StructStart => {
            w.start_struct(tag);
            return copy_container(w, r);
        }
        Value::ArrayStart => {
            w.start_array(tag);
            return copy_container(w, r);
        }
        Value::ListStart => {
            w.start_list(tag);
            return copy_container(w, r);
        }
        Value::ContainerEnd => {}
    }
    Some(())
}

fn copy_container(w: &mut Writer, r: &mut Reader) -> Option<()> {
    loop {
        let el = r.next().ok()??;
        if el.value == Value::ContainerEnd {
            w.end_container();
            return Some(());
        }
        copy_element(w, r, el.tag, el.value)?;
    }
}

#[cfg(test)]
/// テスト専用の array-of-entries エンコーダ（`(privilege, auth_mode,
/// subjects)` の 3 要素タプル列 → 全置換 write が送る Data TLV）。
/// targets は常に null、fabric_index は常に 0 で埋める — write 側は
/// どちらも無視する（前者は本テストの関心事ではなく、後者は書き込み
/// fabric で上書きされる）ので、テストの意図を歪めない。
pub(crate) fn encode_entries_for_test(entries: &[(u8, u8, Vec<u64>)]) -> Vec<u8> {
    let device_entries: Vec<AclDeviceEntry> = entries
        .iter()
        .map(|(privilege, auth_mode, subjects)| AclDeviceEntry {
            privilege: *privilege,
            auth_mode: *auth_mode,
            subjects: subjects.clone(),
            targets_raw: None,
            fabric_index: 0,
        })
        .collect();
    encode_acl_entries(&device_entries)
}

#[cfg(test)]
/// テスト専用の単一 struct エンコーダ（`list_append` write が送る Data
/// TLV — array に包まれない 1 件分）。
pub(crate) fn encode_entry_for_test(privilege: u8, auth_mode: u8, subjects: Vec<u64>) -> Vec<u8> {
    let mut w = Writer::new();
    write_acl_entry(
        &mut w,
        &AclDeviceEntry {
            privilege,
            auth_mode,
            subjects,
            targets_raw: None,
            fabric_index: 0,
        },
    );
    w.finish()
}

#[cfg(test)]
/// テスト専用の array-of-entries デコーダ（`read` の戻り値 → `(privilege,
/// auth_mode, subjects, fabric_index)` の 4 要素タプル列）。
pub(crate) fn decode_entries_for_test(tlv: &[u8]) -> Vec<(u8, u8, Vec<u64>, u8)> {
    decode_acl_entries(tlv)
        .expect("well-formed acl read tlv")
        .into_iter()
        .map(|e| (e.privilege, e.auth_mode, e.subjects, e.fabric_index))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `ReadCtx` for a session on `fabric_index`. The ACL attribute is
    /// fabric-scoped in `read` unconditionally (it only ever answers with
    /// the accessing fabric's entries), so `fabric_filtered` doesn't change
    /// what these tests see — leaving it at the default keeps each call site
    /// about the one thing it varies.
    fn read_ctx(fabric_index: u8) -> ReadCtx {
        ReadCtx {
            fabric_index,
            ..ReadCtx::default()
        }
    }

    #[test]
    fn add_noc_style_admin_entry_reads_back_for_its_fabric_only() {
        let store = AclStore::new();
        store.add_case_admin(1, 112233);
        let h = AccessControlHandler::new(store);
        let tlv = h.read(im::ATTR_ACL, &read_ctx(1)).unwrap();
        let entries = decode_entries_for_test(&tlv);
        assert_eq!(entries, vec![(5u8, 2u8, vec![112233u64], 1u8)]);
        // 他 fabric からは空
        let tlv = h.read(im::ATTR_ACL, &read_ctx(2)).unwrap();
        assert!(decode_entries_for_test(&tlv).is_empty());
    }

    #[test]
    fn acl_write_replaces_own_fabric_entries_and_reports_change() {
        let store = AclStore::new();
        store.add_case_admin(1, 112233);
        store.add_case_admin(2, 445566);
        let mut h = AccessControlHandler::new(store.clone());
        // fabric1 が admin+hub の 2 エントリへ全置換（Apple の実書き込みの形）
        let data = encode_entries_for_test(&[(5, 2, vec![112233]), (5, 2, vec![778899])]);
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            ..InvokeCtx::default()
        };
        assert_eq!(h.write(im::ATTR_ACL, &data, false, &mut ctx), Ok(()));
        assert_eq!(ctx.changed, vec![im::ATTR_ACL]);
        let entries = decode_entries_for_test(&h.read(im::ATTR_ACL, &read_ctx(1)).unwrap());
        assert_eq!(entries.len(), 2);
        // fabric2 は無傷
        assert_eq!(
            decode_entries_for_test(&h.read(im::ATTR_ACL, &read_ctx(2)).unwrap()).len(),
            1
        );
    }

    #[test]
    fn purge_fabric_drops_only_that_fabric() {
        let store = AclStore::new();
        store.add_case_admin(1, 111);
        store.add_case_admin(2, 222);
        store.purge_fabric(1);
        let h = AccessControlHandler::new(store);
        assert!(decode_entries_for_test(&h.read(im::ATTR_ACL, &read_ctx(1)).unwrap()).is_empty());
        assert_eq!(
            decode_entries_for_test(&h.read(im::ATTR_ACL, &read_ctx(2)).unwrap()).len(),
            1
        );
    }

    #[test]
    fn malformed_acl_write_is_constraint_error_and_leaves_store_intact() {
        let store = AclStore::new();
        store.add_case_admin(1, 111);
        let mut h = AccessControlHandler::new(store);
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            ..InvokeCtx::default()
        };
        assert_eq!(
            h.write(im::ATTR_ACL, &[], false, &mut ctx),
            Err(im::STATUS_CONSTRAINT_ERROR)
        );
        assert!(ctx.changed.is_empty());
        assert_eq!(
            decode_entries_for_test(&h.read(im::ATTR_ACL, &read_ctx(1)).unwrap()).len(),
            1
        );
    }

    /// Ruling 2: chip 系 controller の「空配列で置換 → ListIndex null の
    /// append を 1 エントリずつ」チャンク列。
    #[test]
    fn list_append_appends_single_entries_after_an_empty_replace() {
        let store = AclStore::new();
        let mut h = AccessControlHandler::new(store);
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            ..InvokeCtx::default()
        };
        // (a) 空配列で置換
        let empty = encode_entries_for_test(&[]);
        assert_eq!(h.write(im::ATTR_ACL, &empty, false, &mut ctx), Ok(()));
        assert_eq!(ctx.changed, vec![im::ATTR_ACL]);
        assert!(decode_entries_for_test(&h.read(im::ATTR_ACL, &read_ctx(1)).unwrap()).is_empty());

        // (b) ListIndex null の append を 1 エントリずつ
        ctx.changed.clear();
        let entry1 = encode_entry_for_test(5, 2, vec![111]);
        assert_eq!(h.write(im::ATTR_ACL, &entry1, true, &mut ctx), Ok(()));
        assert_eq!(ctx.changed, vec![im::ATTR_ACL]);

        ctx.changed.clear();
        let entry2 = encode_entry_for_test(3, 2, vec![222]);
        assert_eq!(h.write(im::ATTR_ACL, &entry2, true, &mut ctx), Ok(()));
        assert_eq!(ctx.changed, vec![im::ATTR_ACL]);

        let entries = decode_entries_for_test(&h.read(im::ATTR_ACL, &read_ctx(1)).unwrap());
        assert_eq!(
            entries,
            vec![(5u8, 2u8, vec![111u64], 1u8), (3u8, 2u8, vec![222u64], 1u8)]
        );
    }
}
