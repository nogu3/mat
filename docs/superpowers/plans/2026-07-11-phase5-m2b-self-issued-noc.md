# Phase 5 M2b: 自己発行 operational identity で CASE を通す 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** chip-tool KVS に永続化されている root CA 鍵で mat-controller が自前の operational 鍵＋NOC を自己発行し、ローカル chip-all-clusters-app に対して CASE 確立 → onoff toggle → on-off read が通る（`task e2e:m2`）。あわせて CASE 暗号コアのオフライン自己ハンドシェイクテストを入れる。

**Architecture:** M2 の CASE/IM スタック（完成済み）は無変更で流用。新規は「Matter TLV 証明書の生成（Task 4 パースの逆）＋ root 直署名での NOC 自己発行」。chip-tool の自 identity 操作鍵は KVS に無い（実測確認済）ため、KVS から root 証明書＋root 鍵＋IPK を読み、我々の鍵で NOC を出して `FabricCredentials` を組む。

**Tech Stack:** Rust, tokio, RustCrypto: `p256`(ECDSA/ECDH), `sha2`, `sha1`(新規, SKID 用), `hkdf`, `hmac`, `base64ct`。

**Spec:** `docs/superpowers/specs/2026-07-11-phase5-m2b-self-issued-noc-design.md`（承認済み）

## Global Constraints

- SDK フォーマットは connectedhomeip **v1.4.2.0 固定**。
- 暗号プリミティブは RustCrypto 既製のみ。自作しない。新依存は `sha1` のみ。
- `mat` / `matd` クレートは**無変更**。プロトコルコードは `mat-controller` のみ。
- repo は public: 実 fabric の資格情報をコミットしない。フィクスチャは connectedhomeip の**公開テスト証明書**のみ（`tests/fixtures/` に既存 + 本計画で root01/ica01 秘密鍵を追加）。
- CI (`task test`) はデバイス・実資格情報なしで全通過。ライブテストは `#[ignore]`。
- ブランチは `matter-controller`（main マージ禁止）。各コミット前に `task check`（fmt:check + clippy -D warnings + test）を通す。
- crate 内エラーは既存の各モジュール enum を踏襲（`Display + Error`、panic なし）。コミットメッセージ末尾に `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`。

## SDK/KVS 実測で確定した事実（`.superpowers/sdd/kvs-probe-result.txt`、再調査不要）

### chip-tool KVS レイアウト
- `chip_tool_config.alpha.ini`（ExampleOperationalCredentialsIssuer ストレージ、セクション `[Default]`、値 base64、キー生）:
  - `ExampleCARootCert0` = RCAC（Matter TLV 証明書）
  - `ExampleOpCredsCAKey0` = **root CA 鍵** = 生 `P256SerializedKeypair` = 公開鍵65 ‖ 秘密鍵32 = **97 バイト（TLV ラップ無し）**
  - （`ExampleCAIntermediateCert0` / `ExampleOpCredsICAKey0` は本計画では**読まない** — root 直署名を採るため）
- `chip_tool_config.ini`（fabric テーブル）:
  - `f/1/r`=RCAC, `f/1/i`=ICAC, `f/1/n`=NOC（前回起動分）, `f/1/k/0`=group keyset(IPK), `f/1/m`=metadata。**`f/1/o`（操作鍵）は存在しない**（spec 前提の誤りの実証）。
- サフィックス `<idx>` = **0**。fabric index = **1**（alpha）。
- `LocalNodeId` キーは**存在しない** → chip-tool は既定の `kTestControllerNodeId` = **112233**（0x1B669）を controller node id に使う。デバイス ACL の admin もこの node id。

### NOC テンプレート（`EncodeNOCSpecificExtensions`、SDK 実測）
NOC の extension は TLV 出現順で:
1. BasicConstraints: `is_ca=false`, path_len 無し（critical）
2. KeyUsage: `digitalSignature`（RFC5280 bit0） = TLV uint 値 **0x0001**（critical）
3. ExtendedKeyUsage: `[clientAuth, serverAuth]`（**この順**、critical）。EKU 値の Matter TLV 表現は `[2, 1]`（1=serverAuth, 2=clientAuth。Task 4 の cert.rs は EKU を `Vec<u64>` で保持、値は Matter enum: server=1, client=2, ...）
4. SubjectKeyId: `SHA1(op_pub_key 65B)` = 20 バイト
5. AuthorityKeyId: 発行者（root）の SubjectKeyId 拡張値（20 バイト）

validity: not_before = 2021-01-01T00:00:00Z（Matter epoch 662688000）、not_after = そこから 10 年（+ 315360000 秒 = Matter epoch 978048000）。serial = 任意の 1〜20 バイト BER INTEGER（例: 8 バイト乱数、先頭ビットを 0 に）。
DN: subject = list{ 17: node_id(uint), 21: fabric_id(uint) }、issuer = root 証明書の subject DN。

**注記（EKU 値の確認）:** cert.rs の `ExtendedKeyUsage(Vec<u64>)` は Matter TLV 上の enum 値を保持する。`node01_01` フィクスチャをパースして EKU の実値と順序を確認し、それに一致させること（Task 3 Step 1 で確認）。SDK は client, server の順で書く。

## File Structure

| ファイル | 変更 |
|---|---|
| `crates/mat-controller/Cargo.toml` | `sha1 = "0.10"` 追加（Task 2） |
| `crates/mat-controller/src/crypto.rs` | `sign_ecdsa_p256` / `verify_ecdsa_p256`（case.rs から昇格・pub 化、正式エラー型）（Task 1） |
| `crates/mat-controller/src/case.rs` | `sign_raw_ecdsa`/`verify_raw_ecdsa` を crypto の pub 関数呼び出しに置換（Task 1） |
| `crates/mat-controller/src/cert.rs` | `subject_key_id()`、`MatterCert::to_tlv()`（Task 2）、`issue_noc()`（Task 3）、`verify_noc_chain` に validity チェック（Task 5） |
| `crates/mat-controller/tests/fixtures/` | root01/ica01 秘密鍵フィクスチャ追加（Task 3） |
| `crates/mat-controller/src/kvs.rs` | `read_self_issue_materials()` + `SelfIssueMaterials`（Task 4） |
| `crates/mat-controller/src/fabric.rs` | `FabricCredentials::from_self_issued()`（Task 5）、`NocMissingIds` dead code 整理・`source()`（Task 5） |
| `crates/mat-controller/tests/case_self_handshake.rs` | オフライン CASE 自己ハンドシェイク（Task 6、必須） |
| `crates/mat-controller/tests/live_case_im.rs`（既存の未コミット版を置換） | self-issued 経路のライブ E2E（Task 7） |
| `scripts/e2e-m2.sh`（既存の未コミット版を置換）+ `Taskfile.yml` + `ARCHITECTURE.md` | ハーネス・docs（Task 7） |

---

### Task 1: `crypto` — 生 ECDSA(P-256) 署名/検証を pub 化

**Files:**
- Modify: `crates/mat-controller/src/crypto.rs`
- Modify: `crates/mat-controller/src/case.rs`

**Interfaces:**
- Produces:
  ```rust
  // crypto.rs
  pub fn sign_ecdsa_p256(private_key: &[u8; 32], message: &[u8]) -> Result<[u8; 64], CryptoError>;
  pub fn verify_ecdsa_p256(public_key: &[u8; 65], message: &[u8], signature: &[u8; 64]) -> Result<(), CryptoError>;
  // CryptoError に variant 追加: BadKey, BadSignature
  ```
- Consumes（case.rs 側）: 上記 2 関数（既存の `sign_raw_ecdsa`/`verify_raw_ecdsa` を置換）。

- [ ] **Step 1: 失敗するテストを書く**（crypto.rs の tests mod）

```rust
    #[test]
    fn ecdsa_sign_verify_roundtrip() {
        // 既知の p256 テスト鍵（RustCrypto でその場生成）
        use p256::ecdsa::SigningKey;
        let sk = SigningKey::from_slice(&[0x11u8; 32]).unwrap();
        let priv_bytes: [u8; 32] = sk.to_bytes().into();
        let vk = sk.verifying_key();
        let pub_bytes: [u8; 65] = vk
            .to_encoded_point(false)
            .as_bytes()
            .try_into()
            .unwrap();
        let msg = b"attestation over TBS bytes";
        let sig = sign_ecdsa_p256(&priv_bytes, msg).unwrap();
        verify_ecdsa_p256(&pub_bytes, msg, &sig).unwrap();
        // 改ざんメッセージは失敗
        assert!(verify_ecdsa_p256(&pub_bytes, b"other", &sig).is_err());
        // 不正鍵は BadKey
        assert!(matches!(
            verify_ecdsa_p256(&[0u8; 65], msg, &sig),
            Err(CryptoError::BadKey)
        ));
    }
```
（`use p256::elliptic_curve::sec1::ToEncodedPoint;` が必要な場合は追加。）

- [ ] **Step 2: 落ちることを確認** — `cargo test -p mat-controller -- crypto::tests::ecdsa` → コンパイルエラー

- [ ] **Step 3: 実装**（crypto.rs）

```rust
use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
use p256::elliptic_curve::sec1::ToEncodedPoint;

/// ECDSA-P256 sign over SHA-256(message) (p256 default). Returns raw r||s (64B).
pub fn sign_ecdsa_p256(private_key: &[u8; 32], message: &[u8]) -> Result<[u8; 64], CryptoError> {
    let key = SigningKey::from_slice(private_key).map_err(|_| CryptoError::BadKey)?;
    let sig: Signature = key.sign(message);
    Ok(sig.to_bytes().into())
}

/// Verify a raw r||s (64B) ECDSA-P256 signature over SHA-256(message).
pub fn verify_ecdsa_p256(
    public_key: &[u8; 65],
    message: &[u8],
    signature: &[u8; 64],
) -> Result<(), CryptoError> {
    let key = VerifyingKey::from_sec1_bytes(public_key).map_err(|_| CryptoError::BadKey)?;
    let sig = Signature::from_slice(signature).map_err(|_| CryptoError::BadSignature)?;
    key.verify(message, &sig).map_err(|_| CryptoError::BadSignature)
}
```
`CryptoError` に `BadKey` / `BadSignature` を追加し、Display 追記（`"invalid ec key"` / `"ecdsa signature verification failed"`）。

- [ ] **Step 4: case.rs を置換**

case.rs の private `sign_raw_ecdsa`/`verify_raw_ecdsa` を削除し、呼び出しを `crate::crypto::sign_ecdsa_p256` / `verify_ecdsa_p256` に置換。Sigma2 署名検証失敗は従来どおり `CaseError::Sigma2SignatureInvalid` に写像（`.map_err(|_| CaseError::Sigma2SignatureInvalid)`）、Sigma3 署名生成失敗は `CaseError::Crypto("sigma3 signature")` 等に写像。

- [ ] **Step 5: 通ることを確認** — `cargo test -p mat-controller` 全 PASS（case の既存テスト含む）

- [ ] **Step 6: `task check` → コミット**

```bash
git add crates/mat-controller/src/crypto.rs crates/mat-controller/src/case.rs
git commit -m "refactor(mat-controller): promote raw ECDSA sign/verify to crypto (pub, typed errors)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: `cert` — SKID ヘルパ + `MatterCert::to_tlv()` エンコーダ

**Files:**
- Modify: `crates/mat-controller/Cargo.toml`
- Modify: `crates/mat-controller/src/cert.rs`

**Interfaces:**
- Consumes: `tlv::{Writer, Tag}`（M1）、`sha1`
- Produces:
  ```rust
  pub fn subject_key_id(public_key: &[u8; 65]) -> [u8; 20]; // SHA1(pubkey)
  impl MatterCert { pub fn to_tlv(&self) -> Vec<u8>; }        // parse の逆
  ```

- [ ] **Step 1: 依存追加** — `Cargo.toml` の `[dependencies]` に `sha1 = "0.10"`。

- [ ] **Step 2: 失敗するテストを書く**（cert.rs tests mod。既存フィクスチャ利用）

```rust
    #[test]
    fn subject_key_id_is_sha1_of_pubkey() {
        use sha1::{Digest, Sha1};
        let node = MatterCert::parse(NODE_CHIP).unwrap();
        let expected: [u8; 20] = Sha1::digest(node.pub_key).into();
        assert_eq!(subject_key_id(&node.pub_key), expected);
    }

    #[test]
    fn to_tlv_roundtrips_all_fixtures() {
        // パース → 再エンコード → 元の TLV バイトと完全一致（エンコーダ正しさの決定的アンカー）
        for chip in [ROOT_CHIP, ICA_CHIP, NODE_CHIP] {
            let cert = MatterCert::parse(chip).unwrap();
            assert_eq!(cert.to_tlv().as_slice(), chip, "re-encode must byte-match");
        }
    }
```

- [ ] **Step 3: 落ちることを確認** — `cargo test -p mat-controller cert` → コンパイルエラー

- [ ] **Step 4: 実装**（cert.rs）

```rust
/// SubjectKeyIdentifier per Matter/X.509: SHA-1 of the 65-byte public key.
pub fn subject_key_id(public_key: &[u8; 65]) -> [u8; 20] {
    use sha1::{Digest, Sha1};
    Sha1::digest(public_key).into()
}

impl MatterCert {
    /// Encode this certificate back to Matter TLV — inverse of `parse`.
    /// Field/extension order follows the parsed struct (which preserves TLV order).
    pub fn to_tlv(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(1), &self.serial);
        w.put_uint(Tag::Context(2), 1); // ecdsa-with-sha256
        write_dn(&mut w, 3, &self.issuer);
        w.put_uint(Tag::Context(4), u64::from(self.not_before));
        w.put_uint(Tag::Context(5), u64::from(self.not_after));
        write_dn(&mut w, 6, &self.subject);
        w.put_uint(Tag::Context(7), 1); // ec public key
        w.put_uint(Tag::Context(8), 1); // prime256v1
        w.put_bytes(Tag::Context(9), &self.pub_key);
        w.start_list(Tag::Context(10));
        for ext in &self.extensions {
            write_extension(&mut w, ext);
        }
        w.end_container(); // extensions list
        w.put_bytes(Tag::Context(11), &self.signature);
        w.end_container(); // top struct
        w.finish()
    }
}

fn write_dn(w: &mut Writer, ctx: u8, attrs: &[DnAttr]) {
    w.start_list(Tag::Context(ctx));
    for a in attrs {
        match &a.value {
            DnValue::MatterId(id) => w.put_uint(Tag::Context(a.tlv_tag), *id),
            DnValue::Text(s) => w.put_str(Tag::Context(a.tlv_tag), s),
        }
    }
    w.end_container();
}

fn write_extension(w: &mut Writer, ext: &CertExtension) {
    match ext {
        CertExtension::BasicConstraints { is_ca, path_len } => {
            w.start_struct(Tag::Context(1));
            w.put_bool(Tag::Context(1), *is_ca);
            if let Some(pl) = path_len {
                w.put_uint(Tag::Context(2), u64::from(*pl));
            }
            w.end_container();
        }
        CertExtension::KeyUsage(bits) => w.put_uint(Tag::Context(2), u64::from(*bits)),
        CertExtension::ExtendedKeyUsage(purposes) => {
            w.start_array(Tag::Context(3));
            for p in purposes {
                w.put_uint(Tag::Anonymous, *p);
            }
            w.end_container();
        }
        CertExtension::SubjectKeyId(id) => w.put_bytes(Tag::Context(4), id),
        CertExtension::AuthorityKeyId(id) => w.put_bytes(Tag::Context(5), id),
    }
}
```

**デバッグ指針:** `to_tlv_roundtrips_all_fixtures` が落ちたら、`cert.to_tlv()` と元 `chip` バイトを先頭から突き合わせ、最初に食い違うオフセットのフィールドを特定する（DN の uint 幅、EKU の array 表現、BasicConstraints の path_len 省略、拡張の並び順が定番の食い違い）。parse が保持する順序をそのまま書けば一致するはず。

- [ ] **Step 5: 通ることを確認** — `cargo test -p mat-controller cert` → PASS

- [ ] **Step 6: `task check` → コミット**

```bash
git add crates/mat-controller/Cargo.toml Cargo.lock crates/mat-controller/src/cert.rs
git commit -m "feat(mat-controller): Matter TLV cert encoder (to_tlv) + SKID helper

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: `cert` — NOC 自己発行 `issue_noc()`

**Files:**
- Modify: `crates/mat-controller/src/cert.rs`
- Create: `crates/mat-controller/tests/fixtures/root01_privkey.bin`, `ica01_privkey.bin`（SDK 公開テスト鍵）

**Interfaces:**
- Consumes: `crypto::sign_ecdsa_p256`（Task 1）、`subject_key_id`/`to_tlv`（Task 2）
- Produces:
  ```rust
  pub const MATTER_EPOCH_2021: u32 = 662_688_000;   // 2021-01-01T00:00:00Z in Matter epoch seconds
  pub const NOC_VALIDITY_SECS: u32 = 315_360_000;    // 10 years
  /// Build and sign a NOC (2-cert chain: signed directly by `issuer`, no ICAC).
  /// `issuer` is the RCAC (self-signed root); `issuer_private_key` its op key.
  pub fn issue_noc(
      op_public_key: &[u8; 65],
      node_id: u64,
      fabric_id: u64,
      issuer: &MatterCert,
      issuer_private_key: &[u8; 32],
      serial: &[u8],
  ) -> Result<MatterCert, CertError>;
  ```

- [ ] **Step 1: SDK 公開テスト秘密鍵フィクスチャを抽出 + EKU 実値を確認**

Docker イメージ `mat-chip-builder` から抽出（scratchpad に vectors.cpp を出し、Task 4 と同じ抽出スクリプトの対象に `sTestCert_Root01_PrivateKey`→`root01_privkey.bin`、`sTestCert_ICA01_PrivateKey`→`ica01_privkey.bin` を足して実行）:

```bash
docker run --rm mat-chip-builder cat /work/connectedhomeip/src/credentials/tests/CHIPCert_test_vectors.cpp > <scratchpad>/vectors.cpp
# 既存の extract スクリプト（Task 4 で作成、tests/fixtures/README.md 記載の手順）に
# root01_privkey / ica01_privkey を追加して再実行、tests/fixtures/ に 32B ×2 を出力
```

期待: `root01_privkey.bin` / `ica01_privkey.bin` はいずれも 32 バイト。`tests/fixtures/README.md` に 2 ファイルの追記。
同時に、`node01_01` を parse して EKU の実値・順序を確認しておく（`issue_noc` が同じ値を使うため）:

```bash
# 参考: cargo test 内の一時アサートか、既存 parses_node_cert_ids テストに
#   eprintln!("{:?}", node.extensions); を足して EKU の Vec<u64> 実値を目視確認。
# 期待: ExtendedKeyUsage([2, 1]) 相当（client, server）。実値に issue_noc を合わせる。
```

- [ ] **Step 2: 失敗するテストを書く**（cert.rs tests mod）

```rust
    #[test]
    fn issue_noc_produces_chain_valid_cert() {
        let root = MatterCert::parse(ROOT_CHIP).unwrap();
        let root_priv: [u8; 32] = include_bytes!("../tests/fixtures/root01_privkey.bin")
            .as_slice()
            .try_into()
            .unwrap();
        // 我々の operational 鍵ペア（テストではフィクスチャの node 鍵を流用）
        let op_pub: [u8; 65] = include_bytes!("../tests/fixtures/node01_01_pubkey.bin")
            .as_slice()
            .try_into()
            .unwrap();

        let noc = issue_noc(&op_pub, 0x1B669, 1, &root, &root_priv, &[0x01, 0x02, 0x03, 0x04])
            .unwrap();

        // subject に node/fabric id が入っている
        assert_eq!(noc.node_id(), Some(0x1B669));
        assert_eq!(noc.fabric_id(), Some(1));
        // pub key は渡したもの
        assert_eq!(noc.pub_key, op_pub);
        // root で署名検証が通る（DER TBS への署名が正しい）
        noc.verify_signed_by(&root.pub_key).unwrap();
        // ICAC 無しのチェーン検証が通る（root 直署名）
        verify_noc_chain(&noc, None, &root).unwrap();
        // NOC 拡張: BasicConstraints(false) / KeyUsage(digitalSignature) / EKU(client,server) / SKID / AKID
        assert!(noc.extensions.iter().any(|e| matches!(
            e, CertExtension::BasicConstraints { is_ca: false, path_len: None }
        )));
        assert!(noc.extensions.iter().any(|e| matches!(e, CertExtension::KeyUsage(0x0001))));
        // SKID = SHA1(op_pub), AKID = root の SKID
        let root_skid = root.extensions.iter().find_map(|e| match e {
            CertExtension::SubjectKeyId(id) => Some(id.clone()),
            _ => None,
        });
        assert!(noc.extensions.iter().any(|e| matches!(
            e, CertExtension::SubjectKeyId(id) if id.as_slice() == subject_key_id(&op_pub)
        )));
        assert!(noc.extensions.iter().any(|e| matches!(
            e, CertExtension::AuthorityKeyId(id) if Some(id) == root_skid.as_ref()
        )));
        // TLV に書き出して再パースしても等価
        let reparsed = MatterCert::parse(&noc.to_tlv()).unwrap();
        assert_eq!(reparsed.node_id(), Some(0x1B669));
        reparsed.verify_signed_by(&root.pub_key).unwrap();
    }
```

- [ ] **Step 3: 落ちることを確認** — `cargo test -p mat-controller cert` → コンパイルエラー

- [ ] **Step 4: 実装**（cert.rs）

```rust
pub const MATTER_EPOCH_2021: u32 = 662_688_000;
pub const NOC_VALIDITY_SECS: u32 = 315_360_000;

// EKU (Matter TLV enum values): serverAuth=1, clientAuth=2. NOC uses client, server.
const EKU_CLIENT_AUTH: u64 = 2;
const EKU_SERVER_AUTH: u64 = 1;
const KEY_USAGE_DIGITAL_SIGNATURE: u16 = 0x0001;

pub fn issue_noc(
    op_public_key: &[u8; 65],
    node_id: u64,
    fabric_id: u64,
    issuer: &MatterCert,
    issuer_private_key: &[u8; 32],
    serial: &[u8],
) -> Result<MatterCert, CertError> {
    // 発行者(root)の SubjectKeyId を AKID に使う
    let issuer_skid = issuer
        .extensions
        .iter()
        .find_map(|e| match e {
            CertExtension::SubjectKeyId(id) => Some(id.clone()),
            _ => None,
        })
        .ok_or(CertError::Malformed("issuer has no subject key id"))?;

    let extensions = vec![
        CertExtension::BasicConstraints { is_ca: false, path_len: None },
        CertExtension::KeyUsage(KEY_USAGE_DIGITAL_SIGNATURE),
        CertExtension::ExtendedKeyUsage(vec![EKU_CLIENT_AUTH, EKU_SERVER_AUTH]),
        CertExtension::SubjectKeyId(subject_key_id(op_public_key).to_vec()),
        CertExtension::AuthorityKeyId(issuer_skid),
    ];

    let mut noc = MatterCert {
        serial: serial.to_vec(),
        issuer: issuer.subject.clone(),
        not_before: MATTER_EPOCH_2021,
        not_after: MATTER_EPOCH_2021.saturating_add(NOC_VALIDITY_SECS),
        subject: vec![
            DnAttr { tlv_tag: 17, value: DnValue::MatterId(node_id) },
            DnAttr { tlv_tag: 21, value: DnValue::MatterId(fabric_id) },
        ],
        pub_key: *op_public_key,
        extensions,
        signature: [0u8; 64], // TBS 署名で埋める
    };

    let tbs = noc.tbs_der()?;
    noc.signature = crate::crypto::sign_ecdsa_p256(issuer_private_key, &tbs)
        .map_err(|_| CertError::BadSignature)?;
    Ok(noc)
}
```

**注記:** subject DN の並びは node_id(17) → fabric_id(21)。EKU 値・順序は Step 1 で `node01_01` から確認した実値に合わせる（上記は client=2, server=1 の順）。もし実測が異なれば定数を修正。

- [ ] **Step 5: 通ることを確認** — `cargo test -p mat-controller cert` → PASS

- [ ] **Step 6: `task check` → コミット**

```bash
git add crates/mat-controller/src/cert.rs crates/mat-controller/tests/fixtures/root01_privkey.bin crates/mat-controller/tests/fixtures/ica01_privkey.bin crates/mat-controller/tests/fixtures/README.md
git commit -m "feat(mat-controller): self-issue NOC signed directly by root (issue_noc)

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 4: `kvs` — CA 資格材料リーダ

**Files:**
- Modify: `crates/mat-controller/src/kvs.rs`

**Interfaces:**
- Consumes: 既存の ini パーサ補助（`default_section`/`lookup`）、keyset パーサ（`parse_keyset`）、base64ct
- Produces:
  ```rust
  pub struct SelfIssueMaterials {
      pub rcac: Vec<u8>,             // Matter TLV 証明書
      pub root_public_key: [u8; 65],
      pub root_private_key: [u8; 32],
      pub ipk_operational: [u8; 16],
      pub node_id: u64,
      pub fabric_id: u64,
  }
  // Debug は手実装で root_private_key/ipk を REDACTED（既存 RawFabricCredentials と同方針）
  /// alpha_ini = chip_tool_config.alpha.ini（CA 材料）, main_ini = chip_tool_config.ini（IPK/node id）
  pub fn read_self_issue_materials(
      alpha_ini: &std::path::Path,
      main_ini: &std::path::Path,
      fabric_index: u8,
      issuer_index: u8,
  ) -> Result<SelfIssueMaterials, KvsError>;
  ```
  `KvsError` に必要なら `BadCaKey(&'static str)` を追加（97B 長チェック用）。

- [ ] **Step 1: 失敗するテストを書く**（kvs.rs tests mod。合成フィクスチャ）

```rust
    #[test]
    fn reads_self_issue_materials() {
        // root 鍵は生 97B（TLV ラップ無し）
        let mut root_key = Vec::with_capacity(97);
        root_key.extend_from_slice(&[0xAA; 65]); // pub
        root_key.extend_from_slice(&[0xBB; 32]); // priv
        let ks = keyset_blob(&[0xCC; 16]); // 既存ヘルパ（Task 2/M2 の keyset_blob 相当）

        let alpha = write_named_ini("alpha", &[
            ("ExampleCARootCert0", b"rcac-tlv-bytes"),
            ("ExampleOpCredsCAKey0", &root_key),
        ]);
        let main = write_named_ini("main", &[
            ("f/1/k/0", &ks),
            // LocalNodeId 無し → 既定 112233
        ]);

        let m = read_self_issue_materials(&alpha, &main, 1, 0).unwrap();
        assert_eq!(m.rcac, b"rcac-tlv-bytes");
        assert_eq!(m.root_public_key, [0xAA; 65]);
        assert_eq!(m.root_private_key, [0xBB; 32]);
        assert_eq!(m.ipk_operational, [0xCC; 16]);
        assert_eq!(m.node_id, 112233);
        assert_eq!(m.fabric_id, 1);
        std::fs::remove_file(alpha).ok();
        std::fs::remove_file(main).ok();
    }

    #[test]
    fn local_node_id_overrides_default() {
        let mut root_key = vec![0xAA; 65];
        root_key.extend_from_slice(&[0xBB; 32]);
        let ks = keyset_blob(&[0xCC; 16]);
        let alpha = write_named_ini("alpha2", &[
            ("ExampleCARootCert0", b"r"),
            ("ExampleOpCredsCAKey0", &root_key),
        ]);
        // LocalNodeId = 0x1122334455667788, u64 LE 8 バイト
        let node_le = 0x1122_3344_5566_7788u64.to_le_bytes();
        let main = write_named_ini("main2", &[
            ("f/1/k/0", &ks),
            ("LocalNodeId", &node_le),
        ]);
        let m = read_self_issue_materials(&alpha, &main, 1, 0).unwrap();
        assert_eq!(m.node_id, 0x1122_3344_5566_7788);
        std::fs::remove_file(alpha).ok();
        std::fs::remove_file(main).ok();
    }
```
（`write_named_ini(tag, entries)` は既存 `write_ini` を名前付きにした薄いヘルパ。既存 `write_ini`/`keyset_blob` を再利用してよい。`keyset_blob` は M2 Task 2 のテストヘルパ。）

- [ ] **Step 2: 落ちることを確認** — `cargo test -p mat-controller kvs` → コンパイルエラー

- [ ] **Step 3: 実装**（kvs.rs）

```rust
pub const DEFAULT_CONTROLLER_NODE_ID: u64 = 112_233; // kTestControllerNodeId (SDK v1.4.2.0)

pub fn read_self_issue_materials(
    alpha_ini: &Path,
    main_ini: &Path,
    fabric_index: u8,
    issuer_index: u8,
) -> Result<SelfIssueMaterials, KvsError> {
    // --- alpha ini: CA 材料 ---
    let alpha_text = std::fs::read_to_string(alpha_ini).map_err(KvsError::Io)?;
    let alpha_sec = default_section(&alpha_text).ok_or(KvsError::SectionMissing)?;
    let rcac_key = format!("ExampleCARootCert{issuer_index}");
    let cakey_key = format!("ExampleOpCredsCAKey{issuer_index}");
    let rcac = decode_b64(alpha_sec, &rcac_key)?.ok_or(KvsError::KeyMissing(rcac_key))?;
    let ca_key = decode_b64(alpha_sec, &cakey_key.clone())?.ok_or(KvsError::KeyMissing(cakey_key))?;
    if ca_key.len() != 97 {
        return Err(KvsError::BadCaKey("root ca key must be 97 raw bytes (pub65||priv32)"));
    }
    let root_public_key: [u8; 65] = ca_key[..65].try_into().expect("65");
    let root_private_key: [u8; 32] = ca_key[65..].try_into().expect("32");

    // --- main ini: IPK + node id ---
    let main_text = std::fs::read_to_string(main_ini).map_err(KvsError::Io)?;
    let main_sec = default_section(&main_text).ok_or(KvsError::SectionMissing)?;
    let ipk_operational = parse_keyset(
        &decode_b64(main_sec, &format!("f/{fabric_index}/k/0"))?
            .ok_or_else(|| KvsError::KeyMissing(format!("f/{fabric_index}/k/0")))?,
        fabric_index,
    )?;
    let node_id = match decode_b64(main_sec, "LocalNodeId")? {
        Some(b) if b.len() == 8 => u64::from_le_bytes(b.try_into().expect("8")),
        _ => DEFAULT_CONTROLLER_NODE_ID,
    };

    Ok(SelfIssueMaterials {
        rcac,
        root_public_key,
        root_private_key,
        ipk_operational,
        node_id,
        fabric_id: u64::from(fabric_index),
    })
}
```
補助 `decode_b64(section, key) -> Result<Option<Vec<u8>>, KvsError>` は既存 `read_fabric_credentials` 内のクロージャ `get` と同等（無い/空 → None、base64 失敗 → `BadBase64`）。既存コードから小関数として括り出して両者で共有（DRY）。`parse_keyset` は既存シグネチャ（`(&[u8], fabric_index) -> Result<[u8;16], KvsError>`）を流用。

- [ ] **Step 4: 通ることを確認** — `cargo test -p mat-controller kvs` → PASS

- [ ] **Step 5: `task check` → コミット**

```bash
git add crates/mat-controller/src/kvs.rs
git commit -m "feat(mat-controller): read CA materials (root cert+key, IPK, node id) for self-issuance

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 5: `fabric` — `from_self_issued` + verify_noc_chain validity + 持ち越し整理

**Files:**
- Modify: `crates/mat-controller/src/fabric.rs`
- Modify: `crates/mat-controller/src/cert.rs`（verify_noc_chain に validity チェック、NocMissingIds 整理）

**Interfaces:**
- Consumes: `kvs::SelfIssueMaterials`、`cert::{MatterCert, issue_noc, verify_noc_chain}`、`crypto`（鍵生成用 p256）
- Produces:
  ```rust
  impl FabricCredentials {
      /// Generate a fresh operational key, self-issue a NOC under the KVS root,
      /// and assemble credentials for CASE. `now_matter_epoch` lets callers pass
      /// the current time for the validity check (0 to skip).
      pub fn from_self_issued(m: crate::kvs::SelfIssueMaterials) -> Result<Self, FabricError>;
  }
  // FabricError に GenKey / SelfIssue(CertError) variant 追加
  ```

- [ ] **Step 1: 失敗するテストを書く**（fabric.rs tests mod）

```rust
    #[test]
    fn from_self_issued_builds_case_ready_credentials() {
        // root01 フィクスチャを KVS 材料に見立てる
        let rcac = include_bytes!("../tests/fixtures/root01_chip.bin").to_vec();
        let root_priv: [u8; 32] = include_bytes!("../tests/fixtures/root01_privkey.bin")
            .as_slice().try_into().unwrap();
        let root_pub: [u8; 65] = include_bytes!("../tests/fixtures/root01_pubkey.bin")
            .as_slice().try_into().unwrap();
        let m = crate::kvs::SelfIssueMaterials {
            rcac,
            root_public_key: root_pub,
            root_private_key: root_priv,
            ipk_operational: [0xCC; 16],
            node_id: 0x1B669,
            fabric_id: 1,
        };
        let creds = FabricCredentials::from_self_issued(m).unwrap();
        assert_eq!(creds.node_id, 0x1B669);
        assert_eq!(creds.fabric_id, 1);
        assert_eq!(creds.icac_tlv, None); // root 直署名の 2 段
        assert_eq!(creds.root_public_key, root_pub);
        // 生成鍵と NOC の公開鍵が一致
        let noc = crate::cert::MatterCert::parse(&creds.noc_tlv).unwrap();
        assert_eq!(noc.pub_key, creds.op_public_key);
        // 自己発行 NOC は root にチェーン
        let rcac_cert = crate::cert::MatterCert::parse(&creds.rcac_tlv).unwrap();
        crate::cert::verify_noc_chain(&noc, None, &rcac_cert).unwrap();
    }
```

- [ ] **Step 2: 落ちることを確認** — `cargo test -p mat-controller fabric` → コンパイルエラー

- [ ] **Step 3: 実装**（fabric.rs）

```rust
impl FabricCredentials {
    pub fn from_self_issued(m: crate::kvs::SelfIssueMaterials) -> Result<Self, FabricError> {
        use p256::ecdsa::SigningKey;
        use p256::elliptic_curve::sec1::ToEncodedPoint;

        // 1. 新規 operational 鍵ペア
        let sk = SigningKey::random(&mut rand_core_os_rng());
        let op_private_key: [u8; 32] = sk.to_bytes().into();
        let op_public_key: [u8; 65] = sk
            .verifying_key()
            .to_encoded_point(false)
            .as_bytes()
            .try_into()
            .map_err(|_| FabricError::GenKey)?;

        // 2. root で NOC を自己発行
        let rcac = crate::cert::MatterCert::parse(&m.rcac).map_err(FabricError::Cert)?;
        let mut serial = [0u8; 8];
        getrandom::getrandom(&mut serial).expect("os rng");
        serial[0] &= 0x7F; // BER INTEGER を正の最小表現に
        let noc = crate::cert::issue_noc(
            &op_public_key, m.node_id, m.fabric_id, &rcac, &m.root_private_key, &serial,
        ).map_err(FabricError::SelfIssue)?;

        // 3. 自己検証（生成器と検証器の相互チェック）
        crate::cert::verify_noc_chain(&noc, None, &rcac).map_err(FabricError::SelfIssue)?;

        Ok(FabricCredentials {
            rcac_tlv: m.rcac,
            icac_tlv: None,
            noc_tlv: noc.to_tlv(),
            op_public_key,
            op_private_key,
            ipk_operational: m.ipk_operational,
            node_id: m.node_id,
            fabric_id: m.fabric_id,
            root_public_key: m.root_public_key,
        })
    }
}
```
`SigningKey::random` の RNG は p256 0.13 が要求する `rand_core` 版に注意。`rand_core` の OsRng が features 経由で無い場合は、`getrandom` で 32B を引き `SigningKey::from_slice` をループ（0/≥n 再試行）する自前ヘルパ `random_signing_key()` を書く（case.rs の `random_p256_secret` と同型 — そちらを `pub(crate)` に上げて共有してもよい）。**追加 crate（rand)は入れない**（Global Constraints）。

`FabricError` に `GenKey`（Display: `"operational key generation failed"`）、`SelfIssue(CertError)`（`"self-issued NOC invalid: {0}"`）を追加。

- [ ] **Step 4: verify_noc_chain に validity チェックを追加 + NocMissingIds 整理**（cert.rs、最終レビュー持ち越し）

`verify_noc_chain` は現状 validity を見ない。呼び出し側が「今」を渡せるよう任意引数は増やさず、**別の薄い関数**を足す:

```rust
/// True if `matter_epoch_now` is within [not_before, not_after] (not_after 0 = 無期限).
pub fn cert_time_valid(cert: &MatterCert, matter_epoch_now: u32) -> bool {
    matter_epoch_now >= cert.not_before && (cert.not_after == 0 || matter_epoch_now <= cert.not_after)
}
```
（`verify_noc_chain` 自体のシグネチャは M2 の呼び出し元を壊さないため変更しない。時刻源の決定は CASE 実行側／M4 に委ねる。テストを 1 本足す: issue_noc で出した NOC が 2021〜2031 の時刻で valid、2032 で invalid。）
`fabric.rs` の `NocMissingIds` は `verify_noc_chain` が id 存在を保証するため `from_raw`/`from_self_issued` では到達しない。variant は残すが doc コメントで「防御的（verify_noc_chain の保証と重複）」と明記。`FabricError` に `source()` を実装（`Cert`/`SelfIssue` の内側を返す）。

- [ ] **Step 5: 通ることを確認** — `cargo test -p mat-controller` 全 PASS

- [ ] **Step 6: `task check` → コミット**

```bash
git add crates/mat-controller/src/fabric.rs crates/mat-controller/src/cert.rs
git commit -m "feat(mat-controller): FabricCredentials::from_self_issued + cert validity helper

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 6: オフライン CASE 自己ハンドシェイクテスト（最終レビュー必須要件）

**Files:**
- Create: `crates/mat-controller/tests/case_self_handshake.rs`

**Interfaces:**
- Consumes: `case::establish`、`session::SecureSession`、`fabric`, `cert`, `crypto`, `tlv`, `message`, `exchange`, `transport`、フィクスチャ（node01_01 = responder identity、root01/ica01）

**目的:** ライブ E2E がブロックされている間、CASE 暗号コア（transcript 境界・S2K/S3K/SessionKeys 導出・TBS 配置・鍵分割・ワイヤ framing）を**実行**カバレッジに入れる。テスト専用の最小 CASE **responder** を書き、`case::establish`（initiator）とループバック UDP で握手させる。

- [ ] **Step 1: responder を書く（テスト内）**

responder は initiator の鏡像。以下を 1 つの `async fn run_responder(transport, initiator_addr_known_after_recv, responder_creds)` に実装する。使う材料は fixtures:
- responder identity: `node01_01`（NOC + privkey）、`ica01`（ICAC）、`root01`（RCAC）。initiator も**同じ root01 を信頼アンカーに持つ**ように `FabricCredentials` を構成する（下記 Step 2）。
- responder は Sigma1 を UnsecuredExchange 相当で受信 → initiator random / initiator eph pub / initiator session id を parse。
- 自分の ephemeral P-256 鍵生成 → ECDH(initiator eph pub) → S2K salt = ipk || responder_random || responder_eph_pub || SHA256(sigma1) → HKDF info "Sigma2"。
- TBE2 = TLV struct{1: responder NOC, 2: responder ICAC, 3: sign(TBS2), 4: resumptionId(空)}。TBS2 = struct{1: NOC, 2: ICAC, 3: responder eph, 4: initiator eph}、responder op 鍵で `crypto::sign_ecdsa_p256`。
- Sigma2 送信（responder session id は非ゼロ）。transcript に sigma1→sigma2 を積む。
- Sigma3 受信 → S3K = ipk || SHA256(sigma1||sigma2), info "Sigma3" → TBE3 復号 → initiator NOC を root01 でチェーン検証 → TBS3 署名検証。
- StatusReport(success = 0,0,0) 送信。SessionKeys = ipk || SHA256(sigma1||sigma2||sigma3), info "SessionKeys", 48B → i2r/r2i/attestation。
- 以降 secured セッションで initiator の ReadRequest を受け、ReportData(on-off=false, suppress) を返す。

responder は `case.rs` の pub(crate) ヘルパ（`encode_sigma1` 等）を**使わない**（あちらは initiator 専用）。TLV は `tlv::Writer`/`Reader` で直接組む。crypto/HKDF は `crypto` と `hkdf` を直接使う。定数（TBE nonce "NCASE_Sigma2N"/"NCASE_Sigma3N"、info 文字列、opcode）はテスト内に責任を持って再定義する（**残留リスク**: initiator/responder で同一定数を両方間違えると検出できない → テスト doc に明記。orientation/ordering/framing バグは捕捉できる）。

- [ ] **Step 2: initiator 用 FabricCredentials を組む**

initiator は「root01 を信頼し、root01 直署名の自前 NOC を持つ」= `from_self_issued` 相当。テストでは `SelfIssueMaterials { rcac: root01_chip, root_public_key: root01_pub, root_private_key: root01_priv, ipk_operational: <responder と同じ IPK>, node_id: <任意, 例 1>, fabric_id: <responder NOC の fabric id と一致させる> }` から `FabricCredentials::from_self_issued`。**IPK と fabric id は responder と initiator で一致必須**（CASE 成立条件）。responder の NOC(node01_01) の fabric id を parse して合わせる。

- [ ] **Step 3: テスト本体**

```rust
#[tokio::test]
async fn case_establishes_and_reads_over_loopback() {
    // responder をタスク起動、initiator が case::establish → read_attribute。
    // assert: establish が Ok(session)、session.read_attribute(onoff) が Ok(Bool(false))。
    // MrpConfig は fast（initial 50ms, retries 3）。
}
```

expected: PASS。これで transcript/KDF/TBS orientation/framing が緑になる。**落ちた場合**は M2 の case.rs 側バグの可能性が高い（ライブ前の最良の検出機会）— stage 別 CaseError と、responder 側で initiator メッセージを復号/検証した際のエラーで切り分け、根本原因が case.rs にあれば最小修正して別 `fix(mat-controller):` コミット。

- [ ] **Step 4: `task check` → コミット**

```bash
git add crates/mat-controller/tests/case_self_handshake.rs
git commit -m "test(mat-controller): offline CASE self-handshake covering the crypto ordering core

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 7: ライブ E2E（self-issued 経路）+ `task e2e:m2` + docs（受け入れゲート）

**Files:**
- Create/Replace: `crates/mat-controller/tests/live_case_im.rs`（既存の未コミット版を上書き）
- Create/Replace: `scripts/e2e-m2.sh`
- Modify: `Taskfile.yml`（`e2e:m2` エントリ）
- Modify: `ARCHITECTURE.md`

**Interfaces:**
- Consumes: 全モジュール。環境変数: `MAT_E2E_KVS_DIR`（chip-tool の storage ディレクトリ。ここから `chip_tool_config.alpha.ini` と `chip_tool_config.ini` を読む）、`MAT_E2E_NODE_ID`（デバイス node id、`0x` 可）、`MAT_E2E_PEER`（省略時 `[::1]:5540`）。

- [ ] **Step 1: ライブテストを書く**（self-issued 経路）

```rust
//! Live E2E: self-issue an operational identity from chip-tool's persisted root
//! CA key, then CASE + IM against a commissioned chip-all-clusters-app.
//! Run via `task e2e:m2`. Not in CI. Requires MAT_E2E_KVS_DIR / MAT_E2E_NODE_ID.

use std::path::PathBuf;
use mat_controller::exchange::MrpConfig;
use mat_controller::fabric::FabricCredentials;
use mat_controller::im::{ImValue, ATTR_ON_OFF, CLUSTER_ON_OFF, CMD_ON_OFF_TOGGLE};
use mat_controller::message::MATTER_PORT;
use mat_controller::transport::UdpTransport;
use mat_controller::{case, kvs};

fn env_node_id() -> u64 {
    let s = std::env::var("MAT_E2E_NODE_ID").expect("MAT_E2E_NODE_ID required");
    match s.strip_prefix("0x") {
        Some(h) => u64::from_str_radix(h, 16).expect("hex node id"),
        None => s.parse().expect("decimal node id"),
    }
}

#[tokio::test]
#[ignore = "requires a commissioned device + chip-tool KVS (task e2e:m2)"]
async fn self_issued_case_read_toggle_read() {
    let dir = PathBuf::from(std::env::var("MAT_E2E_KVS_DIR").expect("MAT_E2E_KVS_DIR required"));
    let device_node_id = env_node_id();
    let peer = std::env::var("MAT_E2E_PEER")
        .unwrap_or_else(|_| format!("[::1]:{MATTER_PORT}"))
        .parse().expect("socket addr");

    // 受け入れ 2: KVS から CA 材料
    let materials = kvs::read_self_issue_materials(
        &dir.join("chip_tool_config.alpha.ini"),
        &dir.join("chip_tool_config.ini"),
        1, 0,
    ).expect("read CA materials");
    eprintln!("controller node id 0x{:016X}, fabric id {}", materials.node_id, materials.fabric_id);

    // 受け入れ 3: 自前 NOC 自己発行
    let creds = FabricCredentials::from_self_issued(materials).expect("self-issue NOC");

    // 受け入れ 4: CASE 確立（我々の自己発行 NOC を実機が受理）
    let transport = UdpTransport::bind().await.unwrap();
    let cfg = MrpConfig::default();
    let mut session = case::establish(&transport, peer, &creds, device_node_id, &cfg)
        .await.expect("CASE establishment");
    eprintln!("CASE established with device 0x{:016X}", session.peer_node_id());

    // 受け入れ 5/6: read → toggle → read（admin 権限で通る = ACL 継承の実証）
    let before = session.read_attribute(1, CLUSTER_ON_OFF, ATTR_ON_OFF, &cfg).await.expect("read");
    let ImValue::Bool(before) = before else { panic!("on-off not bool: {before:?}") };
    let outcome = session.invoke(1, CLUSTER_ON_OFF, CMD_ON_OFF_TOGGLE, None, &cfg).await.expect("toggle");
    assert_eq!(outcome.status, 0);
    let after = session.read_attribute(1, CLUSTER_ON_OFF, ATTR_ON_OFF, &cfg).await.expect("read2");
    assert_eq!(after, ImValue::Bool(!before), "toggle must flip on-off");
    eprintln!("on-off {before} -> {after:?}");
}
```

- [ ] **Step 2: ハーネススクリプト**（`scripts/e2e-m2.sh`、`chmod +x`）

```bash
#!/usr/bin/env bash
# Phase 5 M2b 受け入れ: 使い捨て fabric でコミッション → 自己発行 NOC で CASE + IM。
# 前提: ./chip-all-clusters-app と ./chip-tool（task chip:extract:app / chip:extract）。
set -euo pipefail
cd "$(dirname "$0")/.."
APP=${MAT_E2E_APP:-./chip-all-clusters-app}
CHIP_TOOL=${MAT_CHIP_TOOL_BIN:-./chip-tool}
NODE_ID=0x12344321
PASSCODE=20202021
[[ -x "$APP" ]] || { echo "error: $APP なし (task chip:extract:app)"; exit 1; }
[[ -x "$CHIP_TOOL" ]] || { echo "error: $CHIP_TOOL なし (task chip:extract)"; exit 1; }

WORK=$(mktemp -d)
APP_PID=""
cleanup() { [[ -n "$APP_PID" ]] && kill "$APP_PID" 2>/dev/null || true; rm -rf "$WORK"; }
trap cleanup EXIT

echo "== 1/3 app 起動 (KVS: $WORK/device_kvs)"
"$APP" --KVS "$WORK/device_kvs" >"$WORK/app.log" 2>&1 &
APP_PID=$!
for i in $(seq 1 40); do
  ss -uln 2>/dev/null | grep -q ':5540' && break
  kill -0 "$APP_PID" 2>/dev/null || { echo "app 起動失敗"; cat "$WORK/app.log"; exit 1; }
  sleep 0.25
done

echo "== 2/3 chip-tool でコミッション (device node $NODE_ID)"
"$CHIP_TOOL" pairing already-discovered "$NODE_ID" "$PASSCODE" ::1 5540 \
  --storage-directory "$WORK" >"$WORK/pairing.log" 2>&1 \
  || { echo "pairing 失敗"; tail -40 "$WORK/pairing.log"; exit 1; }
grep -qi "commissioning completed with success" "$WORK/pairing.log" \
  || { echo "コミッション成功ログ無し"; tail -40 "$WORK/pairing.log"; exit 1; }

echo "== 3/3 self-issued CASE + IM ライブテスト"
# controller node id は chip-tool 既定 112233。デバイス側はその node id に admin。
MAT_E2E_KVS_DIR="$WORK" \
MAT_E2E_NODE_ID="$NODE_ID" \
MAT_E2E_PEER="[::1]:5540" \
  cargo test -p mat-controller --test live_case_im -- --ignored --nocapture

echo "== e2e:m2 PASS"
```

- [ ] **Step 3: Taskfile に追加**（`e2e:m1` の直後。既存の未コミット `e2e:m2` があれば置換）

```yaml
  e2e:m2:
    desc: M2b ライブ E2E（自己発行 NOC で CASE + onoff toggle/read。要 ./chip-tool と ./chip-all-clusters-app）
    cmds:
      - bash scripts/e2e-m2.sh
```

- [ ] **Step 4: CI 相当が通ることを確認** — `task check`（ライブは `#[ignore]`、自己ハンドシェイクは走る）→ PASS

- [ ] **Step 5: 受け入れ実行**

Run: `task e2e:m2`
Expected: `== e2e:m2 PASS`（受け入れ 1〜6 一括）。

**失敗時の切り分け（実行権限あり — case/session/im/cert/kvs のバグなら最小修正して別 fix コミット）:**
- CASE Sigma2 が来ずタイムアウト → destination id 不一致（IPK / root pub / fabric id / device node id）。`$WORK/app.log` に device 側 CASE ログ。
- **CASE 成立するが read/invoke が ACCESS_DENIED (IM status 0x7E)** → **決定 2 のリスク顕在化**（自己発行 NOC の node id が device ACL の admin と不一致、または同一 node id・別鍵 NOC を device が admin と認めない）。この場合の対処: (a) controller node id が本当に 112233 か確認（`materials.node_id`）、(b) device の ACL に我々の node id を追加する経路を chip-tool で試す（`chip-tool accesscontrol write acl ...`）、(c) それでも駄目なら spec の「未決事項」フォールバックに従い spec/ユーザーへエスカレーション。
- 証明書チェーン失敗 → Task 2 の to_tlv round-trip と Task 3 の issue_noc テストが緑である限り、KVS から読んだ root cert/key の取り違えを疑う。
- device 側ログの罠は `mat-discovery-timeout-misclassified` / `matd port9100` メモリ参照。

- [ ] **Step 6: ARCHITECTURE.md 更新**

Phase 5 の記述に M2/M2b 完了を 1〜2 行追記（既存 M1 記述のスタイルに合わせ、日本語）。例:「M2: CASE + IM read/invoke（fabric/kvs/cert/case/session/im）。M2b: chip-tool の永続 root CA 鍵で operational identity を自己発行し、ローカル all-clusters-app に対し CASE + onoff toggle/read の E2E 合格（`task e2e:m2`）。」

- [ ] **Step 7: `task check` → コミット**

```bash
task check
git add crates/mat-controller/tests/live_case_im.rs scripts/e2e-m2.sh Taskfile.yml ARCHITECTURE.md
git commit -m "feat(mat-controller): M2b live E2E — self-issued NOC CASE + onoff toggle/read

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

## Self-Review チェック済み事項

- **spec カバレッジ:** 決定1(root直署名2段NOC)→Task3 issue_noc(icac無し)/Task5 icac_tlv:None。決定2(node id=LocalNodeId継承)→Task4 node_id 読取(既定112233)/Task7 失敗時対処。決定3(自己検証)→Task5 verify_noc_chain 自己検証。cert TLVエンコーダ→Task2。CASEオフライン自己ハンドシェイク(必須)→Task6。受け入れ1〜6→Task7。持ち越し(validity/NocMissingIds/source/raw ECDSA昇格)→Task5/Task1。
- **未決事項の解消:** idx=0/node id=112233/EKU値順/validity/CA鍵生97B — 全て「SDK/KVS実測」節に転記済み、各Taskで使用。
- **型整合:** `SelfIssueMaterials`(kvs)→`from_self_issued`(fabric)→`case::establish(&creds)`(既存)→`SecureSession`。`issue_noc`のシグネチャはTask3で定義、Task5で使用。`sign_ecdsa_p256`/`verify_ecdsa_p256`はTask1定義、Task3/Task6使用。`subject_key_id`/`to_tlv`はTask2定義、Task3使用。
- **プレースホルダ:** responder harness(Task6)は構造+アサートレベル(暗号ステップは establish の鏡像として明記)。実コードはケースバイケースだが、全ステップに具体的な暗号手順・定数・材料を明示済み。
