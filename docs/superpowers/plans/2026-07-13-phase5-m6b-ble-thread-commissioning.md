# Phase 5 M6b: BTP/BLE コミッショニング + Thread dataset 実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** mat-controller に工場出荷状態デバイスの native commission（BLE 発見 → BTP → PASE → attestation/NOC → Thread dataset 書き込み → CASE）を実装し、玄関ライト実機 E2E で実証する。

**Architecture:** BTP は「もう一つの土管」として transport 層に enum で差し込む（BTP 経路は MRP 無効）。BTP プロトコル（handshake/セグメント/ACK/keepalive）は自前実装、GATT アクセスのみ bluer。PASE / im / session の暗号・プロトコルロジックは無変更。NetworkCommissioning ステップは commissioning.rs の既存ステップマシンに挿入する。

**Tech Stack:** Rust (tokio), bluer 0.17（feature gate `ble` の下）, 既存 mat-controller モジュール群。

**Spec:** `docs/superpowers/specs/2026-07-13-phase5-m6b-ble-thread-commissioning-design.md`

## Global Constraints

- 作業ブランチは `matter-controller`（worktree `.claude/worktrees/phase5-m1-controller-core`）。**main へマージしない**。
- 本番 `mat` / `matd` の経路・出力・挙動は**一切変更しない**（ライブラリ + E2E のみ）。
- **bluer 依存は cargo feature `ble` の下に隔離する。** デフォルト無効。既存の aarch64-musl クロスビルド（mat/matd 本番デプロイ）を壊さないため。bluer は libdbus (C) にリンクするので、feature off でリンク要求が消えることを必ず確認する。
- リポジトリは public。実 IP・実 node_id・証明書・Thread dataset を**コミットしない**（RFC 5737 / ダミー値のみ）。
- 各タスク末尾で `task check`（fmt:check + clippy -D warnings + test）を通してからコミット。
- コミットメッセージ末尾: `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>`
- 参照実装（connectedhomeip / rust-matc）は**読んで突合してよいがコピーしない**。
- BTP 定数（spec §4.19）: conn establishment timeout 5s / ACK 受領 timeout 15s / keepalive（idle 時 standalone ack）2.5s / 提案 window 6 / サポート BTP version 4 のみ。

---

### Task 1: Transport enum（Udp | Reliable）と MRP ゲーティング

BTP を差し込むための土台。`Transport` enum を導入し、既存の
`Arc<UdpTransport>` / `&UdpTransport` を使う session/exchange/pase/case/
commissioning の配線を `Arc<Transport>` / `&Transport` に置き換える。
Reliable 経路（BTP）は MRP を使わない: R フラグを立てない・再送しない・
standalone ack を送らない。

**Files:**
- Modify: `crates/mat-controller/src/transport.rs`（enum 追加）
- Modify: `crates/mat-controller/src/exchange.rs`（`&Transport` 化 + ゲーティング）
- Modify: `crates/mat-controller/src/session.rs`（`Arc<Transport>` 化 + ゲーティング）
- Modify: `crates/mat-controller/src/pase.rs` / `src/case.rs` / `src/commissioning.rs`（型の置換）
- Modify: `crates/matd/src/native.rs` ほか `grep -rn 'Arc<UdpTransport>\|&UdpTransport' crates/` で見つかる全呼び出し側（live テスト含む）
- Test: `crates/mat-controller/src/transport.rs` / `src/exchange.rs` の `#[cfg(test)]`

**Interfaces:**
- Produces: `transport::Transport`（enum）、`transport::ReliableChannel`、
  `transport::RELIABLE_PEER: SocketAddr`、`Transport::is_reliable()`、
  `ReliableChannel::new(tx, rx)`、`ReliableChannel::pair() -> (Transport, Transport)`
- 既存 `pase::establish` / `case::establish` の第一引数が `Arc<Transport>` になる（Task 6 が BTP で使う）。`commission_on_network` の公開シグネチャは `Arc<UdpTransport>` のまま維持し内部で wrap（M6a 呼び出し側の互換）。

- [ ] **Step 1: transport.rs に enum と ReliableChannel を追加（テスト先行）**

`transport.rs` 末尾の tests に追加（`ReliableChannel::pair` の往復と `is_reliable`）:

```rust
#[tokio::test]
async fn reliable_pair_roundtrips_messages() {
    let (a, b) = ReliableChannel::pair();
    a.send_to(b"ping", RELIABLE_PEER).await.unwrap();
    let mut buf = [0u8; MAX_DATAGRAM];
    let (n, from) = b.recv_from(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"ping");
    assert_eq!(from, RELIABLE_PEER);
    assert!(a.is_reliable());
}
```

- [ ] **Step 2: 落ちることを確認**

Run: `cargo test -p mat-controller transport:: 2>&1 | tail -5`
Expected: コンパイルエラー（`ReliableChannel` 未定義）

- [ ] **Step 3: 実装**

`transport.rs` に追加:

```rust
use std::net::{IpAddr, Ipv6Addr};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

/// Reliable 経路（BTP 等）で recv_from が返す固定の擬似 peer アドレス。
/// exchange 層の from==peer スクリーニングを素通しするための marker で、
/// 実在の宛先ではない。
pub const RELIABLE_PEER: SocketAddr =
    SocketAddr::new(IpAddr::V6(Ipv6Addr::LOCALHOST), 5541);

/// 順序・信頼性を transport 自身が保証する経路（BTP）のメッセージ土管。
/// チャネルの 1 要素 = Matter メッセージ 1 通（データグラム等価）。
pub struct ReliableChannel {
    tx: mpsc::Sender<Vec<u8>>,
    rx: Mutex<mpsc::Receiver<Vec<u8>>>,
}

impl ReliableChannel {
    pub fn new(tx: mpsc::Sender<Vec<u8>>, rx: mpsc::Receiver<Vec<u8>>) -> Self {
        Self { tx, rx: Mutex::new(rx) }
    }

    /// クロス接続されたループバック対（テスト用）。
    pub fn pair() -> (Transport, Transport) {
        let (atx, brx) = mpsc::channel(8);
        let (btx, arx) = mpsc::channel(8);
        (
            Transport::Reliable(Self::new(atx, arx)),
            Transport::Reliable(Self::new(btx, brx)),
        )
    }
}

/// セッション層が使うメッセージ transport。Udp は MRP あり、Reliable
/// （BTP）は transport 自身が信頼性を持つため MRP なし（spec §4.12.2）。
pub enum Transport {
    Udp(Arc<UdpTransport>),
    Reliable(ReliableChannel),
}

impl Transport {
    pub fn is_reliable(&self) -> bool {
        matches!(self, Transport::Reliable(_))
    }

    pub async fn send_to(&self, buf: &[u8], dest: SocketAddr) -> io::Result<()> {
        match self {
            Transport::Udp(u) => u.send_to(buf, dest).await,
            Transport::Reliable(c) => c
                .tx
                .send(buf.to_vec())
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "reliable channel closed")),
        }
    }

    pub async fn recv_from(&self, buf: &mut [u8]) -> io::Result<(usize, SocketAddr)> {
        match self {
            Transport::Udp(u) => u.recv_from(buf).await,
            Transport::Reliable(c) => {
                let msg = c.rx.lock().await.recv().await.ok_or_else(|| {
                    io::Error::new(io::ErrorKind::BrokenPipe, "reliable channel closed")
                })?;
                let n = msg.len().min(buf.len());
                buf[..n].copy_from_slice(&msg[..n]);
                Ok((n, RELIABLE_PEER))
            }
        }
    }

    pub fn local_addr(&self) -> io::Result<SocketAddr> {
        match self {
            Transport::Udp(u) => u.local_addr(),
            Transport::Reliable(_) => Ok(RELIABLE_PEER),
        }
    }
}
```

`ReliableChannel::pair` のテストは `Transport` を返すので Step 1 のテストはその形に合わせる（上のコードが正）。

- [ ] **Step 4: transport テスト green を確認**

Run: `cargo test -p mat-controller transport:: -- --nocapture 2>&1 | tail -5`
Expected: PASS

- [ ] **Step 5: exchange.rs のゲーティングのテストを書く（先行）**

`exchange.rs` tests に追加。Reliable 上では (a) 送信メッセージに R フラグが立たない、(b) 再送しない、(c) 実応答が返る、(d) needs_ack=false の受信に standalone ack を返さない:

```rust
#[tokio::test]
async fn reliable_transport_disables_mrp() {
    use crate::transport::{ReliableChannel, Transport, RELIABLE_PEER};
    let (a, b) = ReliableChannel::pair();
    let mut ex = UnsecuredExchange::new(&a, RELIABLE_PEER);
    let exchange_id = ex.exchange_id();

    let peer_task = tokio::spawn(async move {
        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, _) = b.recv_from(&mut buf).await.unwrap();
        let (h, off) = MessageHeader::decode(&buf[..n]).unwrap();
        let (p, _) = ProtocolHeader::decode(&buf[off..n]).unwrap();
        assert!(!p.needs_ack, "R flag must not be set on reliable transport");
        // 実応答（R フラグなし・ack 相乗りなし——BTP では MRP 自体が無い）
        let reply = {
            let rh = MessageHeader {
                session_id: 0,
                security_flags: 0,
                message_counter: 4242,
                source_node_id: None,
                destination: Destination::None,
            };
            let rp = ProtocolHeader {
                initiator: false,
                needs_ack: false,
                acked_counter: None,
                opcode: OPCODE_STATUS_REPORT,
                exchange_id,
                protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
                vendor_id: None,
            };
            let mut buf = rh.encoded();
            rp.encode(&mut buf);
            buf
        };
        b.send_to(&reply, RELIABLE_PEER).await.unwrap();
        // 相手からの standalone ack が来ないこと（=チャネルに後続なし）
        let mut buf2 = [0u8; MAX_DATAGRAM];
        let more =
            tokio::time::timeout(Duration::from_millis(200), b.recv_from(&mut buf2)).await;
        assert!(more.is_err(), "no standalone ack expected on reliable transport");
        (h.message_counter, ())
    });

    let res = ex
        .send_reliable(PROTOCOL_ID_SECURE_CHANNEL, 0x99, b"", &fast_cfg())
        .await
        .unwrap()
        .expect("real response");
    assert_eq!(res.proto.opcode, OPCODE_STATUS_REPORT);
    peer_task.await.unwrap();
}
```

- [ ] **Step 6: exchange.rs / session.rs を Transport 化して実装**

機械的置換 + ゲーティング:

1. `exchange.rs`: `use crate::transport::{Transport, MAX_DATAGRAM};`、
   `UnsecuredExchange<'t> { transport: &'t Transport, ... }`。
   `build()` に `let needs_ack = needs_ack && !self.transport.is_reliable();` を先頭に追加。
   `screen()` の 2 箇所の `send_standalone_ack` を `if proto.needs_ack && !self.transport.is_reliable()` 相当に（reliable では peer は R を立てないはずだが防御的に）。
   `send_reliable()` 冒頭に reliable 分岐:

```rust
if self.transport.is_reliable() {
    // BTP: transport が信頼性を持つ。1 回送って実応答を待つだけ。
    let (datagram, our_counter) = self.build(protocol_id, opcode, false, None, payload);
    self.last_sent_counter = Some(our_counter);
    self.transport.send_to(&datagram, self.peer).await?;
    let budget = total_budget(cfg);
    return match self.recv(budget).await {
        Ok(msg) => Ok(Some(msg)),
        Err(e) => Err(e),
    };
}
```

   `total_budget` は exchange.rs 内のフリー関数として追加（MRP 全リトライ相当の時間 = Σ initial·backoff^i）:

```rust
/// MRP の全リトライを使い切るまでの待ち時間合計。reliable transport では
/// 「同じ体感タイムアウト」で実応答を待つ予算として使う。
fn total_budget(cfg: &MrpConfig) -> Duration {
    let mut total = Duration::ZERO;
    let mut interval = cfg.initial_interval;
    for _ in 0..=cfg.max_retries {
        total += interval;
        interval = interval.mul_f64(cfg.backoff);
    }
    total
}
```

2. `session.rs`: `SecureSession { transport: Arc<Transport>, ... }` に置換。
   `send_reliable`（session.rs:262 付近）と `recv`（:312 付近）にも同じゲーティングを適用（送信時 `needs_ack` を落とす・reliable では再送ループを 1 回送信 + budget 待ちに・standalone ack 送出を抑止）。exchange.rs と対称の変更なので、同 pattern を session の secure 経路にあわせて写す。

3. `pase.rs` / `case.rs`: `establish` の第一引数 `Arc<UdpTransport>` → `Arc<Transport>`（内部の使用箇所は send_to/recv_from 同名なので型のみ）。

4. `commissioning.rs`: `commission_on_network(transport: Arc<UdpTransport>, ...)` は**公開シグネチャ維持**。冒頭で `let transport: Arc<Transport> = Arc::new(Transport::Udp(Arc::clone(&transport)));` に wrap し、pase/case へは wrap 済みを渡す。`open_commissioning_window` は SecureSession しか触らないので無変更。

5. 呼び出し側の修正: `grep -rn 'pase::establish\|case::establish\|SecureSession::new' crates/ --include='*.rs'` で全箇所を列挙し、`Arc<UdpTransport>` を渡している所を `Arc::new(Transport::Udp(udp))` に置換（`crates/matd/src/native.rs`、`crates/mat-controller/tests/live_*.rs`、`src/group.rs` が該当し得る。group の multicast 送信は `UdpTransport` 直使用のままでよい——session を作る箇所だけ wrap）。

- [ ] **Step 7: 全テスト green を確認**

Run: `cargo test -p mat-controller 2>&1 | tail -5 && cargo test -p matd 2>&1 | tail -5`
Expected: 全 PASS（live 系 `#[ignore]` は skip のまま）

- [ ] **Step 8: task check + コミット**

Run: `task check`
Expected: PASS

```bash
git add -A crates/
git commit -m "feat(controller): Transport enum(Udp|Reliable)導入とreliable経路のMRP無効化 (M6b Task1)"
```

---

### Task 2: BTP パケット codec（btp.rs 純関数部）

BTP（Matter spec §4.19）のフレーム組立/解釈を純関数 + 小さな状態機械で実装。GATT into/out のバイト列だけを扱い、I/O なし。

**Files:**
- Create: `crates/mat-controller/src/btp.rs`
- Modify: `crates/mat-controller/src/lib.rs`（`pub mod btp;` 追加）
- Test: `btp.rs` 内 `#[cfg(test)]`

**Interfaces:**
- Produces:
  - `btp::FLAG_B/FLAG_E/FLAG_A/FLAG_M/FLAG_H`（u8 定数）
  - `btp::handshake_request(window: u8) -> [u8; 9]`
  - `btp::BtpParams { version: u8, segment_size: u16, window_size: u8 }`
  - `btp::decode_handshake_response(&[u8]) -> Result<BtpParams, BtpError>`
  - `btp::Packet { ack: Option<u8>, seq: Option<u8>, msg_len: Option<u16>, ending: bool, beginning: bool, payload: Vec<u8> }` と `Packet::decode(&[u8]) -> Result<Packet, BtpError>`
  - `btp::encode_data_packet(seq: u8, ack: Option<u8>, position: SegmentPos, msg_len: Option<u16>, payload: &[u8]) -> Vec<u8>`（`SegmentPos { First{ending:bool}, Middle, Last }`）
  - `btp::encode_standalone_ack(seq: u8, ack: u8) -> [u8; 3]`
  - `btp::segment_payload_capacity(segment_size: u16, first: bool, with_ack: bool) -> usize`
  - `btp::Reassembler`（`push(&mut self, pkt: &Packet) -> Result<Option<Vec<u8>>, BtpError>`）
  - `btp::BtpError`（`Handshake(&'static str)` / `Protocol(&'static str)` / `Closed` / `Timeout(&'static str)` / `Gatt(String)`、`Display` 実装）

- [ ] **Step 1: 失敗するテストを書く**

`btp.rs` を作り、フレーム形式のテストを先に書く。バイト列は spec §4.19.3（フラグ: B=0x01, E=0x04, A=0x08, M=0x20, H=0x40。ヘッダ順: flags, [ack], [seq], [msg_len LE]）に基づく:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handshake_request_bytes() {
        // H|M|B|E=0x65, opcode 0x6C, versions[4,0,0,0](v4のみ), MTU=0(不明), window
        assert_eq!(
            handshake_request(6),
            [0x65, 0x6C, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x06]
        );
    }

    #[test]
    fn handshake_response_decodes() {
        // flags,opcode,version(下位nibble),segment_size=244 LE,window=4
        let p = decode_handshake_response(&[0x65, 0x6C, 0x04, 0xF4, 0x00, 0x04]).unwrap();
        assert_eq!(p.version, 4);
        assert_eq!(p.segment_size, 244);
        assert_eq!(p.window_size, 4);
        // version 不一致は拒否
        assert!(decode_handshake_response(&[0x65, 0x6C, 0x03, 0xF4, 0x00, 0x04]).is_err());
        // 短すぎ
        assert!(decode_handshake_response(&[0x65, 0x6C, 0x04]).is_err());
    }

    #[test]
    fn single_segment_packet_roundtrip() {
        // B|E、seq=0、msg_len=5、ackなし
        let bytes = encode_data_packet(0, None, SegmentPos::First { ending: true }, Some(5), b"hello");
        assert_eq!(&bytes[..4], &[0x05, 0x00, 0x05, 0x00]);
        assert_eq!(&bytes[4..], b"hello");
        let pkt = Packet::decode(&bytes).unwrap();
        assert!(pkt.beginning && pkt.ending);
        assert_eq!(pkt.seq, Some(0));
        assert_eq!(pkt.msg_len, Some(5));
        assert_eq!(pkt.payload, b"hello");
    }

    #[test]
    fn packet_with_ack_places_ack_before_seq() {
        let bytes = encode_data_packet(7, Some(3), SegmentPos::First { ending: true }, Some(2), b"ab");
        // flags B|E|A = 0x0D, ack=3, seq=7, len=2
        assert_eq!(&bytes[..5], &[0x0D, 0x03, 0x07, 0x02, 0x00]);
    }

    #[test]
    fn standalone_ack_bytes() {
        assert_eq!(encode_standalone_ack(9, 5), [0x08, 0x05, 0x09]);
    }

    #[test]
    fn reassembles_three_segments() {
        let msg: Vec<u8> = (0u8..=99).collect(); // 100 bytes
        let mut r = Reassembler::new();
        let p1 = Packet::decode(&encode_data_packet(
            0, None, SegmentPos::First { ending: false }, Some(100), &msg[..40],
        ))
        .unwrap();
        let p2 = Packet::decode(&encode_data_packet(1, None, SegmentPos::Middle, None, &msg[40..80]))
            .unwrap();
        let p3 = Packet::decode(&encode_data_packet(2, None, SegmentPos::Last, None, &msg[80..]))
            .unwrap();
        assert!(r.push(&p1).unwrap().is_none());
        assert!(r.push(&p2).unwrap().is_none());
        assert_eq!(r.push(&p3).unwrap().unwrap(), msg);
    }

    #[test]
    fn reassembler_rejects_length_mismatch() {
        let mut r = Reassembler::new();
        let p = Packet::decode(&encode_data_packet(
            0, None, SegmentPos::First { ending: true }, Some(10), b"short",
        ))
        .unwrap();
        assert!(r.push(&p).is_err());
    }

    #[test]
    fn capacity_accounts_for_header() {
        // first + ack: flags(1)+ack(1)+seq(1)+len(2)=5 バイトのヘッダ
        assert_eq!(segment_payload_capacity(244, true, true), 239);
        // 継続 + ackなし: flags(1)+seq(1)=2
        assert_eq!(segment_payload_capacity(244, false, false), 242);
    }
}
```

- [ ] **Step 2: 落ちることを確認**

Run: `cargo test -p mat-controller btp:: 2>&1 | tail -3`
Expected: コンパイルエラー

- [ ] **Step 3: 実装**

```rust
//! BTP (Bluetooth Transport Protocol, spec §4.19) framing.
//!
//! ここは純関数 + Reassembler のみ（I/O なし）。セッション状態機械は同
//! モジュールの actor（Task 3）が持つ。BTP version 4 のみ対応。

pub const FLAG_B: u8 = 0x01; // Beginning Segment
pub const FLAG_E: u8 = 0x04; // Ending Segment
pub const FLAG_A: u8 = 0x08; // Ack number present
pub const FLAG_M: u8 = 0x20; // Management (handshake opcode follows)
pub const FLAG_H: u8 = 0x40; // Handshake
const MGMT_OPCODE_HANDSHAKE: u8 = 0x6C;
pub const BTP_VERSION: u8 = 4;

#[derive(Debug)]
pub enum BtpError {
    Handshake(&'static str),
    Protocol(&'static str),
    Closed,
    Timeout(&'static str),
    Gatt(String),
}
// Display: "btp handshake error: {}", "btp protocol error: {}",
// "btp channel closed", "btp timeout: {}", "gatt error: {}" の形で実装。
// std::error::Error も impl。

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BtpParams {
    pub version: u8,
    pub segment_size: u16,
    pub window_size: u8,
}

pub fn handshake_request(window: u8) -> [u8; 9] {
    [FLAG_H | FLAG_M | FLAG_B | FLAG_E, MGMT_OPCODE_HANDSHAKE,
     BTP_VERSION, 0, 0, 0, // 対応 version は 4 のみ（先頭 nibble スロット）
     0, 0,                 // 観測 ATT_MTU 不明 = 0
     window]
}

pub fn decode_handshake_response(buf: &[u8]) -> Result<BtpParams, BtpError> {
    if buf.len() < 6 {
        return Err(BtpError::Handshake("short handshake response"));
    }
    if buf[0] != (FLAG_H | FLAG_M | FLAG_B | FLAG_E) || buf[1] != MGMT_OPCODE_HANDSHAKE {
        return Err(BtpError::Handshake("not a handshake response"));
    }
    let version = buf[2] & 0x0F;
    if version != BTP_VERSION {
        return Err(BtpError::Handshake("unsupported btp version"));
    }
    let segment_size = u16::from_le_bytes([buf[3], buf[4]]);
    if segment_size < 20 {
        return Err(BtpError::Handshake("segment size too small"));
    }
    let window_size = buf[5];
    if window_size == 0 {
        return Err(BtpError::Handshake("zero window"));
    }
    Ok(BtpParams { version, segment_size, window_size })
}

#[derive(Debug, Clone, Copy)]
pub enum SegmentPos {
    First { ending: bool },
    Middle,
    Last,
}

pub fn encode_data_packet(
    seq: u8,
    ack: Option<u8>,
    position: SegmentPos,
    msg_len: Option<u16>,
    payload: &[u8],
) -> Vec<u8> {
    let mut flags = 0u8;
    match position {
        SegmentPos::First { ending } => {
            flags |= FLAG_B;
            if ending {
                flags |= FLAG_E;
            }
        }
        SegmentPos::Middle => {}
        SegmentPos::Last => flags |= FLAG_E,
    }
    if ack.is_some() {
        flags |= FLAG_A;
    }
    let mut out = Vec::with_capacity(5 + payload.len());
    out.push(flags);
    if let Some(a) = ack {
        out.push(a);
    }
    out.push(seq);
    if let Some(len) = msg_len {
        debug_assert!(matches!(position, SegmentPos::First { .. }));
        out.extend_from_slice(&len.to_le_bytes());
    }
    out.extend_from_slice(payload);
    out
}

pub fn encode_standalone_ack(seq: u8, ack: u8) -> [u8; 3] {
    [FLAG_A, ack, seq]
}

pub fn segment_payload_capacity(segment_size: u16, first: bool, with_ack: bool) -> usize {
    let header = 1 + usize::from(with_ack) + 1 + if first { 2 } else { 0 };
    usize::from(segment_size).saturating_sub(header)
}

#[derive(Debug, Clone, PartialEq)]
pub struct Packet {
    pub beginning: bool,
    pub ending: bool,
    pub ack: Option<u8>,
    pub seq: Option<u8>,
    pub msg_len: Option<u16>,
    pub payload: Vec<u8>,
}

impl Packet {
    pub fn decode(buf: &[u8]) -> Result<Packet, BtpError> {
        let mut i = 0usize;
        let flags = *buf.first().ok_or(BtpError::Protocol("empty packet"))?;
        i += 1;
        if flags & FLAG_H != 0 {
            return Err(BtpError::Protocol("unexpected handshake packet"));
        }
        let ack = if flags & FLAG_A != 0 {
            let a = *buf.get(i).ok_or(BtpError::Protocol("truncated ack"))?;
            i += 1;
            Some(a)
        } else {
            None
        };
        // データ/ackパケットは常に sequenced（spec §4.19.3.5）
        let seq = *buf.get(i).ok_or(BtpError::Protocol("truncated seq"))?;
        i += 1;
        let beginning = flags & FLAG_B != 0;
        let msg_len = if beginning {
            let b = buf.get(i..i + 2).ok_or(BtpError::Protocol("truncated len"))?;
            i += 2;
            Some(u16::from_le_bytes([b[0], b[1]]))
        } else {
            None
        };
        Ok(Packet {
            beginning,
            ending: flags & FLAG_E != 0,
            ack,
            seq: Some(seq),
            msg_len,
            payload: buf[i..].to_vec(),
        })
    }
}

/// 受信セグメントを 1 つの Matter メッセージへ再構成する。
#[derive(Default)]
pub struct Reassembler {
    buf: Vec<u8>,
    expected: Option<u16>,
}

impl Reassembler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, pkt: &Packet) -> Result<Option<Vec<u8>>, BtpError> {
        if pkt.beginning {
            if self.expected.is_some() {
                return Err(BtpError::Protocol("beginning segment mid-message"));
            }
            self.expected = Some(pkt.msg_len.ok_or(BtpError::Protocol("no msg_len"))?);
            self.buf.clear();
        } else if self.expected.is_none() {
            if pkt.payload.is_empty() {
                return Ok(None); // standalone ack / keepalive
            }
            return Err(BtpError::Protocol("continuation without beginning"));
        }
        self.buf.extend_from_slice(&pkt.payload);
        let expected = self.expected.expect("set above");
        if usize::from(expected) < self.buf.len() {
            self.expected = None;
            return Err(BtpError::Protocol("message longer than declared"));
        }
        if pkt.ending {
            if self.buf.len() != usize::from(expected) {
                self.expected = None;
                return Err(BtpError::Protocol("message length mismatch"));
            }
            self.expected = None;
            return Ok(Some(std::mem::take(&mut self.buf)));
        }
        Ok(None)
    }
}
```

`lib.rs` に `pub mod btp;` を追加（アルファベット順で `attestation` の後）。

注意: standalone ack（payload 空・B/E なし）を `Reassembler::push` に通すと `Ok(None)` になる設計（actor 側の分岐を減らす）。テスト `reassembler_rejects_length_mismatch` の「short」ケースは E フラグ到達時に長さ不一致で Err になる。

- [ ] **Step 4: green 確認**

Run: `cargo test -p mat-controller btp:: 2>&1 | tail -3`
Expected: 全 PASS

- [ ] **Step 5: connectedhomeip と突合（フォーマット検算）**

connectedhomeip の `src/ble/BtpEngine.cpp` / `src/ble/tests/TestBtpEngine.cpp` を WebFetch などで参照し、(a) handshake request/response のバイト配置、(b) データパケットのヘッダ順（flags→ack→seq→len）、(c) standalone ack の形、の 3 点が上記実装と一致することを確認。差異があればテストベクタごと修正（コピーはしない、値の検算のみ）。確認結果をコミットメッセージに一行残す。

- [ ] **Step 6: task check + コミット**

```bash
task check
git add crates/mat-controller/src/btp.rs crates/mat-controller/src/lib.rs
git commit -m "feat(controller): BTPパケットcodec(handshake/segment/ack)とReassembler (M6b Task2)"
```

---

### Task 3: BTP セッション actor（handshake・ウィンドウ・keepalive）

GATT リンク（チャネル対）の上で BTP セッションを運転する actor。成立後は
`Transport::Reliable` を返し、以後 Matter メッセージ 1 通 = チャネル 1 要素
で送受できる。

**Files:**
- Modify: `crates/mat-controller/src/btp.rs`（actor 追加）
- Test: `btp.rs` 内 `#[cfg(test)]`（fake peripheral）

**Interfaces:**
- Consumes: Task 2 の codec、Task 1 の `ReliableChannel` / `Transport`
- Produces:
  - `btp::GattLink { pub writes: tokio::sync::mpsc::Sender<Vec<u8>>, pub indications: tokio::sync::mpsc::Receiver<Vec<u8>> }`
    — writes: GATT C1 への write（cap 1、送信完了駆動で背圧）。indications: C2 の indication。
  - `btp::connect(link: GattLink, window: u8) -> Result<(BtpParams, crate::transport::Transport), BtpError>`
    — handshake を行い、actor task を spawn して `Transport::Reliable` を返す。
  - 定数 `btp::PROPOSED_WINDOW: u8 = 6` / `btp::ACK_TIMEOUT: Duration = 15s` / `btp::KEEPALIVE_INTERVAL: Duration = 2.5s` / `btp::HANDSHAKE_TIMEOUT: Duration = 5s`

- [ ] **Step 1: fake peripheral とテストを書く（先行）**

`btp.rs` tests に、チャネルの向こう側で BTP peripheral を演じるヘルパと 4 本のテスト:

```rust
/// テスト用 BTP peripheral。GattLink の裏側を演じる。
/// 返り値: (client 用 GattLink, peripheral 操作ハンドル)
struct FakePeripheral {
    from_client: tokio::sync::mpsc::Receiver<Vec<u8>>, // C1 writes
    to_client: tokio::sync::mpsc::Sender<Vec<u8>>,     // C2 indications
    tx_seq: u8,
    reasm: Reassembler,
}

fn fake_link() -> (GattLink, FakePeripheral) {
    let (wtx, wrx) = tokio::sync::mpsc::channel(1);
    let (itx, irx) = tokio::sync::mpsc::channel(8);
    (
        GattLink { writes: wtx, indications: irx },
        FakePeripheral { from_client: wrx, to_client: itx, tx_seq: 0, reasm: Reassembler::new() },
    )
}

impl FakePeripheral {
    /// handshake request を受けて response を返す。
    async fn do_handshake(&mut self, segment_size: u16, window: u8) {
        let req = self.from_client.recv().await.expect("handshake request");
        assert_eq!(req, handshake_request(PROPOSED_WINDOW));
        let mut resp = vec![0x65, 0x6C, BTP_VERSION];
        resp.extend_from_slice(&segment_size.to_le_bytes());
        resp.push(window);
        self.to_client.send(resp).await.unwrap();
    }

    /// client からの書き込みを 1 メッセージ再構成するまで読む。
    /// 返り値: (メッセージ, 最後に受けた seq)
    async fn recv_message(&mut self) -> (Vec<u8>, u8) {
        loop {
            let frame = self.from_client.recv().await.expect("frame");
            let pkt = Packet::decode(&frame).unwrap();
            let seq = pkt.seq.unwrap();
            if let Some(msg) = self.reasm.push(&pkt).unwrap() {
                return (msg, seq);
            }
        }
    }

    async fn send_ack(&mut self, ack: u8) {
        let seq = self.tx_seq;
        self.tx_seq = self.tx_seq.wrapping_add(1);
        self.to_client.send(encode_standalone_ack(seq, ack).to_vec()).await.unwrap();
    }

    /// メッセージを segment_size で分割して indication する（ack 相乗り付き）。
    async fn send_message(&mut self, msg: &[u8], segment_size: u16, ack: Option<u8>) {
        // encode_data_packet + segment_payload_capacity で分割。
        // 最初のフレームのみ ack を相乗り。実装は client 側 actor の送信と同型。
        let mut off = 0usize;
        let mut first = true;
        while first || off < msg.len() {
            let cap = segment_payload_capacity(segment_size, first, ack.is_some() && first);
            let end = (off + cap).min(msg.len());
            let ending = end == msg.len();
            let pos = if first {
                SegmentPos::First { ending }
            } else if ending {
                SegmentPos::Last
            } else {
                SegmentPos::Middle
            };
            let seq = self.tx_seq;
            self.tx_seq = self.tx_seq.wrapping_add(1);
            let frame = encode_data_packet(
                seq,
                if first { ack } else { None },
                pos,
                if first { Some(msg.len() as u16) } else { None },
                &msg[off..end],
            );
            self.to_client.send(frame).await.unwrap();
            off = end;
            first = false;
        }
    }
}

#[tokio::test]
async fn btp_connect_handshakes_and_roundtrips_small_message() {
    let (link, mut p) = fake_link();
    let peripheral = tokio::spawn(async move {
        p.do_handshake(244, 4).await;
        let (msg, seq) = p.recv_message().await;
        assert_eq!(msg, b"ping-message");
        p.send_message(b"pong-message", 244, Some(seq)).await;
        p
    });
    let (params, t) = connect(link, PROPOSED_WINDOW).await.unwrap();
    assert_eq!(params.segment_size, 244);
    t.send_to(b"ping-message", crate::transport::RELIABLE_PEER).await.unwrap();
    let mut buf = [0u8; 1280];
    let (n, _) = t.recv_from(&mut buf).await.unwrap();
    assert_eq!(&buf[..n], b"pong-message");
    peripheral.await.unwrap();
}

#[tokio::test]
async fn btp_segments_large_message() {
    // segment_size 30 で 100 バイト送ると複数フレームに割れて再構成できる
    let (link, mut p) = fake_link();
    let msg: Vec<u8> = (0u8..100).collect();
    let expect = msg.clone();
    let peripheral = tokio::spawn(async move {
        p.do_handshake(30, 8).await;
        let (m, seq) = p.recv_message().await;
        assert_eq!(m, expect);
        p.send_ack(seq).await;
    });
    let (_, t) = connect(link, PROPOSED_WINDOW).await.unwrap();
    t.send_to(&msg, crate::transport::RELIABLE_PEER).await.unwrap();
    peripheral.await.unwrap();
}

#[tokio::test]
async fn btp_send_blocks_on_window_until_ack() {
    // window=2: 3 フレーム目は ack が来るまで GATT write に出てこない
    let (link, mut p) = fake_link();
    let msg: Vec<u8> = (0u8..60).collect(); // segment 30 → 3 フレーム
    let peripheral = tokio::spawn(async move {
        p.do_handshake(30, 2).await;
        let f1 = p.from_client.recv().await.unwrap();
        let f2 = p.from_client.recv().await.unwrap();
        // 3 枚目はまだ来ない（ack 前）
        let blocked =
            tokio::time::timeout(std::time::Duration::from_millis(300), p.from_client.recv())
                .await;
        assert!(blocked.is_err(), "third frame must wait for ack");
        let s2 = Packet::decode(&f2).unwrap().seq.unwrap();
        p.send_ack(s2).await;
        let f3 = p.from_client.recv().await.unwrap();
        for f in [f1, f2, f3] {
            let pkt = Packet::decode(&f).unwrap();
            if let Some(m) = p.reasm.push(&pkt).unwrap() {
                assert_eq!(m.len(), 60);
            }
        }
    });
    let (_, t) = connect(link, PROPOSED_WINDOW).await.unwrap();
    t.send_to(&msg, crate::transport::RELIABLE_PEER).await.unwrap();
    peripheral.await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn btp_sends_keepalive_ack_when_idle() {
    let (link, mut p) = fake_link();
    let peripheral = tokio::spawn(async move {
        p.do_handshake(244, 4).await;
        // 何もしないで待つ → keepalive standalone ack が来るはず
        let frame = p.from_client.recv().await.expect("keepalive");
        let pkt = Packet::decode(&frame).unwrap();
        assert!(pkt.payload.is_empty());
        assert!(pkt.ack.is_some());
    });
    let (_, _t) = connect(link, PROPOSED_WINDOW).await.unwrap();
    tokio::time::sleep(KEEPALIVE_INTERVAL + std::time::Duration::from_millis(100)).await;
    peripheral.await.unwrap();
}
```

（`start_paused` は tokio の `test-util` feature が必要。dev-dependencies の tokio に `"test-util"` を足す。）

- [ ] **Step 2: 落ちることを確認**

Run: `cargo test -p mat-controller btp:: 2>&1 | tail -3`
Expected: コンパイルエラー（GattLink / connect 未定義）

- [ ] **Step 3: actor を実装**

```rust
use std::time::Duration;
use tokio::sync::mpsc;
use crate::transport::{ReliableChannel, Transport};

pub const PROPOSED_WINDOW: u8 = 6;
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
pub const ACK_TIMEOUT: Duration = Duration::from_secs(15);
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(2500);

/// GATT リンクの土管。ble.rs（feature "ble"）が実体を、テストが fake を作る。
pub struct GattLink {
    /// C1 への write。cap 1 のチャネルにして GATT write 完了で背圧をかける。
    pub writes: mpsc::Sender<Vec<u8>>,
    /// C2 からの indication。
    pub indications: mpsc::Receiver<Vec<u8>>,
}

/// BTP handshake を行い、セッション actor を spawn して Transport を返す。
pub async fn connect(mut link: GattLink, window: u8) -> Result<(BtpParams, Transport), BtpError> {
    link.writes
        .send(handshake_request(window).to_vec())
        .await
        .map_err(|_| BtpError::Closed)?;
    let resp = tokio::time::timeout(HANDSHAKE_TIMEOUT, link.indications.recv())
        .await
        .map_err(|_| BtpError::Timeout("handshake response"))?
        .ok_or(BtpError::Closed)?;
    let params = decode_handshake_response(&resp)?;

    let (app_tx, actor_out_rx) = mpsc::channel::<Vec<u8>>(4); // app → actor（送信）
    let (actor_in_tx, app_rx) = mpsc::channel::<Vec<u8>>(4); // actor → app（受信）
    tokio::spawn(run_session(link, params, actor_out_rx, actor_in_tx));
    Ok((params, Transport::Reliable(ReliableChannel::new(app_tx, app_rx))))
}

struct SessionState {
    tx_seq: u8,             // 次に使う自分の sequence
    peer_acked: u8,         // peer が ack 済みの自分の最新 seq（初期: 0xFF=なし相当）
    unacked: u8,            // 未 ack の自分のフレーム数
    last_rx_seq: u8,        // 受信した最新の peer seq
    pending_ack: bool,      // peer へ ack を返す義務があるか
    reasm: Reassembler,
}

async fn run_session(
    mut link: GattLink,
    params: BtpParams,
    mut out_rx: mpsc::Receiver<Vec<u8>>,
    in_tx: mpsc::Sender<Vec<u8>>,
) {
    let mut st = SessionState {
        tx_seq: 0,
        peer_acked: 0,
        unacked: 0,
        last_rx_seq: 0,
        pending_ack: false,
        reasm: Reassembler::new(),
    };
    let mut keepalive = tokio::time::interval(KEEPALIVE_INTERVAL);
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    keepalive.reset();
    loop {
        tokio::select! {
            biased;
            ind = link.indications.recv() => {
                let Some(frame) = ind else { break }; // リンク切断
                let Ok(pkt) = Packet::decode(&frame) else {
                    tracing::warn!("btp: undecodable frame — dropping session");
                    break;
                };
                if let Some(a) = pkt.ack {
                    // a..=直前 tx_seq-1 のうち ack された分を勘定
                    let newly = a.wrapping_sub(st.peer_acked);
                    st.unacked = st.unacked.saturating_sub(newly);
                    st.peer_acked = a;
                }
                if let Some(s) = pkt.seq {
                    st.last_rx_seq = s;
                    st.pending_ack = true;
                }
                match st.reasm.push(&pkt) {
                    Ok(Some(msg)) => {
                        if in_tx.send(msg).await.is_err() { break; }
                        // 即時 ack（実装簡素化: 受信メッセージ完成ごと）
                        if send_standalone(&mut link, &mut st).await.is_err() { break; }
                        keepalive.reset();
                    }
                    Ok(None) => {}
                    Err(e) => { tracing::warn!(error=%e, "btp reassembly failed"); break; }
                }
            }
            msg = out_rx.recv() => {
                let Some(msg) = msg else { break }; // アプリ側 drop → 終了
                if send_message(&mut link, &mut st, &params, &msg).await.is_err() { break; }
                keepalive.reset();
            }
            _ = keepalive.tick() => {
                if send_standalone(&mut link, &mut st).await.is_err() { break; }
            }
        }
    }
    // actor 終了 = チャネル閉鎖 → Transport 側は BrokenPipe を観測する
}

async fn send_standalone(link: &mut GattLink, st: &mut SessionState) -> Result<(), BtpError> {
    let seq = st.tx_seq;
    st.tx_seq = st.tx_seq.wrapping_add(1);
    st.unacked += 1;
    st.pending_ack = false;
    link.writes
        .send(encode_standalone_ack(seq, st.last_rx_seq).to_vec())
        .await
        .map_err(|_| BtpError::Closed)
}

async fn send_message(
    link: &mut GattLink,
    st: &mut SessionState,
    params: &BtpParams,
    msg: &[u8],
) -> Result<(), BtpError> {
    let mut off = 0usize;
    let mut first = true;
    while first || off < msg.len() {
        // ウィンドウ満杯なら ack を待つ
        while st.unacked >= params.window_size {
            let frame = tokio::time::timeout(ACK_TIMEOUT, link.indications.recv())
                .await
                .map_err(|_| BtpError::Timeout("window ack"))?
                .ok_or(BtpError::Closed)?;
            let pkt = Packet::decode(&frame)?;
            if let Some(a) = pkt.ack {
                let newly = a.wrapping_sub(st.peer_acked);
                st.unacked = st.unacked.saturating_sub(newly);
                st.peer_acked = a;
            }
            if let Some(s) = pkt.seq {
                st.last_rx_seq = s;
                st.pending_ack = true;
            }
            // ウィンドウ待ち中に受けたデータも取りこぼさない
            if let Ok(Some(m)) = st.reasm.push(&pkt) {
                tracing::debug!(len = m.len(), "btp: message received during send");
                // 送信完了後に配送するとフロー逆転するので actor 設計上ここで
                // in_tx を持たない。実装では send_message に in_tx を渡し配送する。
            }
        }
        let with_ack = st.pending_ack && first;
        let cap = segment_payload_capacity(params.segment_size, first, with_ack);
        let end = (off + cap).min(msg.len());
        let ending = end == msg.len();
        let pos = if first {
            SegmentPos::First { ending }
        } else if ending {
            SegmentPos::Last
        } else {
            SegmentPos::Middle
        };
        let seq = st.tx_seq;
        st.tx_seq = st.tx_seq.wrapping_add(1);
        st.unacked += 1;
        let ack = if with_ack { st.pending_ack = false; Some(st.last_rx_seq) } else { None };
        let frame = encode_data_packet(
            seq, ack, pos,
            if first { Some(msg.len() as u16) } else { None },
            &msg[off..end],
        );
        link.writes.send(frame).await.map_err(|_| BtpError::Closed)?;
        off = end;
        first = false;
    }
    Ok(())
}
```

実装ノート:
- `send_message` 中に受信したデータメッセージの配送のため、`send_message` には `in_tx: &mpsc::Sender<Vec<u8>>` も渡す（上のコメント参照。コード整理は実装者に委ねるが、**ウィンドウ待ち中の受信データを捨てない**ことがテスト対象外でも必須要件）。
- `peer_acked` の初期値と `wrapping_sub` の勘定は「最初の ack を受けるまで unacked が減らない」ことだけ守れればよい。u8 wrap（seq 255→0）はユニットテスト追加推奨。
- keepalive も seq を消費し unacked に数える（spec どおり standalone ack は sequenced）。

- [ ] **Step 4: green 確認（4 テスト + 既存）**

Run: `cargo test -p mat-controller btp:: 2>&1 | tail -5`
Expected: 全 PASS

- [ ] **Step 5: task check + コミット**

```bash
task check
git add -A crates/mat-controller
git commit -m "feat(controller): BTPセッションactor(ウィンドウ/ACK/keepalive)とTransport::Reliable接続 (M6b Task3)"
```

---

### Task 4: NetworkCommissioning TLV + Thread dataset 解析 + CommissionError 拡張

**Files:**
- Modify: `crates/mat-controller/src/commissioning.rs`
- Test: `commissioning.rs` 内 `#[cfg(test)]`

**Interfaces:**
- Consumes: 既存 `tlv::{Writer, Reader, Tag}` の idiom（`encode_arm_fail_safe` 等と同型）
- Produces:
  - `CLUSTER_NETWORK_COMMISSIONING: u32 = 0x0031`
  - `CMD_ADD_OR_UPDATE_THREAD: u32 = 0x03`（resp NetworkConfigResponse 0x05）
  - `CMD_CONNECT_NETWORK: u32 = 0x06`（resp ConnectNetworkResponse 0x07）
  - `encode_add_or_update_thread_network(dataset: &[u8], breadcrumb: u64) -> Vec<u8>`
  - `encode_connect_network(network_id: &[u8], breadcrumb: u64) -> Vec<u8>`
  - `decode_network_config_response(fields: &[u8]) -> Result<(u8, Option<String>), CommissionError>`
  - `decode_connect_network_response(fields: &[u8]) -> Result<(u8, Option<String>), CommissionError>`
  - `thread_ext_pan_id(dataset: &[u8]) -> Option<[u8; 8]>`
  - `CommissionError::Ble { step: &'static str, detail: String }` と `CommissionError::NetworkConfig { step: &'static str, status: u8, debug_text: Option<String> }`

- [ ] **Step 1: 失敗するテストを書く**

```rust
#[test]
fn thread_dataset_ext_pan_id_extracts_type2() {
    // MeshCoP TLV: ActiveTimestamp(14,len8) + ExtPanId(2,len8) + Channel(0,len3)
    let mut ds = vec![0x0E, 0x08, 0, 0, 0, 0, 0, 1, 0, 0];
    ds.extend_from_slice(&[0x02, 0x08, 0xDE, 0xAD, 0x00, 0xBE, 0xEF, 0x00, 0xCA, 0xFE]);
    ds.extend_from_slice(&[0x00, 0x03, 0x00, 0x00, 0x0F]);
    assert_eq!(
        thread_ext_pan_id(&ds),
        Some([0xDE, 0xAD, 0x00, 0xBE, 0xEF, 0x00, 0xCA, 0xFE])
    );
    // ExtPanId なし / 壊れた TLV は None
    assert_eq!(thread_ext_pan_id(&ds[..10]), None);
    assert_eq!(thread_ext_pan_id(&[0x02, 0x09, 0x00]), None);
}

#[test]
fn network_commissioning_encoders_shape() {
    // AddOrUpdateThreadNetwork {0: dataset, 1: breadcrumb}
    let f = encode_add_or_update_thread_network(&[0xAA, 0xBB], 3);
    let mut r = tlv::Reader::new(&f);
    // 既存 decode 系テストと同様に Reader で struct{ctx0: bytes, ctx1: uint} を検証
    // （encode_arm_fail_safe の既存テストの書き方に合わせる）
    assert!(!f.is_empty());
    let _ = r; // Reader での検証は既存 idiom を踏襲して書く
    // ConnectNetwork {0: networkID, 1: breadcrumb}
    let f2 = encode_connect_network(&[1, 2, 3, 4, 5, 6, 7, 8], 4);
    assert!(!f2.is_empty());
}

#[test]
fn connect_network_response_decodes_status_and_text() {
    // {0: status=0, 1: "ok", 2: errorValue} を Writer で作って decode
    let mut w = tlv::Writer::new();
    w.start_struct(tlv::Tag::Anonymous);
    w.put_uint(tlv::Tag::Context(0), 0);
    w.put_str(tlv::Tag::Context(1), "ok");
    w.put_int(tlv::Tag::Context(2), 0); // put_int が無ければ put_uint で可
    w.end_container();
    let (status, text) = decode_connect_network_response(&w.finish()).unwrap();
    assert_eq!(status, 0);
    assert_eq!(text.as_deref(), Some("ok"));
}
```

（`network_commissioning_encoders_shape` の Reader 検証は既存の `decode_*` テスト群の idiom を開いて同じ形で埋める。丸め・簡略化しないこと。）

- [ ] **Step 2: 落ちることを確認**

Run: `cargo test -p mat-controller thread_dataset -- --nocapture 2>&1 | tail -3`
Expected: コンパイルエラー

- [ ] **Step 3: 実装**

commissioning.rs のクラスタ定数群の並びに追加:

```rust
// --- Network Commissioning cluster (spec §11.9) ---

pub const CLUSTER_NETWORK_COMMISSIONING: u32 = 0x0031;
pub const CMD_ADD_OR_UPDATE_THREAD: u32 = 0x03; // resp NetworkConfigResponse 0x05
pub const CMD_CONNECT_NETWORK: u32 = 0x06; // resp ConnectNetworkResponse 0x07
```

builders 群（`encode_remove_fabric` の後ろ）:

```rust
/// AddOrUpdateThreadNetwork（spec §11.9.7.3）: `{0: OperationalDataset, 1:
/// Breadcrumb}`。dataset は OTBR の `dataset active -x` が返す Thread TLV
/// 生バイト列そのまま。
pub fn encode_add_or_update_thread_network(dataset: &[u8], breadcrumb: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(0), dataset);
    w.put_uint(Tag::Context(1), breadcrumb);
    w.end_container();
    w.finish()
}

/// ConnectNetwork（spec §11.9.7.9）: `{0: NetworkID, 1: Breadcrumb}`。Thread
/// の NetworkID は dataset 中の Extended PAN ID（8 バイト）。
pub fn encode_connect_network(network_id: &[u8], breadcrumb: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_bytes(Tag::Context(0), network_id);
    w.put_uint(Tag::Context(1), breadcrumb);
    w.end_container();
    w.finish()
}
```

decode 側は既存 `decode_commissioning_status_response`（{0: code, 1: text} 形）の実装を開き、同じ Reader idiom で `{0: NetworkingStatus, 1: DebugText(optional), ...}` を読む 2 関数を実装（NetworkConfigResponse は tag2=NetworkIndex、ConnectNetworkResponse は tag2=ErrorValue を読み飛ばす）。

Thread dataset 解析（純関数、commissioning.rs 内）:

```rust
/// Thread operational dataset（MeshCoP TLV 列）から Extended PAN ID
/// （type 2, len 8）を取り出す。ConnectNetwork の NetworkID に使う。
pub fn thread_ext_pan_id(dataset: &[u8]) -> Option<[u8; 8]> {
    let mut i = 0usize;
    while i + 2 <= dataset.len() {
        let (t, l) = (dataset[i], usize::from(dataset[i + 1]));
        let end = i.checked_add(2)?.checked_add(l)?;
        if end > dataset.len() {
            return None;
        }
        if t == 2 && l == 8 {
            return dataset[i + 2..end].try_into().ok();
        }
        i = end;
    }
    None
}
```

CommissionError 拡張（enum + Display の両方）:

```rust
    /// BLE / BTP 層の失敗（scan / connect / gatt / btp）。
    Ble {
        step: &'static str,
        detail: String,
    },
    /// NetworkCommissioning 応答の NetworkingStatus が成功 (0) でない。
    NetworkConfig {
        step: &'static str,
        status: u8,
        debug_text: Option<String>,
    },
```

Display:

```rust
            CommissionError::Ble { step, detail } => {
                write!(f, "commissioning: ble {step}: {detail}")
            }
            CommissionError::NetworkConfig { step, status, debug_text } => {
                write!(f, "commissioning: {step} NetworkingStatus 0x{status:02X}")?;
                if let Some(t) = debug_text {
                    write!(f, " ({t})")?;
                }
                Ok(())
            }
```

- [ ] **Step 4: green 確認**

Run: `cargo test -p mat-controller commissioning:: 2>&1 | tail -3`
Expected: 全 PASS

- [ ] **Step 5: task check + コミット**

```bash
task check
git add crates/mat-controller/src/commissioning.rs
git commit -m "feat(controller): NetworkCommissioning TLVとThread dataset XPAN抽出、CommissionError拡張 (M6b Task4)"
```

---

### Task 4.5: attestation 強化（M6a 最終レビュー持ち越しの解消、ユーザー承認 2026-07-13）

M6a whole-branch レビューの follow-up: DAC↔PAI の VID/PID 整合・PAI の
cA=true・証明書有効期間が未検証。M6b は実機の本物 DAC を検証するため、ここで
閉じる。**方針（M6a spec 決定 3 の哲学を踏襲）: チェーン構造の欠陥（VID/PID
不整合・cA 不正）= 厳格（失敗 = 中止）、有効期間 = warn 継続**（時計ずれ・
特殊 notAfter 運用でコミッショニングを壊さない）。

**Files:**
- Modify: `crates/mat-controller/src/x509.rs`（basicConstraints cA と Validity の解析追加）
- Modify: `crates/mat-controller/src/attestation.rs`（検証追加）
- Test: 両ファイルの `#[cfg(test)]`（fixtures の SDK テスト証明書を利用）

**Interfaces:**
- Consumes: 既存 `x509::parse_x509` の `ParsedCert`（`vid: Option<u16>` / `pid: Option<u16>` / `issuer` / `subject` は既存）、`attestation::verify_device_attestation` と fixtures 証明書
- Produces: `ParsedCert` に `is_ca: Option<bool>`（basicConstraints 拡張、OID 2.5.29.19。拡張なし = None）と `not_before: Option<String>` / `not_after: Option<String>`（Time の生文字列、UTCTime/GeneralizedTime どちらでも文字列のまま保持）を追加。`AttestationError::Chain` の既存変種を流用（新しい静的メッセージを増やすだけ）。

要求（`verify_device_attestation` のチェーン検証部に追加）:
1. **厳格**: `dac.vid` と `pai.vid` は両方 Some かつ一致（spec §6.2.2.2）。不一致/欠落 = `Chain("dac/pai vid mismatch")`。
2. **厳格**: `pai.pid` が Some の場合 `dac.pid` と一致。PAI に PID が無いのは合法（省略可）なのでスキップ。
3. **厳格**: PAI と PAA は `is_ca == Some(true)`（basicConstraints cA=true 必須、spec §6.2.2.1）。DAC は cA=false または拡張なしであること（cA=true なら拒否）。
4. **warn**: 現在時刻（`std::time::SystemTime`）が DAC/PAI/PAA の有効期間外なら `tracing::warn!` で継続。notAfter `99991231235959Z`（GeneralizedTime）は「無期限」なので警告しない。時刻文字列の解釈は UTCTime（YYMMDDHHMMSSZ、YY<50→20YY）と GeneralizedTime（YYYYMMDDHHMMSSZ）の 2 形のみ対応、他形式は「解釈不能 = warn して継続」。

テスト（fixtures の DAC/PAI/PAA で）:
- 正常系: SDK テスト証明書一式が新チェックを通過する（既存の verify 正常系がそのまま green であること = 回帰確認）。
- `is_ca` / `not_before` / `not_after` が SDK 証明書で期待値どおり解析される（x509.rs ユニット）。
- 失敗系: PAI を DAC と VID 違いの別証明書に差し替え（fixtures 内の別 VID 証明書、無ければ x509 テスト用に合成 DER を既存テスト手法で作る）→ `Chain` エラー。cA 検証は「DAC を PAI の位置に渡す」ことで cA=false 拒否を確認できる。

実装後 `task check` → コミット:

```bash
git add crates/mat-controller/src/x509.rs crates/mat-controller/src/attestation.rs
git commit -m "feat(controller): attestation強化 — DAC/PAI VID-PID整合・cA検証(厳格)と有効期間(warn) (M6b Task4.5)"
```

---

### Task 5: ble.rs（bluer アダプタ、feature "ble"）+ preflight scan example

**Files:**
- Modify: `crates/mat-controller/Cargo.toml`（feature + deps）
- Create: `crates/mat-controller/src/ble.rs`
- Modify: `crates/mat-controller/src/lib.rs`
- Create: `crates/mat-controller/examples/ble-scan.rs`

**Interfaces:**
- Consumes: Task 3 の `GattLink`
- Produces（すべて `#[cfg(feature = "ble")]`）:
  - `ble::MATTER_BLE_SERVICE: uuid::Uuid`（0000fff6-...）
  - `ble::Commissionable { pub discriminator: u16, pub vendor_id: u16, pub product_id: u16, pub addr: bluer::Address }`
  - `ble::parse_matter_service_data(&[u8]) -> Option<Commissionable の中身>`（addr 以外。**cfg なしの純関数**にして WSL でもテスト可能に）
  - `ble::find_commissionable(adapter: &bluer::Adapter, discriminator: u16, timeout: Duration) -> Result<bluer::Device, BtpError>`
  - `ble::open_link(device: &bluer::Device) -> Result<(GattLink, BleConnection), BtpError>`
  - `ble::BleConnection`（drop/明示 `disconnect()` で GATT 切断 + pump task 停止）

- [ ] **Step 1: Cargo feature とダミーモジュールを作る**

`crates/mat-controller/Cargo.toml`:

```toml
[features]
# BLE commissioning (bluer は libdbus にリンクするため隔離。
# 本番 mat/matd の musl クロスビルドはこの feature を使わない)
ble = ["dep:bluer", "dep:futures-util", "dep:uuid"]

[dependencies]
bluer = { version = "0.17", features = ["bluetoothd"], optional = true }
futures-util = { version = "0.3", default-features = false, optional = true }
uuid = { version = "1", optional = true }
```

（tokio の dev-dependencies に `"test-util"` が Task 3 で入っていなければここで確認。）

`lib.rs`:

```rust
#[cfg(feature = "ble")]
pub mod ble;
```

ただし `parse_matter_service_data` と `Commissionable`（bluer 型を含まない形に変える: `addr` を外し disc/vid/pid のみ）は cfg 外でテストしたいので、**service data パーサだけは `btp.rs` ではなく `ble.rs` に置きつつ、`ble.rs` を `#[cfg(feature = "ble")]` で丸ごと隔離し、パーサの単体テストは `--features ble` で回す**方針で可（WSL に libdbus-1-dev を入れれば `cargo test --features ble` が通る。入らない場合はパーサを commissioning.rs 側へ移して cfg なしにする判断を実装者がしてよい——テストが常時回る配置を優先）。

- [ ] **Step 2: service data パーサのテストを書く**

```rust
#[test]
fn parses_matter_commissionable_service_data() {
    // opcode 0x00, disc/adv版=LE16(下位12bit=0xB47), VID=0x125D, PID=0x0055, flags=0
    let sd = [0x00, 0x47, 0x0B, 0x5D, 0x12, 0x55, 0x00, 0x00];
    let c = parse_matter_service_data(&sd).unwrap();
    assert_eq!(c.discriminator, 0x0B47);
    assert_eq!(c.vendor_id, 0x125D);
    assert_eq!(c.product_id, 0x0055);
    // opcode != 0 / 短すぎは None
    assert!(parse_matter_service_data(&[0x01, 0, 0, 0, 0, 0, 0, 0]).is_none());
    assert!(parse_matter_service_data(&[0x00, 0x47]).is_none());
}
```

- [ ] **Step 3: 実装**

```rust
//! BLE central adapter (bluer / BlueZ) — feature "ble".
//!
//! Matter commissionable の発見（0xFFF6 service data）と、BTP が使う
//! GATT C1/C2 を GattLink（チャネル対）へ橋渡しする。プロトコルは一切
//! 持たない——BTP は btp.rs、その上の Matter は既存層。

use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::mpsc;
use uuid::Uuid;

use crate::btp::{BtpError, GattLink};

/// Matter BLE service（16-bit alias 0xFFF6）。
pub const MATTER_BLE_SERVICE: Uuid = Uuid::from_u128(0x0000FFF6_0000_1000_8000_00805F9B34FB);
/// BTP C1（client→server write）。
pub const BTP_C1: Uuid = Uuid::from_u128(0x18EE2EF5_263D_4559_959F_4F9C429F9D11);
/// BTP C2（server→client indication）。
pub const BTP_C2: Uuid = Uuid::from_u128(0x18EE2EF5_263D_4559_959F_4F9C429F9D12);

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MatterAdvert {
    pub discriminator: u16,
    pub vendor_id: u16,
    pub product_id: u16,
}

/// Matter commissionable advertisement の service data（spec §4.17.3.2、
/// opcode 0x00 / 8 バイト）を解釈する。
pub fn parse_matter_service_data(sd: &[u8]) -> Option<MatterAdvert> {
    if sd.len() < 8 || sd[0] != 0x00 {
        return None;
    }
    Some(MatterAdvert {
        discriminator: u16::from_le_bytes([sd[1], sd[2]]) & 0x0FFF,
        vendor_id: u16::from_le_bytes([sd[3], sd[4]]),
        product_id: u16::from_le_bytes([sd[5], sd[6]]),
    })
}

fn gatt(step: &'static str) -> impl FnOnce(bluer::Error) -> BtpError {
    move |e| BtpError::Gatt(format!("{step}: {e}"))
}

/// discriminator 一致の commissionable デバイスをスキャンする。
pub async fn find_commissionable(
    adapter: &bluer::Adapter,
    discriminator: u16,
    timeout: Duration,
) -> Result<bluer::Device, BtpError> {
    adapter.set_powered(true).await.map_err(gatt("power"))?;
    let mut events = adapter.discover_devices().await.map_err(gatt("discover"))?;
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err(BtpError::Timeout("ble scan"));
        }
        let Ok(Some(ev)) = tokio::time::timeout(remaining, events.next()).await else {
            return Err(BtpError::Timeout("ble scan"));
        };
        let bluer::AdapterEvent::DeviceAdded(addr) = ev else { continue };
        let Ok(device) = adapter.device(addr) else { continue };
        let Ok(Some(sd)) = device.service_data().await else { continue };
        let Some(bytes) = sd.get(&MATTER_BLE_SERVICE) else { continue };
        if let Some(adv) = parse_matter_service_data(bytes) {
            tracing::info!(%addr, disc = adv.discriminator, vid = adv.vendor_id,
                pid = adv.product_id, "matter commissionable found");
            if adv.discriminator == discriminator {
                return Ok(device);
            }
        }
    }
}

/// 接続済み GATT リンク。drop または `disconnect()` で切断。
pub struct BleConnection {
    device: bluer::Device,
    writer: tokio::task::JoinHandle<()>,
}

impl BleConnection {
    pub async fn disconnect(self) {
        self.writer.abort();
        let _ = self.device.disconnect().await;
    }
}

/// GATT 接続して BTP 用の GattLink を開く。
///
/// BTP spec §4.19.5 の順序保証: 「handshake request を C1 に write して
/// から C2 を subscribe する」。writer task は最初の 1 write（= btp::connect
/// が送る handshake request）を完了させた後に C2 の indication pump を開始
/// することでこの順序を構造的に守る。
pub async fn open_link(device: &bluer::Device) -> Result<(GattLink, BleConnection), BtpError> {
    if !device.is_connected().await.map_err(gatt("is_connected"))? {
        device.connect().await.map_err(gatt("connect"))?;
    }
    let mut c1 = None;
    let mut c2 = None;
    for service in device.services().await.map_err(gatt("services"))? {
        if service.uuid().await.map_err(gatt("svc uuid"))? != MATTER_BLE_SERVICE {
            continue;
        }
        for ch in service.characteristics().await.map_err(gatt("chars"))? {
            match ch.uuid().await.map_err(gatt("char uuid"))? {
                u if u == BTP_C1 => c1 = Some(ch),
                u if u == BTP_C2 => c2 = Some(ch),
                _ => {}
            }
        }
    }
    let c1 = c1.ok_or(BtpError::Gatt("C1 not found".into()))?;
    let c2 = c2.ok_or(BtpError::Gatt("C2 not found".into()))?;

    let (wtx, mut wrx) = mpsc::channel::<Vec<u8>>(1);
    let (itx, irx) = mpsc::channel::<Vec<u8>>(8);
    let writer = tokio::spawn(async move {
        // 1 通目（handshake request）を write してから subscribe（順序保証）
        let Some(first) = wrx.recv().await else { return };
        let req = bluer::gatt::remote::CharacteristicWriteRequest {
            op_type: bluer::gatt::WriteOp::Request,
            ..Default::default()
        };
        if let Err(e) = c1.write_ext(&first, &req).await {
            tracing::warn!(error = %e, "btp handshake write failed");
            return;
        }
        let ind = match c2.notify().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "C2 subscribe failed");
                return;
            }
        };
        let pump = tokio::spawn(async move {
            futures_util::pin_mut!(ind);
            while let Some(v) = ind.next().await {
                if itx.send(v).await.is_err() {
                    break;
                }
            }
        });
        while let Some(data) = wrx.recv().await {
            if let Err(e) = c1.write_ext(&data, &req).await {
                tracing::warn!(error = %e, "gatt write failed");
                break;
            }
        }
        pump.abort();
    });

    Ok((
        GattLink { writes: wtx, indications: irx },
        BleConnection { device: device.clone(), writer },
    ))
}
```

（bluer 0.17 の正確な API 名は docs.rs で確認しながら合わせる。`CharacteristicWriteRequest` の再利用がムーブで効かない場合はループ内で都度構築。）

- [ ] **Step 4: preflight example を書く**

`examples/ble-scan.rs`:

```rust
//! jarvis preflight: Matter BLE advertisement scanner.
//! 実行: cargo run -p mat-controller --features ble --example ble-scan
//! 30 秒スキャンして 0xFFF6 service data を持つデバイスを列挙する。

#[cfg(feature = "ble")]
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    use futures_util::StreamExt;
    let session = bluer::Session::new().await?;
    let adapter = session.default_adapter().await?;
    adapter.set_powered(true).await?;
    eprintln!("adapter {} — scanning 30s…", adapter.name());
    let mut events = adapter.discover_devices().await?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    while let Ok(Some(ev)) =
        tokio::time::timeout_at(deadline, events.next()).await
    {
        let bluer::AdapterEvent::DeviceAdded(addr) = ev else { continue };
        let device = adapter.device(addr)?;
        let Ok(Some(sd)) = device.service_data().await else { continue };
        if let Some(bytes) = sd.get(&mat_controller::ble::MATTER_BLE_SERVICE) {
            match mat_controller::ble::parse_matter_service_data(bytes) {
                Some(adv) => println!(
                    "{addr}  disc={:#05x}({})  vid={:#06x} pid={:#06x} rssi={:?}",
                    adv.discriminator, adv.discriminator, adv.vendor_id,
                    adv.product_id, device.rssi().await.ok().flatten()
                ),
                None => println!("{addr}  matter service data (unparsed): {bytes:02x?}"),
            }
        }
    }
    Ok(())
}

#[cfg(not(feature = "ble"))]
fn main() {
    eprintln!("build with --features ble");
}
```

`Cargo.toml` に example の required-features:

```toml
[[example]]
name = "ble-scan"
required-features = ["ble"]
```

- [ ] **Step 5: feature off/on 両方のビルド確認**

Run:
```bash
cargo check -p mat-controller                 # feature off: bluer にリンクしない
cargo test  -p mat-controller --features ble ble:: 2>&1 | tail -3   # パーサテスト
```
Expected: 両方成功。`--features ble` のビルドに libdbus ヘッダが要る場合は `sudo apt-get install -y libdbus-1-dev pkg-config` を先に実行（WSL 開発機のみの話。ビルドできない場合はその旨を報告して parse 関数を cfg 外へ移す判断を仰ぐ）。

- [ ] **Step 6: task check + コミット**

`task check` は feature off なので従来通り通ること（bluer 非依存の確認になる）。

```bash
task check
git add -A crates/mat-controller
git commit -m "feat(controller): bluer BLE centralアダプタとpreflight scanner (feature ble) (M6b Task5)"
```

---

### Task 6: commission_ble_thread（共有ステップ抽出 + オーケストレーション + モック統合テスト）

**Files:**
- Modify: `crates/mat-controller/src/commissioning.rs`
- Test: `commissioning.rs` 内（リファクタ回帰）+ `crates/mat-controller/tests/btp_pase_plumbing.rs`（新規）

**Interfaces:**
- Consumes: Task 1〜5 の全成果、既存 `pase::establish` / `case::establish` / `dnssd::resolve_operational` / `fabric::compressed_fabric_id`
- Produces:
  - `commissioning::BleThreadParams<'a> { pub passcode: u32, pub discriminator: u16, pub thread_dataset: &'a [u8], pub device_node_id: u64, pub paa_dir: Option<&'a Path>, pub cd_signer_dir: Option<&'a Path>, pub scope_id: u32 }`
  - `commissioning::commission_btp_thread(link: btp::GattLink, fabric: &CommissioningFabric, params: BleThreadParams<'_>) -> Result<CommissionedDevice, CommissionError>`（**cfg なし** — モックで統合テスト可能）
  - `commissioning::commission_ble_thread(fabric: &CommissioningFabric, params: BleThreadParams<'_>) -> Result<CommissionedDevice, CommissionError>`（`#[cfg(feature = "ble")]` — scan + open_link + 委譲 + 切断）
  - 内部共有関数 `run_credential_steps(session: &mut SecureSession, fabric, device_node_id, paa_dir, cd_signer_dir, cfg) -> Result<Option<u8>, CommissionError>`（ArmFailSafe→SetRegulatory→attestation→CSR→AddRoot→AddNOC）
  - 内部共有関数 `operational_case_and_complete(udp: Arc<UdpTransport>, addr: SocketAddr, fabric, device_node_id, cfg) -> Result<(SecureSession, ()), CommissionError>` 相当（CASE リトライ + CommissioningComplete）

- [ ] **Step 1: 共有ステップを抽出（挙動不変リファクタ）**

`commission_on_network` の本体からステップ 3〜7（ArmFailSafe〜AddNOC、既読コード 554〜692 行）を `run_credential_steps` に、ステップ 8〜9（CASE リトライ + CommissioningComplete、694〜734 行）を `operational_case_and_complete` に、**コード移動のみ**で抽出し、`commission_on_network` は「ターゲット解決 → pase::establish → run_credential_steps → operational_case_and_complete」の 4 行構成に組み直す。シグネチャ・挙動・ログは不変。

Run: `cargo test -p mat-controller 2>&1 | tail -3`
Expected: 全 PASS（回帰なし）

- [ ] **Step 2: commit（リファクタ単体）**

```bash
git add crates/mat-controller/src/commissioning.rs
git commit -m "refactor(controller): commissioningステップをrun_credential_steps/operational_case_and_completeへ抽出 (M6b Task6)"
```

- [ ] **Step 3: commission_btp_thread を実装**

```rust
/// `commission_ble_thread` の入力一式。
pub struct BleThreadParams<'a> {
    pub passcode: u32,
    pub discriminator: u16,
    /// OTBR の active operational dataset（Thread TLV 生バイト列）。
    pub thread_dataset: &'a [u8],
    pub device_node_id: u64,
    pub paa_dir: Option<&'a std::path::Path>,
    pub cd_signer_dir: Option<&'a std::path::Path>,
    /// Thread 参加後の operational mDNS 発見に使う iface index。
    pub scope_id: u32,
}

/// BTP リンク上で工場出荷デバイスを commission する（spec M6b 決定 3）。
///
/// リンク（GattLink）は確立済みで渡される——BLE スキャン/接続は
/// `commission_ble_thread`（feature "ble"）が行い、テストはモックを渡す。
pub async fn commission_btp_thread(
    link: crate::btp::GattLink,
    fabric: &CommissioningFabric,
    params: BleThreadParams<'_>,
) -> Result<CommissionedDevice, CommissionError> {
    let cfg = MrpConfig::default();
    let xpan = thread_ext_pan_id(params.thread_dataset).ok_or(CommissionError::Malformed {
        step: "thread-dataset",
        detail: "no extended pan id (type 2) in dataset",
    })?;

    // 1. BTP handshake → Transport::Reliable
    let (btp_params, transport) = crate::btp::connect(link, crate::btp::PROPOSED_WINDOW)
        .await
        .map_err(|e| CommissionError::Ble { step: "btp-handshake", detail: e.to_string() })?;
    tracing::info!(segment = btp_params.segment_size, window = btp_params.window_size,
        "btp session established");
    let transport = Arc::new(transport);

    // 2. PASE over BTP
    let mut pase = pase::establish(
        Arc::clone(&transport),
        crate::transport::RELIABLE_PEER,
        params.passcode,
        &cfg,
    )
    .await
    .map_err(CommissionError::Pase)?;

    // 3. 資格情報ステップ（M6a と共通）: ArmFailSafe → … → AddNOC
    let fabric_index = run_credential_steps(
        &mut pase,
        fabric,
        params.device_node_id,
        params.paa_dir,
        params.cd_signer_dir,
        &cfg,
    )
    .await?;

    // 4. Thread dataset 書き込み
    let resp = pase
        .invoke_for_data(
            0,
            CLUSTER_NETWORK_COMMISSIONING,
            CMD_ADD_OR_UPDATE_THREAD,
            Some(&encode_add_or_update_thread_network(params.thread_dataset, 5)),
            None,
            &cfg,
        )
        .await
        .map_err(CommissionError::Session)?;
    let (status, text) = decode_network_config_response(fields_of("add-thread-network", &resp)?)?;
    if status != 0 {
        return Err(CommissionError::NetworkConfig {
            step: "add-thread-network",
            status,
            debug_text: text,
        });
    }

    // 5. failsafe 仕切り直し（spec 決定 5: Thread 参加 + operational 発見が
    //    120s を超えないよう ConnectNetwork 直前に再アーム）
    let resp = pase
        .invoke_for_data(
            0,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            Some(&encode_arm_fail_safe(120, 6)),
            None,
            &cfg,
        )
        .await
        .map_err(CommissionError::Session)?;
    check_commissioning_response("re-arm-fail-safe", &resp)?;

    // 6. ConnectNetwork（Thread join は遅い——応答待ちだけ長い budget で）
    let connect_cfg = MrpConfig {
        initial_interval: Duration::from_secs(60),
        max_retries: 0,
        backoff: 1.0,
    };
    let resp = pase
        .invoke_for_data(
            0,
            CLUSTER_NETWORK_COMMISSIONING,
            CMD_CONNECT_NETWORK,
            Some(&encode_connect_network(&xpan, 7)),
            None,
            &connect_cfg,
        )
        .await
        .map_err(CommissionError::Session)?;
    let (status, text) = decode_connect_network_response(fields_of("connect-network", &resp)?)?;
    if status != 0 {
        return Err(CommissionError::NetworkConfig {
            step: "connect-network",
            status,
            debug_text: text,
        });
    }

    // 7. BTP 経路を手放す（BleConnection の切断は呼び出し側）。以後は IP。
    drop(pase);
    drop(transport);

    // 8. operational 発見（リトライ）→ CASE → CommissioningComplete
    let cfid = fabric.compressed_fabric_id()?;
    let mut resolved = None;
    let mut last_err = None;
    for _ in 0..12 {
        match dnssd::resolve_operational(
            params.scope_id,
            &cfid,
            params.device_node_id,
            Duration::from_secs(5),
        )
        .await
        {
            Ok(node) => {
                resolved = Some(node);
                break;
            }
            Err(e) => {
                last_err = Some(e);
                tokio::time::sleep(Duration::from_secs(3)).await;
            }
        }
    }
    let node = resolved.ok_or_else(|| {
        tracing::warn!(error = ?last_err, "operational advertise did not appear");
        CommissionError::Timeout("operational discovery after thread join")
    })?;
    let addr = node
        .socket_addrs(params.scope_id)
        .into_iter()
        .next()
        .ok_or(CommissionError::Timeout("no usable operational address"))?;
    let udp = Arc::new(
        UdpTransport::bind()
            .await
            .map_err(|e| CommissionError::Ble { step: "udp-bind", detail: e.to_string() })?,
    );
    operational_case_and_complete(udp, addr, fabric, params.device_node_id, &node.mrp_config())
        .await
}
```

実装ノート:
- `fabric.compressed_fabric_id()` が無ければ `fabric::compressed_fabric_id(...)` の既存関数（live_commission_real.rs 参照）を使う形に合わせる。
- `operational_case_and_complete` の実引数は Step 1 の抽出結果に合わせる（CASE リトライ 6 回 ×3s は抽出元のまま）。

`commission_ble_thread`（feature "ble"）:

```rust
/// BLE スキャンから完了までの一括フロー（実機用）。
#[cfg(feature = "ble")]
pub async fn commission_ble_thread(
    fabric: &CommissioningFabric,
    params: BleThreadParams<'_>,
) -> Result<CommissionedDevice, CommissionError> {
    let ble_err = |step: &'static str| {
        move |e: crate::btp::BtpError| CommissionError::Ble { step, detail: e.to_string() }
    };
    let session = bluer::Session::new()
        .await
        .map_err(|e| CommissionError::Ble { step: "bluez-session", detail: e.to_string() })?;
    let adapter = session
        .default_adapter()
        .await
        .map_err(|e| CommissionError::Ble { step: "adapter", detail: e.to_string() })?;
    let device =
        crate::ble::find_commissionable(&adapter, params.discriminator, Duration::from_secs(30))
            .await
            .map_err(ble_err("scan"))?;
    let (link, conn) = crate::ble::open_link(&device).await.map_err(ble_err("gatt"))?;
    let result = commission_btp_thread(link, fabric, params).await;
    conn.disconnect().await; // 成否に関わらず GATT を畳む
    result
}
```

- [ ] **Step 4: モック統合テスト（BTP over GattLink の全層貫通）**

`crates/mat-controller/tests/btp_pase_plumbing.rs`（新規）。fake BTP peripheral（Task 3 のテストヘルパを試験用に再掲——tests/ は別クレートなので btp.rs のヘルパは使えない。同型を書く）+ PASE の最初のメッセージ検証:

```rust
//! BTP → exchange → PASE の配管貫通テスト（実 BLE なし）。
//! fake BTP peripheral の上で pase::establish を走らせ、
//! (1) PBKDFParamRequest が BTP フレームとして届くこと、
//! (2) R フラグ（MRP）が立っていないこと、
//! (3) peripheral が不正応答を返すと PaseError で終わること、を確認する。

use std::sync::Arc;
use std::time::Duration;

use mat_controller::btp::{self, GattLink, Packet, Reassembler};
use mat_controller::exchange::MrpConfig;
use mat_controller::{pase, transport};

#[tokio::test]
async fn pase_over_btp_sends_unreliable_pbkdf_request() {
    let (wtx, mut wrx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
    let (itx, irx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let link = GattLink { writes: wtx, indications: irx };

    let peripheral = tokio::spawn(async move {
        // handshake
        let req = wrx.recv().await.expect("handshake request");
        assert_eq!(req[0], 0x65);
        itx.send(vec![0x65, 0x6C, 4, 244, 0, 4]).await.unwrap();
        // PBKDFParamRequest を再構成
        let mut reasm = Reassembler::new();
        let msg = loop {
            let frame = wrx.recv().await.expect("frame");
            let pkt = Packet::decode(&frame).unwrap();
            if let Some(m) = reasm.push(&pkt).unwrap() {
                break m;
            }
        };
        // Matter message header を素で解いて R フラグ無しを確認
        use mat_controller::message::{MessageHeader, ProtocolHeader};
        let (h, off) = MessageHeader::decode(&msg).unwrap();
        assert_eq!(h.session_id, 0);
        let (p, _) = ProtocolHeader::decode(&msg[off..]).unwrap();
        assert!(!p.needs_ack, "MRP must be off over BTP");
        assert_eq!(p.opcode, 0x20, "PBKDFParamRequest opcode");
        // 不正応答（ゴミ TLV の PBKDFParamResponse）を返して abort させる
        // → establish 側は PaseError で終了するはず。壊れ方は問わないので
        //   opcode 0x21 + 空 payload を返す。
        let reply = {
            let rh = MessageHeader {
                session_id: 0,
                security_flags: 0,
                message_counter: 1,
                source_node_id: None,
                destination: mat_controller::message::Destination::None,
            };
            let rp = ProtocolHeader {
                initiator: false,
                needs_ack: false,
                acked_counter: None,
                opcode: 0x21,
                exchange_id: p.exchange_id,
                protocol_id: p.protocol_id,
                vendor_id: None,
            };
            let mut b = rh.encoded();
            rp.encode(&mut b);
            b
        };
        // 1 フレームで送る（BTP data packet, seq=1 相当は fake 側管理: seq 0 は
        // まだ使っていないので 0 から）
        let frame = btp::encode_data_packet(
            0,
            None,
            btp::SegmentPos::First { ending: true },
            Some(reply.len() as u16),
            &reply,
        );
        itx.send(frame).await.unwrap();
    });

    let (_params, t) = btp::connect(link, btp::PROPOSED_WINDOW).await.unwrap();
    let err = pase::establish(
        Arc::new(t),
        transport::RELIABLE_PEER,
        20202021,
        &MrpConfig {
            initial_interval: Duration::from_millis(200),
            max_retries: 1,
            backoff: 1.0,
        },
    )
    .await
    .expect_err("garbage PBKDFParamResponse must fail");
    let _ = err; // 種別は問わない（Malformed / Message いずれか）
    peripheral.await.unwrap();
}
```

（`message::MessageHeader` などが pub でなければ pub 化するか、検証を「opcode 0x20 の Matter メッセージが 1 通届く」まで緩める。R フラグ検証は Task 1 のユニットテストが担保しているため、この統合テストの主眼は**層の貫通**。PBKDFParamRequest opcode 0x20 / Response 0x21 は pase.rs の既存定数 `OPCODE_PBKDF_PARAM_REQUEST` を pub 再利用できるならそれを使う。）

- [ ] **Step 5: green 確認**

Run: `cargo test -p mat-controller --test btp_pase_plumbing 2>&1 | tail -3` と `cargo test -p mat-controller 2>&1 | tail -3`
Expected: 全 PASS

- [ ] **Step 6: task check + コミット**

```bash
task check
git add -A crates/mat-controller
git commit -m "feat(controller): commission_btp_thread/commission_ble_thread(NetworkCommissioning挿入) + BTP-PASE配管統合テスト (M6b Task6)"
```

---

### Task 7: 実機 E2E ハーネス（玄関ライト）+ Taskfile + ARCHITECTURE

**Files:**
- Create: `crates/mat-controller/tests/live_commission_ble.rs`
- Create: `scripts/e2e-m6b-real.sh`
- Modify: `crates/mat-controller/Cargo.toml`（`[[test]] required-features`）
- Modify: `Taskfile.yml`
- Modify: `ARCHITECTURE.md`

**Interfaces:**
- Consumes: `commission_ble_thread` / `open_commissioning_window` / `encode_remove_fabric`（すべて既存 or Task 6）
- Produces: `task e2e:m6b:real`（jarvis 上で実行）と preflight 手順

- [ ] **Step 1: 実機ハーネスを書く**

`live_commission_ble.rs`（`#[ignore]`、feature "ble" 必須）。**フロー**（spec 実機手順そのまま）:

```rust
//! M6b 実機受け入れ: 工場リセット済みデバイスを BLE+Thread で使い捨て
//! fabric へ native commission → onoff 確認 → native open-window →
//! （オペレータが本番 `mat commission` を実行）→ RemoveFabric 撤収。
//! 実行は scripts/e2e-m6b-real.sh 経由（jarvis 上）。
//!
//! 必須 env:
//!   MAT_E2E_BLE_PASSCODE      デバイス印字の setup passcode
//!   MAT_E2E_BLE_DISCRIMINATOR 12-bit discriminator（10進）
//!   MAT_E2E_THREAD_DATASET    `ot-ctl dataset active -x` の hex
//!   MAT_E2E_IFACE             operational 発見用 iface（jarvis は eth0）
//!   MAT_E2E_PAA_DIR           本番 PAA ストア（<store>/paa-trust-store）
//!   MAT_E2E_NODE_ID           使い捨て fabric 上の新 node_id（例 200）

#![cfg(feature = "ble")]

use std::sync::Arc;
use std::time::Duration;

use mat_controller::commissioning::{
    self, BleThreadParams, CommissioningFabric,
};
use mat_controller::exchange::MrpConfig;
use mat_controller::im::{CLUSTER_ON_OFF, CMD_ON_OFF_TOGGLE};

fn env(name: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| panic!("{name} required"))
}

fn hex_bytes(s: &str) -> Vec<u8> {
    let s = s.trim();
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).expect("hex dataset"))
        .collect()
}

#[tokio::test]
#[ignore = "requires jarvis BLE + factory-reset device + OTBR dataset (task e2e:m6b:real)"]
async fn commission_factory_device_over_ble_thread() {
    tracing_subscriber::fmt().with_env_filter("info,mat_controller=debug").init();
    let passcode: u32 = env("MAT_E2E_BLE_PASSCODE").parse().unwrap();
    let discriminator: u16 = env("MAT_E2E_BLE_DISCRIMINATOR").parse().unwrap();
    let dataset = hex_bytes(&env("MAT_E2E_THREAD_DATASET"));
    let scope_id = mat_controller::dnssd::iface_index(&env("MAT_E2E_IFACE")).unwrap();
    let paa_dir = std::path::PathBuf::from(env("MAT_E2E_PAA_DIR"));
    let device_node_id: u64 = env("MAT_E2E_NODE_ID").parse().unwrap();

    // 使い捨て fabric（M6a と同じ機構、プロセスメモリのみ）
    let fabric = CommissioningFabric::generate(0xE2E2, 100).unwrap();

    // 1) BLE+Thread native commission
    let device = commissioning::commission_ble_thread(
        &fabric,
        BleThreadParams {
            passcode,
            discriminator,
            thread_dataset: &dataset,
            device_node_id,
            paa_dir: Some(&paa_dir),
            cd_signer_dir: None,
            scope_id,
        },
    )
    .await
    .expect("ble+thread commissioning failed");
    let mut session = device.session;
    let cfg = MrpConfig::default();
    eprintln!("== commissioned: node {} fabric_index {:?}", device.node_id, device.fabric_index);

    // 2) 使い捨て fabric で onoff toggle（動作確認）
    session
        .invoke(1, CLUSTER_ON_OFF, CMD_ON_OFF_TOGGLE, None, None, &cfg)
        .await
        .expect("toggle over new fabric");
    eprintln!("== toggle OK — ライトが変化したこと目視");

    // 3) native open-window → 本番 join 用コードを表示
    let window = commissioning::open_commissioning_window(&mut session, 300, &cfg)
        .await
        .expect("open window");
    eprintln!("== 本番復帰: 別端末で `mat commission <target> {}` を 5 分以内に実行", window.manual_code);
    eprintln!("== 完了したら Enter:");
    let mut line = String::new();
    std::io::stdin().read_line(&mut line).unwrap();

    // 4) 使い捨て fabric を撤収（RemoveFabric は fabric_index 必須）
    let idx = device.fabric_index.expect("fabric index from AddNOC");
    let resp = session
        .invoke_for_data(
            0,
            commissioning::CLUSTER_OPERATIONAL_CREDENTIALS,
            commissioning::CMD_REMOVE_FABRIC,
            Some(&commissioning::encode_remove_fabric(idx)),
            None,
            &cfg,
        )
        .await
        .expect("remove fabric");
    eprintln!("== RemoveFabric status {} — 使い捨て fabric 撤収完了", resp.status);
}
```

（`invoke` / `invoke_for_data` の引数形・`iface_index` の正確な関数名は live_commission_real.rs / live_jarvis.rs の既存呼び出しに合わせる。tracing_subscriber が dev-deps に無ければ追加するか eprintln に留める。）

`Cargo.toml`:

```toml
[[test]]
name = "live_commission_ble"
required-features = ["ble"]
```

- [ ] **Step 2: スクリプトと Taskfile**

`scripts/e2e-m6b-real.sh`（`scripts/e2e-m6-real.sh` の構成を踏襲。jarvis 上で実行する前提のガード + 手順表示）:

```bash
#!/usr/bin/env bash
# M6b 実機 E2E — jarvis 上で実行する（BLE は WSL では動かない）。
# 事前:
#   1) sudo ot-ctl dataset active -x  → MAT_E2E_THREAD_DATASET
#   2) 対象（玄関ライト）を工場リセットし、印字の passcode/discriminator を控える
#   3) bluetoothctl power on / preflight: cargo run --features ble --example ble-scan
set -euo pipefail
: "${MAT_E2E_BLE_PASSCODE:?device setup passcode}"
: "${MAT_E2E_BLE_DISCRIMINATOR:?12-bit discriminator}"
: "${MAT_E2E_THREAD_DATASET:?ot-ctl dataset active -x}"
: "${MAT_E2E_IFACE:?e.g. eth0}"
: "${MAT_E2E_PAA_DIR:?<store>/paa-trust-store}"
: "${MAT_E2E_NODE_ID:=200}"
exec cargo test -p mat-controller --features ble --test live_commission_ble \
  -- --ignored --nocapture
```

`Taskfile.yml`（`e2e:m6:real` の後ろ）:

```yaml
  e2e:m6b:real:
    desc: "M6b 実機 E2E（jarvis 上で実行。工場リセット済みデバイスを BLE+Thread で native commission→open-window→本番復帰→RemoveFabric。要 MAT_E2E_BLE_* / THREAD_DATASET / IFACE / PAA_DIR）"
    cmds:
      - bash scripts/e2e-m6b-real.sh
```

- [ ] **Step 3: ARCHITECTURE.md 更新**

Phase 5 節の M6a 項の直後に M6b 項を追加（M6a 項と同じ形式）: スコープ（BTP/BLE native・bluer は feature `ble` 隔離・Thread dataset は NetworkCommissioning で書き込み・MRP は BTP 上で無効）、本番経路無変更、テスト（モック GattLink 統合 + `task e2e:m6b:real`）、実機結果は E2E 実施後に追記する旨。465 行付近の「M6b（…）が未着手」の記述を「実装済み・実機 E2E は別途」へ更新（E2E 完了後に最終化）。

- [ ] **Step 4: ビルド確認 + task check + コミット**

Run:
```bash
cargo check -p mat-controller --features ble --tests
task check
```
Expected: 両方 PASS

```bash
git add -A
git commit -m "test(controller)+docs: M6b 実機E2Eハーネス(BLE+Thread)とTaskfile/ARCHITECTURE反映 (M6b Task7)"
```

---

### Task 8（実機、人間と協働）: jarvis preflight → 玄関ライト E2E

コード改変なし。実機作業のチェックリスト。**メインエージェントが jarvis への ssh / ユーザーとの協働で進める**（サブエージェント不可の対話ステップを含む）。

- [ ] **Step 1: jarvis preflight**
  - `ssh jarvis 'ls /sys/class/bluetooth/; bluetoothctl --version; systemctl is-active bluetooth'` — hci0 と BlueZ ≥ 5.56 を確認
  - リポジトリを jarvis に同期し `--features ble` でビルド（bluer は libdbus 必要 → `sudo apt-get install -y libdbus-1-dev pkg-config`。Pi 上ビルドが遅い場合は musl.cc の aarch64 クロス + libdbus vendored を検討、判断は実施時）
  - `cargo run -p mat-controller --features ble --example ble-scan` — 近隣の Matter advertise 観測（既存デバイスの open-window 中 advertise の有無もここで記録）
  - `sudo ot-ctl dataset active -x` で dataset 取得確認（OTBR の所在が jarvis でなければユーザーに確認）
- [ ] **Step 2: 玄関ライトの現状記録** — 台帳 node_id・alias・group 帰属を控える（撤収後の復元に使う）
- [ ] **Step 3: 工場リセット（ユーザー実施）** → 印字 passcode/discriminator を env に設定 → `task e2e:m6b:real`
- [ ] **Step 4: ハーネス内 open-window の manual code で本番 `mat commission` join（既存 chip-tool 経路）→ Enter で RemoveFabric 撤収**
- [ ] **Step 5: 復元確認** — 本番経路で onoff・alias 再設定・（group 帰属があれば `mat group grant` 等で復元）
- [ ] **Step 6: 結果を spec / ARCHITECTURE / メモリに記録し、コミット**

---

## Self-Review 済み事項

- spec の全決定（bluer / BTP 自前 / MRP 無効 / dataset 引数 / エラー写像 / failsafe 再アーム / 実機手順）にタスクが対応していることを確認。
- エラー写像表（spec 決定 4）は CommissionError の変種追加（Task 4）で表現され、本番 ErrorKind への配線は M6b 非ゴール（native 版 mat トラック）なのでコード化しない。
- BTP のバイト配置は Task 2 Step 5 で connectedhomeip と突合するまで「仮確定」扱い。テストベクタは実装と独立に spec から書いてあるため、突合で誤りが見つかればベクタごと直す。
- bluer の libdbus リンクは feature `ble` で隔離し、`task check`（feature off）が従来のビルド経路を守る。
