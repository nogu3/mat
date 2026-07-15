# Phase 5 M6a: on-network commissioning native 化 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** mat-controller に on-network commissioning（PASE / attestation / NOC 発行 / native open-window）の完全な native 実装を持たせ、ローカル all-clusters-app と jarvis 実機（使い捨て第二 fabric）の E2E で実証する。

**Architecture:** PASE は既存 session/exchange 基盤の「もう一つの鍵導出」として統合する（SPAKE2+ で `SessionKeys` を作り既存 `SecureSession::new` に注入、IM は既存 encode/decode を再利用）。commissioning はステップマシン（ArmFailSafe → attestation → CSR → AddTrustedRoot → AddNOC → CASE → CommissioningComplete）。attestation は DAC→PAI→PAA チェーン・nonce・署名が厳格、CD は warn。本番 `mat commission` / matd は無変更（ライブラリ + E2E のみ）。

**Tech Stack:** Rust / tokio、p256（群演算・ECDSA）、hkdf / hmac / sha2、pbkdf2（新規 dep）、既存 mat-controller モジュール（tlv / message / exchange / session / im / case / cert / fabric / kvs / dnssd / asn1 / crypto）。

**Spec:** `docs/superpowers/specs/2026-07-13-phase5-m6a-commissioning-design.md`（決定 1〜7 を必ず読むこと）

## Global Constraints

- 作業場所は **worktree `.claude/worktrees/phase5-m1-controller-core`（ブランチ `matter-controller`）**。サブエージェントの shell はメイン repo（main）で始まるため、**各タスク冒頭で必ず `pwd` と `git branch --show-current` を検証**し、worktree に `cd` してから作業する。main へのマージ・main 上での編集は禁止。
- コミット前に `task check`（fmt:check + clippy -D warnings + 全テスト）を通す。
- リポジトリは public。実在の IP / node_id / 証明書 / 鍵をコミットしない（テストは生成物 or RFC 公開ベクタのみ）。
- doc comment / コメントは既存モジュールに合わせ日本語基調、spec 条項参照（例: `spec §3.10`）を添える。
- プロトコル実装は `crates/mat-controller` のみに置く（design rule 1）。`crates/mat` / `crates/matd` には触れない。
- `session.rs` / `exchange.rs` / `im.rs` の変更は Task 7 に列挙した追加のみ（既存関数のシグネチャ変更禁止 — M4/M5 のホットパスを壊さない）。
- 乱数はすべて `getrandom::getrandom`。秘密鍵・IPK を持つ構造体に `#[derive(Debug)]` を付けない（`FabricCredentials` の手動 Debug と同じ理由）。

---

### Task 1: `setup_code.rs` — manual pairing code / QR payload の parse・生成

**Files:**
- Create: `crates/mat-controller/src/setup_code.rs`
- Modify: `crates/mat-controller/src/lib.rs`（`pub mod setup_code;` を追加）

**Interfaces:**
- Produces:
  - `pub struct SetupPayload { pub version: u8, pub vendor_id: u16, pub product_id: u16, pub custom_flow: u8, pub discovery_capabilities: u8, pub discriminator: u16 /* 12-bit long */, pub passcode: u32 }`
  - `pub struct ManualCode { pub passcode: u32, pub short_discriminator: u8 /* 4-bit */ }`
  - `pub enum SetupCodeError { BadLength, BadChar, BadCheckDigit, BadPrefix, ZeroPasscode }`（`Display` 実装付き）
  - `pub fn parse_qr(s: &str) -> Result<SetupPayload, SetupCodeError>`（`MT:` プレフィックス必須）
  - `pub fn encode_qr(p: &SetupPayload) -> String`
  - `pub fn parse_manual_code(s: &str) -> Result<ManualCode, SetupCodeError>`（11 桁 / 21 桁両対応、VID/PID は読み捨て）
  - `pub fn encode_manual_code(passcode: u32, short_discriminator: u8) -> String`（11 桁）

- [ ] **Step 1: 失敗するテストを書く**

`setup_code.rs` の末尾に（実装より先にテストだけ書いてもコンパイルが通るよう、最初は型と `todo!()` スタブも一緒に置いてよい — ただし必ず一度 FAIL を確認する）:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // 周知のテストペア: chip 全クラスタアプリの onboarding payload。
    // passcode 20202021 / long discriminator 3840 / VID 0xFFF1 / PID 0x8001。
    const QR: &str = "MT:-24J0AFN00KA0648G00";

    #[test]
    fn parses_known_qr() {
        let p = parse_qr(QR).unwrap();
        assert_eq!(p.version, 0);
        assert_eq!(p.vendor_id, 0xFFF1);
        assert_eq!(p.product_id, 0x8001);
        assert_eq!(p.custom_flow, 0);
        assert_eq!(p.discovery_capabilities, 0x04); // on-network
        assert_eq!(p.discriminator, 3840);
        assert_eq!(p.passcode, 20202021);
    }

    #[test]
    fn qr_round_trip() {
        let p = parse_qr(QR).unwrap();
        assert_eq!(encode_qr(&p), QR);
    }

    // 周知のテストペア: manual code 34970112332
    // = passcode 20202021 / short discriminator 15。
    #[test]
    fn parses_known_manual_code() {
        let m = parse_manual_code("34970112332").unwrap();
        assert_eq!(m.passcode, 20202021);
        assert_eq!(m.short_discriminator, 15);
    }

    #[test]
    fn manual_code_round_trip() {
        assert_eq!(encode_manual_code(20202021, 15), "34970112332");
    }

    #[test]
    fn manual_code_rejects_bad_check_digit() {
        assert!(matches!(
            parse_manual_code("34970112331"),
            Err(SetupCodeError::BadCheckDigit)
        ));
    }

    #[test]
    fn qr_rejects_missing_prefix() {
        assert!(matches!(
            parse_qr("-24J0AFN00KA0648G00"),
            Err(SetupCodeError::BadPrefix)
        ));
    }
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-controller setup_code -- --nocapture`
Expected: FAIL（`todo!()` panic またはコンパイルエラー → スタブを置いて panic まで持っていく）

- [ ] **Step 3: 実装**

仕様（spec §5.1.3〜§5.1.4）:

**base38**（QR）: アルファベット `0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ-.`。バイト列を 3 バイトずつの group にし、group ごとに little-endian の u32 値を base38 で 5 文字（余り 2 バイト → 4 文字、1 バイト → 2 文字）。ビット詰めは **LSB-first** で計 88 bit = 11 バイト: version(3) / vendor_id(16) / product_id(16) / custom_flow(2) / discovery_capabilities(8) / discriminator(12) / passcode(27) / padding(4)。parse は 11 バイト以上を要求し、先頭 11 バイトのみ使う（以降の optional TLV は読み捨て）。

**manual code**（11 桁）:
- digit1 = `(vid_pid_present << 2) | (short_disc >> 2)`（本実装の encode は vid_pid_present=0 固定）
- digit2..6 = `((short_disc & 0x3) << 14) | (passcode & 0x3FFF)` を 10 進 5 桁ゼロ詰め
- digit7..10 = `passcode >> 14` を 10 進 4 桁ゼロ詰め
- digit11 = 先頭 10 桁への Verhoeff チェックディジット
- 21 桁は digit1 の bit2 が 1 で、digit11..20 に VID/PID（各 5 桁）、digit21 がチェック。parse は VID/PID を読み捨てて同じ `ManualCode` を返す。

**Verhoeff**: 標準の d / p / inv テーブルをそのまま定数化する:

```rust
const D: [[u8; 10]; 10] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
    [1, 2, 3, 4, 0, 6, 7, 8, 9, 5],
    [2, 3, 4, 0, 1, 7, 8, 9, 5, 6],
    [3, 4, 0, 1, 2, 8, 9, 5, 6, 7],
    [4, 0, 1, 2, 3, 9, 5, 6, 7, 8],
    [5, 9, 8, 7, 6, 0, 4, 3, 2, 1],
    [6, 5, 9, 8, 7, 1, 0, 4, 3, 2],
    [7, 6, 5, 9, 8, 2, 1, 0, 4, 3],
    [8, 7, 6, 5, 9, 3, 2, 1, 0, 4],
    [9, 8, 7, 6, 5, 4, 3, 2, 1, 0],
];
const P: [[u8; 10]; 8] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
    [1, 5, 7, 6, 2, 8, 3, 0, 9, 4],
    [5, 8, 0, 3, 7, 9, 6, 1, 4, 2],
    [8, 9, 1, 6, 0, 4, 3, 5, 2, 7],
    [9, 4, 5, 3, 1, 2, 6, 8, 7, 0],
    [4, 2, 8, 6, 5, 7, 3, 9, 0, 1],
    [2, 7, 9, 3, 8, 0, 6, 4, 1, 5],
    [7, 0, 4, 6, 9, 1, 3, 2, 5, 8],
];
const INV: [u8; 10] = [0, 4, 3, 2, 1, 5, 6, 7, 8, 9];

/// 検査桁の生成: payload（検査桁を含まない）に対し右端から位置 1,2,... で畳み込む。
fn verhoeff_check_digit(payload: &[u8]) -> u8 {
    let mut c = 0u8;
    for (i, d) in payload.iter().rev().enumerate() {
        c = D[usize::from(c)][usize::from(P[(i + 1) % 8][usize::from(*d)])];
    }
    INV[usize::from(c)]
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-controller setup_code`
Expected: PASS（7 テスト）

- [ ] **Step 5: `task check` → コミット**

```bash
task check
git add crates/mat-controller/src/setup_code.rs crates/mat-controller/src/lib.rs
git commit -m "feat(controller): setup code parse/生成 (manual Verhoeff / QR base38) (M6a Task1)"
```

---

### Task 2: `spake2p.rs` — SPAKE2+ (P-256) prover と verifier 素材

**Files:**
- Create: `crates/mat-controller/src/spake2p.rs`
- Modify: `crates/mat-controller/src/lib.rs`（`pub mod spake2p;`）
- Modify: `crates/mat-controller/Cargo.toml`（`pbkdf2 = { version = "0.12", default-features = false }` を追加）

**Interfaces:**
- Consumes: `crypto.rs` は使わず p256 / hkdf / hmac / sha2 / pbkdf2 を直接使う（CASE の `case.rs` と同様の低レベル層）。
- Produces:
  - `pub enum SpakeError { BadPoint, Identity }`（`Display` 付き）
  - `pub fn derive_w0_w1(passcode: u32, salt: &[u8], iterations: u32) -> (p256::Scalar, p256::Scalar)`
  - `pub fn compute_verifier(passcode: u32, salt: &[u8], iterations: u32) -> [u8; 97]`（`w0(32 BE) || L(65 uncompressed)`、open-window 用）
  - `pub struct Spake2pProver`（`fn new(w0, w1) -> Self` / `fn p_a(&self) -> [u8; 65]` / `fn finish(&self, p_b: &[u8], context: &[u8], id_p: &[u8], id_v: &[u8]) -> Result<PakeShared, SpakeError>`）
  - `pub struct PakeShared { pub c_a: [u8; 32], pub expected_c_b: [u8; 32], pub k_e: [u8; 16] }`

- [ ] **Step 1: dep 追加と失敗するテストを書く**

`Cargo.toml` の `[dependencies]` に `pbkdf2 = { version = "0.12", default-features = false }` を追加。

RFC 9383 の P256-SHA256-HKDF-SHA256-HMAC-SHA256 テストベクタを取得して定数化する:

```bash
curl -fsSL https://www.rfc-editor.org/rfc/rfc9383.txt -o /tmp/rfc9383.txt
grep -n 'SPAKE2+-P256-SHA256-HKDF-SHA256-HMAC-SHA256 Test Vectors' /tmp/rfc9383.txt
```

該当節から **Context / idProver / idVerifier / w0 / w1 / x / shareP / y / shareV / Z / V / TT** の hex をテストに転記する（`TT` は複数行 — 連結すること）。テストの骨子:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    fn h(s: &str) -> Vec<u8> {
        // 空白除去付き hex デコード（テスト専用の素朴な実装で良い）
        let s: String = s.chars().filter(|c| !c.is_whitespace()).collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// RFC 9383 Appendix C (P256-SHA256-HKDF-SHA256-HMAC-SHA256) のベクタ。
    /// 値は rfc9383.txt から転記（実装時に取得して埋める。placeholder のまま
    /// コミットしないこと）。
    #[test]
    fn rfc9383_p256_vector() {
        let w0 = scalar_from_be_bytes_mod_n(&h("…w0…"));
        let w1 = scalar_from_be_bytes_mod_n(&h("…w1…"));
        let x = scalar_from_be_bytes_mod_n(&h("…x…"));
        let prover = Spake2pProver::new_with_x(w0, w1, x);
        assert_eq!(prover.p_a().to_vec(), h("…shareP…"));
        // TT の中身（Z/V を含む）はベクタの TT と一致するはず。
        let tt = prover
            .transcript(&h("…shareV…"), &h("…Context…"), b"client", b"server")
            .unwrap();
        assert_eq!(tt, h("…TT…"));
    }

    /// 自己整合: テスト内にミニマムな verifier 役を実装し、prover と突き合わせる。
    /// (Matter 固有の Ka/Ke/確認キー分割は RFC ベクタでは検証できないため、
    ///  両役を自前実装して cA/cB/Ke が一致することを確認する)
    #[test]
    fn prover_and_test_verifier_agree() {
        let (w0, w1) = derive_w0_w1(20202021, b"SPAKE2P Key Salt", 1000);
        let prover = Spake2pProver::new(w0, w1);
        let l = ProjectivePoint::GENERATOR * w1;
        // verifier 役 (デバイス側): y乱数, pB = y*P + w0*N
        let y = random_scalar();
        let n = decode_point(SPAKE_N).unwrap();
        let m = decode_point(SPAKE_M).unwrap();
        let p_b_point = ProjectivePoint::GENERATOR * y + n * w0;
        let p_b = encode_point(&p_b_point);
        let ctx = b"test context";
        let shared = prover.finish(&p_b, ctx, b"", b"").unwrap();
        // verifier 側で同じ TT を組む: Z = y*(pA - w0*M), V = y*L
        let p_a_point = decode_point(&prover.p_a()).unwrap();
        let z = (p_a_point - m * w0) * y;
        let v = l * y;
        let tt = build_transcript(ctx, b"", b"", &prover.p_a(), &p_b, &z, &v, &w0);
        let (k_a, k_e) = split_hash(&tt);
        let (kc_a, kc_b) = confirmation_keys(&k_a);
        assert_eq!(shared.k_e, k_e);
        assert_eq!(shared.c_a, hmac32(&kc_a, &p_b));
        assert_eq!(shared.expected_c_b, hmac32(&kc_b, &prover.p_a()));
    }

    #[test]
    fn verifier_bytes_are_w0_l() {
        let v = compute_verifier(20202021, b"salt-0123456789abcdef", 1000);
        let (w0, w1) = derive_w0_w1(20202021, b"salt-0123456789abcdef", 1000);
        assert_eq!(&v[..32], w0.to_bytes().as_slice());
        let l = ProjectivePoint::GENERATOR * w1;
        assert_eq!(&v[32..], encode_point(&l).as_slice());
    }

    #[test]
    fn rejects_bad_peer_point() {
        let (w0, w1) = derive_w0_w1(1, b"s", 1000);
        let p = Spake2pProver::new(w0, w1);
        assert!(matches!(p.finish(&[0u8; 65], b"c", b"", b""), Err(SpakeError::BadPoint)));
    }
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-controller spake2p`
Expected: FAIL（未実装）

- [ ] **Step 3: 実装**

```rust
//! SPAKE2+ (P256-SHA256-HKDF-HMAC) — Matter spec §3.10 / RFC 9383。
//! controller は常に prover（A 側）。verifier 素材の計算（w0/L）は
//! open-window の PAKEPasscodeVerifier 生成にだけ使う。

use hmac::{Hmac, Mac};
use p256::elliptic_curve::sec1::{FromEncodedPoint, ToEncodedPoint};
use p256::elliptic_curve::PrimeField;
use p256::{AffinePoint, EncodedPoint, ProjectivePoint, Scalar};
use sha2::{Digest, Sha256};

/// RFC 9383 §4 の P-256 用固定点（compressed SEC1）。
pub const SPAKE_M: [u8; 33] =
    hex_literal(  // ← hex_literal クレートは使わない。以下のバイト列を直書きする
    );
```

**定数はバイト配列直書き**（依存を増やさない）。値:

```
M = 02 88 6e 2f 97 ac e4 6e 55 ba 9d d7 24 25 79 f2 99 3b 64 e1 6e f3 dc ab 95 af d4 97 33 3d 8f a1 2f
N = 03 d8 bb d6 c6 39 c6 29 37 b0 4d 99 7f 38 c3 77 07 19 c6 29 d7 01 4d 49 a2 4b 4f 98 ba a1 29 2b 49
```

主要関数:

```rust
/// 40 バイト big-endian を曲線位数 n で還元して Scalar にする。
/// (Scalar 演算は mod n なので、バイトごとの畳み込みで正確に還元できる)
pub(crate) fn scalar_from_be_bytes_mod_n(bytes: &[u8]) -> Scalar {
    let b256 = Scalar::from(256u64);
    bytes
        .iter()
        .fold(Scalar::ZERO, |acc, b| acc * b256 + Scalar::from(u64::from(*b)))
}

/// spec §3.10: PBKDF2-SHA256(passcode の u32 LE, salt, iterations) 80 バイト
/// → 前半 40B が w0s、後半 40B が w1s。それぞれ mod n。
pub fn derive_w0_w1(passcode: u32, salt: &[u8], iterations: u32) -> (Scalar, Scalar) {
    let mut ws = [0u8; 80];
    pbkdf2::pbkdf2_hmac::<Sha256>(&passcode.to_le_bytes(), salt, iterations, &mut ws);
    (
        scalar_from_be_bytes_mod_n(&ws[..40]),
        scalar_from_be_bytes_mod_n(&ws[40..]),
    )
}

pub fn compute_verifier(passcode: u32, salt: &[u8], iterations: u32) -> [u8; 97] {
    let (w0, w1) = derive_w0_w1(passcode, salt, iterations);
    let l = ProjectivePoint::GENERATOR * w1;
    let mut out = [0u8; 97];
    out[..32].copy_from_slice(&w0.to_bytes());
    out[32..].copy_from_slice(&encode_point(&l));
    out
}

fn decode_point(bytes: &[u8]) -> Result<ProjectivePoint, SpakeError> {
    let ep = EncodedPoint::from_bytes(bytes).map_err(|_| SpakeError::BadPoint)?;
    let ap = Option::<AffinePoint>::from(AffinePoint::from_encoded_point(&ep))
        .ok_or(SpakeError::BadPoint)?;
    let p = ProjectivePoint::from(ap);
    if p == ProjectivePoint::IDENTITY {
        return Err(SpakeError::Identity);
    }
    Ok(p)
}

fn encode_point(p: &ProjectivePoint) -> [u8; 65] {
    p.to_affine().to_encoded_point(false).as_bytes().try_into().expect("uncompressed")
}

/// TT の要素: u64 LE の長さ || バイト列（RFC 9383 §3.3）。
fn tt_elem(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    out.extend_from_slice(bytes);
}
```

`Spake2pProver::finish`（`transcript` は TT の Vec<u8> を返す内部関数としてテスト用に分離）:

1. `p_b` を decode（失敗 → `BadPoint`）。
2. `t = p_b_point - n * w0`、`z = t * x`、`v = t * w1`。
3. TT = tt_elem 連結: context, id_p, id_v, M(65B uncompressed), N(65B), pA, pB, Z(65B), V(65B), w0(32B BE)。
4. `hash = SHA256(TT)`; `k_a = hash[..16]`, `k_e = hash[16..32]`（Matter 固有の分割、spec §3.10.3）。
5. `kc = HKDF-SHA256(salt=[], ikm=k_a, info=b"ConfirmationKeys", 32B)`; `kc_a = kc[..16]`, `kc_b = kc[16..]`。
6. `c_a = HMAC-SHA256(kc_a, p_b)`, `expected_c_b = HMAC-SHA256(kc_b, p_a)`。

`new` は `x` を乱数（`case::random_p256_secret` と同様に `getrandom` → `Scalar`; 0 は引き直し）、`new_with_x` は `pub(crate)`＋`#[cfg(test)]` でなく通常の `pub(crate)`（RFC ベクタテストで使用）。`p_a = P*x + M*w0`。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-controller spake2p`
Expected: PASS（RFC ベクタ含む 4 テスト）。ベクタ hex が `…` のままなら FAIL させること（転記漏れ防止）。

- [ ] **Step 5: `task check` → コミット**

```bash
task check
git add crates/mat-controller/src/spake2p.rs crates/mat-controller/src/lib.rs crates/mat-controller/Cargo.toml Cargo.lock
git commit -m "feat(controller): SPAKE2+ P-256 prover/verifier素材 (RFC 9383ベクタ付き) (M6a Task2)"
```

---

### Task 3: `pase.rs` — PASE ハンドシェイク → `SecureSession`

**Files:**
- Create: `crates/mat-controller/src/pase.rs`
- Modify: `crates/mat-controller/src/lib.rs`（`pub mod pase;`）
- Modify: `crates/mat-controller/src/session.rs`（`attestation_challenge` アクセサ 1 個のみ追加）
- Modify: `crates/mat-controller/src/case.rs`（`fn random_nonzero_u16` を `pub(crate)` にする。他は触らない）

**Interfaces:**
- Consumes: `spake2p::{derive_w0_w1, Spake2pProver}`、`exchange::{UnsecuredExchange, MrpConfig}`、`message::{PROTOCOL_ID_SECURE_CHANNEL, OPCODE_STATUS_REPORT}`、`case::parse_status_report`（`pub(crate)` 済み）、`tlv::{Writer, Reader, Tag, Value}`。
- Produces:
  - `pub enum PaseError { Exchange(ExchangeError), Malformed(&'static str), Spake(SpakeError), ConfirmMismatch, StatusReport { general_code: u16, protocol_code: u16 }, NotAcked }`（`Display` 付き。`ConfirmMismatch` = passcode 不一致の代表形）
  - `pub async fn establish(transport: Arc<UdpTransport>, peer: SocketAddr, passcode: u32, cfg: &MrpConfig) -> Result<SecureSession, PaseError>`
  - session.rs: `pub fn attestation_challenge(&self) -> [u8; 16]`

- [ ] **Step 1: 失敗するテストを書く（メッセージ codec のユニットテスト）**

ハンドシェイク全体はライブテスト（Task 11）で検証する。ここでは codec を先にテスト:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tlv::{Reader, Tag, Value};

    #[test]
    fn pbkdf_param_request_shape() {
        let req = encode_pbkdf_param_request(&[7u8; 32], 0x1234);
        let mut r = Reader::new(&req);
        // struct{1: rand[32], 2: session_id, 3: passcode_id=0, 4: has_params=false}
        assert!(matches!(r.next().unwrap().unwrap().value, Value::StructStart));
        let e = r.next().unwrap().unwrap();
        assert_eq!(e.tag, Tag::Context(1));
        assert!(matches!(e.value, Value::Bytes(b) if b == [7u8; 32]));
        let e = r.next().unwrap().unwrap();
        assert!(matches!(e.value, Value::Uint(0x1234)));
        let e = r.next().unwrap().unwrap();
        assert!(matches!(e.value, Value::Uint(0)));
        let e = r.next().unwrap().unwrap();
        assert!(matches!(e.value, Value::Bool(false)));
    }

    #[test]
    fn parses_pbkdf_param_response() {
        // レスポンダの応答を Writer で合成して decode を検証
        let mut w = crate::tlv::Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(1), &[7u8; 32]);
        w.put_bytes(Tag::Context(2), &[8u8; 32]);
        w.put_uint(Tag::Context(3), 0xBEEF);
        w.start_struct(Tag::Context(4));
        w.put_uint(Tag::Context(1), 1000);
        w.put_bytes(Tag::Context(2), b"0123456789abcdef");
        w.end_container();
        w.end_container();
        let resp = decode_pbkdf_param_response(&w.finish()).unwrap();
        assert_eq!(resp.responder_session_id, 0xBEEF);
        assert_eq!(resp.iterations, 1000);
        assert_eq!(resp.salt, b"0123456789abcdef");
    }

    #[test]
    fn pake1_and_pake3_shape() {
        let p1 = encode_pake1(&[3u8; 65]);
        let mut r = Reader::new(&p1);
        assert!(matches!(r.next().unwrap().unwrap().value, Value::StructStart));
        assert!(matches!(r.next().unwrap().unwrap().value, Value::Bytes(b) if b.len() == 65));
        let p3 = encode_pake3(&[4u8; 32]);
        let mut r = Reader::new(&p3);
        assert!(matches!(r.next().unwrap().unwrap().value, Value::StructStart));
        assert!(matches!(r.next().unwrap().unwrap().value, Value::Bytes(b) if b == [4u8; 32]));
    }

    #[test]
    fn parses_pake2() {
        let mut w = crate::tlv::Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(1), &[9u8; 65]);
        w.put_bytes(Tag::Context(2), &[10u8; 32]);
        w.end_container();
        let (p_b, c_b) = decode_pake2(&w.finish()).unwrap();
        assert_eq!(p_b, [9u8; 65]);
        assert_eq!(c_b, [10u8; 32]);
    }
}
```

- [ ] **Step 2: FAIL 確認**

Run: `cargo test -p mat-controller pase`
Expected: FAIL

- [ ] **Step 3: 実装**

opcode（Secure Channel、spec §4.13）:

```rust
pub(crate) const OPCODE_PBKDF_PARAM_REQUEST: u8 = 0x20;
pub(crate) const OPCODE_PBKDF_PARAM_RESPONSE: u8 = 0x21;
pub(crate) const OPCODE_PASE_PAKE1: u8 = 0x22;
pub(crate) const OPCODE_PASE_PAKE2: u8 = 0x23;
pub(crate) const OPCODE_PASE_PAKE3: u8 = 0x24;
```

TLV（すべて anonymous struct、context tag）:
- PBKDFParamRequest: `{1: initiatorRandom[32], 2: initiatorSessionId u16, 3: passcodeId=0, 4: hasPBKDFParameters=false}`（tag5 の SessionParams は送らない）
- PBKDFParamResponse: `{1: initiatorRandom, 2: responderRandom[32], 3: responderSessionId u16, 4: struct{1: iterations u32, 2: salt bytes}}`（tag5 は読み捨て）。`struct PbkdfParamResponse { pub responder_session_id: u16, pub iterations: u32, pub salt: Vec<u8> }`
- Pake1: `{1: pA[65]}` / Pake2: `{1: pB[65], 2: cB[32]}` / Pake3: `{1: cA[32]}`

`establish` は `case::establish`（case.rs:423 以降）の交換パターンを踏襲する（ack 検証・standalone-ack 後の recv 待ち・StatusReport 分岐を同じ形で）:

1. `initiator_random` 32B 乱数、`local_session_id = random_nonzero_u16()`。
2. `req` 送信（opcode 0x20）→ 応答 0x21 期待（0x40 StatusReport なら `StatusReport` エラー、ack 不一致は `NotAcked`）。
3. `context = SHA256(b"CHIP PAKE V1 Commissioning" || req || resp_payload)`。
4. `(w0, w1) = derive_w0_w1(passcode, &resp.salt, resp.iterations)`; `prover = Spake2pProver::new(w0, w1)`; Pake1 送信（0x22）→ Pake2 受信（0x23）。
5. `shared = prover.finish(&p_b, &context, b"", b"")`; `shared.expected_c_b != c_b` → `ConfirmMismatch`。Pake3（0x24, cA）送信 → StatusReport 受信、`parse_status_report` で success（general_code=0, protocol_code=0）以外は `StatusReport` エラー。
6. セッション鍵: `HKDF-SHA256(salt=[], ikm=shared.k_e_full …)` — **注意**: spec §4.13.2.3 では SessionKeys の KDF 入力は **Ke**。`PakeShared.k_e` は 16B（TT ハッシュ後半）なのでそれを ikm に `info=b"SessionKeys"` で 48B 出力 → `SessionKeys { i2r, r2i, attestation_challenge }`。
7. `SecureSession::new(transport, peer, local_session_id, resp.responder_session_id, keys, 0, 0)`（PASE のノード id は両側 0、spec §4.13: unauthenticated）。

session.rs に追加（これだけ）:

```rust
    /// PASE で確立したセッションの Attestation Challenge (spec §11.17.5.4 が
    /// attestation 署名の対象に含める)。
    pub fn attestation_challenge(&self) -> [u8; 16] {
        self.keys.attestation_challenge
    }
```

- [ ] **Step 4: PASS 確認**

Run: `cargo test -p mat-controller pase`
Expected: PASS（codec 4 テスト）

- [ ] **Step 5: `task check` → コミット**

```bash
task check
git add crates/mat-controller/src/pase.rs crates/mat-controller/src/session.rs crates/mat-controller/src/case.rs crates/mat-controller/src/lib.rs
git commit -m "feat(controller): PASEハンドシェイク→SecureSession注入 (M6a Task3)"
```

---

### Task 4: `x509.rs` — 最小 DER リーダと X.509 / CSR 解析

**Files:**
- Create: `crates/mat-controller/src/x509.rs`
- Modify: `crates/mat-controller/src/lib.rs`（`pub mod x509;`）
- Modify: `crates/mat-controller/src/asn1.rs`（`pub fn oid(content: &[u8]) -> Vec<u8>`（tag 0x06）を追加 — テストの証明書合成にも使う）

**Interfaces:**
- Consumes: `crypto::verify_ecdsa_p256(public_key: &[u8; 65], message: &[u8], signature: &[u8; 64])`（message は内部で SHA-256 される）、`asn1.rs`（テストでの DER 合成）。
- Produces:
  - `pub enum X509Error { Der(&'static str), UnsupportedAlg, BadSignature, BadPublicKey }`（`Display` 付き）
  - `pub struct X509Cert { pub tbs: Vec<u8>, pub public_key: [u8; 65], pub issuer: Vec<u8>, pub subject: Vec<u8>, pub signature: [u8; 64], pub skid: Option<Vec<u8>>, pub akid: Option<Vec<u8>>, pub vid: Option<u16>, pub pid: Option<u16> }`（issuer/subject は Name の DER 生バイト。VID/PID は Matter OID 1.3.6.1.4.1.37244.2.1/.2.2 の UTF8String 4-hex、無ければ CN 中の `Mvid:XXXX`/`Mpid:XXXX` fallback）
  - `pub fn parse_x509(der: &[u8]) -> Result<X509Cert, X509Error>`
  - `impl X509Cert { pub fn verify_signed_by(&self, issuer: &X509Cert) -> Result<(), X509Error> }`
  - `pub fn parse_csr(der: &[u8]) -> Result<[u8; 65], X509Error>`（PKCS#10。自己署名を検証し、P-256 公開鍵を返す）
  - テスト用ヘルパ（`#[cfg(test)]` ではなく `pub(crate)`、Task 5 のテストでも使う）: `pub(crate) fn make_test_cert(subject: &[u8], issuer: &[u8], subject_key: &p256::SecretKey, signer_key: &p256::SecretKey, is_ca: bool, vid_pid: Option<(u16, u16)>) -> Vec<u8>`

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::case::random_p256_secret;

    #[test]
    fn parses_and_verifies_self_signed() {
        let key = random_p256_secret();
        let der = make_test_cert(b"root", b"root", &key, &key, true, None);
        let cert = parse_x509(&der).unwrap();
        assert_eq!(cert.issuer, cert.subject);
        cert.verify_signed_by(&cert).unwrap();
    }

    #[test]
    fn verifies_two_level_chain_and_rejects_wrong_issuer() {
        let root = random_p256_secret();
        let leaf = random_p256_secret();
        let other = random_p256_secret();
        let root_der = make_test_cert(b"root", b"root", &root, &root, true, None);
        let leaf_der =
            make_test_cert(b"leaf", b"root", &leaf, &root, false, Some((0xFFF1, 0x8001)));
        let root_c = parse_x509(&root_der).unwrap();
        let leaf_c = parse_x509(&leaf_der).unwrap();
        leaf_c.verify_signed_by(&root_c).unwrap();
        assert_eq!(leaf_c.vid, Some(0xFFF1));
        assert_eq!(leaf_c.pid, Some(0x8001));
        let other_der = make_test_cert(b"other", b"other", &other, &other, true, None);
        let other_c = parse_x509(&other_der).unwrap();
        assert!(leaf_c.verify_signed_by(&other_c).is_err());
    }

    #[test]
    fn parses_csr_and_rejects_tampered() {
        let key = random_p256_secret();
        let csr = make_test_csr(&key); // pub(crate) テストヘルパ（下記）
        let pk = parse_csr(&csr).unwrap();
        assert_eq!(pk.len(), 65);
        let mut bad = csr.clone();
        let n = bad.len();
        bad[n - 1] ^= 0xFF;
        assert!(parse_csr(&bad).is_err());
    }

    #[test]
    fn rejects_truncated_der() {
        assert!(parse_x509(&[0x30, 0x05, 0x01]).is_err());
    }
}
```

- [ ] **Step 2: FAIL 確認**

Run: `cargo test -p mat-controller x509`
Expected: FAIL

- [ ] **Step 3: 実装**

**DerReader**（`asn1.rs` の方針を踏襲: 長さ検査・panic なし・攻撃面はデバイス由来入力）:

```rust
pub(crate) struct DerReader<'a> { buf: &'a [u8], pos: usize }

impl<'a> DerReader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Self { Self { buf, pos: 0 } }
    /// 次の TLV を読み (tag, content, raw_tlv全体) を返す。
    pub(crate) fn read(&mut self) -> Result<(u8, &'a [u8], &'a [u8]), X509Error> { … }
    pub(crate) fn expect(&mut self, tag: u8) -> Result<&'a [u8], X509Error> { … }
    pub(crate) fn peek_tag(&self) -> Option<u8> { … }
    pub(crate) fn is_empty(&self) -> bool { … }
}
```

長さ形式は short / 0x81 / 0x82 のみ受理（`asn1.rs` の writer と対称）。

**parse_x509**: `Certificate ::= SEQ { tbsCertificate SEQ, signatureAlgorithm SEQ, signature BIT STRING }`。
- `tbs` は **raw TLV 全体**（tag+len 込み）を保持（署名対象）。
- TBS 内: `[0] version`（optional）→ serial INT → sigAlg SEQ → issuer（raw 保持）→ validity SEQ → subject（raw 保持）→ SPKI `SEQ { SEQ { OID ecPublicKey, OID prime256v1 }, BIT STRING 0x04||X||Y }`（65B 以外は `BadPublicKey`）→ `[3]` extensions（optional）。
- extensions 走査: `SEQ of SEQ { OID, [BOOL critical,] OCTET STRING value }`。OID `55 1D 0E`（SKID）→ value 内の OCTET STRING 中身。OID `55 1D 23`（AKID）→ SEQ 内の `[0]` keyIdentifier。他は読み捨て。
- signature BIT STRING → 中身が DER ECDSA `SEQ { r INT, s INT }` → 先頭 0x00 を剥がし 32B 左ゼロ詰めで `r||s` 64B。
- VID/PID: subject Name（`SEQ of SET of SEQ { OID, value }`）を走査し、OID `2B 06 01 04 01 82 A2 7C 02 01`（VID）/ `…02 02`（PID）の UTF8String 4-hex を `u16::from_str_radix(_, 16)`。見つからなければ CN（OID `55 04 03`）文字列中の `Mvid:` / `Mpid:` を検索。

**verify_signed_by**: `crypto::verify_ecdsa_p256(&issuer.public_key, &self.tbs, &self.signature)` → err は `BadSignature`。呼び出し側でチェーンの issuer/subject 突き合わせをする（Task 5）。

**parse_csr**: `CertificationRequest ::= SEQ { certificationRequestInfo SEQ, sigAlg SEQ, signature BIT STRING }`。CRI の raw を保持し、CRI 内の SPKI から公開鍵抽出、自己署名を `verify_ecdsa_p256(pk, cri_raw, sig)` で検証。

**make_test_cert / make_test_csr**（`pub(crate)`）: `asn1.rs` の `seq/integer/oid/bit_string/octet_string/utf8_string/printable_string/utc_time/context_constructed/boolean` で最小 DER を合成し、`crypto::sign_ecdsa_p256` で署名。subject/issuer Name は `SEQ{SET{SEQ{OID CN, UTF8String}}}`（+ vid_pid 指定時は Matter VID/PID OID の RDN を追加）。validity は固定文字列（`"260101000000Z"` 等）。CA 用に BasicConstraints/SKID/AKID を付ける（AKID は signer の SKID と同値にする）。

OID 定数（コメント付きで直書き）:

```rust
const OID_EC_PUBLIC_KEY: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01]; // 1.2.840.10045.2.1
const OID_PRIME256V1: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07]; // 1.2.840.10045.3.1.7
const OID_ECDSA_SHA256: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02]; // 1.2.840.10045.4.3.2
const OID_SKID: &[u8] = &[0x55, 0x1D, 0x0E]; // 2.5.29.14
const OID_AKID: &[u8] = &[0x55, 0x1D, 0x23]; // 2.5.29.35
const OID_BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1D, 0x13]; // 2.5.29.19
const OID_CN: &[u8] = &[0x55, 0x04, 0x03]; // 2.5.4.3
const OID_MATTER_VID: &[u8] = &[0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x02, 0x01]; // 1.3.6.1.4.1.37244.2.1
const OID_MATTER_PID: &[u8] = &[0x2B, 0x06, 0x01, 0x04, 0x01, 0x82, 0xA2, 0x7C, 0x02, 0x02]; // 1.3.6.1.4.1.37244.2.2
```

- [ ] **Step 4: PASS 確認**

Run: `cargo test -p mat-controller x509`
Expected: PASS（4 テスト）

- [ ] **Step 5: `task check` → コミット**

```bash
task check
git add crates/mat-controller/src/x509.rs crates/mat-controller/src/asn1.rs crates/mat-controller/src/lib.rs
git commit -m "feat(controller): 最小DERリーダ+X.509/CSR解析 (attestation下地) (M6a Task4)"
```

---

### Task 5: `attestation.rs` — device attestation 検証（チェーン厳格 / CD warn）

**Files:**
- Create: `crates/mat-controller/src/attestation.rs`
- Modify: `crates/mat-controller/src/lib.rs`（`pub mod attestation;`）

**Interfaces:**
- Consumes: `x509::{parse_x509, X509Cert, make_test_cert}`、`crypto::verify_ecdsa_p256`、`tlv::{Reader, Tag, Value}`、`tracing::warn`。
- Produces:
  - `pub enum AttestationError { Chain(&'static str), Nonce, Signature, Elements(&'static str), X509(X509Error), Io(std::io::Error) }`（`Display` 付き。全部 strict = commissioning 中止）
  - `pub fn load_der_dir(dir: &Path) -> Result<Vec<Vec<u8>>, AttestationError>`（`*.der` を全部読む。空 dir は空 Vec）
  - `pub fn verify_device_attestation(dac_der: &[u8], pai_der: &[u8], paa_ders: &[Vec<u8>], cd_signer_ders: &[Vec<u8>], elements: &[u8], signature: &[u8; 64], expected_nonce: &[u8; 32], attestation_challenge: &[u8; 16]) -> Result<(), AttestationError>`

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::case::random_p256_secret;
    use crate::crypto::sign_ecdsa_p256;
    use crate::tlv::{Tag, Writer};
    use crate::x509::make_test_cert;

    struct Fixture {
        dac: Vec<u8>,
        pai: Vec<u8>,
        paa: Vec<u8>,
        dac_key: p256::SecretKey,
    }

    fn chain() -> Fixture {
        let paa_key = random_p256_secret();
        let pai_key = random_p256_secret();
        let dac_key = random_p256_secret();
        let paa = make_test_cert(b"paa", b"paa", &paa_key, &paa_key, true, None);
        let pai = make_test_cert(b"pai", b"paa", &pai_key, &paa_key, true, Some((0xFFF1, 0x8001)));
        let dac = make_test_cert(b"dac", b"pai", &dac_key, &pai_key, false, Some((0xFFF1, 0x8001)));
        Fixture { dac, pai, paa, dac_key }
    }

    fn elements(nonce: &[u8; 32]) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(1), b"fake-cd"); // CD は warn 経路なので偽物で良い
        w.put_bytes(Tag::Context(2), nonce);
        w.put_uint(Tag::Context(3), 0); // timestamp
        w.end_container();
        w.finish()
    }

    fn sign(fix: &Fixture, elements: &[u8], challenge: &[u8; 16]) -> [u8; 64] {
        let mut msg = elements.to_vec();
        msg.extend_from_slice(challenge);
        let priv_bytes: [u8; 32] = fix.dac_key.to_bytes().into();
        sign_ecdsa_p256(&priv_bytes, &msg).unwrap()
    }

    #[test]
    fn accepts_valid_attestation() {
        let fix = chain();
        let nonce = [5u8; 32];
        let challenge = [6u8; 16];
        let el = elements(&nonce);
        let sig = sign(&fix, &el, &challenge);
        verify_device_attestation(
            &fix.dac, &fix.pai, &[fix.paa.clone()], &[], &el, &sig, &nonce, &challenge,
        )
        .unwrap();
    }

    #[test]
    fn rejects_unknown_paa() {
        let fix = chain();
        let other = chain(); // 別の根
        let nonce = [5u8; 32];
        let challenge = [6u8; 16];
        let el = elements(&nonce);
        let sig = sign(&fix, &el, &challenge);
        let err = verify_device_attestation(
            &fix.dac, &fix.pai, &[other.paa.clone()], &[], &el, &sig, &nonce, &challenge,
        )
        .unwrap_err();
        assert!(matches!(err, AttestationError::Chain(_)));
    }

    #[test]
    fn rejects_wrong_nonce() {
        let fix = chain();
        let challenge = [6u8; 16];
        let el = elements(&[5u8; 32]);
        let sig = sign(&fix, &el, &challenge);
        let err = verify_device_attestation(
            &fix.dac, &fix.pai, &[fix.paa.clone()], &[], &el, &sig, &[9u8; 32], &challenge,
        )
        .unwrap_err();
        assert!(matches!(err, AttestationError::Nonce));
    }

    #[test]
    fn rejects_tampered_signature() {
        let fix = chain();
        let nonce = [5u8; 32];
        let challenge = [6u8; 16];
        let el = elements(&nonce);
        let mut sig = sign(&fix, &el, &challenge);
        sig[0] ^= 0xFF;
        let err = verify_device_attestation(
            &fix.dac, &fix.pai, &[fix.paa.clone()], &[], &el, &sig, &nonce, &challenge,
        )
        .unwrap_err();
        assert!(matches!(err, AttestationError::Signature));
    }
}
```

- [ ] **Step 2: FAIL 確認**

Run: `cargo test -p mat-controller attestation`
Expected: FAIL

- [ ] **Step 3: 実装**

`verify_device_attestation` の手順（spec §6.2 / §11.17。strict 部が 1 つでも落ちたら Err）:

1. `dac = parse_x509(dac_der)?`, `pai = parse_x509(pai_der)?`（`X509Error` → `X509`）。
2. **チェーン（厳格）**: `dac.issuer == pai.subject`（バイト一致）でなければ `Chain("dac issuer != pai subject")`。`dac.verify_signed_by(&pai)` 失敗 → `Chain("dac signature")`。
3. PAA 探索: `paa_ders` を parse し、`pai.issuer == paa.subject` の最初の 1 枚。（AKID/SKID があれば併用して絞る。）見つからない → `Chain("no matching PAA in trust store")`。`pai.verify_signed_by(&paa)` 失敗 → `Chain("pai signature")`。
4. **attestation 署名（厳格）**: `msg = elements || attestation_challenge`、`crypto::verify_ecdsa_p256(&dac.public_key, &msg, signature)` 失敗 → `Signature`。
5. **elements 解析 + nonce（厳格）**: TLV anonymous struct `{1: certification_declaration, 2: attestation_nonce, 3: timestamp, …}`。tag2 が `expected_nonce` と不一致 → `Nonce`。tag1 が無い → `Elements("no certification declaration")`。
6. **CD（warn のみ、ユーザー決定 2026-07-13）**: `verify_cd_warn(cd_bytes, &dac, cd_signer_ders)` を呼ぶ。この関数は **決して Err を返さない**:
   - CMS SignedData（`SEQ{OID signedData, [0]{SEQ{ver, digestAlgs, encapContent SEQ{OID, [0] OCTET STRING(=CD TLV)}, signerInfos…}}}`）から eContent の CD TLV を取り出す。parse 失敗 → `warn!(reason, "certification declaration unparseable — continuing")` で終了。
   - CD TLV struct から `{1: vendor_id, 2: product_id_array}` を読み、`dac.vid`/`dac.pid` と突き合わせ。不一致 → warn。
   - `cd_signer_ders` が空 → `warn!("CD signature not verified (no CD signer certs provided)")`。非空なら SignerInfo の署名検証を試み、失敗 → warn。
7. `Ok(())`。

`load_der_dir`: `std::fs::read_dir` で拡張子 `der`（大文字小文字無視）のファイルを `fs::read`。

- [ ] **Step 4: PASS 確認**

Run: `cargo test -p mat-controller attestation`
Expected: PASS（4 テスト）

- [ ] **Step 5: `task check` → コミット**

```bash
task check
git add crates/mat-controller/src/attestation.rs crates/mat-controller/src/lib.rs
git commit -m "feat(controller): device attestation検証 (DAC→PAI→PAA厳格/CD warn) (M6a Task5)"
```

---

### Task 6: `cert.rs` RCAC 自己生成 + `fabric.rs` IPK 導出

**Files:**
- Modify: `crates/mat-controller/src/cert.rs`（`generate_rcac` 追加。`issue_noc` の署名尾部の作りを踏襲すること — cert.rs:32-118 を先に読む）
- Modify: `crates/mat-controller/src/fabric.rs`（`derive_ipk_operational` 追加）

**Interfaces:**
- Consumes: `MatterCert` / `DnAttr` / `DnValue::MatterId` / `CertExtension` / `subject_key_id` / `sign_ecdsa_p256`（すべて既存）。
- Produces:
  - `pub fn generate_rcac() -> Result<(MatterCert, [u8; 32]), CertError>`（self-signed root と root 秘密鍵。rcac-id は乱数 u64。issue_noc と同じ validity 定数を使う）
  - `pub fn derive_ipk_operational(epoch_key: &[u8; 16], compressed_fabric_id: &[u8; 8]) -> [u8; 16]`（HKDF-SHA256, salt=cfid, info=`b"GroupKey v1.0"`、spec §4.15.2）

- [ ] **Step 1: 失敗するテストを書く**

cert.rs / fabric.rs の既存 `#[cfg(test)]` モジュールに追加:

```rust
    // cert.rs
    #[test]
    fn generate_rcac_is_self_signed_and_issues_valid_noc() {
        let (rcac, root_key) = generate_rcac().unwrap();
        // TLV 往復
        let reparsed = MatterCert::parse(&rcac.to_tlv()).unwrap();
        assert_eq!(reparsed.subject, rcac.subject);
        // 自己署名検証
        rcac.verify_signed_by(&rcac.pub_key).unwrap();
        // この root で NOC を発行してチェーン検証が通る
        let op = crate::case::random_p256_secret();
        use p256::elliptic_curve::sec1::ToEncodedPoint;
        let op_pub: [u8; 65] = op.public_key().to_encoded_point(false).as_bytes().try_into().unwrap();
        let noc = issue_noc(&op_pub, 0x1_0001, 0xFAB1, &rcac, &root_key, &[1]).unwrap();
        verify_noc_chain(&noc, None, &rcac).unwrap();
    }

    // fabric.rs
    #[test]
    fn ipk_operational_derivation_is_deterministic_and_input_sensitive() {
        let a = derive_ipk_operational(&[1u8; 16], &[2u8; 8]);
        let b = derive_ipk_operational(&[1u8; 16], &[2u8; 8]);
        let c = derive_ipk_operational(&[1u8; 16], &[3u8; 8]);
        assert_eq!(a, b);
        assert_ne!(a, c);
    }
```

- [ ] **Step 2: FAIL 確認**

Run: `cargo test -p mat-controller generate_rcac && cargo test -p mat-controller ipk_operational`
Expected: FAIL

- [ ] **Step 3: 実装**

`generate_rcac`（`issue_noc` の構造をなぞる）:

```rust
/// self-signed RCAC（使い捨て fabric 用の root）を新規生成する。
/// 戻り値は (RCAC, root 秘密鍵)。rcac-id (DN tag 20) は乱数。
pub fn generate_rcac() -> Result<(MatterCert, [u8; 32]), CertError> {
    use p256::elliptic_curve::sec1::ToEncodedPoint;
    let sk = crate::case::random_p256_secret();
    let private_key: [u8; 32] = sk.to_bytes().into();
    let public_key: [u8; 65] = sk
        .public_key()
        .to_encoded_point(false)
        .as_bytes()
        .try_into()
        .map_err(|_| CertError::Malformed("pubkey encode"))?;

    let mut rcac_id_b = [0u8; 8];
    getrandom::getrandom(&mut rcac_id_b).expect("os rng");
    let rcac_id = u64::from_le_bytes(rcac_id_b);
    let subject = vec![DnAttr { tlv_tag: 20, value: DnValue::MatterId(rcac_id) }];

    let skid = subject_key_id(&public_key).to_vec();
    let extensions = vec![
        CertExtension::BasicConstraints { is_ca: true, path_len: None },
        CertExtension::KeyUsage(KEY_USAGE_KEY_CERT_SIGN | KEY_USAGE_CRL_SIGN),
        CertExtension::SubjectKeyId(skid.clone()),
        CertExtension::AuthorityKeyId(skid),
    ];

    let mut serial = [0u8; 8];
    getrandom::getrandom(&mut serial).expect("os rng");
    serial[0] &= 0x7F;

    // ここから issue_noc と同じ組み立て・署名の尾部
    // (MatterCert { serial, issuer: subject.clone(), not_before/after,
    //  subject, pub_key: public_key, extensions, .. } を作り、tbs_der() に
    //  sign_ecdsa_p256 して signature を格納する — issue_noc の実装をそのまま
    //  踏襲。KEY_USAGE_KEY_CERT_SIGN=0x0020 / KEY_USAGE_CRL_SIGN=0x0040 の
    //  定数が cert.rs に無ければ追加する)
    …
    Ok((cert, private_key))
}
```

`derive_ipk_operational`:

```rust
/// epoch IPK → operational IPK (spec §4.15.2: Crypto_KDF(epoch, salt=CompressedFabricId,
/// info="GroupKey v1.0", 128bit))。AddNOC でデバイスに渡すのは epoch 側、
/// CASE の destination id / Sigma で使うのは operational 側。
pub fn derive_ipk_operational(epoch_key: &[u8; 16], compressed_fabric_id: &[u8; 8]) -> [u8; 16] {
    let hk = hkdf::Hkdf::<sha2::Sha256>::new(Some(compressed_fabric_id), epoch_key);
    let mut out = [0u8; 16];
    hk.expand(b"GroupKey v1.0", &mut out).expect("hkdf 16B");
    out
}
```

`MatterCert::verify_signed_by` のシグネチャは `&[u8; 65]`（issuer 公開鍵）である点に注意（cert.rs:323）。

- [ ] **Step 4: PASS 確認**

Run: `cargo test -p mat-controller -- cert fabric`
Expected: PASS（既存テスト含め全緑）

- [ ] **Step 5: `task check` → コミット**

```bash
task check
git add crates/mat-controller/src/cert.rs crates/mat-controller/src/fabric.rs
git commit -m "feat(controller): RCAC自己生成 + operational IPK導出 (M6a Task6)"
```

---

### Task 7: `im.rs` / `session.rs` — timed invoke とデータ付き InvokeResponse

**Files:**
- Modify: `crates/mat-controller/src/im.rs`
- Modify: `crates/mat-controller/src/session.rs`

**Interfaces:**
- Produces（im.rs）:
  - `pub const OPCODE_TIMED_REQUEST: u8 = 0x0A;`
  - `pub fn encode_timed_request(timeout_ms: u16) -> Vec<u8>`（`struct{0: timeout, 255: IM_REVISION}`）
  - `pub fn encode_invoke_request_timed(endpoint: u16, cluster: u32, command: u32, fields_tlv: Option<&[u8]>) -> Vec<u8>`（TimedRequest フラグ true。既存 `encode_invoke_request` と共通の内部関数に `timed: bool` を渡す形にリファクタ。**既存 pub fn のシグネチャは不変**）
  - `pub struct InvokeResponseData { pub status: u8, pub cluster_status: Option<u8>, pub fields_tlv: Option<Vec<u8>> }`（fields は anonymous 再タグ済みの完全な TLV 1 要素）
  - `pub fn decode_invoke_response_data(payload: &[u8]) -> Result<InvokeResponseData, ImError>`（CommandDataIB なら `status=0` + fields、CommandStatusIB なら既存どおり）
- Produces（session.rs）:
  - `pub async fn invoke_for_data(&mut self, endpoint: u16, cluster: u32, command: u32, fields_tlv: Option<&[u8]>, timed_timeout_ms: Option<u16>, cfg: &MrpConfig) -> Result<crate::im::InvokeResponseData, SessionError>`

- [ ] **Step 1: 失敗するテストを書く**（im.rs の既存 tests モジュールに追加）

```rust
    #[test]
    fn timed_request_shape() {
        let p = encode_timed_request(10_000);
        let mut r = Reader::new(&p);
        assert!(matches!(r.next().unwrap().unwrap().value, Value::StructStart));
        let e = r.next().unwrap().unwrap();
        assert_eq!(e.tag, Tag::Context(0));
        assert!(matches!(e.value, Value::Uint(10_000)));
    }

    #[test]
    fn invoke_request_timed_sets_flag() {
        let p = encode_invoke_request_timed(0, 0x3E, 0x00, None);
        let mut r = Reader::new(&p);
        r.next().unwrap(); // struct
        r.next().unwrap(); // SuppressResponse
        let e = r.next().unwrap().unwrap(); // TimedRequest
        assert!(matches!(e.value, Value::Bool(true)));
    }

    #[test]
    fn decode_invoke_response_with_command_fields() {
        // InvokeResponseMessage { 1: [ { 0: CommandDataIB { 0: path, 1: fields } } ] }
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bool(Tag::Context(0), false);
        w.start_array(Tag::Context(1));
        w.start_struct(Tag::Anonymous); // InvokeResponseIB
        w.start_struct(Tag::Context(0)); // CommandDataIB
        w.start_list(Tag::Context(0)); // CommandPathIB
        w.put_uint(Tag::Context(0), 0);
        w.put_uint(Tag::Context(1), 0x3E);
        w.put_uint(Tag::Context(2), 0x01);
        w.end_container();
        w.start_struct(Tag::Context(1)); // CommandFields
        w.put_bytes(Tag::Context(0), b"elements");
        w.put_bytes(Tag::Context(1), &[0xAB; 64]);
        w.end_container();
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        let d = decode_invoke_response_data(&w.finish()).unwrap();
        assert_eq!(d.status, 0);
        let fields = d.fields_tlv.unwrap();
        let mut fr = Reader::new(&fields);
        assert!(matches!(fr.next().unwrap().unwrap().value, Value::StructStart));
        assert!(matches!(fr.next().unwrap().unwrap().value, Value::Bytes(b) if b == b"elements"));
    }

    #[test]
    fn decode_invoke_response_data_status_form() {
        // 既存 decode_invoke_response のテストと同じ CommandStatusIB 形を入れて
        // status が透過することを確認（既存テストの合成ヘルパがあれば流用）。
        …
    }
```

- [ ] **Step 2: FAIL 確認**

Run: `cargo test -p mat-controller im`
Expected: FAIL（新テストのみ）

- [ ] **Step 3: 実装**

- `encode_invoke_request` の本体を `fn encode_invoke_request_inner(endpoint, cluster, command, fields_tlv, timed: bool)` に移し、既存 pub は `inner(…, false)`、新 pub `encode_invoke_request_timed` は `inner(…, true)`（`w.put_bool(Tag::Context(1), timed)`）。
- `decode_invoke_response_data`: `decode_invoke_response` の走査をベースに、最初の InvokeResponseIB で `Tag::Context(0)`（CommandDataIB）が来たら: CommandPathIB（list）を skip、`Tag::Context(1)` の struct を `copy_retagged(&mut w, &mut r, Tag::Anonymous)` で Vec に写す → `InvokeResponseData { status: 0, cluster_status: None, fields_tlv: Some(bytes) }`。`Tag::Context(1)`（CommandStatusIB）なら既存 `decode_invoke_response_ib` 相当で status を取り fields は None。
- session.rs `invoke_for_data`: 既存 `invoke`（session.rs:405 以降）を複製し、
  1. `timed_timeout_ms = Some(t)` のとき、まず同一 exchange で `encode_timed_request(t)` を `OPCODE_TIMED_REQUEST` で `send_reliable` → 応答は `OPCODE_STATUS_RESPONSE`（status 0 以外は `ImError::StatusResponse`）。
  2. 続けて同一 exchange で `encode_invoke_request_timed`（timed 無しなら `encode_invoke_request`）を送り、`OPCODE_INVOKE_RESPONSE` を `decode_invoke_response_data` で解釈。`OPCODE_STATUS_RESPONSE` が来たら status を `ImError::StatusResponse` に。
  3. 応答が piggyback しない場合の `recv(exchange_id, IM_RECV_TIMEOUT)` フォールバックも既存 `invoke` と同じに。

- [ ] **Step 4: PASS 確認**

Run: `cargo test -p mat-controller`
Expected: 全緑（既存の im/session/matd 統合テスト含む — シグネチャ不変の確認を兼ねる）

- [ ] **Step 5: `task check` → コミット**

```bash
task check
git add crates/mat-controller/src/im.rs crates/mat-controller/src/session.rs
git commit -m "feat(controller): timed invoke + データ付きInvokeResponse復号 (M6a Task7)"
```

---

### Task 8: `dnssd.rs` — commissionable node の one-shot browse

**Files:**
- Modify: `crates/mat-controller/src/dnssd.rs`

**Interfaces:**
- Consumes: dnssd.rs 内の既存 DNS メッセージ組み立て / 解析ヘルパ（**実装前に `resolve_operational`（dnssd.rs:343 以降）と private ヘルパを読むこと** — クエリ組み立て・応答走査・TXT 解釈・AAAA 追跡のパターンをそのまま流用する）。
- Produces:
  - `pub async fn resolve_commissionable(scope_id: u32, long_discriminator: u16, timeout: Duration) -> Result<ResolvedNode, DnssdError>`（戻り型は既存 `ResolvedNode` を再利用 — port / addresses / MRP interval が揃えば commissioning には十分）

- [ ] **Step 1: 失敗するテストを書く**

dnssd.rs の既存テスト群の合成パケット形式に合わせて（既存テストを読んで同じヘルパ/形式で書く）:

```rust
    #[test]
    fn extracts_commissionable_from_ptr_srv_txt_aaaa() {
        // _L3840._sub._matterc._udp.local への PTR 応答 +
        // additional に SRV(port 5540, target host)・TXT("D=3840","SII=5000")・
        // AAAA(fd00::1) を持つ合成応答を parse し、port/addresses/TXT が
        // 取れることを確認する。既存 operational 側の合成応答テストを雛形にする。
    }

    #[test]
    fn rejects_mismatched_discriminator() {
        // TXT D=1234 の応答は long_discriminator=3840 の browse では無視される
        // (タイムアウトまで採用しない) ことを、応答フィルタ関数単体で確認する。
    }
```

（応答フィルタは `fn commissionable_from_response(bytes, long_discriminator) -> Option<ResolvedNode>` のような同期関数に切り出してテストする — `resolve_operational` の実装が応答解析を関数に切り出していない場合は、browse 側は切り出して書く。）

- [ ] **Step 2: FAIL 確認**

Run: `cargo test -p mat-controller dnssd`
Expected: FAIL（新テスト）

- [ ] **Step 3: 実装**

- クエリ: PTR `_L<disc>._sub._matterc._udp.local`（サービスサブタイプで discriminator を絞る、spec §4.3.1）。`resolve_operational` と同じく legacy unicast（QU）で ff02::fb / 5353 へ、1 秒間隔で `timeout` まで再送。
- 応答処理: PTR → instance 名。同一応答の additional から `<instance>._matterc._udp.local` の SRV（port / target）、TXT、target の AAAA を回収。AAAA が同梱されなければ追加クエリ（operational と同じ 2 段目）。
- TXT `D=<disc>` が `long_discriminator` と一致することを検証（サブタイプで絞れていても、他コミッショニング中デバイスの流れ弾を除ける）。`SII`/`SAI` は既存 TXT 解釈をそのまま使い `ResolvedNode` に詰める。
- link-local 優先順・`socket_addrs(scope_id)` の扱いは `ResolvedNode` 既存実装に乗る。

- [ ] **Step 4: PASS 確認**

Run: `cargo test -p mat-controller dnssd`
Expected: PASS

- [ ] **Step 5: `task check` → コミット**

```bash
task check
git add crates/mat-controller/src/dnssd.rs
git commit -m "feat(controller): _matterc._udp commissionable browse (discriminatorフィルタ) (M6a Task8)"
```

---

### Task 9: `commissioning.rs`（前半）— コマンド payload builder / decoder と使い捨て fabric

**Files:**
- Create: `crates/mat-controller/src/commissioning.rs`
- Modify: `crates/mat-controller/src/lib.rs`（`pub mod commissioning;`）

**Interfaces:**
- Consumes: `tlv::{Writer, Reader, Tag, Value}`、`cert::{generate_rcac, issue_noc, MatterCert}`、`fabric::{FabricCredentials, compressed_fabric_id, derive_ipk_operational}`、`kvs::SelfIssueMaterials`、`x509::parse_csr`。
- Produces（Task 10 が消費する正確な形）:

```rust
pub const CLUSTER_GENERAL_COMMISSIONING: u32 = 0x0030;
pub const CMD_ARM_FAIL_SAFE: u32 = 0x00;            // resp 0x01
pub const CMD_SET_REGULATORY_CONFIG: u32 = 0x02;    // resp 0x03
pub const CMD_COMMISSIONING_COMPLETE: u32 = 0x04;   // resp 0x05
pub const CLUSTER_OPERATIONAL_CREDENTIALS: u32 = 0x003E;
pub const CMD_ATTESTATION_REQUEST: u32 = 0x00;      // resp 0x01
pub const CMD_CERT_CHAIN_REQUEST: u32 = 0x02;       // resp 0x03
pub const CMD_CSR_REQUEST: u32 = 0x04;              // resp 0x05
pub const CMD_ADD_NOC: u32 = 0x06;                  // resp NOCResponse 0x08
pub const CMD_REMOVE_FABRIC: u32 = 0x0A;            // resp NOCResponse 0x08
pub const CMD_ADD_TRUSTED_ROOT: u32 = 0x0B;         // 応答は NOCResponse ではなく status
pub const CLUSTER_ADMIN_COMMISSIONING: u32 = 0x003C;
pub const CMD_OPEN_COMMISSIONING_WINDOW: u32 = 0x00; // timed 必須
pub const CERT_TYPE_DAC: u8 = 1;
pub const CERT_TYPE_PAI: u8 = 2;

pub fn encode_arm_fail_safe(expiry_s: u16, breadcrumb: u64) -> Vec<u8>;
pub fn decode_commissioning_status_response(fields: &[u8]) -> Result<(u8, String), CommissionError>; // ArmFailSafe/SetRegulatory/CommissioningComplete の {0: errorCode, 1: debugText} 共通
pub fn encode_set_regulatory_config(config: u8, country: &str, breadcrumb: u64) -> Vec<u8>;
pub fn encode_attestation_request(nonce: &[u8; 32]) -> Vec<u8>;
pub fn decode_attestation_response(fields: &[u8]) -> Result<(Vec<u8>, [u8; 64]), CommissionError>; // (elements, signature)
pub fn encode_cert_chain_request(cert_type: u8) -> Vec<u8>;
pub fn decode_cert_chain_response(fields: &[u8]) -> Result<Vec<u8>, CommissionError>;
pub fn encode_csr_request(nonce: &[u8; 32]) -> Vec<u8>;
pub fn decode_csr_response(fields: &[u8]) -> Result<(Vec<u8>, [u8; 64]), CommissionError>; // (nocsr_elements, signature)
pub fn parse_nocsr_elements(elements: &[u8]) -> Result<(Vec<u8>, Vec<u8>), CommissionError>; // (csr_der, csr_nonce)
pub fn encode_add_trusted_root(rcac_tlv: &[u8]) -> Vec<u8>;
pub fn encode_add_noc(noc_tlv: &[u8], ipk_epoch: &[u8; 16], case_admin_subject: u64, admin_vendor_id: u16) -> Vec<u8>;
pub fn decode_noc_response(fields: &[u8]) -> Result<(u8, Option<u8>), CommissionError>; // (statusCode, fabricIndex)
pub fn encode_remove_fabric(fabric_index: u8) -> Vec<u8>;
pub fn encode_open_commissioning_window(timeout_s: u16, verifier: &[u8; 97], discriminator: u16, iterations: u32, salt: &[u8]) -> Vec<u8>;

/// 使い捨て第二 fabric の素材（spec 決定 4: 永続化しない、呼び出し側が生成して持つ）。
pub struct CommissioningFabric {
    pub rcac_tlv: Vec<u8>,
    root_private_key: [u8; 32],
    pub fabric_id: u64,
    pub ipk_epoch: [u8; 16],
    pub admin_node_id: u64,
}
impl CommissioningFabric {
    pub fn generate(fabric_id: u64, admin_node_id: u64) -> Result<Self, CommissionError>;
    /// controller 自身の CASE 用 credentials（NOC 自己発行を再利用）。
    pub fn admin_credentials(&self) -> Result<FabricCredentials, CommissionError>;
    /// CSR の公開鍵にデバイス NOC を発行して TLV で返す。
    pub fn issue_device_noc(&self, op_public_key: &[u8; 65], node_id: u64) -> Result<Vec<u8>, CommissionError>;
}

pub enum CommissionError {
    Discovery(crate::dnssd::DnssdError),
    Pase(crate::pase::PaseError),
    Session(crate::session::SessionError),
    Attestation(crate::attestation::AttestationError),
    Csr(&'static str),
    Noc(u8),                       // NOCResponse statusCode != 0
    CommandStatus { step: &'static str, code: u8 },  // *CommissioningResponse errorCode != 0
    Malformed { step: &'static str, detail: &'static str },
    Cert(crate::cert::CertError),
    Fabric(crate::fabric::FabricError),
    Case(crate::case::CaseError),
    Timeout(&'static str),
}
```

`CommissionError` の doc comment に spec 決定 5 の対応表（Attestation/Pase(ConfirmMismatch)→`device_rejected`(4)、Discovery/Timeout→`timeout`(3)、Case→`session_failed`(6)、他→`commission_failed`(1)）を記載する。

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::tlv::{Reader, Tag, Value, Writer};

    #[test]
    fn arm_fail_safe_fields_shape() {
        let f = encode_arm_fail_safe(120, 1);
        let mut r = Reader::new(&f);
        assert!(matches!(r.next().unwrap().unwrap().value, Value::StructStart));
        assert!(matches!(r.next().unwrap().unwrap().value, Value::Uint(120)));
        assert!(matches!(r.next().unwrap().unwrap().value, Value::Uint(1)));
    }

    #[test]
    fn add_noc_fields_shape() {
        let f = encode_add_noc(b"noc", &[9u8; 16], 0x1_0001, 0xFFF1);
        let mut r = Reader::new(&f);
        r.next().unwrap(); // struct
        assert!(matches!(r.next().unwrap().unwrap().value, Value::Bytes(b) if b == b"noc"));
        // tag1 (ICAC) は無いこと
        let e = r.next().unwrap().unwrap();
        assert_eq!(e.tag, Tag::Context(2)); // IPKValue
        assert!(matches!(r.next().unwrap().unwrap().value, Value::Uint(0x1_0001)));
        assert!(matches!(r.next().unwrap().unwrap().value, Value::Uint(0xFFF1)));
    }

    #[test]
    fn decodes_noc_response_and_status_response() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 0);
        w.put_uint(Tag::Context(1), 3);
        w.end_container();
        let (status, idx) = decode_noc_response(&w.finish()).unwrap();
        assert_eq!((status, idx), (0, Some(3)));

        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 0);
        w.put_str(Tag::Context(1), "");
        w.end_container();
        let (code, _) = decode_commissioning_status_response(&w.finish()).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn decodes_attestation_and_csr_responses() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(0), b"elements");
        w.put_bytes(Tag::Context(1), &[0xAB; 64]);
        w.end_container();
        let (el, sig) = decode_attestation_response(&w.finish()).unwrap();
        assert_eq!(el, b"elements");
        assert_eq!(sig, [0xAB; 64]);

        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(1), b"csr-der");
        w.put_bytes(Tag::Context(2), &[7u8; 32]);
        w.end_container();
        let (csr, nonce) = parse_nocsr_elements(&w.finish()).unwrap();
        assert_eq!(csr, b"csr-der");
        assert_eq!(nonce, vec![7u8; 32]);
    }

    #[test]
    fn commissioning_fabric_issues_valid_credentials() {
        let fab = CommissioningFabric::generate(0xFAB1, 0x1_0001).unwrap();
        let creds = fab.admin_credentials().unwrap();
        assert_eq!(creds.fabric_id, 0xFAB1);
        assert_eq!(creds.node_id, 0x1_0001);
        // デバイス NOC も同じ root でチェーン検証が通る
        let dev = crate::case::random_p256_secret();
        use p256::elliptic_curve::sec1::ToEncodedPoint;
        let dev_pub: [u8; 65] =
            dev.public_key().to_encoded_point(false).as_bytes().try_into().unwrap();
        let noc_tlv = fab.issue_device_noc(&dev_pub, 0x2_0001).unwrap();
        let noc = crate::cert::MatterCert::parse(&noc_tlv).unwrap();
        let rcac = crate::cert::MatterCert::parse(&fab.rcac_tlv).unwrap();
        crate::cert::verify_noc_chain(&noc, None, &rcac).unwrap();
    }

    #[test]
    fn open_window_fields_shape() {
        let f = encode_open_commissioning_window(180, &[1u8; 97], 0xABC, 1000, &[2u8; 32]);
        let mut r = Reader::new(&f);
        r.next().unwrap(); // struct
        assert!(matches!(r.next().unwrap().unwrap().value, Value::Uint(180)));
        assert!(matches!(r.next().unwrap().unwrap().value, Value::Bytes(b) if b.len() == 97));
        assert!(matches!(r.next().unwrap().unwrap().value, Value::Uint(0xABC)));
        assert!(matches!(r.next().unwrap().unwrap().value, Value::Uint(1000)));
        assert!(matches!(r.next().unwrap().unwrap().value, Value::Bytes(b) if b.len() == 32));
    }
}
```

- [ ] **Step 2: FAIL 確認**

Run: `cargo test -p mat-controller commissioning`
Expected: FAIL

- [ ] **Step 3: 実装**

- builder は全て anonymous struct + context tag（`encode_invoke_request` の `fields_tlv` 契約: 完全な TLV 1 要素、タグは再付与される）。フィールド並びは spec §11.10 / §11.17 / §11.19:
  - ArmFailSafe `{0: expiry u16, 1: breadcrumb u64}` / SetRegulatoryConfig `{0: config u8, 1: country str(2), 2: breadcrumb}` / CommissioningComplete はフィールド無し（`None` を渡すので builder 不要）
  - AttestationRequest `{0: nonce}` / CertificateChainRequest `{0: type u8}` / CSRRequest `{0: nonce}`
  - AddTrustedRootCertificate `{0: rcac_tlv}` / AddNOC `{0: noc, 2: ipk, 3: caseAdminSubject, 4: adminVendorId}`（ICAC の tag1 は出さない）/ RemoveFabric `{0: index u8}`
  - OpenCommissioningWindow `{0: timeout u16, 1: verifier(97B), 2: discriminator u16, 3: iterations u32, 4: salt}`
- decoder は `Reader` 走査で該当 tag を拾い、欠落は `Malformed { step, detail }`。
- `CommissioningFabric::generate`: `generate_rcac()` + `ipk_epoch` 16B 乱数。
- `admin_credentials`: `MatterCert::parse(&self.rcac_tlv)` は不要 — `SelfIssueMaterials { rcac: self.rcac_tlv.clone(), root_private_key, ipk_operational, node_id: admin_node_id, fabric_id }` を `FabricCredentials::from_self_issued` に渡す（M2b の経路を再利用）。`ipk_operational` は root 公開鍵（RCAC を一度 parse して `pub_key`）と `fabric_id` から `compressed_fabric_id` → `derive_ipk_operational(&self.ipk_epoch, &cfid)`。
- `issue_device_noc`: `issue_noc(op_public_key, node_id, self.fabric_id, &rcac_cert, &self.root_private_key, &serial乱数)` → `.to_tlv()`。
- `CommissionError` に `Display` と、各下位エラーからの `From` を実装。

- [ ] **Step 4: PASS 確認**

Run: `cargo test -p mat-controller commissioning`
Expected: PASS（6 テスト）

- [ ] **Step 5: `task check` → コミット**

```bash
task check
git add crates/mat-controller/src/commissioning.rs crates/mat-controller/src/lib.rs
git commit -m "feat(controller): commissioningコマンドcodec + 使い捨てfabric素材 (M6a Task9)"
```

---

### Task 10: `commissioning.rs`（後半）— ステップマシンと open-window

**Files:**
- Modify: `crates/mat-controller/src/commissioning.rs`

**Interfaces:**
- Consumes: Task 9 の全 builder/decoder、`pase::establish`、`case::establish`、`session::SecureSession::{invoke_for_data, attestation_challenge}`、`attestation::{load_der_dir, verify_device_attestation}`、`dnssd::{resolve_commissionable, ResolvedNode}`、`setup_code::{encode_manual_code, encode_qr, SetupPayload}`、`spake2p::compute_verifier`、`exchange::MrpConfig`。
- Produces:

```rust
pub enum CommissionTarget {
    /// アドレス既知 (ローカル E2E / already-discovered 相当)
    Addr(std::net::SocketAddr),
    /// _matterc browse で long discriminator から探索
    Discriminator(u16),
}

pub struct CommissionParams<'a> {
    pub passcode: u32,
    pub target: CommissionTarget,
    pub device_node_id: u64,
    pub paa_dir: Option<&'a std::path::Path>,
    pub cd_signer_dir: Option<&'a std::path::Path>,
    pub scope_id: u32, // mDNS / link-local 用 iface index（Addr 直指定なら 0 可）
}

pub struct CommissionedDevice {
    pub node_id: u64,
    pub fabric_index: Option<u8>,
    /// 新 fabric 上の operational CASE セッション（CommissioningComplete 送信済み）
    pub session: crate::session::SecureSession,
}

pub async fn commission_on_network(
    transport: std::sync::Arc<crate::transport::UdpTransport>,
    fabric: &CommissioningFabric,
    params: CommissionParams<'_>,
) -> Result<CommissionedDevice, CommissionError>;

pub struct OpenedWindow {
    pub passcode: u32,
    pub discriminator: u16, // 12-bit
    pub manual_code: String,
    pub qr_payload: String,
    pub window_timeout_s: u16,
}

pub async fn open_commissioning_window(
    session: &mut crate::session::SecureSession,
    timeout_s: u16,
    cfg: &crate::exchange::MrpConfig,
) -> Result<OpenedWindow, CommissionError>;
```

- [ ] **Step 1: 失敗するテストを書く**（純関数部分のみ — ステップマシン全体は Task 11 のライブテストで検証）

```rust
    #[test]
    fn random_passcode_is_valid() {
        for _ in 0..64 {
            let p = random_valid_passcode();
            assert!(p >= 1 && p <= 99_999_998);
            assert!(!INVALID_PASSCODES.contains(&p));
        }
    }

    #[test]
    fn opened_window_setup_codes_are_consistent() {
        let w = OpenedWindow {
            passcode: 20202021,
            discriminator: 3840,
            manual_code: build_manual_code(20202021, 3840),
            qr_payload: build_window_qr(20202021, 3840),
            window_timeout_s: 180,
        };
        let m = crate::setup_code::parse_manual_code(&w.manual_code).unwrap();
        assert_eq!(m.passcode, 20202021);
        assert_eq!(u16::from(m.short_discriminator), 3840 >> 8);
        let q = crate::setup_code::parse_qr(&w.qr_payload).unwrap();
        assert_eq!(q.passcode, 20202021);
        assert_eq!(q.discriminator, 3840);
    }
```

- [ ] **Step 2: FAIL 確認**

Run: `cargo test -p mat-controller commissioning`
Expected: FAIL（新規 2 テスト）

- [ ] **Step 3: 実装**

`commission_on_network`（spec 決定 3 のフローそのまま。各ステップの `step` 名をエラーに刻む）:

```rust
pub async fn commission_on_network(
    transport: Arc<UdpTransport>,
    fabric: &CommissioningFabric,
    params: CommissionParams<'_>,
) -> Result<CommissionedDevice, CommissionError> {
    // 1. ターゲット解決
    let (peer, cfg) = match params.target {
        CommissionTarget::Addr(a) => (a, MrpConfig::default()),
        CommissionTarget::Discriminator(d) => {
            let node = dnssd::resolve_commissionable(params.scope_id, d, Duration::from_secs(15))
                .await
                .map_err(CommissionError::Discovery)?;
            let addr = node
                .socket_addrs(params.scope_id)
                .into_iter()
                .next()
                .ok_or(CommissionError::Timeout("no usable address"))?;
            (addr, node.mrp_config())
        }
    };

    // 2. PASE
    let mut pase = pase::establish(Arc::clone(&transport), peer, params.passcode, &cfg)
        .await
        .map_err(CommissionError::Pase)?;
    let challenge = pase.attestation_challenge();

    // 3. ArmFailSafe(120s)（必須）
    let resp = pase
        .invoke_for_data(0, CLUSTER_GENERAL_COMMISSIONING, CMD_ARM_FAIL_SAFE,
                         Some(&encode_arm_fail_safe(120, 1)), None, &cfg)
        .await
        .map_err(CommissionError::Session)?;
    check_commissioning_response("arm-fail-safe", &resp)?;

    // 4. SetRegulatoryConfig（任意 — spec 決定 7: 失敗は warn で続行）
    match pase
        .invoke_for_data(0, CLUSTER_GENERAL_COMMISSIONING, CMD_SET_REGULATORY_CONFIG,
                         Some(&encode_set_regulatory_config(2, "XX", 2)), None, &cfg)
        .await
    {
        Ok(resp) => {
            if let Err(e) = check_commissioning_response("set-regulatory", &resp) {
                tracing::warn!(error = %e, "SetRegulatoryConfig rejected — continuing");
            }
        }
        Err(e) => tracing::warn!(error = %e, "SetRegulatoryConfig failed — continuing"),
    }

    // 5. attestation（厳格）
    let mut nonce = [0u8; 32];
    getrandom::getrandom(&mut nonce).expect("os rng");
    let resp = pase.invoke_for_data(0, CLUSTER_OPERATIONAL_CREDENTIALS, CMD_ATTESTATION_REQUEST,
                                    Some(&encode_attestation_request(&nonce)), None, &cfg)
        .await.map_err(CommissionError::Session)?;
    let (elements, att_sig) = decode_attestation_response(fields_of("attestation", &resp)?)?;
    let dac = request_cert("dac", &mut pase, CERT_TYPE_DAC, &cfg).await?;
    let pai = request_cert("pai", &mut pase, CERT_TYPE_PAI, &cfg).await?;
    let paa = match params.paa_dir {
        Some(d) => attestation::load_der_dir(d).map_err(CommissionError::Attestation)?,
        None => Vec::new(), // 空 → チェーン検証は必ず失敗する（PAA 必須運用）
    };
    let cd_signers = match params.cd_signer_dir {
        Some(d) => attestation::load_der_dir(d).map_err(CommissionError::Attestation)?,
        None => Vec::new(),
    };
    attestation::verify_device_attestation(&dac, &pai, &paa, &cd_signers,
                                           &elements, &att_sig, &nonce, &challenge)
        .map_err(CommissionError::Attestation)?;

    // 6. CSR → NOC 発行
    let mut csr_nonce = [0u8; 32];
    getrandom::getrandom(&mut csr_nonce).expect("os rng");
    let resp = pase.invoke_for_data(0, CLUSTER_OPERATIONAL_CREDENTIALS, CMD_CSR_REQUEST,
                                    Some(&encode_csr_request(&csr_nonce)), None, &cfg)
        .await.map_err(CommissionError::Session)?;
    let (nocsr_elements, nocsr_sig) = decode_csr_response(fields_of("csr", &resp)?)?;
    // NOCSR 署名も DAC 鍵で elements||challenge に対して（spec §11.17.5.6）
    {
        let dac_cert = x509::parse_x509(&dac).map_err(|_| CommissionError::Csr("dac reparse"))?;
        let mut msg = nocsr_elements.clone();
        msg.extend_from_slice(&challenge);
        crypto::verify_ecdsa_p256(&dac_cert.public_key, &msg, &nocsr_sig)
            .map_err(|_| CommissionError::Csr("nocsr signature"))?;
    }
    let (csr_der, returned_nonce) = parse_nocsr_elements(&nocsr_elements)?;
    if returned_nonce != csr_nonce {
        return Err(CommissionError::Csr("csr nonce mismatch"));
    }
    let device_pub = x509::parse_csr(&csr_der).map_err(|_| CommissionError::Csr("csr parse"))?;
    let noc_tlv = fabric.issue_device_noc(&device_pub, params.device_node_id)?;

    // 7. AddTrustedRootCertificate → AddNOC
    let resp = pase.invoke_for_data(0, CLUSTER_OPERATIONAL_CREDENTIALS, CMD_ADD_TRUSTED_ROOT,
                                    Some(&encode_add_trusted_root(&fabric.rcac_tlv)), None, &cfg)
        .await.map_err(CommissionError::Session)?;
    if resp.status != 0 {
        return Err(CommissionError::CommandStatus { step: "add-trusted-root", code: resp.status });
    }
    let resp = pase.invoke_for_data(
            0, CLUSTER_OPERATIONAL_CREDENTIALS, CMD_ADD_NOC,
            Some(&encode_add_noc(&noc_tlv, &fabric.ipk_epoch, fabric.admin_node_id, 0xFFF1)),
            None, &cfg)
        .await.map_err(CommissionError::Session)?;
    let (noc_status, fabric_index) = decode_noc_response(fields_of("add-noc", &resp)?)?;
    if noc_status != 0 {
        return Err(CommissionError::Noc(noc_status));
    }

    // 8. 新 fabric で CASE（同一アドレスへ直接。AddNOC 直後は fabric 起動待ちが
    //    必要なことがあるためリトライ、全体 ~30s / failsafe 120s 内）
    let creds = fabric.admin_credentials()?;
    let mut session = None;
    let mut last = None;
    for _ in 0..6 {
        match case::establish(Arc::clone(&transport), peer, &creds, params.device_node_id, &cfg).await {
            Ok(s) => { session = Some(s); break; }
            Err(e) => { last = Some(e); tokio::time::sleep(Duration::from_secs(3)).await; }
        }
    }
    let mut session = session.ok_or_else(|| CommissionError::Case(last.expect("at least one try")))?;

    // 9. CommissioningComplete（CASE 上で）
    let resp = session
        .invoke_for_data(0, CLUSTER_GENERAL_COMMISSIONING, CMD_COMMISSIONING_COMPLETE, None, None, &cfg)
        .await.map_err(CommissionError::Session)?;
    check_commissioning_response("commissioning-complete", &resp)?;

    Ok(CommissionedDevice { node_id: params.device_node_id, fabric_index, session })
}
```

ヘルパ:

```rust
/// InvokeResponseData から fields を取り出す（無ければ Malformed）。
fn fields_of<'a>(step: &'static str, resp: &'a im::InvokeResponseData)
    -> Result<&'a [u8], CommissionError>
{
    if resp.status != 0 {
        return Err(CommissionError::CommandStatus { step, code: resp.status });
    }
    resp.fields_tlv.as_deref()
        .ok_or(CommissionError::Malformed { step, detail: "no command fields" })
}

/// {0: errorCode, 1: debugText} 型のレスポンスを検査。
fn check_commissioning_response(step: &'static str, resp: &im::InvokeResponseData)
    -> Result<(), CommissionError>
{
    let (code, _text) = decode_commissioning_status_response(fields_of(step, resp)?)?;
    if code != 0 {
        return Err(CommissionError::CommandStatus { step, code });
    }
    Ok(())
}

async fn request_cert(step: &'static str, session: &mut SecureSession, cert_type: u8,
                      cfg: &MrpConfig) -> Result<Vec<u8>, CommissionError> { … }
```

`open_commissioning_window`:

```rust
pub const INVALID_PASSCODES: [u32; 12] = [
    0, 11111111, 22222222, 33333333, 44444444, 55555555,
    66666666, 77777777, 88888888, 99999999, 12345678, 87654321,
];

fn random_valid_passcode() -> u32 { /* 1..=99_999_998 かつ INVALID 除外まで引き直し */ }

pub async fn open_commissioning_window(
    session: &mut SecureSession,
    timeout_s: u16,
    cfg: &MrpConfig,
) -> Result<OpenedWindow, CommissionError> {
    let passcode = random_valid_passcode();
    let mut disc_b = [0u8; 2];
    getrandom::getrandom(&mut disc_b).expect("os rng");
    let discriminator = u16::from_le_bytes(disc_b) & 0x0FFF;
    let mut salt = [0u8; 32];
    getrandom::getrandom(&mut salt).expect("os rng");
    let iterations = 1000u32; // spec §3.9: PBKDF_MINIMUM=1000
    let verifier = crate::spake2p::compute_verifier(passcode, &salt, iterations);
    // OpenCommissioningWindow は timed 必須（spec §11.19.8.1）
    let resp = session
        .invoke_for_data(0, CLUSTER_ADMIN_COMMISSIONING, CMD_OPEN_COMMISSIONING_WINDOW,
                         Some(&encode_open_commissioning_window(timeout_s, &verifier,
                                                                discriminator, iterations, &salt)),
                         Some(10_000), cfg)
        .await.map_err(CommissionError::Session)?;
    if resp.status != 0 {
        return Err(CommissionError::CommandStatus { step: "open-window", code: resp.status });
    }
    Ok(OpenedWindow {
        passcode,
        discriminator,
        manual_code: build_manual_code(passcode, discriminator),
        qr_payload: build_window_qr(passcode, discriminator),
        window_timeout_s: timeout_s,
    })
}

fn build_manual_code(passcode: u32, discriminator12: u16) -> String {
    crate::setup_code::encode_manual_code(passcode, (discriminator12 >> 8) as u8)
}

fn build_window_qr(passcode: u32, discriminator12: u16) -> String {
    crate::setup_code::encode_qr(&crate::setup_code::SetupPayload {
        version: 0,
        vendor_id: 0,     // ECM window の QR は VID/PID 不定で良い
        product_id: 0,
        custom_flow: 0,
        discovery_capabilities: 0x04, // on-network
        discriminator: discriminator12,
        passcode,
    })
}
```

- [ ] **Step 4: PASS 確認**

Run: `cargo test -p mat-controller`
Expected: 全緑（コンパイル成功 = 型整合の確認も兼ねる）

- [ ] **Step 5: `task check` → コミット**

```bash
task check
git add crates/mat-controller/src/commissioning.rs
git commit -m "feat(controller): commissioningステップマシン + native open-window (M6a Task10)"
```

---

### Task 11: ローカル live E2E（`task e2e:m6`）

**Files:**
- Create: `crates/mat-controller/tests/live_commissioning.rs`
- Create: `scripts/e2e-m6.sh`
- Modify: `Taskfile.yml`（`e2e:m6` タスク追加 — `e2e:m5` の並びに同形で）

**Interfaces:**
- Consumes: Task 10 の公開 API 一式。既存 `tests/live_case_im.rs` の env 読み・`#[ignore]` パターンを踏襲。

- [ ] **Step 1: ライブテストを書く**

`tests/live_commissioning.rs`（1 本のシナリオテスト — 順序依存を 1 fn に閉じ込める）:

```rust
//! M6a live E2E: 素の all-clusters-app に対する native commissioning。
//! 実行は scripts/e2e-m6.sh 経由（app 起動と PAA 取得を行う）。
//! 必須 env: MAT_E2E_PEER=[::1]:5540, MAT_E2E_PAA_DIR=<test PAA dir>
//! 任意 env: MAT_E2E_PASSCODE (既定 20202021)

use std::sync::Arc;
use std::time::Duration;

use mat_controller::commissioning::{
    self, CommissionError, CommissionParams, CommissionTarget, CommissioningFabric,
};
use mat_controller::exchange::MrpConfig;
use mat_controller::im::{ATTR_ON_OFF, CLUSTER_ON_OFF, CMD_ON_OFF_ON, ImValue};
use mat_controller::transport::UdpTransport;

fn env(k: &str) -> Option<String> { std::env::var(k).ok() }

#[tokio::test]
#[ignore = "live: requires all-clusters-app (scripts/e2e-m6.sh)"]
async fn commission_control_multi_admin() {
    let peer: std::net::SocketAddr = env("MAT_E2E_PEER").expect("MAT_E2E_PEER").parse().unwrap();
    let paa_dir = std::path::PathBuf::from(env("MAT_E2E_PAA_DIR").expect("MAT_E2E_PAA_DIR"));
    let passcode: u32 = env("MAT_E2E_PASSCODE").map_or(20202021, |v| v.parse().unwrap());
    let cfg = MrpConfig::default();

    // ① 誤 passcode → Pase エラー（fresh device の window は失敗では閉じない）
    let transport = Arc::new(UdpTransport::bind().await.unwrap());
    let fab_a = CommissioningFabric::generate(0xFAB1, 0x1_0001).unwrap();
    let err = commissioning::commission_on_network(
        Arc::clone(&transport), &fab_a,
        CommissionParams { passcode: passcode + 1, target: CommissionTarget::Addr(peer),
                           device_node_id: 0x2_0001, paa_dir: Some(&paa_dir),
                           cd_signer_dir: None, scope_id: 0 },
    ).await.unwrap_err();
    assert!(matches!(err, CommissionError::Pase(_)), "expected Pase error, got {err}");

    // ② 正しい passcode で native commission（初回 commissioner）
    let dev = commissioning::commission_on_network(
        Arc::clone(&transport), &fab_a,
        CommissionParams { passcode, target: CommissionTarget::Addr(peer),
                           device_node_id: 0x2_0001, paa_dir: Some(&paa_dir),
                           cd_signer_dir: None, scope_id: 0 },
    ).await.expect("commissioning A");
    let mut session = dev.session;

    // ③ 新 fabric で制御: on → read on-off == true
    session.invoke(1, CLUSTER_ON_OFF, CMD_ON_OFF_ON, None, &cfg).await.unwrap();
    let v = session.read_attribute(1, CLUSTER_ON_OFF, ATTR_ON_OFF, &cfg).await.unwrap();
    assert_eq!(v, ImValue::Bool(true));

    // ④ native open-window → 第二 admin (fabric B) として commission
    let window = commissioning::open_commissioning_window(&mut session, 180, &cfg)
        .await.expect("open window");
    eprintln!("window: manual={} qr={}", window.manual_code, window.qr_payload);
    let fab_b = CommissioningFabric::generate(0xFAB2, 0x1_0002).unwrap();
    let dev_b = commissioning::commission_on_network(
        Arc::clone(&transport), &fab_b,
        CommissionParams { passcode: window.passcode, target: CommissionTarget::Addr(peer),
                           device_node_id: 0x2_0002, paa_dir: Some(&paa_dir),
                           cd_signer_dir: None, scope_id: 0 },
    ).await.expect("commissioning B (multi-admin)");
    let mut session_b = dev_b.session;

    // ⑤ fabric B からも制御でき、B を RemoveFabric で撤収 → A は生きている
    let v = session_b.read_attribute(1, CLUSTER_ON_OFF, ATTR_ON_OFF, &cfg).await.unwrap();
    assert_eq!(v, ImValue::Bool(true));
    let idx = dev_b.fabric_index.expect("fabric index from NOCResponse");
    let resp = session_b
        .invoke_for_data(0, commissioning::CLUSTER_OPERATIONAL_CREDENTIALS,
                         commissioning::CMD_REMOVE_FABRIC,
                         Some(&commissioning::encode_remove_fabric(idx)), None, &cfg)
        .await.expect("remove fabric B");
    assert_eq!(resp.status, 0);
    // A のセッションは同一 socket 上で生存しているはず
    let v = session.read_attribute(1, CLUSTER_ON_OFF, ATTR_ON_OFF, &cfg).await.unwrap();
    assert_eq!(v, ImValue::Bool(true));
}
```

（注: `invoke` は 1 本目のセッションでそのまま使える — timed 不要のコマンドのため。`RemoveFabric` の応答は NOCResponse（fields 付き）だが status=0 の確認だけで十分。）

- [ ] **Step 2: コンパイルのみ確認**

Run: `cargo test -p mat-controller --test live_commissioning --no-run`
Expected: ビルド成功（実行はしない）

- [ ] **Step 3: `scripts/e2e-m6.sh` を書く**

`e2e-m2.sh` を雛形に:

```bash
#!/usr/bin/env bash
# Phase 5 M6a 受け入れ: native commissioning ローカル E2E。
# 前提: ./chip-all-clusters-app (task chip:extract:app)。chip-tool は不要。
set -euo pipefail
cd "$(dirname "$0")/.."
APP=${MAT_E2E_APP:-./chip-all-clusters-app}
PASSCODE=20202021
[[ -x "$APP" ]] || { echo "error: $APP なし (task chip:extract:app)"; exit 1; }

WORK=$(mktemp -d)
APP_PID=""
cleanup() { [[ -n "$APP_PID" ]] && kill "$APP_PID" 2>/dev/null || true; rm -rf "$WORK"; }
trap cleanup EXIT

echo "== 1/3 テスト用 PAA 証明書取得 (connectedhomeip v1.4.2.0)"
mkdir -p "$WORK/paa" .e2e-cache
BASE=https://raw.githubusercontent.com/project-chip/connectedhomeip/v1.4.2.0/credentials/development/paa-root-certs
for f in Chip-Test-PAA-FFF1-Cert.der Chip-Test-PAA-NoVID-Cert.der; do
  [[ -f ".e2e-cache/$f" ]] || curl -fsSL "$BASE/$f" -o ".e2e-cache/$f"
  cp ".e2e-cache/$f" "$WORK/paa/"
done

echo "== 2/3 app 起動 (KVS: $WORK/device_kvs)"
"$APP" --KVS "$WORK/device_kvs" >"$WORK/app.log" 2>&1 &
APP_PID=$!
for i in $(seq 1 40); do
  ss -uln 2>/dev/null | grep -q ':5540' && break
  kill -0 "$APP_PID" 2>/dev/null || { echo "app 起動失敗"; cat "$WORK/app.log"; exit 1; }
  sleep 0.25
done

echo "== 3/3 native commissioning ライブテスト"
MAT_E2E_PEER="[::1]:5540" \
MAT_E2E_PASSCODE="$PASSCODE" \
MAT_E2E_PAA_DIR="$WORK/paa" \
  cargo test -p mat-controller --test live_commissioning -- --ignored --nocapture

echo "== e2e:m6 PASS"
```

`.e2e-cache/` が `.gitignore` に無ければ追加する（証明書をコミットしない機械的保証）。

Taskfile.yml（`e2e:m5` の直後に）:

```yaml
  e2e:m6:
    desc: "M6a live E2E: native commissioning (all-clusters-app)"
    cmds:
      - bash scripts/e2e-m6.sh
```

- [ ] **Step 4: E2E 実行**

Run: `task e2e:m6`
Expected: `== e2e:m6 PASS`（①誤 passcode で Pase エラー、②〜⑤ commissioning×2 + 制御 + RemoveFabric 成功）

デバッグの定石: app 側ログは `$WORK/app.log`（trap 前に `cat` を仕込む）。PASE で落ちる場合は passcode/PBKDF、attestation で落ちる場合は PAA 取得と `load_der_dir` の中身、AddNOC 後の CASE で落ちる場合は IPK 導出（epoch/operational の取り違え）を最初に疑う。

- [ ] **Step 5: `task check` → コミット**

```bash
task check
git add crates/mat-controller/tests/live_commissioning.rs scripts/e2e-m6.sh Taskfile.yml .gitignore
git commit -m "test(controller): M6a ローカルlive E2E (commission/multi-admin/open-window) (M6a Task11)"
```

---

### Task 12: 実機 E2E ハーネス（jarvis・使い捨て第二 fabric）とドキュメント反映

**Files:**
- Create: `crates/mat-controller/tests/live_commission_real.rs`
- Create: `scripts/e2e-m6-real.sh`
- Modify: `Taskfile.yml`（`e2e:m6:real`）
- Modify: `ARCHITECTURE.md`（Phase 5 節に M6a 項を追記 — M5 項の形式に合わせる）

**Interfaces:**
- Consumes: `kvs::read_self_issue_materials` + `FabricCredentials::from_self_issued`（本番 fabric 側、`tests/live_jarvis.rs` と同じ読み方）、`dnssd::{resolve_operational, iface_index}`、`case::establish`、Task 10 の API。

- [ ] **Step 1: 実機ライブテストを書く**

`tests/live_commission_real.rs`（`live_jarvis.rs` の env 読みパターンを踏襲。実行はコントローラ実機上）:

```rust
//! M6a 実機受け入れ: 本番 fabric の Nanoleaf に native open-window →
//! 使い捨て第二 fabric へ native commission → 制御 → RemoveFabric 撤収 →
//! 本番 fabric 無傷を確認。実行は scripts/e2e-m6-real.sh 経由。
//! 必須 env: MAT_E2E_NODE_ID(対象), MAT_E2E_IFACE, MAT_E2E_KVS_DIR,
//!           MAT_E2E_FABRIC_INDEX, MAT_E2E_ISSUER_INDEX, MAT_E2E_PAA_DIR
```

流れ（1 fn、各段で eprintln! による進捗出力）:

1. 本番 credentials: `read_self_issue_materials(alpha_ini, main_ini, fabric_index, issuer_index)` → `FabricCredentials::from_self_issued`（ini パスの組み立ては `live_jarvis.rs` と同一に）。
2. `iface_index(MAT_E2E_IFACE)` → `resolve_operational` → `case::establish` で対象ノードに CASE。onoff `on-off` を read（事前状態記録）。
3. `open_commissioning_window(&mut session, 180, &cfg)` → `window.discriminator` / `window.passcode` を取得（stderr に出す）。
4. `CommissioningFabric::generate(0xFAB1, 0x1_0001)` → `commission_on_network` を `CommissionTarget::Discriminator(window.discriminator)`（**実 browse 経路**）+ `paa_dir=MAT_E2E_PAA_DIR`（本番 PAA ストア）で実行 → 本物 DAC の厳格 attestation を通過することがここの主眼。
5. 新 fabric セッションで onoff toggle → read で反転確認 → toggle で戻す。
6. `encode_remove_fabric(dev.fabric_index.unwrap())` を新 fabric セッションで invoke → status 0。
7. 本番 fabric session（手順 2 のもの、または再 CASE）で read が通る = 本番無傷。

- [ ] **Step 2: コンパイル確認**

Run: `cargo test -p mat-controller --test live_commission_real --no-run`
Expected: ビルド成功

- [ ] **Step 3: `scripts/e2e-m6-real.sh` を書く**

`e2e-m3.sh` を雛形に（クロスビルド → ssh cat 転送 → 実機で実行）。差分:
- テストバイナリ名 `live_commission_real`、転送先 `/tmp/live_commission_real`。
- 追加 env: `MAT_E2E_PAA_DIR`（既定 `$HOME/.config/mat/paa-trust-store`）。
- 必須 env: `MAT_E2E_HOST` / `MAT_E2E_NODE_ID`。既定: `MAT_E2E_FABRIC_INDEX=2`（jarvis の実測値だが既定はダミー 1 のまま、コメントで注記）、`MAT_E2E_IFACE` は default route 自動検出（jarvis は eth0 になる）。

- [ ] **Step 4: 実機 E2E 実行（ユーザーと合意の上で）**

```bash
MAT_E2E_HOST=<jarvis> MAT_E2E_NODE_ID=<対象 node> MAT_E2E_FABRIC_INDEX=2 \
MAT_E2E_ISSUER_INDEX=<alpha idx> task e2e:m6:real
```

Expected: 全手順 PASS。**確認事項**（spec 決定 6）: 途中失敗した場合、第二 fabric が failsafe 満了（120s）で自動 revert されること（`RemoveFabric` 不要になること）をログで確認して記録する。

注意: 対象 Nanoleaf は事前に mat 経由の onoff で疎通確認しておく（弱リンク個体 node5 は避ける）。本実行はユーザー立ち会いで行い、結果（成功／実行時訂正）を本計画に追記する。

- [ ] **Step 5: ARCHITECTURE.md へ M6a 完了を追記**

Phase 5 節の M5 項の直後に、M5 項と同じ形式で: M6a の完了日・スコープ（on-network PASE / attestation 厳格+CD warn / RCAC 自己生成 / native open-window / `_matterc` browse）・本番経路無変更（ライブラリ + E2E のみ）・E2E 結果（ローカル `task e2e:m6`、実機 `task e2e:m6:real`）・M6b（BTP/BLE + Thread dataset）が未着手で chip-tool 廃止はその後であることを記載。

- [ ] **Step 6: `task check` → コミット**

```bash
task check
git add crates/mat-controller/tests/live_commission_real.rs scripts/e2e-m6-real.sh Taskfile.yml ARCHITECTURE.md
git commit -m "test(controller)+docs: M6a 実機E2Eハーネス（使い捨て第二fabric）とARCHITECTURE反映 (M6a Task12)"
```

---

## 完了条件（spec の受け入れ基準と同一）

1. ユニットテスト全合格（SPAKE2+ RFC ベクタ / setup code 既知ペア / attestation 正常+失敗系）。
2. `task e2e:m6` 合格（誤 passcode / commission+制御 / multi-admin open-window の 3 点）。
3. `task e2e:m6:real` 合格 — 本番 fabric・本番 matd に影響ゼロ。
4. `task check` 合格。
