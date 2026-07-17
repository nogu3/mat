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

use std::path::Path;

use crate::fabric::{derive_group_session_id, derive_ipk_operational};
use crate::kvs::{KvsError, KvsTxn};
use crate::tlv::{Reader, Tag, Value, Writer};

/// keyset リンクの終端（上流 kInvalidKeysetId — id 0 は IPK で有効値）。
const INVALID_KEYSET_ID: u16 = 0xFFFF;
/// endpoint 無しの GroupData first_endpoint（上流 kInvalidEndpointId）。
const INVALID_ENDPOINT_ID: u16 = 0xFFFF;
/// KeySetData の operational key 配列は常に 3 スロット（KeySet::kEpochKeysMax）。
const KEYSET_SLOTS: usize = 3;
/// デバイス側 epochStartTime0 と一致させる（mat-core::group::EPOCH_START_TIME = "1"）。
const EPOCH_START_TIME: u64 = 1;
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
struct FabricData {
    first_group: u16,
    group_count: u16,
    first_map: u16,
    map_count: u16,
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

fn parse_fabric_data(blob: &[u8]) -> Option<FabricData> {
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
/// チェーン内 next）。タグは `kvs::parse_keymap_entry` と同じ ctx1/ctx2 に
/// ctx3(next) を足しただけ — 読み側の寛容走査（未知タグ skip）と互換。
#[derive(Clone, Copy)]
struct KeyMap {
    group_id: u16,
    keyset_id: u16,
    next: u16,
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

fn parse_keymap(blob: &[u8]) -> Option<KeyMap> {
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
fn serialize_keyset(policy: u16, start_time: u64, hash: u16, key: &[u8; 16], next: u16) -> Vec<u8> {
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
            let next = keyset_next(&blob).ok_or_else(|| corrupt(&key, "unparseable KeySetData"))?;
            txn.set(
                &key,
                &serialize_keyset(0, EPOCH_START_TIME, hash, operational, next),
            );
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
        if let Some(pos) = entries
            .iter()
            .position(|(_, km)| km.group_id == group_id && km.keyset_id == keyset_id)
        {
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
            }
            fabric.map_count -= 1;
            entries.remove(pos);
        }
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
        // keyset id 0（IPK）が既にチェーンにいる状態（jarvis 実機と同型）を
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
}
