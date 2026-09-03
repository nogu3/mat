# matv subject 検査 + subscribe 残 2 件（監査レーン A フェーズ 1）設計

日付: 2026-09-03 / 対象ブランチ: worktree-matv-subject-checks（base: main b68e3ab = v1.30.0）
触る範囲: `crates/mat-device` のみ（他レーン B/C/D と並行のため mat-core /
mat-controller / mat-native / crates/mat / matd には触らない）。

## 0. 背景

監査バックログ（2026-08-31）レーン A の前半。CAT subject 対応（main 17a4197）の
レビューで残った 2 件と、③（groupcast/ACL enforcement）で spec スコープ外に
送った subscribe の 2 件を閉じる。groupcast 実受信はフェーズ 2（別 worktree、
別 spec）。

現状の事実（コード確認済み）:

- `core/commissioning.rs::handle_add_noc` は `CaseAdminSubject` を無検査で
  `FabricEntry.admin_subject` と `AclStore::add_case_admin` に流す。
  `NOC_STATUS_*` 定数に `InvalidAdminSubject`(0x0B) は無い。
- `core/access_control.rs::AccessControlHandler::write` は decode が通れば
  privilege / auth_mode / subjects / targets を無検査で格納する。`Subject::matches`
  が CAT version 0 を「誰にもマッチしない」で吸収しているので fails-closed
  ではあるが、死にエントリが黙って入る。
- `net/runtime.rs::serve_subscribe_request` は priming の展開結果が空でも
  `SubscribeResponse` を返して購読を作る（spec §8.10 / chip は INVALID_ACTION）。
- `net/runtime.rs::send_subscription_report` は dirty パスを**具体パス**として
  `Node::read_entries` に渡すため、wildcard 購読でも不許可属性が
  `UNSUPPORTED_ACCESS`(0x7E) の status entry になる（priming は wildcard
  展開で黙って落とすので非対称。値漏れは無い）。

## 1. subject の形の判定（共通ヘルパ）

`core/access_control.rs` に追加:

```rust
pub(crate) enum SubjectKind { Node, Cat, Group }

/// ACL / CaseAdminSubject の subject 値の形を判定する（spec §6.6.2.1.2 /
/// §2.5.5 Node Identifier ranges）。
/// - Node: operational node id 0x0000_0000_0000_0001 ..= 0xFFFF_FFEF_FFFF_FFFF
/// - Cat : 上位 32 bit == CAT_SUBJECT_PREFIX かつ version(下位 16 bit) != 0
/// - Group: 0x0000_0000_0000_0001 ..= 0x0000_0000_0000_FFFF（Node と重なる —
///   auth mode で解釈が決まるので呼び側が auth mode 別に受理集合を選ぶ）
/// それ以外（0、CAT version 0、CAT 以外の予約域 0xFFFF_FFF0_0000_0000..）は None。
pub(crate) fn subject_kind(subject: u64) -> Option<SubjectKind>
```

Group は Node の部分集合なので `subject_kind` は「Node 域に入る値」を
`Node` で返し、呼び側の Group auth mode 検査は `subject <= 0xFFFF && subject != 0`
を別途見る（`is_group_subject(u64)` ヘルパ）。`Subject::matches` は変更しない
（version 0 不一致の防波堤はそのまま）。

## 2. AddNOC の CaseAdminSubject 検査

`handle_add_noc`（`core/commissioning.rs`）:

- 検査位置: `TableFull` 判定の**後**、`pending` の CSR/root 取り出し・証明書
  検証の**前**（chip 本家 `OperationalCredentialsServer` と同順。無効な subject
  のために chain 検証をしない）。
- 受理: `subject_kind(case_admin_subject)` が `Some(Node | Cat)`。
  Group 形（≤ 0xFFFF）は Node 域なので受理される — spec の CaseAdminSubject
  は「node id または CAT」で、0xFFFF 以下の operational node id は合法。
- 拒否: `NOCResponse(StatusCode = InvalidAdminSubject = 0x0B)`、
  `tracing::debug!(reason = "invalid admin subject", subject, ...)`。
  `pending` と fail-safe は触らない（同じ PASE セッションで正しい subject の
  AddNOC をやり直せる — chip と同じ）。
- 定数 `NOC_STATUS_INVALID_ADMIN_SUBJECT: u8 = 0x0B` を既存の `NOC_STATUS_*`
  群に追加（doc: spec §11.17.5.9 NodeOperationalCertStatusEnum）。

## 3. ACL write のエントリ妥当性検査

`AccessControlHandler::write`（`core/access_control.rs`）: decode 直後・容量
ガードの前に `validate_entry(&AclDeviceEntry) -> Result<(), u8>` を全エントリに
かける。全置換パスは 1 件でも違反なら store 不変で `CONSTRAINT_ERROR`。
append パスも同じ。順序は decode → validate → 容量（chip も validation を
先にする。容量エラーは「正しいエントリが入り切らない」ときだけ）。

`validate_entry` の規則（chip `AccessControl::Entry` の validation +
spec §11.1.7.1 `AccessControlEntryStruct` 制約）:

| 項目 | 規則 | 違反 |
|---|---|---|
| privilege | 1..=5（View/ProxyView/Operate/Manage/Administer） | CONSTRAINT_ERROR |
| auth_mode | 2 (CASE) または 3 (Group)。1 (PASE) は ACL に書けない | CONSTRAINT_ERROR |
| Administer × Group | Administer は CASE のみ（spec: "Administer privilege SHALL only be granted to CASE"） | CONSTRAINT_ERROR |
| subjects 数 | ≤ `ACL_SUBJECTS_PER_ENTRY`(4)。空/null は wildcard で可 | CONSTRAINT_ERROR |
| CASE subject | 各値が `subject_kind` ∈ {Node, Cat}（CAT version 0 → 違反） | CONSTRAINT_ERROR |
| Group subject | 各値が 1..=0xFFFF | CONSTRAINT_ERROR |
| targets 数 | ≤ `ACL_TARGETS_PER_ENTRY`(3)。null は制限なしで可 | CONSTRAINT_ERROR |
| target 各要素 | `decode_targets` で decode できる。cluster / endpoint / device_type の少なくとも 1 つが non-null。endpoint と device_type の同時指定は不可。cluster は chip `IsValidClusterId` と同じ「下位 16 bit が 0x0000..=0x7FFF（標準）または 0xFC00..=0xFFFE（MS）」、endpoint ≤ 0xFFFE、device_type は chip `IsValidDeviceTypeId` と同じ「下位 16 bit ≤ 0xBFFF」 | CONSTRAINT_ERROR |

`AUTH_MODE_GROUP: u8 = 3` 定数を追加（現状 `AUTH_MODE_CASE` のみ）。
`AclStore::check` は変更しない: Group auth mode のエントリは今まで通り CASE
セッションには一致しない（groupcast 受信＝フェーズ 2 で `Subject::group` を足す）。
`AUTH_MODE_CASE` の doc（「Group は subject 解決を実装していない」）は「Group
エントリは書き込みは受理・検査するが、照合はフェーズ 2 まで CASE のみ」に更新。

`mat group grant` が書くエントリ（`mat-core::acl`: privilege Operate、auth mode
Group、subjects=[group_id]、targets=[{cluster, endpoint}]）はこの規則で受理される
こと（テストで pin）。

## 4. subscribe: 読める属性ゼロの購読は INVALID_ACTION

`Node`（`core/datamodel.rs`）に追加:

```rust
/// spec §8.10 / chip `HasValidAttributePathForSubscription`: paths のどれか
/// 1 つでも、ACL 込みで read できる属性に展開されるか。値は読まない。
pub fn has_readable_path(&self, paths: &[AttrPathIn], read_ctx: &ReadCtx) -> bool
```

規則は chip `ParseAttributePaths` と同じ: **具体パス（endpoint / cluster /
attribute の 3 フィールド全部 Some）は存在・権限に関わらず有効**（拒否や不在は
priming の status entry で伝える）。wildcard を含むパスは、ACL 込みで読める
属性に 1 つ以上展開されるときだけ有効（wildcard-endpoint/cluster + 具体
attribute の組み合わせでは、その attribute が実在することも確認する — 1 件
値読みして existence check とする）。paths が空のリクエストは無効。

実装は `expand_endpoint/expand_cluster/expand_attribute` と同じ展開を「1 件見つけ
次第 true」で回す。既存の展開 3 関数に「値を読まずに可否だけ見る」モードを足す
より、`read_entries` を呼んで `ReportEntryOut::Data` の有無を見る方が単純だが、
NOCs 等の重い属性を無駄に読むので専用の展開にする（展開ロジック重複は
`expand_*` の共通イテレータ化で避ける — 実装で判断、ただし `read_entries` の
既存テストが全部通ること）。

`serve_subscribe_request`（`net/runtime.rs`）: decode 後・`read_chunks` の前に
`node.has_readable_path(&req.paths, &read_ctx)` を見て false なら
`session.reply_reliable(msg, PROTOCOL_ID_INTERACTION_MODEL,
im::OPCODE_STATUS_RESPONSE, &im::encode_status_response(im::STATUS_INVALID_ACTION), ..)`
を返し `None`（購読を作らない）。debug ログに `paths` と `subject`。
`paths` が空のリクエストも同じ扱い（読める属性ゼロ）。

既存購読との関係: 現状 `serve_secured_message` は `**subscription =
serve_subscribe_request(..)` で、失敗（priming 送信失敗等）も含めて必ず
置き換える（doc: "this same peer just asked to start over"）。INVALID_ACTION
拒否の扱いは `KeepSubscriptions` 次第（chip: 拒否された SubscribeRequest の
既存購読への影響は `KeepSubscriptions` が決める — `true` なら既存購読を残す、
`false` なら chip はパス検証より前に既存購読を破棄するので、拒否理由が
何であれ既存購読は破棄済み）。`serve_subscribe_request` の戻りを
`enum SubscribeOutcome { Installed(ActiveSubscription), TornDown, Rejected }`
にし、`Installed`/`TornDown`（= 従来の Some/None）は代入、`Rejected`
（= `KeepSubscriptions=true` での拒否）は `**subscription` に触らない。
`KeepSubscriptions=false` での拒否は `TornDown` を返し、既存購読を破棄する。

## 5. subscribe: dirty report の 0x7E 非対称の解消

`ActiveSubscription`（`net/subscription.rs`）に「この dirty パスを cover する
購読パスは具体パスか」を答えるヘルパを追加:

```rust
/// `path` を cover する購読パスのうち、endpoint/cluster/attribute が全部
/// 具体（Some）なものがあるか。無い = wildcard 由来。
pub fn covered_concretely(&self, path: (u16, u32, u32)) -> bool
```

`send_subscription_report`: dirty パスごとに `read_entries` した結果のうち、
`covered_concretely` が false のパスの `ReportEntryOut::Status` を落とす
（`Data` はそのまま）。具体パス購読の 0x7E は維持（priming と同じ非対称ルール:
「具体的に頼まれた属性の拒否は答える、wildcard 展開で当たっただけの拒否は
黙る」）。実装は dirty パスを (concrete, wildcard) に分けて 2 回 `read_entries`
するのでも、1 回読んでフィルタするのでもよい（後者が単純）。

`note_changed` の cover 判定は変更しない（不許可属性が dirty に入るのは今まで
通り。落とすのは報告時）。

## 6. テスト

ユニット（各モジュール内）:
- `subject_kind` の境界（0 / 1 / 0xFFFF / 0xFFFF_FFEF_FFFF_FFFF / 0xFFFF_FFF0_0000_0000
  / CAT v0 / CAT v1 / CAT prefix 以外の予約域）。
- `validate_entry` の各行（表の全違反 + `mat group grant` 形の受理 + 4 subject / 3 target 境界）。
- `handle_add_noc`（既存の `core/commissioning.rs` テスト流儀）: CAT v0 と 0 と
  予約域で 0x0B、fabric 未追加・ACL 未追加、続く正しい AddNOC が同一 pending で成功。
- `has_readable_path`: 全許可 Node / 拒否 subject で wildcard・具体パス・空 paths。
- `covered_concretely`: 具体購読 / wildcard 購読 / 両方 cover。

統合（`crates/mat-device/tests/`、既存 support の閉ループ流儀）:
- `acl_write_validation.rs`: commission 後、CAT v0 subject のエントリ全置換 →
  `CONSTRAINT_ERROR`、既存 admin エントリで引き続き ACL read 可（store 不変の証明）。
  `mat group grant` 形（Group auth mode）の append → 成功。
- `add_noc_invalid_admin_subject.rs`: support の AddNOC 直前までを切り出し
  （`commission_directly_as` のリファクタ、既存呼び出しは不変）、CAT v0 で
  `NOCResponse` status 0x0B、同セッションで node id subject の AddNOC 成功 →
  CASE まで完走。
- `subscribe_denied.rs`: admin を Operate に降格して AccessControl クラスタ限定の
  wildcard 購読（`subscribe_wildcard(.., &[CLUSTER_ACCESS_CONTROL], ..)`）→
  `Err`（peer の StatusResponse INVALID_ACTION）。続けて OnOff 購読は成功する
  （拒否が既存の状態を壊していない）。
- dirty report 非対称: 既存 `subscribe_loop.rs` の流儀で、View 権限の subject が
  OnOff クラスタ wildcard 購読 → 別経路（admin セッション不可＝matv は 1 CASE
  なので、`Node` 直叩きのユニットテストに寄せる）。実装は
  `send_subscription_report` を「フィルタ関数 + 送信」に分け、フィルタ関数を
  ユニットテストで pin する（wildcard 購読で不許可属性が dirty → Status 0 件、
  具体購読 → Status 1 件）。

検証: `task check`（fmt:check + clippy + test）、`task e2e:device:m1`、
`task e2e:device:m3`（直列、matv は同時 1 CASE）。実機 hogar-matd は使わない。

## 7. やらないこと

- groupcast 受信・`Subject::group`・GroupKeyStore / Groups membership の永続化
  （フェーズ 2）。
- KeySetRead / KeySetRemove、EventRequests。
- mat-device 以外のクレート変更。`mat-core::acl` の grant エントリ形は読むだけ。
- `Subject::matches` の変更。
