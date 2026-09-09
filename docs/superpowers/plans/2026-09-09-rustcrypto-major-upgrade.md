# RustCrypto 一群の major 更新 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** mat-controller / mat-device の RustCrypto 依存を新系列（digest 0.11 / cipher 0.5 / aead 0.6 / elliptic-curve 0.14）に一括で上げ、暗号の出力がバイト単位で不変であることをゴールデンテストで証明する。

**Architecture:** 暗号呼び出しは mat-controller の 9 ファイル + mat-device の 5 ファイルに閉じている（`crypto.rs` の AES-CCM / ECDSA、`spake2p.rs` の SPAKE2+、`case.rs` / `case_responder.rs` / `pase.rs` / `fabric.rs` の HKDF / HMAC / ECDH、`cert.rs` / `x509.rs` / `attestation.rs` の署名検証と SHA-1 SKID）。API は新系列でも名前がほぼ同じで、変わるのは `generic_array::GenericArray` → `hybrid_array::Array` の型と一部の import パスだけ。だから手順は「先に現行版でゴールデン（既知解）テストを固定 → 一括バンプ → コンパイラの指摘を潰す → ゴールデンが通る」。RustCrypto は digest / cipher / aead / elliptic-curve を共有しているため、個別には上げられず **同時バンプ必須**。

**Tech Stack:** Rust 1.98（新系列の MSRV 1.85 を満たす）、sha1 0.11 / sha2 0.11 / hmac 0.13 / hkdf 0.13 / pbkdf2 0.13 / aes 0.9 / ccm 0.6 / p256 0.14（features `ecdh`, `ecdsa` は 0.14 にも存在）。

**Spec:** 本計画は spec なし（bounded タスク）。設計は 2026-09-09 のチャットで合意: 「全部同時に上げる、ゴールデンで不変を証明、実機 E2E をリリース時に必ず回す」。

## Global Constraints

- 暗号の出力（鍵導出・署名・CCM 暗号文・SPAKE2+ 共有値）は **1 バイトも変えない**。変わったらそれはバグで、依存の使い方を直す（テストの期待値を書き換えない）。
- `aes 0.9.0` は yanked。`aes = "0.9"` と書けば 0.9.1 以降が選ばれるが、`Cargo.lock` に 0.9.0 が入っていないことを確認する。
- `p256` の feature は現行どおり `["ecdh", "ecdsa"]` のみ。`expose-field` は使っていないので触らない。
- `hybrid_array::Array::from_slice` は deprecated。スライスからの変換は `TryFrom` / `try_into()`、固定長配列参照からは `.into()`。
- 各タスクの最後に `task check`（fmt:check + clippy -D warnings + doc:check + test）を通してからコミット。コミットメッセージは既存流儀（日本語 + conventional prefix）。
- push / main マージはしない（オーケストレーターが行う）。
- ble feature（bluer）はローカルに libdbus が無くビルドできない。ble 経路は暗号 API に触れていない（`btp.rs` / `ble.rs` は BTP 配管のみ）ので、CI の `clippy (ble)` に委ねる。

---

### Task 1: 現行版でゴールデン（既知解）テストを固定する

**Files:**
- Modify: `crates/mat-controller/src/crypto.rs`（`#[cfg(test)] mod tests` 末尾）
- Modify: `crates/mat-controller/src/spake2p.rs`（`#[cfg(test)] mod tests` 末尾）
- Modify: `crates/mat-controller/src/case.rs`（`#[cfg(test)] mod tests` 末尾。無ければ新設）
- Modify: `crates/mat-device/src/core/group_privacy.rs`（`#[cfg(test)] mod tests` 末尾）

**Interfaces:**
- Consumes: `crypto::encrypt_payload(key: &[u8;16], nonce: &[u8;13], aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, CryptoError>`、`crypto::sign_ecdsa_p256(private_key: &[u8;32], message: &[u8]) -> Result<[u8;64], CryptoError>`、`spake2p::derive_w0_w1(passcode: u32, salt: &[u8], iterations: u32) -> (Scalar, Scalar)`、`case::derive_session_keys(shared: &[u8], ipk: &[u8;16], transcript: &[u8;32]) -> SessionKeys`（フィールド `i2r`, `r2i`, `attestation_challenge` 各 `[u8;16]`）、`group_privacy::derive_privacy_key(operational_key: &[u8;16]) -> [u8;16]`
- Produces: 4 つのゴールデンテスト。Task 2 / 3 の合否判定に使う。

既存テストにある既知解: `spake2p::tests::rfc9383_p256_vector`（RFC 9383 ベクタ）、`fabric::tests::derives_spec_compressed_fabric_id` / `derives_spec_destination_id`（Matter spec ベクタ）、`cert.rs` の SHA-1 SKID 検証。これらは触らない。足りないのは CCM 暗号文・ECDSA 署名・PBKDF2 由来の w0/w1・CASE セッション鍵・group privacy 鍵の 5 つ。

- [ ] **Step 1: 現行版で期待値を採取する使い捨てテストを書く**

`crates/mat-controller/src/crypto.rs` の `mod tests` 末尾に追加:

```rust
    /// ゴールデン採取用（Task 1 の Step 2 で値を写したら本テストは Step 3 の形に置き換える）。
    #[test]
    fn golden_dump() {
        let key: [u8; 16] = core::array::from_fn(|i| i as u8);
        let nonce: [u8; 13] = core::array::from_fn(|i| 0xA0 + i as u8);
        let aad = b"matter-aad";
        let pt = b"the quick brown fox jumps over the lazy dog";
        let ct = encrypt_payload(&key, &nonce, aad, pt).unwrap();
        println!("CCM {:02x?}", ct);
        let sk: [u8; 32] = core::array::from_fn(|i| 0x11 + i as u8);
        let sig = sign_ecdsa_p256(&sk, b"tbs-message").unwrap();
        println!("ECDSA {:02x?}", sig);
    }
```

`crates/mat-controller/src/spake2p.rs` の `mod tests` 末尾に追加:

```rust
    #[test]
    fn golden_dump() {
        let (w0, w1) = derive_w0_w1(20202021, b"SPAKE2P Key Salt", 1000);
        println!("W0 {:02x?}", w0.to_bytes());
        println!("W1 {:02x?}", w1.to_bytes());
    }
```

`crates/mat-controller/src/case.rs` の `mod tests` 末尾（`mod tests` が無ければ `#[cfg(test)] mod tests { use super::*; }` を新設）に追加:

```rust
    #[test]
    fn golden_dump() {
        let shared: [u8; 32] = core::array::from_fn(|i| 0x30 + i as u8);
        let ipk: [u8; 16] = core::array::from_fn(|i| 0x50 + i as u8);
        let transcript: [u8; 32] = core::array::from_fn(|i| 0x70 + i as u8);
        let k = derive_session_keys(&shared, &ipk, &transcript);
        println!("I2R {:02x?}", k.i2r);
        println!("R2I {:02x?}", k.r2i);
        println!("AC {:02x?}", k.attestation_challenge);
    }
```

`crates/mat-device/src/core/group_privacy.rs` の `mod tests` 末尾に追加:

```rust
    #[test]
    fn golden_dump() {
        let op: [u8; 16] = core::array::from_fn(|i| 0x90 + i as u8);
        println!("PRIV {:02x?}", derive_privacy_key(&op));
    }
```

- [ ] **Step 2: 採取テストを走らせて値を写す**

Run: `cargo test -p mat-controller golden_dump -- --nocapture 2>&1 | grep -E '^(CCM|ECDSA|W0|W1|I2R|R2I|AC) ' && cargo test -p mat-device golden_dump -- --nocapture 2>&1 | grep '^PRIV '`
Expected: 8 行（CCM / ECDSA / W0 / W1 / I2R / R2I / AC / PRIV）が 16 進配列で出る。

出力の `[aa, bb, …]` をそのまま Rust の配列リテラル `[0xaa, 0xbb, …]` に写す（`sed -E 's/([0-9a-f]{2})/0x\1/g'` で変換できる）。

- [ ] **Step 3: 採取テストを固定テストに置き換える**

各ファイルの `golden_dump` を削除し、代わりに次を置く（`<…>` は Step 2 の値）:

`crypto.rs`:

```rust
    /// RustCrypto 依存を上げても暗号文が 1 バイトも変わらないことを固定する
    /// ゴールデン（2026-09-09、aes 0.8 / ccm 0.5 で採取）。
    #[test]
    fn golden_ccm_ciphertext_is_stable() {
        let key: [u8; 16] = core::array::from_fn(|i| i as u8);
        let nonce: [u8; 13] = core::array::from_fn(|i| 0xA0 + i as u8);
        let ct = encrypt_payload(&key, &nonce, b"matter-aad", b"the quick brown fox jumps over the lazy dog").unwrap();
        const EXPECTED: [u8; 43 + 16] = [<CCM の 59 バイト>];
        assert_eq!(ct, EXPECTED);
    }

    /// ECDSA は RFC 6979 決定的署名なので依存を上げても同じ r||s になる
    /// （2026-09-09、p256 0.13 で採取）。
    #[test]
    fn golden_ecdsa_signature_is_stable() {
        let sk: [u8; 32] = core::array::from_fn(|i| 0x11 + i as u8);
        let sig = sign_ecdsa_p256(&sk, b"tbs-message").unwrap();
        const EXPECTED: [u8; 64] = [<ECDSA の 64 バイト>];
        assert_eq!(sig, EXPECTED);
    }
```

`spake2p.rs`:

```rust
    /// PBKDF2-HMAC-SHA256 → mod n 還元の w0/w1 ゴールデン（2026-09-09、
    /// pbkdf2 0.12 / p256 0.13 で採取）。passcode / salt / iterations は
    /// Matter テストデバイスの既定値。
    #[test]
    fn golden_w0_w1_are_stable() {
        let (w0, w1) = derive_w0_w1(20202021, b"SPAKE2P Key Salt", 1000);
        const W0: [u8; 32] = [<W0>];
        const W1: [u8; 32] = [<W1>];
        assert_eq!(w0.to_bytes().as_slice(), &W0);
        assert_eq!(w1.to_bytes().as_slice(), &W1);
    }
```

`case.rs`:

```rust
    /// CASE セッション鍵（HKDF-SHA256、spec §4.14.2.6）のゴールデン
    /// （2026-09-09、hkdf 0.12 で採取）。
    #[test]
    fn golden_session_keys_are_stable() {
        let shared: [u8; 32] = core::array::from_fn(|i| 0x30 + i as u8);
        let ipk: [u8; 16] = core::array::from_fn(|i| 0x50 + i as u8);
        let transcript: [u8; 32] = core::array::from_fn(|i| 0x70 + i as u8);
        let k = derive_session_keys(&shared, &ipk, &transcript);
        assert_eq!(k.i2r, [<I2R>]);
        assert_eq!(k.r2i, [<R2I>]);
        assert_eq!(k.attestation_challenge, [<AC>]);
    }
```

`group_privacy.rs`:

```rust
    /// group privacy 鍵（HKDF-SHA256、spec §4.16.2）のゴールデン
    /// （2026-09-09、hkdf 0.12 で採取）。
    #[test]
    fn golden_privacy_key_is_stable() {
        let op: [u8; 16] = core::array::from_fn(|i| 0x90 + i as u8);
        assert_eq!(derive_privacy_key(&op), [<PRIV>]);
    }
```

- [ ] **Step 4: 固定テストが通ることを確認**

Run: `cargo test -p mat-controller golden_ && cargo test -p mat-device golden_`
Expected: 5 テスト PASS（golden_ccm_ciphertext_is_stable / golden_ecdsa_signature_is_stable / golden_w0_w1_are_stable / golden_session_keys_are_stable / golden_privacy_key_is_stable）。

- [ ] **Step 5: `task check` → コミット**

Run: `task check`
Expected: exit 0。

```bash
git add crates/mat-controller/src/crypto.rs crates/mat-controller/src/spake2p.rs crates/mat-controller/src/case.rs crates/mat-device/src/core/group_privacy.rs
git commit -m "test(crypto): RustCrypto 更新前のゴールデン 5 件（CCM 暗号文 / ECDSA 署名 / SPAKE2+ w0w1 / CASE 鍵 / privacy 鍵）"
```

---

### Task 2: mat-controller の RustCrypto を新系列へ一括バンプ

**Files:**
- Modify: `crates/mat-controller/Cargo.toml:21-30`
- Modify: `crates/mat-controller/src/crypto.rs:1-12`（import）と `encrypt_payload` / `decrypt_payload`
- Modify（コンパイラが指摘した箇所のみ）: `crates/mat-controller/src/{spake2p,case,case_responder,pase,cert,fabric,x509,attestation,test_support}.rs`、`crates/mat-controller/src/commissioning/commissioning_fabric.rs`
- Modify: `Cargo.lock`

**Interfaces:**
- Consumes: Task 1 のゴールデン 4 件（mat-controller 側）。
- Produces: 新系列で `cargo test -p mat-controller --all-targets` 合格。mat-device はまだ旧系列の `hkdf` / `sha2` / `p256` を宣言しているので、このタスクの終わりでは **workspace 全体はまだビルドできない**（Task 3 で揃える）。そのため本タスクのコミット前チェックは `-p mat-controller` に限定してよい（`task check` は Task 3 で）。

- [ ] **Step 1: Cargo.toml の版を上げる**

`crates/mat-controller/Cargo.toml` の `[dependencies]` を次に書き換える（他の行は触らない）:

```toml
ccm = "0.6"
aes = "0.9"
sha2 = "0.11"
sha1 = "0.11"
hkdf = "0.13"
hmac = "0.13"
p256 = { version = "0.14", features = ["ecdh", "ecdsa"] }
pbkdf2 = { version = "0.13", default-features = false, features = ["hmac"] }
```

Run: `cargo update -p ccm -p aes -p sha2 -p sha1 -p hkdf -p hmac -p p256 -p pbkdf2 2>&1 | tail -20`
Expected: 新版が解決される。もし `p256 0.13` と `0.14` の両方が `mat-device` 経由で残っても、このタスクではまだ良い。

- [ ] **Step 2: コンパイルエラーを一覧にする**

Run: `cargo check -p mat-controller --all-targets --message-format=short 2>&1 | grep -E '^crates/.*error' | sort | uniq -c | sort -rn`
Expected: エラーの大半は次の 4 種。他にあれば個別に読む。
  1. `ccm::consts` / `ccm::aead::{Aead, KeyInit, Payload}` の import 不整合
  2. `GenericArray` → `Array` 由来の型不一致（`.into()` / `try_into()` で解決する）
  3. `Scalar::to_bytes()` / `sig.to_bytes()` の戻り型が `Array` になった箇所の `.into()`
  4. `Sha256::digest(..).into()` の型推論失敗（`[u8; 32]` の型注釈で解決）

- [ ] **Step 3: crypto.rs を直す**

`crates/mat-controller/src/crypto.rs` 冒頭の import を次にする（ccm 0.6 は `aead` を再公開し、`consts` は `ccm::consts` のまま。`Aead` は `alloc` feature 付きで存在する — ccm の既定 feature で有効）:

```rust
use aes::Aes128;
use ccm::aead::{Aead, KeyInit, Payload};
use ccm::consts::{U13, U16};
use ccm::Ccm;
use p256::ecdsa::signature::{Signer, Verifier};
use p256::ecdsa::{Signature, SigningKey, VerifyingKey};
```

上記でコンパイルが通るなら本体は無変更でよい。`Aead` が見つからない場合だけ `use ccm::aead::Aead;` を `use ccm::aead::{AeadInOut, ...}` に替えず、`ccm` の feature に `alloc` を明示する:

```toml
ccm = { version = "0.6", features = ["alloc"] }
```

`encrypt_payload` の `Aes128Ccm::new(key.into())` と `.encrypt(nonce.into(), Payload { msg, aad })` は `&[u8; 16]` → `&Key<..>`、`&[u8; 13]` → `&Nonce<..>` の `From` が hybrid-array にあるので据え置き。`sign_ecdsa_p256` の `sig.to_bytes().into()` も `Array<u8, U64>` → `[u8; 64]` の `From` があるので据え置き。

- [ ] **Step 4: 残りのファイルをコンパイラの指摘どおりに直す**

方針（各パターンの直し方）:

- `let x: [u8; 32] = scalar.to_bytes().into();` → そのまま通るはず。通らなければ `.to_bytes().as_slice().try_into().unwrap()`。
- `out.copy_from_slice(&w0.to_bytes())` → `Array` は `Deref<Target=[u8]>` なので通る。通らなければ `.to_bytes().as_slice()`。
- `p256::SecretKey::from_slice(&b)` → 0.14 にも `from_slice` がある。据え置き。
- `p256::ecdh::diffie_hellman(secret.to_nonzero_scalar(), pk.as_affine())` と `shared.raw_secret_bytes().as_slice()` → 据え置き。
- `Sha256::digest(x).into()` で `[u8; 32]` に入れている箇所 → 左辺に型注釈があれば通る。無ければ `let d: [u8; 32] = Sha256::digest(x).into();`。
- `hmac::Mac` の `finalize().into_bytes().into()` → 通る。`hmac::Mac` の import は `use hmac::{Hmac, Mac};` のまま。`KeyInit` が要ると言われたら `use hmac::{Hmac, KeyInit, Mac};`。
- `pbkdf2::pbkdf2_hmac::<Sha256>(pw, salt, iter, &mut out)` → 0.13 でも同じシグネチャ。据え置き。
- `Scalar::from(256u64)` / `Scalar::ZERO` / `ProjectivePoint::GENERATOR` / `IDENTITY` / `AffinePoint::from_encoded_point` / `to_encoded_point(false)` → elliptic-curve 0.14 でも同名。据え置き。
- `Signature::from_slice(&bytes)` → ecdsa 0.17 にもある。据え置き。

**やらないこと**: ロジックの書き換え、関数の分割、`unwrap` の追加（既存が `?` / `map_err` ならそのまま）。

Run: `cargo check -p mat-controller --all-targets 2>&1 | grep -cE '^(error|warning)'`
Expected: `0`

- [ ] **Step 5: テストとゴールデンを通す**

Run: `cargo test -p mat-controller 2>&1 | grep -E '^test result|golden_|FAILED|panicked'`
Expected: すべて `ok`、`golden_ccm_ciphertext_is_stable` / `golden_ecdsa_signature_is_stable` / `golden_w0_w1_are_stable` / `golden_session_keys_are_stable` が PASS、既存の `rfc9383_p256_vector` / `derives_spec_*` も PASS。1 つでもゴールデンが落ちたら **期待値を直さず** 使い方を疑う（例: `Array` のエンディアン取り違えはあり得ない — 落ちるなら別の関数を呼んでいる）。

- [ ] **Step 6: fmt + clippy（mat-controller 限定）→ コミット**

Run: `cargo fmt -p mat-controller && cargo clippy -p mat-controller --all-targets -- -D warnings 2>&1 | tail -3`
Expected: `Finished`、warning 0。

```bash
git add crates/mat-controller/Cargo.toml Cargo.lock crates/mat-controller/src
git commit -m "chore(deps): mat-controller の RustCrypto を新系列へ（sha1/sha2 0.11、hmac/hkdf/pbkdf2 0.13、aes 0.9、ccm 0.6、p256 0.14）— ゴールデン不変"
```

---

### Task 3: mat-device を揃えて workspace 全体を新系列に統一

**Files:**
- Modify: `crates/mat-device/Cargo.toml:32-37`
- Modify（コンパイラが指摘した箇所のみ）: `crates/mat-device/src/core/commissioning/mod.rs:553-570`、`crates/mat-device/src/device.rs:211-213`、`crates/mat-device/src/core/pase.rs:261,355`、`crates/mat-device/src/core/group_privacy.rs:28`、`crates/mat-device/src/chip_test_attestation.rs:76-82`
- Modify: `Cargo.lock`

**Interfaces:**
- Consumes: Task 2 で新系列になった `mat_controller::{crypto, case, spake2p, …}` の API（シグネチャは不変）。Task 1 の `golden_privacy_key_is_stable`。
- Produces: workspace 全体が単一系列（digest 0.11 / elliptic-curve 0.14）でビルドされ、`task check` 合格。

- [ ] **Step 1: Cargo.toml の版を上げる**

`crates/mat-device/Cargo.toml` の 3 行を書き換える（コメントは残す）:

```toml
hkdf = "0.13"
sha2 = "0.11"
p256 = { version = "0.14", features = ["ecdh", "ecdsa"] }
```

Run: `cargo update -p hkdf -p sha2 -p p256 2>&1 | tail -5; cargo check -p mat-device --all-targets --message-format=short 2>&1 | grep -E '^crates/.*error' | head -20`
Expected: エラーは 0〜数件（`SecretKey::from_slice` / `to_encoded_point` / `Sha256::digest` は据え置きで通るはず）。Task 2 Step 4 と同じ方針で直す。

- [ ] **Step 2: I/O なしビルド（CI 相当）も通す**

Run: `cargo check -p mat-device --no-default-features 2>&1 | tail -2`
Expected: `Finished`（`core` は I/O フリーで、暗号だけ使う）。

- [ ] **Step 3: 依存ツリーが単一系列になったことを確認**

Run: `cargo tree --workspace -d -e normal 2>&1 | grep -E '^(digest|generic-array|elliptic-curve|p256|sha2|hkdf|hmac|crypto-common|getrandom|rand_core) ' | sort -u`
Expected: これらのクレートが **重複行として出ない**（`-d` は重複のみ表示）。特に `generic-array` と `digest v0.10` と `getrandom v0.2` はツリーから消えている（getrandom 0.2 は rand_core 0.6 経由で残っていたが、p256 0.14 → rand_core 0.9 で解消する）。残った重複があれば `cargo tree -i <crate>@<version>` で持ち込み元を特定し、本計画の範囲外（bluer 等）ならそのまま報告に書く。

Run: `grep -A1 'name = "aes"' Cargo.lock`
Expected: `version = "0.9.1"` 以上（0.9.0 は yanked）。

- [ ] **Step 4: 全テスト + `task check`**

Run: `task check`
Expected: exit 0。`cargo test --workspace` の中で Task 1 のゴールデン 5 件、`pase_self_handshake` / `case_self_handshake` / `btp_pase_plumbing`（mat-controller/tests）、mat-device の `case_establish` / `self_commission_live` / `group_receive` / `group_provision` が PASS。

- [ ] **Step 5: `task semver` で public API が壊れていないことを確認**

Run: `task semver 2>&1 | tail -15`
Expected: 7 クレートとも break なし。**注意**: `mat-controller` の public 関数が `p256::SecretKey` / `Scalar` を露出している（`case::random_p256_secret`、`x509::generate_csr`、`spake2p::derive_w0_w1` など）ため、依存の major が変わるとこれらは技術的に break 扱いになる可能性がある。出た場合は CLAUDE.md の publish ルール（regular release は minor のまま、break は release notes に記載）に従い、報告に **「どのシンボルが p256 0.14 の型を露出しているか」** を列挙して終える。コードで隠そうとしない。

- [ ] **Step 6: コミット**

```bash
git add crates/mat-device/Cargo.toml Cargo.lock crates/mat-device/src
git commit -m "chore(deps): mat-device の hkdf/sha2/p256 を mat-controller と同じ新系列へ（workspace 単一系列、generic-array / digest 0.10 / getrandom 0.2 消滅）"
```

---

### Task 4: オフライン E2E（matv 相手）で PASE / CASE / CCM / group を実配線で確認

**Files:**
- Test only（変更なし）: `scripts/e2e-device-m1.sh`、`scripts/e2e-device-m3.sh`、`scripts/e2e-device-m4.sh`
- Modify: `ARCHITECTURE.md`（「記録」節の末尾に 1 段落）

**Interfaces:**
- Consumes: Task 3 までのビルド成果（`target/release/{mat,matd,matv}` は各スクリプトが `cargo build --release` する）。
- Produces: 実配線（UDP + mDNS + PASE + CASE + AES-CCM + groupcast）での合格ログ、ARCHITECTURE.md の記録。

M1 = mat が matv を commission（PASE → attestation → AddNOC → CASE）→ unpair。M3 = group provision（KVS）→ matd 常駐 Subscribe → listen → events。M4 = IPK rotate（HKDF 由来の group 鍵が両側で一致することの検証）。この 3 本で暗号パスを全部通る。

- [ ] **Step 1: 前提を確認**

Run: `ip -br link | grep -E '^eth1 .*UP'; task --list | grep -E 'e2e:device:m[134]'`
Expected: `eth1 UP`（既定 iface。違う NIC なら `MAT_E2E_IFACE=<name>` を前置き）と 3 タスクが出る。

- [ ] **Step 2: M1 を回す**

Run: `task e2e:device:m1 2>&1 | tail -30`
Expected: 末尾に成功メッセージ（スクリプト内の最終 `echo`、`PASS` / `ok` 系）と exit 0。失敗したら `RUST_LOG=debug` で再実行し、`pase` / `case` / `crypto` のどの段で落ちたかをログで特定して報告（暗号の取り違えなら Task 2 / 3 に戻す。ゴールデンが通っているのに実配線で落ちる場合は nonce / AAD の組み立てでなく **接続や iface の問題** をまず疑う）。

- [ ] **Step 3: M3 と M4 を回す**

Run: `task e2e:device:m3 2>&1 | tail -30 && task e2e:device:m4 2>&1 | tail -30`
Expected: 両方 exit 0。M3 の events 脚（switch / booleanstate）と M4 の `rotated` → 新 IPK で `on` / `group invoke` 成功まで含めて PASS。

- [ ] **Step 4: ARCHITECTURE.md に記録を 1 段落追加**

`ARCHITECTURE.md` の記録節（Phase 5 以降の日付付き記録が並ぶ箇所。`grep -n '2026-09-0' ARCHITECTURE.md` で最後の記録の直後）に追加:

```markdown
- **2026-09-09 RustCrypto 新系列へ**: sha1/sha2 0.11、hmac/hkdf/pbkdf2 0.13、aes 0.9、ccm 0.6、p256 0.14（digest 0.11 / cipher 0.5 / aead 0.6 / elliptic-curve 0.14、generic-array → hybrid-array）に一括更新。更新前に採取したゴールデン 5 件（CCM 暗号文・ECDSA 署名・SPAKE2+ w0/w1・CASE セッション鍵・group privacy 鍵）と RFC 9383 / spec ベクタで暗号出力の不変を固定し、matv 相手の E2E M1/M3/M4 で PASE・CASE・CCM・groupcast の実配線を確認。呼び出しコードのロジック変更なし（型変換の `.into()` / import の追従のみ）。
```

- [ ] **Step 5: `task check` → コミット**

Run: `task check`
Expected: exit 0（doc:check が ARCHITECTURE.md には効かないが、念のため全体を回す）。

```bash
git add ARCHITECTURE.md
git commit -m "docs(architecture): RustCrypto 新系列への更新記録（ゴールデン 5 件 + E2E M1/M3/M4 合格）"
```

---

### Task 5（オーケストレーター、リリース時）: 実機スモーク

計画の実装タスクではなく、main マージ後のリリース手順の一部として記す。実行主体はオーケストレーター（このセッション）で、subagent には振らない。

- hogar への本番デプロイ（despliegue skill: hogar-iac の MAT_REF pin → build → deploy）後、`ssh nas 'docker exec hogar-matd matd --socket /run/matd/matd.sock status'` で 19/19 established（CASE 全ノード成功 = ECDH / HKDF / CCM の実機互換）。
- `mat read` の warm 経路 1 件と groupcast `off` / `on`（group 鍵 = HKDF 由来）1 往復。
- 可能なら 1 台の再 commission（PASE = SPAKE2+ / PBKDF2 の実機互換）。jarvis の BLE 用 mat も同時に更新する。
- 合格したら v1.37.0 として crates.io publish（`task semver` の break 一覧を release notes に記載、minor のまま）。

---

## Self-Review

- **Spec coverage**: 合意事項 3 点（同時バンプ = Task 2/3、ゴールデンで不変証明 = Task 1 + 各タスクの Step、実機 E2E = Task 4（オフライン実配線）+ Task 5（本番））をすべてタスクに落とした。
- **Placeholder scan**: Task 1 の `<…>` は Step 2 で採取する値のプレースホルダで、実装者が採取して埋める手順が Step 2 に明記されている（意図的）。それ以外に TBD / TODO なし。
- **Type consistency**: `derive_session_keys` の戻り `SessionKeys { i2r, r2i, attestation_challenge }`（case.rs:337-339 で確認）、`derive_privacy_key(&[u8;16]) -> [u8;16]`、`derive_w0_w1 -> (Scalar, Scalar)` は現行シグネチャどおり。Task 2 / 3 でシグネチャは変えないので Task 1 のテストがそのまま Task 3 の合否判定になる。
