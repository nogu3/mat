# Tier 2 入力検証（PASE / open-window / setup_code）実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 監査 Tier 2 の入力検証 7 件（PASE の PBKDF パラメータ / responderSessionId / Pake3 ack、open-window の iterations / discriminator、manual code の桁制約、base38 の非正準チャンク）を実装する。

**Architecture:** すべて `crates/mat-controller` 内の検証追加（+ `crates/mat-native` のエラー写像 2 箇所と docs）。ネットワーク入力（PBKDFParamResponse）は PBKDF2 実行前に spec §3.9 範囲を強制し、違反時は responder のセッションスロット解放のため StatusReport を送ってから中断。CLI 引数（open-window）は invoke 送信前に新 variant `CommissionError::InvalidArgument` で弾き `parse_error` に写像。setup_code は parse を spec §5.1.3/§5.1.4 準拠に締める。

**Tech Stack:** Rust（workspace）。テストは各モジュール内 `#[cfg(test)]`、PASE のワイヤ挙動はループバック UDP の fake-device パターン（既存 `confirm_mismatch_sends_abort_status_report` と同型）。

**Spec:** `docs/superpowers/specs/2026-08-10-tier2-input-validation-design.md`

## Global Constraints

- ブランチ: `tier2-input-validation`（作成済み・spec コミット済み）。
- コミットメッセージは日本語・repo の流儀（`fix(mat-controller): ...` / `chore: ...`）。末尾に以下を付ける:
  ```
  Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
  Claude-Session: https://claude.ai/code/session_01VCx1L8mz29knxgNHFcAzNC
  ```
- 検証コマンド: 単体は `cargo test -p mat-controller <filter>`。タスク完了ごとに `cargo clippy --workspace --all-targets -- -D warnings` が通ること。最終タスクで `task check` 全緑。
- spec 範囲: PBKDF iterations `1000..=100_000`、salt 長 `16..=32` バイト（spec §3.9）。discriminator は 12-bit（`<= 0x0FFF`）。manual code passcode は `1..=99_999_998`（spec §5.1.3）。
- trivial passcode（`INVALID_PASSCODES`）の parse 時拒否は**やらない**（設計判断: interop リスク。生成側は既に回避済み）。
- stdout/stderr/JSON スキーマは変更しない。新しい ErrorKind は追加しない（既存 `ParseError` を使う）。

---

### Task 1: PASE — PBKDFParamResponse の iterations/salt 検証 + 中断 StatusReport

**Files:**
- Modify: `crates/mat-controller/src/pase.rs`（`establish` の decode 直後、定数、`validate_pbkdf_params`、テスト）

**Interfaces:**
- Produces: `pub(crate) const PBKDF_ITERATIONS_MIN: u32 = 1000;` / `pub(crate) const PBKDF_ITERATIONS_MAX: u32 = 100_000;`（pase.rs トップレベル。**Task 3 が `crate::pase::PBKDF_ITERATIONS_MIN/MAX` として import する**）
- Consumes: 既存の `encode_status_report` / `SC_PROTOCOL_CODE_INVALID_PARAMETER` / `GENERAL_CODE_FAILURE` / `ex.send_once`（すべて pase.rs 内で既に使用中）

- [ ] **Step 1: ループバックテストヘルパを module スコープへ切り出す（準備リファクタ）**

`pase.rs` の `#[cfg(test)] mod tests` 内、`confirm_mismatch_sends_abort_status_report` テスト**関数の中**に定義されている 4 ヘルパ `fast_cfg()` / `build_unsecured()` / `recv_dg()` / `decode_unsecured()` を、**tests モジュール直下**（テスト関数の外）へそのまま移動する。`random_point()` は cB 不一致テスト専用なので関数内に残す。ヘルパが使う `use crate::message::{Destination, MessageHeader, ProtocolHeader};` と `use crate::transport::{UdpTransport, MAX_DATAGRAM};` はテスト関数内 use なので、tests モジュール直下の use に引き上げる。コードは 1 文字も変えない（移動のみ）。

- [ ] **Step 2: リファクタ後に既存テストが通ることを確認**

Run: `cargo test -p mat-controller pase`
Expected: PASS（既存全テスト、特に `confirm_mismatch_sends_abort_status_report`）

- [ ] **Step 3: リファクタをコミット**

```bash
git add crates/mat-controller/src/pase.rs
git commit -m "refactor(mat-controller): pase ループバックテストヘルパを tests モジュール直下へ移動"
```

- [ ] **Step 4: 値域検証の failing テストを書く**

`pase.rs` の tests モジュールに追加:

```rust
    #[test]
    fn validates_pbkdf_params_bounds() {
        // spec §3.9: iterations 1000..=100_000, salt 16..=32 bytes。境界値は受理。
        assert!(validate_pbkdf_params(1000, 16).is_ok());
        assert!(validate_pbkdf_params(100_000, 32).is_ok());
        assert!(validate_pbkdf_params(999, 16).is_err());
        assert!(validate_pbkdf_params(100_001, 16).is_err());
        assert!(validate_pbkdf_params(1000, 15).is_err());
        assert!(validate_pbkdf_params(1000, 33).is_err());
    }
```

- [ ] **Step 5: コンパイル失敗（= fail）を確認**

Run: `cargo test -p mat-controller validates_pbkdf_params_bounds`
Expected: FAIL — `cannot find function validate_pbkdf_params`

- [ ] **Step 6: 定数と `validate_pbkdf_params` を実装**

`pase.rs` のトップレベル（既存 `const SC_PROTOCOL_CODE_INVALID_PARAMETER` の近く）に追加:

```rust
/// spec §3.9 の PBKDF 制約（CRYPTO_PBKDF_ITERATIONS_MIN/MAX）。iterations の
/// 範囲は commissioning.rs の open-window 引数検証も同じ値を参照する。
pub(crate) const PBKDF_ITERATIONS_MIN: u32 = 1000;
pub(crate) const PBKDF_ITERATIONS_MAX: u32 = 100_000;
const PBKDF_SALT_LEN_MIN: usize = 16;
const PBKDF_SALT_LEN_MAX: usize = 32;

/// PBKDFParamResponse の iterations / salt 長を spec §3.9 の範囲に強制する。
/// unsecured 交換なので on-path 偽造で iterations=u32::MAX を注入されると
/// 同期 PBKDF2 が数時間回る（current_thread ランタイムなので timeout も
/// 効かない）—— PBKDF2 実行前に必ず呼ぶ。Err は Malformed に渡す detail。
fn validate_pbkdf_params(iterations: u32, salt_len: usize) -> Result<(), &'static str> {
    if !(PBKDF_ITERATIONS_MIN..=PBKDF_ITERATIONS_MAX).contains(&iterations) {
        return Err("pbkdf iterations out of range");
    }
    if !(PBKDF_SALT_LEN_MIN..=PBKDF_SALT_LEN_MAX).contains(&salt_len) {
        return Err("pbkdf salt length out of range");
    }
    Ok(())
}
```

- [ ] **Step 7: テストが通ることを確認**

Run: `cargo test -p mat-controller validates_pbkdf_params_bounds`
Expected: PASS

- [ ] **Step 8: ワイヤ挙動の failing テストを書く（fake device ループバック）**

tests モジュールに追加。既存 `confirm_mismatch_sends_abort_status_report` と同じ骨格で、Step 1 で切り出したヘルパを使う:

```rust
    /// Fake device が spec §3.9 範囲外の iterations (999) を返したとき、
    /// initiator が PBKDF2 実行前に中断し、abort StatusReport
    /// (FAILURE / SecureChannel / kInvalidParameter) を同一 exchange で送る
    /// ことを検証する。999 を使うのは、検証が未実装でも PBKDF2 が一瞬で
    /// 終わり、テストが（ハングではなく）即 fail するようにするため。
    #[tokio::test]
    async fn bad_pbkdf_params_send_abort_status_report() {
        let responder_transport = UdpTransport::bind_addr("[::1]:0".parse().unwrap())
            .await
            .unwrap();
        let responder_addr = responder_transport.local_addr().unwrap();
        let initiator_transport = Arc::new(Transport::Udp(Arc::new(
            UdpTransport::bind_addr("[::1]:0".parse().unwrap())
                .await
                .unwrap(),
        )));

        let cfg = fast_cfg();
        let establish_task = {
            let transport = Arc::clone(&initiator_transport);
            let cfg = cfg.clone();
            tokio::spawn(async move { establish(transport, responder_addr, 20202021, &cfg).await })
        };

        // --- PBKDFParamRequest -> 範囲外 iterations の PBKDFParamResponse ---
        let (req_buf, initiator_addr) = recv_dg(&responder_transport).await;
        let (req_header, req_proto, _req_payload) =
            decode_unsecured(&req_buf).expect("valid PBKDFParamRequest datagram");
        assert_eq!(req_proto.opcode, OPCODE_PBKDF_PARAM_REQUEST);

        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(1), &[1u8; 32]); // initiatorRandom echo (ignored)
        w.put_bytes(Tag::Context(2), &[2u8; 32]); // responderRandom (ignored)
        w.put_uint(Tag::Context(3), 0xBEEF); // responderSessionId
        w.start_struct(Tag::Context(4));
        w.put_uint(Tag::Context(1), 999); // iterations: spec 下限 1000 未満
        w.put_bytes(Tag::Context(2), b"0123456789abcdef"); // salt (16B, 正当)
        w.end_container();
        w.end_container();
        let resp_dg = build_unsecured(
            100,
            OPCODE_PBKDF_PARAM_RESPONSE,
            req_proto.exchange_id,
            Some(req_header.message_counter),
            &w.finish(),
        );
        responder_transport
            .send_to(&resp_dg, initiator_addr)
            .await
            .unwrap();

        // --- 次のデータグラムは Pake1 ではなく abort StatusReport のはず ---
        let (abort_buf, _) = recv_dg(&responder_transport).await;
        let (_abort_header, abort_proto, abort_payload) =
            decode_unsecured(&abort_buf).expect("valid abort datagram");
        assert_eq!(abort_proto.opcode, OPCODE_STATUS_REPORT);
        assert_eq!(abort_proto.protocol_id, PROTOCOL_ID_SECURE_CHANNEL);
        assert_eq!(abort_proto.exchange_id, req_proto.exchange_id);
        let (general_code, protocol_id, protocol_code) =
            parse_status_report(&abort_payload).expect("well-formed StatusReport");
        assert_eq!(general_code, GENERAL_CODE_FAILURE);
        assert_eq!(protocol_id, u32::from(PROTOCOL_ID_SECURE_CHANNEL));
        assert_eq!(protocol_code, SC_PROTOCOL_CODE_INVALID_PARAMETER);

        let result = establish_task.await.expect("establish task panicked");
        assert!(matches!(
            result,
            Err(PaseError::Malformed("pbkdf iterations out of range"))
        ));
    }
```

- [ ] **Step 9: fail を確認**

Run: `cargo test -p mat-controller bad_pbkdf_params_send_abort_status_report`
Expected: FAIL — abort 待ちのところへ Pake1 データグラムが来て `assert_eq!(abort_proto.opcode, OPCODE_STATUS_REPORT)` が落ちる

- [ ] **Step 10: `establish` に検証を組み込む**

`establish` 内、`let resp = decode_pbkdf_param_response(&resp_payload)?;`（現 316 行付近）の**直後**に追加:

```rust
    // spec §3.9 範囲外の PBKDF パラメータは PBKDF2 実行前に拒否する。黙って
    // 諦めると responder が Pake1 待ちのままセッション確立スロットを保持し
    // 続け、直後の再試行が固まる（cB 不一致経路と同じ理由 — そちらの
    // コメント参照）ので、StatusReport を send_once で一度だけ送ってから返す。
    if let Err(what) = validate_pbkdf_params(resp.iterations, resp.salt.len()) {
        let sr = encode_status_report(
            GENERAL_CODE_FAILURE,
            u32::from(PROTOCOL_ID_SECURE_CHANNEL),
            SC_PROTOCOL_CODE_INVALID_PARAMETER,
        );
        let _ = ex
            .send_once(PROTOCOL_ID_SECURE_CHANNEL, OPCODE_STATUS_REPORT, &sr)
            .await;
        return Err(PaseError::Malformed(what));
    }
```

- [ ] **Step 11: 全 pase テストが通ることを確認**

Run: `cargo test -p mat-controller pase`
Expected: PASS（新規 2 + 既存全部）

- [ ] **Step 12: clippy + コミット**

Run: `cargo clippy --workspace --all-targets -- -D warnings`

```bash
git add crates/mat-controller/src/pase.rs
git commit -m "fix(mat-controller): PBKDFParamResponse の iterations/salt を spec §3.9 範囲に強制（監査 Tier 2）

範囲外は PBKDF2 実行前に拒否し、responder のセッション確立スロット解放の
ため StatusReport(FAILURE/kInvalidParameter) を送ってから中断する。
on-path 偽造の iterations=u32::MAX で CLI が数時間ハングする穴を塞ぐ。"
```

---

### Task 2: PASE — responderSessionId=0 拒否 + Pake3 応答の ack 検証

**Files:**
- Modify: `crates/mat-controller/src/pase.rs`（`decode_pbkdf_param_response` の Ok 組み立て部、`establish` の Pake3 応答受信部、テスト）

**Interfaces:**
- Consumes: なし（pase.rs 内で完結）
- Produces: なし（挙動変更のみ）

- [ ] **Step 1: sessionId=0 拒否の failing テストを書く**

`pase.rs` の tests モジュールに追加:

```rust
    #[test]
    fn rejects_zero_responder_session_id() {
        // 0 は予約値。CASE 側 (case.rs の Sigma2 decode) と対称の扱い。
        let mut w = crate::tlv::Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(3), 0); // responderSessionId = 0
        w.start_struct(Tag::Context(4));
        w.put_uint(Tag::Context(1), 1000);
        w.put_bytes(Tag::Context(2), b"0123456789abcdef");
        w.end_container();
        w.end_container();
        assert!(matches!(
            decode_pbkdf_param_response(&w.finish()),
            Err(PaseError::Malformed("responder session id must be non-zero"))
        ));
    }
```

- [ ] **Step 2: fail を確認**

Run: `cargo test -p mat-controller rejects_zero_responder_session_id`
Expected: FAIL — 現状 0 を受理して decode が Ok を返す

- [ ] **Step 3: `decode_pbkdf_param_response` に拒否を実装**

関数末尾の Ok 組み立てを次に置き換える（session id の取り出しを先行させて 0 を弾く。case.rs の Sigma2 decode と同じ構造）:

```rust
    let responder_session_id =
        responder_session_id.ok_or(PaseError::Malformed("responder session id"))?;
    if responder_session_id == 0 {
        return Err(PaseError::Malformed("responder session id must be non-zero"));
    }

    Ok(PbkdfParamResponse {
        responder_session_id,
        iterations: iterations.ok_or(PaseError::Malformed("iterations"))?,
        salt: salt.ok_or(PaseError::Malformed("salt"))?,
    })
```

- [ ] **Step 4: pass を確認**

Run: `cargo test -p mat-controller rejects_zero_responder_session_id`
Expected: PASS

- [ ] **Step 5: Pake3 応答の ack 検証を追加（テストなし — 設計 spec の決定事項）**

PASE 成功経路のオフライン応答器が未整備（Tier 5 の既知ギャップ）のため、この項目は既存パターンの移植のみで新規テストは書かない。`establish` 内の Pake3 応答受信（現 387 行付近）:

```rust
    let msg3 = match resp3 {
        Some(m) => m,
        None => ex.recv(RECV_TIMEOUT).await.map_err(PaseError::Exchange)?,
    };
```

を、PBKDFParamResponse / Pake2 受信（現 287-298 / 333-344 行）と同一の形に置き換える:

```rust
    let msg3 = match resp3 {
        Some(m) => {
            // On a reliable transport (BTP) MRP is disabled, so the peer's
            // response carries no piggybacked ack — skip the ack check there.
            // Over UDP the real response must ack the request we just sent.
            if !transport.is_reliable() && m.proto.acked_counter != ex.last_sent_counter() {
                return Err(PaseError::NotAcked);
            }
            m
        }
        None => ex.recv(RECV_TIMEOUT).await.map_err(PaseError::Exchange)?,
    };
```

- [ ] **Step 6: 全 pase テスト + clippy**

Run: `cargo test -p mat-controller pase && cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS（ループバック 2 テストは Pake3 まで到達しないため影響なし）

- [ ] **Step 7: コミット**

```bash
git add crates/mat-controller/src/pase.rs
git commit -m "fix(mat-controller): PASE の responderSessionId=0 拒否と Pake3 応答 ack 検証（監査 Tier 2）

どちらも既存実装との対称化: sessionId=0 拒否は CASE の Sigma2 と、
Pake3 の ack 検証は同関数内の PBKDFParamResponse/Pake2 受信と同じ規律。"
```

---

### Task 3: open-window 引数検証（InvalidArgument variant + parse_error 写像）

**Files:**
- Modify: `crates/mat-controller/src/commissioning.rs`（`validate_window_params` 新設、`open_commissioning_window` 冒頭、`CommissionError` variant + Display、テスト）
- Modify: `crates/mat-native/src/lib.rs`（`map_commission_err` に写像追加、テスト）
- Modify: `crates/mat-native/src/commission.rs`（`kind_of` に写像追加）

**Interfaces:**
- Consumes: `crate::pase::PBKDF_ITERATIONS_MIN` / `crate::pase::PBKDF_ITERATIONS_MAX`（Task 1 が定義、`pub(crate) const u32`）
- Produces: `CommissionError::InvalidArgument { what: &'static str }`（mat-native の 2 写像がこの variant 名で match する）

- [ ] **Step 1: failing テストを書く**

`commissioning.rs` の `#[cfg(test)] mod tests`（現 1601 行）に追加:

```rust
    #[test]
    fn validates_window_params() {
        // 境界値は受理
        assert!(validate_window_params(0x0FFF, 1000).is_ok());
        assert!(validate_window_params(0, 100_000).is_ok());
        // 範囲外は InvalidArgument
        assert!(matches!(
            validate_window_params(0x1000, 1000),
            Err(CommissionError::InvalidArgument { .. })
        ));
        assert!(matches!(
            validate_window_params(0, 999),
            Err(CommissionError::InvalidArgument { .. })
        ));
        assert!(matches!(
            validate_window_params(0, 100_001),
            Err(CommissionError::InvalidArgument { .. })
        ));
    }
```

- [ ] **Step 2: fail を確認**

Run: `cargo test -p mat-controller validates_window_params`
Expected: FAIL — `cannot find function validate_window_params` / variant 未定義

- [ ] **Step 3: variant・Display・検証関数・呼び出しを実装**

`CommissionError`（commissioning.rs 現 1474 行）に variant を追加:

```rust
    /// 呼び出し側引数の値域違反。デバイスへ invoke を送る前に検出する。
    InvalidArgument { what: &'static str },
```

Display impl（現 1509 行〜）の match に arm を追加:

```rust
            CommissionError::InvalidArgument { what } => {
                write!(f, "commissioning: invalid argument: {what}")
            }
```

`open_commissioning_window` の直前に自由関数を追加:

```rust
/// open-window 引数の値域検証。iterations は PASE と同じ spec §3.9 範囲、
/// discriminator は 12-bit。違反は invoke 送信前に InvalidArgument で弾く —
/// 特に discriminator 超過は、デバイスが受理してしまうと window 開放後に
/// setup_code の assert で panic して生成 passcode が失われ、開いた window
/// が放置されるため、送信前に止める順序が重要。
fn validate_window_params(discriminator: u16, iterations: u32) -> Result<(), CommissionError> {
    if discriminator > 0x0FFF {
        return Err(CommissionError::InvalidArgument {
            what: "discriminator must fit in 12 bits (<= 0x0FFF)",
        });
    }
    if !(crate::pase::PBKDF_ITERATIONS_MIN..=crate::pase::PBKDF_ITERATIONS_MAX)
        .contains(&iterations)
    {
        return Err(CommissionError::InvalidArgument {
            what: "iterations must be in 1000..=100000",
        });
    }
    Ok(())
}
```

`open_commissioning_window` 本体の先頭（`let passcode = random_valid_passcode();` の**前**）に:

```rust
    validate_window_params(discriminator, iterations)?;
```

- [ ] **Step 4: pass を確認**

Run: `cargo test -p mat-controller validates_window_params`
Expected: PASS

- [ ] **Step 5: mat-native の写像の failing テストを書く**

`crates/mat-native/src/lib.rs` の `#[cfg(test)] mod tests` に追加:

```rust
    #[test]
    fn invalid_argument_maps_to_parse_error() {
        let e = map_commission_err(
            mat_controller::commissioning::CommissionError::InvalidArgument {
                what: "iterations must be in 1000..=100000",
            },
        );
        assert_eq!(e.kind, mat_core::error::ErrorKind::ParseError);
    }
```

（`ErrorKind` が lib.rs で既に use されていれば `ErrorKind::ParseError` と短く書く。パスは既存 use に合わせる。）

- [ ] **Step 6: fail を確認**

Run: `cargo test -p mat-native invalid_argument_maps_to_parse_error`
Expected: FAIL — 現状 `_ => Other` に落ちるので kind が `Other`

- [ ] **Step 7: 写像 2 箇所を実装**

`crates/mat-native/src/lib.rs` の `map_commission_err`、`CommissionError::Timeout(_)` の arm の後（`_ =>` の前）に追加:

```rust
        CommissionError::InvalidArgument { .. } => {
            MatError::new(ErrorKind::ParseError, format!("native: {e}"))
        }
```

`crates/mat-native/src/commission.rs` の `kind_of`、`CommissionError::Malformed { .. } | CommissionError::Csr(_) => ErrorKind::ParseError,` の arm に variant を足す（commission フロー自体は今日この variant を出さないが、写像表を全域で正しく保つ）:

```rust
        CommissionError::Malformed { .. }
        | CommissionError::Csr(_)
        | CommissionError::InvalidArgument { .. } => ErrorKind::ParseError,
```

- [ ] **Step 8: pass + 全体確認**

Run: `cargo test -p mat-native invalid_argument_maps_to_parse_error && cargo test -p mat-controller -p mat-native && cargo clippy --workspace --all-targets -- -D warnings`
Expected: PASS

- [ ] **Step 9: コミット**

```bash
git add crates/mat-controller/src/commissioning.rs crates/mat-native/src/lib.rs crates/mat-native/src/commission.rs
git commit -m "fix(mat-controller): open-window の iterations/discriminator を invoke 送信前に検証（監査 Tier 2）

iterations 4000000000 での compute_verifier 数時間ハングと、
discriminator>0x0FFF がデバイス受理後に setup_code の assert panic で
passcode を失い window を放置する順序の致命性を、送信前の
InvalidArgument（CLI では parse_error）で構造的に塞ぐ。"
```

---

### Task 4: setup_code — manual code の桁制約（passcode 上限 / 先頭桁 / VID-PID フラグ整合）

**Files:**
- Modify: `crates/mat-controller/src/setup_code.rs`（`SetupCodeError` variant 追加、`parse_manual_code`、テスト）

**Interfaces:**
- Consumes: なし
- Produces: `SetupCodeError::PasscodeOutOfRange` / `SetupCodeError::BadFirstDigit` / `SetupCodeError::VidPidMismatch`（Task 5 と同じ enum に追加するが互いに独立）

- [ ] **Step 1: failing テストを書く**

`setup_code.rs` の tests モジュール（現 368 行）に追加。check digit は本物の `verhoeff_check_digit` で算出して作る:

```rust
    /// body（check digit 抜き）から検証付き manual code を組み立てる。
    fn manual_code_with_check(body: &str) -> String {
        let digits = digits_of(body).unwrap();
        let check = verhoeff_check_digit(&digits);
        format!("{body}{check}")
    }

    #[test]
    fn rejects_manual_code_passcode_over_limit() {
        // digit1=0, digits2_6=16383 (disc=0, low14=0x3FFF), digits7_10=9999
        // → passcode = (9999<<14)|16383 = 163,839,999 > 99,999,998
        let code = manual_code_with_check("0163839999");
        assert_eq!(
            parse_manual_code(&code),
            Err(SetupCodeError::PasscodeOutOfRange)
        );
    }

    #[test]
    fn rejects_manual_code_first_digit_over_7() {
        // digit1 は 3-bit (vid_pid<<2 | disc上位2bit) なので 8/9 は不正。
        let code = manual_code_with_check("8005491233");
        assert_eq!(
            parse_manual_code(&code),
            Err(SetupCodeError::BadFirstDigit)
        );
    }

    #[test]
    fn rejects_manual_code_vid_pid_flag_mismatch() {
        // 11 桁（VID/PID なし）なのに digit1 の bit2 (vid_pid_present) が 1。
        // passcode 20202021: low14=549 → digits2_6="00549", digits7_10="1233"。
        let code = manual_code_with_check("4005491233");
        assert_eq!(
            parse_manual_code(&code),
            Err(SetupCodeError::VidPidMismatch)
        );
    }
```

- [ ] **Step 2: fail を確認**

Run: `cargo test -p mat-controller rejects_manual_code`
Expected: FAIL — variant 未定義でコンパイルエラー

- [ ] **Step 3: variant と検証を実装**

`SetupCodeError`（setup_code.rs 冒頭）に追加:

```rust
    PasscodeOutOfRange,
    BadFirstDigit,
    VidPidMismatch,
```

Display impl に arm を追加:

```rust
            SetupCodeError::PasscodeOutOfRange => {
                write!(f, "setup passcode exceeds the Matter maximum (99999998)")
            }
            SetupCodeError::BadFirstDigit => {
                write!(f, "manual code first digit must be 0-7")
            }
            SetupCodeError::VidPidMismatch => {
                write!(f, "manual code VID/PID presence flag contradicts its length")
            }
```

`parse_manual_code` の `let digit1 = u32::from(digits[0]);` の直後に追加:

```rust
    // digit1 は (vid_pid_present << 2) | 短縮 discriminator 上位 2bit の
    // 3-bit 値（spec §5.1.3.1）。8/9 は表現外、bit2 は桁数と一致すべき。
    if digit1 > 7 {
        return Err(SetupCodeError::BadFirstDigit);
    }
    if (digit1 >> 2 == 1) != (s.len() == 21) {
        return Err(SetupCodeError::VidPidMismatch);
    }
```

既存の `if passcode == 0 { ... }` の直後に追加:

```rust
    if passcode > 99_999_998 {
        // digits7_10 が 9999 まで振れるので 27-bit を超える値が構成できる。
        return Err(SetupCodeError::PasscodeOutOfRange);
    }
```

- [ ] **Step 4: pass + 回帰を確認**

Run: `cargo test -p mat-controller setup_code`
Expected: PASS（新規 3 + 既存全部。既存の正当コードは digit1≤7・11 桁・passcode 範囲内なので通過）

- [ ] **Step 5: clippy + コミット**

Run: `cargo clippy --workspace --all-targets -- -D warnings`

```bash
git add crates/mat-controller/src/setup_code.rs
git commit -m "fix(mat-controller): manual code の桁制約を検証（監査 Tier 2）

passcode>99,999,998（27-bit 超えが黙って通る）、先頭桁 8/9、
VID/PID presence フラグと桁数の不整合を parse で拒否する。"
```

---

### Task 5: setup_code — base38 非正準チャンクの拒否

**Files:**
- Modify: `crates/mat-controller/src/setup_code.rs`（`decode_base38_chunk` のシグネチャ変更、`base38_decode` の呼び出し 3 箇所、variant 追加、テスト）

**Interfaces:**
- Consumes: なし（Task 4 と同じファイルだが独立変更 — Task 4 完了後に着手）
- Produces: `SetupCodeError::NonCanonicalBase38`。`decode_base38_chunk(chars: &[u8], out_bytes: usize) -> Result<u64, SetupCodeError>`（内部関数のシグネチャ変更）

- [ ] **Step 1: failing テストを書く**

tests モジュールに追加:

```rust
    #[test]
    fn rejects_non_canonical_base38_chunks() {
        // '9' の digit 値は 9。5 文字 "99999" = 9*(1+38+38²+38³+38⁴)
        // = 19,273,419 > 0xFFFFFF(3 byte 上限) → 非正準。
        assert_eq!(
            base38_decode("99999"),
            Err(SetupCodeError::NonCanonicalBase38)
        );
        // 4 文字 "9999" = 507,195 > 0xFFFF(2 byte 上限)
        assert_eq!(
            base38_decode("9999"),
            Err(SetupCodeError::NonCanonicalBase38)
        );
        // 2 文字 "99" = 351 > 0xFF(1 byte 上限)
        assert_eq!(base38_decode("99"), Err(SetupCodeError::NonCanonicalBase38));
    }
```

- [ ] **Step 2: fail を確認**

Run: `cargo test -p mat-controller rejects_non_canonical_base38_chunks`
Expected: FAIL — variant 未定義でコンパイルエラー

- [ ] **Step 3: variant と検証を実装**

`SetupCodeError` に追加:

```rust
    NonCanonicalBase38,
```

Display impl に arm を追加:

```rust
            SetupCodeError::NonCanonicalBase38 => {
                write!(f, "QR base38 chunk exceeds its byte range (non-canonical encoding)")
            }
```

`decode_base38_chunk` を出力バイト数つきに変更（現状は下位ビット黙殺 = 同一 payload に複数表現が生まれる）:

```rust
fn decode_base38_chunk(chars: &[u8], out_bytes: usize) -> Result<u64, SetupCodeError> {
    let mut value: u64 = 0;
    for (i, &c) in chars.iter().enumerate() {
        let digit = base38_char_value(c).ok_or(SetupCodeError::BadChar)?;
        value += u64::from(digit) * 38u64.pow(i as u32);
    }
    // チャンクが表現するバイト数を超える値は非正準エンコーディング。
    if value >> (8 * out_bytes) != 0 {
        return Err(SetupCodeError::NonCanonicalBase38);
    }
    Ok(value)
}
```

`base38_decode` 内の呼び出し 3 箇所を更新:
- 5 文字 full group: `decode_base38_chunk(&bytes[g * 5..g * 5 + 5], 3)?`
- tail 4 文字: `decode_base38_chunk(&bytes[tail_start..tail_start + 4], 2)?`
- tail 2 文字: `decode_base38_chunk(&bytes[tail_start..tail_start + 2], 1)?`

- [ ] **Step 4: pass + 回帰を確認**

Run: `cargo test -p mat-controller setup_code`
Expected: PASS（既存の周知 QR `MT:-24J0AFN00KA0648G00` は正準なので通過）

- [ ] **Step 5: clippy + コミット**

Run: `cargo clippy --workspace --all-targets -- -D warnings`

```bash
git add crates/mat-controller/src/setup_code.rs
git commit -m "fix(mat-controller): base38 の非正準チャンクを拒否（監査 Tier 2）

チャンク値が表現バイト数の上限を超える場合、従来は下位ビットを黙って
切り捨てていた（同一 payload の複数表現を許す）。エラーにする。"
```

---

### Task 6: docs 追記 + バージョン 1.25.0 + 全体検証

**Files:**
- Modify: `docs/commands.md`（open-window の箇条書きに値域 1 行）
- Modify: `Cargo.toml`（workspace version）
- Modify: `Cargo.lock`（ビルドで自動更新される分）

**Interfaces:**
- Consumes: Task 3 の挙動（`--iteration` / `--discriminator` 範囲外 → `parse_error`）
- Produces: なし

- [ ] **Step 1: docs/commands.md に値域を追記**

open-window セクションの箇条書き（「If `--discriminator` is omitted, ...」の直後）に追加:

```markdown
- `--iteration` must be in `1000..=100000` (spec §3.9) and `--discriminator`
  within 12 bits (`0..=4095`). Out-of-range values fail fast with
  `parse_error` before any invoke is sent.
```

- [ ] **Step 2: workspace version を上げる**

`Cargo.toml` 6 行目: `version = "1.24.0"` → `version = "1.25.0"`

- [ ] **Step 3: CI 相当を全部回す**

Run: `task check`
Expected: fmt:check + clippy + test すべて緑

- [ ] **Step 4: コミット**

```bash
git add docs/commands.md Cargo.toml Cargo.lock
git commit -m "chore: 1.25.0（監査 Tier 2 — PASE/open-window/setup_code 入力検証）"
```

---

## マージ前の実機 E2E（計画外・メインセッションで実施）

メモリの規律どおり、main マージ前に jarvis 実機 E2E を `*.new`（本番未置換）で行う:
- オンネットワーク commission 経路の退行なし（PASE が正当デバイスで通る）
- `open-window` の正常系（範囲内引数で window が開き manual_code/qr_payload が出る）
- `open-window --iteration 999` / `--discriminator 4096` が即 `parse_error` で落ちる
