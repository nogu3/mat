# Tier 4/5: attestation/x509 検証強化 + テスト層ギャップ 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** commission/証明書監査（2026-08-06）の残り Tier 4（attestation/x509、5 件）+ Tier 5（テスト層、4 件）を消化し、監査バックログを全消化する。

**Architecture:** Tier 4 は `crates/mat-controller` の `x509.rs`（KeyUsage パース）と `attestation.rs`（strict 検査 3 種 + parse_elements 深さ + CMS messageDigest 結合）。Tier 5 は `test_support.rs` への PASE 応答器（SPAKE2+ verifier 役）+ 新統合テスト、e2e スクリプト 2 本の是正、docs.yml の checksum。spec = `docs/superpowers/specs/2026-08-15-tier45-attestation-test-gaps-design.md`。

**Tech Stack:** Rust（p256 / sha2 / hkdf / hmac、自前 DER/TLV）、bash、GitHub Actions YAML。

## Global Constraints

- バージョンは **1.28.0**（Task 8 で bump。それまでの Task は触らない）。
- CD 検証は **warn-only を維持**（2026-07-13 ユーザー決定）— Task 5 は warn 経路内の品質改善であり、`verify_device_attestation` の戻り値を CD 起因で Err にしてはならない。
- コミット前に `task check`（fmt:check + clippy + test）。各 Task 末尾のコミットは Task 単位。
- コミットメッセージは日本語・既存流儀（`fix(attestation): ...` など）。末尾に
  `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>` と
  `Claude-Session: https://claude.ai/code/session_019Yy2VNmAaT2Jk7vNmyMkVu` を付ける。
- このリポジトリは public — 秘密情報・実 IP・実 node id をテスト/ドキュメントに書かない。
- 秘密値を持つ構造体に `Debug` を derive しない（spake2p.rs / fabric.rs の既存方針）。

---

### Task 1: x509.rs — KeyUsage 拡張のパース

**Files:**
- Modify: `crates/mat-controller/src/x509.rs`（X509Cert 構造体 ~line 60-80、parse_extensions ループ ~line 220-251、test_support mod ~line 462-665、tests mod）
- Modify: `crates/mat-controller/src/cert.rs`（`key_usage_bits` と KU 定数 3 つを `pub(crate)` 化のみ）
- Modify: `crates/mat-controller/src/attestation.rs`（`make_test_cert_ext` の既存呼び出し 3 箇所に引数追加）

**Interfaces:**
- Produces: `X509Cert.key_usage: Option<u16>`（RFC 5280 named-bit を LSB=digitalSignature の u16 に正規化。cert.rs の `KEY_USAGE_DIGITAL_SIGNATURE=0x0001` / `KEY_USAGE_KEY_CERT_SIGN=0x0020` / `KEY_USAGE_CRL_SIGN=0x0040` と同じビット割当）。
- Produces: `make_test_cert_ext(..., key_usage: Option<u16>)`（末尾に新引数。None = 拡張なし）。`make_test_cert` は従来シグネチャのまま、既定で CA に `0x0060`（keyCertSign|cRLSign）、leaf に `0x0001`（digitalSignature）を発行する。
- Produces: cert.rs の `pub(crate) const KEY_USAGE_DIGITAL_SIGNATURE / KEY_USAGE_KEY_CERT_SIGN / KEY_USAGE_CRL_SIGN` と `pub(crate) fn key_usage_bits`（Task 2 と test_support が使う）。

- [ ] **Step 1: 失敗するテストを書く** — x509.rs の tests mod に追加:

```rust
#[test]
fn parses_key_usage_from_test_certs() {
    let key = random_p256_secret();
    // make_test_cert: CA には keyCertSign|cRLSign、leaf には digitalSignature が既定で付く
    let ca = parse_x509(&make_test_cert(b"root", b"root", &key, &key, true, None)).unwrap();
    assert_eq!(ca.key_usage, Some(0x0060));
    let leaf_der = make_test_cert(b"leaf", b"root", &random_p256_secret(), &key, false, None);
    let leaf = parse_x509(&leaf_der).unwrap();
    assert_eq!(leaf.key_usage, Some(0x0001));
}
```

既存の SDK DER フィクスチャテスト（`sdk_fixtures_expose_is_ca_and_validity` 系）にも key_usage アサートを追加する（Root/PAA 相当は `key_usage.unwrap() & 0x0020 != 0` を確認。厳密な期待値は Step 4 で実物のパース結果を `openssl x509 -text -noout -in <fixture>` で確認して固定する）。

- [ ] **Step 2: テストが失敗することを確認** — `cargo test -p mat-controller parses_key_usage` → コンパイルエラー（`key_usage` フィールドなし）を確認。

- [ ] **Step 3: 実装**

x509.rs — OID 定数（既存 OID 定数群の隣、内容バイトのみの既存形式に合わせる）:

```rust
const OID_KEY_USAGE: &[u8] = &[0x55, 0x1D, 0x0F]; // 2.5.29.15
```

`X509Cert` に `pub key_usage: Option<u16>,` を追加。parse_extensions ループに分岐追加:

```rust
} else if oid_bytes == OID_KEY_USAGE {
    key_usage = Some(parse_key_usage(value)?);
}
```

パース関数（DER BIT STRING → LSB=digitalSignature の u16。bit 8 = decipherOnly まで見れば十分）:

```rust
/// KeyUsage 拡張値（DER BIT STRING）を LSB=digitalSignature の named-bit u16 に
/// 正規化する（cert.rs の KEY_USAGE_* 定数と同じビット割当）。
fn parse_key_usage(value: &[u8]) -> Result<u16, X509Error> {
    let mut vr = DerReader::new(value);
    let bits = vr.expect(0x03)?;
    if bits.is_empty() {
        return Err(X509Error::Der("empty keyUsage bit string"));
    }
    let mut out = 0u16;
    for i in 0..9usize {
        let byte = 1 + i / 8;
        if byte < bits.len() && bits[byte] & (0x80 >> (i % 8)) != 0 {
            out |= 1 << i;
        }
    }
    Ok(out)
}
```

cert.rs — 定数 3 つ（line 28-30）と `key_usage_bits`（line 805）を `pub(crate)` に。

x509.rs test_support — `make_test_cert_ext` の末尾に `key_usage: Option<u16>` を追加し、ext_items 組み立てで:

```rust
if let Some(bits) = key_usage {
    ext_items.push(key_usage_ext(bits));
}
```

```rust
/// KeyUsage 拡張（critical、RFC 5280 §4.2.1.3）。ビット割当は cert.rs の
/// KEY_USAGE_*（LSB=digitalSignature）。
fn key_usage_ext(bits: u16) -> Vec<u8> {
    let (unused, bytes) = crate::cert::key_usage_bits(bits);
    let value = asn1::bit_string(unused, &bytes);
    asn1::seq(&[
        &asn1::oid(super::OID_KEY_USAGE),
        &asn1::boolean(true),
        &asn1::octet_string(&value),
    ])
}
```

（`OID_KEY_USAGE` を test_support の `use super::{...}` に追加。）`make_test_cert` は委譲時に `if is_ca { Some(0x0060) } else { Some(0x0001) }` を渡す。

attestation.rs の既存 `make_test_cert_ext` 呼び出し 3 箇所（`rejects_dac_pai_vid_mismatch` / `rejects_pai_without_ca_flag` / `rejects_dac_with_ca_flag`）に末尾引数を追加する。検証対象分岐を変えないよう、DAC 相当には `Some(0x0001)`、PAI 相当（`rejects_pai_without_ca_flag` の fake_pai）には `Some(0x0001)`（cA 検査が keyUsage 検査より先に落ちるので影響しない）、`rejects_dac_with_ca_flag` の DAC には `Some(0x0001)` を渡す。

- [ ] **Step 4: テスト実行** — `cargo test -p mat-controller x509` と `cargo test -p mat-controller attestation` が全緑。SDK フィクスチャの key_usage 実値を確認してアサートを固定。

- [ ] **Step 5: コミット** — `git add` 対象 3 ファイル、`feat(x509): KeyUsage 拡張をパース（監査 Tier4 前段）`。

---

### Task 2: attestation.rs — KeyUsage 検査（strict）

**Files:**
- Modify: `crates/mat-controller/src/attestation.rs`（`verify_device_attestation` の cA 検査ブロック直後 ~line 160、tests mod）

**Interfaces:**
- Consumes: Task 1 の `X509Cert.key_usage`、cert.rs の `pub(crate)` KU 定数、`make_test_cert_ext(..., key_usage)`。
- Produces: 新 Chain エラー文字列 `"pai keyusage missing keycertsign"` / `"paa keyusage missing keycertsign"` / `"dac keyusage missing digitalsignature"` / `"dac keyusage must not sign certificates"`。

- [ ] **Step 1: 失敗するテストを書く** — attestation.rs tests mod に追加。フィクスチャは既存テスト（`rejects_pai_without_ca_flag` 等）の流儀を踏襲し、検証対象分岐より手前のチェックをすべて通す形に組む:

```rust
#[test]
fn rejects_pai_without_keycertsign() {
    let paa_key = random_p256_secret();
    let pai_key = random_p256_secret();
    let dac_key = random_p256_secret();
    let paa = make_test_cert(b"paa", b"paa", &paa_key, &paa_key, true, None);
    // PAI: cA=true だが keyUsage は digitalSignature のみ（keyCertSign なし）
    let pai = make_test_cert_ext(
        b"pai", b"paa", &pai_key, &paa_key, true,
        None, Some((0xFFF1, 0x8001)), Some(0x0001),
    );
    let dac = make_test_cert(b"dac", b"pai", &dac_key, &pai_key, false, Some((0xFFF1, 0x8001)));
    let nonce = [5u8; 32];
    let challenge = [6u8; 16];
    let el = elements(&nonce);
    let priv_bytes: [u8; 32] = dac_key.to_bytes().into();
    let mut msg = el.clone();
    msg.extend_from_slice(&challenge);
    let sig = sign_ecdsa_p256(&priv_bytes, &msg).unwrap();
    let err = verify_device_attestation(
        &dac, &pai, std::slice::from_ref(&paa), &[], &el, &sig, &nonce, &challenge,
    ).unwrap_err();
    assert!(matches!(err, AttestationError::Chain("pai keyusage missing keycertsign")));
}

#[test]
fn rejects_dac_with_certsign_keyusage() {
    let paa_key = random_p256_secret();
    let pai_key = random_p256_secret();
    let dac_key = random_p256_secret();
    let paa = make_test_cert(b"paa", b"paa", &paa_key, &paa_key, true, None);
    let pai = make_test_cert(b"pai", b"paa", &pai_key, &paa_key, true, Some((0xFFF1, 0x8001)));
    // DAC: cA なしだが keyUsage に keyCertSign — 証明書に署名できる leaf は拒否
    let dac = make_test_cert_ext(
        b"dac", b"pai", &dac_key, &pai_key, false,
        Some((0xFFF1, 0x8001)), Some((0xFFF1, 0x8001)), Some(0x0021), // digitalSignature|keyCertSign
    );
    let nonce = [5u8; 32];
    let challenge = [6u8; 16];
    let el = elements(&nonce);
    let priv_bytes: [u8; 32] = dac_key.to_bytes().into();
    let mut msg = el.clone();
    msg.extend_from_slice(&challenge);
    let sig = sign_ecdsa_p256(&priv_bytes, &msg).unwrap();
    let err = verify_device_attestation(
        &dac, &pai, std::slice::from_ref(&paa), &[], &el, &sig, &nonce, &challenge,
    ).unwrap_err();
    assert!(matches!(err, AttestationError::Chain("dac keyusage must not sign certificates")));
}

#[test]
fn accepts_dac_without_keyusage_extension() {
    // leaf の keyUsage 欠落は許容（is_ca の DAC 側の寛容と同じ精神）
    let paa_key = random_p256_secret();
    let pai_key = random_p256_secret();
    let dac_key = random_p256_secret();
    let paa = make_test_cert(b"paa", b"paa", &paa_key, &paa_key, true, None);
    let pai = make_test_cert(b"pai", b"paa", &pai_key, &paa_key, true, Some((0xFFF1, 0x8001)));
    let dac = make_test_cert_ext(
        b"dac", b"pai", &dac_key, &pai_key, false,
        Some((0xFFF1, 0x8001)), Some((0xFFF1, 0x8001)), None, // keyUsage 拡張なし
    );
    let nonce = [5u8; 32];
    let challenge = [6u8; 16];
    let el = elements(&nonce);
    let priv_bytes: [u8; 32] = dac_key.to_bytes().into();
    let mut msg = el.clone();
    msg.extend_from_slice(&challenge);
    let sig = sign_ecdsa_p256(&priv_bytes, &msg).unwrap();
    verify_device_attestation(
        &dac, &pai, std::slice::from_ref(&paa), &[], &el, &sig, &nonce, &challenge,
    ).unwrap();
}
```

（`rejects_paa_without_keycertsign` も同型で 1 本: PAA を `make_test_cert_ext(b"paa", b"paa", &paa_key, &paa_key, true, None, None, Some(0x0001))` にして `"paa keyusage missing keycertsign"` を固定。）

- [ ] **Step 2: テストが失敗することを確認** — `cargo test -p mat-controller keyusage` → 新テストが FAIL（現状は検査がないので `unwrap_err` が panic / メッセージ不一致）。

- [ ] **Step 3: 実装** — cA 検査ブロック（`dac must not be a ca certificate` の return）の直後に:

```rust
    // --- チェーン（厳格）: KeyUsage（spec §6.2.2.1 / RFC 5280 §4.2.1.3）---
    // CA（PAI/PAA）は keyCertSign を持つ keyUsage 拡張が必須。DAC は拡張が
    // あるなら digitalSignature 必須かつ証明書署名ビット禁止（拡張なしは
    // is_ca の DAC 側と同じく許容 — leaf の欠落で commissioning を割らない）。
    use crate::cert::{KEY_USAGE_CRL_SIGN, KEY_USAGE_DIGITAL_SIGNATURE, KEY_USAGE_KEY_CERT_SIGN};
    match pai.key_usage {
        Some(bits) if bits & KEY_USAGE_KEY_CERT_SIGN != 0 => {}
        _ => return Err(AttestationError::Chain("pai keyusage missing keycertsign")),
    }
    match paa.key_usage {
        Some(bits) if bits & KEY_USAGE_KEY_CERT_SIGN != 0 => {}
        _ => return Err(AttestationError::Chain("paa keyusage missing keycertsign")),
    }
    if let Some(bits) = dac.key_usage {
        if bits & KEY_USAGE_DIGITAL_SIGNATURE == 0 {
            return Err(AttestationError::Chain("dac keyusage missing digitalsignature"));
        }
        if bits & (KEY_USAGE_KEY_CERT_SIGN | KEY_USAGE_CRL_SIGN) != 0 {
            return Err(AttestationError::Chain("dac keyusage must not sign certificates"));
        }
    }
```

（`use` はファイル先頭の import 群へ移動して良い。）

- [ ] **Step 4: テスト実行** — `cargo test -p mat-controller attestation` 全緑（既存の accepts_valid_attestation は Task 1 の既定 keyUsage 発行で通るはず）。

- [ ] **Step 5: コミット** — `fix(attestation): DAC/PAI/PAA の KeyUsage を検証（監査 Tier4）`。

---

### Task 3: attestation.rs — VID スコープ PAA / DAC PID 必須（strict）

**Files:**
- Modify: `crates/mat-controller/src/attestation.rs`（VID/PID 整合ブロック ~line 137-147、tests mod）

**Interfaces:**
- Consumes: `X509Cert.vid` / `X509Cert.pid`（既存）、`make_test_cert_ext(..., key_usage)`（Task 1）。
- Produces: 新 Chain エラー文字列 `"vid-scoped paa/pai vid mismatch"` / `"dac missing pid"`。

- [ ] **Step 1: 失敗するテストを書く**:

```rust
#[test]
fn rejects_vid_scoped_paa_with_mismatched_pai_vid() {
    let paa_key = random_p256_secret();
    let pai_key = random_p256_secret();
    let dac_key = random_p256_secret();
    // VID スコープ PAA（subject に Mvid FFF2）。PAI の issuer Name はこれと
    // バイト一致させ、PAI 自身の subject VID は FFF1 — 不一致が検証対象。
    let paa = make_test_cert_ext(
        b"paa", b"paa", &paa_key, &paa_key, true,
        Some((0xFFF2, 0x8001)), Some((0xFFF2, 0x8001)), Some(0x0060),
    );
    let pai = make_test_cert_ext(
        b"pai", b"paa", &pai_key, &paa_key, true,
        Some((0xFFF2, 0x8001)), Some((0xFFF1, 0x8001)), Some(0x0060),
    );
    let dac = make_test_cert(b"dac", b"pai", &dac_key, &pai_key, false, Some((0xFFF1, 0x8001)));
    let nonce = [5u8; 32];
    let challenge = [6u8; 16];
    let el = elements(&nonce);
    let priv_bytes: [u8; 32] = dac_key.to_bytes().into();
    let mut msg = el.clone();
    msg.extend_from_slice(&challenge);
    let sig = sign_ecdsa_p256(&priv_bytes, &msg).unwrap();
    let err = verify_device_attestation(
        &dac, &pai, std::slice::from_ref(&paa), &[], &el, &sig, &nonce, &challenge,
    ).unwrap_err();
    assert!(matches!(err, AttestationError::Chain("vid-scoped paa/pai vid mismatch")));
}

#[test]
fn rejects_dac_without_pid() {
    let paa_key = random_p256_secret();
    let pai_key = random_p256_secret();
    let dac_key = random_p256_secret();
    let paa = make_test_cert(b"paa", b"paa", &paa_key, &paa_key, true, None);
    let pai = make_test_cert(b"pai", b"paa", &pai_key, &paa_key, true, Some((0xFFF1, 0x8001)));
    // DAC の subject は CN 埋め込みの Mvid のみ（Matter PID RDN も Mpid: も
    // 無し）— parse_vid_pid は vid=FFF1 / pid=None を返す。issuer Name は
    // PAI subject（vid_pid 入り）にバイト一致させる。
    let dac = make_test_cert_ext(
        b"dac Mvid:FFF1", b"pai", &dac_key, &pai_key, false,
        Some((0xFFF1, 0x8001)), None, Some(0x0001),
    );
    let nonce = [5u8; 32];
    let challenge = [6u8; 16];
    let el = elements(&nonce);
    let priv_bytes: [u8; 32] = dac_key.to_bytes().into();
    let mut msg = el.clone();
    msg.extend_from_slice(&challenge);
    let sig = sign_ecdsa_p256(&priv_bytes, &msg).unwrap();
    let err = verify_device_attestation(
        &dac, &pai, std::slice::from_ref(&paa), &[], &el, &sig, &nonce, &challenge,
    ).unwrap_err();
    assert!(matches!(err, AttestationError::Chain("dac missing pid")));
}
```

- [ ] **Step 2: テストが失敗することを確認** — `rejects_dac_without_pid` は現状 `"dac/pai pid mismatch"` になる（新チェックがまだ無い）ので matches! が FAIL、`rejects_vid_scoped_paa_...` は検証が通ってしまい `unwrap_err` が panic することを確認。

- [ ] **Step 3: 実装** — 既存 VID/PID 整合ブロックを次の順序に再構成:

```rust
    // --- チェーン（厳格）: DAC↔PAI の Matter VID/PID 整合（spec §6.2.2.2）---
    match (dac.vid, pai.vid) {
        (Some(dac_vid), Some(pai_vid)) if dac_vid == pai_vid => {}
        _ => return Err(AttestationError::Chain("dac/pai vid mismatch")),
    }
    // DAC は PID 必須（spec §6.2.2.1: DAC subject は VID と PID を必ず持つ。
    // VID は直前の一致検査が None を弾いている）。
    if dac.pid.is_none() {
        return Err(AttestationError::Chain("dac missing pid"));
    }
    // PAI の PID は省略可（spec 上 optional）。あれば DAC と一致必須。
    if let Some(pai_pid) = pai.pid {
        if dac.pid != Some(pai_pid) {
            return Err(AttestationError::Chain("dac/pai pid mismatch"));
        }
    }
    // VID スコープ PAA（spec §6.2.2.2）: PAA が VID を持つなら PAI と一致必須。
    if let Some(paa_vid) = paa.vid {
        if pai.vid != Some(paa_vid) {
            return Err(AttestationError::Chain("vid-scoped paa/pai vid mismatch"));
        }
    }
```

- [ ] **Step 4: テスト実行** — `cargo test -p mat-controller attestation` 全緑。

- [ ] **Step 5: コミット** — `fix(attestation): VID スコープ PAA の VID 一致と DAC PID 必須を強制（監査 Tier4）`。

---

### Task 4: attestation.rs — parse_elements のネスト ContainerEnd

**Files:**
- Modify: `crates/mat-controller/src/attestation.rs`（`parse_elements` ~line 309-342、tests mod）

**Interfaces:**
- Consumes/Produces: `parse_elements(elements) -> Result<(Vec<u8>, [u8; 32]), AttestationError>`（シグネチャ不変）。

- [ ] **Step 1: 失敗するテストを書く**:

```rust
#[test]
fn parse_elements_skips_nested_containers() {
    // vendor-reserved フィールドにコンテナが来ても外側ループを打ち切らず、
    // 深さ 0 の cd/nonce だけを拾う。ネスト内の Context(2) は nonce と
    // 誤認しない。
    let nonce = [5u8; 32];
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.start_struct(Tag::Context(5)); // vendor フィールド（コンテナ）
    w.put_bytes(Tag::Context(2), &[0xAA; 32]); // 罠: ネスト内の tag 2
    w.end_container();
    w.put_bytes(Tag::Context(1), b"real-cd");
    w.put_bytes(Tag::Context(2), &nonce);
    w.end_container();
    let (cd, parsed_nonce) = parse_elements(&w.finish()).unwrap();
    assert_eq!(cd, b"real-cd");
    assert_eq!(parsed_nonce, nonce);
}
```

- [ ] **Step 2: テストが失敗することを確認** — 現状はネストの ContainerEnd で break し `no certification declaration` になるので FAIL。

- [ ] **Step 3: 実装** — `parse_elements` のループを深さ追跡に変更:

```rust
    let mut cd: Option<Vec<u8>> = None;
    let mut nonce: Option<[u8; 32]> = None;
    let mut depth = 0usize; // 深さ 0 = AttestationElements 直下
    loop {
        let el = r
            .next()
            .map_err(|_| AttestationError::Elements("tlv parse error"))?
            .ok_or(AttestationError::Elements("truncated elements"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => depth += 1,
            (Tag::Context(1), Value::Bytes(b)) if depth == 0 => cd = Some(b.to_vec()),
            (Tag::Context(2), Value::Bytes(b)) if depth == 0 => {
                nonce = Some(
                    b.try_into()
                        .map_err(|_| AttestationError::Elements("nonce wrong length"))?,
                );
            }
            _ => {} // timestamp / firmware_information / ネスト内要素は素通り
        }
    }
```

- [ ] **Step 4: テスト実行** — `cargo test -p mat-controller parse_elements` + attestation 全体が緑。

- [ ] **Step 5: コミット** — `fix(attestation): parse_elements がネストコンテナで外側ループを打ち切るのを修正（監査 Tier4）`。

---

### Task 5: attestation.rs — CD CMS の messageDigest 結合（warn 経路内）

**Files:**
- Modify: `crates/mat-controller/src/attestation.rs`（`parse_signer_info` ~line 498-523、`parse_cms_signed_data` の signer_info 取り出し ~line 489、tests mod）

**Interfaces:**
- Consumes: `crate::asn1`（DER 合成、テスト用）、`sha2::{Digest, Sha256}`（新 import）。
- Produces: `parse_signer_info` が signedAttrs 使用時に messageDigest 属性の存在 + `SHA-256(eContent)` 一致を要求（違反は `Err(&'static str)` → 呼び出し側で warn して署名検証スキップ）。`verify_device_attestation` の戻り値は不変（warn-only 維持）。

- [ ] **Step 1: 失敗するテストを書く** — `parse_signer_info` を直接ユニットテストする（DER は `crate::asn1` で合成）:

```rust
    // --- Task 5: CMS signedAttrs の messageDigest 結合 ---

    /// SignerInfo DER を合成する（version=3, sid=SKID 風ダミー, digestAlg,
    /// signedAttrs（呼び出し側指定）, sigAlg, signature）。
    fn make_signer_info(signed_attrs: Option<&[u8]>) -> Vec<u8> {
        use crate::asn1;
        let mut parts: Vec<Vec<u8>> = vec![
            asn1::integer(&[3]),
            asn1::octet_string(b"sid-dummy"), // SignerIdentifier（CHOICE、中身は読まれない）
            asn1::seq(&[&asn1::oid(&[0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01])]), // sha256
        ];
        if let Some(attrs) = signed_attrs {
            parts.push(asn1::context_constructed(0, attrs));
        }
        parts.push(asn1::seq(&[&asn1::oid(&[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02])])); // ecdsa-sha256
        parts.push(asn1::octet_string(&[0u8; 8])); // signature（形だけ）
        let refs: Vec<&[u8]> = parts.iter().map(Vec::as_slice).collect();
        // parse_signer_info は SEQ の**中身**を受け取る（呼び出し側で expect(0x30) 済み）
        refs.concat()
    }

    /// messageDigest 属性 1 つだけの signedAttrs 内容（[0] の中身）を合成する。
    fn message_digest_attr(digest: &[u8]) -> Vec<u8> {
        use crate::asn1;
        asn1::seq(&[
            &asn1::oid(&[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x04]), // 1.2.840.113549.1.9.4
            &asn1::set_of(&[&asn1::octet_string(digest)]),
        ])
    }

    #[test]
    fn signer_info_accepts_matching_message_digest() {
        use sha2::{Digest, Sha256};
        let econtent = b"cd-tlv-bytes";
        let digest: [u8; 32] = Sha256::digest(econtent).into();
        let attrs = message_digest_attr(&digest);
        let si = make_signer_info(Some(&attrs));
        let info = parse_signer_info(&si, econtent).unwrap();
        // signedAttrs 使用時の署名対象は SET(0x31) に再タグ付けされたもの
        assert_eq!(info.signed_bytes[0], 0x31);
    }

    #[test]
    fn signer_info_rejects_mismatched_message_digest() {
        let attrs = message_digest_attr(&[0xEE; 32]);
        let si = make_signer_info(Some(&attrs));
        let err = parse_signer_info(&si, b"cd-tlv-bytes").unwrap_err();
        assert_eq!(err, "signedAttrs messageDigest does not match econtent");
    }

    #[test]
    fn signer_info_rejects_missing_message_digest() {
        use crate::asn1;
        // signedAttrs はあるが messageDigest 属性が無い（contentType だけ）
        let attrs = asn1::seq(&[
            &asn1::oid(&[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x03]),
            &asn1::set_of(&[&asn1::oid(&[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x10, 0x01, 0x19])]),
        ]);
        let si = make_signer_info(Some(&attrs));
        let err = parse_signer_info(&si, b"cd-tlv-bytes").unwrap_err();
        assert_eq!(err, "signedAttrs missing messageDigest");
    }
```

（`asn1::context_constructed` のシグネチャは asn1.rs を確認して合わせる — x509 test_support の用法 `asn1::context_constructed(0, &asn1::integer(&[2]))` と同じ。`make_signer_info` の sid はダミーで良い — `parse_signer_info` は `r.read()` で読み飛ばすだけ。合わなければ実際のパーサ挙動に合わせて調整して良いが、**検証対象（messageDigest の 3 分岐）は変えない**。）

- [ ] **Step 2: テストが失敗することを確認** — `cargo test -p mat-controller signer_info` → 現状 `parse_signer_info` は messageDigest を見ないので reject 系 2 本が FAIL。

- [ ] **Step 3: 実装** — attestation.rs に定数とヘルパを追加し、`parse_signer_info` の signedAttrs 分岐に検証を挿入:

```rust
/// CMS messageDigest 属性 OID 1.2.840.113549.1.9.4（内容バイト）。
const OID_CMS_MESSAGE_DIGEST: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x04];

/// signedAttrs（[0] の中身 = SET OF Attribute の要素列）から messageDigest
/// 属性を探し、`SHA-256(econtent)` と一致することを確認する（CMS §5.4:
/// signedAttrs 使用時は messageDigest が eContent を署名に結合する。これが
/// 無いと signedAttrs への正しい署名が eContent と無関係でも通ってしまう）。
fn verify_message_digest_attr(attrs: &[u8], econtent: &[u8]) -> Result<(), &'static str> {
    let mut r = DerReader::new(attrs);
    while !r.is_empty() {
        let attr = r.expect(0x30).map_err(|_| "bad signedAttrs attribute")?;
        let mut ar = DerReader::new(attr);
        let oid = ar.expect(0x06).map_err(|_| "bad signedAttrs attribute oid")?;
        if oid != OID_CMS_MESSAGE_DIGEST {
            continue;
        }
        let vals = ar.expect(0x31).map_err(|_| "bad messageDigest value set")?;
        let mut vr = DerReader::new(vals);
        let digest = vr.expect(0x04).map_err(|_| "messageDigest not an octet string")?;
        let actual: [u8; 32] = sha2::Sha256::digest(econtent).into();
        if digest == actual {
            return Ok(());
        }
        return Err("signedAttrs messageDigest does not match econtent");
    }
    Err("signedAttrs missing messageDigest")
}
```

（`use sha2::Digest;` を import に追加。）`parse_signer_info` の signedAttrs 分岐:

```rust
    if r.peek_tag() == Some(0xA0) {
        let (_, content, raw) = r.read().map_err(|_| "bad signedAttrs")?;
        if raw.is_empty() {
            return Err("empty signedAttrs");
        }
        verify_message_digest_attr(content, econtent)?;
        let mut reencoded = Vec::with_capacity(raw.len());
        reencoded.push(0x31); // [0] IMPLICIT -> SET タグに戻す
        reencoded.extend_from_slice(&raw[1..]);
        signed_bytes = reencoded;
    }
```

（`r.read()` の戻り値タプルの 2 要素目が内容バイト — 既存コードは `_content` と捨てていたのを使う。）`parse_cms_signed_data` 側は理由を warn してから None に落とす:

```rust
    let signer_info = match parse_signer_info(first, &cd_tlv) {
        Ok(v) => Some(v),
        Err(reason) => {
            tracing::warn!(reason, "certification declaration signerInfo rejected — continuing");
            None
        }
    };
    Ok((cd_tlv, signer_info))
```

- [ ] **Step 4: テスト実行** — `cargo test -p mat-controller attestation` 全緑（既存 `accepts_valid_attestation` は CD が偽物 = CMS パース失敗の warn 経路のままで影響なし）。

- [ ] **Step 5: コミット** — `fix(attestation): CD CMS の signedAttrs.messageDigest と eContent の結合を検証（監査 Tier4、warn 経路内）`。

---

### Task 6: PASE 成功経路のオフラインテスト（SPAKE2+ verifier 応答器）

**Files:**
- Modify: `crates/mat-controller/src/spake2p.rs`（内部関数の `pub(crate)` 化のみ、ロジック不変）
- Modify: `crates/mat-controller/src/test_support.rs`（`pase_responder_task` 追加）
- Create: `crates/mat-controller/tests/pase_self_handshake.rs`

**Interfaces:**
- Consumes: `spake2p::{SPAKE_M, SPAKE_N, derive_w0_w1}`（既存 pub）と、`pub(crate)` 化する `decode_point` / `encode_point` / `build_transcript` / `split_hash` / `confirmation_keys` / `hmac32` / `random_scalar`。`pase::{OPCODE_PBKDF_PARAM_REQUEST, OPCODE_PBKDF_PARAM_RESPONSE}`（pub）と `pase::{OPCODE_PASE_PAKE1, OPCODE_PASE_PAKE2, OPCODE_PASE_PAKE3}`（pub(crate)、同一クレートの test_support から可視）。test_support 既存の `build_unsecured` / `recv_dg` / `decode_unsecured` / `report_data_false_suppressed`。
- Produces: `pub async fn pase_responder_task(transport: UdpTransport, passcode: u32) -> SocketAddr`（test-responder feature 下）。

- [ ] **Step 1: 失敗するテストを書く** — `crates/mat-controller/tests/pase_self_handshake.rs` を新規作成（case_self_handshake.rs と同型）:

```rust
//! Offline PASE self-handshake (audit Tier 5).
//!
//! Drives the real PASE initiator (`pase::establish`) against a test-only
//! SPAKE2+ *verifier* responder (`test_support::pase_responder_task`) over
//! loopback UDP, then performs one secured IM read. First executable
//! coverage of the PASE success path: Ke derivation, the SessionKeys
//! i2r/r2i split, and both confirmation directions (cA/cB) — previously
//! only encode/decode shapes and failure paths were tested.

use std::sync::Arc;

use mat_controller::im::{self, ImValue};
use mat_controller::pase;
use mat_controller::test_support::{fast_cfg, pase_responder_task};
use mat_controller::transport::{Transport, UdpTransport};

#[tokio::test]
async fn pase_establishes_and_reads_over_loopback() {
    let responder_transport = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
        .await
        .unwrap();
    let responder_addr = responder_transport.local_addr().unwrap();
    let responder = tokio::spawn(pase_responder_task(responder_transport, 20202021));

    let initiator_udp = Arc::new(
        UdpTransport::bind_addr("[::1]:0".parse().unwrap())
            .await
            .unwrap(),
    );
    let initiator_local = initiator_udp.local_addr().unwrap();
    let transport = Arc::new(Transport::Udp(Arc::clone(&initiator_udp)));

    let cfg = fast_cfg();
    let mut session = pase::establish(Arc::clone(&transport), responder_addr, 20202021, &cfg)
        .await
        .expect("PASE establish should succeed over loopback");

    let value = session
        .read_attribute(1, im::CLUSTER_ON_OFF, im::ATTR_ON_OFF, &cfg)
        .await
        .expect("secured read should succeed");
    assert_eq!(value, ImValue::Bool(false));

    let observed = responder.await.expect("responder task panicked");
    assert_eq!(observed, initiator_local, "responder saw the initiator's socket");
}
```

- [ ] **Step 2: テストが失敗することを確認** — `cargo test -p mat-controller --test pase_self_handshake` → コンパイルエラー（`pase_responder_task` 未定義）。

- [ ] **Step 3: spake2p.rs の内部を pub(crate) 化** — `decode_point` / `encode_point` / `build_transcript` / `split_hash` / `confirmation_keys` / `hmac32` / `random_scalar` の `fn` を `pub(crate) fn` に（ロジック・シグネチャ不変。doc コメントに「test_support の PASE verifier 役が使う」旨を 1 行追記）。

- [ ] **Step 4: test_support.rs に PASE 応答器を実装** — モジュール末尾に追加:

```rust
// ============================================================================
// PASE responder（SPAKE2+ verifier 役）— audit Tier 5
// ============================================================================

/// The test-only PASE responder: the mirror of `pase::establish` with the
/// roles swapped, using real SPAKE2+ *verifier* math (Y = y·P + w0·N,
/// Z = y·(X − w0·M), V = y·L with L = w1·P). Serves PBKDFParamRequest →
/// PBKDFParamResponse → Pake1/2/3 → StatusReport(success), then answers one
/// secured IM ReadRequest with ReportData(on-off=false). PASE sessions are
/// unauthenticated: both nonce node ids are 0 (spec §4.13).
///
/// Returns the initiator's observed source `SocketAddr` (same contract as
/// `responder_task`).
pub async fn pase_responder_task(transport: UdpTransport, passcode: u32) -> SocketAddr {
    use p256::ProjectivePoint;
    use crate::pase::{
        OPCODE_PASE_PAKE1, OPCODE_PASE_PAKE2, OPCODE_PASE_PAKE3, OPCODE_PBKDF_PARAM_REQUEST,
        OPCODE_PBKDF_PARAM_RESPONSE,
    };
    use crate::spake2p;

    const ITERATIONS: u32 = 1000;
    const SALT: &[u8; 16] = b"SPAKE2P Key Salt";

    // --- PBKDFParamRequest ---
    let (req_payload, req_exchange, req_counter, initiator_session_id, initiator_addr) = loop {
        let (buf, from) = recv_dg(&transport).await;
        let Some((p, payload)) = decode_unsecured(&buf) else {
            continue;
        };
        if p.opcode != OPCODE_PBKDF_PARAM_REQUEST || !p.initiator {
            continue;
        }
        let (h, _) = MessageHeader::decode(&buf).unwrap();
        // initiatorSessionId は struct の tag 2
        let mut r = Reader::new(&payload);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let mut sid: Option<u16> = None;
        loop {
            let el = r.next().unwrap().expect("pbkdf request truncated");
            match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(2), Value::Uint(v)) => sid = Some(v as u16),
                _ => {}
            }
        }
        break (
            payload,
            p.exchange_id,
            h.message_counter,
            sid.expect("pbkdf request missing initiator session id"),
            from,
        );
    };

    let resp_session_id: u16 = 0xB0B1;

    // --- PBKDFParamResponse ---
    let resp_payload = {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(1), &[0u8; 32]); // initiatorRandom echo（initiator は無視）
        w.put_bytes(Tag::Context(2), &[1u8; 32]); // responderRandom（同上）
        w.put_uint(Tag::Context(3), u64::from(resp_session_id));
        w.start_struct(Tag::Context(4));
        w.put_uint(Tag::Context(1), u64::from(ITERATIONS));
        w.put_bytes(Tag::Context(2), SALT);
        w.end_container();
        w.end_container();
        w.finish()
    };
    let resp_dg = build_unsecured(
        200,
        OPCODE_PBKDF_PARAM_RESPONSE,
        req_exchange,
        Some(req_counter),
        false,
        &resp_payload,
    );
    transport
        .send_to(&resp_dg, initiator_addr)
        .await
        .expect("send pbkdf param response");

    // --- PAKE context（spec §4.13.1.2、pase.rs と同じ構成）---
    let mut hasher = Sha256::new();
    hasher.update(b"CHIP PAKE V1 Commissioning");
    hasher.update(&req_payload);
    hasher.update(&resp_payload);
    let context: [u8; 32] = hasher.finalize().into();

    // --- Pake1 ---
    let (p_a, pake1_exchange, pake1_counter) = loop {
        let (buf, _from) = recv_dg(&transport).await;
        let Some((p, payload)) = decode_unsecured(&buf) else {
            continue;
        };
        if p.opcode != OPCODE_PASE_PAKE1 {
            continue; // PBKDFParamRequest の MRP 再送などは無視
        }
        let (h, _) = MessageHeader::decode(&buf).unwrap();
        let mut r = Reader::new(&payload);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let mut pa: Option<[u8; 65]> = None;
        loop {
            let el = r.next().unwrap().expect("pake1 truncated");
            match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(1), Value::Bytes(b)) => {
                    pa = Some(b.try_into().expect("pA is 65 bytes"))
                }
                _ => {}
            }
        }
        break (pa.expect("pake1 missing pA"), p.exchange_id, h.message_counter);
    };

    // --- SPAKE2+ verifier 計算 ---
    let (w0, w1) = spake2p::derive_w0_w1(passcode, SALT, ITERATIONS);
    let y = spake2p::random_scalar();
    let m = spake2p::decode_point(&spake2p::SPAKE_M).expect("SPAKE_M constant");
    let n = spake2p::decode_point(&spake2p::SPAKE_N).expect("SPAKE_N constant");
    let p_b_point = ProjectivePoint::GENERATOR * y + n * w0;
    let p_b = spake2p::encode_point(&p_b_point);
    let p_a_point = spake2p::decode_point(&p_a).expect("pA on curve");
    let z = (p_a_point - m * w0) * y;
    let l = ProjectivePoint::GENERATOR * w1;
    let v = l * y;
    let tt = spake2p::build_transcript(&context, b"", b"", &p_a, &p_b, &z, &v, &w0);
    let (k_a, k_e) = spake2p::split_hash(&tt);
    let (kc_a, kc_b) = spake2p::confirmation_keys(&k_a);
    let c_b = spake2p::hmac32(&kc_b, &p_a);
    let expected_c_a = spake2p::hmac32(&kc_a, &p_b);

    // --- Pake2 ---
    let pake2_payload = {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(1), &p_b);
        w.put_bytes(Tag::Context(2), &c_b);
        w.end_container();
        w.finish()
    };
    let pake2_dg = build_unsecured(
        201,
        OPCODE_PASE_PAKE2,
        pake1_exchange,
        Some(pake1_counter),
        false,
        &pake2_payload,
    );
    transport
        .send_to(&pake2_dg, initiator_addr)
        .await
        .expect("send pake2");

    // --- Pake3（cA 検証）---
    let (c_a, pake3_exchange, pake3_counter) = loop {
        let (buf, _from) = recv_dg(&transport).await;
        let Some((p, payload)) = decode_unsecured(&buf) else {
            continue;
        };
        if p.opcode != OPCODE_PASE_PAKE3 {
            continue;
        }
        let (h, _) = MessageHeader::decode(&buf).unwrap();
        let mut r = Reader::new(&payload);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let mut ca: Option<[u8; 32]> = None;
        loop {
            let el = r.next().unwrap().expect("pake3 truncated");
            match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(1), Value::Bytes(b)) => {
                    ca = Some(b.try_into().expect("cA is 32 bytes"))
                }
                _ => {}
            }
        }
        break (ca.expect("pake3 missing cA"), p.exchange_id, h.message_counter);
    };
    assert_eq!(c_a, expected_c_a, "initiator cA mismatch (transcript bug?)");

    // --- StatusReport(success) ---
    let status = [0u8; 8]; // general=0, protocol id=0, code=0
    let status_dg = build_unsecured(
        202,
        OPCODE_STATUS_REPORT,
        pake3_exchange,
        Some(pake3_counter),
        false,
        &status,
    );
    transport
        .send_to(&status_dg, initiator_addr)
        .await
        .expect("send pase status report");

    // --- SessionKeys（spec §4.13.2.3: HKDF(salt=[], ikm=Ke, "SessionKeys")）---
    let okm = hkdf48(&k_e, &[], INFO_SESSION_KEYS);
    let i2r: [u8; 16] = okm[..16].try_into().unwrap();
    let r2i: [u8; 16] = okm[16..32].try_into().unwrap();

    // --- Serve one secured IM ReadRequest（PASE は両側 node id 0）---
    let (read_exchange, read_counter) = loop {
        let (buf, _from) = recv_dg(&transport).await;
        let (mh, _) = match MessageHeader::decode(&buf) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if mh.session_id != resp_session_id {
            continue;
        }
        let (h, p, _payload) = match open_message(&i2r, &buf, 0) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if p.protocol_id != im::PROTOCOL_ID_IM || p.opcode != im::OPCODE_READ_REQUEST {
            continue;
        }
        break (p.exchange_id, h.message_counter);
    };

    let report = report_data_false_suppressed();
    let header = MessageHeader {
        session_id: initiator_session_id,
        security_flags: 0,
        message_counter: 2000,
        source_node_id: None,
        destination: Destination::None,
    };
    let proto = ProtocolHeader {
        initiator: false,
        needs_ack: true,
        acked_counter: Some(read_counter),
        opcode: im::OPCODE_REPORT_DATA,
        exchange_id: read_exchange,
        protocol_id: im::PROTOCOL_ID_IM,
        vendor_id: None,
    };
    let report_dg = seal_message(&r2i, &header, &proto, &report, 0).expect("seal report data");
    transport
        .send_to(&report_dg, initiator_addr)
        .await
        .expect("send report data");
    initiator_addr
}
```

注意:
- `hkdf48(&k_e, &[], INFO_SESSION_KEYS)` — 既存 `hkdf48(shared, salt, info)` は salt を `Some(salt)` で渡す実装。空 salt（`&[]`）で pase.rs 側の `Hkdf::new(Some(&[]), ...)` と一致する。
- MRP 設定の注意: initiator の `send_reliable` は piggyback ack（`acked_counter`）を検査する — 各応答で直前受信メッセージの counter を `Some(..)` で返しているのはそのため。
- `pase.rs` の `OPCODE_PASE_PAKE1/2/3` は `pub(crate)` — test_support は同一クレートなのでそのまま使える（re-export 不要）。
- import 追加が要る: `use crate::pase::{...}` / `use crate::spake2p;` / `use p256::ProjectivePoint;`（関数内 use でも可、既存流儀に合わせる）。

- [ ] **Step 5: テスト実行** — `cargo test -p mat-controller --test pase_self_handshake -- --nocapture` が PASS。`cargo test -p mat-controller` 全体も緑（spake2p の可視性変更で警告が出ないこと）。

- [ ] **Step 6: コミット** — `test(pase): SPAKE2+ verifier 応答器による PASE 成功経路のオフラインテスト（監査 Tier5）`。

---

### Task 7: e2e スクリプト是正（m6 ヘッダ + m8c3 リトライ抑止）

**Files:**
- Modify: `scripts/e2e-m6.sh`（ヘッダ 5 行 + line 10 のエラーメッセージ）
- Modify: `scripts/e2e-m8c3-real.sh`（`_run_ssh_capture` ~line 219-251 + 状態変更 op 呼び出し点）

**Interfaces:**
- Produces: `_run_ssh_capture` が環境変数 `NO_RETRY=1` のときリトライしない。状態変更 op は `NO_RETRY=1 run_xxx ...` 形式。

- [ ] **Step 1: e2e-m6.sh のヘッダを是正** — 現在の 1-5 行目:

```bash
#!/usr/bin/env bash
# [M8c-3] chip-tool 撤去済みのため 0.22.0 以降では動かない（歴史的アーカイブ。
# 動かすなら git tag の 0.21.0 時点を checkout）。現行ハーネスは e2e-m8c3-real.sh。
# Phase 5 M6a 受け入れ: native commissioning ローカル E2E。
# 前提: ./chip-all-clusters-app (task chip:extract:app)。chip-tool は不要。
```

を次に置換（このスクリプトは chip-tool 非依存で現行コードで動く。「歴史的アーカイブ」表記は監査で誤りと確認済み）:

```bash
#!/usr/bin/env bash
# Phase 5 M6a 受け入れ: native commissioning ローカル E2E。chip-tool 非依存で
# 現行コードでも動く、唯一の device-free commissioning ハーネス（実機ハーネスは
# e2e-m8c3-real.sh）。
# 前提: ./chip-all-clusters-app（または MAT_E2E_APP=<path>）。入手方法:
#   - 旧 Docker ステージを使う: `git show c415bda~1:Dockerfile` の
#     all-clusters-builder ステージ（chip-tool 退役コミットで撤去済み）
#   - または upstream connectedhomeip の examples/all-clusters-app/linux をビルド
```

line 10 の `[[ -x "$APP" ]] || { echo "error: $APP なし (task chip:extract:app)"; exit 1; }` を
`[[ -x "$APP" ]] || { echo "error: $APP なし（入手方法はこのファイル先頭のコメント参照）"; exit 1; }` に。

- [ ] **Step 2: e2e-m8c3-real.sh の `_run_ssh_capture` に NO_RETRY ガード** — リトライ判定行:

```bash
    if [ "$rc" != 0 ] && [ "$attempt" = "1" ]; then
```

を:

```bash
    if [ "$rc" != 0 ] && [ "$attempt" = "1" ] && [ "${NO_RETRY:-0}" != "1" ]; then
```

に変更し、関数コメント（~line 219-226）へ 1 行追記:

```bash
# NO_RETRY=1 を付けて呼ぶとリトライしない（toggle/write/commission/RemoveFabric
# 等の状態変更 op の二重実行防止 — 監査 Tier5）。
```

- [ ] **Step 3: 状態変更 op の呼び出し点に NO_RETRY=1 を付ける** — 対象（2026-08-15 時点の行番号、`grep -n "run_native_default \|run_matd \|run_native_fresh "` で再確認して漏れなく）:
  - write: 390, 402, 521, 527
  - invoke（toggle / remove-group / remove-fabric）: 206, 410, 417, 828, 1050, 1091
  - group provision: 466, 484
  - group invoke: 500, 507, 531, 534
  - open-window: 452, 762
  - fabric init: 716, 737
  - commission: 787, 1056

  例: `WRITE_OUT=$(run_native_default ...)` → `WRITE_OUT=$(NO_RETRY=1 run_native_default ...)`、
  `run_native_fresh invoke ... remove-fabric ... >/dev/null` → `NO_RETRY=1 run_native_fresh invoke ... remove-fabric ... >/dev/null`。
  read / discover / describe / diag / status / fabric read 系は従来どおり（触らない）。

- [ ] **Step 4: 構文確認** — `bash -n scripts/e2e-m6.sh && bash -n scripts/e2e-m8c3-real.sh` がエラーなし。`/usr/bin/grep -c "NO_RETRY=1" scripts/e2e-m8c3-real.sh` が対象件数（21 前後）と一致することを確認。

- [ ] **Step 5: コミット** — `fix(e2e): m6 ハーネスの死蔵ヘッダ是正 + m8c3 の状態変更 op リトライ抑止（監査 Tier5）`。

---

### Task 8: docs.yml checksum + バージョン 1.28.0

**Files:**
- Modify: `.github/workflows/docs.yml`（Install mdBook ステップ）
- Modify: `Cargo.toml`（workspace version）、`Cargo.lock`（追従）

**Interfaces:** なし（締め作業）。

- [ ] **Step 1: mdBook tarball の sha256 を取得** — 一時ディレクトリ（セッションの scratchpad）に落として計測:

```bash
curl -sSL -o "$SCRATCH/mdbook.tar.gz" \
  "https://github.com/rust-lang/mdBook/releases/download/v0.5.4/mdbook-v0.5.4-x86_64-unknown-linux-gnu.tar.gz"
sha256sum "$SCRATCH/mdbook.tar.gz"
```

（得られたハッシュを Step 2 で埋め込む。trust-on-first-use — 以後 CI が同一物であることを固定する。）

- [ ] **Step 2: docs.yml を修正** — env に `MDBOOK_SHA256: <Step 1 のハッシュ>` を追加し、Install ステップを:

```yaml
      - name: Install mdBook (prebuilt)
        run: |
          mkdir -p "$HOME/.local/bin"
          curl -sSL -o /tmp/mdbook.tar.gz \
            "https://github.com/rust-lang/mdBook/releases/download/${MDBOOK_VERSION}/mdbook-${MDBOOK_VERSION}-x86_64-unknown-linux-gnu.tar.gz"
          echo "${MDBOOK_SHA256}  /tmp/mdbook.tar.gz" | sha256sum -c -
          tar -xz -C "$HOME/.local/bin" -f /tmp/mdbook.tar.gz
          echo "$HOME/.local/bin" >> "$GITHUB_PATH"
```

- [ ] **Step 3: バージョン bump** — `Cargo.toml` の `version = "1.27.0"` → `"1.28.0"`、`cargo build` で `Cargo.lock` を追従させる。

- [ ] **Step 4: 全体検証** — `task check` 全緑。

- [ ] **Step 5: コミット** — `chore: 1.28.0（attestation/x509 検証強化・PASE オフラインテスト・e2e/CI 整備 — 監査 Tier4/5）`（docs.yml と Cargo.* を同一コミットで良い）。

---

## 実装後（メインセッションで実施 — この計画のタスク外）

1. `task dist:arm64` で aarch64 バイナリをビルドし、jarvis へ `*.new` として転送。
2. jarvis 実機 E2E: `scripts/e2e-m8c3-real.sh`（STAGE=2 の cross-fabric commission が実デバイスの DAC/PAI/PAA チェーンに対して新 keyUsage / PID / VID 検査を実機実証する）。落ちた場合は該当検査の warn 降格を検討して再判断。
3. E2E 合格後に main へ push。メモリ（mat-commission-cert-audit-backlog）を全消化として更新。
