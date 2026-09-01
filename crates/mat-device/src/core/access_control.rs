//! AccessControl クラスタサーバ (spec §11.1, cluster 0x001F) — Apple Home
//! の interview がコミッショニング直後に必ず読み書きする、fabric スコープ
//! の ACL 帳簿。読みは commissioner 自身の admin エントリの確認、書きは
//! Apple がユーザーロール／自動化用に追加エントリを push する経路。
//!
//! `AclStore` は `AccessControlHandler`（EP0 のクラスタハンドラ）と
//! `CommissioningServer`（AddNOC の自動 admin エントリ、RemoveFabric /
//! fail-safe rollback の purge）の両方から触られる共有state —
//! `Arc<Mutex<..>>` で包み、`Clone` で配線先に配る（`FabricStore` 等
//! 既存の共有 state と同じ形）。永続化は `AclPersist` を注入する形で
//! `core::fabric_store::FabricPersist`/`FabricStore::with_persist` と同じ
//! パターン — `core` はファイル I/O を持ち込まず、具象（JSON ファイル）は
//! `crate::net::store::FileAclStore` にある。save 失敗は `tracing::warn`
//! して続行する（fabric 撤去等の後始末をブロックしない — 復旧はコント
//! ローラの再 write で可能。
//! `FabricStore::insert`のロールバック（save 失敗で in-memory も戻す）とは
//! 非対称 — あちらは fabric-index の払い出しなど整合性が壊れうる箇所を
//! 守る必要があるが、こちらは ACL が直前の状態のまま残っても「古い正当な
//! 状態」であり安全性は落ちない）。
use std::sync::{Arc, Mutex};

use mat_controller::cert::{cat_identifier, cat_version, CaseAuthTags};

use mat_controller::im;
use mat_controller::sync::locked;
use mat_controller::tlv::{Reader, Tag, Value, Writer};
use serde::{Deserialize, Serialize};

use crate::core::datamodel::{ClusterHandler, InvokeCtx, InvokeReply, ReadCtx};

/// `AccessControlEntryPrivilegeEnum` (spec §11.1.7.1) の全値。`check` の
/// privilege lattice（`privilege_grants`）と AddNOC 自動 admin エントリの
/// 両方がこれを使う。
pub(crate) const PRIVILEGE_VIEW: u8 = 1;
pub(crate) const PRIVILEGE_PROXY_VIEW: u8 = 2;
pub(crate) const PRIVILEGE_OPERATE: u8 = 3;
pub(crate) const PRIVILEGE_MANAGE: u8 = 4;
pub(crate) const PRIVILEGE_ADMINISTER: u8 = 5;

/// `AccessControlEntryAuthModeEnum` (spec §11.1.7.1) のうちこの実装が扱う
/// 唯一の値 — CASE。Group/PASE の auth mode は `check`/`decode_acl_entry_body`
/// のどちらも意味的に扱わない（PASE は fabric を持たないので ACL の対象
/// 外、Group は subject 解決を実装していない）。
pub(crate) const AUTH_MODE_CASE: u8 = 2;

/// `SubjectsPerAccessControlEntry`/`TargetsPerAccessControlEntry`/
/// `AccessControlEntriesPerFabric` (spec §11.1.5) — 固定値を返すのみで
/// 実容量は追跡しない（M2/M3 のどの経路も限界に近づかない）。
const ACL_SUBJECTS_PER_ENTRY: u64 = 4;
const ACL_TARGETS_PER_ENTRY: u64 = 3;

/// `AccessControlEntriesPerFabric` (spec §11.1.5) の申告値であり、かつ
/// `write` の容量ガードが実際に強制する上限でもある — 単一の定数で
/// 「申告と実装が食い違う」を構造的にあり得なくする。
pub(crate) const ACL_ENTRIES_PER_FABRIC: usize = 4;

/// Upper 32 bits of a CASE-Authenticated-Tag subject in an ACL entry
/// (spec §6.6.2.1.2: `0xFFFF_FFFD_hhhh_vvvv`, `hhhh` = CAT identifier,
/// `vvvv` = minimum version).
pub const CAT_SUBJECT_PREFIX: u64 = 0xFFFF_FFFD;

/// The ACL subject value that names CAT `cat` (identifier + version) —
/// what a controller puts in `AddNOC.CaseAdminSubject` or an ACL entry's
/// `Subjects` to grant by tag instead of by node id.
pub fn cat_subject(cat: u32) -> u64 {
    (CAT_SUBJECT_PREFIX << 32) | u64::from(cat)
}

/// The authenticated identity of one CASE session, as the ACL sees it
/// (spec §6.6.2): the peer's operational node id plus the CASE
/// Authenticated Tags in its NOC. Built once per session from the
/// verified Sigma3 NOC (`net::case` → `SecureSession::peer_cats`) and
/// carried by value in every `ReadCtx` / `InvokeCtx`. `Default` (node 0,
/// no CATs) is the PASE placeholder — PASE bypasses `AclStore::check`
/// entirely, so it never matches anything.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Subject {
    pub node_id: u64,
    pub cats: CaseAuthTags,
}

impl Subject {
    pub fn new(node_id: u64, cats: CaseAuthTags) -> Self {
        Self { node_id, cats }
    }

    /// A subject identified by node id only (a NOC without CATs).
    pub fn node(node_id: u64) -> Self {
        Self::new(node_id, CaseAuthTags::default())
    }

    /// Whether one ACL entry subject (`AccessControlEntryStruct.Subjects`
    /// element) names this session. Two shapes (spec §6.6.2.1.2):
    ///
    /// - a CAT subject (`CAT_SUBJECT_PREFIX` in the upper 32 bits) matches
    ///   when the NOC carries a CAT with the **same identifier** and a
    ///   **version at or above** the subject's — and never when the
    ///   subject's version is 0, which the spec reserves as invalid;
    /// - anything else is an operational node id and must equal the
    ///   peer's node id exactly. A node id whose bits happen to fall in the
    ///   CAT range is judged as a CAT, never as a node id.
    pub fn matches(&self, acl_subject: u64) -> bool {
        if acl_subject >> 32 != CAT_SUBJECT_PREFIX {
            return acl_subject == self.node_id;
        }
        let wanted = acl_subject as u32;
        if cat_version(wanted) == 0 {
            return false;
        }
        self.cats.iter().any(|have| {
            cat_identifier(have) == cat_identifier(wanted)
                && cat_version(have) >= cat_version(wanted)
        })
    }
}

/// デバイス上の 1 ACL エントリ (`AccessControlEntryStruct`, spec
/// §11.1.7.1)。`targets_raw` は Context(4) の値要素をそのまま
/// `Tag::Anonymous` に再タグして保持した raw TLV（`None` = null =
/// ターゲット制限なし）— `TargetStruct` の各フィールド
/// (cluster/endpoint/device_type のいずれも optional/null 可) を
/// mat-device 側で意味的にデコードする必要がないための passthrough。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AclDeviceEntry {
    pub privilege: u8,
    pub auth_mode: u8,
    pub subjects: Vec<u64>,
    pub targets_raw: Option<Vec<u8>>,
    pub fabric_index: u8,
}

/// Persistence boundary `core` calls through instead of touching a
/// filesystem directly — same shape as `core::fabric_store::FabricPersist`
/// (see that trait's doc for the rationale of whole-table save/load and
/// `: Send`/object-safety). The one behavioral difference from
/// `FabricPersist` lives in `AclStore`'s callers, not this trait: a save
/// failure here is logged and ignored rather than propagated (module doc).
pub trait AclPersist: Send {
    fn save(&self, entries: &[AclDeviceEntry]) -> Result<(), String>;
    fn load(&self) -> Result<Vec<AclDeviceEntry>, String>;
}

/// `AclStore`'s guarded state: the entry table plus the (optional)
/// persistence backend saved to after every mutation.
#[derive(Default)]
struct AclInner {
    entries: Vec<AclDeviceEntry>,
    persist: Option<Box<dyn AclPersist>>,
}

/// ACL ストア。fabric をまたいだ 1 本の `Vec` — エントリ数がたかだか数十の
/// オーダーなので、read/write のたびに対象 fabric だけ filter/retain する
/// 素朴な実装で十分（`FabricStore`と同格の単純さ）。永続化は任意
/// （`new` は非永続、`with_persist` は注入した `AclPersist` に毎 mutation
/// 後 save する — `FabricStore` と同じ二段構え）。
#[derive(Clone, Default)]
pub struct AclStore(Arc<Mutex<AclInner>>);

impl AclStore {
    /// 非永続の空ストア。
    pub fn new() -> Self {
        Self::default()
    }

    /// `persist` に既にあるものを load して始める永続ストア。load 失敗
    /// (I/O エラー・破損 JSON いずれも) は空扱い — 初回起動と区別しない
    /// （`FabricStore::with_persist` と同じ裁定、doc 参照）。
    pub fn with_persist(persist: Box<dyn AclPersist>) -> Self {
        let entries = persist.load().unwrap_or_default();
        Self(Arc::new(Mutex::new(AclInner {
            entries,
            persist: Some(persist),
        })))
    }

    /// AddNOC (spec §11.17.6.8) の自動 admin エントリ: 新規 fabric の
    /// CASE admin subject に Administer 権限、ターゲット制限なしのエントリ
    /// を 1 件追加する（このエントリを ACL に自動で持たない device は、
    /// コミッショナ自身が二度と自分の書いた ACL を読み書きできなくなる）。
    pub fn add_case_admin(&self, fabric_index: u8, case_admin_subject: u64) {
        let mut guard = self.lock();
        guard.entries.push(AclDeviceEntry {
            privilege: PRIVILEGE_ADMINISTER,
            auth_mode: AUTH_MODE_CASE,
            subjects: vec![case_admin_subject],
            targets_raw: None,
            fabric_index,
        });
        Self::save(&guard);
    }

    /// fabric 撤去時の purge（RemoveFabric / fail-safe rollback 共用）。
    pub fn purge_fabric(&self, fabric_index: u8) {
        let mut guard = self.lock();
        guard.entries.retain(|e| e.fabric_index != fabric_index);
        Self::save(&guard);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, AclInner> {
        locked(&self.0)
    }

    /// 現在の `guard.entries` を persist backend に保存する（backend 未設定
    /// なら何もしない）。save 失敗はモジュール doc の裁定どおり
    /// `tracing::warn` して続行 — 呼び出し元のミューテーションはロール
    /// バックしない。
    fn save(guard: &AclInner) {
        if let Some(persist) = &guard.persist {
            if let Err(e) = persist.save(&guard.entries) {
                tracing::warn!("acl store save failed: {e}");
            }
        }
    }

    /// ACL 判定 (spec §9.10, Access Control のポリシー本体):
    /// `fabric_index`/`subject`/`required_privilege`/`endpoint`/`cluster` の
    /// 5 つ組を、fabric スコープのエントリ群のうち **いずれか 1 件でも**
    /// 満たせば許可する。エントリごとの条件は AND:
    /// - `fabric_index` が一致する
    /// - `auth_mode` が CASE（`AUTH_MODE_CASE` = 2）— この実装が持つ唯一の
    ///   auth mode で、Group/PASE の subject は扱わない
    /// - `subjects` が空（wildcard）か、いずれかの要素が `subject` に
    ///   マッチする（`Subject::matches`: node id は完全一致、CAT subject
    ///   `0xFFFF_FFFD_hhhh_vvvv` は NOC の CAT と identifier 一致かつ
    ///   version が vvvv 以上。vvvv = 0 は spec 上無効で誰にもマッチしない）
    /// - `privilege_grants(e.privilege, required_privilege)` が true
    /// - target が一致する（`targets_match`、下記）
    ///
    /// `fabric_index == 0` は呼び出し側（PASE bypass）が `check` 自体を
    /// 呼ばない前提だが、防御的に常に `false` を返す — PASE セッションは
    /// どのエントリの `fabric_index` とも一致しないので自然に `false` に
    /// なるはずだが、0 を「一致条件を持たない」特別な値として扱うバグを
    /// 将来のエントリ走査ロジックの変更が持ち込む余地を先に潰しておく。
    pub fn check(
        &self,
        fabric_index: u8,
        subject: Subject,
        required_privilege: u8,
        endpoint: u16,
        cluster: u32,
    ) -> bool {
        if fabric_index == 0 {
            return false;
        }
        self.lock().entries.iter().any(|e| {
            e.fabric_index == fabric_index
                && e.auth_mode == AUTH_MODE_CASE
                && (e.subjects.is_empty() || e.subjects.iter().any(|&s| subject.matches(s)))
                && privilege_grants(e.privilege, required_privilege)
                && targets_match(&e.targets_raw, endpoint, cluster)
        })
    }

    fn entries_for(&self, fabric_index: u8) -> Vec<AclDeviceEntry> {
        self.lock()
            .entries
            .iter()
            .filter(|e| e.fabric_index == fabric_index)
            .cloned()
            .collect()
    }

    /// 書き込み fabric のエントリを丸ごと入れ替える（write の全置換径路）。
    fn replace_fabric(&self, fabric_index: u8, entries: Vec<AclDeviceEntry>) {
        let mut guard = self.lock();
        guard.entries.retain(|e| e.fabric_index != fabric_index);
        guard.entries.extend(entries);
        Self::save(&guard);
    }

    /// 書き込み fabric の末尾に 1 件足す（write の ListIndex null append
    /// 径路）。
    fn append_to_fabric(&self, entry: AclDeviceEntry) {
        let mut guard = self.lock();
        guard.entries.push(entry);
        Self::save(&guard);
    }

    #[cfg(test)]
    /// テスト専用: 任意の privilege/subject のエントリ列を fabric に据える。
    /// production の投入経路は AddNOC 由来の `add_case_admin`（常に
    /// Administer）と `write` の 2 つだけなので、enforcement を検証する
    /// 側（`datamodel` の ACL テスト）が「Operate だけの subject」のような
    /// 中間状態を組むにはこれが要る。
    pub(crate) fn set_entries_for_test(&self, fabric_index: u8, entries: Vec<AclDeviceEntry>) {
        self.replace_fabric(fabric_index, entries);
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
            im::ATTR_ACL_ENTRIES_PER_FABRIC => Some(uint_value(ACL_ENTRIES_PER_FABRIC as u64)),
            _ => None,
        }
    }

    fn invoke(&mut self, _command: u32, _fields_tlv: &[u8], _ctx: &mut InvokeCtx) -> InvokeReply {
        // ReviewFabricRestrictions (spec §11.1.7.6.1) は ARL feature 限定
        // — feature_map()=0 のこの実装には accepted command が無い。
        InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
    }

    /// write の対象は `ATTR_ACL` のみ。属性ルーティングの次に 2 つの
    /// ガードを見る: (1) `ctx.fabric_index == 0` — PASE セッションは
    /// fabric を持たず ACL を書けない（spec §11.1.7.1 の subject
    /// resolution 上、fabric なしでは書き込み自体が成立しない）ので
    /// `STATUS_UNSUPPORTED_ACCESS`。(2) 容量 — 全置換パスは送られた
    /// エントリ数、append パスは書き込み fabric の既存エントリ数で
    /// `ACL_ENTRIES_PER_FABRIC` を超えるか判定し、超えるなら
    /// `STATUS_RESOURCE_EXHAUSTED`（`ATTR_ACL_ENTRIES_PER_FABRIC` が
    /// 申告する値と同じ定数）。いずれのガードも通った上で、`list_append`
    /// で「エントリ全置換」と「1 件 append」を切り替える — どちらも
    /// decode 失敗は `STATUS_CONSTRAINT_ERROR`、書き込み fabric のエントリ
    /// の `fabric_index` フィールドは無視して `ctx.fabric_index` で上書き
    /// する（commissioner が書く値を信用しない）。
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
        if ctx.fabric_index == 0 {
            return Err(im::STATUS_UNSUPPORTED_ACCESS);
        }
        if list_append {
            let Some(entry) = decode_single_acl_entry(data_tlv) else {
                return Err(im::STATUS_CONSTRAINT_ERROR);
            };
            if self.store.entries_for(ctx.fabric_index).len() >= ACL_ENTRIES_PER_FABRIC {
                return Err(im::STATUS_RESOURCE_EXHAUSTED);
            }
            self.store.append_to_fabric(AclDeviceEntry {
                fabric_index: ctx.fabric_index,
                ..entry
            });
        } else {
            let Some(entries) = decode_acl_entries(data_tlv) else {
                return Err(im::STATUS_CONSTRAINT_ERROR);
            };
            if entries.len() > ACL_ENTRIES_PER_FABRIC {
                return Err(im::STATUS_RESOURCE_EXHAUSTED);
            }
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

    /// spec §11.1.5 のアクセス表: `ACL` は read/write とも Administer
    /// （ACL 自身を読み書きできる者は事実上その fabric の管理者）。容量
    /// 系の 3 属性は View のまま（trait default）。
    fn read_privilege(&self, attribute: u32) -> u8 {
        match attribute {
            im::ATTR_ACL => PRIVILEGE_ADMINISTER,
            _ => PRIVILEGE_VIEW,
        }
    }

    /// 書ける属性は `ATTR_ACL` だけ（他は `write` が
    /// `STATUS_UNSUPPORTED_WRITE`）なので、属性を問わず Administer。
    fn write_privilege(&self, _attribute: u32) -> u8 {
        PRIVILEGE_ADMINISTER
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
                mat_controller::tlv::copy_value(&mut w, r, Tag::Anonymous, Value::ArrayStart)
                    .ok()?;
                targets_raw = Some(w.finish());
            }
            (Tag::Context(254), Value::Uint(v)) => fabric_index = u8::try_from(v).ok()?,
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                mat_controller::tlv::skip_container(r).ok()?;
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

/// privilege lattice (spec §11.1.7.1 Table "Privilege Semantics"): エントリ
/// の `privilege` が要求 privilege を含むかどうか。Administer は全部、
/// Manage は {Manage,Operate,View}、Operate は {Operate,View}、ProxyView は
/// {ProxyView,View}、View は {View} のみ。未知の `entry_privilege` 値
/// （decode で弾かれるはずだが防御的に）は何も grant しない。
pub(crate) fn privilege_grants(entry_privilege: u8, required: u8) -> bool {
    match entry_privilege {
        PRIVILEGE_ADMINISTER => true,
        PRIVILEGE_MANAGE => matches!(
            required,
            PRIVILEGE_MANAGE | PRIVILEGE_OPERATE | PRIVILEGE_VIEW
        ),
        PRIVILEGE_OPERATE => matches!(required, PRIVILEGE_OPERATE | PRIVILEGE_VIEW),
        PRIVILEGE_PROXY_VIEW => matches!(required, PRIVILEGE_PROXY_VIEW | PRIVILEGE_VIEW),
        PRIVILEGE_VIEW => required == PRIVILEGE_VIEW,
        _ => false,
    }
}

/// `TargetStruct` (spec §11.1.7.1) 1 件分をデコードした形。3 フィールド
/// いずれも optional/null 可 — `None` は「このフィールドでは絞り込まない」
/// (`decode_targets` の doc も参照)。
pub(crate) struct AclTargetDev {
    pub cluster: Option<u32>,
    pub endpoint: Option<u16>,
    pub device_type: Option<u32>,
}

/// `raw` は `AclDeviceEntry::targets_raw`（`Tag::Anonymous` に再タグされた
/// `TargetStruct` array の raw TLV — 上の `AclDeviceEntry` の doc 参照）。
/// ArrayStart → 各
/// StructStart（`Context(0)`=cluster / `Context(1)`=endpoint /
/// `Context(2)`=device_type、いずれも `Null` 可）→ ContainerEnd。
/// パース不能（想定外の形、truncated 等）は `None` — 呼び出し側
/// (`targets_match`) はこれを「どの target にもマッチしない」= 拒否側の
/// 安全なフォールバックとして扱う。
pub(crate) fn decode_targets(raw: &[u8]) -> Option<Vec<AclTargetDev>> {
    let mut r = Reader::new(raw);
    let el = r.next().ok()??;
    if el.value != Value::ArrayStart {
        return None;
    }
    let mut targets = Vec::new();
    loop {
        let el = r.next().ok()??;
        match el.value {
            Value::ContainerEnd => break,
            Value::StructStart => targets.push(decode_acl_target_body(&mut r)?),
            _ => return None,
        }
    }
    Some(targets)
}

fn decode_acl_target_body(r: &mut Reader) -> Option<AclTargetDev> {
    let mut cluster = None;
    let mut endpoint = None;
    let mut device_type = None;
    loop {
        let el = r.next().ok()??;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::Uint(v)) => cluster = Some(u32::try_from(v).ok()?),
            (Tag::Context(0), Value::Null) => {}
            (Tag::Context(1), Value::Uint(v)) => endpoint = Some(u16::try_from(v).ok()?),
            (Tag::Context(1), Value::Null) => {}
            (Tag::Context(2), Value::Uint(v)) => device_type = Some(u32::try_from(v).ok()?),
            (Tag::Context(2), Value::Null) => {}
            _ => return None,
        }
    }
    Some(AclTargetDev {
        cluster,
        endpoint,
        device_type,
    })
}

/// `check` の target 条件: `targets_raw: None` は無制限（全 endpoint/
/// cluster に一致）。`Some` はデコードして、いずれか 1 件の target が
/// `cluster`/`endpoint` の両方を満たせば一致 — `device_type` 制約付きの
/// target は（このデバイス側で device type 解決を実装していないので）
/// 常に不一致扱い、安全側に倒す（設計メモ通り）。デコード失敗
/// (`decode_targets` が `None`) も同様に不一致扱い。
fn targets_match(targets_raw: &Option<Vec<u8>>, endpoint: u16, cluster: u32) -> bool {
    let Some(raw) = targets_raw else {
        return true;
    };
    let Some(targets) = decode_targets(raw) else {
        return false;
    };
    targets.iter().any(|t| {
        t.cluster.is_none_or(|c| c == cluster)
            && t.endpoint.is_none_or(|ep| ep == endpoint)
            && t.device_type.is_none()
    })
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

    #[test]
    fn write_from_pase_session_is_unsupported_access() {
        let store = AclStore::new();
        let mut h = AccessControlHandler::new(store);
        let mut ctx = InvokeCtx {
            fabric_index: 0,
            ..InvokeCtx::default()
        };
        let data = encode_entries_for_test(&[]);
        assert_eq!(
            h.write(im::ATTR_ACL, &data, false, &mut ctx),
            Err(im::STATUS_UNSUPPORTED_ACCESS)
        );
        assert!(ctx.changed.is_empty());
    }

    #[test]
    fn full_replace_over_capacity_is_resource_exhausted() {
        let store = AclStore::new();
        let mut h = AccessControlHandler::new(store);
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            ..InvokeCtx::default()
        };
        // ACL_ENTRIES_PER_FABRIC(=4) を 1 件超える 5 エントリの全置換。
        let five = encode_entries_for_test(&[
            (5, 2, vec![1]),
            (5, 2, vec![2]),
            (5, 2, vec![3]),
            (5, 2, vec![4]),
            (5, 2, vec![5]),
        ]);
        assert_eq!(
            h.write(im::ATTR_ACL, &five, false, &mut ctx),
            Err(im::STATUS_RESOURCE_EXHAUSTED)
        );
        assert!(ctx.changed.is_empty());
        assert!(decode_entries_for_test(&h.read(im::ATTR_ACL, &read_ctx(1)).unwrap()).is_empty());
    }

    #[test]
    fn append_to_already_full_fabric_is_resource_exhausted() {
        let store = AclStore::new();
        let mut h = AccessControlHandler::new(store);
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            ..InvokeCtx::default()
        };
        // ちょうど ACL_ENTRIES_PER_FABRIC(=4) 件まで全置換で埋める。
        let four = encode_entries_for_test(&[
            (5, 2, vec![1]),
            (5, 2, vec![2]),
            (5, 2, vec![3]),
            (5, 2, vec![4]),
        ]);
        assert_eq!(h.write(im::ATTR_ACL, &four, false, &mut ctx), Ok(()));
        ctx.changed.clear();

        // 5 件目の append は容量超過。
        let entry5 = encode_entry_for_test(5, 2, vec![5]);
        assert_eq!(
            h.write(im::ATTR_ACL, &entry5, true, &mut ctx),
            Err(im::STATUS_RESOURCE_EXHAUSTED)
        );
        assert!(ctx.changed.is_empty());
        assert_eq!(
            decode_entries_for_test(&h.read(im::ATTR_ACL, &read_ctx(1)).unwrap()).len(),
            4
        );
    }

    /// テスト内 `AclPersist`: 呼び出し元と共有した `Arc<Mutex<Vec>>` を
    /// 裏の「ディスク」代わりに使う — 別インスタンスの `AclStore` が同じ
    /// `Arc` を渡されれば、save された内容を独立に load できる（実ファイル
    /// を介さずに `AclStore::with_persist` の注入契約だけを検証する）。
    struct MemAclPersist(Arc<Mutex<Vec<AclDeviceEntry>>>);

    impl AclPersist for MemAclPersist {
        fn save(&self, entries: &[AclDeviceEntry]) -> Result<(), String> {
            *self.0.lock().unwrap() = entries.to_vec();
            Ok(())
        }

        fn load(&self) -> Result<Vec<AclDeviceEntry>, String> {
            Ok(self.0.lock().unwrap().clone())
        }
    }

    #[test]
    fn add_case_admin_persists_and_reloads_in_a_new_instance() {
        let backing = Arc::new(Mutex::new(Vec::new()));
        let store = AclStore::with_persist(Box::new(MemAclPersist(Arc::clone(&backing))));
        store.add_case_admin(1, 112233);

        // 新しい `AclStore` インスタンスが同じ backing を load すると、
        // save された内容がそのまま見える。
        let store2 = AclStore::with_persist(Box::new(MemAclPersist(backing)));
        let h = AccessControlHandler::new(store2);
        let entries = decode_entries_for_test(&h.read(im::ATTR_ACL, &read_ctx(1)).unwrap());
        assert_eq!(entries, vec![(5u8, 2u8, vec![112233u64], 1u8)]);
    }

    #[test]
    fn purge_fabric_saves_the_removal() {
        let backing = Arc::new(Mutex::new(Vec::new()));
        let store = AclStore::with_persist(Box::new(MemAclPersist(Arc::clone(&backing))));
        store.add_case_admin(1, 111);
        store.add_case_admin(2, 222);
        store.purge_fabric(1);

        let store2 = AclStore::with_persist(Box::new(MemAclPersist(backing)));
        let h = AccessControlHandler::new(store2);
        assert!(decode_entries_for_test(&h.read(im::ATTR_ACL, &read_ctx(1)).unwrap()).is_empty());
        assert_eq!(
            decode_entries_for_test(&h.read(im::ATTR_ACL, &read_ctx(2)).unwrap()).len(),
            1
        );
    }

    #[test]
    fn write_replace_and_append_both_persist() {
        let backing = Arc::new(Mutex::new(Vec::new()));
        let store = AclStore::with_persist(Box::new(MemAclPersist(Arc::clone(&backing))));
        let mut h = AccessControlHandler::new(store);
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            ..InvokeCtx::default()
        };
        let data = encode_entries_for_test(&[(5, 2, vec![111])]);
        assert_eq!(h.write(im::ATTR_ACL, &data, false, &mut ctx), Ok(()));
        ctx.changed.clear();
        let entry2 = encode_entry_for_test(3, 2, vec![222]);
        assert_eq!(h.write(im::ATTR_ACL, &entry2, true, &mut ctx), Ok(()));

        let store2 = AclStore::with_persist(Box::new(MemAclPersist(backing)));
        let h2 = AccessControlHandler::new(store2);
        let entries = decode_entries_for_test(&h2.read(im::ATTR_ACL, &read_ctx(1)).unwrap());
        assert_eq!(
            entries,
            vec![(5u8, 2u8, vec![111u64], 1u8), (3u8, 2u8, vec![222u64], 1u8)]
        );
    }

    /// save が常に失敗する `AclPersist` — mutation 自体は in-memory では
    /// そのまま成立し続けることを確認する（モジュール doc の裁定:
    /// save 失敗は warn して続行、ロールバックしない）。
    struct FailingAclPersist;

    impl AclPersist for FailingAclPersist {
        fn save(&self, _entries: &[AclDeviceEntry]) -> Result<(), String> {
            Err("disk full".to_string())
        }

        fn load(&self) -> Result<Vec<AclDeviceEntry>, String> {
            Ok(Vec::new())
        }
    }

    #[test]
    fn mutation_still_applies_in_memory_when_persist_save_fails() {
        let store = AclStore::with_persist(Box::new(FailingAclPersist));
        store.add_case_admin(1, 111);
        let h = AccessControlHandler::new(store);
        assert_eq!(
            decode_entries_for_test(&h.read(im::ATTR_ACL, &read_ctx(1)).unwrap()).len(),
            1
        );
    }

    #[test]
    fn check_matches_subject_privilege_and_targets() {
        let store = AclStore::new();
        store.add_case_admin(1, 112233); // Administer / CASE / 制限なし
        assert!(store.check(1, Subject::node(112233), PRIVILEGE_ADMINISTER, 0, 0x001F));
        assert!(store.check(1, Subject::node(112233), PRIVILEGE_VIEW, 2, 0x0006));
        assert!(!store.check(1, Subject::node(999), PRIVILEGE_VIEW, 2, 0x0006)); // subject 不一致
        assert!(!store.check(2, Subject::node(112233), PRIVILEGE_VIEW, 2, 0x0006)); // fabric 不一致
        assert!(!store.check(0, Subject::node(112233), PRIVILEGE_VIEW, 2, 0x0006));
        // fabric 0 は常に false
    }

    #[test]
    fn check_respects_privilege_lattice() {
        let store = AclStore::new();
        store.replace_fabric(
            1,
            vec![AclDeviceEntry {
                privilege: PRIVILEGE_OPERATE,
                auth_mode: 2,
                subjects: vec![5],
                targets_raw: None,
                fabric_index: 1,
            }],
        );
        assert!(store.check(1, Subject::node(5), PRIVILEGE_VIEW, 2, 0x0006));
        assert!(store.check(1, Subject::node(5), PRIVILEGE_OPERATE, 2, 0x0006));
        assert!(!store.check(1, Subject::node(5), PRIVILEGE_MANAGE, 2, 0x0006));
        assert!(!store.check(1, Subject::node(5), PRIVILEGE_ADMINISTER, 0, 0x001F));
    }

    #[test]
    fn check_with_cluster_target_limits_scope() {
        // targets_raw は write と同形の raw TLV を Writer で組む:
        // array[ struct{ Context(0)=0x0006 } ]（cluster=OnOff のみ許可）
        let mut w = Writer::new();
        w.start_array(Tag::Anonymous);
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 0x0006);
        w.end_container();
        w.end_container();
        let store = AclStore::new();
        store.replace_fabric(
            1,
            vec![AclDeviceEntry {
                privilege: PRIVILEGE_OPERATE,
                auth_mode: 2,
                subjects: vec![5],
                targets_raw: Some(w.finish()),
                fabric_index: 1,
            }],
        );
        assert!(store.check(1, Subject::node(5), PRIVILEGE_OPERATE, 2, 0x0006));
        assert!(!store.check(1, Subject::node(5), PRIVILEGE_OPERATE, 2, 0x0008));
        // 他 cluster は拒否
    }

    #[test]
    fn empty_subjects_entry_is_a_wildcard() {
        let store = AclStore::new();
        store.replace_fabric(
            1,
            vec![AclDeviceEntry {
                privilege: PRIVILEGE_VIEW,
                auth_mode: 2,
                subjects: vec![],
                targets_raw: None,
                fabric_index: 1,
            }],
        );
        assert!(store.check(1, Subject::node(424242), PRIVILEGE_VIEW, 2, 0x0006));
    }

    // --- CASE Authenticated Tag subjects (spec §6.6.2.1.2) ---

    /// A session for node 999 whose NOC carries exactly one CAT.
    fn session_with_cat(cat: u32) -> Subject {
        Subject::new(999, CaseAuthTags::new(&[cat]).unwrap())
    }

    /// The Apple Home shape: AddNOC's `CaseAdminSubject` is a CAT subject,
    /// so the auto-created admin entry's only subject is that CAT — the
    /// admin's later CASE sessions must match it through their NOC's CATs
    /// (same identifier, version at or above the entry's), never through
    /// the node id.
    #[test]
    fn check_matches_a_cat_subject_by_identifier_and_minimum_version() {
        let store = AclStore::new();
        store.add_case_admin(1, cat_subject(0xABCD_0002));
        let admin = |subject: Subject| store.check(1, subject, PRIVILEGE_ADMINISTER, 0, 0x001F);
        assert!(admin(session_with_cat(0xABCD_0002)), "same version");
        assert!(admin(session_with_cat(0xABCD_0007)), "newer version");
        assert!(!admin(session_with_cat(0xABCD_0001)), "older version");
        assert!(!admin(session_with_cat(0x1234_0002)), "other identifier");
        assert!(!admin(Subject::node(999)), "no CATs at all");
        assert!(
            !admin(Subject::node(cat_subject(0xABCD_0002))),
            "a node id that merely equals the CAT subject's bits is not a CAT"
        );
    }

    #[test]
    fn check_matches_any_of_several_session_cats() {
        let store = AclStore::new();
        store.add_case_admin(1, cat_subject(0x0002_0001));
        let cats = CaseAuthTags::new(&[0x0001_0001, 0x0002_0003, 0x0003_0001]).unwrap();
        assert!(store.check(1, Subject::new(999, cats), PRIVILEGE_ADMINISTER, 0, 0x001F));
    }

    #[test]
    fn cat_entry_with_version_zero_never_matches() {
        let store = AclStore::new();
        store.add_case_admin(1, cat_subject(0xABCD_0000));
        assert!(!store.check(1, session_with_cat(0xABCD_0001), PRIVILEGE_VIEW, 2, 0x0006));
    }

    #[test]
    fn node_id_entry_still_matches_a_session_that_also_has_cats() {
        let store = AclStore::new();
        store.add_case_admin(1, 999);
        assert!(store.check(1, session_with_cat(0xABCD_0001), PRIVILEGE_VIEW, 2, 0x0006));
        assert!(!store.check(
            1,
            Subject::new(998, CaseAuthTags::new(&[0xABCD_0001]).unwrap()),
            PRIVILEGE_VIEW,
            2,
            0x0006
        ));
    }

    #[test]
    fn cat_subject_encodes_the_spec_prefix() {
        assert_eq!(cat_subject(0xABCD_0002), 0xFFFF_FFFD_ABCD_0002);
    }
}
