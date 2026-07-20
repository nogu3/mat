# matd 常駐 Subscribe + mat listen 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** matd が commissioned 全ノードへ常駐 wildcard Subscribe を張り、デバイス発の attribute report を `mat listen`（matd 専用 op、NDJSON ストリーム）へ配信する。

**Architecture:** ① `mat-controller`（im.rs に Subscribe TLV、session.rs に購読ハンドシェイク+ポンプ。購読は専用ソケット+専用 CASE — 既存 op 経路は無改変）② `matd`（SubscriptionManager が per-node 購読タスク、`tokio::sync::broadcast` で listen 接続へ配信、socket 新 op `listen` は「ack 1行 + 以後ストリーム」の唯一の例外）③ `mat`（`mat listen` = matd への薄い口。count/timeout は mat 側制御、新 kind `matd_unavailable` = exit 13）。

**Tech Stack:** Rust / tokio（broadcast, UnixListener）/ 既存 TLV・CASE スタック。新規依存なし。

**Spec:** `docs/superpowers/specs/2026-07-20-matd-subscribe-listen-design.md`

## Global Constraints

- stdout は純粋な構造化 JSON のみ。診断は stderr の `tracing`（CLAUDE.md 設計ルール 2, 3）。
- プロトコルコードは backend crate（mat-controller / mat-native）のみ。mat / matd のコマンド層に TLV を書かない（設計ルール 1）。
- `mat` 一発経路は無変更（listen は matd 専用 op、direct fallback なし）。
- 購読パラメータ: MinIntervalFloor = **0** / MaxIntervalCeiling = **3600**(s) / KeepSubscriptions = **false**。
- 無音死亡判定: **MaxInterval の 1.5 倍**。再購読 backoff: **5s 開始、指数、上限 5min**。リトライは debug ログ、確立/喪失の状態遷移のみ info。
- イベントは **scalar 値のみ**（list/struct は debug ログで捨てる）。`priming` フラグ必須。
- lag した listener には `{"error":{"kind":"other","detail":"event stream lagged"}}` を送って切断。
- 新エラー kind **`matd_unavailable`** = **exit 13**（12 は歴史的欠番のため飛ばす）。
- `mat listen` 既定: count=1 / timeout-ms=60000。`--timeout-ms 0` = 無期限。0件 timeout → exit 3、1件以上 → exit 0。
- v1 スコープ外: EventReport / DataVersionFilter / LIT ICD / subscriptions.toml / リプレイ。
- コミット前に `task check`（fmt:check + clippy -D warnings + test）を通すこと。

## File Structure

- `crates/mat-controller/src/im.rs` — Subscribe TLV encode/decode、`ReportDataMessage` に `subscription_id`、`AttributeReport` に `cluster` を追加（Task 1）
- `crates/mat-controller/src/session.rs` — screen フィルタ一般化、`subscribe_wildcard` / `next_subscription_report` / `respond_status`（Task 2）
- `crates/mat-native/src/lib.rs` — `SubscribeConn` trait、`Establisher::establish_subscription`（default = Err）、`CaseEstablisher` 実装（Task 3）
- `crates/mat-native/src/test_support.rs` — `FakeSubConn` / `FakeEstablisher` 拡張（Task 3）
- `crates/matd/src/subscription.rs` — **新規**: `Event` / `SubscriptionManager`（Task 4）
- `crates/matd/src/protocol.rs` — `Op::Listen`（Task 5）
- `crates/matd/src/server.rs` — listen ストリーム処理 + フィルタ + lag 切断、`serve` に events 引数（Task 5）
- `crates/matd/src/main.rs` + `crates/matd/src/lib.rs` — SubscriptionManager 起動の結線（Task 6）
- `crates/mat-core/src/error.rs` — `ErrorKind::MatdUnavailable` = exit 13（Task 7）
- `crates/mat/src/cli.rs` / `resolve.rs` / `matd_client.rs` / `main.rs` — `mat listen`（Task 8）
- `crates/mat/tests/listen.rs` — **新規**: fake matd ストリーミング統合テスト（Task 9）
- `README.md` / `ARCHITECTURE.md` / `CLAUDE.md` / `Cargo.toml` — docs + 0.25.0（Task 10）

---

### Task 1: im.rs — Subscribe TLV encode/decode

**Files:**
- Modify: `crates/mat-controller/src/im.rs`

**Interfaces:**
- Produces:
  - `pub const OPCODE_SUBSCRIBE_REQUEST: u8 = 0x03;` / `pub const OPCODE_SUBSCRIBE_RESPONSE: u8 = 0x04;`
  - `pub fn encode_subscribe_request_wildcard(min_interval_floor_s: u16, max_interval_ceiling_s: u16, keep_subscriptions: bool) -> Vec<u8>`
  - `pub struct SubscribeResponse { pub subscription_id: u32, pub max_interval_s: u16 }` + `pub fn decode_subscribe_response(payload: &[u8]) -> Result<SubscribeResponse, ImError>`
  - `ReportDataMessage` に `pub subscription_id: Option<u32>` フィールド追加
  - `AttributeReport` に `pub cluster: Option<u32>` フィールド追加（wildcard 購読の report にはパスの cluster が要る）

- [ ] **Step 1: 失敗するテストを書く**

`im.rs` の `#[cfg(test)] mod tests` に追加（既存テストのフィクスチャ構築スタイル = `tlv::Writer` 直書き に合わせる）:

```rust
#[test]
fn subscribe_request_wildcard_shape() {
    // SubscribeRequestMessage (spec §8.10): {0: KeepSubscriptions, 1: MinIntervalFloor,
    // 2: MaxIntervalCeiling, 3: AttributeRequests[[]], 7: IsFabricFiltered, 255: rev}
    let b = encode_subscribe_request_wildcard(0, 3600, false);
    let mut r = crate::tlv::Reader::new(&b);
    assert!(matches!(r.next().unwrap().unwrap().value, Value::StructStart));
    let el = r.next().unwrap().unwrap(); // KeepSubscriptions
    assert_eq!(el.tag, Tag::Context(0));
    assert_eq!(el.value, Value::Bool(false));
    let el = r.next().unwrap().unwrap(); // MinIntervalFloorSeconds
    assert_eq!(el.tag, Tag::Context(1));
    assert_eq!(el.value, Value::Uint(0));
    let el = r.next().unwrap().unwrap(); // MaxIntervalCeilingSeconds
    assert_eq!(el.tag, Tag::Context(2));
    assert_eq!(el.value, Value::Uint(3600));
    let el = r.next().unwrap().unwrap(); // AttributeRequests
    assert_eq!(el.tag, Tag::Context(3));
    assert!(matches!(el.value, Value::ArrayStart));
    // wildcard AttributePathIB = 空 list（endpoint/cluster/attribute 全省略）
    assert!(matches!(r.next().unwrap().unwrap().value, Value::ListStart));
    assert!(matches!(r.next().unwrap().unwrap().value, Value::ContainerEnd)); // path
    assert!(matches!(r.next().unwrap().unwrap().value, Value::ContainerEnd)); // requests
    let el = r.next().unwrap().unwrap(); // IsFabricFiltered
    assert_eq!(el.tag, Tag::Context(7));
    assert_eq!(el.value, Value::Bool(true));
}

#[test]
fn subscribe_response_decodes_id_and_max_interval() {
    // SubscribeResponseMessage: {0: SubscriptionId(u32), 2: MaxInterval(u16), 255: rev}
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), 0xDEAD_BEEF);
    w.put_uint(Tag::Context(2), 120);
    w.put_uint(Tag::Context(255), 12);
    w.end_container();
    let resp = decode_subscribe_response(&w.finish()).unwrap();
    assert_eq!(resp.subscription_id, 0xDEAD_BEEF);
    assert_eq!(resp.max_interval_s, 120);
}

#[test]
fn subscribe_response_without_id_is_malformed() {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(2), 120);
    w.end_container();
    assert!(decode_subscribe_response(&w.finish()).is_err());
}

#[test]
fn report_data_message_carries_subscription_id_and_cluster_path() {
    // 購読 report: {0: SubscriptionId, 1: [AttributeReportIB(onoff on-off=true)], 255: rev}
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), 77); // SubscriptionId
    w.start_array(Tag::Context(1));
    w.start_struct(Tag::Anonymous);
    w.start_struct(Tag::Context(1)); // AttributeDataIB
    w.put_uint(Tag::Context(0), 1); // DataVersion
    w.start_list(Tag::Context(1)); // Path
    w.put_uint(Tag::Context(2), 1); // endpoint
    w.put_uint(Tag::Context(3), 6); // cluster ← 新規に拾う
    w.put_uint(Tag::Context(4), 0); // attribute
    w.end_container();
    w.put_bool(Tag::Context(2), true); // Data
    w.end_container();
    w.end_container();
    w.end_container();
    w.put_uint(Tag::Context(255), 12);
    w.end_container();
    let m = decode_report_data_message(&w.finish()).unwrap();
    assert_eq!(m.subscription_id, Some(77));
    assert_eq!(m.reports.len(), 1);
    assert_eq!(m.reports[0].endpoint, Some(1));
    assert_eq!(m.reports[0].cluster, Some(6));
    assert_eq!(m.reports[0].attribute, Some(0));
    assert_eq!(m.reports[0].data, Some(serde_json::json!(true)));
}

#[test]
fn empty_keepalive_report_decodes_with_no_reports() {
    // keep-alive: SubscriptionId + rev のみ（AttributeReports 無し）
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), 77);
    w.put_uint(Tag::Context(255), 12);
    w.end_container();
    let m = decode_report_data_message(&w.finish()).unwrap();
    assert_eq!(m.subscription_id, Some(77));
    assert!(m.reports.is_empty());
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-controller --lib im::tests::subscribe -- --nocapture` と `cargo test -p mat-controller --lib im::tests::report_data_message_carries`
Expected: コンパイルエラー（`encode_subscribe_request_wildcard` 未定義、`subscription_id` / `cluster` フィールド無し）

- [ ] **Step 3: 実装**

opcode 定数（既存の `OPCODE_READ_REQUEST` 群の並びに追加）:

```rust
pub const OPCODE_SUBSCRIBE_REQUEST: u8 = 0x03;
pub const OPCODE_SUBSCRIBE_RESPONSE: u8 = 0x04;
```

構造体拡張（既存定義を置換）:

```rust
pub struct AttributeReport {
    pub endpoint: Option<u16>,
    /// パスの ClusterId（Context 3）。wildcard 購読 report のイベント化に必要。
    pub cluster: Option<u32>,
    pub attribute: Option<u32>,
    pub list_append: bool,
    pub data: Option<serde_json::Value>,
    pub status: Option<u8>,
}

pub struct ReportDataMessage {
    pub reports: Vec<AttributeReport>,
    /// 購読 report が運ぶ SubscriptionId（tag 0）。read 応答では None。
    pub subscription_id: Option<u32>,
    pub more_chunks: bool,
    pub suppress_response: bool,
}
```

`decode_attribute_path_ib` の戻り値を `(Option<u16>, Option<u32>, Option<u32>, bool)`（endpoint, cluster, attribute, list_append）へ拡張し、`(Tag::Context(3), Value::Uint(v)) => cluster = Some(u32::try_from(v)...)` の腕を追加。呼び出し元 3 箇所（`decode_attribute_status_ib_full` / `decode_attribute_data_ib_full` / それらの戻り値を組む `decode_attribute_report_ib_full`）を cluster を通すよう更新。`AttributeDataFields` type alias も 5 要素に。

`decode_report_data_message` に `(Tag::Context(0), Value::Uint(v)) => subscription_id = Some(u32::try_from(v).map_err(|_| ImError::Malformed("subscription id out of range"))?)` の腕を追加。

新規関数:

```rust
/// SubscribeRequestMessage (spec §8.10) の wildcard 版。AttributeRequests は
/// 全フィールド省略の AttributePathIB 1 本（= 全 endpoint / 全 cluster / 全
/// attribute）。EventRequests は載せない（v1 は attribute report のみ）。
pub fn encode_subscribe_request_wildcard(
    min_interval_floor_s: u16,
    max_interval_ceiling_s: u16,
    keep_subscriptions: bool,
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bool(Tag::Context(0), keep_subscriptions);
    w.put_uint(Tag::Context(1), u64::from(min_interval_floor_s));
    w.put_uint(Tag::Context(2), u64::from(max_interval_ceiling_s));
    w.start_array(Tag::Context(3)); // AttributeRequests
    w.start_list(Tag::Anonymous); // AttributePathIB（全省略 = wildcard）
    w.end_container();
    w.end_container();
    // IsFabricFiltered = true: read と同じ既定（encode_read_request のコメント参照）。
    w.put_bool(Tag::Context(7), true);
    w.put_uint(Tag::Context(255), u64::from(IM_REVISION));
    w.end_container();
    w.finish()
}

/// SubscribeResponseMessage (spec §8.10): {0: SubscriptionId, 2: MaxInterval}.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubscribeResponse {
    pub subscription_id: u32,
    pub max_interval_s: u16,
}

pub fn decode_subscribe_response(payload: &[u8]) -> Result<SubscribeResponse, ImError> {
    let mut r = Reader::new(payload);
    expect_struct_start(&mut r)?;
    let mut id = None;
    let mut max_interval = None;
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated subscribe response"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::Uint(v)) => {
                id = Some(
                    u32::try_from(v)
                        .map_err(|_| ImError::Malformed("subscription id out of range"))?,
                );
            }
            (Tag::Context(2), Value::Uint(v)) => {
                max_interval = Some(
                    u16::try_from(v)
                        .map_err(|_| ImError::Malformed("max interval out of range"))?,
                );
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r)?;
            }
            _ => {}
        }
    }
    Ok(SubscribeResponse {
        subscription_id: id.ok_or(ImError::Malformed("subscribe response without id"))?,
        max_interval_s: max_interval
            .ok_or(ImError::Malformed("subscribe response without max interval"))?,
    })
}
```

既存の `ReportDataMessage` / `AttributeReport` 構造体リテラル（im.rs 内 decode 関数・既存テスト、session.rs は無し）をフィールド追加に合わせて更新する。

- [ ] **Step 4: テスト通過を確認**

Run: `cargo test -p mat-controller --lib`
Expected: 全 PASS（既存 im テスト含む）

- [ ] **Step 5: Commit**

```bash
git add crates/mat-controller/src/im.rs
git commit -m "feat(controller): Subscribe TLV encode/decode + report path cluster/subscription-id"
```

---

### Task 2: session.rs — 購読ハンドシェイク + ポンプ

**Files:**
- Modify: `crates/mat-controller/src/session.rs`

**Interfaces:**
- Consumes: Task 1 の `encode_subscribe_request_wildcard` / `decode_subscribe_response` / `decode_report_data_message`（`subscription_id` 付き）/ `OPCODE_SUBSCRIBE_*`
- Produces（`SecureSession` のメソッド）:
  - `pub async fn subscribe_wildcard(&mut self, min_interval_floor_s: u16, max_interval_ceiling_s: u16, keep_subscriptions: bool, cfg: &MrpConfig) -> Result<(crate::im::SubscribeResponse, Vec<crate::im::ReportDataMessage>), SessionError>` — priming 分割対応。各チャンクに StatusResponse(0) 応答、SubscribeResponse 受信で成立。
  - `pub async fn next_subscription_report(&mut self, timeout: Duration, cfg: &MrpConfig) -> Result<crate::im::ReportDataMessage, SessionError>` — デバイス発の新 exchange の ReportData を受け、StatusResponse で閉じる。無音は `SessionError::Timeout`。keep-alive は `reports` 空で返る。

**設計メモ（実装者向け）:** 既存 `screen()` は「自分が initiator の exchange」宛てしか通さない（`proto.initiator == true` は捨てる）。購読 report はデバイスが**新しい exchange を自分起点で**開いて送ってくるため、フィルタを一般化する。また `screen()` は認証済み needs_ack メッセージを**フィルタ前に ack する**ので、フィルタ落ちした device 発 ReportData を捨てると永久喪失する — バッファに積む。

- [ ] **Step 1: 失敗するテストを書く**

`session.rs` の tests に追加。**ReliableChannel ペア**を使う（MRP ack 無しで台本が単純になる。spec テスト方針 1）。デバイス側ヘルパは既存 `device_datagram` / `open_from_controller` が UDP 前提の const 鍵をそのまま使えるので流用する（transport だけ Reliable にする）:

```rust
/// ReliableChannel ペアで SecureSession（controller 側）と生 Transport（device 側）を組む。
fn reliable_session_pair() -> (SecureSession, Transport) {
    let (a, b) = crate::transport::ReliableChannel::pair();
    let s = SecureSession::new(
        Arc::new(a),
        crate::transport::RELIABLE_PEER,
        LOCAL_SID,
        PEER_SID,
        keys(),
        OUR_NODE,
        DEV_NODE,
    );
    (s, b)
}

/// 購読 priming 用 ReportData payload（subscription_id 付き、more 指定可）。
fn subscription_report_payload(sub_id: u32, value: bool, more: bool) -> Vec<u8> {
    use crate::tlv::{Tag, Writer};
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(sub_id));
    w.start_array(Tag::Context(1));
    w.start_struct(Tag::Anonymous);
    w.start_struct(Tag::Context(1));
    w.put_uint(Tag::Context(0), 1);
    w.start_list(Tag::Context(1));
    w.put_uint(Tag::Context(2), 1);
    w.put_uint(Tag::Context(3), 6);
    w.put_uint(Tag::Context(4), 0);
    w.end_container();
    w.put_bool(Tag::Context(2), value);
    w.end_container();
    w.end_container();
    w.end_container();
    if more {
        w.put_bool(Tag::Context(3), true);
    }
    w.put_uint(Tag::Context(255), 12);
    w.end_container();
    w.finish()
}

/// keep-alive（空 report）payload。
fn keepalive_payload(sub_id: u32) -> Vec<u8> {
    use crate::tlv::{Tag, Writer};
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(sub_id));
    w.put_uint(Tag::Context(255), 12);
    w.end_container();
    w.finish()
}

/// SubscribeResponse payload。
fn subscribe_response_payload(sub_id: u32, max_interval: u16) -> Vec<u8> {
    use crate::tlv::{Tag, Writer};
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(sub_id));
    w.put_uint(Tag::Context(2), u64::from(max_interval));
    w.put_uint(Tag::Context(255), 12);
    w.end_container();
    w.finish()
}

/// 購読ハンドシェイク: priming 2 チャンク（各チャンクに StatusResponse(0)）→
/// SubscribeResponse で成立。fragile part の釘打ち（spec テスト方針 1）。
#[tokio::test]
async fn subscribe_wildcard_handshake_with_chunked_priming() {
    let (mut s, dev) = reliable_session_pair();

    let dev_task = tokio::spawn(async move {
        // SubscribeRequest を受ける
        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, _) = dev.recv_from(&mut buf).await.unwrap();
        let (_, p, _body) = open_from_controller(&buf[..n]);
        assert_eq!(p.protocol_id, crate::im::PROTOCOL_ID_IM);
        assert_eq!(p.opcode, crate::im::OPCODE_SUBSCRIBE_REQUEST);
        let ex = p.exchange_id;
        // priming チャンク1（more=true）
        let d = device_datagram(
            ex,
            crate::im::PROTOCOL_ID_IM,
            crate::im::OPCODE_REPORT_DATA,
            None,
            false,
            9000,
            &subscription_report_payload(42, true, true),
        );
        dev.send_to(&d, RELIABLE_PEER).await.unwrap();
        // StatusResponse(0) を受ける
        let (n, _) = dev.recv_from(&mut buf).await.unwrap();
        let (_, p2, body) = open_from_controller(&buf[..n]);
        assert_eq!(p2.opcode, crate::im::OPCODE_STATUS_RESPONSE);
        assert_eq!(crate::im::decode_status_response(&body).unwrap(), 0);
        // priming チャンク2（more=false）
        let d = device_datagram(
            ex,
            crate::im::PROTOCOL_ID_IM,
            crate::im::OPCODE_REPORT_DATA,
            None,
            false,
            9001,
            &subscription_report_payload(42, false, false),
        );
        dev.send_to(&d, RELIABLE_PEER).await.unwrap();
        // 最終チャンクにも StatusResponse(0)（SubscribeResponse がこの後に続くため必須）
        let (n, _) = dev.recv_from(&mut buf).await.unwrap();
        let (_, p3, body) = open_from_controller(&buf[..n]);
        assert_eq!(p3.opcode, crate::im::OPCODE_STATUS_RESPONSE);
        assert_eq!(crate::im::decode_status_response(&body).unwrap(), 0);
        // SubscribeResponse
        let d = device_datagram(
            ex,
            crate::im::PROTOCOL_ID_IM,
            crate::im::OPCODE_SUBSCRIBE_RESPONSE,
            None,
            false,
            9002,
            &subscribe_response_payload(42, 120),
        );
        dev.send_to(&d, RELIABLE_PEER).await.unwrap();
    });

    let (resp, priming) = s.subscribe_wildcard(0, 3600, false, &fast_cfg()).await.unwrap();
    assert_eq!(resp.subscription_id, 42);
    assert_eq!(resp.max_interval_s, 120);
    assert_eq!(priming.len(), 2);
    assert_eq!(priming[0].reports[0].data, Some(serde_json::json!(true)));
    dev_task.await.unwrap();
}

/// ポンプ: デバイス起点の新 exchange（initiator=true）で届く ReportData を受け、
/// StatusResponse(0) で閉じる。keep-alive（空 report）も受かる。
#[tokio::test]
async fn next_subscription_report_receives_device_initiated_reports_and_keepalive() {
    let (mut s, dev) = reliable_session_pair();

    let dev_task = tokio::spawn(async move {
        // device 発の新 exchange。initiator=true（デバイスがその exchange の起点）。
        let header = MessageHeader {
            session_id: LOCAL_SID,
            security_flags: 0,
            message_counter: 100,
            source_node_id: None,
            destination: Destination::None,
        };
        let proto = ProtocolHeader {
            initiator: true,
            needs_ack: false,
            acked_counter: None,
            opcode: crate::im::OPCODE_REPORT_DATA,
            exchange_id: 0x7777,
            protocol_id: crate::im::PROTOCOL_ID_IM,
            vendor_id: None,
        };
        let d = seal_message(
            &R2I, &header, &proto,
            &subscription_report_payload(42, true, false), DEV_NODE,
        ).unwrap();
        dev.send_to(&d, RELIABLE_PEER).await.unwrap();
        // StatusResponse(0) が device の exchange 上で、こちら=non-initiator として返る
        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, _) = dev.recv_from(&mut buf).await.unwrap();
        let (_, p, body) = open_from_controller(&buf[..n]);
        assert_eq!(p.opcode, crate::im::OPCODE_STATUS_RESPONSE);
        assert_eq!(p.exchange_id, 0x7777);
        assert!(!p.initiator);
        assert_eq!(crate::im::decode_status_response(&body).unwrap(), 0);
        // keep-alive（別 exchange）
        let mut h2 = header;
        h2.message_counter = 101;
        let mut p2 = proto;
        p2.exchange_id = 0x7778;
        let d = seal_message(&R2I, &h2, &p2, &keepalive_payload(42), DEV_NODE).unwrap();
        dev.send_to(&d, RELIABLE_PEER).await.unwrap();
        let (n, _) = dev.recv_from(&mut buf).await.unwrap();
        let (_, p3, _) = open_from_controller(&buf[..n]);
        assert_eq!(p3.opcode, crate::im::OPCODE_STATUS_RESPONSE);
        assert_eq!(p3.exchange_id, 0x7778);
    });

    let rd = s
        .next_subscription_report(Duration::from_secs(2), &fast_cfg())
        .await
        .unwrap();
    assert_eq!(rd.subscription_id, Some(42));
    assert_eq!(rd.reports.len(), 1);
    let ka = s
        .next_subscription_report(Duration::from_secs(2), &fast_cfg())
        .await
        .unwrap();
    assert!(ka.reports.is_empty()); // keep-alive
    dev_task.await.unwrap();
}

/// 無音は Timeout（上位=matd が MaxInterval×1.5 で購読死亡と判定して再購読する）。
#[tokio::test]
async fn next_subscription_report_times_out_on_silence() {
    let (mut s, _dev) = reliable_session_pair();
    assert!(matches!(
        s.next_subscription_report(Duration::from_millis(100), &fast_cfg()).await,
        Err(SessionError::Timeout)
    ));
}
```

UDP 経路の回帰も 1 本（screen の ack/バッファ動作を釘打ち）:

```rust
/// UDP: device 発 needs_ack ReportData は screen が ack し、購読 API で取り出せる
/// （ack 済みメッセージの取り落とし=永久喪失が無いこと）。
#[tokio::test]
async fn udp_device_initiated_report_is_acked_and_delivered() {
    let device = bind_local().await;
    let peer = device.local_addr().unwrap();
    let transport = Arc::new(Transport::Udp(Arc::new(bind_local().await)));
    let local = transport.local_addr().unwrap();
    let mut s = SecureSession::new(
        Arc::clone(&transport), peer, LOCAL_SID, PEER_SID, keys(), OUR_NODE, DEV_NODE,
    );

    let dev = tokio::spawn(async move {
        let header = MessageHeader {
            session_id: LOCAL_SID,
            security_flags: 0,
            message_counter: 300,
            source_node_id: None,
            destination: Destination::None,
        };
        let proto = ProtocolHeader {
            initiator: true,
            needs_ack: true,
            acked_counter: None,
            opcode: crate::im::OPCODE_REPORT_DATA,
            exchange_id: 0x5555,
            protocol_id: crate::im::PROTOCOL_ID_IM,
            vendor_id: None,
        };
        let d = seal_message(
            &R2I, &header, &proto,
            &subscription_report_payload(9, true, false), DEV_NODE,
        ).unwrap();
        device.send_to(&d, local).await.unwrap();
        // standalone ack と StatusResponse(needs_ack) が来る。StatusResponse は ack を返す。
        loop {
            let mut buf = [0u8; MAX_DATAGRAM];
            let Ok(Ok((n, from))) = tokio::time::timeout(
                Duration::from_secs(2), device.recv_from(&mut buf)).await else { break };
            let (h, p, _) = open_from_controller(&buf[..n]);
            if p.opcode == crate::im::OPCODE_STATUS_RESPONSE {
                let ack = device_datagram(
                    p.exchange_id, PROTOCOL_ID_SECURE_CHANNEL, OPCODE_MRP_STANDALONE_ACK,
                    Some(h.message_counter), false, 9900, &[],
                );
                // device は自 exchange の initiator。ack の initiator は device 視点で true。
                // device_datagram は initiator=false 固定なので直接 seal する。
                let header2 = MessageHeader {
                    session_id: LOCAL_SID, security_flags: 0, message_counter: 9900,
                    source_node_id: None, destination: Destination::None,
                };
                let proto2 = ProtocolHeader {
                    initiator: true, needs_ack: false, acked_counter: Some(h.message_counter),
                    opcode: OPCODE_MRP_STANDALONE_ACK, exchange_id: p.exchange_id,
                    protocol_id: PROTOCOL_ID_SECURE_CHANNEL, vendor_id: None,
                };
                let _ = ack;
                let d2 = seal_message(&R2I, &header2, &proto2, &[], DEV_NODE).unwrap();
                device.send_to(&d2, from).await.unwrap();
                break;
            }
        }
    });

    let rd = s
        .next_subscription_report(Duration::from_secs(2), &fast_cfg())
        .await
        .unwrap();
    assert_eq!(rd.subscription_id, Some(9));
    dev.await.unwrap();
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-controller --lib session::tests::subscribe`
Expected: コンパイルエラー（`subscribe_wildcard` / `next_subscription_report` 未定義）

- [ ] **Step 3: 実装**

(a) screen のフィルタ一般化。private enum とバッファを導入:

```rust
/// screen の配送フィルタ。ack/dedup はフィルタに依らず常に行う。
#[derive(Clone, Copy)]
enum ScreenFilter {
    /// 自分が initiator の exchange 宛て（従来動作）。
    OurExchange(u16),
    /// デバイスが initiator の特定 exchange 宛て（購読 report への応答 ack 待ち用）。
    PeerExchange(u16),
    /// デバイス起点 exchange 全部（購読ポンプの report 待ち用）。
    AnyPeerInitiated,
}
```

`SecureSession` にフィールド追加:

```rust
/// screen のフィルタ落ちで捨てると永久喪失する device 発 ReportData の待避
/// バッファ（screen は認証済み needs_ack メッセージをフィルタ前に ack するため、
/// ack 済みをドロップしてはならない）。購読 API だけが消費する。
peer_initiated: std::collections::VecDeque<IncomingMessage>,
```

（`new()` で `peer_initiated: std::collections::VecDeque::new(),` を初期化。上限 32、超過時は最古を捨てて `tracing::warn!`。）

`screen` を `screen_with(&mut self, buf, from, filter: ScreenFilter)` に改名し、末尾の判定を:

```rust
let deliver = match filter {
    ScreenFilter::OurExchange(ex) => proto.exchange_id == ex && !proto.initiator,
    ScreenFilter::PeerExchange(ex) => proto.exchange_id == ex && proto.initiator,
    ScreenFilter::AnyPeerInitiated => proto.initiator,
};
if !deliver {
    // フィルタ落ちでも device 発 ReportData は ack 済みなので待避する。
    if proto.initiator
        && proto.protocol_id == crate::im::PROTOCOL_ID_IM
        && proto.opcode == crate::im::OPCODE_REPORT_DATA
    {
        if self.peer_initiated.len() >= MAX_PEER_INITIATED_BUFFER {
            tracing::warn!("peer-initiated report buffer full; dropping oldest");
            self.peer_initiated.pop_front();
        }
        self.peer_initiated.push_back(IncomingMessage { header, proto, payload });
    }
    return Ok(None);
}
Ok(Some(IncomingMessage { header, proto, payload }))
```

既存の `screen(&mut self, buf, from, exchange_id)` は `self.screen_with(buf, from, ScreenFilter::OurExchange(exchange_id))` を呼ぶ薄いラッパとして残す（`send_reliable` / `recv` は無改変で通る）。定数 `const MAX_PEER_INITIATED_BUFFER: usize = 32;`。

(b) device 起点 exchange への応答送信（役割 initiator=false）:

```rust
/// デバイス起点の exchange へ StatusResponse(status) を返す。UDP では
/// needs_ack + 再送で相手の standalone ack を待つ（購読 report の確認応答は
/// IM 契約上必須 — 取りこぼすとデバイスが購読を落とす）。Reliable transport
/// は 1 回送るだけ。
pub async fn respond_status(
    &mut self,
    exchange_id: u16,
    status: u8,
    cfg: &MrpConfig,
) -> Result<(), SessionError> {
    use crate::im;
    let payload = im::encode_status_response(status);
    if self.transport.is_reliable() {
        let (datagram, _) = self.seal(
            exchange_id, false, im::PROTOCOL_ID_IM, im::OPCODE_STATUS_RESPONSE,
            false, None, &payload,
        )?;
        self.transport.send_to(&datagram, self.peer).await?;
        return Ok(());
    }
    let (datagram, our_counter) = self.seal(
        exchange_id, false, im::PROTOCOL_ID_IM, im::OPCODE_STATUS_RESPONSE,
        true, None, &payload,
    )?;
    let mut interval = cfg.initial_interval;
    let mut attempts = 0u32;
    loop {
        self.transport.send_to(&datagram, self.peer).await?;
        let deadline = Instant::now() + interval;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let mut buf = [0u8; MAX_DATAGRAM];
            let Ok(recv) =
                tokio::time::timeout(remaining, self.transport.recv_from(&mut buf)).await
            else {
                break;
            };
            let (n, from) = recv?;
            let Some(msg) = self
                .screen_with(&buf[..n], from, ScreenFilter::PeerExchange(exchange_id))
                .await?
            else {
                continue;
            };
            if msg.proto.acked_counter == Some(our_counter) {
                return Ok(());
            }
        }
        attempts += 1;
        if attempts > cfg.max_retries {
            return Err(SessionError::Timeout);
        }
        interval = interval.mul_f64(cfg.backoff);
    }
}
```

(c) 購読ハンドシェイク:

```rust
/// wildcard Subscribe を張る（spec §8.10、v1: attribute report のみ）。
/// priming ReportData（分割対応、各チャンクに StatusResponse(0) 応答）→
/// SubscribeResponse 受信で成立。priming の中身も返す（matd が priming=true
/// イベントとして流す）。
pub async fn subscribe_wildcard(
    &mut self,
    min_interval_floor_s: u16,
    max_interval_ceiling_s: u16,
    keep_subscriptions: bool,
    cfg: &MrpConfig,
) -> Result<(crate::im::SubscribeResponse, Vec<crate::im::ReportDataMessage>), SessionError> {
    use crate::im::{self, ImError};
    let exchange_id = Self::new_exchange_id();
    let req = im::encode_subscribe_request_wildcard(
        min_interval_floor_s,
        max_interval_ceiling_s,
        keep_subscriptions,
    );
    let resp = self
        .send_reliable(exchange_id, im::PROTOCOL_ID_IM, im::OPCODE_SUBSCRIBE_REQUEST, &req, cfg)
        .await?;
    let mut msg = match resp {
        Some(m) => m,
        None => self.recv(exchange_id, IM_RECV_TIMEOUT).await?,
    };
    let mut priming = Vec::new();
    loop {
        match msg.proto.opcode {
            im::OPCODE_REPORT_DATA => {
                let rd = im::decode_report_data_message(&msg.payload).map_err(SessionError::Im)?;
                priming.push(rd);
                if priming.len() > MAX_REPORT_CHUNKS {
                    return Err(SessionError::Im(ImError::Malformed("too many report chunks")));
                }
                // priming の各チャンクに StatusResponse(0)。最終チャンク後は
                // SubscribeResponse が同 exchange で続く。
                let ok = im::encode_status_response(0);
                let resp = self
                    .send_reliable(
                        exchange_id, im::PROTOCOL_ID_IM, im::OPCODE_STATUS_RESPONSE, &ok, cfg,
                    )
                    .await?;
                msg = match resp {
                    Some(m) => m,
                    None => self.recv(exchange_id, IM_RECV_TIMEOUT).await?,
                };
            }
            im::OPCODE_SUBSCRIBE_RESPONSE => {
                let sr = im::decode_subscribe_response(&msg.payload).map_err(SessionError::Im)?;
                return Ok((sr, priming));
            }
            im::OPCODE_STATUS_RESPONSE => {
                let s = im::decode_status_response(&msg.payload).map_err(SessionError::Im)?;
                return Err(SessionError::Im(ImError::StatusResponse(s)));
            }
            op => return Err(SessionError::UnexpectedOpcode(op)),
        }
    }
}
```

(d) ポンプ:

```rust
/// 購読成立後のデバイス発 ReportData を 1 通受ける。keep-alive（空 report）も
/// そのまま返す（deadline リセットは呼び出し側 = matd の責務）。`timeout` 無音は
/// `SessionError::Timeout`（上位が購読死亡として再購読する）。
pub async fn next_subscription_report(
    &mut self,
    timeout: Duration,
    cfg: &MrpConfig,
) -> Result<crate::im::ReportDataMessage, SessionError> {
    use crate::im;
    // screen が待避した report が先にあればそれを消費する。
    let msg = if let Some(m) = self.peer_initiated.pop_front() {
        m
    } else {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(SessionError::Timeout);
            }
            let mut buf = [0u8; MAX_DATAGRAM];
            let Ok(recv) =
                tokio::time::timeout(remaining, self.transport.recv_from(&mut buf)).await
            else {
                return Err(SessionError::Timeout);
            };
            let (n, from) = recv?;
            let Some(m) = self
                .screen_with(&buf[..n], from, ScreenFilter::AnyPeerInitiated)
                .await?
            else {
                continue;
            };
            if m.proto.protocol_id == PROTOCOL_ID_SECURE_CHANNEL
                && m.proto.opcode == OPCODE_MRP_STANDALONE_ACK
            {
                continue;
            }
            break m;
        }
    };
    if msg.proto.opcode != im::OPCODE_REPORT_DATA {
        return Err(SessionError::UnexpectedOpcode(msg.proto.opcode));
    }
    let rd = im::decode_report_data_message(&msg.payload).map_err(SessionError::Im)?;
    if !rd.suppress_response {
        self.respond_status(msg.proto.exchange_id, 0, cfg).await?;
    }
    Ok(rd)
}
```

- [ ] **Step 4: テスト通過を確認**

Run: `cargo test -p mat-controller --lib`
Expected: 全 PASS（既存 session テストの回帰なし）

- [ ] **Step 5: Commit**

```bash
git add crates/mat-controller/src/session.rs
git commit -m "feat(controller): wildcard subscribe handshake + device-initiated report pump"
```

---

### Task 3: mat-native — SubscribeConn / establish_subscription

**Files:**
- Modify: `crates/mat-native/src/lib.rs`
- Modify: `crates/mat-native/src/test_support.rs`

**Interfaces:**
- Consumes: Task 2 の `SecureSession::subscribe_wildcard` / `next_subscription_report`、`im::SubscribeResponse` / `im::ReportDataMessage`
- Produces:
  - `pub const SUBSCRIBE_MIN_INTERVAL_FLOOR_S: u16 = 0;` / `pub const SUBSCRIBE_MAX_INTERVAL_CEILING_S: u16 = 3600;` / `pub const SUBSCRIBE_KEEP_SUBSCRIPTIONS: bool = false;`
  - `pub struct SubscriptionInfo { pub subscription_id: u32, pub max_interval_s: u16 }`
  - `pub trait SubscribeConn: Send`:
    - `async fn subscribe_wildcard(&mut self) -> Result<(SubscriptionInfo, Vec<mat_controller::im::ReportDataMessage>), MatError>`
    - `async fn next_report(&mut self, timeout: Duration) -> Result<mat_controller::im::ReportDataMessage, MatError>`
  - `Establisher` trait に default メソッド `async fn establish_subscription(&self, node_id: u64) -> Result<Box<dyn SubscribeConn>, MatError>`（default = `Err(Other)`）
  - test_support: `pub struct FakeSubConn`（scripted reports）+ `FakeEstablisher` の `establish_subscription` 実装

- [ ] **Step 1: 失敗するテストを書く**

`lib.rs` の tests に追加:

```rust
#[tokio::test]
async fn default_establisher_rejects_subscription() {
    // Establisher trait の default 実装は購読非対応（CaseEstablisher だけが上書き）。
    struct NoSub;
    #[async_trait]
    impl Establisher for NoSub {
        async fn establish(&self, _node_id: u64) -> Result<Box<dyn NodeConn>, MatError> {
            Err(MatError::new(ErrorKind::Other, "unused"))
        }
    }
    let err = NoSub.establish_subscription(1).await.unwrap_err();
    assert_eq!(err.kind, ErrorKind::Other);
    assert!(err.detail.contains("subscription"));
}

#[tokio::test]
async fn fake_establisher_serves_scripted_subscription() {
    use crate::test_support::{FakeEstablisher, FakeSubConn};
    let est = FakeEstablisher::default();
    let mut conn = est.establish_subscription(5).await.unwrap();
    let (info, priming) = conn.subscribe_wildcard().await.unwrap();
    assert_eq!(info.max_interval_s, 60);
    assert_eq!(priming.len(), 1); // default fake は onoff=true の priming 1 チャンク
    // scripted report が尽きたら next_report は timeout で Err(Timeout)。
    let err = conn
        .next_report(std::time::Duration::from_millis(50))
        .await
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::Timeout);
    let _ = FakeSubConn::default(); // 型が公開されていること
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p mat-native --lib`
Expected: コンパイルエラー（`establish_subscription` / `FakeSubConn` 未定義）

- [ ] **Step 3: 実装（lib.rs）**

```rust
/// 購読パラメータ（spec 決定値）: 人感の即応性優先で floor 0、sleepy の電池優先で
/// ceiling 3600s（実間隔はデバイスが選ぶ）、再購読時に古い購読を掃除するため
/// KeepSubscriptions=false。
pub const SUBSCRIBE_MIN_INTERVAL_FLOOR_S: u16 = 0;
pub const SUBSCRIBE_MAX_INTERVAL_CEILING_S: u16 = 3600;
pub const SUBSCRIBE_KEEP_SUBSCRIPTIONS: bool = false;

/// 購読成立の結果（SubscriptionId とデバイス選択の MaxInterval）。
#[derive(Debug, Clone, Copy)]
pub struct SubscriptionInfo {
    pub subscription_id: u32,
    pub max_interval_s: u16,
}

/// 購読専用コネクション（専用 UdpTransport + 専用 CASE をポンプが独占する。
/// 既存 op 経路 = warm session は不変 — spec 構造判断）。
#[async_trait]
pub trait SubscribeConn: Send {
    /// wildcard Subscribe を張り、成立情報と priming report 群を返す。
    async fn subscribe_wildcard(
        &mut self,
    ) -> Result<(SubscriptionInfo, Vec<mat_controller::im::ReportDataMessage>), MatError>;
    /// 次のデバイス発 report を待つ（keep-alive は reports 空で返る）。
    /// 無音 `timeout` 経過は kind=Timeout。
    async fn next_report(
        &mut self,
        timeout: Duration,
    ) -> Result<mat_controller::im::ReportDataMessage, MatError>;
}
```

`Establisher` trait に default メソッド追加:

```rust
#[async_trait]
pub trait Establisher: Send + Sync {
    async fn establish(&self, node_id: u64) -> Result<Box<dyn NodeConn>, MatError>;
    /// 購読専用の transport + CASE を別に確立する（matd SubscriptionManager 用）。
    /// 既定は非対応 — 実確立器（CaseEstablisher）だけが上書きする。
    async fn establish_subscription(
        &self,
        _node_id: u64,
    ) -> Result<Box<dyn SubscribeConn>, MatError> {
        Err(MatError::new(
            ErrorKind::Other,
            "subscription not supported by this establisher",
        ))
    }
}
```

`CaseEstablisher` に impl 追加（既存 `establish` と同じ resolve→CASE、ただし**毎回新しい UdpTransport を bind** する）:

```rust
#[async_trait]
impl Establisher for CaseEstablisher {
    async fn establish(&self, node_id: u64) -> Result<Box<dyn NodeConn>, MatError> {
        // （既存実装そのまま）
    }

    async fn establish_subscription(
        &self,
        node_id: u64,
    ) -> Result<Box<dyn SubscribeConn>, MatError> {
        // 購読専用ソケット: op 用の共有 transport と recv を奪い合わないよう、
        // ノードごとに専用 UdpTransport + 専用 CASE を確立する（spec 構造判断）。
        let transport = UdpTransport::bind().await.map_err(|e| {
            MatError::new(ErrorKind::Other, format!("native: bind subscription udp: {e}"))
        })?;
        let transport = Arc::new(Transport::Udp(Arc::new(transport)));
        let cfid = compressed_fabric_id(&self.creds.root_public_key, self.creds.fabric_id);
        let resolved = self
            .resolver
            .resolve(self.scope_id, cfid, node_id, RESOLVE_TIMEOUT)
            .await
            .map_err(|e| map_resolve_err(node_id, e))?;
        let mrp = resolved.mrp_config();
        let peers: Vec<SocketAddr> = resolved.socket_addrs(self.scope_id);
        let mut last: Option<MatError> = None;
        for peer in peers {
            match case::establish(Arc::clone(&transport), peer, &self.creds, node_id, &mrp).await {
                Ok(session) => return Ok(Box::new(SubscriptionSession { session, mrp })),
                Err(e) => {
                    last = Some(MatError::new(
                        ErrorKind::SessionFailed,
                        format!("native: subscription CASE via {peer}: {e}"),
                    ));
                }
            }
        }
        Err(last.unwrap_or_else(|| {
            MatError::new(
                ErrorKind::Unreachable,
                format!("native: no addresses resolved for node {node_id}"),
            )
        }))
    }
}

/// 購読専用の実セッション。
struct SubscriptionSession {
    session: mat_controller::session::SecureSession,
    mrp: MrpConfig,
}

#[async_trait]
impl SubscribeConn for SubscriptionSession {
    async fn subscribe_wildcard(
        &mut self,
    ) -> Result<(SubscriptionInfo, Vec<mat_controller::im::ReportDataMessage>), MatError> {
        let (resp, priming) = self
            .session
            .subscribe_wildcard(
                SUBSCRIBE_MIN_INTERVAL_FLOOR_S,
                SUBSCRIBE_MAX_INTERVAL_CEILING_S,
                SUBSCRIBE_KEEP_SUBSCRIPTIONS,
                &self.mrp,
            )
            .await
            .map_err(map_session_err)?;
        Ok((
            SubscriptionInfo {
                subscription_id: resp.subscription_id,
                max_interval_s: resp.max_interval_s,
            },
            priming,
        ))
    }

    async fn next_report(
        &mut self,
        timeout: Duration,
    ) -> Result<mat_controller::im::ReportDataMessage, MatError> {
        self.session
            .next_subscription_report(timeout, &self.mrp)
            .await
            .map_err(map_session_err)
    }
}
```

（注: `map_session_err` は `SessionError::Timeout` → kind=Timeout に写す既存関数。ポンプの無音死亡検知はこの Timeout kind に乗る。）

- [ ] **Step 4: 実装（test_support.rs）**

`FakeSubConn`（matd の SubscriptionManager テスト用。既存 FakeConn と同じ流儀で scripted）:

```rust
/// 購読 fake。`priming` は subscribe_wildcard が返す priming チャンク、`live` は
/// next_report が 1 呼び出し 1 通で払い出すキュー。尽きたら timeout まで待って
/// kind=Timeout（実セッションの無音と同じ形）。
pub struct FakeSubConn {
    pub max_interval_s: u16,
    pub priming: Vec<mat_controller::im::ReportDataMessage>,
    pub live: std::collections::VecDeque<mat_controller::im::ReportDataMessage>,
}

/// onoff on-off=true の AttributeReport 1 件を持つ ReportDataMessage を作る
/// （テストフィクスチャ共通形）。
pub fn onoff_report(sub_id: u32, value: bool) -> mat_controller::im::ReportDataMessage {
    mat_controller::im::ReportDataMessage {
        reports: vec![mat_controller::im::AttributeReport {
            endpoint: Some(1),
            cluster: Some(0x0006),
            attribute: Some(0x0000),
            list_append: false,
            data: Some(serde_json::json!(value)),
            status: None,
        }],
        subscription_id: Some(sub_id),
        more_chunks: false,
        suppress_response: false,
    }
}

impl Default for FakeSubConn {
    fn default() -> Self {
        Self {
            max_interval_s: 60,
            priming: vec![onoff_report(1, true)],
            live: std::collections::VecDeque::new(),
        }
    }
}

#[async_trait]
impl crate::SubscribeConn for FakeSubConn {
    async fn subscribe_wildcard(
        &mut self,
    ) -> Result<(crate::SubscriptionInfo, Vec<mat_controller::im::ReportDataMessage>), MatError>
    {
        Ok((
            crate::SubscriptionInfo {
                subscription_id: 1,
                max_interval_s: self.max_interval_s,
            },
            std::mem::take(&mut self.priming),
        ))
    }

    async fn next_report(
        &mut self,
        timeout: std::time::Duration,
    ) -> Result<mat_controller::im::ReportDataMessage, MatError> {
        if let Some(r) = self.live.pop_front() {
            return Ok(r);
        }
        tokio::time::sleep(timeout).await;
        Err(MatError::new(ErrorKind::Timeout, "fake: no more reports"))
    }
}
```

`FakeEstablisher` に `establish_subscription` を実装（default 構成で `FakeSubConn::default()` を返す）:

```rust
async fn establish_subscription(
    &self,
    _node_id: u64,
) -> Result<Box<dyn crate::SubscribeConn>, MatError> {
    self.calls.fetch_add(1, Ordering::SeqCst);
    Ok(Box::new(FakeSubConn::default()))
}
```

（`FakeEstablisher` の既存フィールド構成は変えない。`fail_first_send` 等は購読 fake に波及させない。）

- [ ] **Step 5: テスト通過を確認**

Run: `cargo test -p mat-native --lib && cargo test -p mat-native --features test-support`
Expected: 全 PASS

- [ ] **Step 6: Commit**

```bash
git add crates/mat-native/src/lib.rs crates/mat-native/src/test_support.rs
git commit -m "feat(native): SubscribeConn + dedicated-socket subscription establisher"
```

---

### Task 4: matd — Event / SubscriptionManager

**Files:**
- Create: `crates/matd/src/subscription.rs`
- Modify: `crates/matd/src/lib.rs`（`pub mod subscription;` 追加）
- Modify: `crates/matd/src/native.rs`（`NativeBackend::establish_subscription` 委譲メソッド追加）

**Interfaces:**
- Consumes: Task 3 の `SubscribeConn` / `SubscriptionInfo` / `FakeSubConn`、`mat_controller::im::{ReportDataMessage, AttributeReport}`、`mat_core::store::Store`、`mat_core::output::now_iso8601`
- Produces:
  - `pub struct Event { pub node_id: u64, pub endpoint: u16, pub cluster: u32, pub attribute: u32, pub value: serde_json::Value, pub priming: bool }`（`Clone`）
  - `impl Event { pub fn to_json(&self) -> serde_json::Value }` — `{"timestamp","node_id","endpoint","cluster","attribute","value","priming"}`、cluster/attribute は `mat_core::ids` で名前化（無ければ数値のまま）
  - `pub fn events_from_report(node_id: u64, msg: &ReportDataMessage, priming: bool) -> Vec<Event>` — scalar のみ、list/struct は debug ログで捨てる
  - `pub fn spawn_subscription_manager(native: std::sync::Arc<crate::server::NativeState>, store_path: std::path::PathBuf, events: tokio::sync::broadcast::Sender<Event>) -> Vec<tokio::task::JoinHandle<()>>`
  - `pub(crate) fn next_backoff(cur: std::time::Duration) -> std::time::Duration`（5s→倍々→上限 300s）
  - `NativeBackend::establish_subscription(&self, node_id) -> Result<Box<dyn mat_native::SubscribeConn>, MatError>`

- [ ] **Step 1: 失敗するテストを書く**

`crates/matd/src/subscription.rs` を tests 込みで新規作成する前提なので、まず tests 部分（ファイル末尾）を先に確定する:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use mat_native::test_support::{onoff_report, FakeEstablisher};
    use serde_json::json;

    #[test]
    fn event_json_uses_chip_tool_names_and_numeric_fallback() {
        let ev = Event {
            node_id: 21,
            endpoint: 1,
            cluster: 0x0406, // occupancysensing
            attribute: 0x0000, // occupancy
            value: json!(1),
            priming: false,
        };
        let j = ev.to_json();
        assert_eq!(j["node_id"], 21);
        assert_eq!(j["endpoint"], 1);
        assert_eq!(j["cluster"], "occupancysensing");
        assert_eq!(j["attribute"], "occupancy");
        assert_eq!(j["value"], 1);
        assert_eq!(j["priming"], false);
        assert!(j["timestamp"].is_string());

        // ids テーブルに無いものは数値のまま。
        let ev = Event { cluster: 0xFFF1_0001, attribute: 0x9999, ..ev };
        let j = ev.to_json();
        assert_eq!(j["cluster"], 0xFFF1_0001u32);
        assert_eq!(j["attribute"], 0x9999);
    }

    #[test]
    fn events_from_report_keeps_scalars_and_drops_containers() {
        let mut msg = onoff_report(1, true);
        // list/struct（wildcard priming に混ざる ACL / server-list 等）は捨てる。
        msg.reports.push(mat_controller::im::AttributeReport {
            endpoint: Some(0),
            cluster: Some(0x001F),
            attribute: Some(0x0000),
            list_append: false,
            data: Some(json!([{ "1": 5 }])),
            status: None,
        });
        // status-only / path 欠落も捨てる。
        msg.reports.push(mat_controller::im::AttributeReport {
            endpoint: None,
            cluster: None,
            attribute: None,
            list_append: false,
            data: None,
            status: Some(0x7E),
        });
        let evs = events_from_report(7, &msg, true);
        assert_eq!(evs.len(), 1);
        assert_eq!(evs[0].node_id, 7);
        assert_eq!(evs[0].cluster, 0x0006);
        assert_eq!(evs[0].value, json!(true));
        assert!(evs[0].priming);
    }

    #[test]
    fn backoff_doubles_from_5s_capped_at_5min() {
        use std::time::Duration;
        assert_eq!(next_backoff(Duration::ZERO), Duration::from_secs(5));
        assert_eq!(next_backoff(Duration::from_secs(5)), Duration::from_secs(10));
        assert_eq!(next_backoff(Duration::from_secs(160)), Duration::from_secs(300));
        assert_eq!(next_backoff(Duration::from_secs(300)), Duration::from_secs(300));
    }

    /// manager 経路: fake establisher の priming report が priming=true イベントで
    /// broadcast へ流れる。
    #[tokio::test]
    async fn manager_emits_priming_events_from_fake_subscription() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = mat_core::store::Store::open_or_init(dir.path()).unwrap();
        store
            .upsert_node(mat_core::store::NodeRecord {
                node_id: 5,
                address: Some("192.0.2.10".into()),
                commissioned_at: "2026-07-20T00:00:00+09:00".into(),
            })
            .unwrap();

        let native = crate::native::NativeBackend::with_establisher(Box::new(
            FakeEstablisher::default(),
        ));
        let state = std::sync::Arc::new(crate::server::NativeState::Ready(Box::new(native)));
        let (tx, mut rx) = tokio::sync::broadcast::channel(16);
        let _handles = spawn_subscription_manager(state, dir.path().to_path_buf(), tx);

        let ev = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("no event within 2s")
            .unwrap();
        assert_eq!(ev.node_id, 5);
        assert_eq!(ev.cluster, 0x0006);
        assert!(ev.priming);
    }
}
```

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p matd --lib subscription`
Expected: コンパイルエラー（モジュール未実装）

- [ ] **Step 3: 実装**

`crates/matd/src/native.rs` に委譲メソッド:

```rust
/// 購読専用コネクション（専用ソケット + 専用 CASE）を確立する。warm session
/// slot（`with_session`）とは独立 — 購読ポンプが独占する。
pub async fn establish_subscription(
    &self,
    node_id: u64,
) -> Result<Box<dyn mat_native::SubscribeConn>, MatError> {
    self.engine.establisher.establish_subscription(node_id).await
}
```

`crates/matd/src/lib.rs` に `pub mod subscription;` を追加。

`crates/matd/src/subscription.rs` 本体:

```rust
//! matd 常駐 Subscribe（spec: 2026-07-20-matd-subscribe-listen-design.md ②）。
//!
//! 起動時に KVS から commissioned ノード一覧を読み、ノードごとに購読タスクを
//! 1 本張る: resolve（常駐 mDNS キャッシュ）→ 専用 CASE → wildcard Subscribe →
//! ポンプ。失敗・死亡時は指数 backoff（5s 開始、上限 5min）で再購読。
//! イベントは `tokio::sync::broadcast` で listen 接続へ配る。
//! 状態は持たない（リングバッファ/リプレイ無し — 聞いている間だけ届く契約）。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;

use mat_controller::im::ReportDataMessage;
use mat_core::output::now_iso8601;
use mat_core::store::Store;

use crate::server::NativeState;

/// 購読死亡判定: デバイス選択 MaxInterval の 1.5 倍無音でループを抜け再購読。
const DEATH_FACTOR: f64 = 1.5;
/// 再購読 backoff の初期値 / 上限。
const BACKOFF_INITIAL: Duration = Duration::from_secs(5);
const BACKOFF_MAX: Duration = Duration::from_secs(300);

/// listen へ配る 1 イベント。cluster/attribute は数値で持ち、JSON 化時に
/// chip-tool 記法へ名前化する（フィルタ照合は数値で行うため）。
#[derive(Debug, Clone)]
pub struct Event {
    pub node_id: u64,
    pub endpoint: u16,
    pub cluster: u32,
    pub attribute: u32,
    pub value: serde_json::Value,
    pub priming: bool,
}

impl Event {
    /// mat スキーマの NDJSON 1 行分。cluster/attribute は `mat-core::ids` に
    /// あれば chip-tool 記法名、無ければ数値のまま（read と同じ規律）。
    pub fn to_json(&self) -> serde_json::Value {
        let cluster = match mat_core::ids::find_cluster(self.cluster) {
            Some(def) => serde_json::json!(def.name),
            None => serde_json::json!(self.cluster),
        };
        let attribute = match mat_core::ids::find_cluster(self.cluster)
            .and_then(|c| c.attrs.iter().find(|a| a.id == self.attribute))
        {
            Some(def) => serde_json::json!(def.name),
            None => serde_json::json!(self.attribute),
        };
        serde_json::json!({
            "timestamp": now_iso8601(),
            "node_id": self.node_id,
            "endpoint": self.endpoint,
            "cluster": cluster,
            "attribute": attribute,
            "value": self.value,
            "priming": self.priming,
        })
    }
}

/// ReportDataMessage をイベント列へ。scalar 値のみイベント化し、list/struct
/// （ACL・server-list 等 wildcard priming に混ざるもの）は debug ログで捨てる
/// （generic read と同じ既知の制限）。path が欠けた report・status-only も捨てる。
pub fn events_from_report(node_id: u64, msg: &ReportDataMessage, priming: bool) -> Vec<Event> {
    let mut out = Vec::new();
    for rep in &msg.reports {
        let (Some(endpoint), Some(cluster), Some(attribute)) =
            (rep.endpoint, rep.cluster, rep.attribute)
        else {
            continue;
        };
        let Some(data) = &rep.data else { continue };
        if data.is_array() || data.is_object() {
            tracing::debug!(node_id, endpoint, cluster, attribute, "dropping non-scalar report");
            continue;
        }
        out.push(Event {
            node_id,
            endpoint,
            cluster,
            attribute,
            value: data.clone(),
            priming,
        });
    }
    out
}

/// 指数 backoff: 5s 開始、倍々、上限 5min。
pub(crate) fn next_backoff(cur: Duration) -> Duration {
    if cur.is_zero() {
        BACKOFF_INITIAL
    } else {
        (cur * 2).min(BACKOFF_MAX)
    }
}

/// commissioned 全ノードへ購読タスクを張る（v1: 起動時の台帳スナップショット。
/// 将来 subscriptions.toml で絞り込み）。native が Unavailable なら何もしない。
pub fn spawn_subscription_manager(
    native: Arc<NativeState>,
    store_path: PathBuf,
    events: broadcast::Sender<Event>,
) -> Vec<tokio::task::JoinHandle<()>> {
    let node_ids: Vec<u64> = match Store::open(&store_path) {
        Ok(store) => store.nodes().map(|n| n.node_id).collect(),
        Err(e) => {
            tracing::warn!(error = %e.detail, "subscription manager: store unreadable; no subscriptions");
            return Vec::new();
        }
    };
    tracing::info!(nodes = node_ids.len(), "subscription manager starting");
    node_ids
        .into_iter()
        .map(|node_id| {
            let native = Arc::clone(&native);
            let events = events.clone();
            tokio::spawn(async move { node_subscription_loop(node_id, native, events).await })
        })
        .collect()
}

/// 1 ノードの購読ループ。確立 → priming 配信 → ポンプ。失敗・死亡は backoff 再購読。
/// リトライは debug、確立/喪失の状態遷移のみ info（弱リンクノードを常駐ノイズに
/// しない — spec ②）。
async fn node_subscription_loop(
    node_id: u64,
    native: Arc<NativeState>,
    events: broadcast::Sender<Event>,
) {
    let NativeState::Ready(backend) = &*native else { return };
    let mut backoff = Duration::ZERO;
    loop {
        match run_subscription_once(node_id, backend, &events).await {
            Ok(()) => {
                // 購読が成立して喪失した: 状態遷移なので info、backoff はリセット。
                tracing::info!(node_id, "subscription lost; resubscribing");
                backoff = Duration::ZERO;
            }
            Err(e) => {
                tracing::debug!(node_id, kind = ?e.kind, detail = %e.detail, "subscription attempt failed");
            }
        }
        backoff = next_backoff(backoff);
        tokio::time::sleep(backoff).await;
    }
}

/// 1 回の購読試行。確立+Subscribe 成立まで到達したら Ok を返して抜ける
/// （ポンプ死亡=正常喪失）。確立前の失敗は Err。
async fn run_subscription_once(
    node_id: u64,
    backend: &crate::native::NativeBackend,
    events: &broadcast::Sender<Event>,
) -> Result<(), mat_core::error::MatError> {
    let mut conn = backend.establish_subscription(node_id).await?;
    let (info, priming) = conn.subscribe_wildcard().await?;
    tracing::info!(
        node_id,
        subscription_id = info.subscription_id,
        max_interval_s = info.max_interval_s,
        "subscription established"
    );
    for msg in &priming {
        for ev in events_from_report(node_id, msg, true) {
            let _ = events.send(ev); // 受信者ゼロは正常（listen 接続なし）
        }
    }
    let deadline = Duration::from_secs_f64(f64::from(info.max_interval_s) * DEATH_FACTOR)
        .max(Duration::from_secs(5)); // MaxInterval が極端に小さくても常識的な下限
    loop {
        match conn.next_report(deadline).await {
            Ok(msg) => {
                for ev in events_from_report(node_id, &msg, false) {
                    let _ = events.send(ev);
                }
                // keep-alive（reports 空）も無音 deadline をリセットするだけで良い。
            }
            Err(_) => return Ok(()), // 無音死亡 or セッションエラー → 再購読
        }
    }
}
```

- [ ] **Step 4: テスト通過を確認**

Run: `cargo test -p matd --lib subscription`
Expected: 全 PASS

（注: `manager_emits_priming_events_from_fake_subscription` は FakeSubConn の live が空 → timeout 後 backoff ループに入るが、テストは最初のイベント受信で終わる。ハンドルは drop され task は runtime 終了で消える。）

- [ ] **Step 5: Commit**

```bash
git add crates/matd/src/subscription.rs crates/matd/src/lib.rs crates/matd/src/native.rs
git commit -m "feat(matd): SubscriptionManager — per-node wildcard subscribe with backoff"
```

---

### Task 5: matd — socket 新 op `listen`（ack + ストリーム）

**Files:**
- Modify: `crates/matd/src/protocol.rs`
- Modify: `crates/matd/src/server.rs`
- Modify: `crates/matd/tests/integration.rs`
- Modify: `crates/matd/src/main.rs`（serve 呼び出しの引数追随のみ — 本結線は Task 6）

**Interfaces:**
- Consumes: Task 4 の `Event` / `events_from_report`
- Produces:
  - `Op::Listen { node_id: Option<u64>, endpoint: Option<u16>, cluster: Option<String>, attribute: Option<String> }`
  - `server::serve(socket_path: &Path, store_path: PathBuf, native: NativeState, events: broadcast::Sender<Event>)`（第 4 引数追加）
  - `pub(crate) struct ListenFilter { node_id: Option<u64>, endpoint: Option<u16>, cluster: Option<u32>, attribute: Option<u32> }` + `fn matches(&self, ev: &Event) -> bool` + `fn from_op(...) -> Result<ListenFilter, MatError>`
  - listen 応答契約: ack 1 行 `{"id"?, "timestamp", "listening": true}` → 以後フィルタ一致イベントを NDJSON で流し続ける。lag は `{"error":{"kind":"other","detail":"event stream lagged"},"timestamp":...}` を送って切断。

- [ ] **Step 1: 失敗するテストを書く**

protocol.rs tests:

```rust
#[test]
fn listen_parses_with_all_filters_optional() {
    let r = parse(r#"{"op":"listen"}"#);
    assert_eq!(r.op.node_id(), None);
    assert!(matches!(
        r.op,
        Op::Listen { node_id: None, endpoint: None, cluster: None, attribute: None }
    ));
    let r = parse(
        r#"{"op":"listen","node_id":21,"endpoint":1,"cluster":"occupancysensing","attribute":"occupancy"}"#,
    );
    assert!(matches!(
        r.op,
        Op::Listen { node_id: Some(21), endpoint: Some(1), .. }
    ));
}
```

server.rs tests（フィルタ単体）:

```rust
#[test]
fn listen_filter_matches_by_resolved_ids() {
    use crate::subscription::Event;
    let ev = Event {
        node_id: 21, endpoint: 1, cluster: 0x0406, attribute: 0x0000,
        value: serde_json::json!(1), priming: false,
    };
    let f = ListenFilter::from_op(
        &Some(21), &Some(1), &Some("occupancysensing".into()), &Some("occupancy".into()),
    ).unwrap();
    assert!(f.matches(&ev));
    // node 不一致
    let f = ListenFilter::from_op(&Some(22), &None, &None, &None).unwrap();
    assert!(!f.matches(&ev));
    // 全省略 = 全イベント
    let f = ListenFilter::from_op(&None, &None, &None, &None).unwrap();
    assert!(f.matches(&ev));
    // 数値 cluster/attribute も可
    let f = ListenFilter::from_op(&None, &None, &Some("0x0406".into()), &Some("0".into())).unwrap();
    assert!(f.matches(&ev));
    // 未知 cluster 名は parse_error
    let err = ListenFilter::from_op(&None, &None, &Some("nosuch".into()), &None).unwrap_err();
    assert_eq!(err.kind, mat_core::error::ErrorKind::ParseError);
    // 属性名フィルタは cluster 無しでは解決できない（数値なら可）
    let err = ListenFilter::from_op(&None, &None, &None, &Some("occupancy".into())).unwrap_err();
    assert_eq!(err.kind, mat_core::error::ErrorKind::ParseError);
    let f = ListenFilter::from_op(&None, &None, &None, &Some("0".into())).unwrap();
    assert!(f.matches(&ev));
}
```

tests/integration.rs（socket 越しの ack → ストリーム → フィルタ → lag）。既存 `start_matd` を events 付きに変更:

```rust
/// serve に渡す broadcast と、その送信ハンドルを返す版。
async fn start_matd_with_events(
    store_path: PathBuf,
    native: NativeState,
    capacity: usize,
) -> (
    PathBuf,
    tokio::task::JoinHandle<()>,
    tokio::sync::broadcast::Sender<matd::subscription::Event>,
) {
    let socket = std::env::temp_dir().join(format!("matd-test-{}.sock", rand_suffix()));
    let (tx, _rx) = tokio::sync::broadcast::channel(capacity);
    let socket_clone = socket.clone();
    let tx2 = tx.clone();
    let handle = tokio::spawn(async move {
        let _ = matd::server::serve(&socket_clone, store_path, native, tx2).await;
    });
    (socket, handle, tx)
}

fn occupancy_event(node_id: u64) -> matd::subscription::Event {
    matd::subscription::Event {
        node_id,
        endpoint: 1,
        cluster: 0x0406,
        attribute: 0x0000,
        value: serde_json::json!(1),
        priming: false,
    }
}

#[tokio::test]
async fn listen_acks_then_streams_filtered_events() {
    let (_dir, store_path) = make_store();
    let native = NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
    let (socket, handle, tx) =
        start_matd_with_events(store_path, NativeState::Ready(Box::new(native)), 16).await;

    // 接続して listen（node 21 のみ）
    let mut stream = None;
    for _ in 0..250 {
        if let Ok(s) = UnixStream::connect(&socket).await {
            stream = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let stream = stream.expect("connect");
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    write_half
        .write_all(b"{\"id\":9,\"op\":\"listen\",\"node_id\":21}\n")
        .await
        .unwrap();

    // ack 1 行
    let ack: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(ack["listening"], json!(true));
    assert_eq!(ack["id"], json!(9));
    assert!(ack["timestamp"].is_string());

    // フィルタ不一致（node 22）→ 届かない。一致（node 21）→ 届く。
    tx.send(occupancy_event(22)).unwrap();
    tx.send(occupancy_event(21)).unwrap();
    let ev: Value = serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
    assert_eq!(ev["node_id"], json!(21));
    assert_eq!(ev["cluster"], "occupancysensing");
    assert_eq!(ev["attribute"], "occupancy");
    assert_eq!(ev["value"], json!(1));
    assert_eq!(ev["priming"], json!(false));

    handle.abort();
}

#[tokio::test]
async fn lagged_listener_gets_error_line_and_disconnect() {
    let (_dir, store_path) = make_store();
    let native = NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
    // capacity 1: listener が読む前に多数流すと必ず lag する。
    let (socket, handle, tx) =
        start_matd_with_events(store_path, NativeState::Ready(Box::new(native)), 1).await;

    let mut stream = None;
    for _ in 0..250 {
        if let Ok(s) = UnixStream::connect(&socket).await {
            stream = Some(s);
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let stream = stream.expect("connect");
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();
    write_half.write_all(b"{\"op\":\"listen\"}\n").await.unwrap();
    let _ack = lines.next_line().await.unwrap().unwrap();

    // 大量送信で lag を起こす（handler が 1 通処理する間に capacity 超過させる）。
    for _ in 0..64 {
        let _ = tx.send(occupancy_event(21));
    }
    // どこかで lag エラー行が来て、その後 EOF（切断）。
    let mut saw_lag = false;
    while let Some(line) = lines.next_line().await.unwrap() {
        let v: Value = serde_json::from_str(&line).unwrap();
        if v.get("error").is_some() {
            assert_eq!(v["error"]["kind"], "other");
            assert!(v["error"]["detail"].as_str().unwrap().contains("lagged"));
            saw_lag = true;
            break;
        }
    }
    assert!(saw_lag, "expected lag error line");
    // 切断される（次の read は EOF）。
    assert!(lines.next_line().await.unwrap().is_none());

    handle.abort();
}
```

既存 `start_matd` / `start_matd_with_fake` は `start_matd_with_events(..., 16)` を包んで `(socket, handle)` を返す形へ書き換える（既存テスト本文は無改変で通す）。

- [ ] **Step 2: テストが失敗することを確認**

Run: `cargo test -p matd`
Expected: コンパイルエラー（`Op::Listen` / serve 引数 / `ListenFilter` 未定義）

- [ ] **Step 3: 実装**

protocol.rs — `Op` に追加（`Ping` の前あたり）:

```rust
/// イベントストリーム購読（matd 専用 op）。ack 1 行の後、フィルタ一致
/// イベントを同接続へ流し続ける（「1行=1往復」の唯一の例外）。全省略 = 全イベント。
Listen {
    #[serde(default)]
    node_id: Option<u64>,
    #[serde(default)]
    endpoint: Option<u16>,
    #[serde(default)]
    cluster: Option<String>,
    #[serde(default)]
    attribute: Option<String>,
},
```

`Op::node_id()` の `None` 腕へ `Op::Listen { .. }` を追加（listen のフィルタ node は commission 済み検査の対象ではない）。

server.rs — 変更点:

1. `use crate::subscription::Event;` / `use tokio::sync::broadcast;` を追加。
2. `serve` シグネチャに `events: broadcast::Sender<Event>` を追加し、`handle_conn` へ `Arc` 無しの clone で渡す（`broadcast::Sender` は Clone）。
3. `handle_conn`: 行パースを dispatch から前倒しして Listen を分岐:

```rust
async fn handle_conn(
    stream: UnixStream,
    native: Arc<NativeState>,
    store_path: Arc<PathBuf>,
    shutdown: Arc<Notify>,
    events: broadcast::Sender<Event>,
) -> std::io::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        // listen だけは「ack 1 行 + 以後ストリーム」の例外。この接続を占有する。
        if let Ok(req) = serde_json::from_str::<Request>(&line) {
            if let Op::Listen { node_id, endpoint, cluster, attribute } = &req.op {
                let filter = match ListenFilter::from_op(node_id, endpoint, cluster, attribute) {
                    Ok(f) => f,
                    Err(e) => {
                        let mut buf = serde_json::to_vec(&error_response(req.id, &e))
                            .unwrap_or_else(|_| b"{}".to_vec());
                        buf.push(b'\n');
                        write_half.write_all(&buf).await?;
                        write_half.flush().await?;
                        return Ok(());
                    }
                };
                // ack より先に subscribe（ack 直後のイベントを取りこぼさない）。
                let rx = events.subscribe();
                let mut ack = json!({ "timestamp": now_iso8601(), "listening": true });
                if let (Value::Object(map), Some(id)) = (&mut ack, req.id) {
                    map.insert("id".into(), id);
                }
                let mut buf = serde_json::to_vec(&ack).unwrap_or_else(|_| b"{}".to_vec());
                buf.push(b'\n');
                write_half.write_all(&buf).await?;
                write_half.flush().await?;
                return stream_events(rx, filter, &mut lines, &mut write_half).await;
            }
        }
        let (response, is_shutdown) = dispatch(&line, &native, &store_path).await;
        // （以下既存どおり）
        ...
    }
    Ok(())
}
```

（`handle_conn` の呼び出し側 `serve` の `tokio::spawn` に `events.clone()` を渡す。`native` は現行どおり `Arc<NativeState>`。）

4. ストリーム本体とフィルタ:

```rust
/// listen ストリーム: フィルタ一致イベントを NDJSON で流し続ける。lag した
/// listener は黙って欠落させず、エラー行を送って切断する（spec ②）。
/// クライアント切断（EOF）でも抜ける。
async fn stream_events(
    mut rx: broadcast::Receiver<Event>,
    filter: ListenFilter,
    lines: &mut tokio::io::Lines<BufReader<tokio::net::unix::OwnedReadHalf>>,
    write_half: &mut tokio::net::unix::OwnedWriteHalf,
) -> std::io::Result<()> {
    loop {
        tokio::select! {
            ev = rx.recv() => match ev {
                Ok(ev) => {
                    if !filter.matches(&ev) {
                        continue;
                    }
                    let mut buf = serde_json::to_vec(&ev.to_json())
                        .unwrap_or_else(|_| b"{}".to_vec());
                    buf.push(b'\n');
                    write_half.write_all(&buf).await?;
                    write_half.flush().await?;
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(skipped = n, "listen client lagged; disconnecting");
                    let body = json!({
                        "error": { "kind": "other", "detail": "event stream lagged" },
                        "timestamp": now_iso8601(),
                    });
                    let mut buf = serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec());
                    buf.push(b'\n');
                    write_half.write_all(&buf).await?;
                    write_half.flush().await?;
                    return Ok(());
                }
                Err(broadcast::error::RecvError::Closed) => return Ok(()),
            },
            line = lines.next_line() => {
                // クライアント切断（None/Err）でストリーム終了。listen 中の追加
                // リクエスト行は無視する（この op は接続占有の例外）。
                match line {
                    Ok(Some(_)) => continue,
                    _ => return Ok(()),
                }
            }
        }
    }
}

/// listen のイベントフィルタ。リクエストの cluster/attribute 名はここで数値へ
/// 解決して照合する（イベント側は数値を持つ）。属性名は cluster 無しでは解決
/// できない（数値なら可）。
pub(crate) struct ListenFilter {
    node_id: Option<u64>,
    endpoint: Option<u16>,
    cluster: Option<u32>,
    attribute: Option<u32>,
}

impl ListenFilter {
    pub(crate) fn from_op(
        node_id: &Option<u64>,
        endpoint: &Option<u16>,
        cluster: &Option<String>,
        attribute: &Option<String>,
    ) -> Result<Self, MatError> {
        let cluster_id = match cluster {
            None => None,
            Some(c) => Some(mat_core::ids::resolve_cluster(c).ok_or_else(|| {
                MatError::parse_error(format!("unknown cluster name {c:?}; numeric IDs are accepted"))
            })?),
        };
        let attribute_id = match attribute {
            None => None,
            Some(a) => match cluster_id {
                Some(cid) => Some(
                    mat_core::ids::resolve_attribute(cid, a)
                        .ok_or_else(|| {
                            MatError::parse_error(format!(
                                "unknown attribute name {a:?}; numeric IDs are accepted"
                            ))
                        })?
                        .id,
                ),
                None => match mat_core::ids::parse_num(a) {
                    Some(n) => Some(u32::try_from(n).map_err(|_| {
                        MatError::parse_error("attribute id out of range")
                    })?),
                    None => {
                        return Err(MatError::parse_error(
                            "attribute name filter requires a cluster filter (or use a numeric id)",
                        ))
                    }
                },
            },
        };
        Ok(Self {
            node_id: *node_id,
            endpoint: *endpoint,
            cluster: cluster_id,
            attribute: attribute_id,
        })
    }

    pub(crate) fn matches(&self, ev: &Event) -> bool {
        self.node_id.is_none_or(|n| n == ev.node_id)
            && self.endpoint.is_none_or(|e| e == ev.endpoint)
            && self.cluster.is_none_or(|c| c == ev.cluster)
            && self.attribute.is_none_or(|a| a == ev.attribute)
    }
}
```

（`Option::is_none_or` が MSRV/clippy で使えない場合は `map_or(true, ...)` に置き換える。）

5. `run_op` の網羅 match: `Op::Listen` は handle_conn で先取りされるが、防御として `run_op` 冒頭の Ping/Shutdown と同様に到達不能腕を足す（`Op::Listen { .. } => Err(MatError::parse_error("listen must be the streaming path"))`）。

6. main.rs の `server::serve(&socket, store_path, native)` 呼び出しをコンパイルが通る最小変更（`let (events_tx, _) = tokio::sync::broadcast::channel(1024);` を作って渡す）にする。**SubscriptionManager の起動は Task 6。**

- [ ] **Step 4: テスト通過を確認**

Run: `cargo test -p matd`
Expected: 全 PASS（既存 integration テスト含む）

- [ ] **Step 5: Commit**

```bash
git add crates/matd/src/protocol.rs crates/matd/src/server.rs crates/matd/src/main.rs crates/matd/tests/integration.rs
git commit -m "feat(matd): listen op — ack line then filtered NDJSON event stream with lag cut"
```

---

### Task 6: matd main — SubscriptionManager 結線

**Files:**
- Modify: `crates/matd/src/main.rs`
- Modify: `crates/matd/src/server.rs`（serve が `Arc<NativeState>` を受ける形に揃える）

**Interfaces:**
- Consumes: Task 4 `spawn_subscription_manager`、Task 5 の serve 新シグネチャ
- Produces: `server::serve(socket_path, store_path, native: Arc<NativeState>, events)` — main / テストの呼び出しを追随

- [ ] **Step 1: server::serve を `Arc<NativeState>` 受けに変更**

serve 内部の `let native = Arc::new(native);` を削除し、引数を `native: Arc<NativeState>` に。tests/integration.rs の `start_matd_with_events` は `Arc::new(native)` を包んで渡すよう更新。

- [ ] **Step 2: main.rs `serve_daemon` の結線**

native 構築の後（`server::serve` 呼び出しの前）:

```rust
    let native = std::sync::Arc::new(native);
    // listen へのイベント配信路。購読 → broadcast → listen 接続（spec ②）。
    let (events_tx, _events_rx) = tokio::sync::broadcast::channel(1024);
    // 常駐購読は native が使えるときだけ張る（Unavailable なら listen は
    // ack だけ返り、イベントは流れない — `mat fabric init` 後の再起動で解消）。
    let _sub_handles = matd::subscription::spawn_subscription_manager(
        std::sync::Arc::clone(&native),
        store_path.clone(),
        events_tx.clone(),
    );

    server::serve(&socket, store_path, native, events_tx)
        .await
        .map_err(|e| MatError::new(ErrorKind::Other, format!("socket server failed: {e}")))
```

（`spawn_subscription_manager` は `NativeState::Unavailable` を内部で判定して no-op。）

- [ ] **Step 3: ビルド + 全テスト**

Run: `cargo test -p matd && cargo clippy -p matd -- -D warnings`
Expected: PASS / warnings なし

- [ ] **Step 4: Commit**

```bash
git add crates/matd/src/main.rs crates/matd/src/server.rs crates/matd/tests/integration.rs
git commit -m "feat(matd): wire SubscriptionManager into serve startup"
```

---

### Task 7: mat-core — ErrorKind::MatdUnavailable（exit 13）

**Files:**
- Modify: `crates/mat-core/src/error.rs`

**Interfaces:**
- Produces: `ErrorKind::MatdUnavailable`（serde: `"matd_unavailable"`、exit 13）

- [ ] **Step 1: 失敗するテストを書く**

既存 tests に追記:

```rust
#[test]
fn matd_unavailable_is_exit_13_snake_case() {
    assert_eq!(ErrorKind::MatdUnavailable.exit_code(), 13);
    assert_eq!(
        serde_json::to_string(&ErrorKind::MatdUnavailable).unwrap(),
        "\"matd_unavailable\""
    );
}
```

- [ ] **Step 2: 失敗確認**

Run: `cargo test -p mat-core --lib error`
Expected: コンパイルエラー（variant 未定義）

- [ ] **Step 3: 実装**

variant 追加（`Other` の前）:

```rust
    /// matd が居ない / socket が応答しない（matd 専用 op = `mat listen` 用）。
    /// 12 は chip-tool 退役の歴史的欠番のため 13 を使う。
    MatdUnavailable,
```

`exit_code()` に `ErrorKind::MatdUnavailable => 13,` を追加。

- [ ] **Step 4: 通過確認 + Commit**

Run: `cargo test -p mat-core`

```bash
git add crates/mat-core/src/error.rs
git commit -m "feat(core): matd_unavailable error kind (exit 13)"
```

---

### Task 8: mat CLI — `mat listen`

**Files:**
- Modify: `crates/mat/src/cli.rs`
- Modify: `crates/mat/src/resolve.rs`
- Modify: `crates/mat/src/matd_client.rs`
- Modify: `crates/mat/src/main.rs`

**Interfaces:**
- Consumes: Task 7 `ErrorKind::MatdUnavailable`、matd の listen 応答契約（Task 5）
- Produces:
  - `Command::Listen { node_id: Option<NodeRef>, endpoint: Option<EndpointRef>, cluster: Option<String>, attribute: Option<String>, count: u32, timeout_ms: u64 }`
  - `matd_client::listen_request_json(node: Option<u64>, endpoint: Option<u16>, cluster: &Option<String>, attribute: &Option<String>) -> serde_json::Value`
  - `pub fn dispatch_listen(socket: &Path, command: &Command) -> ExitCode` — 接続 → listen リクエスト → ack → イベント行をそのまま stdout へ。count/timeout は mat 側制御。

- [ ] **Step 1: 失敗するテストを書く**

matd_client.rs tests:

```rust
#[test]
fn listen_request_json_omits_absent_filters() {
    assert_eq!(
        listen_request_json(None, None, &None, &None),
        json!({"op":"listen"})
    );
    assert_eq!(
        listen_request_json(
            Some(21), Some(1),
            &Some("occupancysensing".into()), &Some("occupancy".into()),
        ),
        json!({
            "op":"listen","node_id":21,"endpoint":1,
            "cluster":"occupancysensing","attribute":"occupancy"
        })
    );
}
```

resolve.rs tests:

```rust
#[test]
fn listen_resolves_node_alias_and_rejects_endpoint_alias_without_node() {
    let dir = store_with(SAMPLE);
    let cmd = Command::Listen {
        node_id: Some(NodeRef::Alias("living-light".into())),
        endpoint: Some(EndpointRef::Alias("night".into())),
        cluster: None,
        attribute: None,
        count: 1,
        timeout_ms: 60_000,
    };
    match resolve_command(cmd, dir.path()).unwrap() {
        Command::Listen { node_id, endpoint, .. } => {
            assert_eq!(node_id, Some(NodeRef::Id(5)));
            assert_eq!(endpoint, Some(EndpointRef::Id(2)));
        }
        other => panic!("unexpected: {other:?}"),
    }
    // node 無しの endpoint alias は解決不能 → エラー（数値なら可）。
    let cmd = Command::Listen {
        node_id: None,
        endpoint: Some(EndpointRef::Alias("night".into())),
        cluster: None,
        attribute: None,
        count: 1,
        timeout_ms: 60_000,
    };
    assert!(resolve_command(cmd, dir.path()).is_err());
}
```

- [ ] **Step 2: 失敗確認**

Run: `cargo test -p mat --lib`
Expected: コンパイルエラー

- [ ] **Step 3: 実装（cli.rs）**

`Command` に追加（`Diag` の前）:

```rust
    /// matd の常駐 Subscribe が受けたデバイス発の状態変化イベントを流す
    /// （matd 専用 — matd 不在時は `matd_unavailable` / exit 13。direct
    /// fallback は無い）。1 行 1 JSON。`--count` 到達で exit 0、`--timeout-ms`
    /// 経過で打ち切り（0 = 無期限）。0 件で timeout は exit 3。
    Listen {
        /// フィルタ: node_id または node alias（省略 = 全ノード）。
        #[arg(short = 'n', long = "node", value_name = "N|ALIAS")]
        node_id: Option<NodeRef>,
        /// フィルタ: エンドポイント（alias は --node 指定時のみ解決可）。
        #[arg(short = 'e', long, value_name = "EP|ALIAS")]
        endpoint: Option<EndpointRef>,
        /// フィルタ: クラスタ名（chip-tool 表記）または数値 ID。
        #[arg(short = 'c', long, value_name = "NAME")]
        cluster: Option<String>,
        /// フィルタ: 属性名（chip-tool 表記、--cluster 必須）または数値 ID。
        #[arg(short = 'a', long, value_name = "NAME")]
        attribute: Option<String>,
        /// 受信するイベント数（到達で exit 0）。
        #[arg(long, value_name = "N", default_value_t = 1,
              value_parser = clap::value_parser!(u32).range(1..))]
        count: u32,
        /// 打ち切りミリ秒（0 = 無期限）。既定 60000。
        #[arg(long = "timeout-ms", value_name = "T", default_value_t = 60_000)]
        timeout_ms: u64,
    },
```

- [ ] **Step 4: 実装（resolve.rs）**

`resolve_command` に腕を追加:

```rust
        Command::Listen {
            node_id,
            endpoint,
            cluster,
            attribute,
            count,
            timeout_ms,
        } => {
            let node = node_id.map(|n| book.resolve_node(&n)).transpose()?;
            let endpoint = match endpoint {
                None => None,
                Some(e) => Some(match node {
                    Some(n) => EndpointRef::Id(book.resolve_endpoint(n, &e)?),
                    // node 文脈が無いと endpoint alias は解決できない（数値のみ可）。
                    None => match e {
                        EndpointRef::Id(v) => EndpointRef::Id(v),
                        EndpointRef::Alias(a) => {
                            return Err(MatError::new(
                                ErrorKind::Other,
                                format!("endpoint alias {a:?} requires --node"),
                            ))
                        }
                    },
                }),
            };
            Command::Listen {
                node_id: node.map(NodeRef::Id),
                endpoint,
                cluster,
                attribute,
                count,
                timeout_ms,
            }
        }
```

- [ ] **Step 5: 実装（matd_client.rs）**

`to_op` の網羅 match に `Command::Listen { .. } => return Err(unsupported("listen (streaming op; handled before route dispatch)")),` を追加（main が先取りするため実際には到達しない）。

listen 専用経路:

```rust
/// listen リクエスト行を組む（None フィルタは省略）。
fn listen_request_json(
    node: Option<u64>,
    endpoint: Option<u16>,
    cluster: &Option<String>,
    attribute: &Option<String>,
) -> Value {
    let mut op = json!({ "op": "listen" });
    if let Some(n) = node {
        op["node_id"] = json!(n);
    }
    if let Some(e) = endpoint {
        op["endpoint"] = json!(e);
    }
    if let Some(c) = cluster {
        op["cluster"] = json!(c);
    }
    if let Some(a) = attribute {
        op["attribute"] = json!(a);
    }
    op
}

/// `mat listen`: matd へ接続し、ack 後のイベント行をそのまま stdout へ流す。
/// count/timeout は mat 側制御（enl listen と同じ UX）。matd 不在・応答なし・
/// ストリーム途中の matd 落ちは `matd_unavailable`（exit 13）。
pub fn dispatch_listen(socket: &Path, command: &Command) -> ExitCode {
    let Command::Listen {
        node_id,
        endpoint,
        cluster,
        attribute,
        count,
        timeout_ms,
    } = command
    else {
        unreachable!("dispatch_listen called with non-Listen command");
    };
    let op = listen_request_json(
        node_id.as_ref().map(NodeRef::id),
        endpoint.as_ref().map(mat_core::alias::EndpointRef::id),
        cluster,
        attribute,
    );

    let stream = match UnixStream::connect(socket) {
        Ok(s) => s,
        Err(e) => {
            emit_error(
                ErrorKind::MatdUnavailable,
                &format!(
                    "matd not reachable at {} ({e}); `mat listen` requires a running matd",
                    socket.display()
                ),
            );
            return ExitCode::from(ErrorKind::MatdUnavailable.exit_code());
        }
    };

    match run_listen_stream(stream, &op, *count, *timeout_ms) {
        Ok(code) => code,
        Err(detail) => {
            emit_error(ErrorKind::MatdUnavailable, &detail);
            ExitCode::from(ErrorKind::MatdUnavailable.exit_code())
        }
    }
}

/// ack → イベント行ループ。戻り値 Ok(exit code) / Err(detail) = matd 落ち扱い。
fn run_listen_stream(
    mut stream: UnixStream,
    op: &Value,
    count: u32,
    timeout_ms: u64,
) -> Result<ExitCode, String> {
    use std::time::{Duration, Instant};

    let mut line = serde_json::to_vec(op).map_err(|e| format!("failed to encode request: {e}"))?;
    line.push(b'\n');
    stream
        .write_all(&line)
        .map_err(|e| format!("failed to send listen request to matd: {e}"))?;

    let deadline = (timeout_ms > 0).then(|| Instant::now() + Duration::from_millis(timeout_ms));
    let mut reader = BufReader::new(stream);
    let mut received: u32 = 0;
    let mut first = true; // 1 行目は ack（または即エラー）

    loop {
        // 残り時間を socket の read timeout に反映（0 = 無期限）。
        if let Some(dl) = deadline {
            let remaining = dl.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(finish_on_timeout(received));
            }
            reader
                .get_ref()
                .set_read_timeout(Some(remaining))
                .map_err(|e| format!("failed to set read timeout: {e}"))?;
        }
        let mut buf = String::new();
        match reader.read_line(&mut buf) {
            Ok(0) => {
                // EOF = matd がストリーム途中で落ちた（出力済みイベントはそのまま）。
                return Err("matd closed the event stream".to_string());
            }
            Ok(_) => {}
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                return Ok(finish_on_timeout(received));
            }
            Err(e) => return Err(format!("failed to read from matd: {e}")),
        }
        let v: Value = serde_json::from_str(&buf)
            .map_err(|e| format!("matd sent non-JSON line: {e}; body={buf}"))?;
        if let Some(err) = v.get("error") {
            // ack 前のエラー（フィルタ不正等）/ ストリーム中の lag 切断。
            eprintln!("{v}");
            let kind = err
                .get("kind")
                .and_then(|k| serde_json::from_value::<ErrorKind>(k.clone()).ok())
                .unwrap_or(ErrorKind::Other);
            return Ok(ExitCode::from(kind.exit_code()));
        }
        if first {
            // ack 行 `{"listening":true}` は出力せず読み捨てる。
            first = false;
            if v.get("listening").is_none() {
                return Err(format!("matd listen ack malformed: {v}"));
            }
            continue;
        }
        println!("{v}");
        received += 1;
        if received >= count {
            return Ok(ExitCode::SUCCESS);
        }
    }
}

/// timeout 打ち切り: 0 件なら timeout(exit 3)、1 件以上なら成功（enl 準拠）。
fn finish_on_timeout(received: u32) -> ExitCode {
    if received == 0 {
        emit_error(ErrorKind::Timeout, "no events received within --timeout-ms");
        ExitCode::from(ErrorKind::Timeout.exit_code())
    } else {
        ExitCode::SUCCESS
    }
}
```

- [ ] **Step 6: 実装（main.rs）**

alias 解決の後・既存の経路解決 match の前に listen を先取り:

```rust
    // listen は初の matd 専用 op（direct fallback なし — 常駐なしに購読は成立
    // しない）。経路解決の socket だけ流用し、Direct（MAT_MATD=falsy）は
    // matd_unavailable で即エラー。
    if let Command::Listen { .. } = &command {
        return match matd_client::resolve_route(
            &args.matd,
            std::env::var_os("MAT_MATD_SOCKET"),
            std::env::var_os("MAT_MATD"),
        ) {
            matd_client::Route::Forced(socket) | matd_client::Route::Auto(socket) => {
                matd_client::dispatch_listen(&socket, &command)
            }
            matd_client::Route::Direct => {
                mat_core::error::MatError::new(
                    ErrorKind::MatdUnavailable,
                    "`mat listen` requires matd (MAT_MATD=0 disables it)",
                )
                .emit();
                ExitCode::from(ErrorKind::MatdUnavailable.exit_code())
            }
        };
    }
```

（既存の `match matd_client::resolve_route` ブロックはそのまま。listen は到達しない。）

- [ ] **Step 7: テスト通過を確認**

Run: `cargo test -p mat --lib && cargo build -p mat`
Expected: PASS

- [ ] **Step 8: Commit**

```bash
git add crates/mat/src/cli.rs crates/mat/src/resolve.rs crates/mat/src/matd_client.rs crates/mat/src/main.rs
git commit -m "feat(mat): listen subcommand — matd-only event stream with count/timeout control"
```

---

### Task 9: mat バイナリ統合テスト（fake matd ストリーム）

**Files:**
- Create: `crates/mat/tests/listen.rs`

**Interfaces:**
- Consumes: `mat listen` CLI（Task 8）、matd listen 応答契約（Task 5）
- Produces: exit code 契約（0 / 3 / 13）とストリーム挙動の釘打ち

- [ ] **Step 1: テストを書く**

```rust
//! `mat listen` の統合テスト。fake matd（tmp の UnixListener + ストリーム応答）で
//! count / timeout / exit code / matd 落ちの契約を釘打ちする（spec テスト方針 3）。
//! 実 matd も実デバイスも不要。

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::thread::JoinHandle;

use assert_cmd::Command;
use predicates::prelude::*;
use tempfile::TempDir;

const ACK: &str = "{\"timestamp\":\"2026-07-20T00:00:00+09:00\",\"listening\":true}\n";
const EVENT: &str = "{\"timestamp\":\"2026-07-20T00:00:01+09:00\",\"node_id\":21,\"endpoint\":1,\"cluster\":\"occupancysensing\",\"attribute\":\"occupancy\",\"value\":1,\"priming\":false}\n";

fn mat_listen(socket: &std::path::Path, extra: &[&str]) -> Command {
    let store = TempDir::new().unwrap();
    let mut c = Command::cargo_bin("mat").unwrap();
    c.env("MAT_IFACE", "lo")
        .env("MAT_MATD_SOCKET", socket)
        .env_remove("MAT_MATD")
        .arg("--store")
        .arg(store.into_path()) // TempDir は listen 終了まで生かすため into_path でリーク
        .arg("listen")
        .args(extra);
    c
}

/// fake matd: listen リクエスト 1 行を読み、ack + イベント N 行を返す。
/// `hold` = 送信後も接続を開いたまま維持する秒数（timeout 系テスト用）。
fn spawn_fake_matd_stream(socket: PathBuf, events: usize, hold_ms: u64) -> JoinHandle<String> {
    let listener = UnixListener::bind(&socket).unwrap();
    std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut req = String::new();
        BufReader::new(stream.try_clone().unwrap())
            .read_line(&mut req)
            .unwrap();
        stream.write_all(ACK.as_bytes()).unwrap();
        for _ in 0..events {
            stream.write_all(EVENT.as_bytes()).unwrap();
        }
        stream.flush().unwrap();
        if hold_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(hold_ms));
        }
        req
    })
}

#[test]
fn listen_count_reached_exits_zero_with_events_on_stdout() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    let matd = spawn_fake_matd_stream(socket.clone(), 2, 500);

    mat_listen(&socket, &["--count", "2", "--timeout-ms", "5000"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"occupancy\"").count(2));

    let req = matd.join().unwrap();
    assert!(req.contains("\"op\":\"listen\""), "request line: {req}");
}

#[test]
fn listen_filters_are_forwarded_in_request() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    let matd = spawn_fake_matd_stream(socket.clone(), 1, 200);

    mat_listen(
        &socket,
        &["--node", "21", "--cluster", "occupancysensing", "--count", "1"],
    )
    .assert()
    .success();

    let req = matd.join().unwrap();
    assert!(req.contains("\"node_id\":21"), "request line: {req}");
    assert!(
        req.contains("\"cluster\":\"occupancysensing\""),
        "request line: {req}"
    );
}

#[test]
fn listen_timeout_without_events_exits_3() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    // ack のみ・イベント 0・接続は維持 → mat 側の timeout で打ち切り。
    let _matd = spawn_fake_matd_stream(socket.clone(), 0, 3000);

    mat_listen(&socket, &["--timeout-ms", "300"])
        .assert()
        .code(3)
        .stderr(predicate::str::contains("timeout"));
}

#[test]
fn listen_timeout_with_partial_events_exits_zero() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    // 1 件だけ流して沈黙 → count=2 未達のまま timeout → 1 件以上なので exit 0。
    let _matd = spawn_fake_matd_stream(socket.clone(), 1, 3000);

    mat_listen(&socket, &["--count", "2", "--timeout-ms", "300"])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"occupancy\"").count(1));
}

#[test]
fn listen_without_matd_exits_13() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock"); // bind しない

    mat_listen(&socket, &[])
        .assert()
        .code(13)
        .stderr(predicate::str::contains("matd_unavailable"));
}

#[test]
fn listen_stream_cut_by_matd_death_exits_13_keeping_output() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    // 1 件流して即クローズ（hold 0）→ count=2 未達で EOF → exit 13、出力済みは残る。
    let _matd = spawn_fake_matd_stream(socket.clone(), 1, 0);

    mat_listen(&socket, &["--count", "2", "--timeout-ms", "5000"])
        .assert()
        .code(13)
        .stdout(predicate::str::contains("\"occupancy\"").count(1))
        .stderr(predicate::str::contains("matd_unavailable"));
}

#[test]
fn listen_with_mat_matd_disabled_exits_13() {
    let dir = TempDir::new().unwrap();
    let socket = dir.path().join("matd.sock");
    let _listener = UnixListener::bind(&socket).unwrap(); // 居ても使わない

    mat_listen(&socket, &[])
        .env("MAT_MATD", "0")
        .assert()
        .code(13)
        .stderr(predicate::str::contains("matd_unavailable"));
}
```

- [ ] **Step 2: 失敗確認 → 通過確認**

Run: `cargo test -p mat --test listen`
Expected: Task 8 完了済みなら PASS。落ちる場合は mat 側実装のバグ — テストではなく実装を直す。

- [ ] **Step 3: Commit**

```bash
git add crates/mat/tests/listen.rs
git commit -m "test(mat): listen exit-code contract against fake streaming matd"
```

---

### Task 10: docs + 0.25.0 + 最終チェック

**Files:**
- Modify: `README.md` / `ARCHITECTURE.md` / `CLAUDE.md` / `Cargo.toml`（workspace version 0.24.0 → 0.25.0）/ `Cargo.lock`

- [ ] **Step 1: README 更新**

- サブコマンド一覧 / usage に `mat listen` を追加（スペック §③ の書式・利用形をそのまま。casa の while ループ例も載せる）:
  ```
  mat listen [--node <id|alias>] [--endpoint <n>] [--cluster <name>] [--attribute <name>]
             [--count <N>] [--timeout-ms <T>]
  ```
- "Errors and exit codes" 表に `matd_unavailable` / **13** の行を追加（「matd 不在/応答なし。`mat listen` 専用。12 は歴史的欠番」）。
- matd 節: 常駐 Subscribe（wildcard、MinInterval 0 / MaxCeiling 3600s / KeepSubscriptions false、backoff 5s→5min、MaxInterval×1.5 死亡判定）、`listen` op（「1行=1往復」の唯一の例外、lag 切断）、イベント形式（`priming` フラグ、scalar のみ）を記述。
- matd 専用 op / direct-only op の一覧に listen（matd 専用）を追記。

- [ ] **Step 2: ARCHITECTURE.md 更新**

- matd の op 列挙に `listen` を追加し「初の matd 専用 op（direct fallback なし）」と明記。
- Phase 5 後の追記節（または matd 節）に本機能の 1 段落記録: 購読は専用ソケット + 専用 CASE（demux 全面改修はしない構造判断）、イベント配信は broadcast、スコープ外（EventReport / ICD / subscriptions.toml）は将来。

- [ ] **Step 3: CLAUDE.md 更新**

- stderr / kind 一覧の例に `matd_unavailable` を追加。
- exit code 短表に `13` matd 不在 を追加。
- スコープ注意（「セッションキャッシュ、購読、freshness は matd の役割」）はそのまま成立 — `mat listen` は薄い口である旨を 1 行補足。

- [ ] **Step 4: version bump**

`Cargo.toml` の `[workspace.package] version = "0.24.0"` → `"0.25.0"`。`cargo build` で `Cargo.lock` を更新。

- [ ] **Step 5: 最終チェック**

Run: `task check`
Expected: fmt:check / clippy (-D warnings) / 全テスト PASS

- [ ] **Step 6: Commit**

```bash
git add README.md ARCHITECTURE.md CLAUDE.md Cargo.toml Cargo.lock
git commit -m "docs+chore(release): mat listen / matd resident subscribe, 0.25.0"
```

---

## Self-Review 記録

- **Spec coverage:** ①im.rs(Task1)/session.rs ポンプ+専用ソケット構造判断(Task2,3)/購読パラメータ(Task3 定数) ②SubscriptionManager+backoff+ログ規律(Task4)/broadcast+lag 切断(Task5)/イベント形式+priming+scalar限定(Task4)/listen op(Task5)/状態なし(Task4 実装なし=満たす) ③mat listen CLI+count/timeout+alias(Task8) ④matd_unavailable/exit13+途中落ち(Task7,8) ⑤テスト1=Task2、テスト2=Task5、テスト3=Task9、テスト4(実機E2E)=**計画外**（スペックどおり実装後・デプロイ後の別セッション）。
- **明示的な残課題:** 実機 E2E（Nanoleaf で `mat on` → listen にイベント）はデプロイ後の別セッションで実施。
- **Type consistency:** `SubscribeConn::next_report` は `ReportDataMessage` を返し、matd `events_from_report` がそれを消費（Task3→4）。`Event` は matd 定義で server/main/テストが共有（Task4→5→6→9 は JSON 契約経由）。`ErrorKind::MatdUnavailable` は Task7 で定義し Task8 が使用。
