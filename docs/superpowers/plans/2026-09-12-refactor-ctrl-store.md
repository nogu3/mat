# Refactor lane `ctrl-store` — mat-controller KVS / cert / commissioning / dnssd

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 監査バックログ（`mat-wt/tasks/audit.md`、main 411ba68 時点）のうちレーン
`ctrl-store` 担当分（Tier 1 死コード、Tier 2 の cert/x509/kvs/dnssd/commissioning
重複、Tier 6 テスト基盤、バグ候補 2 件）を **挙動変更 0** で消化する。

**Architecture:** 純粋なリファクタ。公開 API の削除は「本番呼び手ゼロ」を grep で
確認済みのものだけ（`kvs::read_fabric_credentials` 一式、`cert::cert_time_valid`、
`FabricError::{OpKeyMismatch,NocMissingIds,GenKey}`）。共有ヘルパは呼び手の
モジュール階層に合わせて置く（`asn1::oids` / `kvs::fs_util` / `dnssd::codec`）。
KVS（chip-tool INI）と dnssd の 3 本の send/recv ループは **pure move のみ**。

**Tech Stack:** Rust 2021 / workspace crate `mat-controller` / `cargo test -p mat-controller` / `task check`

**Spec:** `/home/noguk/ghq/github.com/nogu3/mat-wt/tasks/ctrl-store.md`（担当項目 1〜10）
と `/home/noguk/ghq/github.com/nogu3/mat-wt/tasks/_common.md`（共通ルール）、
バックログ本文 `/home/noguk/ghq/github.com/nogu3/mat-wt/tasks/audit.md`。

## Global Constraints

- worktree: `/home/noguk/ghq/github.com/nogu3/mat-wt/ctrl-store`、branch `refactor/ctrl-store`。**main へのマージ・push・release・deploy はしない。**
- **触ってよいファイル**（これ以外は編集禁止 — 他レーンが並行編集中）:
  `crates/mat-controller/src/{kvs,group_settings,group,fabric,cert,x509,asn1,cd,attestation,setup_code}.rs`、
  `crates/mat-controller/src/commissioning/*`、`crates/mat-controller/src/dnssd/*`、
  `crates/mat-controller/examples/ble-scan.rs`、この計画ファイル、
  `docs/superpowers/specs/2026-08-09-noc-chain-ca-constraints-design.md`（`cert_time_valid` の一文だけ）。
  **`lib.rs` も禁止** → 新モジュールはファイルを増やさず、担当ファイル内の inline `mod` にする（`asn1::oids`、`kvs::fs_util`）。
- 別レーン（ctrl-proto）の `pub` 関数は **呼ぶだけ**: `case::eph_pub_bytes` / `case::random_p256_secret` / `tlv::skip_container` / `tlv::copy_value` / `crypto::{sign,verify}_ecdsa_p256`。
- 挙動変更 0。既存テストが pin する文言・バイト列・順序は保つ。テストの削除は「同じ性質を別テストが既に固定している」場合のみ。
- 各タスク後: `cargo test -p mat-controller` 緑 → `git add <触ったファイル>` → commit（メッセージは `refactor(<area>): 日本語要約` 形式、末尾に `Co-Authored-By: Claude Fable 5.1 <noreply@anthropic.com>` と `Claude-Session: https://claude.ai/code/session_0191SgCMTJCSAB3rWmfFkFms`）。
- 最後に `task check`（fmt:check + clippy + doc:check + test）緑。`doc:check` は `RUSTDOCFLAGS=-D warnings` なので壊れた intra-doc link は失敗になる。
- 並行 5 セッションが cargo を回すので遅い。待つ（timeout は 600000 ms を指定）。
- 数値報告の grep は `/usr/bin/grep` をフルパスで（rtk プロキシが件数を汚す）。

---

### Task 1: Tier 1 死コード削除（kvs / fabric / cert）+ `ALPHA_INI_FILE`

**Files:**
- Modify: `crates/mat-controller/src/kvs.rs`
- Modify: `crates/mat-controller/src/fabric.rs`
- Modify: `crates/mat-controller/src/cert.rs`
- Modify: `crates/mat-controller/src/group_settings.rs:420`
- Modify: `crates/mat-controller/src/commissioning/commissioning_fabric.rs:140-141, 213-215, 224, 246, 262`
- Modify: `docs/superpowers/specs/2026-08-09-noc-chain-ca-constraints-design.md:45`

**Interfaces:**
- Produces: `pub const kvs::ALPHA_INI_FILE: &str = "chip_tool_config.alpha.ini"`（native-core レーンが使う）。
- Removes: `kvs::{RawFabricCredentials, read_fabric_credentials, parse_opkey, next_opkey_el, KvsError::BadOpKey}`、`fabric::{FabricCredentials::from_raw, FabricError::{OpKeyMismatch, NocMissingIds}}`、`cert::cert_time_valid`。他クレートからの参照ゼロは確認済み（`/usr/bin/grep -rn "read_fabric_credentials\|RawFabricCredentials\|OpKeyMismatch\|NocMissingIds\|BadOpKey\|cert_time_valid" crates --include='*.rs'` が mat-controller 内のみ）。

- [ ] **Step 1: 現存確認**

Run:
```bash
cd /home/noguk/ghq/github.com/nogu3/mat-wt/ctrl-store
/usr/bin/grep -rn "read_fabric_credentials\|RawFabricCredentials\|OpKeyMismatch\|NocMissingIds\|BadOpKey\|cert_time_valid\|keyset_with_slot0" crates --include='*.rs' | /usr/bin/grep -v "crates/mat-controller/src/"
```
Expected: 出力なし（他クレート参照ゼロ）。

- [ ] **Step 2: kvs.rs — 定数追加・死コード削除・doc 修正**

モジュール doc（1〜11 行）を次に置き換える:
```rust
//! Minimal reader for chip-tool's Linux ini KVS (connectedhomeip v1.4.2.0).
//!
//! Readers: [`read_self_issue_materials`] (what self-issuing our own NOC
//! needs: the root CA key from the alpha ini; the root cert, our node/fabric
//! id, and the IPK from the main ini fabric table), the `mat fabric list`
//! helpers ([`list_fabric_indices`] / [`read_noc_identity`] /
//! [`read_rcac_pubkey`]), the group-send credentials
//! ([`read_group_credentials`]) and the persisted counters / mat-only epoch
//! keys. Format facts (verified against SDK v1.4.2.0): `[Default]` section,
//! base64 values; the keyset stores the already derived *operational* group
//! key, not the epoch key.
```
`MAIN_INI_FILE` の直後に追加:
```rust
/// chip-tool 互換 alpha KVS（CA 鍵ペア `ExampleOpCredsCAKey<n>` を持つ）の
/// ファイル名（store ルート直下）。
pub const ALPHA_INI_FILE: &str = "chip_tool_config.alpha.ini";
```
削除: `RawFabricCredentials` struct + `impl Debug`（22〜49 行）、`KvsError::BadOpKey` 変種と `Display` の対応アーム、`next_opkey_el` / `parse_opkey`（186〜251 行）、`read_fabric_credentials`（410〜439 行）。`use crate::tlv::{Element, Reader, Tag, Value}` は `next_keyset_el` が `Element` を使うのでそのまま。

doc 修正:
- `parse_key_struct` の doc `([`parse_keyset`]/[`read_fabric_credentials`], used by M4 CASE)` → `([`parse_keyset`]/[`read_self_issue_materials`], used by M4 CASE)`。
- `SelfIssueMaterials` / `GroupCredentials` の Debug doc `See `RawFabricCredentials`'s `Debug` impl for the same rationale.` → `See `crate::fabric::FabricCredentials`'s `Debug` impl for the same rationale.`

テスト（`mod tests`）:
- 削除: `opkey_blob`、`const PUB`、`const PRIV`、`reads_all_five_items`、`missing_icac_is_none_but_missing_noc_is_error`（NOC 欠落は `missing_noc_is_key_missing` が既に固定）、`rejects_bad_opkey_version_and_bad_base64`。
- 追加ヘルパ（`noc_fixture` の直後）:
```rust
    /// `read_self_issue_materials` 用の最小フィクスチャ（alpha: root 鍵 97B、
    /// main: rcac / node01_01 NOC / 指定 keyset blob）。戻りは (alpha, main)。
    fn self_issue_fixture(tag: &str, ks: &[u8]) -> (std::path::PathBuf, std::path::PathBuf) {
        let mut root_key = vec![0xAA; 65];
        root_key.extend_from_slice(&[0xBB; 32]);
        let (noc, _, _) = noc_fixture();
        let alpha = write_named_ini(
            &format!("{tag}-alpha"),
            &[("ExampleOpCredsCAKey0", &root_key)],
        );
        let main = write_named_ini(
            &format!("{tag}-main"),
            &[("f/1/r", b"rcac-tlv-bytes"), ("f/1/n", noc), ("f/1/k/0", ks)],
        );
        (alpha, main)
    }
```
- `lookup_skips_lines_without_equals_sign` を置換:
```rust
    #[test]
    fn lookup_skips_lines_without_equals_sign() {
        let (alpha, main) = self_issue_fixture("noeq", &keyset_blob(&IPK));
        // [Default] 直後に空行と '=' の無いコメント風の行を差し込む（実 chip-tool
        // ini の癖）。セクション走査が中断してはいけない。
        let text = std::fs::read_to_string(&main).unwrap();
        let text = text.replacen(
            "[Default]\n",
            "[Default]\n\n; a comment without an equals sign\n",
            1,
        );
        std::fs::write(&main, text).unwrap();
        let m = read_self_issue_materials(&alpha, &main, 1, 0).unwrap();
        assert_eq!(m.rcac, b"rcac-tlv-bytes");
        assert_eq!(m.ipk_operational, IPK);
        std::fs::remove_file(alpha).ok();
        std::fs::remove_file(main).ok();
    }
```
- 新規（bad base64 の半分を継承）:
```rust
    #[test]
    fn rejects_bad_base64_naming_the_key() {
        let path = std::env::temp_dir().join(format!("mat-kvs-badb64-{}.ini", std::process::id()));
        std::fs::write(&path, "[Default]\nf/1/r = !!notbase64!!\n").unwrap();
        assert!(matches!(
            read_rcac_pubkey(&path, 1).unwrap_err(),
            KvsError::BadBase64(k) if k == "f/1/r"
        ));
        std::fs::remove_file(path).ok();
    }
```
- `rejects_keyset_with_zero_keys_count` を置換:
```rust
    #[test]
    fn rejects_keyset_with_zero_keys_count() {
        let (alpha, main) = self_issue_fixture("zero", &keyset_blob_with_count(&IPK, 0));
        let err = read_self_issue_materials(&alpha, &main, 1, 0).unwrap_err();
        assert!(matches!(
            err,
            KvsError::BadKeyset {
                fabric_index: 1,
                reason: "keys_count must be >= 1"
            }
        ));
        assert!(
            err.to_string().contains("f/1/k/0"),
            "error message should name the failing key: {err}"
        );
        std::fs::remove_file(alpha).ok();
        std::fs::remove_file(main).ok();
    }
```
- `keyset_without_hash_tolerated_by_ipk_path_but_rejected_by_group_path` の前半（`// IPK 読み出し` 〜 `remove_file(&path)`）を置換:
```rust
        // IPK 読み出し（read_self_issue_materials 経由）: hash 無しでも成功。
        let (alpha, main) = self_issue_fixture("nohash", &ks_no_hash);
        let m = read_self_issue_materials(&alpha, &main, 1, 0).unwrap();
        assert_eq!(m.ipk_operational, GROUP_KEY);
        std::fs::remove_file(alpha).ok();
        std::fs::remove_file(main).ok();
```
（コメント `// IPK 読み出し（read_fabric_credentials 経由）` も上のとおり直す。）

- [ ] **Step 3: fabric.rs — `from_raw` / `OpKeyMismatch` / `NocMissingIds` 削除**

- `FabricError` doc: `/// `FabricCredentials::from_raw` / `from_self_issued` error.` → `/// `FabricCredentials::from_self_issued` error.`
- 変種 `NocMissingIds`（doc 含む）と `OpKeyMismatch` を削除。`Display` の 2 アームを削除。`source` の最後のアームを `FabricError::GenKey => None,` に。
- `impl FabricCredentials` の `from_raw`（doc 含む 177〜210 行）を削除。
- tests: `builds_credentials_from_fixture_chain`、`rejects_opkey_not_matching_noc` を削除（3 段チェーン検証は `cert::tests::verifies_signatures_and_chain` が固定）。

- [ ] **Step 4: cert.rs — `cert_time_valid` 削除 + 設計 doc の一文**

`cert_time_valid`（doc 含む 722〜730 行）と test `cert_time_valid_checks_notbefore_notafter_window` を削除。
`docs/superpowers/specs/2026-08-09-noc-chain-ca-constraints-design.md` の 45 行目を
```
- 有効期間 — `verify_noc_chain` では検証しない意図的設計（監査でも棄却済み論点。旧 `cert_time_valid` は呼び手ゼロのため 2026-09-12 のリファクタで削除）。
```
に置換。

- [ ] **Step 5: `keyset_with_slot0` を private に、`ALPHA_INI_FILE` を commissioning_fabric で使う**

`group_settings.rs:420` の `pub(crate) fn keyset_with_slot0(` → `fn keyset_with_slot0(`。
`commissioning_fabric.rs`:
```rust
        let alpha_path = store.join(crate::kvs::ALPHA_INI_FILE);
        let main_path = store.join(crate::kvs::MAIN_INI_FILE);
```
tests 内の `"chip_tool_config.alpha.ini"` → `crate::kvs::ALPHA_INI_FILE`、`"chip_tool_config.ini"`（3 箇所）→ `crate::kvs::MAIN_INI_FILE`。

- [ ] **Step 6: テスト**

Run: `cd /home/noguk/ghq/github.com/nogu3/mat-wt/ctrl-store && cargo test -p mat-controller 2>&1 | tail -30`
Expected: 全緑（`kvs::tests::*`、`fabric::tests::*`、`cert::tests::*`、`commissioning::commissioning_fabric::tests::*` を含む）。warning（unused import 等）が出たら消す。

- [ ] **Step 7: Commit**

```bash
git add crates/mat-controller/src/kvs.rs crates/mat-controller/src/fabric.rs crates/mat-controller/src/cert.rs crates/mat-controller/src/group_settings.rs crates/mat-controller/src/commissioning/commissioning_fabric.rs docs/superpowers/specs/2026-08-09-noc-chain-ca-constraints-design.md docs/superpowers/plans/2026-09-12-refactor-ctrl-store.md
git commit -m "refactor(kvs,fabric,cert): 呼び手ゼロの read_fabric_credentials 一式 / from_raw / cert_time_valid を削除、ALPHA_INI_FILE 追加、keyset_with_slot0 を private に"
```

---

### Task 2: `eph_pub_bytes` / `random_serial` / `verify_ecdsa_p256` への委譲

**Files:**
- Modify: `crates/mat-controller/src/cert.rs:9-12, 136-168, 487-494, 1184-1191, 1268-1281, 1300-1312`
- Modify: `crates/mat-controller/src/fabric.rs:123-168, 212-231`
- Modify: `crates/mat-controller/src/x509.rs:671-684, 734-741`
- Modify: `crates/mat-controller/src/commissioning/commissioning_fabric.rs:100-121, 268-287`

**Interfaces:**
- Produces: `pub fn cert::random_serial() -> [u8; 8]`（BER INTEGER 最小正表現、`serial[0] &= 0x7F`）。
- Consumes: `crate::case::eph_pub_bytes(&p256::SecretKey) -> [u8; 65]`、`crate::crypto::verify_ecdsa_p256(&[u8;65], &[u8], &[u8;64]) -> Result<(), CryptoError>`（`CryptoError::{BadKey, BadSignature}`）。
- Removes: `FabricError::GenKey`（`eph_pub_bytes` は不可失敗なので不要）。

- [ ] **Step 1: cert.rs**

`generate_rcac` の鍵生成を置換:
```rust
pub fn generate_rcac() -> Result<(MatterCert, [u8; 32]), CertError> {
    let sk = crate::case::random_p256_secret();
    let private_key: [u8; 32] = sk.to_bytes().into();
    let public_key = crate::case::eph_pub_bytes(&sk);
```
（`use p256::elliptic_curve::sec1::ToSec1Point;` と `.map_err(|_| CertError::Malformed("pubkey encode"))?` を消す。）
`generate_rcac` 内の serial 3 行を `let serial = random_serial();` に置換し、`serial: serial.to_vec(),` はそのまま。`generate_rcac` の直前に追加:
```rust
/// 証明書 serial 用の乱数 8 バイト。先頭ビットを落として BER INTEGER の
/// 最小正表現（`asn1::integer` に先頭 0x00 を足させない）を保つ。
/// `generate_rcac` / `fabric::FabricCredentials::from_self_issued` /
/// `commissioning::CommissioningFabric::issue_device_noc` が共有する。
pub fn random_serial() -> [u8; 8] {
    let mut serial = [0u8; 8];
    getrandom::fill(&mut serial).expect("os rng");
    serial[0] &= 0x7F;
    serial
}
```
`verify_signed_by` を置換:
```rust
    /// Verify this certificate's signature was produced by `issuer_public_key`.
    pub fn verify_signed_by(&self, issuer_public_key: &[u8; 65]) -> Result<(), CertError> {
        let tbs = self.tbs_der()?;
        crate::crypto::verify_ecdsa_p256(issuer_public_key, &tbs, &self.signature).map_err(|e| {
            match e {
                crate::crypto::CryptoError::BadKey => CertError::BadPublicKey,
                _ => CertError::BadSignature,
            }
        })
    }
```
ファイル冒頭の `use p256::ecdsa::signature::Verifier;` と `use p256::ecdsa::{Signature, VerifyingKey};` を削除。

tests: `generate_rcac_is_self_signed_and_issues_valid_noc` の
```rust
        let op = crate::case::random_p256_secret();
        use p256::elliptic_curve::sec1::ToSec1Point;
        let op_pub: [u8; 65] = op.public_key().to_sec1_point(false).as_bytes().try_into().unwrap();
```
→ `let op_pub = crate::case::eph_pub_bytes(&crate::case::random_p256_secret());`。
`fresh_chain` の `op_pub` 生成（`use ToSec1Point` 含む）→ `let op_pub = crate::case::eph_pub_bytes(&op);`。
`rejects_forged_chain_with_noc_as_icac` の `x_pub` 生成（`use ToSec1Point` 含む）→ `let x_pub = crate::case::eph_pub_bytes(&x);`。

- [ ] **Step 2: fabric.rs**

`from_self_issued` の 1. を置換:
```rust
        // 1. new operational key pair.
        let sk = crate::case::random_p256_secret();
        let op_private_key: [u8; 32] = sk.to_bytes().into();
        let op_public_key = crate::case::eph_pub_bytes(&sk);
```
（関数頭の `use p256::elliptic_curve::sec1::ToSec1Point;` を消す。）2. の serial 3 行を `let serial = crate::cert::random_serial();` に。
`FabricError::GenKey`（doc・変種・Display アーム・source アーム）を削除。`source` は
```rust
        match self {
            FabricError::Cert(e) => Some(e),
            FabricError::SelfIssue(e) => Some(e),
        }
```

- [ ] **Step 3: x509.rs test_support**

`make_test_cert_ext` の `subject_pub` / `signer_pub` 生成（`use ToSec1Point` 含む 671〜684 行）を
```rust
        let subject_pub = crate::case::eph_pub_bytes(subject_key);
        let signer_pub = crate::case::eph_pub_bytes(signer_key);
```
に。`make_test_csr` の `pub_bytes` 生成（734〜741 行）を `let pub_bytes = crate::case::eph_pub_bytes(key);` に。

- [ ] **Step 4: commissioning_fabric.rs**

`issue_device_noc` の serial 生成（106〜111 行、`map_err(Malformed{..os rng failure})` 含む）を `let serial = cert::random_serial();` に（getrandom 失敗は他の全経路と同じく不可失敗扱い = `expect("os rng")`、これは既存の `cert::generate_rcac` と同じ規律）。
test `commissioning_fabric_issues_valid_credentials` の `dev_pub` 生成（`use ToSec1Point` 含む）→ `let dev_pub = crate::case::eph_pub_bytes(&dev);`。

- [ ] **Step 5: テスト + コミット**

Run: `cargo test -p mat-controller 2>&1 | tail -20` → 全緑。`/usr/bin/grep -rn "to_sec1_point" crates/mat-controller/src/{cert,fabric,x509}.rs crates/mat-controller/src/commissioning/` → 出力なし。

```bash
git add crates/mat-controller/src/cert.rs crates/mat-controller/src/fabric.rs crates/mat-controller/src/x509.rs crates/mat-controller/src/commissioning/commissioning_fabric.rs
git commit -m "refactor(cert): eph_pub_bytes / random_serial / crypto::verify_ecdsa_p256 へ委譲（手書き 8+3+1 箇所、FabricError::GenKey 不要化）"
```

---

### Task 3: `asn1::oids` に OID を一本化 + x509 ↔ attestation の DER 署名パーサ共有

**Files:**
- Modify: `crates/mat-controller/src/asn1.rs`
- Modify: `crates/mat-controller/src/cert.rs:187-216, 470-484, 836-857, 884-918, 946-962`
- Modify: `crates/mat-controller/src/x509.rs:14-25, 242-256, 371, 384-388, 399-428, 453-457, 593-596, 690-698, 745-752, 764-776, 795-821, 1072-1112, 1186-1227`
- Modify: `crates/mat-controller/src/attestation.rs:572-573, 587, 637-661, 1452-1461, 1472, 1528-1531`
- Modify: `crates/mat-controller/src/cd.rs:92-96, 161-182, 294, 315`

**Interfaces:**
- Produces: `pub mod asn1::oids`（内容バイトのみ、`asn1::oid()` でタグ付与）: `EC_PUBLIC_KEY`, `PRIME256V1`, `ECDSA_WITH_SHA256`, `SHA256`, `COMMON_NAME`, `MATTER_NODE_ID`, `MATTER_FIRMWARE_SIGNING_ID`, `MATTER_ICAC_ID`, `MATTER_RCAC_ID`, `MATTER_FABRIC_ID`, `MATTER_NOC_CAT`, `MATTER_VID`, `MATTER_PID`, `BASIC_CONSTRAINTS`, `KEY_USAGE`, `EXTENDED_KEY_USAGE`, `SUBJECT_KEY_ID`, `AUTHORITY_KEY_ID`, `ID_KP_PREFIX`, `PKCS7_DATA`, `PKCS7_SIGNED_DATA`, `CMS_MESSAGE_DIGEST`。
- Produces: `pub(crate) fn x509::parse_ecdsa_der_signature(der: &[u8]) -> Result<[u8; 64], X509Error>`、`pub(crate) fn x509::int_to_32(&[u8]) -> Result<[u8; 32], X509Error>`。
- Pin: `cert::tests::rebuilt_tbs_matches_der_vectors_byte_for_byte`、`x509::tests::parse_ecdsa_signature_roundtrips_and_rejects_malformed`、`cd::tests::cms_envelope_matches_what_chip_parses`。

- [ ] **Step 1: asn1.rs に `oids` を追加**（`ecdsa_signature` の直後、`#[cfg(test)]` の前）

```rust
/// このクレートが DER に書く／読む OID の内容バイト（タグ `0x06` と長さは
/// 含まない — [`oid`] が付与する）。cert.rs（Matter TLV → DER TBS 再構築）、
/// x509.rs（DAC/PAI/PAA・CSR 解析と test fixture 合成）、cd.rs / attestation.rs
/// （CMS SignedData）が共有する唯一の表。
pub mod oids {
    /// 1.2.840.10045.2.1 id-ecPublicKey
    pub const EC_PUBLIC_KEY: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];
    /// 1.2.840.10045.3.1.7 prime256v1
    pub const PRIME256V1: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];
    /// 1.2.840.10045.4.3.2 ecdsa-with-SHA256
    pub const ECDSA_WITH_SHA256: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02];
    /// 2.16.840.1.101.3.4.2.1 sha256
    pub const SHA256: &[u8] = &[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01];
    /// 2.5.4.3 commonName
    pub const COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];
    // Matter arc 1.3.6.1.4.1.37244.1.x -> 2B 06 01 04 01 82 A2 7C 01 xx
    /// 1.3.6.1.4.1.37244.1.1 matter-node-id
    pub const MATTER_NODE_ID: &[u8] = &[0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01, 0x01];
    /// 1.3.6.1.4.1.37244.1.2 matter-firmware-signing-id
    pub const MATTER_FIRMWARE_SIGNING_ID: &[u8] =
        &[0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01, 0x02];
    /// 1.3.6.1.4.1.37244.1.3 matter-icac-id
    pub const MATTER_ICAC_ID: &[u8] = &[0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01, 0x03];
    /// 1.3.6.1.4.1.37244.1.4 matter-rcac-id
    pub const MATTER_RCAC_ID: &[u8] = &[0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01, 0x04];
    /// 1.3.6.1.4.1.37244.1.5 matter-fabric-id
    pub const MATTER_FABRIC_ID: &[u8] = &[0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01, 0x05];
    /// 1.3.6.1.4.1.37244.1.6 matter-noc-cat
    pub const MATTER_NOC_CAT: &[u8] = &[0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x01, 0x06];
    /// 1.3.6.1.4.1.37244.2.1 matter-vid（DAC/PAI subject）
    pub const MATTER_VID: &[u8] = &[0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x02, 0x01];
    /// 1.3.6.1.4.1.37244.2.2 matter-pid（DAC/PAI subject）
    pub const MATTER_PID: &[u8] = &[0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x02, 0x02];
    /// 2.5.29.19 basicConstraints
    pub const BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1D, 0x13];
    /// 2.5.29.15 keyUsage
    pub const KEY_USAGE: &[u8] = &[0x55, 0x1D, 0x0F];
    /// 2.5.29.37 extKeyUsage
    pub const EXTENDED_KEY_USAGE: &[u8] = &[0x55, 0x1D, 0x25];
    /// 2.5.29.14 subjectKeyIdentifier
    pub const SUBJECT_KEY_ID: &[u8] = &[0x55, 0x1D, 0x0E];
    /// 2.5.29.35 authorityKeyIdentifier
    pub const AUTHORITY_KEY_ID: &[u8] = &[0x55, 0x1D, 0x23];
    /// 1.3.6.1.5.5.7.3 id-kp（末尾 1 バイトの purpose を足して EKU OID になる）
    pub const ID_KP_PREFIX: &[u8] = &[0x2B, 0x06, 0x01, 0x05, 0x05, 0x07, 0x03];
    /// 1.2.840.113549.1.7.1 pkcs7-data
    pub const PKCS7_DATA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x07, 0x01];
    /// 1.2.840.113549.1.7.2 pkcs7-signedData
    pub const PKCS7_SIGNED_DATA: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x07, 0x02];
    /// 1.2.840.113549.1.9.4 messageDigest（CMS signedAttrs）
    pub const CMS_MESSAGE_DIGEST: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x04];
}
```

- [ ] **Step 2: cert.rs — 事前符号化 OID 表（187〜216 行）を削除し `asn1::oid(oids::X)` で置換**

`use crate::asn1;` を `use crate::asn1::{self, oids};` に。置換箇所:
- `tbs_der`: `&asn1::seq(&[OID_EC_PUBLIC_KEY, OID_PRIME256V1])` → `&asn1::seq(&[&asn1::oid(oids::EC_PUBLIC_KEY), &asn1::oid(oids::PRIME256V1)])`；`&asn1::seq(&[OID_ECDSA_WITH_SHA256])` → `&asn1::seq(&[&asn1::oid(oids::ECDSA_WITH_SHA256)])`。
- `dn_name`: 各 `asn1::seq(&[OID_MATTER_NODE_ID, &hex16(*id)])` → `asn1::seq(&[&asn1::oid(oids::MATTER_NODE_ID), &hex16(*id)])`（FIRMWARE_SIGNING_ID / ICAC_ID / RCAC_ID / FABRIC_ID / NOC_CAT も同様）、`asn1::seq(&[OID_COMMON_NAME, &value])` → `asn1::seq(&[&asn1::oid(oids::COMMON_NAME), &value])`。
- `extension_der`: `OID_EXT_BASIC_CONSTRAINTS` → `&asn1::oid(oids::BASIC_CONSTRAINTS)`、`OID_EXT_KEY_USAGE` → `&asn1::oid(oids::KEY_USAGE)`、`OID_EXT_EXTENDED_KEY_USAGE` → `&asn1::oid(oids::EXTENDED_KEY_USAGE)`、`OID_EXT_SUBJECT_KEY_ID` → `&asn1::oid(oids::SUBJECT_KEY_ID)`、`OID_EXT_AUTHORITY_KEY_ID` → `&asn1::oid(oids::AUTHORITY_KEY_ID)`。
- `eku_oid` の末尾:
```rust
    let mut content = oids::ID_KP_PREFIX.to_vec();
    content.push(x);
    Ok(asn1::oid(&content))
```
（`Ok(vec![0x06, 0x08, ...])` を置換。）

- [ ] **Step 3: x509.rs — ローカル OID 表（14〜25 行）を削除、`oids` を使う**

`use crate::asn1;` → `use crate::asn1::{self, oids};`。置換: `OID_SKID`→`oids::SUBJECT_KEY_ID`、`OID_AKID`→`oids::AUTHORITY_KEY_ID`、`OID_BASIC_CONSTRAINTS`→`oids::BASIC_CONSTRAINTS`、`OID_KEY_USAGE`→`oids::KEY_USAGE`、`OID_ECDSA_SHA256`→`oids::ECDSA_WITH_SHA256`、`OID_EC_PUBLIC_KEY`→`oids::EC_PUBLIC_KEY`、`OID_PRIME256V1`→`oids::PRIME256V1`、`OID_MATTER_VID`→`oids::MATTER_VID`、`OID_MATTER_PID`→`oids::MATTER_PID`、`OID_CN`→`oids::COMMON_NAME`（本体・`test_support`・`tests` の全箇所。`test_support` の `use super::{asn1, OID_...}` は `use super::asn1; use crate::asn1::oids;` に、tests の `use super::*` はそのままで `oids::` を明示）。

`parse_ecdsa_signature` を分割:
```rust
/// 署名 BIT STRING（unused-bits byte + DER `SEQ { r INT, s INT }`）を
/// raw r||s（32B 左ゼロ詰め x2 = 64B）に正規化する。
fn parse_ecdsa_signature(bits: &[u8]) -> Result<[u8; 64], X509Error> {
    let (unused, seq_bytes) = bits
        .split_first()
        .ok_or(X509Error::Der("empty signature bit string"))?;
    if *unused != 0 {
        return Err(X509Error::Der("unexpected unused bits in signature"));
    }
    parse_ecdsa_der_signature(seq_bytes)
}

/// DER `SEQ { r INTEGER, s INTEGER }` を raw r‖s（64B）に正規化する。X.509 では
/// BIT STRING の中身、CMS SignerInfo では OCTET STRING の中身がこの形
/// （attestation.rs の CD 署名検証が共有する）。
pub(crate) fn parse_ecdsa_der_signature(der: &[u8]) -> Result<[u8; 64], X509Error> {
    let mut r = DerReader::new(der);
    let seq_content = r.expect(0x30)?;
    let mut inner = DerReader::new(seq_content);
    let r_bytes = inner.expect(0x02)?;
    let s_bytes = inner.expect(0x02)?;
    let mut out = [0u8; 64];
    out[..32].copy_from_slice(&int_to_32(r_bytes)?);
    out[32..].copy_from_slice(&int_to_32(s_bytes)?);
    Ok(out)
}
```
`int_to_32` を `pub(crate) fn int_to_32(...)` に。

- [ ] **Step 4: attestation.rs — 重複削除**

`der_ecdsa_sig_to_raw64` と `int_to_32`（637〜661 行）を削除。`verify_cd_signature_warn` の
```rust
    let Ok(raw_sig) = der_ecdsa_sig_to_raw64(&signer_info.signature) else {
```
→ `let Ok(raw_sig) = crate::x509::parse_ecdsa_der_signature(&signer_info.signature) else {`。
`const OID_CMS_MESSAGE_DIGEST`（572〜573 行）を削除し、`verify_message_digest_attr` の `if oid != OID_CMS_MESSAGE_DIGEST` → `if oid != crate::asn1::oids::CMS_MESSAGE_DIGEST`。
tests: `make_signer_info` の sha256 OID リテラル → `asn1::oid(oids::SHA256)`、ecdsa → `asn1::oid(oids::ECDSA_WITH_SHA256)`；`message_digest_attr` の OID → `asn1::oid(oids::CMS_MESSAGE_DIGEST)`（`use crate::asn1::{self, oids};` を tests 冒頭に）。`signer_info_rejects_missing_message_digest` の contentType OID リテラルはそのまま（oids に無い、追加不要）。

- [ ] **Step 5: cd.rs — ローカル OID 4 本（92〜96 行）を削除**

`use crate::asn1;` → `use crate::asn1::{self, oids};`。`cms_sign`: `OID_SHA256`→`oids::SHA256`、`OID_PKCS7_DATA`→`oids::PKCS7_DATA`、`OID_ECDSA_WITH_SHA256`→`oids::ECDSA_WITH_SHA256`、`OID_PKCS7_SIGNED_DATA`→`oids::PKCS7_SIGNED_DATA`。tests の `(0x06, OID_PKCS7_SIGNED_DATA)` → `(0x06, oids::PKCS7_SIGNED_DATA)`、`OID_PKCS7_DATA` 同様。

- [ ] **Step 6: テスト + コミット**

Run: `cargo test -p mat-controller 2>&1 | tail -20` → 全緑（特に `rebuilt_tbs_matches_der_vectors_byte_for_byte`、`to_tlv_roundtrips_all_fixtures`、`parse_ecdsa_signature_roundtrips_and_rejects_malformed`、`cms_envelope_matches_what_chip_parses`、`dev_attestation_chain_*`）。
`/usr/bin/grep -rn "const OID_" crates/mat-controller/src` → `asn1.rs` 以外に出ないこと。

```bash
git add crates/mat-controller/src/asn1.rs crates/mat-controller/src/cert.rs crates/mat-controller/src/x509.rs crates/mat-controller/src/attestation.rs crates/mat-controller/src/cd.rs
git commit -m "refactor(asn1): OID 表を asn1::oids に一本化（cert/x509/cd/attestation の 2 流儀を解消）、x509 の DER 署名パーサを attestation と共有"
```

---

### Task 4: `skip_container` 3 再実装 + `parse_elements` の自前 depth を `tlv::skip_container` へ委譲

**Files:**
- Modify: `crates/mat-controller/src/kvs.rs`（`skip_rest_of_container`）
- Modify: `crates/mat-controller/src/group_settings.rs:115-134`
- Modify: `crates/mat-controller/src/commissioning/tlv_fields.rs:38-50`
- Modify: `crates/mat-controller/src/attestation.rs:371-411`

**Interfaces:**
- Consumes: `crate::tlv::skip_container(&mut Reader) -> Result<(), TlvError>`（start 要素読了後・深さ 1 の状態から対応する `ContainerEnd` まで消費。`TlvError::Truncated` = 末尾切れ）。
- Pin: `kvs::tests::*`（`keyset_*`）、`group_settings::tests::keyset_with_next_preserves_unknown_tags_order_and_nested_slots`、`tlv_fields::tests::scan_skips_nested_containers_under_unknown_tags`（"truncated" 文言）、`attestation::tests::parse_elements_skips_nested_containers`。

- [ ] **Step 1: kvs.rs**

`skip_rest_of_container` を置換（doc は残し、末尾に一文追加）:
```rust
/// Skips the remainder of the container currently open at relative depth 0
/// (i.e. reads elements, tracking nested container depth, until the
/// `ContainerEnd` that matches the container we're inside of). Used both to
/// skip over unknown/uninteresting subtrees and to finish consuming a
/// container after we've already extracted what we needed from its start.
/// Delegates to [`crate::tlv::skip_container`], folding every failure into
/// `BadKeyset { "malformed tlv" }` like `next_keyset_el`.
fn skip_rest_of_container(r: &mut Reader, fabric_index: u8) -> Result<(), KvsError> {
    crate::tlv::skip_container(r).map_err(|_| KvsError::BadKeyset {
        fabric_index,
        reason: "malformed tlv",
    })
}
```

- [ ] **Step 2: group_settings.rs**

```rust
/// 未知タグを読み飛ばして、現在開いているコンテナ（相対深さ0）の
/// `ContainerEnd` まで消費する（[`crate::tlv::skip_container`] の `Option`
/// 版 — こちらの parser 群は `Option` チェーンで書かれている）。
fn skip_container(r: &mut Reader) -> Option<()> {
    crate::tlv::skip_container(r).ok()
}
```

- [ ] **Step 3: tlv_fields.rs**

```rust
/// 未知のタグに付随するコンテナを、対応する `ContainerEnd` まで読み飛ばす
/// （深さ 1 の状態、つまり start 要素は読み終わっている前提）。
/// [`crate::tlv::skip_container`] に委譲し、[`next_el`] と同じ文言へ写す。
pub(super) fn skip_container(r: &mut Reader, step: &'static str) -> Result<(), CommissionError> {
    crate::tlv::skip_container(r).map_err(|e| CommissionError::Malformed {
        step,
        detail: match e {
            crate::tlv::TlvError::Truncated => "truncated",
            _ => "tlv decode error",
        },
    })
}
```

- [ ] **Step 4: attestation.rs `parse_elements`**

ループを置換（`depth` 変数を廃止）:
```rust
    let mut cd: Option<Vec<u8>> = None;
    let mut nonce: Option<[u8; 32]> = None;
    loop {
        let el = r
            .next()
            .map_err(|_| AttestationError::Elements("tlv parse error"))?
            .ok_or(AttestationError::Elements("truncated elements"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                // vendor-reserved のネストは丸ごと読み飛ばす（中の Context(2) を
                // nonce と誤認しない）。
                crate::tlv::skip_container(&mut r).map_err(|e| match e {
                    crate::tlv::TlvError::Truncated => {
                        AttestationError::Elements("truncated elements")
                    }
                    _ => AttestationError::Elements("tlv parse error"),
                })?;
            }
            (Tag::Context(1), Value::Bytes(b)) => cd = Some(b.to_vec()),
            (Tag::Context(2), Value::Bytes(b)) => {
                nonce = Some(
                    b.try_into()
                        .map_err(|_| AttestationError::Elements("nonce wrong length"))?,
                );
            }
            _ => {} // timestamp / firmware_information は素通り
        }
    }
```

- [ ] **Step 5: テスト + コミット**

Run: `cargo test -p mat-controller 2>&1 | tail -20` → 全緑。

```bash
git add crates/mat-controller/src/kvs.rs crates/mat-controller/src/group_settings.rs crates/mat-controller/src/commissioning/tlv_fields.rs crates/mat-controller/src/attestation.rs
git commit -m "refactor(tlv): kvs / group_settings / commissioning / attestation の自前 skip_container を tlv::skip_container への委譲ラッパに"
```

---

### Task 5: kvs.rs の読み出し前置き共通化 + `fs_util`（flock + tmp/rename）を group.rs と共有

**Files:**
- Modify: `crates/mat-controller/src/kvs.rs`
- Modify: `crates/mat-controller/src/group.rs:48-127`

**Interfaces:**
- Produces（kvs.rs 内）: `fn with_default_section<T>(path: &Path, f: impl FnOnce(&str) -> Result<T, KvsError>) -> Result<T, KvsError>`、`fn must_b64(section: &str, key: &str) -> Result<Vec<u8>, KvsError>`（`KeyMissing(key)`）、`fn noc_identity_in(section: &str, fabric_index: u8) -> Result<(u64, u64), KvsError>`。
- Produces: `pub(crate) mod kvs::fs_util { pub(crate) fn take_lock(path: &Path) -> io::Result<File>; pub(crate) fn atomic_replace(path: &Path, bytes: &[u8]) -> io::Result<()>; }`。`take_lock` は sidecar `<path>.lock` を `NonBlockingLockExclusive` で flock し、競合は `io::ErrorKind::WouldBlock`。`atomic_replace` は `<path>.tmp`（拡張子置換ではなく **末尾に付加**）へ書き → `sync_all` → `rename`。
- Pin: `kvs::tests::kvs_txn_*`（byte-identical commit、Locked）、`group::tests::counter_*`（`WouldBlock` kind、persist-ahead）。

- [ ] **Step 1: kvs.rs 読み出しヘルパ**

`decode_b64` の直後に追加:
```rust
/// Like [`decode_b64`], but a missing/empty key is `KeyMissing(key)`.
fn must_b64(section: &str, key: &str) -> Result<Vec<u8>, KvsError> {
    decode_b64(section, key)?.ok_or_else(|| KvsError::KeyMissing(key.to_string()))
}

/// Reads `path` and hands its `[Default]` section body to `f`. Every reader
/// below starts this way; the ini is re-read per call on purpose (design
/// rule 4: no state between runs).
fn with_default_section<T>(
    path: &Path,
    f: impl FnOnce(&str) -> Result<T, KvsError>,
) -> Result<T, KvsError> {
    let text = std::fs::read_to_string(path).map_err(KvsError::Io)?;
    let section = default_section(&text).ok_or(KvsError::SectionMissing)?;
    f(section)
}

/// `f/<idx>/n`（fabric table の自 NOC、Matter-TLV）の subject から
/// `(node_id, fabric_id)` を読む。`read_self_issue_materials` と
/// `read_noc_identity` が共有する。
fn noc_identity_in(section: &str, fabric_index: u8) -> Result<(u64, u64), KvsError> {
    let noc_tlv = must_b64(section, &format!("f/{fabric_index}/n"))?;
    let noc = crate::cert::MatterCert::parse(&noc_tlv).map_err(|_| KvsError::BadNoc {
        fabric_index,
        reason: "unparseable matter-tlv certificate",
    })?;
    let node_id = noc.node_id().ok_or(KvsError::BadNoc {
        fabric_index,
        reason: "subject missing node id (tag 17)",
    })?;
    let fabric_id = noc.fabric_id().ok_or(KvsError::BadNoc {
        fabric_index,
        reason: "subject missing fabric id (tag 21)",
    })?;
    Ok((node_id, fabric_id))
}
```
各リーダを書き換え（doc コメントは維持）:
```rust
pub fn read_self_issue_materials(
    alpha_ini: &Path,
    main_ini: &Path,
    fabric_index: u8,
    issuer_index: u8,
) -> Result<SelfIssueMaterials, KvsError> {
    // --- alpha ini: root CA key pair ---
    let root_private_key = with_default_section(alpha_ini, |sec| {
        let ca_key = must_b64(sec, &format!("ExampleOpCredsCAKey{issuer_index}"))?;
        if ca_key.len() != 97 {
            return Err(KvsError::BadCaKey(
                "root ca key must be 97 raw bytes (pub65||priv32)",
            ));
        }
        // Only the private half is needed; the root public key is taken from the
        // parsed RCAC (single source of truth for `case_destination_id`).
        Ok(ca_key[65..].try_into().expect("32"))
    })?;

    // --- main ini: root cert (TLV), IPK, node id ---
    with_default_section(main_ini, |sec| {
        // （既存の rcac / ipk / node id に関する説明コメントはそのまま残す）
        let rcac = must_b64(sec, &format!("f/{fabric_index}/r"))?;
        let ipk_operational =
            parse_keyset(&must_b64(sec, &format!("f/{fabric_index}/k/0"))?, fabric_index)?;
        let (node_id, fabric_id) = noc_identity_in(sec, fabric_index)?;
        Ok(SelfIssueMaterials {
            rcac,
            root_private_key,
            ipk_operational,
            node_id,
            fabric_id,
        })
    })
}

pub fn list_fabric_indices(main_ini: &Path) -> Result<Vec<u8>, KvsError> {
    with_default_section(main_ini, |sec| {
        let mut out: Vec<u8> = sec
            .lines()
            .filter_map(|line| line.split_once('=').map(|(k, _)| k.trim()))
            .filter_map(|k| {
                k.strip_prefix("f/")
                    .and_then(|rest| rest.strip_suffix("/n"))
                    .and_then(|n| n.parse::<u8>().ok())
            })
            .collect();
        out.sort_unstable();
        out.dedup();
        Ok(out)
    })
}

pub fn read_noc_identity(main_ini: &Path, fabric_index: u8) -> Result<(u64, u64), KvsError> {
    with_default_section(main_ini, |sec| noc_identity_in(sec, fabric_index))
}

pub fn read_rcac_pubkey(main_ini: &Path, fabric_index: u8) -> Result<[u8; 65], KvsError> {
    with_default_section(main_ini, |sec| {
        let rcac = must_b64(sec, &format!("f/{fabric_index}/r"))?;
        let cert = crate::cert::MatterCert::parse(&rcac).map_err(|_| KvsError::BadNoc {
            fabric_index,
            reason: "unparseable rcac",
        })?;
        Ok(cert.pub_key)
    })
}
```
`read_group_credentials` / `read_group_data_counter` / `read_mat_ipk_epoch_slot` も `with_default_section(path, |section| { ...既存本体... })` に包む（本体は無変更。`read_group_credentials` の `decode_b64(section, &key)?.ok_or(KvsError::KeyMissing(key))?` は `must_b64(section, &key)?` に）。
`SelfIssueMaterials` の `root_private_key` 型が `[u8; 32]` なので closure の戻り型注釈が要る場合は `Ok::<[u8; 32], KvsError>(...)`。

- [ ] **Step 2: kvs.rs `fs_util` + KvsTxn の委譲**

`KvsTxn` struct の直前（`take_lock` の位置）に:
```rust
/// flock 排他 + tmp/rename 原子置換。`KvsTxn`（chip-tool INI）と
/// `group::PersistedGroupCounter`（group data counter）が同じ規律を共有する。
/// `lib.rs` は他レーンの担当なので独立ファイルにせず `kvs` 配下に置く。
pub(crate) mod fs_util {
    use std::io;
    use std::path::{Path, PathBuf};

    /// `path` の隣の sidecar `<path>.lock` を advisory flock（NonBlocking
    /// exclusive）する。本体は tmp+rename で置換されるので本体 fd への flock は
    /// rename 後に無効化される — 安定した別ファイルに取り、戻り値の `File` を
    /// 持っている間だけロックが生きる（Drop で OS が解放）。競合は
    /// `io::ErrorKind::WouldBlock`。
    pub(crate) fn take_lock(path: &Path) -> io::Result<std::fs::File> {
        use rustix::fs::{flock, FlockOperation};
        let mut lock_path = path.as_os_str().to_owned();
        lock_path.push(".lock");
        let lock = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(PathBuf::from(lock_path))?;
        flock(&lock, FlockOperation::NonBlockingLockExclusive).map_err(|e| {
            if e == rustix::io::Errno::WOULDBLOCK {
                io::Error::new(io::ErrorKind::WouldBlock, "locked by another process")
            } else {
                io::Error::other(e)
            }
        })?;
        Ok(lock)
    }

    /// `<path>.tmp`（ファイル名末尾に付加 — `with_extension` の stem 衝突
    /// （`a.ini` と `a.counter` が同じ `a.tmp` を取り合う）を避ける）へ書き、
    /// `sync_all` してから `rename` で置換する。クラッシュしても途中書きの
    /// 本体は残らない。
    pub(crate) fn atomic_replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
        use std::io::Write;
        let mut tmp = path.as_os_str().to_owned();
        tmp.push(".tmp");
        let tmp = PathBuf::from(tmp);
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }
}
```
既存 `fn take_lock(path) -> Result<File, KvsError>` を
```rust
/// sidecar `<path>.lock` を advisory flock する（[`fs_util::take_lock`]）。
/// `open` / `create` 共通。競合は `KvsError::Locked`。
fn take_lock(path: &Path) -> Result<std::fs::File, KvsError> {
    fs_util::take_lock(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            KvsError::Locked
        } else {
            KvsError::Io(e)
        }
    })
}
```
に。`KvsTxn::commit` を
```rust
    pub fn commit(self) -> Result<(), KvsError> {
        let mut body = self.lines.join("\n");
        if self.trailing_newline && !self.lines.is_empty() {
            body.push('\n');
        }
        fs_util::atomic_replace(&self.path, body.as_bytes()).map_err(KvsError::Io)
    }
```
に（tmp 名は `chip_tool_config.ini.tmp` のまま = 従来の `with_extension("ini.tmp")` と同一）。

- [ ] **Step 3: group.rs を委譲**

`PersistedGroupCounter::load` の flock 部分（`use rustix...` 〜 `})?;`）を
```rust
        let lock = crate::kvs::fs_util::take_lock(path).map_err(|e| {
            if e.kind() == io::ErrorKind::WouldBlock {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "group counter is locked by another process (matd running?)",
                )
            } else {
                e
            }
        })?;
```
に。`persist` を
```rust
    /// Atomic write (tmp + fsync + rename, [`crate::kvs::fs_util::atomic_replace`])
    /// so a crash never leaves a truncated value behind.
    fn persist(&mut self, ceiling: u32) -> io::Result<()> {
        crate::kvs::fs_util::atomic_replace(&self.path, format!("{ceiling}\n").as_bytes())?;
        self.ceiling = ceiling;
        Ok(())
    }
```
に（tmp 名は `native_group_counter.tmp` — counter path は拡張子無しなので従来と同一）。struct doc の「`<path>.lock` に取る」説明は維持。

- [ ] **Step 4: テスト + コミット**

Run: `cargo test -p mat-controller 2>&1 | tail -20` → 全緑（`kvs_txn_noop_commit_is_byte_identical`、`kvs_txn_second_open_would_block`、`counter_load_is_exclusive_across_handles`）。

```bash
git add crates/mat-controller/src/kvs.rs crates/mat-controller/src/group.rs
git commit -m "refactor(kvs): with_default_section / must_b64 / noc_identity_in で読み出し前置きを共通化、flock+tmp/rename を kvs::fs_util に集約して group counter と共有（tmp 名は末尾付加で stem 衝突を回避）"
```

---

### Task 6: dnssd 小物の共通化（`mdns_dest` / `addresses_for_target` / `ResolvedNode::from_parts`）

**Files:**
- Modify: `crates/mat-controller/src/dnssd/mod.rs`
- Modify: `crates/mat-controller/src/dnssd/codec.rs`
- Modify: `crates/mat-controller/src/dnssd/resolve.rs:6-15, 31-57, 85, 223-250, 299`
- Modify: `crates/mat-controller/src/dnssd/browse.rs:7-18, 185-211, 224`
- Modify: `crates/mat-controller/src/dnssd/cache.rs:7, 16-18, 242-251, 266`
- Modify: `crates/mat-controller/src/dnssd/test_util.rs:5-9, 111`

**Interfaces:**
- Produces（`dnssd/mod.rs`、子モジュールから `super::` で見える）: `fn mdns_dest(scope_id: u32) -> SocketAddr`、`impl ResolvedNode { pub(super) fn from_parts(port: u16, addresses: Vec<Ipv6Addr>, txt: &[Vec<u8>]) -> Self }`。
- Produces（`dnssd/codec.rs`）: `pub(super) fn addresses_for_target(aaaa: &[(String, Ipv6Addr)], target: &str) -> Vec<Ipv6Addr>`（target 一致 → 出現順 dedup → `sort_by_key(is_link_local)`）。
- **3 本の send/recv ループ（resolve_operational_many / resolve_commissionable / browse）本体は触らない。**

- [ ] **Step 1: mod.rs**

`bind_mdns_socket` の直後に:
```rust
/// mDNS 問い合わせ／広告の宛先 `[ff02::fb]:5353`。multicast 宛先では
/// `sin6_scope_id` が送出 iface を選ぶので `scope_id` を載せる。
fn mdns_dest(scope_id: u32) -> SocketAddr {
    SocketAddr::V6(SocketAddrV6::new(MDNS_GROUP, MDNS_PORT, 0, scope_id))
}
```
`impl ResolvedNode` の先頭に:
```rust
    /// SRV（port）+ target 一致 AAAA（`addresses`、呼び出し側でソート済み）+
    /// TXT から組む。`SII` / `SAI` は TXT に無ければ `None`（`mrp_config` が
    /// spec 既定へフォールバックする）。
    pub(super) fn from_parts(port: u16, addresses: Vec<Ipv6Addr>, txt: &[Vec<u8>]) -> Self {
        ResolvedNode {
            port,
            addresses,
            session_idle_interval_ms: codec::txt_u32(txt, "SII"),
            session_active_interval_ms: codec::txt_u32(txt, "SAI"),
        }
    }
```

- [ ] **Step 2: codec.rs**（`prune_aaaa` の直後）

```rust
/// SRV target に一致する AAAA を出現順に dedup して集め、非 link-local を
/// 先頭に並べる（stable sort なので同クラス内は応答順のまま）。
/// resolve（operational / commissionable）と browse の `finish` が共有する。
pub(super) fn addresses_for_target(aaaa: &[(String, Ipv6Addr)], target: &str) -> Vec<Ipv6Addr> {
    let mut addresses: Vec<Ipv6Addr> = Vec::new();
    for (name, addr) in aaaa {
        if name.eq_ignore_ascii_case(target) && !addresses.contains(addr) {
            addresses.push(*addr);
        }
    }
    addresses.sort_by_key(super::is_link_local);
    addresses
}
```

- [ ] **Step 3: resolve.rs**

import を `use super::codec::{addresses_for_target, encode_query, parse_message, prune_aaaa, push_aaaa, RData};` と `use super::{bind_mdns_socket, mdns_dest, operational_instance, DnssdError, ResolvedNode, QUERY_RESEND_INTERVAL, TYPE_AAAA, TYPE_PTR, TYPE_SRV, TYPE_TXT};`（`Ipv6Addr` は残す。`SocketAddr`/`SocketAddrV6`/`is_link_local`/`txt_u32`/`MDNS_GROUP`/`MDNS_PORT` は不要になる — `txt_u32` は `build_commissionable` の `D=` 判定で使うので残す）。
`try_finish`:
```rust
        let Some((port, target)) = &self.srv else {
            return;
        };
        let addresses = addresses_for_target(&self.aaaa, target);
        if addresses.is_empty() {
            return;
        }
        let strings = self.txt.as_deref().unwrap_or(&[]);
        self.resolved = Some(ResolvedNode::from_parts(*port, addresses, strings));
```
`build_commissionable`:
```rust
    if txt_u32(txt, "D") != Some(u32::from(long_discriminator)) {
        return None;
    }
    let addresses = addresses_for_target(aaaa, target);
    if addresses.is_empty() {
        return None;
    }
    Some(ResolvedNode::from_parts(port, addresses, txt))
```
`let dest = SocketAddr::V6(SocketAddrV6::new(MDNS_GROUP, MDNS_PORT, 0, scope_id));`（2 箇所）→ `let dest = mdns_dest(scope_id);`。

- [ ] **Step 4: browse.rs**

`finish`:
```rust
                let mut addresses: Vec<Ipv6Addr> = Vec::new();
                if let Some(t) = &target {
                    addresses = addresses_for_target(&pool, t);
                }
```
（`let mut addresses` → 上の形。）`dest` を `mdns_dest(scope_id)` に。import から `SocketAddr, SocketAddrV6, is_link_local, MDNS_GROUP, MDNS_PORT` を落とし `addresses_for_target`（codec）と `mdns_dest`（super）を足す。

- [ ] **Step 5: cache.rs**

`fold_operational_into_cache` 末尾:
```rust
        let addresses: Vec<Ipv6Addr> = entries.into_iter().map(|(a, _)| a).collect();
        let txt = fold.txt.get(&inst).map(Vec::as_slice).unwrap_or(&[]);
        let node = ResolvedNode::from_parts(*port, addresses, txt);
```
（`is_link_local` は `sort_by_key(|(a, seen)| (is_link_local(a), ...))` で引き続き使う。）`run_operational_cache` の `dest` → `mdns_dest(scope_id)`。import から `SocketAddr, SocketAddrV6, MDNS_GROUP, MDNS_PORT, txt_u32` を落とす（`txt_u32` は他で未使用になるはず — コンパイラ警告で確認）。

- [ ] **Step 6: test_util.rs**

`spawn_multicast_announcer` の `dest` → `super::mdns_dest(scope_id)`。import から `SocketAddr, SocketAddrV6, MDNS_GROUP, MDNS_PORT` を落とす。

- [ ] **Step 7: テスト + コミット**

Run: `cargo test -p mat-controller dnssd 2>&1 | tail -20` → 全緑（実 iface が要る `*_receives_multicast_only_*` / `*_demuxes_unicast_only_*` 含む）。`cargo clippy -p mat-controller --all-targets` に unused import が無いこと。

```bash
git add crates/mat-controller/src/dnssd/
git commit -m "refactor(dnssd): mdns_dest / addresses_for_target / ResolvedNode::from_parts を共通化（send/recv ループ本体は不変）"
```

---

### Task 7: commissioning の縫い目（`take_uint<T>` / `required()` / `#[cfg(doc)]` ハック解消 / `attestation_tbs`）

**Files:**
- Modify: `crates/mat-controller/src/commissioning/tlv_fields.rs:97-181`
- Modify: `crates/mat-controller/src/commissioning/codec.rs:7, 179-321`
- Modify: `crates/mat-controller/src/commissioning/device_codec.rs:13-27, 32-234`
- Modify: `crates/mat-controller/src/commissioning/flow.rs:232-239`

**Interfaces:**
- Produces（`tlv_fields`, `pub(super)`）: `fn take_uint<T: TryFrom<u64>>(map, tag, step, range_detail) -> Result<Option<T>, CommissionError>`（`take_u8/u16/u32` を置換。`take_u64` は範囲検査不要なのでそのまま）、`fn required<T>(value: Option<T>, step: &'static str, detail: &'static str) -> Result<T, CommissionError>`。
- Pin: `tlv_fields::tests::take_helpers_reject_out_of_range_uints`（"statusCode out of range" / "expiry out of range"）、`device_codec::tests::decode_open_commissioning_window_reports_each_missing_field`（"missing timeout" 等の文言）、`decoders_treat_wrong_field_type_as_missing`。

- [ ] **Step 1: tlv_fields.rs**

`take_u8` / `take_u16` / `take_u32` を削除し、代わりに:
```rust
/// `map` からタグ `tag` を整数 `T`（u8 / u16 / u32）として取り出す。タグが
/// 無い、または値が `Uint` 以外の型なら「無かった」扱いで `Ok(None)`（旧
/// 実装で型不一致の分岐が黙って読み捨てられていたのと同じ）。`Uint` では
/// あるが `T` に収まらない場合だけ `range_detail` で `Malformed` を返す。
pub(super) fn take_uint<T: TryFrom<u64>>(
    map: &mut BTreeMap<u8, FieldValue>,
    tag: u8,
    step: &'static str,
    range_detail: &'static str,
) -> Result<Option<T>, CommissionError> {
    match map.remove(&tag) {
        Some(FieldValue::Uint(v)) => Ok(Some(T::try_from(v).map_err(|_| {
            CommissionError::Malformed {
                step,
                detail: range_detail,
            }
        })?)),
        _ => Ok(None),
    }
}

/// 必須フィールドの欠落を `Malformed { step, detail }` にする（`detail` は
/// `"missing …"`）。各 decoder の `.ok_or(CommissionError::Malformed { .. })`
/// 25 箇所の置き換え。
pub(super) fn required<T>(
    value: Option<T>,
    step: &'static str,
    detail: &'static str,
) -> Result<T, CommissionError> {
    value.ok_or(CommissionError::Malformed { step, detail })
}
```
`take_bytes` / `take_utf8` の doc 内 `[`take_u8`]` → `[`take_uint`]`。

- [ ] **Step 2: codec.rs decoders**

import: `use super::tlv_fields::{required, scan_struct_fields, take_bytes, take_uint, take_utf8};`。書き換え例（残りも同じ形、文言はそのまま）:
```rust
pub fn decode_commissioning_status_response(
    fields: &[u8],
) -> Result<(u8, String), CommissionError> {
    let step = "commissioning_status_response";
    let mut map = scan_struct_fields(fields, step)?;
    let error_code = required(
        take_uint::<u8>(&mut map, 0, step, "errorCode out of range")?,
        step,
        "missing errorCode",
    )?;
    let debug_text = take_utf8(&mut map, 1).unwrap_or_default();
    Ok((error_code, debug_text))
}

pub fn decode_attestation_response(fields: &[u8]) -> Result<(Vec<u8>, [u8; 64]), CommissionError> {
    let step = "attestation_response";
    let mut map = scan_struct_fields(fields, step)?;
    let elements = required(take_bytes(&mut map, 0), step, "missing elements")?;
    let sig_bytes = required(take_bytes(&mut map, 1), step, "missing signature")?;
    let signature: [u8; 64] = sig_bytes
        .try_into()
        .map_err(|_| CommissionError::Malformed {
            step,
            detail: "signature length",
        })?;
    Ok((elements, signature))
}
```
`decode_noc_response` の `fabric_index` は `take_uint::<u8>(&mut map, 1, step, "fabricIndex out of range")?`（optional のまま）。

- [ ] **Step 3: device_codec.rs**

import: `use super::tlv_fields::{required, scan_struct_fields, take_bytes, take_u64, take_uint, take_utf8};`。`#[cfg(doc)] use super::{...};` ブロック（18〜27 行、コメント含む）を削除し、doc コメント内の intra-doc link を `super::` 付きに: `[`encode_arm_fail_safe`]` → `[`super::encode_arm_fail_safe`]`（`encode_set_regulatory_config` / `encode_attestation_request` / `encode_cert_chain_request` / `encode_csr_request` / `encode_add_trusted_root` / `encode_add_noc` / `encode_update_fabric_label` / `encode_remove_fabric` / `encode_open_commissioning_window` / `decode_commissioning_status_response` / `decode_attestation_response` / `decode_cert_chain_response` / `parse_nocsr_elements` / `decode_csr_response` / `decode_noc_response` の全部）。各 decoder は Step 2 と同形に（例）:
```rust
pub fn decode_open_commissioning_window(
    fields: &[u8],
) -> Result<OpenCommissioningWindowFields, CommissionError> {
    let step = "open_commissioning_window_request";
    let mut map = scan_struct_fields(fields, step)?;
    let timeout_s = required(
        take_uint::<u16>(&mut map, 0, step, "timeout out of range")?,
        step,
        "missing timeout",
    )?;
    let verifier = required(take_bytes(&mut map, 1), step, "missing verifier")?;
    let discriminator = required(
        take_uint::<u16>(&mut map, 2, step, "discriminator out of range")?,
        step,
        "missing discriminator",
    )?;
    let iterations = required(
        take_uint::<u32>(&mut map, 3, step, "iterations out of range")?,
        step,
        "missing iterations",
    )?;
    let salt = required(take_bytes(&mut map, 4), step, "missing salt")?;
    Ok((timeout_s, verifier, discriminator, iterations, salt))
}
```

- [ ] **Step 4: flow.rs**

```rust
    // NOCSR 署名も DAC 鍵で elements||challenge に対して（spec §11.17.5.6）。
    {
        let dac_cert = x509::parse_x509(&dac).map_err(|_| CommissionError::Csr("dac reparse"))?;
        let msg = attestation::attestation_tbs(&nocsr_elements, &challenge);
        crypto::verify_ecdsa_p256(&dac_cert.public_key, &msg, &nocsr_sig)
            .map_err(|_| CommissionError::Csr("nocsr signature"))?;
    }
```

- [ ] **Step 5: テスト + doc + コミット**

Run: `cargo test -p mat-controller commissioning 2>&1 | tail -20` → 全緑。
Run: `cd /home/noguk/ghq/github.com/nogu3/mat-wt/ctrl-store && RUSTDOCFLAGS="-D warnings" cargo doc -p mat-controller --no-deps 2>&1 | tail -5` → 警告 0（`super::` リンクが解決すること）。

```bash
git add crates/mat-controller/src/commissioning/
git commit -m "refactor(commissioning): take_uint<T> / required() で decoder の縫い目を畳み、cfg(doc) use ハックを super:: リンクに、NOCSR 署名対象を attestation_tbs に"
```

---

### Task 8: テスト基盤（attestation の `rejects_*` パラメータ化 + cert 鍵ロード）

**Files:**
- Modify: `crates/mat-controller/src/attestation.rs`（`mod tests`）
- Modify: `crates/mat-controller/src/cert.rs`（`mod tests`）

**Interfaces:**
- Produces（attestation tests）: `fn sign_with(dac_key: &p256::SecretKey, elements: &[u8], challenge: &[u8; 16]) -> [u8; 64]`、`fn verify_chain(dac: &[u8], pai: &[u8], paa: &[u8], dac_key: &p256::SecretKey) -> Result<(), AttestationError>`（nonce `[5;32]` / challenge `[6;16]` / elements `encode_attestation_elements(b"fake-cd", nonce, 0)` を固定して `verify_device_attestation` を呼ぶ）。
- Pin: 各 `rejects_*` の `AttestationError::Chain("…")` 文言はそのまま。

- [ ] **Step 1: attestation.rs tests**

`sign` を置換し `verify_chain` を追加:
```rust
    /// `elements ‖ challenge` に DAC 鍵で署名する。
    fn sign_with(dac_key: &p256::SecretKey, elements: &[u8], challenge: &[u8; 16]) -> [u8; 64] {
        let msg = attestation_tbs(elements, challenge);
        let priv_bytes: [u8; 32] = dac_key.to_bytes().into();
        sign_ecdsa_p256(&priv_bytes, &msg).unwrap()
    }

    fn sign(fix: &Fixture, elements: &[u8], challenge: &[u8; 16]) -> [u8; 64] {
        sign_with(&fix.dac_key, elements, challenge)
    }

    /// 固定 nonce / challenge / 偽 CD で `verify_device_attestation` を呼ぶ。
    /// `rejects_*` 系（チェーン制約の分岐だけを踏ませたいテスト）の共通足場。
    fn verify_chain(
        dac: &[u8],
        pai: &[u8],
        paa: &[u8],
        dac_key: &p256::SecretKey,
    ) -> Result<(), AttestationError> {
        let nonce = [5u8; 32];
        let challenge = [6u8; 16];
        let el = elements(&nonce);
        let sig = sign_with(dac_key, &el, &challenge);
        let paa_ders = [paa.to_vec()];
        verify_device_attestation(dac, pai, &paa_ders, &[], &el, &sig, &nonce, &challenge)
    }
```
次の 10 テストの「nonce/challenge/el/priv_bytes/msg/sig/verify_device_attestation(...)」ブロックを `verify_chain(&dac, &pai, &paa, &dac_key)` に置換（証明書の組み立てと説明コメント、`assert!(matches!(err, AttestationError::Chain("…")))` は不変）: `rejects_dac_pai_vid_mismatch`、`rejects_pai_without_ca_flag`、`rejects_dac_with_ca_flag`、`rejects_paa_without_ca_flag`、`rejects_pai_without_keycertsign`、`rejects_paa_without_keycertsign`、`rejects_dac_with_certsign_keyusage`、`accepts_dac_without_keyusage_extension`（`.unwrap()`）、`rejects_vid_scoped_paa_with_mismatched_pai_vid`、`rejects_dac_without_pid`。例:
```rust
    #[test]
    fn rejects_paa_without_ca_flag() {
        let paa_key = random_p256_secret();
        let pai_key = random_p256_secret();
        let dac_key = random_p256_secret();
        // PAA に is_ca=false（basicConstraints 拡張なし = cA は None）を
        // 付ける — spec 上 PAA は CA 証明書でなければならない。
        let paa = make_test_cert(b"paa", b"paa", &paa_key, &paa_key, false, None);
        let pai = make_test_cert(b"pai", b"paa", &pai_key, &paa_key, true, Some((0xFFF1, 0x8001)));
        let dac = make_test_cert(b"dac", b"pai", &dac_key, &pai_key, false, Some((0xFFF1, 0x8001)));
        let err = verify_chain(&dac, &pai, &paa, &dac_key).unwrap_err();
        assert!(matches!(
            err,
            AttestationError::Chain("paa is not a ca certificate")
        ));
    }
```
`accepts_valid_attestation` / `rejects_unknown_paa` / `rejects_wrong_nonce` / `rejects_tampered_signature` / `dev_generated_chain_passes_verify_device_attestation` は nonce 不一致・署名改竄・別 PAA を個別に扱うのでそのまま（`sign` 経由）。

- [ ] **Step 2: cert.rs tests — `root_and_op_keys()` を上へ移し ×2 で使う**

`root_and_op_keys`（CAT セクションの直前にある）を `der_len` の直後へ移動し、`issue_noc_produces_chain_valid_cert` の冒頭 3 ロード（`root` / `root_priv` / `op_pub`）を `let (root, root_priv, op_pub) = root_and_op_keys();` に、`rejects_icac_constraint_violations` の `root` / `root_priv` ロードと末尾の `op_pub` ロードを `let (root, root_priv, op_pub) = root_and_op_keys();`（`ica` / `node` の parse はそのまま）に置換。

- [ ] **Step 3: テスト + コミット**

Run: `cargo test -p mat-controller attestation 2>&1 | tail -30 && cargo test -p mat-controller cert:: 2>&1 | tail -20` → 全緑（テスト数は削減前と同じ: attestation 22、cert の数も不変）。

```bash
git add crates/mat-controller/src/attestation.rs crates/mat-controller/src/cert.rs
git commit -m "test(attestation,cert): rejects_* を verify_chain / sign_with でパラメータ化、cert の鍵ロードを root_and_op_keys に集約"
```

---

### Task 9: テスト基盤（kvs の keyset blob builder ×3 + `temp_dir()` → tempfile）

**Files:**
- Modify: `crates/mat-controller/src/kvs.rs`（`mod tests` のみ）

**Interfaces:**
- Produces（tests）: `fn keyset_blob_ext(key: &[u8; 16], keys_count: u64, hash: Option<u16>) -> Vec<u8>`；`keyset_blob` / `keyset_blob_with_count` / `keyset_blob_with_hash` / `keyset_blob_no_hash` はこれの薄い別名。`fn write_ini(entries) -> (tempfile::TempDir, PathBuf)`、`fn write_named_ini(tag, entries) -> (tempfile::TempDir, PathBuf)`、Task 1 の `self_issue_fixture` は `(TempDir, PathBuf, PathBuf)`（alpha, main）。

- [ ] **Step 1: builder 統合**

```rust
    /// chip-tool `KeySetData` 互換の 3 スロット blob。slot 0 に `key`、
    /// `hash` は `Some` なら slot 0 の ctx5 に書き `None` なら ctx5 を丸ごと
    /// 省く（実機で観測された「hash 無し」形）。他スロットは 0 / 0 / ゼロ鍵。
    fn keyset_blob_ext(key: &[u8; 16], keys_count: u64, hash: Option<u16>) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(1), 0); // policy
        w.put_uint(Tag::Context(2), keys_count);
        w.start_array(Tag::Context(3));
        for i in 0..3u8 {
            w.start_struct(Tag::Anonymous);
            w.put_uint(Tag::Context(4), 0); // start_time
            if i == 0 {
                if let Some(h) = hash {
                    w.put_uint(Tag::Context(5), u64::from(h));
                }
            } else {
                w.put_uint(Tag::Context(5), 0);
            }
            w.put_bytes(Tag::Context(6), if i == 0 { key } else { &[0u8; 16] });
            w.end_container();
        }
        w.end_container();
        w.put_uint(Tag::Context(7), 0xFFFF); // next keyset id（リンクリスト、読み側は無視）
        w.end_container();
        w.finish()
    }

    fn keyset_blob(key: &[u8; 16]) -> Vec<u8> {
        keyset_blob_ext(key, 1, Some(0x1234))
    }

    fn keyset_blob_with_count(key: &[u8; 16], keys_count: u64) -> Vec<u8> {
        keyset_blob_ext(key, keys_count, Some(0x1234))
    }

    fn keyset_blob_with_hash(key: &[u8; 16], hash: u16) -> Vec<u8> {
        keyset_blob_ext(key, 1, Some(hash))
    }

    fn keyset_blob_no_hash(key: &[u8; 16]) -> Vec<u8> {
        keyset_blob_ext(key, 1, None)
    }
```
（旧 `keyset_blob_with_count` / `keyset_blob_with_hash` / `keyset_blob_no_hash` の本体と doc を削除。読み側は先頭エントリの ctx5/ctx6 と keys_count しか見ないので、他スロットの start_time / next の差は無関係。）

- [ ] **Step 2: tempfile 化**

```rust
    fn write_ini(entries: &[(&str, &[u8])]) -> (tempfile::TempDir, std::path::PathBuf) {
        write_named_ini("kvs", entries)
    }

    /// `tag` は失敗メッセージで alpha / main を見分けるためのファイル名。
    fn write_named_ini(
        tag: &str,
        entries: &[(&str, &[u8])],
    ) -> (tempfile::TempDir, std::path::PathBuf) {
        let mut body = String::from("[Default]\n");
        for (k, v) in entries {
            body.push_str(&format!("{} = {}\n", k, Base64::encode_string(v)));
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(format!("{tag}.ini"));
        std::fs::write(&path, body).unwrap();
        (dir, path)
    }

    fn self_issue_fixture(
        tag: &str,
        ks: &[u8],
    ) -> (tempfile::TempDir, std::path::PathBuf, std::path::PathBuf) {
        let mut root_key = vec![0xAA; 65];
        root_key.extend_from_slice(&[0xBB; 32]);
        let (noc, _, _) = noc_fixture();
        let dir = tempfile::tempdir().unwrap();
        let alpha = dir.path().join(format!("{tag}-alpha.ini"));
        let main = dir.path().join(format!("{tag}-main.ini"));
        std::fs::write(&alpha, ini_body(&[("ExampleOpCredsCAKey0", &root_key)])).unwrap();
        std::fs::write(
            &main,
            ini_body(&[("f/1/r", b"rcac-tlv-bytes"), ("f/1/n", noc), ("f/1/k/0", ks)]),
        )
        .unwrap();
        (dir, alpha, main)
    }

    fn ini_body(entries: &[(&str, &[u8])]) -> String {
        let mut body = String::from("[Default]\n");
        for (k, v) in entries {
            body.push_str(&format!("{} = {}\n", k, Base64::encode_string(v)));
        }
        body
    }
```
（`write_named_ini` も `ini_body` を使う。）全呼び出し側を `let (_d, path) = write_ini(...)` / `let (_da, alpha) = write_named_ini(...)` / `let (_d, alpha, main) = self_issue_fixture(...)` に直し、`std::fs::remove_file(...)` 行を全て削除。`rejects_bad_base64_naming_the_key` の `std::env::temp_dir()` も `tempfile::tempdir()` に。同一テスト内で `write_named_ini` を 2 回呼ぶところは 2 つの `TempDir` を別名で保持する。`reads_self_issue_materials` / `ids_come_from_noc_subject_not_table_index` / `missing_noc_is_key_missing` / `garbage_noc_is_bad_noc_naming_the_key` は既存の inline 組み立てのまま `(TempDir, PathBuf)` タプルに追従させる（`self_issue_fixture` への寄せは任意）。

- [ ] **Step 3: 検証 + コミット**

Run: `cargo test -p mat-controller kvs:: 2>&1 | tail -20` → 全緑。`/usr/bin/grep -c "temp_dir\|remove_file" crates/mat-controller/src/kvs.rs` → `0`。

```bash
git add crates/mat-controller/src/kvs.rs
git commit -m "test(kvs): keyset blob builder を keyset_blob_ext に統合、temp_dir()+remove_file を tempfile::tempdir に（panic 時のゴミ残りを解消）"
```

---

### Task 10: テスト基盤（dnssd 合成 DNS の `MsgBuilder` + multicast iface スキャン 1 本化）

**Files:**
- Modify: `crates/mat-controller/src/dnssd/test_util.rs`
- Modify: `crates/mat-controller/src/dnssd/mod.rs:49-50`
- Modify: `crates/mat-controller/src/dnssd/browse.rs:313-427`
- Modify: `crates/mat-controller/src/dnssd/cache.rs:443-472`
- Modify: `crates/mat-controller/src/group.rs:597-649, 678, 758, 852, 938, 1057`

**Interfaces:**
- Produces（`dnssd::test_util`, `pub(crate)`）: `struct MsgBuilder` — `new()`（id 0、flags `0x8400` QR|AA、qd 0）、`ptr(name, instance)`（class IN）、`srv(name, port, target)`（cache-flush|IN、以後 `aaaa_ptr_srv_target` 用に target 名のオフセットを記憶）、`txt(name, &[&str])`（cache-flush|IN）、`aaaa(name, addr)`（cache-flush|IN）、`aaaa_class(name, ttl, addr, class)`、`aaaa_ptr_srv_target(addr)`（名前を直前の SRV target への圧縮ポインタで書く）、`finish() -> Vec<u8>`（an count を確定）。TTL は `aaaa_class` 以外 120 固定。
- Produces: `pub(crate) fn multicast_ifaces() -> Vec<(String, u32)>`（`operstate == "up"` を先に、各群は ifindex 昇順 — group.rs 版のソートを採用）。`dnssd/mod.rs` の `mod test_util;` を `pub(crate) mod test_util;` に。
- Pin: `codec::tests::parses_srv_txt_aaaa_with_compression`（AAAA 名が SRV target への圧縮ポインタで解決される）、`browse::tests::record_ttl_is_parsed`（TTL 120）、`cache::tests::parse_message_reads_cache_flush_bit` 等。

- [ ] **Step 1: test_util.rs に `MsgBuilder`**

```rust
/// 合成 mDNS 応答（QR|AA、id 0、question 無し）のビルダ。`synth_*` 各関数と
/// browse / cache の合成ヘルパはすべてこれで組む。
pub(crate) struct MsgBuilder {
    buf: Vec<u8>,
    an: u16,
    /// 直前の `srv` が書いた target 名のメッセージ内オフセット
    /// （`aaaa_ptr_srv_target` が圧縮ポインタで指す）。
    srv_target_off: Option<usize>,
}

impl MsgBuilder {
    pub(crate) fn new() -> Self {
        let mut buf = Vec::new();
        buf.extend_from_slice(&[0, 0, 0x84, 0x00]); // id 0, QR|AA
        buf.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]); // qd 0, an (後で埋める), ns/ar 0
        MsgBuilder {
            buf,
            an: 0,
            srv_target_off: None,
        }
    }

    fn header(&mut self, rtype: u16, class: u16, ttl: u32) {
        self.buf.extend_from_slice(&rtype.to_be_bytes());
        self.buf.extend_from_slice(&class.to_be_bytes());
        self.buf.extend_from_slice(&ttl.to_be_bytes());
        self.an += 1;
    }

    fn rdata(&mut self, rdata: &[u8]) {
        self.buf.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
        self.buf.extend_from_slice(rdata);
    }

    /// PTR（class IN — PTR は cache-flush を立てないのが通例）。
    pub(crate) fn ptr(mut self, name: &str, instance: &str) -> Self {
        push_name(&mut self.buf, name);
        self.header(TYPE_PTR, CLASS_IN, 120);
        let mut rdata = Vec::new();
        push_name(&mut rdata, instance);
        self.rdata(&rdata);
        self
    }

    /// SRV（cache-flush|IN）。target 名の位置を記憶する。
    pub(crate) fn srv(mut self, name: &str, port: u16, target: &str) -> Self {
        push_name(&mut self.buf, name);
        self.header(TYPE_SRV, FLUSH_IN, 120);
        let mut rdata = vec![0, 0, 0, 0]; // priority, weight
        rdata.extend_from_slice(&port.to_be_bytes());
        push_name(&mut rdata, target);
        self.srv_target_off = Some(self.buf.len() + 2 + 6); // rdlength(2) + prio/weight/port(6)
        self.rdata(&rdata);
        self
    }

    /// TXT（cache-flush|IN）。
    pub(crate) fn txt(mut self, name: &str, strings: &[&str]) -> Self {
        push_name(&mut self.buf, name);
        self.header(TYPE_TXT, FLUSH_IN, 120);
        let mut rdata = Vec::new();
        for s in strings {
            rdata.push(s.len() as u8);
            rdata.extend_from_slice(s.as_bytes());
        }
        self.rdata(&rdata);
        self
    }

    /// AAAA（cache-flush|IN、TTL 120）。
    pub(crate) fn aaaa(self, name: &str, addr: Ipv6Addr) -> Self {
        self.aaaa_class(name, 120, addr, FLUSH_IN)
    }

    /// class / TTL 指定の AAAA（cache-flush ビット検証用）。
    pub(crate) fn aaaa_class(mut self, name: &str, ttl: u32, addr: Ipv6Addr, class: u16) -> Self {
        push_name(&mut self.buf, name);
        self.header(TYPE_AAAA, class, ttl);
        self.rdata(&addr.octets());
        self
    }

    /// 名前を直前の `srv` の target への圧縮ポインタで書く AAAA（実 mDNS
    /// 応答の形 — `parse_message` の名前圧縮解決を踏ませる）。
    pub(crate) fn aaaa_ptr_srv_target(mut self, addr: Ipv6Addr) -> Self {
        let off = self.srv_target_off.expect("srv() must precede aaaa_ptr_srv_target()");
        self.buf.extend_from_slice(&[0xC0 | (off >> 8) as u8, (off & 0xFF) as u8]);
        self.header(TYPE_AAAA, FLUSH_IN, 120);
        self.rdata(&addr.octets());
        self
    }

    pub(crate) fn finish(mut self) -> Vec<u8> {
        self.buf[6..8].copy_from_slice(&self.an.to_be_bytes());
        self.buf
    }
}

const FLUSH_IN: u16 = 0x8000 | CLASS_IN;
```
（import に `CLASS_IN` を足す。）`synth_response` / `synth_commissionable_response` / `synth_aaaa_class` の本体を置換:
```rust
pub(super) fn synth_response(service: &str, target: &str, port: u16, txt: &[&str], addr: Ipv6Addr) -> Vec<u8> {
    MsgBuilder::new()
        .srv(service, port, target)
        .txt(service, txt)
        .aaaa_ptr_srv_target(addr)
        .finish()
}

pub(super) fn synth_commissionable_response(subtype: &str, instance: &str, target: &str, port: u16, txt: &[&str], addr: Ipv6Addr) -> Vec<u8> {
    MsgBuilder::new()
        .ptr(subtype, instance)
        .srv(instance, port, target)
        .txt(instance, txt)
        .aaaa_ptr_srv_target(addr)
        .finish()
}

pub(super) fn synth_aaaa_class(name: &str, ttl: u32, addr: Ipv6Addr, class: u16) -> Vec<u8> {
    MsgBuilder::new().aaaa_class(name, ttl, addr, class).finish()
}
```
`srv_target_off` の算出を旧 `let target_off = m.len() + 6;`（rdlength 書き込み後の値）と一致させること: 旧コードは rdlength を書いた**後**に `m.len() + 6` を取っているので、`self.rdata(&rdata)` の前に取る本ビルダでは `self.buf.len() + 2 + 6`。`parses_srv_txt_aaaa_with_compression` が緑なら正しい。

- [ ] **Step 2: browse.rs / cache.rs の合成ヘルパを `MsgBuilder` に**

browse.rs `synth_browse_response`:
```rust
    fn synth_browse_response(
        service: &str,
        instance: &str,
        with_srv: Option<(u16, &str)>,
        with_txt: Option<&[&str]>,
        with_aaaa: Option<(&str, Ipv6Addr)>,
    ) -> Vec<u8> {
        let mut b = MsgBuilder::new().ptr(service, instance);
        if let Some((port, target)) = with_srv {
            b = b.srv(instance, port, target);
        }
        if let Some(strings) = with_txt {
            b = b.txt(instance, strings);
        }
        if let Some((host, addr)) = with_aaaa {
            b = b.aaaa(host, addr);
        }
        b.finish()
    }
```
（`#[allow(clippy::too_many_arguments)]` と `push_name` / `CLASS_IN` / `TYPE_PTR` の import を落とし、`MsgBuilder` を import。SRV/TXT/AAAA のクラスが IN → cache-flush|IN に変わるが `BrowseFold` は `cache_flush` を読まず、TTL 120 は不変なので browse テストの pin に影響しない。）
cache.rs `synth_srv_txt_only`:
```rust
    fn synth_srv_txt_only(service: &str, target: &str, port: u16, txt: &[&str]) -> Vec<u8> {
        MsgBuilder::new().srv(service, port, target).txt(service, txt).finish()
    }
```
（`push_name` import を落とす。）

- [ ] **Step 3: multicast iface スキャンを 1 本に**

`dnssd/mod.rs`: `#[cfg(test)] mod test_util;` → `#[cfg(test)] pub(crate) mod test_util;`。
`test_util.rs` の `multicast_ifaces` を `pub(crate)` にし、末尾を
```rust
    up_first.sort_by_key(|(_, idx)| *idx);
    rest.sort_by_key(|(_, idx)| *idx);
    up_first.extend(rest);
    up_first
```
に（doc の「group.rs のテストと同じ実行時発見方式」→「group.rs のテストもこれを使う」）。
`group.rs` tests: `struct McastCandidate` と `multicast_capable_interfaces`（597〜649 行）を削除し、5 箇所の `for cand in multicast_capable_interfaces() {` を `for (name, index) in crate::dnssd::test_util::multicast_ifaces() {` に、本体の `cand.name` → `name`、`cand.index` → `index`（`cand.name.clone()` → `name.clone()`）に置換。`test_util.rs` は `#![cfg(test)]` なので group.rs の `#[cfg(test)] mod tests` からのみ参照される。

- [ ] **Step 4: テスト + コミット**

Run: `cargo test -p mat-controller dnssd 2>&1 | tail -20 && cargo test -p mat-controller group:: 2>&1 | tail -20` → 全緑。

```bash
git add crates/mat-controller/src/dnssd/ crates/mat-controller/src/group.rs
git commit -m "test(dnssd,group): 合成 DNS 応答を MsgBuilder に集約、multicast iface スキャンを dnssd::test_util::multicast_ifaces 1 本に"
```

---

### Task 11: バグ候補 — cd.rs テスト `der_split` の long-form 対応

**Files:**
- Modify: `crates/mat-controller/src/cd.rs:218-233, 286-350`

**Interfaces:**
- Produces（tests）: `fn der_split(buf: &[u8]) -> (u8, &[u8], &[u8])`（tag, content, **その要素の後ろの残り**）。呼び出し側の `&x[2 + y.len()..]`（short-form 2 バイトヘッダ前提の手計算）を全部 `rest` に置き換える。

- [ ] **Step 1: 書き換え**

```rust
    /// DER の 1 要素を `(tag, content, rest)` に割る（テスト用の最小パーサ）。
    /// short / long-form どちらの長さも読み、`rest` は要素の直後 — 呼び出し側が
    /// ヘッダ長を 2 バイト固定で足していた旧版は、CD が育って long-form に
    /// なると黙って誤解析した。
    fn der_split(buf: &[u8]) -> (u8, &[u8], &[u8]) {
        let tag = buf[0];
        let first = buf[1];
        let (len, off) = if first & 0x80 == 0 {
            (usize::from(first), 2)
        } else {
            let n = usize::from(first & 0x7F);
            let mut v = 0usize;
            for b in &buf[2..2 + n] {
                v = (v << 8) | usize::from(*b);
            }
            (v, 2 + n)
        };
        assert!(off + len <= buf.len(), "der element overruns buffer");
        (tag, &buf[off..off + len], &buf[off + len..])
    }
```
`cms_envelope_matches_what_chip_parses` を `rest` 駆動に:
```rust
        let (tag, outer, _) = der_split(&cms);
        assert_eq!(tag, 0x30);
        let (tag, oid, after_oid) = der_split(outer);
        assert_eq!((tag, oid), (0x06, oids::PKCS7_SIGNED_DATA));
        let (tag, explicit, _) = der_split(after_oid);
        assert_eq!(tag, 0xA0);
        let (tag, signed_data, _) = der_split(explicit);
        assert_eq!(tag, 0x30);

        let (tag, version, rest) = der_split(signed_data);
        assert_eq!((tag, version), (0x02, &[3u8][..]));
        let (tag, _digest_algs, rest) = der_split(rest);
        assert_eq!(tag, 0x31);
        let (tag, encap, rest) = der_split(rest);
        assert_eq!(tag, 0x30);
        let (tag, oid, after_oid) = der_split(encap);
        assert_eq!((tag, oid), (0x06, oids::PKCS7_DATA));
        let (tag, econtent_explicit, _) = der_split(after_oid);
        assert_eq!(tag, 0xA0);
        let (tag, econtent, _) = der_split(econtent_explicit);
        assert_eq!(tag, 0x04);
        assert_eq!(econtent, content, "eContent は CD の生バイト列そのもの");

        let (tag, signer_infos, _) = der_split(rest);
        assert_eq!(tag, 0x31);
        let (tag, si, _) = der_split(signer_infos);
        assert_eq!(tag, 0x30);
        let (_, _si_version, si_rest) = der_split(si);
        let (tag, key_id, si_rest) = der_split(si_rest);
        assert_eq!(tag, 0x80, "signer key id は [0] IMPLICIT OCTET STRING");
        assert_eq!(key_id, TEST_CD_SIGNING_KEY_ID);

        let (_, _digest_alg, si_rest) = der_split(si_rest);
        let (_, _sig_alg, si_rest) = der_split(si_rest);
        let (tag, sig_der, _) = der_split(si_rest);
        assert_eq!(tag, 0x04);
        let (tag, sig_seq, _) = der_split(sig_der);
        assert_eq!(tag, 0x30);
        let (_, r, after_r) = der_split(sig_seq);
        let (_, s, _) = der_split(after_r);
```
（raw r‖s の組み立てと `verify_ecdsa_p256` はそのまま。`oids` は Task 3 で import 済み。）

- [ ] **Step 2: 検証 + コミット**

Run: `cargo test -p mat-controller cd:: 2>&1 | tail -10` → 全緑。

```bash
git add crates/mat-controller/src/cd.rs
git commit -m "test(cd): der_split を (tag, content, rest) にして long-form 長でも CMS 封筒を正しく辿る"
```

---

### Task 12（任意・最後）: group_settings のフラット struct パーサ 5 本を `walk_flat_struct` に

**Files:**
- Modify: `crates/mat-controller/src/group_settings.rs:188-223, 247-273, 297-321, 361-379, 511-533`

**Interfaces:**
- Produces: `fn walk_flat_struct(blob: &[u8], on_field: impl FnMut(u8, Value<'_>)) -> Option<()>`（先頭が struct start でなければ `None`、直下の context-tag 付き leaf を `on_field(tag, value)` に渡し、ネストコンテナは `skip_container`、`ContainerEnd` で `Some(())`。非 context タグの leaf は無視 = 旧 `_ => {}`）。
- 条件: `cargo test -p mat-controller group_settings:: kvs::` の byte-equality テスト（`keyset_with_next_*` / `keyset_with_slot0_*` / `write_keyset_reprovision_*` / `ipk_rotation_*` / `existing_chiptool_like_store_*`）が全緑であること。**1 本でも赤ならこのタスクは revert して見送る**（DONE.md に理由を書く）。chain walker 3 本（`scan_map` / `scan_groups` / `scan_keysets`）は record ごとの key 形式と corrupt 文言が違い closure 化で読みにくくなるので **見送り**。

- [ ] **Step 1: ヘルパ追加**（`skip_container` の直後）

```rust
/// フラットな struct（直下に leaf だけ、未知のネストは読み飛ばす）を 1 段
/// 走査し、context-tag 付き leaf を `on_field(tag, value)` に渡す。先頭が
/// struct start でない / 途中で切れている / TLV 不正なら `None`。
/// `FabricData` / `GroupData` / `KeyMap` / `FabricList` / `keyset_next` の
/// 5 パーサが共有する骨格。
fn walk_flat_struct(blob: &[u8], mut on_field: impl FnMut(u8, Value<'_>)) -> Option<()> {
    let mut r = Reader::new(blob);
    if r.next().ok()??.value != Value::StructStart {
        return None;
    }
    loop {
        let el = r.next().ok()??;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => return Some(()),
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r)?
            }
            (Tag::Context(t), v) => on_field(t, v),
            _ => {}
        }
    }
}
```

- [ ] **Step 2: 5 パーサを書き換え**（例 — 他も同形）

```rust
pub(crate) fn parse_fabric_data(blob: &[u8]) -> Option<FabricData> {
    let (mut first_group, mut group_count) = (None, None);
    let (mut first_map, mut map_count) = (None, None);
    let (mut first_keyset, mut keyset_count) = (None, None);
    let mut next = None;
    walk_flat_struct(blob, |tag, v| {
        if let Value::Uint(v) = v {
            let v = u16::try_from(v).ok();
            match tag {
                1 => first_group = v,
                2 => group_count = v,
                3 => first_map = v,
                4 => map_count = v,
                5 => first_keyset = v,
                6 => keyset_count = v,
                7 => next = v,
                _ => {}
            }
        }
    })?;
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

fn parse_group_data(blob: &[u8]) -> Option<GroupData> {
    let (mut name, mut first_endpoint, mut endpoint_count, mut next) = (None, None, None, None);
    walk_flat_struct(blob, |tag, v| match (tag, v) {
        (1, Value::Utf8(s)) => name = Some(s.to_string()),
        (2, Value::Uint(v)) => first_endpoint = u16::try_from(v).ok(),
        (3, Value::Uint(v)) => endpoint_count = u16::try_from(v).ok(),
        (4, Value::Uint(v)) => next = u16::try_from(v).ok(),
        _ => {}
    })?;
    Some(GroupData {
        name: name?,
        first_endpoint: first_endpoint?,
        endpoint_count: endpoint_count?,
        next: next?,
    })
}

fn keyset_next(blob: &[u8]) -> Option<u16> {
    let mut next = None;
    walk_flat_struct(blob, |tag, v| {
        if let (7, Value::Uint(v)) = (tag, v) {
            next = u16::try_from(v).ok();
        }
    })?;
    next
}
```
`parse_keymap` / `parse_fabric_list` も同じ形（タグ→フィールド対応は既存どおり）。旧実装の「同タグ複数回は後勝ち」「型不一致は無視」「`u16` 超過は `None` 扱い」は closure 内の `u16::try_from(v).ok()` 代入でそのまま再現される。

- [ ] **Step 3: 検証 + コミット（赤なら `git checkout -- crates/mat-controller/src/group_settings.rs` で見送り）**

Run: `cargo test -p mat-controller group_settings:: 2>&1 | tail -20 && cargo test -p mat-controller kvs:: 2>&1 | tail -10` → 全緑。

```bash
git add crates/mat-controller/src/group_settings.rs
git commit -m "refactor(group_settings): フラット struct パーサ 5 本を walk_flat_struct に集約（chip-tool INI 互換テスト全緑）"
```

---

### Task 13: 最終検証 + DONE.md

**Files:**
- Create: `/home/noguk/ghq/github.com/nogu3/mat-wt/tasks/ctrl-store.DONE.md`

- [ ] **Step 1: `task check`**

Run: `cd /home/noguk/ghq/github.com/nogu3/mat-wt/ctrl-store && task check 2>&1 | tail -40`（timeout 600000）
Expected: fmt:check / clippy / doc:check / test 全部緑。赤なら直して該当タスクの commit に `fix:` で積む。

- [ ] **Step 2: 担当外ファイルに差分が無いことの確認**

Run: `git diff --stat main..HEAD | cat`
Expected: 変更ファイルは `crates/mat-controller/src/{kvs,group_settings,group,fabric,cert,x509,asn1,cd,attestation}.rs`、`crates/mat-controller/src/commissioning/*`、`crates/mat-controller/src/dnssd/*`、`docs/superpowers/plans/2026-09-12-refactor-ctrl-store.md`、`docs/superpowers/specs/2026-08-09-noc-chain-ca-constraints-design.md` のみ。

- [ ] **Step 3: DONE.md を書く**

内容（見出し固定）: `## やった項目`（タスク 1〜12 の要約と commit hash、行数差分 `git diff --shortstat main..HEAD`）/ `## 見送った項目と理由`（chain walker 3 本、`case_responder.rs:968` の random_serial は ctrl-proto レーン、`x509::test_support` の常時コンパイルは `generate_dev_attestation` が本番で使うため対象外、`lib.rs` 不可のため `fs_util` / `oids` は inline mod — 親セッションで `src/fs_util.rs` へ昇格可）/ `## task check 結果`（各サブタスクの結果行）/ `## 実機 E2E が必要か`（KVS 互換は byte-equality テスト・dnssd は実 iface テストで固定、ワイヤ／暗号出力は不変 → 実機スモーク推奨だが必須ではない。マージ後の通常スモーク（19/19 + groupcast off/on）で十分）/ `## 親セッションへの注意`（`kvs::ALPHA_INI_FILE` は native-core レーンが参照する前提、`FabricError::{GenKey,OpKeyMismatch,NocMissingIds}` 削除は他クレート参照ゼロを確認済み）。
