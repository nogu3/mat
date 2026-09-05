//! chip-tool 互換 KVS への controller 側 group state 書込（M8c-2）。
//!
//! chip-tool `groupsettings add-group / add-keysets / (unbind-keyset) /
//! bind-keyset` が KVS に残す 5 レコード（g/gfl, f/<i>/g, f/<i>/g/<gid>,
//! f/<i>/gk/<id>, f/<i>/k/<ksid>）を、上流 v1.4.2.0 GroupDataProviderImpl
//! と同じリンク規律（group=末尾挿入・終端0 / keyset=head 挿入・終端0xFFFF、
//! id 0 = IPK は有効値 / keymap=末尾連結・id は max+1 で sparse / 走査は
//! count 正）で書く。1 回の provision は 1 つの KvsTxn（flock 区間）で
//! 読み・変更・commit まで完結する。
//!
//! 上流との意図的な差分は 2 つ: ①リンク切れ・解釈不能レコードは黙って
//! 進まず [`GroupSettingsError::Corrupt`]（不整合ストアを悪化させない）。
//! ②新規 GroupData の first_endpoint は常に kInvalidEndpointId（0xFFFF）
//! —— 上流は直前に走査した他レコードの値が漏れ込むが、endpoint_count=0 の
//! とき読者はこの欄を見ないため互換に影響しない。
//!
//! IPK ローテーション（`begin_ipk_rotation` / `commit_ipk_rotation` /
//! `abort_ipk_rotation`）も同じ 1 KvsTxn 規律で書く。

use std::path::Path;

use crate::fabric::{derive_group_session_id, derive_ipk_operational};
use crate::kvs::{mat_ipk_epoch_slot_key, IpkEpochSlot, KvsError, KvsTxn};
use crate::tlv::{Reader, Tag, Value, Writer};

/// keyset リンクの終端（上流 kInvalidKeysetId — id 0 は IPK で有効値）。
const INVALID_KEYSET_ID: u16 = 0xFFFF;
/// endpoint 無しの GroupData first_endpoint（上流 kInvalidEndpointId）。
const INVALID_ENDPOINT_ID: u16 = 0xFFFF;
/// KeySetData の operational key 配列は常に 3 スロット（KeySet::kEpochKeysMax）。
const KEYSET_SLOTS: usize = 3;
/// デバイス側 epochStartTime0 と一致させる（mat-core::group::EPOCH_START_TIME = "1"）。
pub(crate) const EPOCH_START_TIME: u64 = 1;
/// GroupName の最大バイト数（上流 CHIP_CONFIG_MAX_GROUP_NAME_LENGTH）。
const GROUP_NAME_MAX: usize = 16;

/// group_settings の書込エラー。`Display` に鍵名を残し、AI/オペレータが
/// リカバリを判断できるようにする。
#[derive(Debug)]
pub enum GroupSettingsError {
    /// 既に同じ (group, keyset) の bind がある（chip-tool の
    /// CHIP_ERROR_DUPLICATE_KEY_ID 相当 — `--rebind` で解消する）。
    DuplicateBind {
        group_id: u16,
        keyset_id: u16,
    },
    /// 既存レコードのリンク切れ・解釈不能（書かずに中断）。
    Corrupt {
        key: String,
        reason: &'static str,
    },
    /// 撤収対象の group がコントローラ KVS に無い。
    NotFound {
        group_id: u16,
    },
    Kvs(KvsError),
}

impl std::fmt::Display for GroupSettingsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GroupSettingsError::DuplicateBind {
                group_id,
                keyset_id,
            } => write!(
                f,
                "group_settings: group {group_id} already bound to keyset {keyset_id} (use --rebind)"
            ),
            GroupSettingsError::Corrupt { key, reason } => {
                write!(f, "group_settings key \"{key}\": {reason}")
            }
            GroupSettingsError::NotFound { group_id } => write!(
                f,
                "group_settings: group {group_id} is not provisioned in the controller kvs"
            ),
            GroupSettingsError::Kvs(e) => write!(f, "group_settings: {e}"),
        }
    }
}

impl std::error::Error for GroupSettingsError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            GroupSettingsError::Kvs(e) => Some(e),
            _ => None,
        }
    }
}

impl From<KvsError> for GroupSettingsError {
    fn from(e: KvsError) -> Self {
        GroupSettingsError::Kvs(e)
    }
}

fn corrupt(key: &str, reason: &'static str) -> GroupSettingsError {
    GroupSettingsError::Corrupt {
        key: key.to_string(),
        reason,
    }
}

/// 未知タグを読み飛ばして、現在開いているコンテナ（相対深さ0）の
/// `ContainerEnd` まで消費する（`kvs.rs` の `skip_rest_of_container` と
/// 同じ寛容走査だが、こちらの parser 群は `Option` チェーンで書かれている
/// ので `Option` を返す）。
fn skip_container(r: &mut Reader) -> Option<()> {
    let mut depth: i32 = 0;
    loop {
        let el = r.next().ok()??;
        match el.value {
            Value::StructStart | Value::ArrayStart | Value::ListStart => depth += 1,
            Value::ContainerEnd => {
                if depth == 0 {
                    return Some(());
                }
                depth -= 1;
            }
            _ => {}
        }
    }
}

/// GroupName を `GROUP_NAME_MAX` バイト以内へ char 境界で切り詰める。上流は
/// バイト単位で切るが、ここでは UTF-8 を割らない方へ倒す（chip-tool の
/// group name は基本 ASCII なので互換上の実害はない）。
fn truncate_name(name: &str) -> String {
    let cut = name
        .char_indices()
        .take_while(|(i, c)| i + c.len_utf8() <= GROUP_NAME_MAX)
        .last()
        .map(|(i, c)| i + c.len_utf8())
        .unwrap_or(0);
    name[..cut].to_string()
}

/// `f/<idx>/g` — フラットなフィールドのみの struct（ctx1..7 全て Uint）。
pub(crate) struct FabricData {
    first_group: u16,
    group_count: u16,
    pub(crate) first_map: u16,
    pub(crate) map_count: u16,
    first_keyset: u16,
    keyset_count: u16,
    next: u16,
}

impl FabricData {
    fn empty() -> Self {
        Self {
            first_group: 0,
            group_count: 0,
            first_map: 0,
            map_count: 0,
            first_keyset: INVALID_KEYSET_ID,
            keyset_count: 0,
            next: 0,
        }
    }

    fn serialize(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(1), u64::from(self.first_group));
        w.put_uint(Tag::Context(2), u64::from(self.group_count));
        w.put_uint(Tag::Context(3), u64::from(self.first_map));
        w.put_uint(Tag::Context(4), u64::from(self.map_count));
        w.put_uint(Tag::Context(5), u64::from(self.first_keyset));
        w.put_uint(Tag::Context(6), u64::from(self.keyset_count));
        w.put_uint(Tag::Context(7), u64::from(self.next));
        w.end_container();
        w.finish()
    }
}

pub(crate) fn parse_fabric_data(blob: &[u8]) -> Option<FabricData> {
    let mut r = Reader::new(blob);
    if r.next().ok()??.value != Value::StructStart {
        return None;
    }
    let (mut first_group, mut group_count) = (None, None);
    let (mut first_map, mut map_count) = (None, None);
    let (mut first_keyset, mut keyset_count) = (None, None);
    let mut next = None;
    loop {
        let el = r.next().ok()??;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::Uint(v)) => first_group = u16::try_from(v).ok(),
            (Tag::Context(2), Value::Uint(v)) => group_count = u16::try_from(v).ok(),
            (Tag::Context(3), Value::Uint(v)) => first_map = u16::try_from(v).ok(),
            (Tag::Context(4), Value::Uint(v)) => map_count = u16::try_from(v).ok(),
            (Tag::Context(5), Value::Uint(v)) => first_keyset = u16::try_from(v).ok(),
            (Tag::Context(6), Value::Uint(v)) => keyset_count = u16::try_from(v).ok(),
            (Tag::Context(7), Value::Uint(v)) => next = u16::try_from(v).ok(),
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r)?
            }
            _ => {}
        }
    }
    Some(FabricData {
        first_group: first_group?,
        group_count: group_count?,
        first_map: first_map?,
        map_count: map_count?,
        first_keyset: first_keyset?,
        keyset_count: keyset_count?,
        next: next?,
    })
}

/// `f/<idx>/g/<gid>` — group name / endpoint 情報 / チェーン内 next。
#[derive(Clone)]
struct GroupData {
    name: String,
    first_endpoint: u16,
    endpoint_count: u16,
    next: u16,
}

impl GroupData {
    fn serialize(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_str(Tag::Context(1), &self.name);
        w.put_uint(Tag::Context(2), u64::from(self.first_endpoint));
        w.put_uint(Tag::Context(3), u64::from(self.endpoint_count));
        w.put_uint(Tag::Context(4), u64::from(self.next));
        w.end_container();
        w.finish()
    }
}

fn parse_group_data(blob: &[u8]) -> Option<GroupData> {
    let mut r = Reader::new(blob);
    if r.next().ok()??.value != Value::StructStart {
        return None;
    }
    let (mut name, mut first_endpoint, mut endpoint_count, mut next) = (None, None, None, None);
    loop {
        let el = r.next().ok()??;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::Utf8(s)) => name = Some(s.to_string()),
            (Tag::Context(2), Value::Uint(v)) => first_endpoint = u16::try_from(v).ok(),
            (Tag::Context(3), Value::Uint(v)) => endpoint_count = u16::try_from(v).ok(),
            (Tag::Context(4), Value::Uint(v)) => next = u16::try_from(v).ok(),
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r)?
            }
            _ => {}
        }
    }
    Some(GroupData {
        name: name?,
        first_endpoint: first_endpoint?,
        endpoint_count: endpoint_count?,
        next: next?,
    })
}

/// `f/<idx>/gk/<id>` — GroupKeyMap の1エントリ（group_id, keyset_id,
/// チェーン内 next）。読み側（`kvs::read_group_credentials`）もこの parse を
/// 共有し、first_map→next のチェーン走査で使う。
#[derive(Clone, Copy)]
pub(crate) struct KeyMap {
    pub(crate) group_id: u16,
    pub(crate) keyset_id: u16,
    pub(crate) next: u16,
}

impl KeyMap {
    fn serialize(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(1), u64::from(self.group_id));
        w.put_uint(Tag::Context(2), u64::from(self.keyset_id));
        w.put_uint(Tag::Context(3), u64::from(self.next));
        w.end_container();
        w.finish()
    }
}

pub(crate) fn parse_keymap(blob: &[u8]) -> Option<KeyMap> {
    let mut r = Reader::new(blob);
    if r.next().ok()??.value != Value::StructStart {
        return None;
    }
    let (mut group_id, mut keyset_id, mut next) = (None, None, None);
    loop {
        let el = r.next().ok()??;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::Uint(v)) => group_id = u16::try_from(v).ok(),
            (Tag::Context(2), Value::Uint(v)) => keyset_id = u16::try_from(v).ok(),
            (Tag::Context(3), Value::Uint(v)) => next = u16::try_from(v).ok(),
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r)?
            }
            _ => {}
        }
    }
    Some(KeyMap {
        group_id: group_id?,
        keyset_id: keyset_id?,
        next: next?,
    })
}

/// `f/<idx>/k/<ksid>` — KeySetData: struct{ ctx1 policy, ctx2 keys_count=1,
/// ctx3 array[`KEYSET_SLOTS`個 の struct{ctx4 start_time, ctx5 hash, ctx6
/// bytes16}]（スロット1のみ実値、残りは 0/0/[0u8;16]）, ctx7 next }。読み側
/// `kvs::parse_keyset_first_entry`/`parse_key_struct` はこの形の最初の
/// エントリだけを見るので、残り2スロットの中身は問われない。
pub(crate) fn serialize_keyset(
    policy: u16,
    start_time: u64,
    hash: u16,
    key: &[u8; 16],
    next: u16,
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(1), u64::from(policy));
    w.put_uint(Tag::Context(2), 1); // keys_count
    w.start_array(Tag::Context(3));
    for i in 0..KEYSET_SLOTS {
        w.start_struct(Tag::Anonymous);
        if i == 0 {
            w.put_uint(Tag::Context(4), start_time);
            w.put_uint(Tag::Context(5), u64::from(hash));
            w.put_bytes(Tag::Context(6), key);
        } else {
            w.put_uint(Tag::Context(4), 0);
            w.put_uint(Tag::Context(5), 0);
            w.put_bytes(Tag::Context(6), &[0u8; 16]);
        }
        w.end_container();
    }
    w.end_container();
    w.put_uint(Tag::Context(7), u64::from(next));
    w.end_container();
    w.finish()
}

/// KeySetData の ctx7（チェーン内 next）だけを読む。既存 keyset を上書きする
/// ときにリンクを保つために使う。
fn keyset_next(blob: &[u8]) -> Option<u16> {
    let mut r = Reader::new(blob);
    if r.next().ok()??.value != Value::StructStart {
        return None;
    }
    let mut next = None;
    loop {
        let el = r.next().ok()??;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(7), Value::Uint(v)) => next = u16::try_from(v).ok(),
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r)?
            }
            _ => {}
        }
    }
    next
}

/// KeySetData の ctx7（チェーン内 next）だけを `next` に差し替えた blob を返す。
/// それ以外の要素（policy / keys_count / 3 スロットの epoch key）は TLV 要素単位で
/// そのまま写す — chip-tool `add-keysets` 由来の複数 epoch keyset を、mat 自身の
/// 1 スロット形（[`serialize_keyset`]）に潰さずにリンクだけ更新するため。
/// 外側が struct でない / ctx7 が無い / 途中で切れている blob は `None`。
fn keyset_with_next(blob: &[u8], next: u16) -> Option<Vec<u8>> {
    let mut r = Reader::new(blob);
    if r.next().ok()??.value != Value::StructStart {
        return None;
    }
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    let mut seen_next = false;
    loop {
        let el = r.next().ok()??;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(7), Value::Uint(_)) => {
                w.put_uint(Tag::Context(7), u64::from(next));
                seen_next = true;
            }
            (tag, value) => crate::tlv::copy_value(&mut w, &mut r, tag, value).ok()?,
        }
    }
    if !seen_next {
        return None;
    }
    w.end_container();
    Some(w.finish())
}

/// KeySetData の ctx3 配列・先頭 struct（slot 0）の ctx4 start_time / ctx5 hash /
/// ctx6 key だけを差し替えた blob を返す。policy / keys_count / 残スロット /
/// 未知タグ / ctx7 next は TLV 要素単位でそのまま写す（[`keyset_with_next`] と
/// 同型）— chip-tool `add-keysets` 由来の複数 epoch keyset を mat の 1 スロット形
/// に潰さずに鍵だけ置き換えるため（re-provision と IPK ローテーションの commit）。
/// 元の slot 0 に ctx4/5/6 のどれかが無ければ末尾に補う（mat / chip-tool どちらの
/// 書き手も 3 つ揃えるので実運用では起きない）。外側が struct でない / ctx3 が無い /
/// 先頭要素が struct でない / 途中で切れている blob は `None`。
pub(crate) fn keyset_with_slot0(
    blob: &[u8],
    start_time: u64,
    hash: u16,
    key: &[u8; 16],
) -> Option<Vec<u8>> {
    let mut r = Reader::new(blob);
    if r.next().ok()??.value != Value::StructStart {
        return None;
    }
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    let mut seen_slot0 = false;
    loop {
        let el = r.next().ok()??;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(3), Value::ArrayStart) if !seen_slot0 => {
                w.start_array(Tag::Context(3));
                if r.next().ok()??.value != Value::StructStart {
                    return None;
                }
                w.start_struct(Tag::Anonymous);
                let (mut put4, mut put5, mut put6) = (false, false, false);
                loop {
                    let e = r.next().ok()??;
                    match (e.tag, e.value) {
                        (_, Value::ContainerEnd) => break,
                        (Tag::Context(4), Value::Uint(_)) => {
                            w.put_uint(Tag::Context(4), start_time);
                            put4 = true;
                        }
                        (Tag::Context(5), Value::Uint(_)) => {
                            w.put_uint(Tag::Context(5), u64::from(hash));
                            put5 = true;
                        }
                        (Tag::Context(6), Value::Bytes(_)) => {
                            w.put_bytes(Tag::Context(6), key);
                            put6 = true;
                        }
                        (tag, value) => crate::tlv::copy_value(&mut w, &mut r, tag, value).ok()?,
                    }
                }
                if !put4 {
                    w.put_uint(Tag::Context(4), start_time);
                }
                if !put5 {
                    w.put_uint(Tag::Context(5), u64::from(hash));
                }
                if !put6 {
                    w.put_bytes(Tag::Context(6), key);
                }
                w.end_container();
                // 残りスロット（slot 1..）はそのまま写す。
                loop {
                    let e = r.next().ok()??;
                    match (e.tag, e.value) {
                        (_, Value::ContainerEnd) => break,
                        (tag, value) => crate::tlv::copy_value(&mut w, &mut r, tag, value).ok()?,
                    }
                }
                w.end_container();
                seen_slot0 = true;
            }
            (tag, value) => crate::tlv::copy_value(&mut w, &mut r, tag, value).ok()?,
        }
    }
    if !seen_slot0 {
        return None;
    }
    w.end_container();
    Some(w.finish())
}

/// `g/gfl` — FabricList（ctx1 first_entry, ctx2 entry_count）。
struct FabricList {
    first_entry: u16,
    entry_count: u16,
}

impl FabricList {
    fn serialize(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(1), u64::from(self.first_entry));
        w.put_uint(Tag::Context(2), u64::from(self.entry_count));
        w.end_container();
        w.finish()
    }
}

fn parse_fabric_list(blob: &[u8]) -> Option<FabricList> {
    let mut r = Reader::new(blob);
    if r.next().ok()??.value != Value::StructStart {
        return None;
    }
    let (mut first_entry, mut entry_count) = (None, None);
    loop {
        let el = r.next().ok()??;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(1), Value::Uint(v)) => first_entry = u16::try_from(v).ok(),
            (Tag::Context(2), Value::Uint(v)) => entry_count = u16::try_from(v).ok(),
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r)?
            }
            _ => {}
        }
    }
    Some(FabricList {
        first_entry: first_entry?,
        entry_count: entry_count?,
    })
}

/// 1回の `mat group provision` が chip-tool 側で行う 4 操作（add-group /
/// add-keysets / [unbind-keyset] / bind-keyset）の入力。
pub struct GroupProvisionWrite<'a> {
    pub group_id: u16,
    pub keyset_id: u16,
    pub name: &'a str,
    pub epoch_key: [u8; 16],
    pub rebind: bool,
}

/// `add-group`（上流 `GroupDataProviderImpl::SetGroupInfo` 相当）。既存
/// group_id があれば name を差し替えて再保存（endpoint 情報・next は維持）、
/// 無ければ `GroupData` を新規に末尾挿入する。
fn write_group(
    txn: &mut KvsTxn,
    fabric_index: u8,
    fabric: &mut FabricData,
    group_id: u16,
    name: &str,
) -> Result<(), GroupSettingsError> {
    let name = truncate_name(name);
    let mut cur = fabric.first_group;
    let mut tail: Option<u16> = None;
    for _ in 0..fabric.group_count {
        let key = format!("f/{fabric_index}/g/{cur:x}");
        let blob = txn
            .get(&key)?
            .ok_or_else(|| corrupt(&key, "missing group record"))?;
        let mut gd =
            parse_group_data(&blob).ok_or_else(|| corrupt(&key, "unparseable GroupData"))?;
        if cur == group_id {
            gd.name = name;
            txn.set(&key, &gd.serialize());
            return Ok(());
        }
        tail = Some(cur);
        cur = gd.next;
    }

    let gd = GroupData {
        name,
        first_endpoint: INVALID_ENDPOINT_ID,
        endpoint_count: 0,
        next: 0,
    };
    txn.set(&format!("f/{fabric_index}/g/{group_id:x}"), &gd.serialize());
    if fabric.group_count == 0 {
        fabric.first_group = group_id;
    } else {
        let tail_id = tail.expect("group_count > 0 walked at least one node");
        let tail_key = format!("f/{fabric_index}/g/{tail_id:x}");
        let tail_blob = txn
            .get(&tail_key)?
            .ok_or_else(|| corrupt(&tail_key, "missing group record"))?;
        let mut tail_gd = parse_group_data(&tail_blob)
            .ok_or_else(|| corrupt(&tail_key, "unparseable GroupData"))?;
        tail_gd.next = group_id;
        txn.set(&tail_key, &tail_gd.serialize());
    }
    fabric.group_count += 1;
    Ok(())
}

/// `add-keysets`（`SetKeySet` 相当）。既存 keyset_id があれば既存の
/// チェーン内 next を保ったまま上書き、無ければ head 挿入（`first_keyset`
/// を新 id に差し替え、旧 `first_keyset` を新エントリの next にする）。
///
/// 既存 keyset id への上書き（re-provision）は [`keyset_with_slot0`] で slot 0 の
/// 鍵・hash だけ差し替える — chip-tool `add-keysets` 由来の policy / 残スロットは
/// 落とさない（`unlink_keyset` のリンク差し替えと同じ無損失規律）。
fn write_keyset(
    txn: &mut KvsTxn,
    fabric_index: u8,
    fabric: &mut FabricData,
    keyset_id: u16,
    operational: &[u8; 16],
    hash: u16,
) -> Result<(), GroupSettingsError> {
    let mut cur = fabric.first_keyset;
    for _ in 0..fabric.keyset_count {
        let key = format!("f/{fabric_index}/k/{cur:x}");
        let blob = txn
            .get(&key)?
            .ok_or_else(|| corrupt(&key, "missing keyset record"))?;
        if cur == keyset_id {
            let relinked = keyset_with_slot0(&blob, EPOCH_START_TIME, hash, operational)
                .ok_or_else(|| corrupt(&key, "unparseable KeySetData"))?;
            txn.set(&key, &relinked);
            return Ok(());
        }
        cur = keyset_next(&blob).ok_or_else(|| corrupt(&key, "unparseable KeySetData"))?;
    }

    let key = format!("f/{fabric_index}/k/{keyset_id:x}");
    txn.set(
        &key,
        &serialize_keyset(0, EPOCH_START_TIME, hash, operational, fabric.first_keyset),
    );
    fabric.first_keyset = keyset_id;
    fabric.keyset_count += 1;
    Ok(())
}

/// GroupKeyMap を `first_map` から `map_count` 回、count 駆動で走査して
/// `(id, KeyMap)` を出現順に集める。unbind/bind 双方がこの結果を土台にする
/// —— とりわけ「unbind で消した id も max に数えたまま」を成立させるため、
/// max_id は unbind より前の、この一括走査の結果から取る必要がある。
fn scan_map(
    txn: &KvsTxn,
    fabric_index: u8,
    fabric: &FabricData,
) -> Result<Vec<(u16, KeyMap)>, GroupSettingsError> {
    let mut cur = fabric.first_map;
    let mut entries = Vec::with_capacity(fabric.map_count as usize);
    for _ in 0..fabric.map_count {
        let id = cur;
        let key = format!("f/{fabric_index}/gk/{id:x}");
        let blob = txn
            .get(&key)?
            .ok_or_else(|| corrupt(&key, "missing keymap record"))?;
        let km = parse_keymap(&blob).ok_or_else(|| corrupt(&key, "unparseable KeyMap"))?;
        cur = km.next;
        entries.push((id, km));
    }
    Ok(entries)
}

/// GroupData チェーンを `first_group` から `group_count` 回歩く。
fn scan_groups(
    txn: &KvsTxn,
    fabric_index: u8,
    fabric: &FabricData,
) -> Result<Vec<(u16, GroupData)>, GroupSettingsError> {
    let mut cur = fabric.first_group;
    let mut out = Vec::with_capacity(fabric.group_count as usize);
    for _ in 0..fabric.group_count {
        let key = format!("f/{fabric_index}/g/{cur:x}");
        let blob = txn
            .get(&key)?
            .ok_or_else(|| corrupt(&key, "missing group record"))?;
        let gd = parse_group_data(&blob).ok_or_else(|| corrupt(&key, "unparseable GroupData"))?;
        let next = gd.next;
        out.push((cur, gd));
        cur = next;
    }
    Ok(out)
}

/// KeySetData チェーンを `first_keyset` から `keyset_count` 回歩き、(id, 生 blob) を返す。
fn scan_keysets(
    txn: &KvsTxn,
    fabric_index: u8,
    fabric: &FabricData,
) -> Result<Vec<(u16, Vec<u8>)>, GroupSettingsError> {
    let mut cur = fabric.first_keyset;
    let mut out = Vec::with_capacity(fabric.keyset_count as usize);
    for _ in 0..fabric.keyset_count {
        let key = format!("f/{fabric_index}/k/{cur:x}");
        let blob = txn
            .get(&key)?
            .ok_or_else(|| corrupt(&key, "missing keyset record"))?;
        let next = keyset_next(&blob).ok_or_else(|| corrupt(&key, "unparseable KeySetData"))?;
        out.push((cur, blob));
        cur = next;
    }
    Ok(out)
}

/// `mat group list` の読み出し結果。鍵素材は載せない。
#[derive(Debug)]
pub struct GroupTable {
    pub groups: Vec<GroupRow>,
    pub keysets: Vec<KeysetRow>,
}
#[derive(Debug)]
pub struct GroupRow {
    pub group_id: u16,
    pub name: String,
    pub keyset_id: Option<u16>,
}
#[derive(Debug)]
pub struct KeysetRow {
    pub keyset_id: u16,
    pub bound_groups: Vec<u16>,
}

/// コントローラ側 group state を読む（`KvsTxn::open` の flock 区間で読み切り、
/// 書かずに drop）。`f/<idx>/g` が無い = 未 provision = 空。
pub fn read_groups(main_ini: &Path, fabric_index: u8) -> Result<GroupTable, GroupSettingsError> {
    let txn = KvsTxn::open(main_ini)?;
    let fkey = format!("f/{fabric_index}/g");
    let Some(fb) = txn.get(&fkey)? else {
        return Ok(GroupTable {
            groups: Vec::new(),
            keysets: Vec::new(),
        });
    };
    let fabric = parse_fabric_data(&fb).ok_or_else(|| corrupt(&fkey, "unparseable FabricData"))?;
    let maps = scan_map(&txn, fabric_index, &fabric)?;
    let groups = scan_groups(&txn, fabric_index, &fabric)?
        .into_iter()
        .map(|(id, gd)| GroupRow {
            group_id: id,
            name: gd.name,
            keyset_id: maps
                .iter()
                .find(|(_, m)| m.group_id == id)
                .map(|(_, m)| m.keyset_id),
        })
        .collect();
    let keysets = scan_keysets(&txn, fabric_index, &fabric)?
        .into_iter()
        .map(|(id, _)| KeysetRow {
            keyset_id: id,
            bound_groups: maps
                .iter()
                .filter(|(_, m)| m.keyset_id == id)
                .map(|(_, m)| m.group_id)
                .collect(),
        })
        .collect();
    Ok(GroupTable { groups, keysets })
}

/// rebind の unbind（best-effort、chip-tool 経路と同じ: 見つからなくても
/// 続行）＋ bind（`SetGroupKeyAt` 相当: 重複は `DuplicateBind`、新 id は
/// max_id+1 で sparse を維持）。
fn write_keymap(
    txn: &mut KvsTxn,
    fabric_index: u8,
    fabric: &mut FabricData,
    group_id: u16,
    keyset_id: u16,
    rebind: bool,
) -> Result<(), GroupSettingsError> {
    let mut entries = scan_map(txn, fabric_index, fabric)?;
    let max_id = entries.iter().map(|(id, _)| *id).max().unwrap_or(0);

    if rebind {
        unlink_keymaps_in(txn, fabric_index, fabric, &mut entries, |km| {
            km.group_id == group_id && km.keyset_id == keyset_id
        })?;
    }

    if entries
        .iter()
        .any(|(_, km)| km.group_id == group_id && km.keyset_id == keyset_id)
    {
        return Err(GroupSettingsError::DuplicateBind {
            group_id,
            keyset_id,
        });
    }

    let new_id = max_id + 1;
    let km = KeyMap {
        group_id,
        keyset_id,
        next: 0,
    };
    txn.set(&format!("f/{fabric_index}/gk/{new_id:x}"), &km.serialize());
    if fabric.map_count == 0 {
        fabric.first_map = new_id;
    } else {
        let (tail_id, mut tail_km) = *entries
            .last()
            .expect("map_count > 0 implies scan_map visited at least one node");
        tail_km.next = new_id;
        txn.set(
            &format!("f/{fabric_index}/gk/{tail_id:x}"),
            &tail_km.serialize(),
        );
    }
    fabric.map_count += 1;
    Ok(())
}

/// `g/gfl` への fabric_index 登録。無ければ新規作成、有れば `FabricData.next`
/// チェーンを辿って既に載っているか確認し、無ければ head 挿入する
/// （`fabric.next` は呼び出し側が最終的に保存する当該 fabric_index 自身の
/// `FabricData` なので、ここで書き換えて返す）。
fn write_fabric_list(
    txn: &mut KvsTxn,
    fabric_index: u8,
    fabric: &mut FabricData,
) -> Result<(), GroupSettingsError> {
    const KEY: &str = "g/gfl";
    match txn.get(KEY)? {
        None => {
            let list = FabricList {
                first_entry: u16::from(fabric_index),
                entry_count: 1,
            };
            txn.set(KEY, &list.serialize());
        }
        Some(b) => {
            let list =
                parse_fabric_list(&b).ok_or_else(|| corrupt(KEY, "unparseable FabricList"))?;
            let mut cur = list.first_entry;
            let mut found = false;
            for _ in 0..list.entry_count {
                if cur == u16::from(fabric_index) {
                    found = true;
                    break;
                }
                let fk = format!("f/{cur}/g");
                let blob = txn
                    .get(&fk)?
                    .ok_or_else(|| corrupt(&fk, "missing FabricData"))?;
                let fd = parse_fabric_data(&blob)
                    .ok_or_else(|| corrupt(&fk, "unparseable FabricData"))?;
                cur = fd.next;
            }
            if !found {
                fabric.next = list.first_entry;
                let new_list = FabricList {
                    first_entry: u16::from(fabric_index),
                    entry_count: list.entry_count + 1,
                };
                txn.set(KEY, &new_list.serialize());
            }
        }
    }
    Ok(())
}

/// chip-tool `groupsettings add-group / add-keysets / (unbind-keyset) /
/// bind-keyset` の一括版。5 レコード（`g/gfl`, `f/<i>/g`, `f/<i>/g/<gid>`,
/// `f/<i>/gk/<id>`, `f/<i>/k/<ksid>`）を1つの `KvsTxn`（1 flock 区間・1
/// commit）で読み・変更・書き切る。既存レコードのリンク切れ・解釈不能は
/// [`GroupSettingsError::Corrupt`]（何も書かず中断）、`rebind: false` での
/// 重複 bind は [`GroupSettingsError::DuplicateBind`]。
pub fn write_group_provision(
    main_ini: &Path,
    fabric_index: u8,
    compressed_fabric_id: &[u8; 8],
    w: &GroupProvisionWrite<'_>,
) -> Result<(), GroupSettingsError> {
    let mut txn = KvsTxn::open(main_ini)?;
    let fkey = format!("f/{fabric_index}/g");
    let mut fabric = match txn.get(&fkey)? {
        None => FabricData::empty(),
        Some(b) => parse_fabric_data(&b).ok_or_else(|| corrupt(&fkey, "unparseable FabricData"))?,
    };

    // 1) add-group
    write_group(&mut txn, fabric_index, &mut fabric, w.group_id, w.name)?;

    // 2) add-keysets
    let operational = derive_ipk_operational(&w.epoch_key, compressed_fabric_id);
    let hash = derive_group_session_id(&operational);
    write_keyset(
        &mut txn,
        fabric_index,
        &mut fabric,
        w.keyset_id,
        &operational,
        hash,
    )?;

    // 3) rebind なら unbind-keyset（best-effort）＋ 4) bind-keyset
    write_keymap(
        &mut txn,
        fabric_index,
        &mut fabric,
        w.group_id,
        w.keyset_id,
        w.rebind,
    )?;

    // 5) FabricList 登録 + FabricData 保存 → commit
    write_fabric_list(&mut txn, fabric_index, &mut fabric)?;

    txn.set(&fkey, &fabric.serialize());
    txn.commit()?;
    Ok(())
}

/// `entries`（`scan_map` の結果）から `pred` に合う KeyMap を全て外す: レコード
/// 削除・前ノードの next つなぎ替え・`first_map` / `map_count` 更新。`entries`
/// も同期して縮める（呼び手が続けて使えるように）。外した KeyMap を返す。
fn unlink_keymaps_in(
    txn: &mut KvsTxn,
    fabric_index: u8,
    fabric: &mut FabricData,
    entries: &mut Vec<(u16, KeyMap)>,
    pred: impl Fn(&KeyMap) -> bool,
) -> Result<Vec<KeyMap>, GroupSettingsError> {
    let mut removed = Vec::new();
    while let Some(pos) = entries.iter().position(|(_, km)| pred(km)) {
        let (removed_id, removed_km) = entries[pos];
        txn.remove(&format!("f/{fabric_index}/gk/{removed_id:x}"));
        if pos == 0 {
            fabric.first_map = removed_km.next;
        } else {
            let (prev_id, mut prev_km) = entries[pos - 1];
            prev_km.next = removed_km.next;
            txn.set(
                &format!("f/{fabric_index}/gk/{prev_id:x}"),
                &prev_km.serialize(),
            );
            entries[pos - 1].1 = prev_km;
        }
        fabric.map_count -= 1;
        entries.remove(pos);
        removed.push(removed_km);
    }
    Ok(removed)
}

/// KeySetData チェーンから `keyset_id` を外す（前ノードは生 blob の ctx7 だけ
/// 差し替える — chip-tool 由来の policy / 複数 epoch key を落とさない）。
/// 見つからなければ何もしない。
fn unlink_keyset(
    txn: &mut KvsTxn,
    fabric_index: u8,
    fabric: &mut FabricData,
    keyset_id: u16,
) -> Result<bool, GroupSettingsError> {
    // keyset 0 は IPK（`f/<idx>/k/0`）。fabric の運用鍵そのもので、group の
    // KeyMap から参照が消えたからといって外してよいものではない — 外すと CASE /
    // group メッセージの鍵素材ごと失われ、fabric を張り直すまで復旧しない。
    if keyset_id == 0 {
        return Ok(false);
    }
    let chain = scan_keysets(txn, fabric_index, fabric)?;
    let Some(pos) = chain.iter().position(|(id, _)| *id == keyset_id) else {
        return Ok(false);
    };
    let key = format!("f/{fabric_index}/k/{keyset_id:x}");
    let removed_next =
        keyset_next(&chain[pos].1).ok_or_else(|| corrupt(&key, "unparseable KeySetData"))?;
    txn.remove(&key);
    if pos == 0 {
        fabric.first_keyset = removed_next;
    } else {
        let (prev_id, prev_blob) = &chain[pos - 1];
        let prev_key = format!("f/{fabric_index}/k/{prev_id:x}");
        let relinked = keyset_with_next(prev_blob, removed_next)
            .ok_or_else(|| corrupt(&prev_key, "unparseable KeySetData"))?;
        txn.set(&prev_key, &relinked);
    }
    fabric.keyset_count -= 1;
    Ok(true)
}

/// [`remove_group`] の結果。KeySet も一緒に外れたかを伝える（CLI の出力用）。
#[derive(Debug)]
pub struct RemoveOutcome {
    pub keyset_removed: bool,
}

/// `mat group remove` のコントローラ側: GroupData をチェーンから外し、その
/// group の KeyMap 行を全て外し、参照が無くなった KeySet を外す。1 つの
/// `KvsTxn` で完結。`g/gfl`（FabricList）は触らない。
pub fn remove_group(
    main_ini: &Path,
    fabric_index: u8,
    group_id: u16,
) -> Result<RemoveOutcome, GroupSettingsError> {
    let mut txn = KvsTxn::open(main_ini)?;
    let fkey = format!("f/{fabric_index}/g");
    let Some(fb) = txn.get(&fkey)? else {
        return Err(GroupSettingsError::NotFound { group_id });
    };
    let mut fabric =
        parse_fabric_data(&fb).ok_or_else(|| corrupt(&fkey, "unparseable FabricData"))?;

    // 1) GroupData チェーンから外す
    let groups = scan_groups(&txn, fabric_index, &fabric)?;
    let pos = groups
        .iter()
        .position(|(id, _)| *id == group_id)
        .ok_or(GroupSettingsError::NotFound { group_id })?;
    txn.remove(&format!("f/{fabric_index}/g/{group_id:x}"));
    if pos == 0 {
        fabric.first_group = groups[pos].1.next;
    } else {
        let (prev_id, prev) = &groups[pos - 1];
        let mut prev = prev.clone();
        prev.next = groups[pos].1.next;
        txn.set(
            &format!("f/{fabric_index}/g/{prev_id:x}"),
            &prev.serialize(),
        );
    }
    fabric.group_count -= 1;

    // 2) KeyMap 行を外す
    let mut entries = scan_map(&txn, fabric_index, &fabric)?;
    let removed = unlink_keymaps_in(&mut txn, fabric_index, &mut fabric, &mut entries, |km| {
        km.group_id == group_id
    })?;

    // 3) 参照が無くなった KeySet を外す
    let mut keyset_removed = false;
    let mut seen = std::collections::BTreeSet::new();
    for km in &removed {
        if !seen.insert(km.keyset_id) {
            continue;
        }
        if entries.iter().any(|(_, m)| m.keyset_id == km.keyset_id) {
            continue;
        }
        keyset_removed |= unlink_keyset(&mut txn, fabric_index, &mut fabric, km.keyset_id)?;
    }

    txn.set(&fkey, &fabric.serialize());
    txn.commit()?;
    Ok(RemoveOutcome { keyset_removed })
}

/// IPK ローテーションの pending 開始: `mat/f/<idx>/ipk-epoch-next` を書く。既に
/// 同じ値なら no-op、別の値なら `Corrupt`（呼び手は事前に read して resume 判定
/// する前提なので、違う値に出会うのは並行実行の証拠）。
pub fn begin_ipk_rotation(
    main_ini: &Path,
    fabric_index: u8,
    next: &[u8; 16],
) -> Result<(), GroupSettingsError> {
    let mut txn = KvsTxn::open(main_ini)?;
    let key = mat_ipk_epoch_slot_key(fabric_index, IpkEpochSlot::Next);
    match txn.get(&key)? {
        Some(existing) if existing.as_slice() == next => return Ok(()),
        Some(_) => {
            return Err(corrupt(
                &key,
                "a different ipk rotation is already pending (concurrent rotate-ipk?)",
            ))
        }
        None => {}
    }
    txn.set(&key, next);
    txn.commit()?;
    Ok(())
}

/// IPK ローテーションの commit（1 KvsTxn）: `f/<idx>/k/0` の slot 0 を
/// `derive(next)` に差し替え（[`keyset_with_slot0`] — policy / 残スロット / next
/// リンクは無傷）、`ipk-epoch := next`、`ipk-epoch-prev := cur`、`ipk-epoch-next`
/// を削除。pending が無い / 値が `next` と違う / k/0 が無い・解釈不能は `Corrupt`
/// で何も書かない。
pub fn commit_ipk_rotation(
    main_ini: &Path,
    fabric_index: u8,
    cfid: &[u8; 8],
    cur: &[u8; 16],
    next: &[u8; 16],
) -> Result<(), GroupSettingsError> {
    let mut txn = KvsTxn::open(main_ini)?;
    let next_key = mat_ipk_epoch_slot_key(fabric_index, IpkEpochSlot::Next);
    match txn.get(&next_key)? {
        Some(v) if v.as_slice() == next => {}
        Some(_) => {
            return Err(corrupt(
                &next_key,
                "pending ipk epoch differs from the one being committed (concurrent rotate-ipk?)",
            ))
        }
        None => return Err(corrupt(&next_key, "no ipk rotation pending")),
    }
    let k0 = format!("f/{fabric_index}/k/0");
    let blob = txn
        .get(&k0)?
        .ok_or_else(|| corrupt(&k0, "missing IPK keyset record"))?;
    let operational = derive_ipk_operational(next, cfid);
    let hash = derive_group_session_id(&operational);
    let rewritten = keyset_with_slot0(&blob, EPOCH_START_TIME, hash, &operational)
        .ok_or_else(|| corrupt(&k0, "unparseable KeySetData"))?;
    txn.set(&k0, &rewritten);
    txn.set(
        &mat_ipk_epoch_slot_key(fabric_index, IpkEpochSlot::Current),
        next,
    );
    txn.set(
        &mat_ipk_epoch_slot_key(fabric_index, IpkEpochSlot::Prev),
        cur,
    );
    txn.remove(&next_key);
    txn.commit()?;
    Ok(())
}

/// pending の IPK ローテーションを取り消す（`ipk-epoch-next` を消すだけ）。
/// pending が無ければ `Ok(false)`（ファイルは触らない）。
pub fn abort_ipk_rotation(main_ini: &Path, fabric_index: u8) -> Result<bool, GroupSettingsError> {
    let mut txn = KvsTxn::open(main_ini)?;
    let key = mat_ipk_epoch_slot_key(fabric_index, IpkEpochSlot::Next);
    if txn.get(&key)?.is_none() {
        return Ok(false);
    }
    txn.remove(&key);
    txn.commit()?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fabric::{derive_group_session_id, derive_ipk_operational};

    const CFID: [u8; 8] = [0xAA, 0xBB, 0xCC, 0xDD, 0xEE, 0xFF, 0x00, 0x11];

    fn tmp_ini(lines: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("chip_tool_config.ini");
        std::fs::write(&p, lines).unwrap();
        (dir, p)
    }

    fn provision(p: &std::path::Path, group: u16, keyset: u16, rebind: bool) {
        write_group_provision(
            p,
            2,
            &CFID,
            &GroupProvisionWrite {
                group_id: group,
                keyset_id: keyset,
                name: "e2e",
                epoch_key: [0x42; 16],
                rebind,
            },
        )
        .unwrap();
    }

    #[test]
    fn fresh_fabric_full_provision_readable_by_existing_parser() {
        let (_d, p) = tmp_ini("[Default]\n");
        provision(&p, 99, 99, false);
        // 読み側パーサ（実機検証済み）で読み戻せる = タグ互換の証明。
        let creds = crate::kvs::read_group_credentials(&p, 2, 99).unwrap();
        let op = derive_ipk_operational(&[0x42; 16], &CFID);
        assert_eq!(creds.encryption_key, op);
        assert_eq!(creds.session_id, derive_group_session_id(&op));
        // 5 レコードが揃っている。
        let txn = crate::kvs::KvsTxn::open(&p).unwrap();
        for key in ["g/gfl", "f/2/g", "f/2/g/63", "f/2/gk/1", "f/2/k/63"] {
            assert!(txn.get(key).unwrap().is_some(), "missing {key}");
        }
    }

    #[test]
    fn keyset_blob_has_three_slots_and_terminator_ffff() {
        let (_d, p) = tmp_ini("[Default]\n");
        provision(&p, 99, 99, false);
        let txn = crate::kvs::KvsTxn::open(&p).unwrap();
        let blob = txn.get("f/2/k/63").unwrap().unwrap();
        // struct{ctx1 policy=0, ctx2 keys_count=1, ctx3 array[3 structs], ctx7 next=0xFFFF}
        // を Reader で走査して検証（スロット2,3 は start_time=0/hash=0/key=16B ゼロ）。
        let mut r = crate::tlv::Reader::new(&blob);
        assert!(matches!(
            r.next().unwrap().unwrap().value,
            crate::tlv::Value::StructStart
        ));
        let mut slots = 0;
        let mut next: Option<u64> = None;
        loop {
            let el = r.next().unwrap().unwrap();
            match (el.tag, el.value) {
                (_, crate::tlv::Value::ContainerEnd) => break,
                (crate::tlv::Tag::Context(3), crate::tlv::Value::ArrayStart) => loop {
                    let e = r.next().unwrap().unwrap();
                    match e.value {
                        crate::tlv::Value::StructStart => {
                            slots += 1;
                            // struct を読み飛ばす。
                            loop {
                                if matches!(
                                    r.next().unwrap().unwrap().value,
                                    crate::tlv::Value::ContainerEnd
                                ) {
                                    break;
                                }
                            }
                        }
                        crate::tlv::Value::ContainerEnd => break,
                        _ => {}
                    }
                },
                (crate::tlv::Tag::Context(7), crate::tlv::Value::Uint(v)) => next = Some(v),
                _ => {}
            }
        }
        assert_eq!(slots, 3);
        assert_eq!(next, Some(0xFFFF));
    }

    #[test]
    fn existing_chiptool_like_store_gets_tail_group_and_head_keyset() {
        // chip-tool が作った既存状態を再現: IPK keyset 0 + group 10/keyset 60
        //（keyset チェーン 60→0→end、group チェーン 10→end、map gk/1）。
        // fixture は自前の write_group_provision で group10/keyset60 を書き、
        // 手で first_keyset チェーンに keyset 0 を差し込む …のは複雑なので、
        // 「まず 10/60 を provision → 次に 99/99 を provision」して
        // 2 group 目のリンク規律を検証する（keyset 0 の混在は
        // ipk_keyset_zero_in_chain_is_preserved で別途）。
        let (_d, p) = tmp_ini("[Default]\n");
        provision(&p, 10, 60, false);
        provision(&p, 99, 99, false);
        let txn = crate::kvs::KvsTxn::open(&p).unwrap();
        // FabricData: group 末尾挿入 → first_group=10 のまま、group 10 の next=99。
        // keyset head 挿入 → first_keyset=99、99 の next=60。map は gk/1→gk/2。
        let fabric = parse_fabric_data(&txn.get("f/2/g").unwrap().unwrap()).unwrap();
        assert_eq!(fabric.first_group, 10);
        assert_eq!(fabric.group_count, 2);
        assert_eq!(fabric.first_keyset, 99);
        assert_eq!(fabric.keyset_count, 2);
        assert_eq!(fabric.first_map, 1);
        assert_eq!(fabric.map_count, 2);
        let g10 = parse_group_data(&txn.get("f/2/g/a").unwrap().unwrap()).unwrap();
        assert_eq!(g10.next, 99);
        let g99 = parse_group_data(&txn.get("f/2/g/63").unwrap().unwrap()).unwrap();
        assert_eq!(g99.next, 0);
        assert_eq!(g99.first_endpoint, 0xFFFF);
        let m1 = parse_keymap(&txn.get("f/2/gk/1").unwrap().unwrap()).unwrap();
        assert_eq!((m1.group_id, m1.keyset_id, m1.next), (10, 60, 2));
        let m2 = parse_keymap(&txn.get("f/2/gk/2").unwrap().unwrap()).unwrap();
        assert_eq!((m2.group_id, m2.keyset_id, m2.next), (99, 99, 0));
        // 両 group とも読み側で解決できる。
        assert!(crate::kvs::read_group_credentials(&p, 2, 10).is_ok());
        assert!(crate::kvs::read_group_credentials(&p, 2, 99).is_ok());
    }

    #[test]
    fn duplicate_bind_without_rebind_is_error() {
        let (_d, p) = tmp_ini("[Default]\n");
        provision(&p, 99, 99, false);
        let err = write_group_provision(
            &p,
            2,
            &CFID,
            &GroupProvisionWrite {
                group_id: 99,
                keyset_id: 99,
                name: "e2e",
                epoch_key: [0x42; 16],
                rebind: false,
            },
        )
        .expect_err("duplicate bind must fail");
        assert!(matches!(
            err,
            GroupSettingsError::DuplicateBind {
                group_id: 99,
                keyset_id: 99
            }
        ));
    }

    #[test]
    fn rebind_unbinds_then_binds_and_map_ids_stay_sparse() {
        let (_d, p) = tmp_ini("[Default]\n");
        provision(&p, 10, 60, false); // gk/1
        provision(&p, 99, 99, false); // gk/2
        provision(&p, 99, 99, true); // unbind gk/2 → 新 id は max+1=3（詰め直さない）
        let txn = crate::kvs::KvsTxn::open(&p).unwrap();
        assert!(
            txn.get("f/2/gk/2").unwrap().is_none(),
            "unbound entry must be deleted"
        );
        let m3 = parse_keymap(&txn.get("f/2/gk/3").unwrap().unwrap()).unwrap();
        assert_eq!((m3.group_id, m3.keyset_id, m3.next), (99, 99, 0));
        let m1 = parse_keymap(&txn.get("f/2/gk/1").unwrap().unwrap()).unwrap();
        assert_eq!(m1.next, 3, "prev link must be re-pointed");
        let fabric = parse_fabric_data(&txn.get("f/2/g").unwrap().unwrap()).unwrap();
        assert_eq!(fabric.map_count, 2);
        // group / keyset は更新のみ（重複レコードにならない）。
        assert_eq!(fabric.group_count, 2);
        assert_eq!(fabric.keyset_count, 2);
    }

    #[test]
    fn ipk_keyset_zero_in_chain_is_preserved() {
        // keyset id 0（IPK）が既にチェーンにいる状態（実機と同型）を
        // 手組みで再現: まず 99/99 を書き、first_keyset チェーンを
        // 99 → 0 に差し替え + k/0 を置く（0 は有効 id、終端は 0xFFFF）。
        let (_d, p) = tmp_ini("[Default]\n");
        provision(&p, 99, 99, false);
        {
            let mut txn = crate::kvs::KvsTxn::open(&p).unwrap();
            // k/0 = keyset 99 の blob を流用し next を 0xFFFF のままにする
            //（中身は問わない — 走査対象になることだけが重要）。
            let blob = txn.get("f/2/k/63").unwrap().unwrap();
            txn.set("f/2/k/0", &blob);
            // f/2/k/63 の next を 0 に書き換え（serialize_keyset で再生成）。
            let op = derive_ipk_operational(&[0x42; 16], &CFID);
            let hash = derive_group_session_id(&op);
            txn.set("f/2/k/63", &serialize_keyset(0, 1, hash, &op, 0));
            let mut fabric = parse_fabric_data(&txn.get("f/2/g").unwrap().unwrap()).unwrap();
            fabric.keyset_count = 2;
            txn.set("f/2/g", &fabric.serialize());
            txn.commit().unwrap();
        }
        // ここへ新 keyset 100 を head 挿入しても、0 を終端と誤認せず
        // チェーンが 100 → 99 → 0 になる。
        write_group_provision(
            &p,
            2,
            &CFID,
            &GroupProvisionWrite {
                group_id: 100,
                keyset_id: 100,
                name: "x",
                epoch_key: [0x43; 16],
                rebind: false,
            },
        )
        .unwrap();
        let txn = crate::kvs::KvsTxn::open(&p).unwrap();
        let fabric = parse_fabric_data(&txn.get("f/2/g").unwrap().unwrap()).unwrap();
        assert_eq!(fabric.first_keyset, 100);
        assert_eq!(fabric.keyset_count, 3);
        assert_eq!(
            keyset_next(&txn.get("f/2/k/64").unwrap().unwrap()).unwrap(),
            99
        );
    }

    #[test]
    fn corrupt_chain_is_hard_error_and_writes_nothing() {
        // first_group が指す group レコードが無い → Corrupt、ファイル無変更。
        let (_d, p) = tmp_ini("[Default]\n");
        {
            let mut txn = crate::kvs::KvsTxn::open(&p).unwrap();
            let fabric = FabricData {
                first_group: 7,
                group_count: 1,
                ..FabricData::empty()
            };
            txn.set("f/2/g", &fabric.serialize());
            txn.commit().unwrap();
        }
        let before = std::fs::read_to_string(&p).unwrap();
        let err = write_group_provision(
            &p,
            2,
            &CFID,
            &GroupProvisionWrite {
                group_id: 99,
                keyset_id: 99,
                name: "x",
                epoch_key: [0x42; 16],
                rebind: false,
            },
        )
        .expect_err("corrupt chain");
        assert!(matches!(err, GroupSettingsError::Corrupt { .. }), "{err:?}");
        assert_eq!(
            std::fs::read_to_string(&p).unwrap(),
            before,
            "must not write"
        );
    }

    #[test]
    fn group_name_is_truncated_to_16_bytes_on_char_boundary() {
        let (_d, p) = tmp_ini("[Default]\n");
        write_group_provision(
            &p,
            2,
            &CFID,
            &GroupProvisionWrite {
                group_id: 5,
                keyset_id: 5,
                name: "0123456789abcdefOVERFLOW",
                epoch_key: [0x42; 16],
                rebind: false,
            },
        )
        .unwrap();
        let txn = crate::kvs::KvsTxn::open(&p).unwrap();
        let g = parse_group_data(&txn.get("f/2/g/5").unwrap().unwrap()).unwrap();
        assert_eq!(g.name, "0123456789abcdef");
    }

    #[test]
    fn read_groups_empty_when_never_provisioned() {
        let (_d, p) = tmp_ini("[Default]\n");
        let t = read_groups(&p, 2).unwrap();
        assert!(t.groups.is_empty() && t.keysets.is_empty());
    }

    #[test]
    fn read_groups_lists_chain_with_shared_keyset() {
        let (_d, p) = tmp_ini("[Default]\n");
        provision(&p, 1, 42, false);
        provision(&p, 2, 42, false);
        provision(&p, 3, 7, false);
        let t = read_groups(&p, 2).unwrap();
        let ids: Vec<(u16, Option<u16>)> =
            t.groups.iter().map(|g| (g.group_id, g.keyset_id)).collect();
        assert_eq!(ids, vec![(1, Some(42)), (2, Some(42)), (3, Some(7))]);
        assert!(t.groups.iter().all(|g| g.name == "e2e"));
        let mut ks: Vec<(u16, Vec<u16>)> = t
            .keysets
            .iter()
            .map(|k| (k.keyset_id, k.bound_groups.clone()))
            .collect();
        ks.sort();
        assert_eq!(ks, vec![(7, vec![3]), (42, vec![1, 2])]);
    }

    #[test]
    fn read_groups_missing_ini_is_kvs_io_error() {
        let d = tempfile::tempdir().unwrap();
        let err = read_groups(&d.path().join("chip_tool_config.ini"), 2).unwrap_err();
        assert!(matches!(err, GroupSettingsError::Kvs(KvsError::Io(_))));
    }

    #[test]
    fn remove_group_unlinks_middle_and_drops_unreferenced_keyset() {
        let (_d, p) = tmp_ini("[Default]\n");
        provision(&p, 1, 42, false);
        provision(&p, 2, 7, false);
        provision(&p, 3, 42, false);
        let out = remove_group(&p, 2, 2).unwrap();
        assert!(out.keyset_removed, "keyset 7 は group 2 だけが参照");
        let t = read_groups(&p, 2).unwrap();
        let ids: Vec<u16> = t.groups.iter().map(|g| g.group_id).collect();
        assert_eq!(ids, vec![1, 3]);
        assert_eq!(t.keysets.len(), 1);
        assert_eq!(t.keysets[0].keyset_id, 42);
        assert_eq!(t.keysets[0].bound_groups, vec![1, 3]);
        // 再 provision できる（チェーンが壊れていない）。
        provision(&p, 2, 7, false);
        assert_eq!(read_groups(&p, 2).unwrap().groups.len(), 3);
    }

    #[test]
    fn remove_group_head_and_tail_keep_shared_keyset() {
        let (_d, p) = tmp_ini("[Default]\n");
        provision(&p, 1, 42, false);
        provision(&p, 2, 42, false);
        assert!(
            !remove_group(&p, 2, 1).unwrap().keyset_removed,
            "keyset 42 は group 2 が参照中"
        );
        assert!(remove_group(&p, 2, 2).unwrap().keyset_removed);
        let t = read_groups(&p, 2).unwrap();
        assert!(t.groups.is_empty() && t.keysets.is_empty());
    }

    #[test]
    fn remove_group_unknown_is_not_found() {
        let (_d, p) = tmp_ini("[Default]\n");
        provision(&p, 1, 42, false);
        assert!(matches!(
            remove_group(&p, 2, 9).unwrap_err(),
            GroupSettingsError::NotFound { group_id: 9 }
        ));
        // 未 provision（FabricData 無し）も NotFound。
        let (_d2, p2) = tmp_ini("[Default]\n");
        assert!(matches!(
            remove_group(&p2, 2, 1).unwrap_err(),
            GroupSettingsError::NotFound { .. }
        ));
    }

    /// F5: keyset 0 = IPK。group を `--keyset-id 0` で bind してから
    /// `remove_group` しても IPK レコード（`f/<idx>/k/0`）は残さねばならない
    /// （外すと fabric の運用鍵ごと失われ、張り直すまで復旧しない）。
    #[test]
    fn remove_group_never_unlinks_the_ipk_keyset_zero() {
        let (_d, p) = tmp_ini("[Default]\n");
        provision(&p, 1, 0, false);
        let before = read_groups(&p, 2).unwrap();
        assert!(
            before.keysets.iter().any(|k| k.keyset_id == 0),
            "前提: keyset 0 が居る {before:?}"
        );
        let out = remove_group(&p, 2, 1).unwrap();
        assert!(!out.keyset_removed, "IPK keyset は外さない");
        let after = read_groups(&p, 2).unwrap();
        assert!(after.groups.is_empty(), "group 自体は外れる");
        assert!(
            after.keysets.iter().any(|k| k.keyset_id == 0),
            "keyset 0 (IPK) はチェーンに残る: {after:?}"
        );
        // 鍵素材そのものも読める（チェーンが壊れていない）。
        let txn = KvsTxn::open(&p).unwrap();
        assert!(
            txn.get("f/2/k/0").unwrap().is_some(),
            "f/2/k/0 レコードが残っている"
        );
    }

    #[test]
    fn remove_group_unlinks_middle_keyset_preserving_prev_key_material() {
        let (_d, p) = tmp_ini("[Default]\n");
        for (g, ks, epoch) in [(1u16, 10u16, 0x11u8), (2, 20, 0x22), (3, 30, 0x33)] {
            write_group_provision(
                &p,
                2,
                &CFID,
                &GroupProvisionWrite {
                    group_id: g,
                    keyset_id: ks,
                    name: "e2e",
                    epoch_key: [epoch; 16],
                    rebind: false,
                },
            )
            .unwrap();
        }
        // keyset は head 挿入なのでチェーンは 30 → 20 → 10。真ん中の 20 を外す。
        assert!(remove_group(&p, 2, 2).unwrap().keyset_removed);
        {
            let txn = crate::kvs::KvsTxn::open(&p).unwrap();
            assert!(txn.get("f/2/k/14").unwrap().is_none(), "keyset 20 must go");
            let fabric = parse_fabric_data(&txn.get("f/2/g").unwrap().unwrap()).unwrap();
            assert_eq!((fabric.first_keyset, fabric.keyset_count), (30, 2));
            assert_eq!(
                keyset_next(&txn.get("f/2/k/1e").unwrap().unwrap()).unwrap(),
                10,
                "prev node must skip the removed keyset"
            );
        }
        // 前ノード（30）は next だけ差し替え、鍵素材は無傷。
        let creds = crate::kvs::read_group_credentials(&p, 2, 3).unwrap();
        let op = derive_ipk_operational(&[0x33; 16], &CFID);
        assert_eq!(creds.encryption_key, op);
        assert_eq!(creds.session_id, derive_group_session_id(&op));
        assert!(crate::kvs::read_group_credentials(&p, 2, 1).is_ok());
    }

    /// chip-tool `groupsettings add-keysets` が書く形の KeySetData: policy /
    /// keys_count / 3 スロットとも実値（mat の serialize_keyset はスロット 1 のみ）。
    fn multi_epoch_keyset(policy: u16, slots: &[(u64, u16, [u8; 16])], next: u16) -> Vec<u8> {
        assert_eq!(slots.len(), KEYSET_SLOTS);
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(1), u64::from(policy));
        w.put_uint(Tag::Context(2), slots.len() as u64);
        w.start_array(Tag::Context(3));
        for (start, hash, key) in slots {
            w.start_struct(Tag::Anonymous);
            w.put_uint(Tag::Context(4), *start);
            w.put_uint(Tag::Context(5), u64::from(*hash));
            w.put_bytes(Tag::Context(6), key);
            w.end_container();
        }
        w.end_container();
        w.put_uint(Tag::Context(7), u64::from(next));
        w.end_container();
        w.finish()
    }

    #[test]
    fn remove_group_unlinks_after_chiptool_multi_epoch_keyset_without_losing_epochs() {
        // chip-tool 由来の前ノード（policy=CacheAndSync、epoch 3 本）の直後の
        // keyset を外す。前ノードは ctx7（next）だけ差し替わり、policy / keys_count /
        // 3 スロット全部が無傷で残ること（旧実装はスロット 1 だけ残して再 serialize
        // していた）。
        let (_d, p) = tmp_ini("[Default]\n");
        for (g, ks, epoch) in [(1u16, 10u16, 0x11u8), (2, 20, 0x22), (3, 30, 0x33)] {
            write_group_provision(
                &p,
                2,
                &CFID,
                &GroupProvisionWrite {
                    group_id: g,
                    keyset_id: ks,
                    name: "e2e",
                    epoch_key: [epoch; 16],
                    rebind: false,
                },
            )
            .unwrap();
        }
        // チェーンは 30 → 20 → 10。head の 30 を chip-tool 風 3 epoch 版に差し替える。
        let slots = [
            (1u64, 0x1111u16, [0x31u8; 16]),
            (2_000_000, 0x2222, [0x32; 16]),
            (3_000_000, 0x3333, [0x33; 16]),
        ];
        {
            let mut txn = crate::kvs::KvsTxn::open(&p).unwrap();
            assert_eq!(
                keyset_next(&txn.get("f/2/k/1e").unwrap().unwrap()),
                Some(20)
            );
            txn.set("f/2/k/1e", &multi_epoch_keyset(1, &slots, 20));
            txn.commit().unwrap();
        }

        assert!(remove_group(&p, 2, 2).unwrap().keyset_removed);

        let txn = crate::kvs::KvsTxn::open(&p).unwrap();
        assert!(txn.get("f/2/k/14").unwrap().is_none(), "keyset 20 must go");
        let fabric = parse_fabric_data(&txn.get("f/2/g").unwrap().unwrap()).unwrap();
        assert_eq!((fabric.first_keyset, fabric.keyset_count), (30, 2));
        let prev = txn.get("f/2/k/1e").unwrap().unwrap();
        assert_eq!(
            prev,
            multi_epoch_keyset(1, &slots, 10),
            "prev node must change only ctx7 (next 20 -> 10)"
        );
        // 読み側も第 1 エントリを従来どおり解決できる。
        let (key, hash) = crate::kvs::parse_keyset_first_entry(&prev, 2).unwrap();
        assert_eq!((key, hash), ([0x31; 16], Some(0x1111)));
    }

    #[test]
    fn keyset_with_next_rewrites_only_ctx7() {
        let slots = [
            (1u64, 0xAAAAu16, [0xA0u8; 16]),
            (2, 0xBBBB, [0xB0; 16]),
            (3, 0xCCCC, [0xC0; 16]),
        ];
        let blob = multi_epoch_keyset(1, &slots, 0x0BAD);
        assert_eq!(
            keyset_with_next(&blob, INVALID_KEYSET_ID).unwrap(),
            multi_epoch_keyset(1, &slots, INVALID_KEYSET_ID)
        );
        // mat 自身が書く 1 スロット形も同じ経路で無損失。
        let mine = serialize_keyset(0, EPOCH_START_TIME, 7, &[0x42; 16], 5);
        assert_eq!(
            keyset_with_next(&mine, 9).unwrap(),
            serialize_keyset(0, EPOCH_START_TIME, 7, &[0x42; 16], 9)
        );
        // ctx7 が無い / 壊れた blob は None（呼び手が Corrupt にする）。
        assert!(keyset_with_next(&[0x15, 0x18], 1).is_none());
        assert!(keyset_with_next(&blob[..blob.len() - 3], 1).is_none());
    }

    #[test]
    fn keyset_with_next_preserves_unknown_tags_order_and_nested_slots() {
        // 外側 ctx7（チェーンの next）が先頭、ctx9 は未知の追加タグ、そして
        // スロット内部にも ctx7（uint）と ctx8（struct）を紛れ込ませる —
        // どちらも外側の chain-next ctx7 と取り違えてはいけない。
        // `build` を両側で共有し、keyset_with_next の出力が「他の要素は
        // 一切いじらず ctx7 だけを差し替えた」ことをバイト単位で確認する。
        fn build(next: u16) -> Vec<u8> {
            let mut w = Writer::new();
            w.start_struct(Tag::Anonymous);
            w.put_uint(Tag::Context(7), u64::from(next)); // chain next（先頭）
            w.put_uint(Tag::Context(9), 42); // 未知の追加タグ
            w.put_uint(Tag::Context(1), 1); // policy
            w.put_uint(Tag::Context(2), 1); // keys_count
            w.start_array(Tag::Context(3));
            w.start_struct(Tag::Anonymous);
            w.put_uint(Tag::Context(4), 1);
            w.put_uint(Tag::Context(5), 0x1111);
            w.put_bytes(Tag::Context(6), &[0x31; 16]);
            w.put_uint(Tag::Context(7), 99); // スロット内 ctx7（chain link ではない）
            w.start_struct(Tag::Context(8)); // スロット内のネストしたコンテナ
            w.put_uint(Tag::Context(0), 7);
            w.end_container();
            w.end_container(); // スロット struct
            w.end_container(); // ctx3 array
            w.end_container(); // outer struct
            w.finish()
        }

        let blob = build(0x0BAD);
        let result = keyset_with_next(&blob, 10).unwrap();
        assert_eq!(result, build(10), "only outer ctx7 may change");
        assert_eq!(keyset_next(&result), Some(10));
        assert_eq!(
            crate::kvs::parse_keyset_first_entry(&result, 2).unwrap(),
            ([0x31; 16], Some(0x1111))
        );
    }

    #[test]
    fn keyset_with_slot0_matches_serialize_keyset_for_mat_form() {
        // mat 1 スロット形の slot0 差し替えは serialize_keyset の作り直しとバイト一致。
        let mine = serialize_keyset(0, EPOCH_START_TIME, 7, &[0x42; 16], 5);
        assert_eq!(
            keyset_with_slot0(&mine, EPOCH_START_TIME, 9, &[0x43; 16]).unwrap(),
            serialize_keyset(0, EPOCH_START_TIME, 9, &[0x43; 16], 5)
        );
    }

    #[test]
    fn keyset_with_slot0_preserves_policy_other_slots_and_next() {
        let slots = [
            (1u64, 0xAAAAu16, [0xA0u8; 16]),
            (2, 0xBBBB, [0xB0; 16]),
            (3, 0xCCCC, [0xC0; 16]),
        ];
        let blob = multi_epoch_keyset(1, &slots, 0x0BAD);
        let expected =
            multi_epoch_keyset(1, &[(9, 0x1234, [0x11; 16]), slots[1], slots[2]], 0x0BAD);
        assert_eq!(
            keyset_with_slot0(&blob, 9, 0x1234, &[0x11; 16]).unwrap(),
            expected
        );
        assert_eq!(keyset_next(&expected), Some(0x0BAD));
        assert_eq!(
            crate::kvs::parse_keyset_first_entry(&expected, 2).unwrap(),
            ([0x11; 16], Some(0x1234))
        );
    }

    #[test]
    fn keyset_with_slot0_touches_only_slot0_ctx456() {
        // keyset_with_next_preserves_unknown_tags_order_and_nested_slots と同じ
        // 「未知タグ / スロット内 ctx7 / ネスト」fixture。slot 0 の ctx4/5/6 以外は
        // 1 バイトも変わらない。
        fn build(start: u64, hash: u16, key: &[u8; 16]) -> Vec<u8> {
            let mut w = Writer::new();
            w.start_struct(Tag::Anonymous);
            w.put_uint(Tag::Context(7), 0x0BAD); // chain next（先頭）
            w.put_uint(Tag::Context(9), 42); // 未知の追加タグ
            w.put_uint(Tag::Context(1), 1); // policy
            w.put_uint(Tag::Context(2), 2); // keys_count
            w.start_array(Tag::Context(3));
            w.start_struct(Tag::Anonymous);
            w.put_uint(Tag::Context(4), start);
            w.put_uint(Tag::Context(5), u64::from(hash));
            w.put_bytes(Tag::Context(6), key);
            w.put_uint(Tag::Context(7), 99); // スロット内 ctx7
            w.start_struct(Tag::Context(8));
            w.put_uint(Tag::Context(0), 7);
            w.end_container();
            w.end_container();
            w.start_struct(Tag::Anonymous); // slot 1（無傷で残る）
            w.put_uint(Tag::Context(4), 2);
            w.put_uint(Tag::Context(5), 0x2222);
            w.put_bytes(Tag::Context(6), &[0x32; 16]);
            w.end_container();
            w.end_container(); // ctx3 array
            w.end_container(); // outer struct
            w.finish()
        }
        let blob = build(1, 0x1111, &[0x31; 16]);
        assert_eq!(
            keyset_with_slot0(&blob, 5, 0x5555, &[0x55; 16]).unwrap(),
            build(5, 0x5555, &[0x55; 16])
        );
    }

    #[test]
    fn keyset_with_slot0_rejects_broken_blobs() {
        assert!(keyset_with_slot0(&[0x15, 0x18], 1, 1, &[0; 16]).is_none());
        let blob = serialize_keyset(0, 1, 7, &[0x42; 16], 5);
        assert!(keyset_with_slot0(&blob[..blob.len() - 3], 1, 1, &[0; 16]).is_none());
        // ctx3 配列が無い struct
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(1), 0);
        w.put_uint(Tag::Context(7), 0xFFFF);
        w.end_container();
        assert!(keyset_with_slot0(&w.finish(), 1, 1, &[0; 16]).is_none());
    }

    #[test]
    fn write_keyset_reprovision_keeps_chiptool_policy_and_epochs() {
        // 同じ keyset id で 2 回 provision（re-provision）。間に chip-tool 形
        // （policy 1、3 epoch）へ差し替えておくと、2 回目は slot 0 だけ差し替わる。
        let (_d, p) = tmp_ini("[Default]\n");
        provision(&p, 1, 10, false);
        let slots = [
            (1u64, 0x1111u16, [0x31u8; 16]),
            (2_000_000, 0x2222, [0x32; 16]),
            (3_000_000, 0x3333, [0x33; 16]),
        ];
        {
            let mut txn = crate::kvs::KvsTxn::open(&p).unwrap();
            txn.set("f/2/k/a", &multi_epoch_keyset(1, &slots, INVALID_KEYSET_ID));
            txn.commit().unwrap();
        }
        write_group_provision(
            &p,
            2,
            &CFID,
            &GroupProvisionWrite {
                group_id: 1,
                keyset_id: 10,
                name: "e2e",
                epoch_key: [0x77; 16],
                rebind: true,
            },
        )
        .unwrap();
        let txn = crate::kvs::KvsTxn::open(&p).unwrap();
        let op = derive_ipk_operational(&[0x77; 16], &CFID);
        let hash = derive_group_session_id(&op);
        assert_eq!(
            txn.get("f/2/k/a").unwrap().unwrap(),
            multi_epoch_keyset(
                1,
                &[(EPOCH_START_TIME, hash, op), slots[1], slots[2]],
                INVALID_KEYSET_ID
            )
        );
    }

    /// rotation テスト用: chip-tool 3 epoch 形の k/0 と mat epoch キーを持つ INI。
    #[allow(clippy::type_complexity)]
    fn rotation_fixture(
        cur: &[u8; 16],
    ) -> (
        tempfile::TempDir,
        std::path::PathBuf,
        [(u64, u16, [u8; 16]); 3],
    ) {
        let (d, p) = tmp_ini("[Default]\n");
        let op = derive_ipk_operational(cur, &CFID);
        let slots = [
            (EPOCH_START_TIME, derive_group_session_id(&op), op),
            (2_000_000, 0x2222, [0x32; 16]),
            (3_000_000, 0x3333, [0x33; 16]),
        ];
        let mut txn = crate::kvs::KvsTxn::open(&p).unwrap();
        txn.set("f/2/k/0", &multi_epoch_keyset(0, &slots, INVALID_KEYSET_ID));
        txn.set(&crate::kvs::mat_ipk_epoch_key(2), cur);
        txn.commit().unwrap();
        (d, p, slots)
    }

    #[test]
    fn ipk_rotation_begin_commit_round_trip() {
        use crate::kvs::{read_mat_ipk_epoch_slot, IpkEpochSlot};
        let cur = [0x0C; 16];
        let next = [0x0E; 16];
        let (_d, p, slots) = rotation_fixture(&cur);

        begin_ipk_rotation(&p, 2, &next).unwrap();
        assert_eq!(
            read_mat_ipk_epoch_slot(&p, 2, IpkEpochSlot::Next).unwrap(),
            Some(next)
        );
        // 同じ値での begin は冪等、違う値は Corrupt（並行実行）。
        begin_ipk_rotation(&p, 2, &next).unwrap();
        assert!(matches!(
            begin_ipk_rotation(&p, 2, &[0x0F; 16]),
            Err(GroupSettingsError::Corrupt { .. })
        ));

        commit_ipk_rotation(&p, 2, &CFID, &cur, &next).unwrap();
        assert_eq!(
            read_mat_ipk_epoch_slot(&p, 2, IpkEpochSlot::Current).unwrap(),
            Some(next)
        );
        assert_eq!(
            read_mat_ipk_epoch_slot(&p, 2, IpkEpochSlot::Prev).unwrap(),
            Some(cur)
        );
        assert_eq!(
            read_mat_ipk_epoch_slot(&p, 2, IpkEpochSlot::Next).unwrap(),
            None
        );
        let op_next = derive_ipk_operational(&next, &CFID);
        let txn = crate::kvs::KvsTxn::open(&p).unwrap();
        assert_eq!(
            txn.get("f/2/k/0").unwrap().unwrap(),
            multi_epoch_keyset(
                0,
                &[
                    (EPOCH_START_TIME, derive_group_session_id(&op_next), op_next),
                    slots[1],
                    slots[2]
                ],
                INVALID_KEYSET_ID
            ),
            "k/0: slot 0 だけ新 operational、残りスロットと next リンクは無傷"
        );
        // 読み側（CASE 用）も新 operational を見る。
        assert_eq!(
            crate::kvs::parse_keyset_first_entry(&txn.get("f/2/k/0").unwrap().unwrap(), 2)
                .unwrap()
                .0,
            op_next
        );
    }

    #[test]
    fn ipk_rotation_commit_refuses_mismatch_and_missing_state_without_writing() {
        let cur = [0x0C; 16];
        let next = [0x0E; 16];
        let (_d, p, _) = rotation_fixture(&cur);
        let before = std::fs::read(&p).unwrap();
        // pending 無し
        assert!(matches!(
            commit_ipk_rotation(&p, 2, &CFID, &cur, &next),
            Err(GroupSettingsError::Corrupt { .. })
        ));
        assert_eq!(std::fs::read(&p).unwrap(), before);
        // pending と違う next
        begin_ipk_rotation(&p, 2, &next).unwrap();
        let before = std::fs::read(&p).unwrap();
        assert!(matches!(
            commit_ipk_rotation(&p, 2, &CFID, &cur, &[0x0F; 16]),
            Err(GroupSettingsError::Corrupt { .. })
        ));
        assert_eq!(std::fs::read(&p).unwrap(), before);
        // k/0 欠落
        {
            let mut txn = crate::kvs::KvsTxn::open(&p).unwrap();
            txn.remove("f/2/k/0");
            txn.commit().unwrap();
        }
        let before = std::fs::read(&p).unwrap();
        assert!(matches!(
            commit_ipk_rotation(&p, 2, &CFID, &cur, &next),
            Err(GroupSettingsError::Corrupt { .. })
        ));
        assert_eq!(std::fs::read(&p).unwrap(), before);
    }

    #[test]
    fn ipk_rotation_abort_removes_pending_only() {
        use crate::kvs::{read_mat_ipk_epoch_slot, IpkEpochSlot};
        let cur = [0x0C; 16];
        let (_d, p, _) = rotation_fixture(&cur);
        assert!(!abort_ipk_rotation(&p, 2).unwrap(), "pending 無しは false");
        begin_ipk_rotation(&p, 2, &[0x0E; 16]).unwrap();
        assert!(abort_ipk_rotation(&p, 2).unwrap());
        assert_eq!(
            read_mat_ipk_epoch_slot(&p, 2, IpkEpochSlot::Next).unwrap(),
            None
        );
        assert_eq!(
            read_mat_ipk_epoch_slot(&p, 2, IpkEpochSlot::Current).unwrap(),
            Some(cur)
        );
    }

    #[test]
    fn ipk_rotation_locked_kvs_is_hard_error() {
        let (_d, p, _) = rotation_fixture(&[0x0C; 16]);
        let _held = crate::kvs::KvsTxn::open(&p).unwrap();
        assert!(matches!(
            begin_ipk_rotation(&p, 2, &[0x0E; 16]),
            Err(GroupSettingsError::Kvs(KvsError::Locked))
        ));
    }
}
