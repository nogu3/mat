# v1 前品質修正（エラー分類ほか 8 項目）Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** mat-v1-roadmap の残 8 項目（挙動変更ありの品質修正）を実装し、1.0.0 bump の前提を満たす。

**Architecture:** 変更はすべてエラー分類・防御的パニックの typed error 化・テスト追加で、プロトコル層のロジックは触らない。exit code の意味が変わる準破壊的変更（commissioning の timeout/拒否分離、matd 経路の `matd_unavailable` 拡大）を v1 契約確定前に入れる。

**Tech Stack:** Rust workspace（crates: mat-core / mat-native / mat-controller / mat / matd）、Taskfile（`task check` = fmt:check + clippy -D warnings + test）。

## Global Constraints

- 各タスクの最後に `task check` を通してからコミットする（CLAUDE.md）。
- stdout は純 JSON、エラーは stderr に `{"error":{"kind":...,"detail":...}}`（design rule 2/3）。panic で JSON 契約を破らないことが本計画の柱。
- リポジトリは public。テストの値はダミーのみ（実 node_id・実 IP 禁止; IPv6 は `fd00::/8` のローカル例）。
- コメントは既存コードに合わせ日本語。既存の命名・イディオムに従う。
- プロトコルコードは backend crate（mat-controller / mat-native）のみ（design rule 1）。本計画で mat / matd の command 層に足すのはエラー写像と検証だけ。
- コミットメッセージ末尾: `Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>`。

---

### Task 1: commissioning `kind_of` の timeout / 拒否分離

**Files:**
- Modify: `crates/mat-native/src/commission.rs:61-72`（`kind_of`）+ 同ファイル `mod tests`（:399〜）
- Modify: `crates/mat-controller/src/commissioning.rs:1442-1450`（doc 表）
- Modify: `README.md:1006`（`commission_failed` 行）

**Interfaces:**
- Consumes: `mat_controller::{pase::PaseError, case::CaseError, session::SessionError, exchange::ExchangeError}`（全 variant pub）。
- Produces: `kind_of(&CommissionError) -> ErrorKind` の新写像。シグネチャ不変（後続タスクへの影響なし）。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-native/src/commission.rs` の `mod tests` に追加:

```rust
    /// v1 品質修正 1: `_ => CommissionFailed` が吸収していた Pase/Case/Session の
    /// うち、timeout 系 → `Timeout`(exit 3)、デバイス明示拒否系 → `DeviceRejected`
    /// (exit 4) に分離されること。
    #[test]
    fn kind_of_splits_timeout_and_rejection_out_of_commission_failed() {
        use mat_controller::case::CaseError;
        use mat_controller::exchange::ExchangeError;
        use mat_controller::pase::PaseError;
        use mat_controller::session::SessionError;

        // timeout 系（MRP 再送尽き）→ Timeout
        assert_eq!(
            kind_of(&CommissionError::Pase(PaseError::Exchange(
                ExchangeError::Timeout
            ))),
            ErrorKind::Timeout
        );
        assert_eq!(
            kind_of(&CommissionError::Case(CaseError::Exchange(
                ExchangeError::Timeout
            ))),
            ErrorKind::Timeout
        );
        assert_eq!(
            kind_of(&CommissionError::Session(SessionError::Timeout)),
            ErrorKind::Timeout
        );

        // 拒否系 → DeviceRejected
        assert_eq!(
            kind_of(&CommissionError::Pase(PaseError::ConfirmMismatch)),
            ErrorKind::DeviceRejected
        );
        assert_eq!(
            kind_of(&CommissionError::Pase(PaseError::StatusReport {
                general_code: 1,
                protocol_code: 0,
            })),
            ErrorKind::DeviceRejected
        );
        assert_eq!(
            kind_of(&CommissionError::Case(CaseError::PeerStatus {
                stage: "sigma2",
                general_code: 1,
                protocol_code: 0,
            })),
            ErrorKind::DeviceRejected
        );
        assert_eq!(
            kind_of(&CommissionError::Case(CaseError::Sigma2SignatureInvalid)),
            ErrorKind::DeviceRejected
        );

        // 上記以外の Pase/Case/Session は従来どおり CommissionFailed の残余
        assert_eq!(
            kind_of(&CommissionError::Pase(PaseError::NotAcked)),
            ErrorKind::CommissionFailed
        );
        assert_eq!(
            kind_of(&CommissionError::Case(CaseError::Tbe2DecryptFailed)),
            ErrorKind::CommissionFailed
        );
    }
```

（`ErrorKind` は `PartialEq` derive 済み — `crates/mat-core/src/error.rs:11`。tests mod 冒頭の `use super::*;` で `kind_of` / `CommissionError` / `ErrorKind` は見える。見えない場合のみ use を足す。）

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat-native kind_of_splits -- --nocapture`
Expected: FAIL（`ConfirmMismatch` 等が `CommissionFailed` になる）

- [ ] **Step 3: `kind_of` を実装**

`crates/mat-native/src/commission.rs:61-72` を置換:

```rust
/// `CommissionError` → mat の `ErrorKind`（spec の写像。発見の空振り
/// （`Discovery`）は `commission` 本体で `unreachable` に写すためここには来ない）。
///
/// v1 品質修正 1: 旧 `_ => CommissionFailed` が Pase/Case/Session を全部吸収して
/// いたのを分離 — timeout 系（MRP 再送尽き）は `Timeout`(exit 3)、デバイスの明示
/// 拒否（passcode 不一致 = SPAKE2+ confirm 不一致 / PASE・CASE の StatusReport 拒否 /
/// Sigma2 署名不正）は `DeviceRejected`(exit 4)。残余のみ `CommissionFailed`。
fn kind_of(e: &CommissionError) -> ErrorKind {
    use mat_controller::case::CaseError;
    use mat_controller::exchange::ExchangeError;
    use mat_controller::pase::PaseError;
    use mat_controller::session::SessionError;
    match e {
        CommissionError::Timeout(_)
        | CommissionError::Pase(PaseError::Exchange(ExchangeError::Timeout))
        | CommissionError::Case(CaseError::Exchange(ExchangeError::Timeout))
        | CommissionError::Session(SessionError::Timeout) => ErrorKind::Timeout,
        CommissionError::Pase(PaseError::ConfirmMismatch | PaseError::StatusReport { .. })
        | CommissionError::Case(CaseError::PeerStatus { .. } | CaseError::Sigma2SignatureInvalid) => {
            ErrorKind::DeviceRejected
        }
        CommissionError::Attestation(_) => ErrorKind::DeviceRejected,
        CommissionError::Noc(_) | CommissionError::CommandStatus { .. } => {
            ErrorKind::DeviceRejected
        }
        CommissionError::NetworkConfig { .. } => ErrorKind::Unreachable,
        CommissionError::Malformed { .. } | CommissionError::Csr(_) => ErrorKind::ParseError,
        _ => ErrorKind::CommissionFailed,
    }
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-native kind_of_splits`
Expected: PASS

- [ ] **Step 5: doc を追随させる**

`crates/mat-controller/src/commissioning.rs:1442-1450` の写像表を更新。行を差し替え:

```
/// | variant                                               | kind                | exit |
/// |--------------------------------------------------------|---------------------|------|
/// | `Timeout(_)` / `Pase(Exchange(Timeout))` /               | `timeout`           | 3    |
/// |   `Case(Exchange(Timeout))` / `Session(Timeout)`         |                     |      |
/// | `Attestation(_)` / `Noc(_)` / `CommandStatus { .. }` /   | `device_rejected`   | 4    |
/// |   `Pase(ConfirmMismatch)`（passcode 不一致）/            |                     |      |
/// |   `Pase(StatusReport)` / `Case(PeerStatus)` /            |                     |      |
/// |   `Case(Sigma2SignatureInvalid)`                         |                     |      |
/// | `NetworkConfig { .. }`                                   | `unreachable`       | 5    |
/// | `Malformed { .. }` / `Csr(_)`                            | `parse_error`       | 1    |
```

（`Discovery` 行と「上記以外すべて」行は既存のまま。「上記以外」の列挙から分離済み variant が漏れるので、`Pase` / `Case` / `Session` を「（上記で分離した variant を除く）」と注記する。）

`README.md:1006` を差し替え:

```
- `commission_failed` — commissioning failed (unclassified residue, exit 1).
  Since 1.0.0 timeouts during PASE/CASE map to `timeout` and explicit device
  refusals (wrong passcode / StatusReport rejection / bad Sigma2 signature) map
  to `device_rejected` instead of landing here.
```

- [ ] **Step 6: `task check` → コミット**

```bash
task check
git add crates/mat-native/src/commission.rs crates/mat-controller/src/commissioning.rs README.md
git commit -m "fix(commission): kind_of で timeout/デバイス拒否を commission_failed から分離"
```

---

### Task 2: group 送信 `Crypto` → `Other` 分離

**Files:**
- Modify: `crates/mat-native/src/group.rs:92-112` + 同ファイル `mod tests`

**Interfaces:**
- Consumes: `mat_controller::group::GroupSendError`（`Crypto(CryptoError)` / `Io(std::io::Error)`、pub）、`mat_controller::crypto::CryptoError`（pub、`PayloadTooLarge` 等）。
- Produces: `fn group_send_error(group_id: u16, e: GroupSendError) -> MatError`（このファイル内専用）。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-native/src/group.rs` の `mod tests` に追加:

```rust
    /// v1 品質修正 2: `Io`(socket 送出失敗) は `Unreachable` のままだが、
    /// `Crypto`(AES-CCM 暗号化失敗 = ネットワーク不達ではない) は `Other` へ。
    #[test]
    fn group_send_error_maps_io_to_unreachable_and_crypto_to_other() {
        use mat_controller::group::GroupSendError;
        let io = group_send_error(
            10,
            GroupSendError::Io(std::io::Error::other("send failed")),
        );
        assert_eq!(io.kind, mat_core::error::ErrorKind::Unreachable);
        let crypto = group_send_error(
            10,
            GroupSendError::Crypto(mat_controller::crypto::CryptoError::PayloadTooLarge),
        );
        assert_eq!(crypto.kind, mat_core::error::ErrorKind::Other);
        assert!(crypto.detail.contains("group 10"), "detail: {}", crypto.detail);
    }
```

- [ ] **Step 2: テストが落ちる（コンパイルエラー）ことを確認**

Run: `cargo test -p mat-native group_send_error_maps`
Expected: FAIL（`group_send_error` 未定義）

- [ ] **Step 3: 写像関数を抽出して実装**

`crates/mat-native/src/group.rs:102-111` の `Err(e) => Err(MatError::new(ErrorKind::Unreachable, ...))` を差し替え、関数を追加:

```rust
        Err(e) => Err(group_send_error(group_id, e)),
    }
}

/// `GroupSendError` → mat の `ErrorKind`。`Io`（socket 送出失敗 = ワイヤに乗らな
/// かった）は `Unreachable`。`Crypto`（AES-CCM 暗号化失敗 — 実用上 caller bug か
/// payload サイズ超過のみで、ネットワーク不達ではない）は `Other` へ分離
/// （v1 品質修正 2 — 旧実装は両者を Unreachable に一括写像していた）。
fn group_send_error(group_id: u16, e: mat_controller::group::GroupSendError) -> MatError {
    use mat_controller::group::GroupSendError;
    let kind = match &e {
        GroupSendError::Io(_) => ErrorKind::Unreachable,
        GroupSendError::Crypto(_) => ErrorKind::Other,
    };
    MatError::new(kind, format!("groupcast send to group {group_id}: {e}"))
}
```

（元の 102-106 行のコメント（「pragmatic な catch-all」）は削除 — 新コメントが置き換える。）

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-native group_send_error_maps`
Expected: PASS

- [ ] **Step 5: `task check` → コミット**

```bash
task check
git add crates/mat-native/src/group.rs
git commit -m "fix(group): 送信エラーの Crypto を Unreachable から Other へ分離"
```

---

### Task 3: matd 経路の途中失敗を typed error 化

**Files:**
- Modify: `crates/mat/src/matd_client.rs`（`exchange_on_stream` :384-400 / `dispatch` :98-123 / `dispatch_auto` :147-153 / `emit_response` :404-416）+ `mod tests`（:575〜）
- Modify: `crates/mat-core/src/error.rs:47-49`（`MatdUnavailable` doc）
- Modify: `README.md:1012-1017`（`matd_unavailable` 行）

**Interfaces:**
- Consumes: `mat_core::error::MatError`（`new` / `parse_error` / `emit` / pub フィールド `kind`・`detail`）。
- Produces: `exchange_on_stream(UnixStream, &Value) -> Result<Value, MatError>`（従来 `Result<Value, String>`）。呼び出し側は `e.emit()` + `ExitCode::from(e.kind.exit_code())`。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat/src/matd_client.rs` の `mod tests` に追加（`tempfile` は既にテストで使用中）:

```rust
    /// v1 品質修正 3: matd 経路の途中失敗が一律 `other` だったのを分離。
    /// 応答なし切断（EOF）= matd 側が死んだ → `matd_unavailable`(exit 13)。
    #[test]
    fn exchange_on_stream_maps_eof_to_matd_unavailable() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("matd.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            drop(conn); // 1 行も返さず切断 → クライアント側は EOF
        });
        let stream = UnixStream::connect(&path).unwrap();
        let err = exchange_on_stream(stream, &json!({ "op": "on" })).unwrap_err();
        assert_eq!(err.kind, ErrorKind::MatdUnavailable);
        assert!(
            err.detail.contains("may have been executed"),
            "detail should warn about possible partial execution: {}",
            err.detail
        );
        server.join().unwrap();
    }

    /// 応答は来たが JSON でない → `parse_error`（native 経路の出力不能時と同じ分類）。
    #[test]
    fn exchange_on_stream_maps_non_json_response_to_parse_error() {
        use std::io::{BufRead as _, BufReader, Write as _};
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("matd.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let server = std::thread::spawn(move || {
            let (conn, _) = listener.accept().unwrap();
            let mut reader = BufReader::new(conn.try_clone().unwrap());
            let mut req = String::new();
            reader.read_line(&mut req).unwrap(); // リクエスト 1 行を消費
            let mut conn = conn;
            conn.write_all(b"garbage\n").unwrap();
        });
        let stream = UnixStream::connect(&path).unwrap();
        let err = exchange_on_stream(stream, &json!({ "op": "on" })).unwrap_err();
        assert_eq!(err.kind, ErrorKind::ParseError);
        server.join().unwrap();
    }
```

（tests mod で `MatError` / `ErrorKind` が未 use なら `use mat_core::error::{ErrorKind, MatError};` を足す。）

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat exchange_on_stream_maps`
Expected: FAIL（コンパイルエラー: 戻り値が `Result<Value, String>` のため `.kind` が無い）

- [ ] **Step 3: `exchange_on_stream` を `MatError` 化**

`crates/mat/src/matd_client.rs:384-400` を置換:

```rust
/// 接続済み stream で 1 行送り 1 行受け取る（自動検出は probe した接続を使い回す）。
///
/// v1 品質修正 3: 途中失敗を typed error 化。送受信の I/O 断・応答なし切断は
/// 「matd がいなくなった」= `matd_unavailable`（送信後はリクエストが実行済みの
/// 可能性があるので detail で明示）。応答が JSON でないのは `parse_error`。
fn exchange_on_stream(mut stream: UnixStream, op: &Value) -> Result<Value, MatError> {
    let mut line = serde_json::to_vec(op)
        .map_err(|e| MatError::new(ErrorKind::Other, format!("failed to encode request: {e}")))?;
    line.push(b'\n');
    stream.write_all(&line).map_err(|e| {
        MatError::new(
            ErrorKind::MatdUnavailable,
            format!("failed to send request to matd: {e}"),
        )
    })?;

    let mut reader = BufReader::new(stream);
    let mut resp = String::new();
    let n = reader.read_line(&mut resp).map_err(|e| {
        MatError::new(
            ErrorKind::MatdUnavailable,
            format!("failed to read response from matd: {e}; the request may have been executed"),
        )
    })?;
    if n == 0 {
        return Err(MatError::new(
            ErrorKind::MatdUnavailable,
            "matd closed the connection without responding; the request may have been executed",
        ));
    }
    serde_json::from_str(&resp)
        .map_err(|e| MatError::parse_error(format!("matd response was not JSON: {e}; body={resp}")))
}
```

`use mat_core::error::ErrorKind;`（:28）を `use mat_core::error::{ErrorKind, MatError};` に変更。

- [ ] **Step 4: 呼び出し側を追随させる**

`dispatch`（:116-122）:

```rust
    match exchange_on_stream(stream, &op) {
        Ok(resp) => emit_response(resp),
        Err(e) => {
            e.emit();
            ExitCode::from(e.kind.exit_code())
        }
    }
```

`dispatch`（:107-113）の connect 失敗も `matd_unavailable` へ（強制 matd で matd 不在はまさにこの kind）:

```rust
    let (stream, socket) = match connect_candidates(sockets) {
        Ok(s) => s,
        Err(detail) => {
            emit_error(ErrorKind::MatdUnavailable, &detail);
            return ExitCode::from(ErrorKind::MatdUnavailable.exit_code());
        }
    };
```

`dispatch_auto`（:147-153）:

```rust
    Some(match exchange_on_stream(stream, &op) {
        Ok(resp) => emit_response(resp),
        Err(e) => {
            e.emit();
            ExitCode::from(e.kind.exit_code())
        }
    })
```

他に `exchange_on_stream` の呼び出しがあればコンパイルエラーで見つかる — 同じ `e.emit()` パターンにする。

- [ ] **Step 5: 未知 kind の暗黙 Other 化に観測性を足す**

`emit_response`（:404-416）の kind 逆引きを置換（応答 JSON 自体は従来どおり stderr に素通しなので元文字列は失われないが、exit code が黙って 1 に落ちる点を warn で可視化）:

```rust
        let kind = match err
            .get("kind")
            .and_then(|k| serde_json::from_value::<ErrorKind>(k.clone()).ok())
        {
            Some(k) => k,
            None => {
                tracing::warn!(
                    kind = %err.get("kind").cloned().unwrap_or(Value::Null),
                    "unknown error kind from matd; mapping to `other` for the exit code"
                );
                ErrorKind::Other
            }
        };
        ExitCode::from(kind.exit_code())
```

- [ ] **Step 6: doc を追随させる**

`crates/mat-core/src/error.rs:47-49` の `MatdUnavailable` doc コメントを置換:

```rust
    /// matd が利用できない。`mat listen`（常駐リスナ必須・バインド失敗含む）に
    /// 加え、1.0.0 から matd 経路の途中失敗（強制 matd の接続失敗・送受信の
    /// I/O 断・応答なし切断）にも使う。
    /// exit code 12 は歴史的欠番（chip-tool 撤去）のため、13 を割当。
    MatdUnavailable,
```

`README.md:1012-1017` の `matd_unavailable` 行を置換:

```
- `matd_unavailable` (exit 13) — `matd` was not reachable or died mid-request.
  For `mat listen`: no socket, connection refused, `MAT_MATD=0`, or the
  connection was cut partway through the event stream (`mat listen` has no
  direct-path fallback). Since 1.0.0 also for every other op on the matd path:
  forced `--matd` failing to connect, or an I/O failure / silent disconnect
  after the request line was sent (the request may or may not have been
  executed — the detail says so; there is deliberately no direct-path retry, to
  avoid double execution of writes). Distinct from `timeout` (exit 3), which
  `mat listen` uses only for "connected fine, zero events arrived before
  `--timeout-ms`."
```

- [ ] **Step 7: テストが通ることを確認 → `task check` → コミット**

```bash
cargo test -p mat exchange_on_stream_maps
task check
git add crates/mat/src/matd_client.rs crates/mat-core/src/error.rs README.md
git commit -m "fix(matd-client): 途中失敗を matd_unavailable/parse_error に分類、未知 kind を warn"
```

---

### Task 4: `SessionError::Message` → `parse_error`

**Files:**
- Modify: `crates/mat-native/src/lib.rs:663-676`（`map_session_err`）+ `mod tests`（:696〜）

**Interfaces:**
- Consumes: `mat_controller::message::MessageError`（pub、`Truncated` 等）。
- Produces: `map_session_err` の新写像。シグネチャ不変。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-native/src/lib.rs` の `mod tests` に追加:

```rust
    /// v1 品質修正 4: ピアの壊れた応答（Message 層のパース失敗）は「応答は来た
    /// が解釈不能」= `parse_error`。旧実装は catch-all で `other` に落ちていた。
    #[test]
    fn map_session_err_maps_malformed_message_to_parse_error() {
        let e = map_session_err(mat_controller::session::SessionError::Message(
            mat_controller::message::MessageError::Truncated,
        ));
        assert_eq!(e.kind, ErrorKind::ParseError);
    }
```

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat-native map_session_err_maps`
Expected: FAIL（kind が `Other`）

- [ ] **Step 3: 写像を追加**

`crates/mat-native/src/lib.rs` の `map_session_err` に、`SessionError::Io(_)` の行の後・catch-all の前に追加:

```rust
        // ピアの応答がメッセージ層で壊れている → 応答は来た（不達ではない）が
        // 解釈不能 = parse_error（v1 品質修正 4）。
        SessionError::Message(_) => MatError::new(ErrorKind::ParseError, format!("native: {e}")),
```

- [ ] **Step 4: テストが通ることを確認 → `task check` → コミット**

```bash
cargo test -p mat-native map_session_err_maps
task check
git add crates/mat-native/src/lib.rs
git commit -m "fix(native): SessionError::Message を other から parse_error へ"
```

---

### Task 5: alias `id()` の `unreachable!` を typed error 化

**Files:**
- Modify: `crates/mat-core/src/alias.rs:35-64`（`impl_ref` マクロ）+ 同ファイルの既存テスト
- Modify: `crates/mat/src/native_direct.rs`（`classify` :172〜 / `classify_strict` / `run` :557-568）
- Modify: `crates/mat/src/matd_client.rs`（`to_op` :157〜、`.id()` 14 箇所）
- Modify: `crates/mat/src/main.rs:173-179`（diag node アーム）
- Modify: `crates/mat-core/src/error.rs`（`impl From<MatError> for String` 追加)

**Interfaces:**
- Produces: `NodeRef::id() -> Result<u64, MatError>`、`GroupRef::id() -> Result<u16, MatError>`、`EndpointRef::id() -> Result<u16, MatError>`（従来はパニックする `-> $num`）。
- Produces: `classify(command) -> Option<Result<NativeOp, MatError>>`（従来 `Option<NativeOp>`）。`classify_strict` は従来どおり `Option<Result<NativeOp, MatError>>`。
- Produces: `impl From<MatError> for String`（`detail` を返す — `to_op` の `Result<_, String>` 内で `?` を通すため）。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-core/src/alias.rs` のテスト mod に追加:

```rust
    /// v1 品質修正 5: 未解決 alias が実行層まで届いた場合（resolve_command の
    /// 考慮漏れ = 内部バグ）でも panic せず typed error — stdout/stderr の
    /// JSON 契約を守る。
    #[test]
    fn unresolved_alias_id_is_typed_error_not_panic() {
        let r: NodeRef = "living".parse().unwrap();
        let err = r.id().unwrap_err();
        assert_eq!(err.kind, ErrorKind::Other);
        assert!(err.detail.contains("living"), "detail: {}", err.detail);
        assert!(err.detail.contains("resolve_command"), "detail: {}", err.detail);
    }
```

- [ ] **Step 2: テストが落ちる（コンパイルエラー）ことを確認**

Run: `cargo test -p mat-core unresolved_alias_id`
Expected: FAIL（`id()` は `u64` を返すので `.unwrap_err()` が無い）

- [ ] **Step 3: マクロを Result 化**

`crates/mat-core/src/alias.rs:46-59`（`impl $ty` ブロック）を置換:

```rust
        impl $ty {
            /// 解決済み（`Id`）前提で数値を返す。resolve 層通過後にのみ呼ぶ。
            /// 未解決 alias が届いたら resolve_command の考慮漏れ（内部バグ）
            /// だが、panic で JSON 契約を破らず typed error として返す。
            pub fn id(&self) -> Result<$num, MatError> {
                match self {
                    $ty::Id(n) => Ok(*n),
                    $ty::Alias(a) => Err(MatError::new(
                        ErrorKind::Other,
                        format!(
                            "internal: unresolved {} alias '{a}' reached execution \
                             — resolve_command must run first",
                            $what
                        ),
                    )),
                }
            }
        }
```

（`use crate::error::{ErrorKind, MatError};` はファイル後半 :71 に既にある — Rust の `use` はモジュール内で順序不問なので移動不要。）

`crates/mat-core/src/error.rs` の `MatError` impl 群の近くに追加:

```rust
/// `Result<_, String>` の関数内で `MatError` を `?` で流すための縮退変換
/// （matd_client::to_op が使う）。kind は落ち detail のみ残る — 内部バグ経路
/// 専用で、通常経路では発生しない。
impl From<MatError> for String {
    fn from(e: MatError) -> Self {
        e.detail
    }
}
```

- [ ] **Step 4: `native_direct::classify` / `classify_strict` を transpose 型に**

`crates/mat/src/native_direct.rs` — `classify` を外側ラッパー + inner に分離:

```rust
pub(crate) fn classify(command: &Command) -> Option<Result<NativeOp, MatError>> {
    classify_inner(command).transpose()
}

/// `Ok(None)` = native 高速路の対象外（classify_strict へ）。`Err` = 未解決
/// alias が届いた内部バグ（alias.rs `id()` 参照）。
fn classify_inner(command: &Command) -> Result<Option<NativeOp>, MatError> {
    Ok(Some(match command {
        Command::On { node_id, endpoint } => NativeOp::On {
            node_id: node_id.id()?,
            endpoint: endpoint.id()?,
        },
        // …（以下、既存の全アームを同型変換: `Some(NativeOp::X { node_id: node_id.id(), … })`
        //    → `NativeOp::X { node_id: node_id.id()?, … }`、既存の `None` アーム
        //    （catch-all `_ => None` 含む）→ `return Ok(None)`）…
        _ => return Ok(None),
    }))
}
```

機械的変換ルール（inner 内、全アーム共通）:
- `Some(NativeOp::…{ node_id: node_id.id(), … })` → `NativeOp::…{ node_id: node_id.id()?, … }`（`Some(…)` 外し、`.id()` に `?`）。
- `None` を返していた箇所 → `return Ok(None)`。
- Color アームの `resolve_spec(…).ok()?` → `let Ok(c) = mat_core::color::resolve_spec(…) else { return Ok(None); };`（不正 spec は従来どおり `unresolved_op_error` へ委ねる — コメント :228-229 は維持）。
- `let gid = group_id.id();` 形 → `let gid = group_id.id()?;`。

`classify_strict` も同じパターンで `classify_strict_inner(command) -> Result<Option<NativeOp>, MatError>` + `classify_strict(command) -> Option<Result<NativeOp, MatError>> { classify_strict_inner(command).transpose() }` に分離。既存アームの変換:
- `Some(Ok(NativeOp::…))` → `NativeOp::…`（`.id()` に `?`）。
- `Some(Err(MatError::parse_error(msg)))`（Reject）→ `return Err(MatError::parse_error(msg))`。
- `None`（NotNative / catch-all）→ `return Ok(None)`。

`run()`（:557-568）の消費側:

```rust
    let op = match classify(command) {
        Some(Ok(op)) => op,
        // 未解決 alias が届いた内部バグ — typed error で JSON 契約を守る。
        Some(Err(e)) => return Some(Err(e)),
        None => match classify_strict(command) {
            Some(Ok(op)) => op,
            // 値が符号化不能（非スカラー型等）: 即 parse_error
            // （spec 決定3。chip-tool 側では通る形をあえて拒む opt-in の縮小）。
            Some(Err(e)) => return Some(Err(e)),
            // 名前未解決（名前→ID 表外）。chip-tool 撤去でフォールバック先が無い
            // ため、黙って落とさず parse_error にする（数値 ID は受理される）。
            None => return Some(Err(unresolved_op_error(command))),
        },
    };
```

- [ ] **Step 5: `matd_client::to_op` / `main.rs` を追随させる**

`to_op` は `Result<Value, String>` のまま、`From<MatError> for String` により全 `.id()` 呼び出しへ `?` を付けるだけ: `node_id.id()` → `node_id.id()?`（json! マクロ内でも式位置なので `?` はそのまま書ける。14 箇所、:165-330）。挙動: 未解決 alias（内部バグ）時、強制 matd は exit 2 + detail、自動検出は直経路へ落ちて `classify` の typed error（`other`, exit 1）になる — どちらも panic しない。

`main.rs:173-179` の diag node アーム:

```rust
        } => node_id.id().and_then(|node| {
            endpoint.id().and_then(|ep| {
                commands::diag::node(&store_path, node, ep, *deep, native_cfg.as_ref())
            })
        }),
```

- [ ] **Step 6: コンパイルを全 crate で通す**

Run: `cargo check --workspace --all-targets 2>&1 | head -50`

残る `.id()` 呼び出し（native_direct の execute 内・テストコード等）がエラーになるので、本体コードは `?`、テストコードは `.unwrap()` を付ける。エラーが尽きるまで繰り返す。

- [ ] **Step 7: テストが通ることを確認 → `task check` → コミット**

```bash
cargo test -p mat-core unresolved_alias_id
task check
git add crates/mat-core/src/alias.rs crates/mat-core/src/error.rs crates/mat/src/native_direct.rs crates/mat/src/matd_client.rs crates/mat/src/main.rs
git commit -m "fix(alias): 未解決 alias の unreachable! を typed error 化 — id() を Result に"
```

（他にコンパイル追随で触ったファイルがあれば git add に足す。）

---

### Task 6: matd `native_op` Op::Read の expect ×2 撤去

**Files:**
- Modify: `crates/matd/src/server.rs:675-678`

**Interfaces:**
- Consumes: `mat_core::ids::resolve_cluster(&str) -> Option<u32>`、`resolve_attribute(u32, &str) -> Option<AttrRef>`。
- Produces: なし（関数は既に `Result<Value, MatError>` を返す — `?` で流すだけ）。

- [ ] **Step 1: expect を typed error に置換**

`crates/matd/src/server.rs:675-678` を置換:

```rust
                // is_native_hotpath が解決済みのはずだが、不変条件が破れても
                // panic せず typed error（v1 品質修正 6 — alias.rs id() と同じ規律）。
                let cluster_id = mat_core::ids::resolve_cluster(cluster).ok_or_else(|| {
                    MatError::parse_error(format!(
                        "internal: unknown cluster name '{cluster}' (is_native_hotpath invariant violated)"
                    ))
                })?;
                let attr =
                    mat_core::ids::resolve_attribute(cluster_id, attribute).ok_or_else(|| {
                        MatError::parse_error(format!(
                            "internal: unknown attribute name '{attribute}' for cluster '{cluster}' (is_native_hotpath invariant violated)"
                        ))
                    })?;
```

（外部から到達不能な防御経路のため新規テストは不要 — 既存の matd 統合テストが回帰を担保。）

- [ ] **Step 2: `task check` → コミット**

```bash
task check
git add crates/matd/src/server.rs
git commit -m "fix(matd): Op::Read の resolve expect ×2 を typed parse_error 化"
```

---

### Task 7: `resolve_operational` のマルチキャスト in-process テスト追加

**Files:**
- Modify: `crates/mat-controller/src/dnssd.rs`（tests mod — `browse_receives_multicast_only_announcement` :1646 の直後に追加）

**Interfaces:**
- Consumes: 既存テストヘルパ `synth_response`（:1447）/ `multicast_ifaces`（:1545）/ `spawn_multicast_announcer`（:1587）、本体 `resolve_operational`（:567）と `operational_instance`（:100）。

- [ ] **Step 1: テストを書く**

```rust
    /// resolve_operational（CASE 前の targeted resolve）も、マルチキャストで
    /// しか応答しない responder（実機 OTBR proxy と同型）の広告を受信できる
    /// こと。browse / resolve_commissionable と同じ回帰（QU bit 層1、sibling
    /// 関数の適用漏れ = 0.23.1 の教訓）のピン留め — これで 3 兄弟が対称になる。
    #[tokio::test]
    async fn resolve_operational_receives_multicast_only_response() {
        let cfid: [u8; 8] = 0xAB7D_E088_02E0_CD54u64.to_be_bytes();
        let node_id: u64 = 5;
        let service = format!(
            "{}._matter._tcp.local",
            operational_instance(&cfid, node_id)
        );
        let msg = synth_response(
            &service,
            "mcastonly-op.local",
            5540,
            &["SII=5000"],
            "fd00::5".parse().unwrap(),
        );
        let mut tried = Vec::new();
        for (name, idx) in multicast_ifaces() {
            let Ok(announcer) = spawn_multicast_announcer(idx, msg.clone()) else {
                tried.push(format!("{name}(idx={idx}): responder bind failed"));
                continue;
            };
            let res =
                resolve_operational(idx, &cfid, node_id, Duration::from_millis(1500)).await;
            announcer.abort();
            match res {
                Ok(node) => {
                    assert_eq!(node.port, 5540);
                    assert_eq!(
                        node.addresses,
                        vec!["fd00::5".parse::<Ipv6Addr>().unwrap()]
                    );
                    return; // 最初に届いた iface で十分 — PASS。
                }
                Err(e) => tried.push(format!("{name}(idx={idx}): {e:?}")),
            }
        }
        panic!(
            "no multicast-capable interface delivered the multicast-only \
             operational answer to resolve_operational (lo excluded — it lacks \
             IFF_MULTICAST on Linux); tried: {tried:?}"
        );
    }
```

- [ ] **Step 2: テストが通ることを確認**

Run: `cargo test -p mat-controller resolve_operational_receives`
Expected: PASS（現行実装は 0.23.0 で修正済み — これは回帰ピン留め。落ちたら実装のバグなので調査、テストを曲げない）

- [ ] **Step 3: `task check` → コミット**

```bash
task check
git add crates/mat-controller/src/dnssd.rs
git commit -m "test(dnssd): resolve_operational のマルチキャスト限定応答テストを browse と対称化"
```

---

### Task 8: `body::color_success` / `group_color_sent` の rgb=Some 形状テスト

**Files:**
- Modify: `crates/mat-core/src/body.rs`（tests mod :252〜 — 既存 `level_success_shape` :315 と同スタイル）

**Interfaces:**
- Consumes: `ResolvedColor`（`crates/mat-core/src/color.rs:71` — `hue_raw`/`sat_raw`/`hue`/`sat`/`name`/`rgb`）、`GROUPCAST_NOTE`（body.rs 内 const）。

- [ ] **Step 1: テストを書く**

```rust
    /// name / rgb 指定時のみキーが現れる分岐の Some 側（rgb=Some）。None 側は
    /// キー不在が既存テストで担保済み — Some 側の形状はここで初めてピン留め。
    #[test]
    fn color_success_includes_name_and_rgb_when_present() {
        let color = ResolvedColor {
            hue_raw: 10,
            sat_raw: 254,
            hue: 14,
            sat: 100,
            name: Some("red".to_string()),
            rgb: Some("#ff0000".to_string()),
        };
        assert_eq!(
            color_success(5, 1, &color, 0),
            json!({
                "node_id": 5, "endpoint": 1, "cluster": "colorcontrol",
                "command": "move-to-hue-and-saturation",
                "hue": 14, "saturation": 100,
                "hue_raw": 10, "saturation_raw": 254,
                "transition": 0, "status": "success",
                "name": "red", "rgb": "#ff0000",
            })
        );
    }

    #[test]
    fn group_color_sent_includes_name_and_rgb_when_present() {
        let color = ResolvedColor {
            hue_raw: 10,
            sat_raw: 254,
            hue: 14,
            sat: 100,
            name: Some("red".to_string()),
            rgb: Some("#ff0000".to_string()),
        };
        assert_eq!(
            group_color_sent(10, &color, 0, 1),
            json!({
                "group_id": 10, "cluster": "colorcontrol",
                "command": "move-to-hue-and-saturation",
                "hue": 14, "saturation": 100,
                "hue_raw": 10, "saturation_raw": 254,
                "transition": 0, "endpoint": 1,
                "status": "sent", "note": GROUPCAST_NOTE,
            })
        );
    }
```

（tests mod に `ResolvedColor` が未 use なら `use crate::color::ResolvedColor;` を足す。）

- [ ] **Step 2: テストが通ることを確認 → `task check` → コミット**

```bash
cargo test -p mat-core includes_name_and_rgb
task check
git add crates/mat-core/src/body.rs
git commit -m "test(body): color_success/group_color_sent の rgb=Some 形状をピン留め"
```

---

### Task 9（任意 — スキップ可）: `level` 系の隣接同型引数を struct 化

roadmap 上「(任意)」の項目。swap 耐性のためのリファクタで JSON 出力は不変。時間・リスク判断でスキップしてよい。

**Files:**
- Modify: `crates/mat-core/src/body.rs:89-106`（`level_success`）/ :178-196（`group_level_sent`）+ tests
- Modify: 呼び出し側（コンパイルエラー駆動: `crates/matd/src/server.rs:651-657` ほか `mat` 側）

**Interfaces:**
- Produces: `pub struct LevelEcho { pub percent: u8, pub level: u8 }`、`level_success(node_id: u64, endpoint: u16, echo: LevelEcho, transition: u16) -> Value`、`group_level_sent(group_id: u16, echo: LevelEcho, transition: u16, endpoint: u16) -> Value`。

- [ ] **Step 1: struct を導入してシグネチャを変更**

`crates/mat-core/src/body.rs` に追加し、2 関数を変更:

```rust
/// `percent`（入力）と `level`（換算後 0–254）のエコーペア。隣接する同型 `u8`
/// 引数の取り違え（swap）をコンパイル時に防ぐ（v1 品質修正 8 — 任意項目）。
#[derive(Debug, Clone, Copy)]
pub struct LevelEcho {
    pub percent: u8,
    pub level: u8,
}

/// `level` の成功 body。入力 percent と換算後 level を両方エコー。
pub fn level_success(node_id: u64, endpoint: u16, echo: LevelEcho, transition: u16) -> Value {
    json!({
        "node_id": node_id,
        "endpoint": endpoint,
        "cluster": "levelcontrol",
        "command": "move-to-level",
        "percent": echo.percent,
        "level": echo.level,
        "transition": transition,
        "status": "success",
    })
}

/// `group level` の sent body。
pub fn group_level_sent(group_id: u16, echo: LevelEcho, transition: u16, endpoint: u16) -> Value {
    json!({
        "group_id": group_id,
        "cluster": "levelcontrol",
        "command": "move-to-level",
        "percent": echo.percent,
        "level": echo.level,
        "transition": transition,
        "endpoint": endpoint,
        "status": "sent",
        "note": GROUPCAST_NOTE,
    })
}
```

- [ ] **Step 2: 呼び出し側をコンパイルエラー駆動で追随**

Run: `cargo check --workspace --all-targets 2>&1 | head -30`

変換は機械的: `level_success(n, e, *percent, *level, *transition)` → `level_success(n, e, LevelEcho { percent: *percent, level: *level }, *transition)`（`group_level_sent` も同様、`use mat_core::body::LevelEcho;` を追加）。例: `crates/matd/src/server.rs:651-657` は

```rust
            Ok(mat_core::body::level_success(
                *node_id,
                *endpoint,
                mat_core::body::LevelEcho {
                    percent: *percent,
                    level: *level,
                },
                *transition,
            ))
```

既存テスト `level_success_shape` 等も同じ変換で追随（期待 JSON は不変 — 変わったらバグ）。

- [ ] **Step 3: `task check` → コミット**

```bash
task check
git add -A crates/
git commit -m "refactor(body): level 系の percent/level を LevelEcho struct 化（swap 耐性）"
```

---

### Task 10: 1.0.0 bump（実施前にユーザー確認）

**このタスクはコード変更が全部終わり、ユーザーが v1 リリースを明示承認してから実行する。**

**Files:**
- Modify: `Cargo.toml:6`（workspace version）
- Modify: `Cargo.lock`（`cargo check` で自動追随）

- [ ] **Step 1: バージョン更新**

`Cargo.toml` の `version = "0.28.1"` → `version = "1.0.0"`。

Run: `cargo check --workspace`（Cargo.lock を更新させる）

- [ ] **Step 2: 最終検証**

Run: `task check`
Expected: fmt / clippy / 全テスト PASS

Run: `target/debug/mat --version 2>/dev/null || cargo run -p mat -- --version`
Expected: `1.0.0` を含む

- [ ] **Step 3: リリースコミット**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore(release): 1.0.0 — v1 品質修正完了（エラー分類分離・防御的 panic の typed error 化）"
```

（jarvis への 1.0.0 デプロイは本計画のスコープ外 — 完了後に despliegue skill で別途。）
