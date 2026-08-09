# verify_noc_chain CA 制約検証 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** `verify_noc_chain` に Matter 運用証明書プロファイルの CA 制約検証を追加し、fabric 内ノードが自分の NOC を ICAC に仕立てて任意ノードへ成りすます攻撃（監査 Tier 1 ①）を塞ぐ。

**Architecture:** `crates/mat-controller/src/cert.rs` 1 ファイル完結。`MatterCert` にアクセサ 2 つ（`basic_constraints()` / `key_usage()`）を追加し、役割別チェック関数（`check_ca_cert` / `check_noc_leaf`）を `verify_noc_chain` 本体から呼ぶ。3 呼び出し経路（CASE Sigma2 / KVS ロード / self-issue 自己検証）全てに一様に効く。

**Tech Stack:** Rust。テストは cert.rs 内 `mod tests`（`use super::*;` で private 項目にアクセス可）。違反証明書は正規証明書の extensions を書き換えて `crypto::sign_ecdsa_p256` で再署名して作る。

**Spec:** `docs/superpowers/specs/2026-08-09-noc-chain-ca-constraints-design.md`

## Global Constraints

- コミット前に必ず `task check`（fmt:check + clippy + test）を通す。
- リポジトリは public。実 IP・実 node_id・実証明書をコミットしない（テストは fixture と乱数生成のみ）。
- エラーは既存 `CertError::Malformed(&'static str)` を流用。新 variant 追加禁止。`Display` が `"malformed certificate: missing/invalid {what}"` と前置するため、メッセージは名詞句にする（例: `"icac cA basic-constraint"`）。
- 既存の検査（署名連鎖 / DN 一致 / fabric-id 一致）とその順序・エラーメッセージは変えない。
- `issue_noc` / `generate_rcac` の発行内容は変えない（既にプロファイル準拠）。
- 現行バージョン 1.23.0。Task 4 で 1.24.0 へ bump。
- 作業ブランチ: `fix/tier1-noc-ca-constraints`（worktree 推奨、superpowers:using-git-worktrees）。

## 参照値（cert.rs 冒頭の既存 private 定数）

```rust
const EKU_CLIENT_AUTH: u64 = 2;                    // cert.rs:26
const EKU_SERVER_AUTH: u64 = 1;                    // cert.rs:27
const KEY_USAGE_DIGITAL_SIGNATURE: u16 = 0x0001;   // cert.rs:28
const KEY_USAGE_KEY_CERT_SIGN: u16 = 0x0020;       // cert.rs:29
const KEY_USAGE_CRL_SIGN: u16 = 0x0040;            // cert.rs:30
```

fixture（`crates/mat-controller/tests/fixtures/`、SDK 製・全てプロファイル準拠、privkey 付き）:
root01 = cA=true + keyCertSign|cRLSign(0x0060)、ica01 = 同、node01_01 = cA=false +
digitalSignature(0x0001) + EKU{clientAuth,serverAuth}。いずれも pathLen なし。

---

### Task 1: `MatterCert::basic_constraints()` / `key_usage()` アクセサ

**Files:**
- Modify: `crates/mat-controller/src/cert.rs`（`impl MatterCert` ブロック内、`node_id()` 等の隣。テストは末尾 `mod tests`）

**Interfaces:**
- Consumes: 既存 `MatterCert.extensions: Vec<CertExtension>`、`CertExtension::{BasicConstraints, KeyUsage}`
- Produces: `pub fn basic_constraints(&self) -> Option<(bool, Option<u8>)>` / `pub fn key_usage(&self) -> Option<u16>`（Task 2, 3 が使用）

- [ ] **Step 1: 失敗するテストを書く**

`mod tests` に追加（`ROOT_CHIP` / `NODE_CHIP` はテストモジュール既存の定数）:

```rust
#[test]
fn basic_constraints_and_key_usage_accessors() {
    let root = MatterCert::parse(ROOT_CHIP).unwrap();
    let node = MatterCert::parse(NODE_CHIP).unwrap();
    assert_eq!(root.basic_constraints(), Some((true, None)));
    assert_eq!(root.key_usage(), Some(0x0060)); // keyCertSign | cRLSign
    assert_eq!(node.basic_constraints(), Some((false, None)));
    assert_eq!(node.key_usage(), Some(0x0001)); // digitalSignature
}
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat-controller basic_constraints_and_key_usage_accessors`
Expected: コンパイルエラー（`no method named basic_constraints`）

- [ ] **Step 3: アクセサを実装**

`impl MatterCert` ブロック（`node_id()` などがある方）に追加:

```rust
/// basicConstraints 拡張の (is_ca, path_len)。拡張が無ければ None。
pub fn basic_constraints(&self) -> Option<(bool, Option<u8>)> {
    self.extensions.iter().find_map(|e| match e {
        CertExtension::BasicConstraints { is_ca, path_len } => Some((*is_ca, *path_len)),
        _ => None,
    })
}

/// KeyUsage 拡張のビット値（TLV named-bit、LSB=digitalSignature）。
/// 拡張が無ければ None。
pub fn key_usage(&self) -> Option<u16> {
    self.extensions.iter().find_map(|e| match e {
        CertExtension::KeyUsage(bits) => Some(*bits),
        _ => None,
    })
}
```

- [ ] **Step 4: テスト通過を確認**

Run: `cargo test -p mat-controller basic_constraints_and_key_usage_accessors`
Expected: PASS

- [ ] **Step 5: `task check` → コミット**

```bash
task check
git add crates/mat-controller/src/cert.rs
git commit -m "feat(mat-controller): MatterCert に basic_constraints/key_usage アクセサ"
```

---

### Task 2: 攻撃再現テスト + RCAC/ICAC の CA 制約検査

**Files:**
- Modify: `crates/mat-controller/src/cert.rs`（`verify_noc_chain` 本体 = cert.rs:478 付近、直前に `check_ca_cert` 追加。テストは `mod tests`）

**Interfaces:**
- Consumes: Task 1 の `basic_constraints()` / `key_usage()`、既存 `generate_rcac()` / `issue_noc()` / `crate::case::random_p256_secret()` / `crate::crypto::sign_ecdsa_p256(&[u8;32], &[u8]) -> Result<[u8;64], _>`
- Produces: `fn check_ca_cert(cert: &MatterCert, not_ca: &'static str, no_key_cert_sign: &'static str) -> Result<(), CertError>`、テストヘルパ `fn fresh_chain() -> (MatterCert, [u8; 32], MatterCert, [u8; 32])`（RCAC, root 鍵, NOC, NOC の運用鍵）と `fn mutate_and_resign(cert: &MatterCert, signer_key: &[u8; 32], f: impl FnOnce(&mut Vec<CertExtension>)) -> MatterCert`（Task 3 が再利用）
- エラーメッセージ（テストが exact match で固定）: `"rcac cA basic-constraint"` / `"rcac keyCertSign key-usage"` / `"icac cA basic-constraint"` / `"icac keyCertSign key-usage"` / `"icac path-len (must be 0)"` / `"rcac path-len (0 forbids an icac)"`

- [ ] **Step 1: テストヘルパ 2 つを `mod tests` に追加**

```rust
/// テスト用: 新規 RCAC とそこから発行した NOC、および双方の秘密鍵。
fn fresh_chain() -> (MatterCert, [u8; 32], MatterCert, [u8; 32]) {
    use p256::elliptic_curve::sec1::ToEncodedPoint;
    let (rcac, root_key) = generate_rcac().unwrap();
    let op = crate::case::random_p256_secret();
    let op_priv: [u8; 32] = op.to_bytes().into();
    let op_pub: [u8; 65] = op
        .public_key()
        .to_encoded_point(false)
        .as_bytes()
        .try_into()
        .unwrap();
    let noc = issue_noc(&op_pub, 0x1_0001, 0xFAB1, &rcac, &root_key, &[1]).unwrap();
    (rcac, root_key, noc, op_priv)
}

/// テスト用: 拡張リストを書き換えた複製を作り、指定鍵で署名し直す。
fn mutate_and_resign(
    cert: &MatterCert,
    signer_key: &[u8; 32],
    f: impl FnOnce(&mut Vec<CertExtension>),
) -> MatterCert {
    let mut c = cert.clone();
    f(&mut c.extensions);
    let tbs = c.tbs_der().unwrap();
    c.signature = crate::crypto::sign_ecdsa_p256(signer_key, &tbs).unwrap();
    c
}
```

- [ ] **Step 2: 攻撃再現テストを書く（修正前は verify が Ok を返して FAIL するのが脆弱性の証明）**

```rust
#[test]
fn rejects_forged_chain_with_noc_as_icac() {
    // 監査 Tier1① の攻撃再現: fabric 内ノード A が自分の NOC_A を ICAC に
    // 仕立て、A の運用鍵で偽 NOC_X（subject=node X, issuer=NOC_A.subject）を
    // 発行して積む。CA 制約検査が無いと署名・DN・fabric-id 全てを通過する。
    use p256::elliptic_curve::sec1::ToEncodedPoint;
    let (rcac, _root_key, noc_a, a_op_priv) = fresh_chain();
    let x = crate::case::random_p256_secret();
    let x_pub: [u8; 65] = x
        .public_key()
        .to_encoded_point(false)
        .as_bytes()
        .try_into()
        .unwrap();
    // issue_noc は issuer の subject を偽 NOC の issuer に写すので、
    // NOC_A を「発行者」に渡すだけで攻撃チェーンが組み上がる。
    let fake_noc = issue_noc(&x_pub, 0xBEEF, 0xFAB1, &noc_a, &a_op_priv, &[7]).unwrap();
    let err = verify_noc_chain(&fake_noc, Some(&noc_a), &rcac).unwrap_err();
    assert!(matches!(err, CertError::Malformed("icac cA basic-constraint")));
}
```

- [ ] **Step 3: RCAC / ICAC の個別違反テストを書く**

```rust
#[test]
fn rejects_rcac_without_ca_constraints() {
    let (rcac, root_key, noc, _) = fresh_chain();

    // cA=false の RCAC（noc の署名は rcac 鍵のままなので CA 検査だけで落ちる）
    let not_ca = mutate_and_resign(&rcac, &root_key, |exts| {
        for e in exts.iter_mut() {
            if let CertExtension::BasicConstraints { is_ca, .. } = e {
                *is_ca = false;
            }
        }
    });
    let err = verify_noc_chain(&noc, None, &not_ca).unwrap_err();
    assert!(matches!(err, CertError::Malformed("rcac cA basic-constraint")));

    // keyCertSign を落とした RCAC
    let no_sign = mutate_and_resign(&rcac, &root_key, |exts| {
        for e in exts.iter_mut() {
            if let CertExtension::KeyUsage(bits) = e {
                *bits = KEY_USAGE_CRL_SIGN;
            }
        }
    });
    let err = verify_noc_chain(&noc, None, &no_sign).unwrap_err();
    assert!(matches!(err, CertError::Malformed("rcac keyCertSign key-usage")));

    // basicConstraints 拡張ごと欠落も拒否（fail-closed）
    let bc_missing = mutate_and_resign(&rcac, &root_key, |exts| {
        exts.retain(|e| !matches!(e, CertExtension::BasicConstraints { .. }));
    });
    let err = verify_noc_chain(&noc, None, &bc_missing).unwrap_err();
    assert!(matches!(err, CertError::Malformed("rcac cA basic-constraint")));
}

#[test]
fn rejects_icac_constraint_violations() {
    let root = MatterCert::parse(ROOT_CHIP).unwrap();
    let ica = MatterCert::parse(ICA_CHIP).unwrap();
    let node = MatterCert::parse(NODE_CHIP).unwrap();
    let root_priv: [u8; 32] = include_bytes!("../tests/fixtures/root01_privkey.bin")
        .as_slice()
        .try_into()
        .unwrap();

    // cA=false の ICAC
    let not_ca = mutate_and_resign(&ica, &root_priv, |exts| {
        for e in exts.iter_mut() {
            if let CertExtension::BasicConstraints { is_ca, .. } = e {
                *is_ca = false;
            }
        }
    });
    let err = verify_noc_chain(&node, Some(&not_ca), &root).unwrap_err();
    assert!(matches!(err, CertError::Malformed("icac cA basic-constraint")));

    // keyCertSign を落とした ICAC
    let no_sign = mutate_and_resign(&ica, &root_priv, |exts| {
        for e in exts.iter_mut() {
            if let CertExtension::KeyUsage(bits) = e {
                *bits = KEY_USAGE_CRL_SIGN;
            }
        }
    });
    let err = verify_noc_chain(&node, Some(&no_sign), &root).unwrap_err();
    assert!(matches!(err, CertError::Malformed("icac keyCertSign key-usage")));

    // pathLen=1 の ICAC（Matter プロファイル: 存在するなら 0 のみ）
    let deep = mutate_and_resign(&ica, &root_priv, |exts| {
        for e in exts.iter_mut() {
            if let CertExtension::BasicConstraints { path_len, .. } = e {
                *path_len = Some(1);
            }
        }
    });
    let err = verify_noc_chain(&node, Some(&deep), &root).unwrap_err();
    assert!(matches!(err, CertError::Malformed("icac path-len (must be 0)")));

    // RCAC pathLen=0 なのに ICAC 付きチェーン（RFC 5280 §6.1.4(m)）
    let shallow_root = mutate_and_resign(&root, &root_priv, |exts| {
        for e in exts.iter_mut() {
            if let CertExtension::BasicConstraints { path_len, .. } = e {
                *path_len = Some(0);
            }
        }
    });
    let err = verify_noc_chain(&node, Some(&ica), &shallow_root).unwrap_err();
    assert!(matches!(
        err,
        CertError::Malformed("rcac path-len (0 forbids an icac)")
    ));

    // pathLen=0 自体は 2-cert チェーンでは合法
    let op_pub: [u8; 65] = include_bytes!("../tests/fixtures/node01_01_pubkey.bin")
        .as_slice()
        .try_into()
        .unwrap();
    let direct = issue_noc(&op_pub, 0x1B669, 1, &shallow_root, &root_priv, &[9]).unwrap();
    verify_noc_chain(&direct, None, &shallow_root).unwrap();
}
```

- [ ] **Step 4: 失敗を確認**

Run: `cargo test -p mat-controller rejects_forged_chain rejects_rcac rejects_icac`
（`cargo test` は複数フィルタを取れないので 3 回に分けるか `cargo test -p mat-controller rejects_` で一括）
Expected: FAIL — `rejects_forged_chain_with_noc_as_icac` は `unwrap_err()` パニック（verify が Ok を返す = 脆弱性の再現）。他 2 本も同様。

- [ ] **Step 5: `check_ca_cert` を実装し `verify_noc_chain` に組み込む**

`verify_noc_chain`（cert.rs:478）の直前に追加:

```rust
/// RCAC / ICAC 共通の CA 制約検査: basicConstraints cA=true + KeyUsage
/// keyCertSign（RFC 5280 §6.1.4(k)/(n)、Matter 1.4 §6.6.4 が MUST 参照）。
/// Matter 運用証明書では両拡張とも必須なので、欠落も拒否（fail-closed）。
fn check_ca_cert(
    cert: &MatterCert,
    not_ca: &'static str,
    no_key_cert_sign: &'static str,
) -> Result<(), CertError> {
    match cert.basic_constraints() {
        Some((true, _)) => {}
        _ => return Err(CertError::Malformed(not_ca)),
    }
    match cert.key_usage() {
        Some(bits) if bits & KEY_USAGE_KEY_CERT_SIGN != 0 => {}
        _ => return Err(CertError::Malformed(no_key_cert_sign)),
    }
    Ok(())
}
```

`verify_noc_chain` 本体を次の形にする（既存検査は文言・順序とも不変、追記のみ）:

```rust
pub fn verify_noc_chain(
    noc: &MatterCert,
    icac: Option<&MatterCert>,
    rcac: &MatterCert,
) -> Result<(), CertError> {
    rcac.verify_signed_by(&rcac.pub_key)?; // self-signed root
    check_ca_cert(
        rcac,
        "rcac cA basic-constraint",
        "rcac keyCertSign key-usage",
    )?;
    let signer = match icac {
        Some(ica) => {
            ica.verify_signed_by(&rcac.pub_key)?;
            if ica.issuer != rcac.subject {
                return Err(CertError::Malformed("icac issuer != rcac subject"));
            }
            check_ca_cert(
                ica,
                "icac cA basic-constraint",
                "icac keyCertSign key-usage",
            )?;
            // Matter プロファイル: ICAC の pathLen は存在するなら 0 のみ。
            if let Some((_, Some(pl))) = ica.basic_constraints() {
                if pl != 0 {
                    return Err(CertError::Malformed("icac path-len (must be 0)"));
                }
            }
            // RFC 5280 §6.1.4(m): root の pathLen=0 は中間 CA を許さない。
            if let Some((_, Some(0))) = rcac.basic_constraints() {
                return Err(CertError::Malformed("rcac path-len (0 forbids an icac)"));
            }
            ica
        }
        None => rcac,
    };
    noc.verify_signed_by(&signer.pub_key)?;
    if noc.issuer != signer.subject {
        return Err(CertError::Malformed("noc issuer != signer subject"));
    }
    if noc.node_id().is_none() || noc.fabric_id().is_none() {
        return Err(CertError::Malformed("noc missing node/fabric id"));
    }
    if let Some(ica) = icac {
        if let (Some(a), Some(b)) = (ica.fabric_id(), noc.fabric_id()) {
            if a != b {
                return Err(CertError::Malformed("fabric id mismatch in chain"));
            }
        }
    }
    Ok(())
}
```

- [ ] **Step 6: テスト通過を確認（既存テスト含む）**

Run: `cargo test -p mat-controller --lib`
Expected: 全 PASS（新 3 本 + 既存の `verifies_signatures_and_chain` / `generate_rcac_is_self_signed_and_issues_valid_noc` / `issue_noc_produces_chain_valid_cert` が回帰ガード）

- [ ] **Step 7: `task check` → コミット**

```bash
task check
git add crates/mat-controller/src/cert.rs
git commit -m "fix(mat-controller): verify_noc_chain に RCAC/ICAC の CA 制約検証を追加（監査 Tier 1 ①）"
```

---

### Task 3: NOC リーフ制約検査（`check_noc_leaf`）

**Files:**
- Modify: `crates/mat-controller/src/cert.rs`（`check_ca_cert` の隣に `check_noc_leaf` 追加、`verify_noc_chain` に 1 行挿入。テストは `mod tests`）

**Interfaces:**
- Consumes: Task 1 のアクセサ、Task 2 のテストヘルパ `fresh_chain()` / `mutate_and_resign(cert, signer_key, f)`、既存定数 `EKU_CLIENT_AUTH`(=2) / `EKU_SERVER_AUTH`(=1) / `KEY_USAGE_*`
- Produces: `fn check_noc_leaf(noc: &MatterCert) -> Result<(), CertError>`
- エラーメッセージ（exact match）: `"noc end-entity basic-constraint"` / `"noc key-usage"` / `"noc digitalSignature key-usage"` / `"noc key-usage (keyCertSign/cRLSign set)"` / `"noc clientAuth+serverAuth extended-key-usage"`

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test]
fn rejects_noc_leaf_constraint_violations() {
    let (rcac, root_key, noc, _) = fresh_chain();

    // cA=true の NOC
    let ca_noc = mutate_and_resign(&noc, &root_key, |exts| {
        for e in exts.iter_mut() {
            if let CertExtension::BasicConstraints { is_ca, .. } = e {
                *is_ca = true;
            }
        }
    });
    let err = verify_noc_chain(&ca_noc, None, &rcac).unwrap_err();
    assert!(matches!(
        err,
        CertError::Malformed("noc end-entity basic-constraint")
    ));

    // basicConstraints 欠落
    let bc_missing = mutate_and_resign(&noc, &root_key, |exts| {
        exts.retain(|e| !matches!(e, CertExtension::BasicConstraints { .. }));
    });
    let err = verify_noc_chain(&bc_missing, None, &rcac).unwrap_err();
    assert!(matches!(
        err,
        CertError::Malformed("noc end-entity basic-constraint")
    ));

    // keyUsage 欠落
    let ku_missing = mutate_and_resign(&noc, &root_key, |exts| {
        exts.retain(|e| !matches!(e, CertExtension::KeyUsage(_)));
    });
    let err = verify_noc_chain(&ku_missing, None, &rcac).unwrap_err();
    assert!(matches!(err, CertError::Malformed("noc key-usage")));

    // digitalSignature 無し
    let no_ds = mutate_and_resign(&noc, &root_key, |exts| {
        for e in exts.iter_mut() {
            if let CertExtension::KeyUsage(bits) = e {
                *bits = KEY_USAGE_CRL_SIGN;
            }
        }
    });
    let err = verify_noc_chain(&no_ds, None, &rcac).unwrap_err();
    assert!(matches!(
        err,
        CertError::Malformed("noc digitalSignature key-usage")
    ));

    // keyCertSign が立っている（digitalSignature があっても拒否）
    let cert_sign = mutate_and_resign(&noc, &root_key, |exts| {
        for e in exts.iter_mut() {
            if let CertExtension::KeyUsage(bits) = e {
                *bits = KEY_USAGE_DIGITAL_SIGNATURE | KEY_USAGE_KEY_CERT_SIGN;
            }
        }
    });
    let err = verify_noc_chain(&cert_sign, None, &rcac).unwrap_err();
    assert!(matches!(
        err,
        CertError::Malformed("noc key-usage (keyCertSign/cRLSign set)")
    ));

    // EKU 欠落
    let eku_missing = mutate_and_resign(&noc, &root_key, |exts| {
        exts.retain(|e| !matches!(e, CertExtension::ExtendedKeyUsage(_)));
    });
    let err = verify_noc_chain(&eku_missing, None, &rcac).unwrap_err();
    assert!(matches!(
        err,
        CertError::Malformed("noc clientAuth+serverAuth extended-key-usage")
    ));

    // EKU に serverAuth だけ（clientAuth 欠け）
    let eku_partial = mutate_and_resign(&noc, &root_key, |exts| {
        for e in exts.iter_mut() {
            if let CertExtension::ExtendedKeyUsage(v) = e {
                *v = vec![EKU_SERVER_AUTH];
            }
        }
    });
    let err = verify_noc_chain(&eku_partial, None, &rcac).unwrap_err();
    assert!(matches!(
        err,
        CertError::Malformed("noc clientAuth+serverAuth extended-key-usage")
    ));

    // 正常系: 無改変チェーンは引き続き通る
    verify_noc_chain(&noc, None, &rcac).unwrap();
}
```

- [ ] **Step 2: 失敗を確認**

Run: `cargo test -p mat-controller rejects_noc_leaf_constraint_violations`
Expected: FAIL（最初の `unwrap_err()` で panic — cA=true の NOC が現状は通ってしまう）

- [ ] **Step 3: `check_noc_leaf` を実装し `verify_noc_chain` に挿入**

`check_ca_cert` の直後に追加:

```rust
/// NOC（リーフ）の Matter プロファイル検査: cA=false、KeyUsage は
/// digitalSignature 必須かつ keyCertSign/cRLSign 禁止、EKU は
/// clientAuth と serverAuth の両方を含むこと（Matter 1.4 §6.5）。
/// 拡張欠落は拒否（fail-closed）。
fn check_noc_leaf(noc: &MatterCert) -> Result<(), CertError> {
    match noc.basic_constraints() {
        Some((false, _)) => {}
        _ => return Err(CertError::Malformed("noc end-entity basic-constraint")),
    }
    let Some(bits) = noc.key_usage() else {
        return Err(CertError::Malformed("noc key-usage"));
    };
    if bits & KEY_USAGE_DIGITAL_SIGNATURE == 0 {
        return Err(CertError::Malformed("noc digitalSignature key-usage"));
    }
    if bits & (KEY_USAGE_KEY_CERT_SIGN | KEY_USAGE_CRL_SIGN) != 0 {
        return Err(CertError::Malformed("noc key-usage (keyCertSign/cRLSign set)"));
    }
    let eku = noc.extensions.iter().find_map(|e| match e {
        CertExtension::ExtendedKeyUsage(v) => Some(v.as_slice()),
        _ => None,
    });
    match eku {
        Some(v) if v.contains(&EKU_CLIENT_AUTH) && v.contains(&EKU_SERVER_AUTH) => Ok(()),
        _ => Err(CertError::Malformed(
            "noc clientAuth+serverAuth extended-key-usage",
        )),
    }
}
```

`verify_noc_chain` の `noc.issuer != signer.subject` チェックの直後（`noc.node_id().is_none()` チェックの直前）に 1 行挿入:

```rust
    check_noc_leaf(noc)?;
```

- [ ] **Step 4: テスト通過を確認（既存テスト含む）**

Run: `cargo test -p mat-controller --lib`
Expected: 全 PASS

- [ ] **Step 5: workspace 全体の回帰を確認**

Run: `cargo test --workspace`
Expected: 全 PASS — 特に `case_self_handshake.rs`（SDK fixture の ICAC 付きチェーン）と fabric 系テスト（KVS ロード / self-issue 経路）が新検査を通過すること。

- [ ] **Step 6: `task check` → コミット**

```bash
task check
git add crates/mat-controller/src/cert.rs
git commit -m "fix(mat-controller): verify_noc_chain に NOC リーフ制約検証を追加（監査 Tier 1 ①）"
```

---

### Task 4: バージョン bump 1.24.0

**Files:**
- Modify: `Cargo.toml`（workspace `version = "1.23.0"` → `"1.24.0"`）、`Cargo.lock`（cargo が自動更新）

**Interfaces:**
- Consumes: Task 2, 3 の実装が完了していること
- Produces: リリース可能な 1.24.0（実機 E2E は Task 5）

- [ ] **Step 1: Cargo.toml の version を 1.24.0 に変更**

`Cargo.toml:6` の `version = "1.23.0"` を `version = "1.24.0"` に。

- [ ] **Step 2: Cargo.lock を追従させる**

Run: `cargo check --workspace`
Expected: 成功（Cargo.lock の自 crate エントリが 1.24.0 に更新される）

- [ ] **Step 3: `task check` → コミット**

```bash
task check
git add Cargo.toml Cargo.lock
git commit -m "chore: 1.24.0（verify_noc_chain CA 制約検証 — commission/証明書監査 Tier 1 ①）"
```

---

### Task 5: 実機 E2E（マージ前ゲート — メインセッションで実施）

subagent ではなくメインセッションで despliegue スキルの手順に従って行う。
目的: Sigma2 のピア NOC チェーン検証が**実発行物**（mat 発行 + chip-tool 採用 fabric）で
引き続き通過することの確認。

- [ ] **Step 1: arm64 バイナリをビルド**

Run: `task dist:arm64`

- [ ] **Step 2: jarvis へ `mat.new` として転送（本番は置換しない）**

[[e2e-before-merge]] / [[jarvis-matd-deploy]] の隔離手順どおり。

- [ ] **Step 3: 実デバイスへ read 1 発**

jarvis 上で（node id は実運用の値を使う — このドキュメントには書かない）:

```bash
MAT_FABRIC_INDEX=2 ./mat.new read onoff on-off <node-id> 1
```

Expected: `timestamp` / `value` 入りの JSON が stdout に出る（= CASE 成立 =
新しい CA 制約検査を実チェーンが通過）。stderr に WARN が無いこと。

- [ ] **Step 4: 合格したらマージへ**

superpowers:finishing-a-development-branch に従い main へマージ。
`*.new` の昇格は次回デプロイ同乗（既運用どおり）。
