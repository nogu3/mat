# 監査⑨ IMデコード失敗の誤分類修正+購読耐性 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** IM ペイロードのデコード失敗を `parse_error` に正しく分類し、購読 2 経路（priming / live pump）がデコード失敗で死なないようにする（恒久盲目シナリオの根絶）。

**Architecture:** spec = `docs/superpowers/specs/2026-08-04-im-decode-parse-error-design.md`（案A: session 層 salvage）。デコーダ（`im.rs`）は strict のまま、(1) `mat-native` の `map_session_err` で `ImError` variant を分割、(2) `session.rs` の購読 2 経路でデコード失敗を「warn + StatusResponse(0) + 空 `ReportDataMessage` 差し替え」で握って続行する。

**Tech Stack:** Rust（cargo workspace）。テストは既存の session テスト足場（`reliable_session_pair` / `device_datagram` / `open_from_controller`）をそのまま使う。

## Global Constraints

- ブランチ: `fix/tier2-im-decode-parse-error`（main から分岐）
- バージョン: 1.20.0（workspace `Cargo.toml` の `[workspace.package] version`）
- デコーダ（`crates/mat-controller/src/im.rs`）は変更しない
- one-shot read/invoke 経路（`session.rs` の read ループ等）は挙動変更しない（分類のみ変わる）
- JSON スキーマ・`kind` の語彙は不変
- コミット前に対象クレートのテスト、最終タスクで `task check`（fmt:check + clippy + test）
- 実機 E2E（隔離 matd 無回帰スモーク）は計画実行後・マージ前に別途実施（このタスク群には含めない）

---

### Task 1: 誤分類修正 — `map_session_err` の `ImError` variant 分割

**Files:**
- Modify: `crates/mat-native/src/lib.rs`（`map_session_err`、lib.rs:686 付近）
- Test: 同ファイルの `#[cfg(test)] mod tests`（`map_session_err_maps_malformed_message_to_parse_error` の直後、lib.rs:780 付近）

**Interfaces:**
- Consumes: `mat_controller::im::ImError`（variants: `Tlv` / `Malformed` / `UnsupportedValue` / `AttributeStatus` / `StatusResponse` / `CommandStatus`）、`mat_controller::session::SessionError::Im`
- Produces: `map_session_err` が `Im(Tlv|Malformed|UnsupportedValue)` → `ErrorKind::ParseError`、`Im(StatusResponse|AttributeStatus|CommandStatus)` → `ErrorKind::DeviceRejected` を返す（Task 2/3 は依存しない — 独立）

- [ ] **Step 1: ブランチ作成**

```bash
git switch -c fix/tier2-im-decode-parse-error main
```

（worktree 実行で既にこのブランチ上にいる場合はスキップ）

- [ ] **Step 2: 失敗するテストを書く**

`crates/mat-native/src/lib.rs` の tests mod、`map_session_err_maps_malformed_message_to_parse_error`（lib.rs:773 付近）の直後に追加:

```rust
#[test]
fn map_session_err_splits_im_decode_failure_from_device_rejection() {
    // 監査⑨: デコード失敗（Tlv/Malformed/UnsupportedValue）は「応答は来たが
    // 解釈不能」= parse_error（Message(_) と同じ規律）。device_rejected は
    // 本当のデバイス拒否（StatusResponse/AttributeStatus/CommandStatus）だけ。
    use mat_controller::im::ImError;
    use mat_controller::session::SessionError;
    let e = map_session_err(SessionError::Im(ImError::Malformed("truncated report data")));
    assert_eq!(e.kind, ErrorKind::ParseError);
    let e = map_session_err(SessionError::Im(ImError::UnsupportedValue));
    assert_eq!(e.kind, ErrorKind::ParseError);
    let e = map_session_err(SessionError::Im(ImError::StatusResponse(0x80)));
    assert_eq!(e.kind, ErrorKind::DeviceRejected);
    let e = map_session_err(SessionError::Im(ImError::AttributeStatus(0x86)));
    assert_eq!(e.kind, ErrorKind::DeviceRejected);
    let e = map_session_err(SessionError::Im(ImError::CommandStatus {
        status: 0x01,
        cluster_status: None,
    }));
    assert_eq!(e.kind, ErrorKind::DeviceRejected);
}
```

- [ ] **Step 3: テストが失敗することを確認**

Run: `cargo test -p mat-native map_session_err_splits_im_decode_failure -- --nocapture`
Expected: FAIL（`Malformed` が `DeviceRejected` に分類される — 現行の一括写像）

- [ ] **Step 4: 実装**

`crates/mat-native/src/lib.rs` の `map_session_err`（lib.rs:686 付近）で、現行の

```rust
        // デバイスがコマンド/読みを IM ステータスで拒否 → コマンドは届いた。
        SessionError::Im(_) => MatError::new(ErrorKind::DeviceRejected, format!("native: {e}")),
```

を以下に置換:

```rust
        // デバイスがコマンド/読みを IM ステータスで拒否 → コマンドは届いた。
        SessionError::Im(
            ImError::StatusResponse(_)
            | ImError::AttributeStatus(_)
            | ImError::CommandStatus { .. },
        ) => MatError::new(ErrorKind::DeviceRejected, format!("native: {e}")),
        // こちらのデコーダが応答を解釈できなかった（監査⑨）→ 応答は来たが
        // 解釈不能 = parse_error（Message(_) と同じ規律）。variant 追加時に
        // ここで分類を決めさせるため wildcard にしない。
        SessionError::Im(ImError::Tlv(_) | ImError::Malformed(_) | ImError::UnsupportedValue) => {
            MatError::new(ErrorKind::ParseError, format!("native: {e}"))
        }
```

関数冒頭の `use mat_controller::session::SessionError;` の隣に `use mat_controller::im::ImError;` を追加。

- [ ] **Step 5: テストが通ることを確認**

Run: `cargo test -p mat-native`
Expected: PASS（新テスト含む全テスト。既存の分類テスト群も無回帰）

- [ ] **Step 6: Commit**

```bash
git add crates/mat-native/src/lib.rs
git commit -m "fix(mat-native): IMデコード失敗を parse_error に分類（監査⑨ 前段）"
```

---

### Task 2: priming 耐性 — `subscribe_wildcard` がデコード失敗チャンクで死なない

**Files:**
- Modify: `crates/mat-controller/src/session.rs`（`subscribe_wildcard` の `OPCODE_REPORT_DATA` 腕 = session.rs:1085 付近、ヘルパ追加は `const MAX_REPORT_CHUNKS`（session.rs:35）の直後）
- Test: 同ファイルの tests mod（`subscribe_wildcard_handshake_with_chunked_priming` = session.rs:2606 付近の後ろ）

**Interfaces:**
- Consumes: 既存テスト足場 `reliable_session_pair()` / `device_datagram(...)` / `open_from_controller(...)` / `subscription_report_payload(sub_id, value, more)` / `subscribe_response_payload(sub_id, max_interval)` / `fast_cfg()`（すべて tests mod 内に定義済み）
- Produces: `fn payload_head_hex(payload: &[u8]) -> String`（session.rs トップレベル private、Task 3 が使う）。`subscribe_wildcard` はデコード失敗チャンクを空 `ReportDataMessage` として priming に含めて成功する

- [ ] **Step 1: 失敗するテスト 2 本を書く**

`crates/mat-controller/src/session.rs` の tests mod、`subscribe_wildcard_sends_cluster_paths_when_narrowed` の直後に追加:

```rust
    /// 監査⑨: 非デコード可能な priming チャンクは購読を殺さない —
    /// warn + StatusResponse(0) + 空 rd 差し替えで続行し、ハンドシェイクは成立する。
    #[tokio::test]
    async fn subscribe_wildcard_survives_undecodable_priming_chunk() {
        let (mut s, dev) = reliable_session_pair();

        let dev_task = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, _) = dev.recv_from(&mut buf).await.unwrap();
            let (_, p, _) = open_from_controller(&buf[..n]);
            assert_eq!(p.opcode, crate::im::OPCODE_SUBSCRIBE_REQUEST);
            let ex = p.exchange_id;
            // garbage チャンク（struct start だけで途中切れ = デコード不能）
            let d = device_datagram(
                ex,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_REPORT_DATA,
                None,
                false,
                9200,
                &[0x15],
            );
            dev.send_to(&d, RELIABLE_PEER).await.unwrap();
            // garbage にも StatusResponse(0) が返る
            let (n, _) = dev.recv_from(&mut buf).await.unwrap();
            let (_, p2, body) = open_from_controller(&buf[..n]);
            assert_eq!(p2.opcode, crate::im::OPCODE_STATUS_RESPONSE);
            assert_eq!(crate::im::decode_status_response(&body).unwrap(), 0);
            // 正常チャンク（more=false）→ StatusResponse(0) → SubscribeResponse
            let d = device_datagram(
                ex,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_REPORT_DATA,
                None,
                false,
                9201,
                &subscription_report_payload(44, true, false),
            );
            dev.send_to(&d, RELIABLE_PEER).await.unwrap();
            let (n, _) = dev.recv_from(&mut buf).await.unwrap();
            let (_, p3, body) = open_from_controller(&buf[..n]);
            assert_eq!(p3.opcode, crate::im::OPCODE_STATUS_RESPONSE);
            assert_eq!(crate::im::decode_status_response(&body).unwrap(), 0);
            let d = device_datagram(
                ex,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_SUBSCRIBE_RESPONSE,
                None,
                false,
                9202,
                &subscribe_response_payload(44, 120),
            );
            dev.send_to(&d, RELIABLE_PEER).await.unwrap();
        });

        let (resp, priming) = s
            .subscribe_wildcard(0, 3600, false, &[], &fast_cfg())
            .await
            .expect("undecodable priming chunk must not kill the subscribe");
        assert_eq!(resp.subscription_id, 44);
        assert_eq!(priming.len(), 2);
        assert!(priming[0].reports.is_empty()); // salvage 差し替えの空 rd
        assert_eq!(priming[1].reports[0].data, Some(serde_json::json!(true)));
        dev_task.await.unwrap();
    }

    /// 監査⑨の flood 防御維持: 非デコード可能チャンクも MAX_REPORT_CHUNKS に
    /// 数えられ、超過で subscribe は Malformed で失敗する（無限チャンク防御が
    /// salvage で消えていないことの釘打ち）。
    #[tokio::test]
    async fn subscribe_wildcard_undecodable_chunks_still_count_toward_chunk_cap() {
        let (mut s, dev) = reliable_session_pair();

        let dev_task = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, _) = dev.recv_from(&mut buf).await.unwrap();
            let (_, p, _) = open_from_controller(&buf[..n]);
            let ex = p.exchange_id;
            // cap(64)+1 = 65 チャンク送る。65 個目は push で cap を超えて
            // subscribe が Err で抜けるため、StatusResponse は 64 回しか返らない。
            for i in 0..=(MAX_REPORT_CHUNKS as u32) {
                let d = device_datagram(
                    ex,
                    crate::im::PROTOCOL_ID_IM,
                    crate::im::OPCODE_REPORT_DATA,
                    None,
                    false,
                    9300 + i,
                    &[0x15],
                );
                dev.send_to(&d, RELIABLE_PEER).await.unwrap();
                if (i as usize) < MAX_REPORT_CHUNKS {
                    let (n, _) = dev.recv_from(&mut buf).await.unwrap();
                    let (_, p2, _) = open_from_controller(&buf[..n]);
                    assert_eq!(p2.opcode, crate::im::OPCODE_STATUS_RESPONSE);
                }
            }
        });

        let err = s
            .subscribe_wildcard(0, 3600, false, &[], &fast_cfg())
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                SessionError::Im(crate::im::ImError::Malformed("too many report chunks"))
            ),
            "err: {err:?}"
        );
        dev_task.await.unwrap();
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-controller subscribe_wildcard_survives_undecodable_priming_chunk subscribe_wildcard_undecodable_chunks_still_count -- --nocapture`
Expected: 1 本目 FAIL（現行は garbage チャンクで `SessionError::Im` が返り subscribe ごと失敗）。2 本目も FAIL（同上 — 1 チャンク目の Err で即抜けし StatusResponse が返らない）

- [ ] **Step 3: 実装**

(a) `crates/mat-controller/src/session.rs` の `const MAX_REPORT_CHUNKS: usize = 64;`（session.rs:35）の直後にヘルパを追加:

```rust
/// デコード失敗 payload の先頭を hex で（未知エンコーディングの事後診断用、
/// debug ログ専用）。
fn payload_head_hex(payload: &[u8]) -> String {
    payload.iter().take(64).map(|b| format!("{b:02x}")).collect()
}
```

(b) `subscribe_wildcard` の `OPCODE_REPORT_DATA` 腕（session.rs:1085 付近）で、現行の

```rust
                im::OPCODE_REPORT_DATA => {
                    let rd =
                        im::decode_report_data_message(&msg.payload).map_err(SessionError::Im)?;
```

を以下に置換:

```rust
                im::OPCODE_REPORT_DATA => {
                    // 監査⑨: デコード失敗でも購読は殺さない。認証済み（MIC 検証
                    // 済み）のチャンクなので ack して先へ進み、失われるのはこの
                    // チャンクの属性値だけ（matd の state cache は次のレポートで
                    // 自己回復）。空 rd を push するのは MAX_REPORT_CHUNKS の
                    // flood 防御を非デコード可能チャンクにも効かせるため。
                    let rd = match im::decode_report_data_message(&msg.payload) {
                        Ok(rd) => rd,
                        Err(e) => {
                            tracing::warn!(
                                exchange_id,
                                payload_len = msg.payload.len(),
                                error = %e,
                                "subscribe: undecodable priming chunk; acking and continuing"
                            );
                            tracing::debug!(
                                payload_head = %payload_head_hex(&msg.payload),
                                "undecodable priming chunk payload"
                            );
                            im::ReportDataMessage {
                                reports: Vec::new(),
                                subscription_id: None,
                                more_chunks: false,
                                suppress_response: false,
                            }
                        }
                    };
```

続く `tracing::debug!(...priming report chunk...)` / `priming.push(rd)` / cap ガード / `StatusResponse(0)` 送信は無変更（空 rd がそのまま流れる）。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-controller`
Expected: PASS（新 2 本 + 既存の subscribe/priming テスト群も無回帰）

- [ ] **Step 5: Commit**

```bash
git add crates/mat-controller/src/session.rs
git commit -m "fix(mat-controller): priming のデコード失敗チャンクで購読を殺さない（監査⑨）"
```

---

### Task 3: live pump 耐性 — `next_subscription_report` がデコード失敗で死なない

**Files:**
- Modify: `crates/mat-controller/src/session.rs`（`next_subscription_report` の decode 行 = session.rs:1186 付近）
- Test: 同ファイルの tests mod（`next_subscription_report_receives_device_initiated_reports_and_keepalive` = session.rs:2738 付近の後ろ）

**Interfaces:**
- Consumes: Task 2 の `payload_head_hex(payload: &[u8]) -> String`、既存テスト足場（`reliable_session_pair()` / `seal_message` / `subscription_report_payload` / `open_from_controller` / `fast_cfg()`）
- Produces: `next_subscription_report` はデコード失敗時に空 `ReportDataMessage`（`reports` 空・`subscription_id: None`・`suppress_response: false`）を `Ok` で返し、既存の `!rd.suppress_response` 分岐が `StatusResponse(0)` を送る

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat-controller/src/session.rs` の tests mod、`next_subscription_report_receives_device_initiated_reports_and_keepalive` の直後に追加:

```rust
    /// 監査⑨: 非デコード可能な live report は購読を殺さない — 空 rd
    /// （keep-alive 相当）として届き、StatusResponse(0) で exchange を閉じ、
    /// 次の正常 report は通常配送される。
    #[tokio::test]
    async fn next_subscription_report_survives_undecodable_report() {
        let (mut s, dev) = reliable_session_pair();

        let dev_task = tokio::spawn(async move {
            let header = MessageHeader {
                session_id: LOCAL_SID,
                security_flags: 0,
                message_counter: 200,
                source_node_id: None,
                destination: Destination::None,
            };
            let proto = ProtocolHeader {
                initiator: true,
                needs_ack: false,
                acked_counter: None,
                opcode: crate::im::OPCODE_REPORT_DATA,
                exchange_id: 0x7779,
                protocol_id: crate::im::PROTOCOL_ID_IM,
                vendor_id: None,
            };
            // garbage report（struct start だけで途中切れ = デコード不能）
            let d = seal_message(&R2I, &header, &proto, &[0x15], DEV_NODE).unwrap();
            dev.send_to(&d, RELIABLE_PEER).await.unwrap();
            // StatusResponse(0) が同 exchange に返る
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, _) = dev.recv_from(&mut buf).await.unwrap();
            let (_, p, body) = open_from_controller(&buf[..n]);
            assert_eq!(p.opcode, crate::im::OPCODE_STATUS_RESPONSE);
            assert_eq!(p.exchange_id, 0x7779);
            assert_eq!(crate::im::decode_status_response(&body).unwrap(), 0);
            // 正常 report（別 exchange）は通常配送される
            let mut h2 = header;
            h2.message_counter = 201;
            let mut p2 = proto;
            p2.exchange_id = 0x777a;
            let d = seal_message(
                &R2I,
                &h2,
                &p2,
                &subscription_report_payload(42, true, false),
                DEV_NODE,
            )
            .unwrap();
            dev.send_to(&d, RELIABLE_PEER).await.unwrap();
            let (n, _) = dev.recv_from(&mut buf).await.unwrap();
            let (_, p3, _) = open_from_controller(&buf[..n]);
            assert_eq!(p3.opcode, crate::im::OPCODE_STATUS_RESPONSE);
            assert_eq!(p3.exchange_id, 0x777a);
        });

        let rd = s
            .next_subscription_report(Duration::from_secs(2), &fast_cfg())
            .await
            .expect("undecodable report must not kill the pump");
        assert!(rd.reports.is_empty());
        assert_eq!(rd.subscription_id, None);
        let rd2 = s
            .next_subscription_report(Duration::from_secs(2), &fast_cfg())
            .await
            .unwrap();
        assert_eq!(rd2.subscription_id, Some(42));
        dev_task.await.unwrap();
    }
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-controller next_subscription_report_survives_undecodable_report -- --nocapture`
Expected: FAIL（現行は garbage report で `SessionError::Im` が返る）

- [ ] **Step 3: 実装**

`next_subscription_report`（session.rs:1186 付近）で、現行の

```rust
        let rd = im::decode_report_data_message(&msg.payload).map_err(SessionError::Im)?;
```

を以下に置換:

```rust
        // 監査⑨: デコード失敗でも購読は殺さない。認証済み（MIC 検証済み）の
        // デバイス発メッセージなので生存の証拠としては正しく、空 rd
        // （= keep-alive 相当）に差し替えて届ける。suppress_response は読めない
        // ので false 扱い → 下の分岐が StatusResponse(0) で exchange を閉じる
        // （1.16.0 ワイヤ実測: 実デバイスの購読レポートは suppress=false +
        // StatusResponse 期待。suppress=true の相手への余計な SR は exchange
        // 終端で無害）。
        let rd = match im::decode_report_data_message(&msg.payload) {
            Ok(rd) => rd,
            Err(e) => {
                tracing::warn!(
                    exchange_id = msg.proto.exchange_id,
                    payload_len = msg.payload.len(),
                    error = %e,
                    "sub pump: undecodable report; delivering as empty"
                );
                tracing::debug!(
                    payload_head = %payload_head_hex(&msg.payload),
                    "undecodable report payload"
                );
                im::ReportDataMessage {
                    reports: Vec::new(),
                    subscription_id: None,
                    more_chunks: false,
                    suppress_response: false,
                }
            }
        };
```

続く `tracing::debug!(...report delivered...)` / `!rd.suppress_response` の `respond_status` + `deferred_sub_err` 持ち越し / `Ok(rd)` は無変更。

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-controller`
Expected: PASS（新テスト + 既存の `respond_status_failure_defers_error_and_still_delivers_report` / `report_chunk_arriving_during_status_ack_wait_is_not_lost` 等の pump テスト群も無回帰）

- [ ] **Step 5: Commit**

```bash
git add crates/mat-controller/src/session.rs
git commit -m "fix(mat-controller): live report のデコード失敗で購読を殺さない（監査⑨ 完結）"
```

---

### Task 4: バージョン 1.20.0 + 全体チェック

**Files:**
- Modify: `Cargo.toml`（workspace ルート、`[workspace.package] version`）
- Modify: `Cargo.lock`（cargo が自動更新）

**Interfaces:**
- Consumes: Task 1〜3 の全変更
- Produces: 1.20.0 のリリース可能なブランチ（`task check` 合格）

- [ ] **Step 1: バージョンを上げる**

ルート `Cargo.toml` の `[workspace.package]`:

```toml
version = "1.20.0"
```

- [ ] **Step 2: Cargo.lock 更新**

Run: `cargo check --workspace`
Expected: 成功（Cargo.lock のメンバー version 行が 1.20.0 に更新される）

- [ ] **Step 3: 全体チェック**

Run: `task check`
Expected: fmt:check + clippy + 全クレートのテストが PASS

- [ ] **Step 4: Commit**

```bash
git add Cargo.toml Cargo.lock
git commit -m "chore: 1.20.0（IMデコード誤分類修正+購読耐性 — 安定性監査 Tier 2 ⑨）"
```

---

## 実行後（このタスク群の外）

- マージ前実機 E2E: 隔離 matd 無回帰スモーク（`*.new` 方式 — 購読確立 attempts=1、`matd status` 健全、warm read exit0、WARN 0）。非デコード可能レポートは実機で誘発不能のため無回帰確認のみ。
- スモーク合格後 main へマージ、メモリ（監査バックログ / jarvis-matd-deploy）更新。
