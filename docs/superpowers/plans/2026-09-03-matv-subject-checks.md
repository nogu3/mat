# matv subject 検査 + subscribe 残 2 件 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** matv（`mat-device`）が AddNOC の `CaseAdminSubject` と ACL write のエントリを spec どおり検査して不正値を拒否し、Subscribe が「読める属性ゼロ」を INVALID_ACTION で拒否し、dirty report が wildcard 購読で 0x7E status を漏らさないようにする。

**Architecture:** 判定ヘルパ（`subject_kind` / `validate_entry`）は `core/access_control.rs` に集約し、`core/commissioning.rs`（AddNOC）と `AccessControlHandler::write` から呼ぶ。Subscribe 側は `Node::has_readable_path`（`core/datamodel.rs`、値を読まない ACL 込み展開）を `net/runtime.rs::serve_subscribe_request` の入口で見て `StatusResponse(INVALID_ACTION)` を返し、dirty report は `net/subscription.rs` の純関数 `retain_reportable` で wildcard 由来の status entry を落とす。既存の `Subject::matches` / `AclStore::check` は変えない。

**Tech Stack:** Rust 2021 workspace、`mat-controller`（TLV/IM/CASE の公開 API のみ使用）、tokio 統合テスト（`tests/support/mod.rs` の閉ループ）、Taskfile（`task check` / `task e2e:device:m1` / `task e2e:device:m3`）。

**Spec:** `docs/superpowers/specs/2026-09-03-matv-subject-checks-design.md`

## Global Constraints

- 変更は `crates/mat-device/**`（+ この plan/spec）のみ。`mat-core` / `mat-controller` / `mat-native` / `crates/mat` / `matd` / `matv` には触らない（他レーン並行中）。
- 各タスクの最後に `cargo test -p mat-device --features net` が緑、`cargo clippy -p mat-device --features net --all-targets -- -D warnings` と `cargo fmt --all -- --check` が通ること。
- コミットは日本語 subject + 末尾トレーラ:
  ```
  Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01SNgdnaxwYTVSmcYYGRKQSK
  ```
- ステータス定数は `mat_controller::im::STATUS_*` を使う（`STATUS_CONSTRAINT_ERROR = 0x87`、`STATUS_INVALID_ACTION = 0x80`、`STATUS_UNSUPPORTED_ACCESS = 0x7E`）。
- doc コメントは既存ファイルの言語に合わせる（access_control.rs / commissioning.rs は日英混在、既存の隣接コメントに揃える）。
- 統合テストは `#![cfg(feature = "net")]` + `mod support;` の既存流儀（`tests/acl_enforce.rs` 参照）。matv は同時 1 CASE セッションのみ: 1 テスト = 1 セッション。

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/mat-device/src/core/access_control.rs` | `SubjectKind` / `subject_kind` / `is_group_subject` / `AUTH_MODE_GROUP` / `validate_entry`（Task 1, 4）。`write` から `validate_entry` を呼ぶ（Task 4） |
| `crates/mat-device/src/core/commissioning.rs` | `NOC_STATUS_INVALID_ADMIN_SUBJECT` + `handle_add_noc` の検査（Task 2） |
| `crates/mat-device/tests/support/mod.rs` | `commission_directly_as` を `pase_until_add_noc` / `add_noc` / `case_and_complete` に分割（既存呼び出し不変、Task 3） |
| `crates/mat-device/tests/add_noc_invalid_admin_subject.rs` | 統合: CAT v0 → 0x0B → 同セッションで再 AddNOC 成功（Task 3） |
| `crates/mat-device/tests/acl_write_validation.rs` | 統合: 不正エントリ全置換 → CONSTRAINT_ERROR + store 不変、`mat group grant` 形 append → 成功（Task 5） |
| `crates/mat-device/src/core/datamodel.rs` | `Node::has_readable_path`（Task 6） |
| `crates/mat-device/src/net/runtime.rs` | `SubscribeOutcome` + INVALID_ACTION 応答（Task 7）、`send_subscription_report` のフィルタ配線（Task 8） |
| `crates/mat-device/tests/subscribe_denied.rs` | 統合: 読めない wildcard 購読 → INVALID_ACTION、既存購読は生き残る（Task 7） |
| `crates/mat-device/src/net/subscription.rs` | `covered_concretely` + `retain_reportable`（Task 8） |

---

### Task 1: subject の形の判定ヘルパ（`subject_kind` / `is_group_subject` / `AUTH_MODE_GROUP`）

**Files:**
- Modify: `crates/mat-device/src/core/access_control.rs`（`AUTH_MODE_CASE` 定数の直後、`cat_subject` の直後、tests モジュール末尾）

**Interfaces:**
- Produces:
  - `pub(crate) const AUTH_MODE_GROUP: u8 = 3;`
  - `pub(crate) enum SubjectKind { Node, Cat }`
  - `pub(crate) fn subject_kind(subject: u64) -> Option<SubjectKind>`
  - `pub(crate) fn is_group_subject(subject: u64) -> bool`
  - `pub(crate) const OPERATIONAL_NODE_ID_MAX: u64 = 0xFFFF_FFEF_FFFF_FFFF;`

（spec §1 の `Group` variant は落とす: Group 域は Node 域の部分集合で、`is_group_subject` が auth mode Group の受理集合を別に答える方が呼び側が素直。）

- [ ] **Step 1: 失敗するユニットテストを書く**

`access_control.rs` の `mod tests` 末尾に追加:

```rust
    /// spec §6.6.2.1.2 / §2.5.5: CaseAdminSubject と ACL の CASE subject が
    /// 取れる形は operational node id（1..=0xFFFF_FFEF_FFFF_FFFF）か
    /// CAT（prefix 0xFFFF_FFFD、version ≠ 0）の 2 つだけ。
    #[test]
    fn subject_kind_classifies_node_cat_and_rejects_reserved_values() {
        // node id の両端
        assert!(matches!(subject_kind(1), Some(SubjectKind::Node)));
        assert!(matches!(subject_kind(0xFFFF), Some(SubjectKind::Node)));
        assert!(matches!(
            subject_kind(OPERATIONAL_NODE_ID_MAX),
            Some(SubjectKind::Node)
        ));
        // CAT
        assert!(matches!(
            subject_kind(cat_subject(0xABCD_0001)),
            Some(SubjectKind::Cat)
        ));
        assert!(matches!(
            subject_kind(cat_subject(0x0001_FFFF)),
            Some(SubjectKind::Cat)
        ));
        // 無効: 0、CAT version 0、CAT 以外の予約域、予約域の先頭と末尾
        assert!(subject_kind(0).is_none());
        assert!(subject_kind(cat_subject(0xABCD_0000)).is_none());
        assert!(subject_kind(OPERATIONAL_NODE_ID_MAX + 1).is_none()); // 0xFFFF_FFF0_0000_0000
        assert!(subject_kind(0xFFFF_FFFE_0000_0001).is_none()); // temporary local
        assert!(subject_kind(0xFFFF_FFFF_FFFF_0001).is_none()); // group range
        assert!(subject_kind(u64::MAX).is_none());
    }

    /// Group auth mode の subject は GroupId（1..=0xFFFF）そのもの。
    #[test]
    fn is_group_subject_accepts_only_nonzero_u16() {
        assert!(is_group_subject(1));
        assert!(is_group_subject(0xFFFF));
        assert!(!is_group_subject(0));
        assert!(!is_group_subject(0x1_0000));
        assert!(!is_group_subject(cat_subject(0xABCD_0001)));
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-device --features net --lib access_control::tests::subject_kind_classifies_node_cat_and_rejects_reserved_values`
Expected: コンパイルエラー（`subject_kind` / `SubjectKind` / `is_group_subject` / `OPERATIONAL_NODE_ID_MAX` 未定義）

- [ ] **Step 3: 実装**

`AUTH_MODE_CASE` の定数ブロックを次に置き換える（既存 doc の「Group は subject 解決を実装していない」の一文を更新）:

```rust
/// `AccessControlEntryAuthModeEnum` (spec §11.1.7.1) のうちこの実装が
/// 照合に使う唯一の値 — CASE。PASE は fabric を持たないので ACL の対象外
/// （`write` は PASE エントリを `CONSTRAINT_ERROR` で拒否する）。Group
/// エントリは `write` が受理・検査して保持するが、`check` は CASE
/// セッションに対して Group エントリを一致させない（groupcast 受信の
/// 配線 = `Subject` に group 形を足すのは別フェーズ）。
pub(crate) const AUTH_MODE_CASE: u8 = 2;
/// `AccessControlEntryAuthModeEnum::Group` — `mat group grant` が書く
/// エントリの auth mode。`write` の妥当性検査（`validate_entry`）が
/// subject を GroupId として検査する根拠。
pub(crate) const AUTH_MODE_GROUP: u8 = 3;
```

`cat_subject` の直後に追加:

```rust
/// Operational Node ID の上限 (spec §2.5.5.1: 0x0000_0000_0000_0001 ..=
/// 0xFFFF_FFEF_FFFF_FFFF)。これより上は予約域（temporary local /
/// CAT / group 等）。
pub(crate) const OPERATIONAL_NODE_ID_MAX: u64 = 0xFFFF_FFEF_FFFF_FFFF;

/// `subject_kind` の答え — CASE 文脈で subject が取れる 2 つの形。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SubjectKind {
    /// Operational node id（1..=`OPERATIONAL_NODE_ID_MAX`）。
    Node,
    /// CASE Authenticated Tag subject（`CAT_SUBJECT_PREFIX` + version ≠ 0）。
    Cat,
}

/// CASE 文脈の subject 値（`AddNOC.CaseAdminSubject`、CASE auth mode の
/// ACL `Subjects` 要素）の形を判定する (spec §6.6.2.1.2 / §2.5.5)。
/// 0、CAT version 0、CAT prefix 以外の予約域（0xFFFF_FFF0_0000_0000..）は
/// `None` — 呼び側はこれを `InvalidAdminSubject` / `CONSTRAINT_ERROR` に
/// 写す。version 0 の CAT は `Subject::matches` でも誰にもマッチしない
/// が、そもそも書かせないのがここの役目。
pub(crate) fn subject_kind(subject: u64) -> Option<SubjectKind> {
    if (1..=OPERATIONAL_NODE_ID_MAX).contains(&subject) {
        return Some(SubjectKind::Node);
    }
    if subject >> 32 == CAT_SUBJECT_PREFIX && cat_version(subject as u32) != 0 {
        return Some(SubjectKind::Cat);
    }
    None
}

/// Group auth mode の ACL `Subjects` 要素として妥当か: GroupId そのもの
/// （1..=0xFFFF、spec §11.1.7.1 "the subject is a GroupId"）。
pub(crate) fn is_group_subject(subject: u64) -> bool {
    (1..=0xFFFF).contains(&subject)
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-device --features net --lib access_control::tests`
Expected: 全 PASS（新規 2 件含む）。`AUTH_MODE_GROUP` / `is_group_subject` が未使用警告になる場合は Task 4 まで `#[allow(dead_code)]` を付けず、`cargo clippy` が `-D warnings` で落ちるなら一時的に `#[cfg_attr(not(test), allow(dead_code))]` を付けて Task 4 で外す。

- [ ] **Step 5: Commit**

```bash
git add crates/mat-device/src/core/access_control.rs
git commit -m "feat(mat-device): subject の形の判定ヘルパ subject_kind / is_group_subject（レーン A Task 1）"
```

---

### Task 2: AddNOC の CaseAdminSubject 検査（InvalidAdminSubject 0x0B）

**Files:**
- Modify: `crates/mat-device/src/core/commissioning.rs`（`NOC_STATUS_*` 定数群 72-87 行付近、`handle_add_noc` 1049-1170 行付近、tests）

**Interfaces:**
- Consumes: `crate::core::access_control::{subject_kind, cat_subject, OPERATIONAL_NODE_ID_MAX, Subject, PRIVILEGE_ADMINISTER}`
- Produces: `const NOC_STATUS_INVALID_ADMIN_SUBJECT: u8 = 0x0B;`（private、テストから参照）

- [ ] **Step 1: 失敗するユニットテストを書く**

`commissioning.rs` の tests モジュール、`add_noc_rejects_sixth_fabric_with_table_full` の直後に追加。`install_fabric` は admin subject 固定なので、subject を選べる派生ヘルパを先に足す:

```rust
    /// `install_fabric` の admin subject 可変版: ArmFailSafe → CSR →
    /// AddTrustedRoot まで進めてから `case_admin_subject` で AddNOC を
    /// 打ち、その reply と「同じ pending で再 AddNOC するための NOC/
    /// fabric」を返す。
    fn install_fabric_with_admin(
        server: &mut CommissioningServer,
        fabric_id: u64,
        node_id: u64,
        case_admin_subject: u64,
    ) -> (InvokeReply, Vec<u8>, CommissioningFabric) {
        let fabric = CommissioningFabric::generate(fabric_id, 0xAA).unwrap();
        drive_invoke(
            server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(120, 1),
        );
        let (_, csr_resp) = expect_data(drive_invoke(
            server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CSR_REQUEST,
            &encode_csr_request(&[3u8; 32]),
        ));
        let (elements, _sig) = decode_csr_response(&csr_resp).unwrap();
        let (csr_der, _nonce) = parse_nocsr_elements(&elements).unwrap();
        let device_pub = parse_csr(&csr_der).unwrap();
        let noc = fabric.issue_device_noc(&device_pub, node_id).unwrap();
        assert_eq!(
            drive_invoke(
                server,
                CLUSTER_OPERATIONAL_CREDENTIALS,
                CMD_ADD_TRUSTED_ROOT,
                &encode_add_trusted_root(&fabric.rcac_tlv),
            ),
            InvokeReply::Status(im::STATUS_SUCCESS)
        );
        let reply = drive_invoke(
            server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ADD_NOC,
            &encode_add_noc(&noc, &fabric.ipk_epoch, case_admin_subject, 0xFFF1),
        );
        (reply, noc, fabric)
    }

    /// spec §11.17.6.8.1: `CaseAdminSubject` は operational node id か
    /// CAT（version ≠ 0）でなければ `NOCResponse(InvalidAdminSubject=0x0B)`。
    /// fabric も ACL エントリも作られず、pending（CSR/root）は残るので
    /// 同じセッションで正しい subject の AddNOC をやり直せる。
    #[test]
    fn add_noc_rejects_invalid_case_admin_subject_and_allows_retry() {
        use crate::core::access_control::{
            cat_subject, AclStore, Subject, OPERATIONAL_NODE_ID_MAX, PRIVILEGE_ADMINISTER,
        };
        for bad in [
            0u64,
            cat_subject(0xABCD_0000),      // CAT version 0
            OPERATIONAL_NODE_ID_MAX + 1,   // 予約域の先頭
            0xFFFF_FFFF_FFFF_0001,         // group 域
        ] {
            let mut server = test_server();
            let acl_store = AclStore::new();
            server.set_acl_store(acl_store.clone());

            let (reply, noc, fabric) = install_fabric_with_admin(&mut server, 0x1122, 0x5001, bad);
            let (response_command, resp) = expect_data(reply);
            assert_eq!(response_command, RESP_NOC);
            let (status, fabric_index) = decode_noc_response(&resp).unwrap();
            assert_eq!(status, NOC_STATUS_INVALID_ADMIN_SUBJECT, "subject {bad:#x}");
            assert_eq!(fabric_index, None);
            assert!(server.fabrics().is_empty(), "rejected AddNOC must not install a fabric");
            assert!(
                !acl_store.check(1, Subject::node(0x5001), PRIVILEGE_ADMINISTER, 0, 0x001F),
                "rejected AddNOC must not add an admin ACL entry"
            );

            // 同じ pending（CSR keypair / trusted root）で正しい subject なら通る。
            let (_, resp) = expect_data(drive_invoke(
                &mut server,
                CLUSTER_OPERATIONAL_CREDENTIALS,
                CMD_ADD_NOC,
                &encode_add_noc(&noc, &fabric.ipk_epoch, 0xAA, 0xFFF1),
            ));
            let (status, fabric_index) = decode_noc_response(&resp).unwrap();
            assert_eq!(status, NOC_STATUS_OK, "retry after {bad:#x}");
            assert_eq!(fabric_index, Some(1));
            assert_eq!(server.fabrics().len(), 1);
        }
    }

    /// CAT 形の admin subject（Apple Home が送る形）は version ≠ 0 なら受理。
    #[test]
    fn add_noc_accepts_cat_case_admin_subject() {
        use crate::core::access_control::cat_subject;
        let mut server = test_server();
        let (reply, _, _) =
            install_fabric_with_admin(&mut server, 0x1122, 0x5001, cat_subject(0xABCD_0002));
        let (_, resp) = expect_data(reply);
        let (status, fabric_index) = decode_noc_response(&resp).unwrap();
        assert_eq!(status, NOC_STATUS_OK);
        assert_eq!(fabric_index, Some(1));
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-device --features net --lib commissioning::tests::add_noc_rejects_invalid_case_admin_subject_and_allows_retry`
Expected: コンパイルエラー（`NOC_STATUS_INVALID_ADMIN_SUBJECT` 未定義）

- [ ] **Step 3: 実装**

定数群（`NOC_STATUS_INVALID_FABRIC_INDEX` の直後）に追加:

```rust
/// `NodeOperationalCertStatusEnum::InvalidAdminSubject` (spec §11.17.5.9):
/// `AddNOC.CaseAdminSubject` が operational node id でも CAT でもない
/// （`access_control::subject_kind` が `None`）。
const NOC_STATUS_INVALID_ADMIN_SUBJECT: u8 = 0x0B;
```

`handle_add_noc` の TableFull 判定ブロックの**直後**、`let (Some(root_tlv), ...) = ... else { MissingCsr }` の**直前**に挿入:

```rust
        // spec §11.17.6.8.1: the admin subject must be a real operational
        // node id or a CAT with a non-zero version — anything else would
        // install an Administer ACL entry nobody can ever match (a fabric
        // whose only admin is locked out). Checked before the certificate
        // work, like `TableFull`: no point verifying a chain for a fabric
        // that can't be administered. `pending` is left intact so the same
        // session can retry with a valid subject.
        if crate::core::access_control::subject_kind(case_admin_subject).is_none() {
            tracing::debug!(
                reason = "invalid admin subject",
                case_admin_subject = format_args!("{case_admin_subject:#x}"),
                "AddNOC rejected: InvalidAdminSubject"
            );
            return noc_status(NOC_STATUS_INVALID_ADMIN_SUBJECT);
        }
```

（`noc_status` クロージャは TableFull 判定より前で定義済みなのでそのまま使える。）

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-device --features net --lib commissioning::tests`
Expected: 全 PASS（新規 2 件含む。既存の `install_fabric` は subject 0xAA = node id なので不変）

- [ ] **Step 5: Commit**

```bash
git add crates/mat-device/src/core/commissioning.rs
git commit -m "feat(mat-device): AddNOC の CaseAdminSubject 検査 — 不正なら NOCResponse InvalidAdminSubject(0x0B)（レーン A Task 2）"
```

---

### Task 3: AddNOC 拒否の閉ループ統合テスト（support 分割）

**Files:**
- Modify: `crates/mat-device/tests/support/mod.rs`（`commission_directly_as` 129-316 行を 3 関数に分割）
- Create: `crates/mat-device/tests/add_noc_invalid_admin_subject.rs`

**Interfaces:**
- Produces（`tests/support/mod.rs`、全部 `#[allow(dead_code)] pub`）:
  - `pub async fn pase_until_add_noc(addr: SocketAddr, paa_der: &[u8], fabric: &CommissioningFabric) -> (SecureSession, Vec<u8>)` — PASE → ArmFailSafe → attestation 検証 → CSR → NOC 発行 → AddTrustedRoot まで。戻りは PASE セッションとデバイス NOC TLV。
  - `pub async fn add_noc(pase: &mut SecureSession, noc_tlv: &[u8], fabric: &CommissioningFabric, case_admin_subject: u64) -> (u8, Option<u8>)` — AddNOC 1 発、`(NOCResponse status, fabric_index)`。
  - `pub async fn case_and_complete(addr: SocketAddr, creds: &FabricCredentials) -> SecureSession` — CASE（10 回リトライ）+ CommissioningComplete。
  - `commission_directly_as` はこの 3 つの合成（署名・挙動不変、`assert_eq!(status, 0)` / `Some(1)` もそのまま）。

- [ ] **Step 1: support を分割する**

`commission_directly_as` の本体を 3 関数に切り出す。挙動を 1 行も変えないこと（既存テスト `acl_cat_subject.rs` 等が回帰テスト）:

```rust
/// [`commission_directly_as`] の前半: PASE → ArmFailSafe → attestation
/// （`verify_device_attestation` で strict 検証）→ CSR → NOC 発行 →
/// AddTrustedRootCertificate。AddNOC の直前で止め、PASE セッションと
/// 発行済みデバイス NOC を返す — AddNOC の応答そのものを検査したい
/// テスト（`add_noc_invalid_admin_subject.rs`）のための切り出し。
#[allow(dead_code)]
pub async fn pase_until_add_noc(
    addr: SocketAddr,
    paa_der: &[u8],
    fabric: &CommissioningFabric,
) -> (SecureSession, Vec<u8>) {
    let cfg = fast_cfg();
    let transport = Arc::new(Transport::Udp(Arc::new(
        UdpTransport::bind().await.unwrap(),
    )));

    // 2. PASE.
    let mut pase = pase::establish(Arc::clone(&transport), addr, PASSCODE, &cfg)
        .await
        .expect("pase establish");
    let challenge = pase.attestation_challenge();

    // ... （既存の 3. ArmFailSafe / 5. Attestation / DAC / PAI /
    //      verify_device_attestation / 6. CSR→NOC / 7. AddTrustedRoot を
    //      そのまま移動。`assert_eq!(resp.status, 0)` も含めて一字一句）
    (pase, noc_tlv)
}

/// [`commission_directly_as`] の AddNOC 1 発分: `(NOCResponse status,
/// fabric_index)` を返す。成功判定は呼び側（拒否を期待するテストもある）。
#[allow(dead_code)]
pub async fn add_noc(
    pase: &mut SecureSession,
    noc_tlv: &[u8],
    fabric: &CommissioningFabric,
    case_admin_subject: u64,
) -> (u8, Option<u8>) {
    let cfg = fast_cfg();
    let resp = pase
        .invoke_for_data(
            0,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ADD_NOC,
            Some(&encode_add_noc(
                noc_tlv,
                &fabric.ipk_epoch,
                case_admin_subject,
                ADMIN_VENDOR_ID,
            )),
            None,
            &cfg,
        )
        .await
        .expect("add noc");
    decode_noc_response(resp.fields_tlv.as_deref().unwrap()).expect("decode noc response")
}

/// [`commission_directly_as`] の後半: 新 fabric への CASE（リトライ付き）
/// + CommissioningComplete。
#[allow(dead_code)]
pub async fn case_and_complete(addr: SocketAddr, creds: &FabricCredentials) -> SecureSession {
    let cfg = fast_cfg();
    // 8. CASE ... （既存コードをそのまま移動）
    // 9. CommissioningComplete ... （同上）
    session
}

#[allow(dead_code)]
pub async fn commission_directly_as(
    addr: SocketAddr,
    paa_der: &[u8],
    fabric: &CommissioningFabric,
    case_admin_subject: u64,
    creds: &FabricCredentials,
) -> SecureSession {
    let (mut pase, noc_tlv) = pase_until_add_noc(addr, paa_der, fabric).await;
    let (noc_status, fabric_index) = add_noc(&mut pase, &noc_tlv, fabric, case_admin_subject).await;
    assert_eq!(noc_status, 0, "AddNOC should succeed");
    assert_eq!(fabric_index, Some(1));
    case_and_complete(addr, creds).await
}
```

- [ ] **Step 2: 既存の統合テストが全部通ることを確認（分割のリグレッション）**

Run: `cargo test -p mat-device --features net --test acl_cat_subject --test acl_enforce --test onoff_invoke --test group_provision --test subscribe_loop`
Expected: 全 PASS

- [ ] **Step 3: 失敗する統合テストを書く**

`crates/mat-device/tests/add_noc_invalid_admin_subject.rs`:

```rust
//! Closed-loop proof of AddNOC's `CaseAdminSubject` check (spec
//! §11.17.6.8.1): a CAT subject with version 0 — the shape that used to
//! install an Administer entry nobody could ever match — is answered with
//! `NOCResponse(InvalidAdminSubject = 0x0B)`, nothing is installed, and the
//! *same* PASE session can retry with a valid subject and commission all
//! the way through CASE + CommissioningComplete.
//!
//! Uses `tests/support/mod.rs`'s split commissioning helpers
//! (`pase_until_add_noc` / `add_noc` / `case_and_complete`) so the AddNOC
//! reply itself is observable.
#![cfg(feature = "net")]

use std::net::SocketAddr;

use mat_controller::commissioning::CommissioningFabric;
use mat_controller::im;

use mat_device::core::access_control::cat_subject;
use mat_device::device::Device;

mod support;
use support::{add_noc, case_and_complete, device_config, pase_until_add_noc};

const ADMIN_NODE_ID: u64 = 660_033;
/// `NodeOperationalCertStatusEnum::InvalidAdminSubject` (spec §11.17.5.9).
const NOC_STATUS_INVALID_ADMIN_SUBJECT: u8 = 0x0B;

#[tokio::test]
async fn add_noc_with_cat_version_zero_is_rejected_then_retry_succeeds() {
    let store_dir = tempfile::tempdir().expect("tempdir");
    let device = Device::new(device_config(store_dir.path().to_path_buf())).expect("device new");
    let addr = SocketAddr::new(
        std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        device.local_addr().port(),
    );
    let paa_der = std::fs::read(store_dir.path().join("paa").join("paa.der"))
        .expect("device should have written its PAA DER at Device::new");
    let device_task = tokio::spawn(async move {
        let _ = device.run().await;
    });

    let fabric =
        CommissioningFabric::generate(0x2233_4455, ADMIN_NODE_ID).expect("fabric generate");
    let (mut pase, noc_tlv) = pase_until_add_noc(addr, &paa_der, &fabric).await;

    // CAT(identifier 0xABCD, version 0): syntactically a CAT subject, but
    // version 0 is reserved — no NOC can ever carry it.
    let (status, fabric_index) = add_noc(&mut pase, &noc_tlv, &fabric, cat_subject(0xABCD_0000)).await;
    assert_eq!(status, NOC_STATUS_INVALID_ADMIN_SUBJECT);
    assert_eq!(fabric_index, None);

    // Same PASE session, same CSR/root: a node-id subject goes through.
    let (status, fabric_index) = add_noc(&mut pase, &noc_tlv, &fabric, ADMIN_NODE_ID).await;
    assert_eq!(status, 0, "retry with a valid admin subject must succeed");
    assert_eq!(fabric_index, Some(1));

    // And the fabric is fully usable: CASE + CommissioningComplete + an
    // Administer-gated read (ACL) under the automatic admin entry.
    let creds = fabric.admin_credentials().expect("admin credentials");
    let mut session = case_and_complete(addr, &creds).await;
    let cfg = support::fast_cfg();
    let acl = session
        .read_attribute_json(0, im::CLUSTER_ACCESS_CONTROL, im::ATTR_ACL, &cfg)
        .await
        .expect("ACL read under the automatic admin entry");
    assert_eq!(
        acl.as_array().map(Vec::len),
        Some(1),
        "exactly one (automatic admin) ACL entry after the retried AddNOC: {acl}"
    );

    device_task.abort();
    let _ = device_task.await;
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-device --features net --test add_noc_invalid_admin_subject`
Expected: PASS（Task 2 の実装が入っているので初回から通る。通らなければ Task 2 のロジックか support 分割の問題 — support の `pase_until_add_noc` が AddTrustedRoot まで進んでいるか確認）

- [ ] **Step 5: clippy / fmt**

Run: `cargo fmt --all && cargo clippy -p mat-device --features net --all-targets -- -D warnings`
Expected: 警告 0

- [ ] **Step 6: Commit**

```bash
git add crates/mat-device/tests/support/mod.rs crates/mat-device/tests/add_noc_invalid_admin_subject.rs
git commit -m "test(mat-device): AddNOC InvalidAdminSubject の閉ループ + support の commissioning 分割（レーン A Task 3）"
```

---

### Task 4: ACL write のエントリ妥当性検査（`validate_entry`）

**Files:**
- Modify: `crates/mat-device/src/core/access_control.rs`（`write` 353-395 行、`decode_targets` の近く、tests）

**Interfaces:**
- Consumes: Task 1 の `subject_kind` / `is_group_subject` / `AUTH_MODE_GROUP`、既存 `decode_targets` / `AclTargetDev` / `ACL_SUBJECTS_PER_ENTRY` / `ACL_TARGETS_PER_ENTRY` / `PRIVILEGE_*`
- Produces: `pub(crate) fn validate_entry(entry: &AclDeviceEntry) -> Result<(), u8>`（Err は常に `im::STATUS_CONSTRAINT_ERROR`）、テスト用 `pub(crate) fn encode_targets_for_test(targets: &[(Option<u32>, Option<u16>, Option<u32>)]) -> Vec<u8>`

- [ ] **Step 1: 失敗するユニットテストを書く**

tests モジュールに追加（`encode_targets_for_test` は `#[cfg(test)]` の非 tests 領域、`encode_entry_for_test` の隣に置く）:

```rust
#[cfg(test)]
/// テスト専用: `(cluster, endpoint, device_type)` 列 → `AclDeviceEntry::
/// targets_raw` の形（Anonymous 再タグ済み TargetStruct array）。
pub(crate) fn encode_targets_for_test(
    targets: &[(Option<u32>, Option<u16>, Option<u32>)],
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    for (cluster, endpoint, device_type) in targets {
        w.start_struct(Tag::Anonymous);
        match cluster {
            Some(c) => w.put_uint(Tag::Context(0), u64::from(*c)),
            None => w.put_null(Tag::Context(0)),
        }
        match endpoint {
            Some(e) => w.put_uint(Tag::Context(1), u64::from(*e)),
            None => w.put_null(Tag::Context(1)),
        }
        match device_type {
            Some(d) => w.put_uint(Tag::Context(2), u64::from(*d)),
            None => w.put_null(Tag::Context(2)),
        }
        w.end_container();
    }
    w.end_container();
    w.finish()
}
```

tests 内:

```rust
    fn entry(privilege: u8, auth_mode: u8, subjects: Vec<u64>, targets_raw: Option<Vec<u8>>) -> AclDeviceEntry {
        AclDeviceEntry {
            privilege,
            auth_mode,
            subjects,
            targets_raw,
            fabric_index: 1,
        }
    }

    /// spec §11.1.7.1 / chip `AccessControl::Entry` validation の受理側:
    /// AddNOC 自動 admin 形、`mat group grant` 形（Operate/Group/[gid]/
    /// targets null）、CAT subject、4 subject / 3 target の上限ちょうど、
    /// 各 target 1 フィールド指定。
    #[test]
    fn validate_entry_accepts_well_formed_entries() {
        assert_eq!(validate_entry(&entry(PRIVILEGE_ADMINISTER, AUTH_MODE_CASE, vec![112233], None)), Ok(()));
        assert_eq!(validate_entry(&entry(PRIVILEGE_OPERATE, AUTH_MODE_GROUP, vec![0x0102], None)), Ok(()));
        assert_eq!(validate_entry(&entry(PRIVILEGE_VIEW, AUTH_MODE_CASE, vec![cat_subject(0xABCD_0001)], None)), Ok(()));
        assert_eq!(validate_entry(&entry(PRIVILEGE_VIEW, AUTH_MODE_CASE, vec![], None)), Ok(()), "empty subjects = wildcard");
        assert_eq!(validate_entry(&entry(PRIVILEGE_VIEW, AUTH_MODE_CASE, vec![1, 2, 3, 4], None)), Ok(()));
        let three_targets = encode_targets_for_test(&[
            (Some(0x0006), None, None),
            (None, Some(1), None),
            (None, None, Some(0x0100)),
        ]);
        assert_eq!(validate_entry(&entry(PRIVILEGE_VIEW, AUTH_MODE_CASE, vec![1], Some(three_targets))), Ok(()));
        let ms_cluster = encode_targets_for_test(&[(Some(0xFFF1_FC00), Some(0xFFFE), None)]);
        assert_eq!(validate_entry(&entry(PRIVILEGE_VIEW, AUTH_MODE_CASE, vec![1], Some(ms_cluster))), Ok(()));
    }

    /// 同 validation の拒否側 — 表（spec §3）の各行を 1 ケースずつ。
    #[test]
    fn validate_entry_rejects_each_constraint_violation() {
        const E: Result<(), u8> = Err(im::STATUS_CONSTRAINT_ERROR);
        // privilege
        assert_eq!(validate_entry(&entry(0, AUTH_MODE_CASE, vec![1], None)), E);
        assert_eq!(validate_entry(&entry(6, AUTH_MODE_CASE, vec![1], None)), E);
        // auth mode: PASE(1) と未知値
        assert_eq!(validate_entry(&entry(PRIVILEGE_VIEW, 1, vec![1], None)), E);
        assert_eq!(validate_entry(&entry(PRIVILEGE_VIEW, 0, vec![1], None)), E);
        assert_eq!(validate_entry(&entry(PRIVILEGE_VIEW, 4, vec![1], None)), E);
        // Administer × Group
        assert_eq!(validate_entry(&entry(PRIVILEGE_ADMINISTER, AUTH_MODE_GROUP, vec![1], None)), E);
        // subjects 数
        assert_eq!(validate_entry(&entry(PRIVILEGE_VIEW, AUTH_MODE_CASE, vec![1, 2, 3, 4, 5], None)), E);
        // CASE subject の形
        assert_eq!(validate_entry(&entry(PRIVILEGE_VIEW, AUTH_MODE_CASE, vec![0], None)), E);
        assert_eq!(validate_entry(&entry(PRIVILEGE_VIEW, AUTH_MODE_CASE, vec![cat_subject(0xABCD_0000)], None)), E);
        assert_eq!(validate_entry(&entry(PRIVILEGE_VIEW, AUTH_MODE_CASE, vec![OPERATIONAL_NODE_ID_MAX + 1], None)), E);
        assert_eq!(validate_entry(&entry(PRIVILEGE_VIEW, AUTH_MODE_CASE, vec![1, cat_subject(0xABCD_0000)], None)), E, "one bad subject poisons the entry");
        // Group subject の形
        assert_eq!(validate_entry(&entry(PRIVILEGE_OPERATE, AUTH_MODE_GROUP, vec![0], None)), E);
        assert_eq!(validate_entry(&entry(PRIVILEGE_OPERATE, AUTH_MODE_GROUP, vec![0x1_0000], None)), E);
        assert_eq!(validate_entry(&entry(PRIVILEGE_OPERATE, AUTH_MODE_GROUP, vec![cat_subject(0xABCD_0001)], None)), E);
        // targets 数
        let four = encode_targets_for_test(&[(Some(1), None, None); 4]);
        assert_eq!(validate_entry(&entry(PRIVILEGE_VIEW, AUTH_MODE_CASE, vec![1], Some(four))), E);
        // target: 全 null
        let empty = encode_targets_for_test(&[(None, None, None)]);
        assert_eq!(validate_entry(&entry(PRIVILEGE_VIEW, AUTH_MODE_CASE, vec![1], Some(empty))), E);
        // target: endpoint と device_type の同時指定
        let both = encode_targets_for_test(&[(None, Some(1), Some(0x0100))]);
        assert_eq!(validate_entry(&entry(PRIVILEGE_VIEW, AUTH_MODE_CASE, vec![1], Some(both))), E);
        // target: cluster 域外（下位 16 bit が 0x8000..=0xFBFF / 0xFFFF）
        for c in [0x8000u32, 0xFBFF, 0xFFFF, 0x0001_FFFF] {
            let t = encode_targets_for_test(&[(Some(c), None, None)]);
            assert_eq!(validate_entry(&entry(PRIVILEGE_VIEW, AUTH_MODE_CASE, vec![1], Some(t))), E, "cluster {c:#x}");
        }
        // target: endpoint 0xFFFF / device_type 下位 16 bit > 0xBFFF
        let ep = encode_targets_for_test(&[(None, Some(0xFFFF), None)]);
        assert_eq!(validate_entry(&entry(PRIVILEGE_VIEW, AUTH_MODE_CASE, vec![1], Some(ep))), E);
        let dt = encode_targets_for_test(&[(None, None, Some(0xC000))]);
        assert_eq!(validate_entry(&entry(PRIVILEGE_VIEW, AUTH_MODE_CASE, vec![1], Some(dt))), E);
        // target: decode 不能な raw
        assert_eq!(validate_entry(&entry(PRIVILEGE_VIEW, AUTH_MODE_CASE, vec![1], Some(vec![0xFF]))), E);
    }

    /// `write` は decode → validate → 容量の順: 全置換で 1 件でも不正なら
    /// store 不変で CONSTRAINT_ERROR、append も同じ。
    #[test]
    fn write_rejects_invalid_entries_and_leaves_store_intact() {
        let store = AclStore::new();
        store.add_case_admin(1, 111);
        let mut h = AccessControlHandler::new(store);
        let mut ctx = InvokeCtx {
            fabric_index: 1,
            ..InvokeCtx::default()
        };
        // 全置換: 正しい 1 件 + CAT v0 の 1 件
        let tlv = encode_entries_for_test(&[
            (PRIVILEGE_ADMINISTER, AUTH_MODE_CASE, vec![111]),
            (PRIVILEGE_VIEW, AUTH_MODE_CASE, vec![cat_subject(0xABCD_0000)]),
        ]);
        assert_eq!(h.write(im::ATTR_ACL, &tlv, false, &mut ctx), Err(im::STATUS_CONSTRAINT_ERROR));
        assert!(ctx.changed.is_empty());
        let entries = decode_entries_for_test(&h.read(im::ATTR_ACL, &read_ctx(1)).unwrap());
        assert_eq!(entries, vec![(PRIVILEGE_ADMINISTER, AUTH_MODE_CASE, vec![111], 1)]);
        // append: Administer × Group
        let tlv = encode_entry_for_test(PRIVILEGE_ADMINISTER, AUTH_MODE_GROUP, vec![0x0102]);
        assert_eq!(h.write(im::ATTR_ACL, &tlv, true, &mut ctx), Err(im::STATUS_CONSTRAINT_ERROR));
        assert_eq!(decode_entries_for_test(&h.read(im::ATTR_ACL, &read_ctx(1)).unwrap()).len(), 1);
        // append: `mat group grant` 形は通る
        let tlv = encode_entry_for_test(PRIVILEGE_OPERATE, AUTH_MODE_GROUP, vec![0x0102]);
        assert_eq!(h.write(im::ATTR_ACL, &tlv, true, &mut ctx), Ok(()));
        assert_eq!(decode_entries_for_test(&h.read(im::ATTR_ACL, &read_ctx(1)).unwrap()).len(), 2);
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-device --features net --lib access_control::tests::validate_entry`
Expected: コンパイルエラー（`validate_entry` / `encode_targets_for_test` 未定義）

- [ ] **Step 3: 実装**

`decode_targets` の直前に追加:

```rust
/// chip `IsValidClusterId`: 下位 16 bit が標準域 0x0000..=0x7FFF か
/// manufacturer-specific 域 0xFC00..=0xFFFE。
fn is_valid_cluster_id(cluster: u32) -> bool {
    matches!(cluster & 0xFFFF, 0x0000..=0x7FFF | 0xFC00..=0xFFFE)
}

/// chip `IsValidDeviceTypeId`: 下位 16 bit が 0x0000..=0xBFFF。
fn is_valid_device_type_id(device_type: u32) -> bool {
    (device_type & 0xFFFF) <= 0xBFFF
}

/// spec §11.1.7.1 `AccessControlEntryStruct` の制約 + chip
/// `AccessControl::Entry` validation 相当。`write` が decode 直後・容量
/// ガードの前に全エントリへかける — 1 件でも違反なら store に触らず
/// `STATUS_CONSTRAINT_ERROR`。規則は spec ドキュメント
/// (`docs/superpowers/specs/2026-09-03-matv-subject-checks-design.md` §3)
/// の表と 1:1。
pub(crate) fn validate_entry(entry: &AclDeviceEntry) -> Result<(), u8> {
    const ERR: u8 = im::STATUS_CONSTRAINT_ERROR;
    if !(PRIVILEGE_VIEW..=PRIVILEGE_ADMINISTER).contains(&entry.privilege) {
        return Err(ERR);
    }
    match entry.auth_mode {
        AUTH_MODE_CASE => {
            if !entry.subjects.iter().all(|s| subject_kind(*s).is_some()) {
                return Err(ERR);
            }
        }
        AUTH_MODE_GROUP => {
            // spec §11.1.7.1: "Administer privilege SHALL only be granted
            // to CASE"
            if entry.privilege == PRIVILEGE_ADMINISTER {
                return Err(ERR);
            }
            if !entry.subjects.iter().all(|s| is_group_subject(*s)) {
                return Err(ERR);
            }
        }
        _ => return Err(ERR), // PASE(1) / 未知値
    }
    if entry.subjects.len() > ACL_SUBJECTS_PER_ENTRY as usize {
        return Err(ERR);
    }
    if let Some(raw) = &entry.targets_raw {
        let targets = decode_targets(raw).ok_or(ERR)?;
        if targets.len() > ACL_TARGETS_PER_ENTRY as usize {
            return Err(ERR);
        }
        for t in &targets {
            if t.cluster.is_none() && t.endpoint.is_none() && t.device_type.is_none() {
                return Err(ERR);
            }
            if t.endpoint.is_some() && t.device_type.is_some() {
                return Err(ERR);
            }
            if t.cluster.is_some_and(|c| !is_valid_cluster_id(c)) {
                return Err(ERR);
            }
            if t.endpoint.is_some_and(|e| e == 0xFFFF) {
                return Err(ERR);
            }
            if t.device_type.is_some_and(|d| !is_valid_device_type_id(d)) {
                return Err(ERR);
            }
        }
    }
    Ok(())
}
```

`write` の 2 経路に配線（decode 直後、容量判定の前）:

```rust
        if list_append {
            let Some(entry) = decode_single_acl_entry(data_tlv) else {
                return Err(im::STATUS_CONSTRAINT_ERROR);
            };
            validate_entry(&entry)?;
            if self.store.entries_for(ctx.fabric_index).len() >= ACL_ENTRIES_PER_FABRIC {
```

```rust
            let Some(entries) = decode_acl_entries(data_tlv) else {
                return Err(im::STATUS_CONSTRAINT_ERROR);
            };
            for entry in &entries {
                validate_entry(entry)?;
            }
            if entries.len() > ACL_ENTRIES_PER_FABRIC {
```

`write` の doc コメントに 1 文追加: 「decode の直後に `validate_entry`（spec §11.1.7.1 の制約: privilege / auth mode / subject の形 / targets）を全エントリへかけ、1 件でも違反なら store 不変で `STATUS_CONSTRAINT_ERROR`。容量判定より先（正しいエントリが入り切らないときだけ RESOURCE_EXHAUSTED）。」

Task 1 で `#[cfg_attr(not(test), allow(dead_code))]` を付けていたら外す。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-device --features net --lib access_control::tests`
Expected: 全 PASS。既存の `full_replace_over_capacity_is_resource_exhausted` / `append_to_already_full_fabric_is_resource_exhausted` が使うエントリ形（subject 1 等）が validate を通ることを確認 — 落ちるなら、そのテストのエントリを妥当な値（subject ≥ 1、privilege 1..=5、auth mode 2）に直す（意図はそのまま）。

- [ ] **Step 5: clippy / fmt / Commit**

Run: `cargo fmt --all && cargo clippy -p mat-device --features net --all-targets -- -D warnings`

```bash
git add crates/mat-device/src/core/access_control.rs
git commit -m "feat(mat-device): ACL write のエントリ妥当性検査 — subject の形・auth mode・targets 違反は CONSTRAINT_ERROR（レーン A Task 4）"
```

---

### Task 5: ACL write 検査の閉ループ統合テスト

**Files:**
- Create: `crates/mat-device/tests/acl_write_validation.rs`

**Interfaces:**
- Consumes: `support::{commission_directly, device_config, fast_cfg}`、`mat_device::core::access_control::cat_subject`、`SecureSession::{write_attribute_tlv, read_attribute_json}`

- [ ] **Step 1: 統合テストを書く**

```rust
//! Closed-loop proof of the ACL write validation (spec §11.1.7.1 entry
//! constraints, `access_control::validate_entry`) over a real CASE
//! session: a full replace carrying a CAT subject with version 0 is
//! answered with `CONSTRAINT_ERROR` and leaves the store untouched (the
//! admin can still read the ACL — its automatic Administer entry survived),
//! and the entry shape `mat group grant` writes (Operate / Group auth mode
//! / subject = group id / no targets) is accepted as a list append.
//!
//! Same direct-drive setup as `acl_enforce.rs` (`tests/support/mod.rs`).
#![cfg(feature = "net")]

use std::net::SocketAddr;

use mat_controller::commissioning::CommissioningFabric;
use mat_controller::im::{self, ImError};
use mat_controller::session::SessionError;
use mat_controller::tlv::{Tag, Writer};

use mat_device::core::access_control::cat_subject;
use mat_device::device::Device;

mod support;
use support::{commission_directly, device_config};

const ADMIN_NODE_ID: u64 = 660_033;
const GROUP_ID: u16 = 0x0102;

/// spec §11.1.7.1 enums, mirrored as in `acl_enforce.rs`.
const PRIVILEGE_VIEW: u8 = 1;
const PRIVILEGE_OPERATE: u8 = 3;
const PRIVILEGE_ADMINISTER: u8 = 5;
const AUTH_MODE_CASE: u8 = 2;
const AUTH_MODE_GROUP: u8 = 3;
const FABRIC_INDEX: u8 = 1;

/// One `AccessControlEntryStruct` (`{1: privilege, 2: authMode, 3:
/// subjects, 4: null, 254: fabricIndex}`) into `w`.
fn put_entry(w: &mut Writer, privilege: u8, auth_mode: u8, subjects: &[u64]) {
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(1), u64::from(privilege));
    w.put_uint(Tag::Context(2), u64::from(auth_mode));
    w.start_array(Tag::Context(3));
    for s in subjects {
        w.put_uint(Tag::Anonymous, *s);
    }
    w.end_container();
    w.put_null(Tag::Context(4));
    w.put_uint(Tag::Context(254), u64::from(FABRIC_INDEX));
    w.end_container();
}

/// Full-replace Data TLV: an array of entries.
fn entries_tlv(entries: &[(u8, u8, &[u64])]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    for (privilege, auth_mode, subjects) in entries {
        put_entry(&mut w, *privilege, *auth_mode, subjects);
    }
    w.end_container();
    w.finish()
}

#[tokio::test]
async fn invalid_acl_write_is_constraint_error_and_group_grant_shape_is_accepted() {
    let store_dir = tempfile::tempdir().expect("tempdir");
    let device = Device::new(device_config(store_dir.path().to_path_buf())).expect("device new");
    let addr = SocketAddr::new(
        std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        device.local_addr().port(),
    );
    let paa_der = std::fs::read(store_dir.path().join("paa").join("paa.der"))
        .expect("device should have written its PAA DER at Device::new");
    let device_task = tokio::spawn(async move {
        let _ = device.run().await;
    });

    let fabric =
        CommissioningFabric::generate(0x2233_4455, ADMIN_NODE_ID).expect("fabric generate");
    let mut session = commission_directly(addr, &paa_der, &fabric).await;
    let cfg = support::fast_cfg();

    // 1. Full replace with a valid admin entry *and* a CAT-version-0 entry:
    //    rejected as a whole, CONSTRAINT_ERROR on the attribute path.
    let bad = entries_tlv(&[
        (PRIVILEGE_ADMINISTER, AUTH_MODE_CASE, &[ADMIN_NODE_ID]),
        (PRIVILEGE_VIEW, AUTH_MODE_CASE, &[cat_subject(0xABCD_0000)]),
    ]);
    let res = session
        .write_attribute_tlv(0, im::CLUSTER_ACCESS_CONTROL, im::ATTR_ACL, &bad, None, &cfg)
        .await;
    match res {
        Err(SessionError::Im(ImError::AttributeStatus(status))) => assert_eq!(
            status,
            im::STATUS_CONSTRAINT_ERROR,
            "CAT version 0 subject must be CONSTRAINT_ERROR, got {status:#04x}"
        ),
        other => panic!("expected AttributeStatus(CONSTRAINT_ERROR), got {other:?}"),
    }

    // 2. The store is untouched: the automatic admin entry still grants
    //    Administer, so the ACL read goes through and shows exactly it.
    let acl = session
        .read_attribute_json(0, im::CLUSTER_ACCESS_CONTROL, im::ATTR_ACL, &cfg)
        .await
        .expect("ACL read must still succeed — the rejected write must not have replaced anything");
    assert_eq!(acl.as_array().map(Vec::len), Some(1), "store must be unchanged: {acl}");

    // 3. `mat group grant` shape (Operate / Group / [group id] / no
    //    targets) next to the admin entry, as a full replace: accepted.
    //    (`SecureSession::write_attribute_tlv` has no list-append form, so
    //    the replace carries both entries — the same wire shape `mat group
    //    grant` ends up writing after its read-merge-write.)
    let grant = entries_tlv(&[
        (PRIVILEGE_ADMINISTER, AUTH_MODE_CASE, &[ADMIN_NODE_ID]),
        (PRIVILEGE_OPERATE, AUTH_MODE_GROUP, &[u64::from(GROUP_ID)]),
    ]);
    session
        .write_attribute_tlv(0, im::CLUSTER_ACCESS_CONTROL, im::ATTR_ACL, &grant, None, &cfg)
        .await
        .expect("the group-grant entry shape must be accepted");
    let acl = session
        .read_attribute_json(0, im::CLUSTER_ACCESS_CONTROL, im::ATTR_ACL, &cfg)
        .await
        .expect("ACL read after the grant-shaped replace");
    assert_eq!(acl.as_array().map(Vec::len), Some(2), "admin + group entries: {acl}");

    device_task.abort();
    let _ = device_task.await;
}
```

- [ ] **Step 2: テストが通ることを確認**

Run: `cargo test -p mat-device --features net --test acl_write_validation`
Expected: PASS

- [ ] **Step 3: clippy / fmt / Commit**

Run: `cargo fmt --all && cargo clippy -p mat-device --features net --all-targets -- -D warnings`

```bash
git add crates/mat-device/tests/acl_write_validation.rs
git commit -m "test(mat-device): ACL write 検査の閉ループ — CAT v0 は CONSTRAINT_ERROR、group grant 形は受理（レーン A Task 5）"
```

---

### Task 6: `Node::has_readable_path`（値を読まない ACL 込み展開）

**Files:**
- Modify: `crates/mat-device/src/core/datamodel.rs`（`read_entries` の直後、tests に `acl_denied_read_...` の隣）

**Interfaces:**
- Consumes: 既存 `ExpandCtx`、`Node::read_allowed`、`ClusterHandler::{cluster_id, attributes}`、`im::AttrPathIn`
- Produces: `pub fn has_readable_path(&self, paths: &[AttrPathIn], read_ctx: &ReadCtx) -> bool`

規則（chip `InteractionModelEngine::ParseAttributePaths` と同じ）: **具体パス（3 フィールド全部 Some）は常に「有効」**（存在しない/不許可はそのパスの priming に status entry として出るのが正しい応答）、**wildcard を含むパスは ACL 込みで読める属性に 1 つ以上展開されるときだけ有効**。paths のどれか 1 つでも有効なら true。paths 空は false。

- [ ] **Step 1: 失敗するユニットテストを書く**

`acl_denied_read_is_a_status_on_a_concrete_path_but_silent_under_a_wildcard` の直後:

```rust
    /// spec §8.10 / chip `ParseAttributePaths`: 購読の受理判定。wildcard
    /// パスは ACL 込みで 1 つでも読める属性に展開されれば有効、具体パスは
    /// （存在・権限に関わらず）常に有効 — その拒否は priming の status
    /// entry として伝わる。paths 空は無効。
    #[test]
    fn has_readable_path_follows_the_subscription_validity_rule() {
        let node = node_with_acl(PRIVILEGE_OPERATE, 7);
        let allowed = case_read_ctx(1, 7);
        let denied = case_read_ctx(1, 8);
        let full_wildcard = AttrPathIn {
            endpoint: None,
            cluster: None,
            attribute: None,
        };
        let onoff_wildcard = AttrPathIn {
            endpoint: None,
            cluster: Some(im::CLUSTER_ON_OFF),
            attribute: None,
        };
        let concrete = AttrPathIn {
            endpoint: Some(1),
            cluster: Some(im::CLUSTER_ON_OFF),
            attribute: Some(im::ATTR_ON_OFF),
        };
        let missing_cluster_wildcard = AttrPathIn {
            endpoint: None,
            cluster: Some(0x7FFF),
            attribute: None,
        };

        assert!(node.has_readable_path(&[full_wildcard.clone()], &allowed));
        assert!(node.has_readable_path(&[onoff_wildcard.clone()], &allowed));
        assert!(!node.has_readable_path(&[full_wildcard.clone()], &denied));
        assert!(!node.has_readable_path(&[onoff_wildcard.clone()], &denied));
        assert!(!node.has_readable_path(&[missing_cluster_wildcard], &allowed));
        // 具体パスは拒否 subject でも「有効」（status entry で答える経路）。
        assert!(node.has_readable_path(&[concrete.clone()], &denied));
        // 1 つでも有効なら全体は有効。
        assert!(node.has_readable_path(&[full_wildcard, concrete], &denied));
        assert!(!node.has_readable_path(&[], &allowed));
    }
```

（`AttrPathIn` が `Clone` でなければ `.clone()` を外して都度リテラルを書く。）

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-device --features net --lib datamodel::tests::has_readable_path_follows_the_subscription_validity_rule`
Expected: コンパイルエラー（`has_readable_path` 未定義）

- [ ] **Step 3: 実装**

`read_entries` の直後に追加:

```rust
    /// Whether a SubscribeRequest's `paths` may be accepted at all (spec
    /// §8.10; chip `InteractionModelEngine::ParseAttributePaths`): a path
    /// with any wildcard field counts only if it expands to at least one
    /// attribute this session may read (ACL included — `read_allowed`,
    /// the same gate `read_entries` applies), while a fully concrete path
    /// always counts (a missing or refused attribute is answered by a
    /// status entry in the priming report instead). No values are read.
    /// `false` for an empty `paths`; the caller answers `INVALID_ACTION`.
    pub fn has_readable_path(&self, paths: &[AttrPathIn], read_ctx: &ReadCtx) -> bool {
        paths.iter().any(|path| {
            if path.endpoint.is_some() && path.cluster.is_some() && path.attribute.is_some() {
                return true;
            }
            self.endpoints
                .iter()
                .filter(|(ep, _)| path.endpoint.is_none_or(|e| e == *ep))
                .any(|(endpoint, clusters)| {
                    let ectx = ExpandCtx {
                        endpoint: *endpoint,
                        clusters,
                        read_ctx,
                    };
                    clusters
                        .iter()
                        .filter(|h| path.cluster.is_none_or(|c| c == h.cluster_id()))
                        .any(|handler| match path.attribute {
                            Some(attribute) => {
                                self.read_allowed(&ectx, handler.as_ref(), attribute)
                            }
                            None => handler
                                .attributes()
                                .into_iter()
                                .any(|a| self.read_allowed(&ectx, handler.as_ref(), a)),
                        })
                })
        })
    }
```

`self.endpoints` の実型が `Vec<(u16, Vec<Box<dyn ClusterHandler>>)>` でない場合（`expand_endpoint` の `self.endpoints.iter().find(|(id, _)| ...)` を見て）合わせる。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-device --features net --lib datamodel::tests`
Expected: 全 PASS

- [ ] **Step 5: clippy / fmt / Commit**

```bash
git add crates/mat-device/src/core/datamodel.rs
git commit -m "feat(mat-device): Node::has_readable_path — 購読受理判定用の値を読まない ACL 込み展開（レーン A Task 6）"
```

---

### Task 7: Subscribe の INVALID_ACTION 拒否（既存購読は残す）+ 統合テスト

**Files:**
- Modify: `crates/mat-device/src/net/runtime.rs`（`serve_secured_message` 1212 行付近、`serve_subscribe_request` 1475-1580 行）
- Create: `crates/mat-device/tests/subscribe_denied.rs`

**Interfaces:**
- Consumes: Task 6 の `Node::has_readable_path`、既存 `session_subject`、`reply_reliable`、`im::{OPCODE_STATUS_RESPONSE, STATUS_INVALID_ACTION, encode_status_response}`
- Produces: `enum SubscribeOutcome { Installed(ActiveSubscription), TornDown, Rejected }`（private）、`serve_subscribe_request(..) -> SubscribeOutcome`

- [ ] **Step 1: 失敗する統合テストを書く**

`crates/mat-device/tests/subscribe_denied.rs`:

```rust
//! Closed-loop proof of the subscribe acceptance rule (spec §8.10, chip
//! `ParseAttributePaths`): a wildcard SubscribeRequest that expands to no
//! attribute the session may read is answered with
//! `StatusResponse(INVALID_ACTION)` instead of an empty subscription — and
//! the rejection leaves the session's *existing* subscription alive.
//!
//! Scenario: after commissioning, demote the admin's automatic ACL entry
//! to Operate (same move as `acl_enforce.rs`), subscribe to OnOff
//! (Operate-readable — succeeds), then subscribe to the AccessControl
//! cluster (Administer-gated, so the wildcard expands to nothing readable
//! — rejected). Finally turn the light on and expect the *first*
//! subscription's report, proving the rejected request did not tear it
//! down.
#![cfg(feature = "net")]

use std::net::SocketAddr;
use std::time::Duration;

use mat_controller::commissioning::CommissioningFabric;
use mat_controller::im;
use mat_controller::session::SecureSession;
use mat_controller::tlv::{Tag, Writer};

use mat_device::device::Device;

mod support;
use support::{commission_directly, device_config};

const ADMIN_NODE_ID: u64 = 660_033;
const PRIVILEGE_OPERATE: u8 = 3;
const AUTH_MODE_CASE: u8 = 2;
const FABRIC_INDEX: u8 = 1;
/// Generous: the change report is due at MinIntervalFloor = 0.
const REPORT_WAIT: Duration = Duration::from_secs(10);

fn operate_entry_tlv() -> Vec<u8> {
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(1), u64::from(PRIVILEGE_OPERATE));
    w.put_uint(Tag::Context(2), u64::from(AUTH_MODE_CASE));
    w.start_array(Tag::Context(3));
    w.put_uint(Tag::Anonymous, ADMIN_NODE_ID);
    w.end_container();
    w.put_null(Tag::Context(4));
    w.put_uint(Tag::Context(254), u64::from(FABRIC_INDEX));
    w.end_container();
    w.end_container();
    w.finish()
}

#[tokio::test]
async fn unreadable_wildcard_subscribe_is_invalid_action_and_keeps_the_existing_subscription() {
    let store_dir = tempfile::tempdir().expect("tempdir");
    let device = Device::new(device_config(store_dir.path().to_path_buf())).expect("device new");
    let addr = SocketAddr::new(
        std::net::IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
        device.local_addr().port(),
    );
    let paa_der = std::fs::read(store_dir.path().join("paa").join("paa.der"))
        .expect("device should have written its PAA DER at Device::new");
    let device_task = tokio::spawn(async move {
        let _ = device.run().await;
    });

    let fabric =
        CommissioningFabric::generate(0x2233_4455, ADMIN_NODE_ID).expect("fabric generate");
    let mut session = commission_directly(addr, &paa_der, &fabric).await;
    let cfg = support::fast_cfg();

    // Demote to Operate (judged against the pre-replace Administer entry).
    session
        .write_attribute_tlv(0, im::CLUSTER_ACCESS_CONTROL, im::ATTR_ACL, &operate_entry_tlv(), None, &cfg)
        .await
        .expect("demoting ACL write");

    // 1. OnOff wildcard subscription: Operate may read it — accepted.
    let (sr, _priming) = session
        .subscribe_wildcard(0, 5, false, &[im::CLUSTER_ON_OFF], &cfg)
        .await
        .expect("OnOff subscription must be accepted for an Operate subject");

    // 2. A wildcard path whose only expansion is Administer-gated:
    //    endpoint=wildcard, cluster=AccessControl, attribute=ACL. Operate
    //    may not read it anywhere, so the request has no readable path.
    //    `SecureSession::subscribe_wildcard` can only express cluster-only
    //    paths, so the SubscribeRequest is hand-rolled here (same layout
    //    as `im::encode_subscribe_request`: `{0: KeepSubscriptions, 1:
    //    MinIntervalFloor, 2: MaxIntervalCeiling, 3: AttributeRequests
    //    [AttributePathIB{3: cluster, 4: attribute}], 7: IsFabricFiltered,
    //    255: rev}`) and driven with the session's raw exchange API.
    let req = {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bool(Tag::Context(0), false);
        w.put_uint(Tag::Context(1), 0);
        w.put_uint(Tag::Context(2), 5);
        w.start_array(Tag::Context(3));
        w.start_list(Tag::Anonymous);
        w.put_uint(Tag::Context(3), u64::from(im::CLUSTER_ACCESS_CONTROL));
        w.put_uint(Tag::Context(4), u64::from(im::ATTR_ACL));
        w.end_container();
        w.end_container();
        w.put_bool(Tag::Context(7), true);
        w.put_uint(Tag::Context(255), u64::from(im::IM_REVISION));
        w.end_container();
        w.finish()
    };
    let exchange_id = SecureSession::new_exchange_id();
    let piggybacked = session
        .send_reliable(exchange_id, im::PROTOCOL_ID_IM, im::OPCODE_SUBSCRIBE_REQUEST, &req, &cfg)
        .await
        .expect("SubscribeRequest send");
    let reply = match piggybacked {
        Some(m) => m,
        None => session
            .recv(exchange_id, Duration::from_secs(5))
            .await
            .expect("a reply to the SubscribeRequest"),
    };
    assert_eq!(
        reply.proto.opcode,
        im::OPCODE_STATUS_RESPONSE,
        "an unreadable wildcard subscribe must be answered with a StatusResponse, got opcode {:#04x}",
        reply.proto.opcode
    );
    let status = im::decode_status_response(&reply.payload).expect("decode StatusResponse");
    assert_eq!(status, im::STATUS_INVALID_ACTION, "got {status:#04x}");

    // 3. The first subscription survived the rejection: a change report
    //    still arrives, tagged with its SubscriptionId.
    session
        .invoke(support::BRIDGED_EP, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON, None, &cfg)
        .await
        .expect("On invoke should succeed");
    let rd = session
        .next_subscription_report(REPORT_WAIT, &cfg)
        .await
        .expect("the OnOff subscription must still deliver after the rejected request");
    assert_eq!(rd.subscription_id, Some(sr.subscription_id));
    assert!(
        rd.reports.iter().any(|r| r.attribute == Some(im::ATTR_ON_OFF)),
        "change report must carry OnOff: {:?}",
        rd.reports
    );

    device_task.abort();
    let _ = device_task.await;
}
```

`SecureSession::{new_exchange_id, send_reliable, recv}` と `im::{PROTOCOL_ID_IM, OPCODE_SUBSCRIBE_REQUEST, OPCODE_STATUS_RESPONSE, IM_REVISION, decode_status_response}` は全部 pub（確認済み）。`reply.proto.opcode` / `reply.payload` は `mat_controller::exchange::IncomingMessage` のフィールド名（runtime.rs の `is_status_response_ok` と同じ）。

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-device --features net --test subscribe_denied`
Expected: FAIL — 2 本目の subscribe が `Ok`（SubscribeResponse が返る）で `expected StatusResponse(INVALID_ACTION), got Ok(..)` の panic

- [ ] **Step 3: 実装**

`serve_subscribe_request` の直前に enum を追加:

```rust
/// What `serve_subscribe_request` did to the node's single subscription slot.
enum SubscribeOutcome {
    /// A new subscription is live (replaces whatever was there).
    Installed(ActiveSubscription),
    /// The request was accepted but the interaction failed partway
    /// (undecodable request, priming send failure, missing ack) — the peer
    /// asked to start over and the flow broke, so nothing is subscribed.
    TornDown,
    /// The request was refused up front with `StatusResponse(INVALID_ACTION)`
    /// (spec §8.10: no readable path) — a refusal, not a restart, so the
    /// existing subscription (if any) is left alone, as chip does.
    Rejected,
}
```

`serve_subscribe_request` の戻りを `SubscribeOutcome` に変え、既存の `return None` を全部 `return SubscribeOutcome::TornDown`、末尾の `Some(ActiveSubscription {..})` を `SubscribeOutcome::Installed(ActiveSubscription {..})` に。`read_ctx` を組んだ直後・`read_chunks` の前に挿入:

```rust
    // spec §8.10 / chip `ParseAttributePaths`: a request none of whose
    // paths can yield anything this subject may read is refused outright
    // rather than answered with an empty priming report and a dead
    // subscription. (Concrete paths always count — their refusal shows up
    // as a status entry in the priming report; see
    // `Node::has_readable_path`.)
    if !node.has_readable_path(&req.paths, &read_ctx) {
        tracing::debug!(
            exchange_id = msg.proto.exchange_id,
            paths = ?req.paths,
            subject = ?read_ctx.subject,
            fabric_index,
            "SubscribeRequest rejected: no readable attribute path (INVALID_ACTION)"
        );
        let reply_result = session
            .reply_reliable(
                msg,
                PROTOCOL_ID_INTERACTION_MODEL,
                im::OPCODE_STATUS_RESPONSE,
                &im::encode_status_response(im::STATUS_INVALID_ACTION),
                &reply_cfg(),
            )
            .await;
        if let Err(e) = reply_result {
            tracing::debug!(exchange_id = msg.proto.exchange_id, error = %e, "INVALID_ACTION StatusResponse not delivered");
        }
        return SubscribeOutcome::Rejected;
    }
```

`serve_secured_message` の呼び出し側:

```rust
        if msg.proto.opcode == im::OPCODE_SUBSCRIBE_REQUEST {
            match serve_subscribe_request(&msg, session, fabric_index, node).await {
                SubscribeOutcome::Installed(sub) => **subscription = Some(sub),
                SubscribeOutcome::TornDown => **subscription = None,
                SubscribeOutcome::Rejected => {}
            }
            return ServeOutcome::Continue;
        }
```

その直前のコメント（"Success installs the node's one active subscription; failure anywhere in the flow leaves it with none"）に「an up-front `INVALID_ACTION` refusal is the exception: it leaves the existing subscription alone (`SubscribeOutcome::Rejected`)」を足す。

`reply_reliable` が `Option<IncomingMessage>`（piggyback）を返す場合、拒否応答に相手が MRP ack だけ返すのが普通なので戻り値は捨ててよい。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-device --features net --test subscribe_denied --test subscribe_loop`
Expected: 両方 PASS。runtime の既存ユニットテスト（`cargo test -p mat-device --features net --lib runtime`）も PASS。

- [ ] **Step 5: clippy / fmt / Commit**

```bash
git add crates/mat-device/src/net/runtime.rs crates/mat-device/tests/subscribe_denied.rs
git commit -m "feat(mat-device): 読める属性の無い SubscribeRequest を INVALID_ACTION で拒否、既存購読は維持（レーン A Task 7）"
```

---

### Task 8: dirty report の wildcard 由来 status entry を落とす

**Files:**
- Modify: `crates/mat-device/src/net/subscription.rs`（`covers` の直後、tests）
- Modify: `crates/mat-device/src/net/runtime.rs`（`send_subscription_report` 1604-1650 行付近、`let entries = ...` の直後）

**Interfaces:**
- Produces:
  - `ActiveSubscription::covered_concretely(&self, path: (u16, u32, u32)) -> bool`
  - `pub fn retain_reportable(sub: &ActiveSubscription, entries: Vec<ReportEntryOut>) -> Vec<ReportEntryOut>`（`mat_controller::im::ReportEntryOut`）

- [ ] **Step 1: 失敗するユニットテストを書く**

`subscription.rs` の tests に追加（既存 `sub(..)` ヘルパは `dirty` を取る — `paths` を指定できる派生を足す）:

```rust
    fn sub_with_paths(paths: Vec<AttrPathIn>) -> ActiveSubscription {
        ActiveSubscription {
            id: 1,
            paths,
            fabric_filtered: true,
            min_interval: Duration::from_secs(0),
            max_interval: Duration::from_secs(5),
            last_report_at: Instant::now(),
            dirty: Vec::new(),
        }
    }

    fn status_entry(endpoint: u16, cluster: u32, attribute: u32) -> ReportEntryOut {
        ReportEntryOut::Status {
            endpoint,
            cluster,
            attribute,
            status: im::STATUS_UNSUPPORTED_ACCESS,
        }
    }

    #[test]
    fn covered_concretely_is_true_only_for_a_fully_concrete_subscribed_path() {
        let concrete = AttrPathIn {
            endpoint: Some(1),
            cluster: Some(im::CLUSTER_ON_OFF),
            attribute: Some(im::ATTR_ON_OFF),
        };
        let wildcard = AttrPathIn {
            endpoint: None,
            cluster: Some(im::CLUSTER_ON_OFF),
            attribute: None,
        };
        let path = (1, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF);
        assert!(sub_with_paths(vec![concrete.clone()]).covered_concretely(path));
        assert!(!sub_with_paths(vec![wildcard.clone()]).covered_concretely(path));
        // 両方 cover していれば「具体的に頼まれた」側が勝つ。
        assert!(sub_with_paths(vec![wildcard, concrete]).covered_concretely(path));
    }

    /// spec §8.4.2.2 の非対称を dirty report にも適用: wildcard 購読が
    /// 拾った不許可属性の status entry は落とし（priming と同じ「黙る」）、
    /// 具体パス購読の status entry と Data は残す。
    #[test]
    fn retain_reportable_drops_status_entries_only_under_wildcard_paths() {
        let onoff_wildcard = AttrPathIn {
            endpoint: None,
            cluster: Some(im::CLUSTER_ON_OFF),
            attribute: None,
        };
        let acl_concrete = AttrPathIn {
            endpoint: Some(0),
            cluster: Some(im::CLUSTER_ACCESS_CONTROL),
            attribute: Some(im::ATTR_ACL),
        };
        let sub = sub_with_paths(vec![onoff_wildcard, acl_concrete]);
        let data = ReportEntryOut::Data(im::AttrReportOut {
            endpoint: 1,
            cluster: im::CLUSTER_ON_OFF,
            attribute: im::ATTR_ON_OFF,
            data_version: 0,
            value_tlv: vec![0x09], // TLV true
        });
        let entries = vec![
            data.clone(),
            status_entry(1, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF), // wildcard → 落ちる
            status_entry(0, im::CLUSTER_ACCESS_CONTROL, im::ATTR_ACL), // 具体 → 残る
        ];
        let kept = retain_reportable(&sub, entries);
        assert_eq!(kept.len(), 2);
        assert!(matches!(&kept[0], ReportEntryOut::Data(d) if d.attribute == im::ATTR_ON_OFF));
        assert!(matches!(
            &kept[1],
            ReportEntryOut::Status { cluster, attribute, .. }
                if *cluster == im::CLUSTER_ACCESS_CONTROL && *attribute == im::ATTR_ACL
        ));
    }
```

`ReportEntryOut` / `AttrReportOut` の実フィールド名は `crates/mat-controller/src/im.rs` で確認（`/usr/bin/grep -n "pub enum ReportEntryOut" -A 12 crates/mat-controller/src/im.rs`、`pub struct AttrReportOut` も）。`Clone` が無ければ `data` を 2 回組み立てる。

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-device --features net --lib subscription::tests`
Expected: コンパイルエラー（`covered_concretely` / `retain_reportable` 未定義）

- [ ] **Step 3: 実装**

`covers` の直後:

```rust
    /// Whether some subscribed path names `path` with **all three fields
    /// concrete** — i.e. the subscriber asked for exactly this attribute,
    /// not for a wildcard that happens to expand to it. Decides whether a
    /// refused attribute in a dirty report is answered with a status entry
    /// (concrete: yes) or dropped (wildcard: silent, the same asymmetry
    /// `Node::read_entries` applies to the priming report — spec §8.4.2.2).
    pub fn covered_concretely(&self, path: (u16, u32, u32)) -> bool {
        self.paths.iter().any(|p| {
            p.endpoint.is_some() && p.cluster.is_some() && p.attribute.is_some()
                && path_matches(p, path)
        })
    }
```

`path_matches` の直後（自由関数）:

```rust
/// Filters one dirty report's entries the way `Node::read_entries` already
/// filters a priming report: `Data` always stays; a `Status` entry stays
/// only when the subscriber named that attribute concretely
/// (`covered_concretely`). The dirty set is read back through *concrete*
/// paths (`send_subscription_report` builds them from `dirty`), so without
/// this every attribute a wildcard subscription is not allowed to read
/// would resurface as an `UNSUPPORTED_ACCESS` status entry on every
/// change — the asymmetry the priming report avoids.
pub fn retain_reportable(sub: &ActiveSubscription, entries: Vec<ReportEntryOut>) -> Vec<ReportEntryOut> {
    entries
        .into_iter()
        .filter(|e| match e {
            ReportEntryOut::Data(_) => true,
            ReportEntryOut::Status {
                endpoint,
                cluster,
                attribute,
                ..
            } => sub.covered_concretely((*endpoint, *cluster, *attribute)),
        })
        .collect()
}
```

`use mat_controller::im::ReportEntryOut;` を subscription.rs の use に追加。

`runtime.rs::send_subscription_report`:

```rust
    let entries = if paths.is_empty() {
        Vec::new()
    } else {
        crate::net::subscription::retain_reportable(sub, node.read_entries(&paths, &read_ctx))
    };
```

そのすぐ上の "Values are read *now*" コメントの末尾に「`retain_reportable` drops the status entries a wildcard subscription would otherwise get for attributes it may not read (see its doc)」を足す。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-device --features net --lib subscription::tests && cargo test -p mat-device --features net --test subscribe_loop --test subscribe_denied`
Expected: 全 PASS

- [ ] **Step 5: clippy / fmt / Commit**

```bash
git add crates/mat-device/src/net/subscription.rs crates/mat-device/src/net/runtime.rs
git commit -m "fix(mat-device): dirty report で wildcard 購読の不許可属性が 0x7E status になる非対称を解消（レーン A Task 8）"
```

---

### Task 9: 全体検証（task check + e2e:device:m1 / m3）と doc 整合

**Files:**
- Modify（必要なら）: `crates/mat-device/src/core/datamodel.rs` モジュール doc / `crates/mat-device/src/net/subscription.rs` モジュール doc（「dirty report は wildcard 由来の拒否を黙らせる」「読める属性ゼロは INVALID_ACTION」の 1 文ずつ）
- Modify: `docs/superpowers/specs/2026-09-03-matv-subject-checks-design.md` §4（Task 6 で確定した「具体パスは常に有効」の規則を反映）

- [ ] **Step 1: spec §4 を実装に合わせる**

§4 の `has_readable_path` の doc コメントの直後に 1 段落: 「規則は chip `ParseAttributePaths` と同じ: **具体パス（3 フィールド全部 Some）は存在・権限に関わらず有効**（拒否は priming の status entry で伝える）、wildcard を含むパスは ACL 込みで読める属性に 1 つ以上展開されるときだけ有効。paths 空は無効。」§6 の「has_readable_path: 全許可 Node / 拒否 subject で wildcard・具体パス・空 paths」はそのまま（具体パスは true を期待）。

- [ ] **Step 2: `task check`**

Run: `task check`
Expected: fmt:check / clippy / test 全部緑（ワークスペース全体。mat-device 以外に差分が無いことも `git status` で確認）。`mat-controller` の `group::tests::send_invoke_emits_identical_datagram_on_each_egress` が稀に flaky（既知、単独 pass）— 落ちたらそのテストだけ `cargo test -p mat-controller send_invoke_emits_identical_datagram_on_each_egress` で単独再実行して pass を確認。

- [ ] **Step 3: e2e（直列）**

Run: `task e2e:device:m1`
Expected: PASS（末尾に `PASS` 行）

Run: `task e2e:device:m3`
Expected: PASS（`mat group provision` が status=provisioned — Task 4 の validate が `mat group grant` 形（Operate/Group/[gid]/targets null）を通す実線検証 — と、`mat listen` の on-off イベント受信）

いずれも `MAT_E2E_IFACE` は既定（eth1）。iface が無い環境なら `MAT_E2E_IFACE=<iface>` を付ける（`ip -6 addr | grep fe80` で候補確認）。

- [ ] **Step 4: Commit**

```bash
git add docs/superpowers/specs/2026-09-03-matv-subject-checks-design.md crates/mat-device
git commit -m "docs(mat-device): 購読受理規則・dirty report フィルタの doc 整合（レーン A Task 9）"
```

（差分が spec だけならそれだけコミット。）

---

## Self-Review

- **Spec coverage:** §1 → Task 1、§2 → Task 2+3、§3 → Task 4+5（`AUTH_MODE_CASE` doc 更新は Task 1 に含む）、§4 → Task 6+7（既存購読を残す = `SubscribeOutcome::Rejected`）、§5 → Task 8、§6 テスト → 各タスク + Task 9 の e2e、§7 やらないこと → どのタスクも mat-device 外に触れない。
- **Placeholder scan:** 「既存コードをそのまま移動」（Task 3）は移動対象を行番号と手順で特定済み。Task 5 は全置換 2 件（controller 側に list-append API が無い）、Task 7 は hand-rolled SubscribeRequest + raw exchange API（どちらも pub を確認済み）。
- **Type consistency:** `subject_kind -> Option<SubjectKind{Node,Cat}>`（Task 1）を Task 2/4 が `.is_none()` / `.is_some()` で使う。`is_group_subject(u64) -> bool`（Task 1）を Task 4。`has_readable_path(&[AttrPathIn], &ReadCtx) -> bool`（Task 6）を Task 7。`covered_concretely((u16,u32,u32)) -> bool` / `retain_reportable(&ActiveSubscription, Vec<ReportEntryOut>) -> Vec<ReportEntryOut>`（Task 8）。support の `pase_until_add_noc / add_noc / case_and_complete`（Task 3）の署名は本文どおり。
