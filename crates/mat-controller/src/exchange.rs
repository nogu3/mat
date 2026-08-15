//! Exchange layer and MRP reliability (spec §4.6, §4.12).

use std::net::SocketAddr;
use std::time::Duration;

use tokio::time::Instant;

use crate::counter::{RxWindow, TxCounter};
use crate::message::{
    Destination, MessageError, MessageHeader, ProtocolHeader, OPCODE_MRP_STANDALONE_ACK,
    PROTOCOL_ID_SECURE_CHANNEL,
};
use crate::transport::{Transport, MAX_DATAGRAM};

/// spec 4.12.2.1 MRP_BACKOFF_JITTER: 再送待ちジッタ係数の既定上限。
pub const MRP_BACKOFF_JITTER: f64 = 0.25;

/// [0,1) の一様乱数。getrandom 失敗時は 0.5 へ退避 — jitter は品質であって
/// 正しさではないので、ここでは panic させない（暗号用途には使わないこと）。
pub fn unit_random() -> f64 {
    let mut b = [0u8; 8];
    if getrandom::getrandom(&mut b).is_err() {
        return 0.5;
    }
    (u64::from_le_bytes(b) >> 11) as f64 / (1u64 << 53) as f64
}

/// 1 回の再送待ちへジッタを乗せる（純関数 — r は `unit_random()` の値）。
pub fn jittered_interval(interval: Duration, jitter: f64, r: f64) -> Duration {
    interval.mul_f64(1.0 + jitter * r)
}

/// MRP retransmission parameters (spec 4.12; defaults follow chip defaults).
#[derive(Debug, Clone)]
pub struct MrpConfig {
    /// ピアが idle とみなされるときの再送初期間隔（mDNS TXT の SII 由来）。
    pub initial_interval: Duration,
    /// ピアが active とみなされるときの再送初期間隔（mDNS TXT の SAI 由来）。
    /// spec 4.12.8: 直近に受信があるピアは SESSION_ACTIVE_INTERVAL で再送する。
    /// Thread sleepy device は SII=5000ms が普通で、これを active 中も使うと
    /// 1 パケット喪失で 5 秒止まり、購読 priming のようなチャンク往復は
    /// デバイス側 chunk タイムアウトに負けて死ぬ（実機で確認済み）。
    pub active_interval: Duration,
    pub max_retries: u32,
    pub backoff: f64,
    /// 各再送待ちに乗せるジッタ係数の上限（spec 4.12.2.1 MRP_BACKOFF_JITTER）。
    /// 実待ち = interval × (1 + jitter · r)、r ∈ [0,1)。0.0 = ジッタ無し
    /// （テストの決定論用）。
    pub jitter: f64,
}

impl Default for MrpConfig {
    fn default() -> Self {
        Self {
            initial_interval: Duration::from_millis(300),
            active_interval: Duration::from_millis(300),
            max_retries: 4,
            backoff: 1.6,
            jitter: MRP_BACKOFF_JITTER,
        }
    }
}

/// ピアを active とみなす受信からの経過時間の窓
/// (spec: SESSION_ACTIVE_THRESHOLD、既定 4000ms)。
pub(crate) const PEER_ACTIVE_WINDOW: Duration = Duration::from_millis(4000);

/// 直近の受信時刻から MRP 再送の初期間隔を選ぶ（spec 4.12.8: 受信の新しい
/// ピアは active → SAI、それ以外は idle → SII）。
pub(crate) fn retrans_base(last_rx: Option<Instant>, cfg: &MrpConfig) -> Duration {
    match last_rx {
        Some(t) if t.elapsed() < PEER_ACTIVE_WINDOW => cfg.active_interval,
        _ => cfg.initial_interval,
    }
}

/// MRP 再送が尽きるまでの待ち時間総和（ジッタ最悪値込みの上界）。op 予算
/// 設計（Issue #16）の成分 — 実待ちは各項 × (1 + jitter·r) なので、上界は
/// r=1 で見積もる。
pub fn total_budget(cfg: &MrpConfig) -> Duration {
    let mut total = Duration::ZERO;
    let mut interval = cfg.initial_interval;
    for _ in 0..=cfg.max_retries {
        total += interval.mul_f64(1.0 + cfg.jitter);
        interval = interval.mul_f64(cfg.backoff);
    }
    total
}

#[derive(Debug)]
pub enum ExchangeError {
    Timeout,
    Io(std::io::Error),
    Message(MessageError),
}

impl std::fmt::Display for ExchangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExchangeError::Timeout => write!(f, "no acknowledgement within MRP retry budget"),
            ExchangeError::Io(e) => write!(f, "transport error: {e}"),
            ExchangeError::Message(e) => write!(f, "peer sent malformed message: {e}"),
        }
    }
}

impl std::error::Error for ExchangeError {}

impl From<std::io::Error> for ExchangeError {
    fn from(e: std::io::Error) -> Self {
        ExchangeError::Io(e)
    }
}

impl From<MessageError> for ExchangeError {
    fn from(e: MessageError) -> Self {
        ExchangeError::Message(e)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct IncomingMessage {
    pub header: MessageHeader,
    pub proto: ProtocolHeader,
    pub payload: Vec<u8>,
}

/// One unsecured (session id 0) exchange, this side as initiator, with MRP.
pub struct UnsecuredExchange<'t> {
    transport: &'t Transport,
    peer: SocketAddr,
    exchange_id: u16,
    source_node_id: u64,
    counter: TxCounter,
    rx_window: RxWindow,
    last_sent_counter: Option<u32>,
    /// ピアから最後に有効なメッセージを受けた時刻（MRP active/idle 判定用）。
    last_rx: Option<Instant>,
}

impl<'t> UnsecuredExchange<'t> {
    pub fn new(transport: &'t Transport, peer: SocketAddr) -> Self {
        let mut b = [0u8; 10];
        getrandom::getrandom(&mut b).expect("os rng");
        Self {
            transport,
            peer,
            exchange_id: u16::from_le_bytes([b[0], b[1]]),
            source_node_id: u64::from_le_bytes(b[2..10].try_into().expect("8 bytes")),
            counter: TxCounter::new_random(),
            rx_window: RxWindow::new(),
            last_sent_counter: None,
            last_rx: None,
        }
    }

    pub fn exchange_id(&self) -> u16 {
        self.exchange_id
    }

    /// The message counter used by the most recent `send_reliable` call, if any.
    pub fn last_sent_counter(&self) -> Option<u32> {
        self.last_sent_counter
    }

    fn build(
        &mut self,
        protocol_id: u16,
        opcode: u8,
        needs_ack: bool,
        acked_counter: Option<u32>,
        payload: &[u8],
    ) -> (Vec<u8>, u32) {
        let needs_ack = needs_ack && !self.transport.is_reliable();
        let message_counter = self.counter.next();
        let header = MessageHeader {
            session_id: 0,
            security_flags: 0,
            message_counter,
            source_node_id: Some(self.source_node_id),
            destination: Destination::None,
        };
        let proto = ProtocolHeader {
            initiator: true,
            needs_ack,
            acked_counter,
            opcode,
            exchange_id: self.exchange_id,
            protocol_id,
            vendor_id: None,
        };
        let mut buf = header.encoded();
        proto.encode(&mut buf);
        buf.extend_from_slice(payload);
        (buf, message_counter)
    }

    async fn send_standalone_ack(&mut self, acked: u32) -> Result<(), ExchangeError> {
        let (buf, _) = self.build(
            PROTOCOL_ID_SECURE_CHANNEL,
            OPCODE_MRP_STANDALONE_ACK,
            false,
            Some(acked),
            &[],
        );
        self.transport.send_to(&buf, self.peer).await?;
        Ok(())
    }

    /// Decodes a datagram and screens it for this exchange. Returns `None`
    /// for foreign or duplicate traffic the caller should skip (duplicates
    /// are re-acked here). Standalone acks pass screening and are returned
    /// as `Some`; callers filter them by opcode.
    async fn screen(
        &mut self,
        buf: &[u8],
        from: SocketAddr,
    ) -> Result<Option<IncomingMessage>, ExchangeError> {
        if from != self.peer {
            return Ok(None);
        }
        let (header, off) = match MessageHeader::decode(buf) {
            Ok(v) => v,
            Err(_) => return Ok(None), // 不正データグラムは無視（DoS 耐性）
        };
        if header.session_id != 0 || header.security_flags != 0 {
            return Ok(None);
        }
        let (proto, body_off) = match ProtocolHeader::decode(&buf[off..]) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        if proto.exchange_id != self.exchange_id || proto.initiator {
            return Ok(None);
        }
        // ここまで来た = このピアからの当該 exchange の有効トラフィック。
        // MRP active/idle 判定の材料として受信時刻を記録する（重複でも良い —
        // ピアが生きて送っている事実に変わりない）。
        self.last_rx = Some(Instant::now());
        if !self.rx_window.check_and_commit(header.message_counter) {
            if proto.needs_ack && !self.transport.is_reliable() {
                self.send_standalone_ack(header.message_counter).await?;
            }
            return Ok(None);
        }
        if proto.needs_ack && !self.transport.is_reliable() {
            self.send_standalone_ack(header.message_counter).await?;
        }
        Ok(Some(IncomingMessage {
            header,
            proto,
            payload: buf[off + body_off..].to_vec(),
        }))
    }

    /// Sends a reliability-flagged message and retransmits until the peer
    /// acknowledges it. Returns the peer's real response if one carried the
    /// ack (or arrived on the exchange), `None` for a standalone ack.
    pub async fn send_reliable(
        &mut self,
        protocol_id: u16,
        opcode: u8,
        payload: &[u8],
        cfg: &MrpConfig,
    ) -> Result<Option<IncomingMessage>, ExchangeError> {
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
        let (datagram, our_counter) = self.build(protocol_id, opcode, true, None, payload);
        self.last_sent_counter = Some(our_counter);
        let mut interval = retrans_base(self.last_rx, cfg);
        let mut attempts = 0u32;
        loop {
            self.transport.send_to(&datagram, self.peer).await?;
            let deadline = Instant::now() + jittered_interval(interval, cfg.jitter, unit_random());
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let mut buf = [0u8; MAX_DATAGRAM];
                let Ok(recv) =
                    tokio::time::timeout(remaining, self.transport.recv_from(&mut buf)).await
                else {
                    break; // interval 経過 → 再送
                };
                let (n, from) = recv?;
                let Some(msg) = self.screen(&buf[..n], from).await? else {
                    // ack-only の可能性: screen は standalone ack も Some で返す
                    continue;
                };
                let acks_us = msg.proto.acked_counter == Some(our_counter);
                let is_standalone_ack = msg.proto.protocol_id == PROTOCOL_ID_SECURE_CHANNEL
                    && msg.proto.opcode == OPCODE_MRP_STANDALONE_ACK;
                if is_standalone_ack {
                    if acks_us {
                        return Ok(None);
                    }
                    continue;
                }
                // exchange 上の実メッセージは応答とみなす（相手が処理した証拠）
                return Ok(Some(msg));
            }
            attempts += 1;
            if attempts > cfg.max_retries {
                return Err(ExchangeError::Timeout);
            }
            interval = interval.mul_f64(cfg.backoff);
        }
    }

    /// Sends a reliability-flagged message exactly once and returns
    /// immediately, without waiting for (or retransmitting on a missing)
    /// acknowledgement. The R flag is still set, so the peer's own MRP layer
    /// tracks and acks it normally — only *our* wait/retry loop is skipped.
    /// For genuine fire-and-forget sends where the caller cannot afford
    /// `send_reliable`'s worst-case retry budget (e.g. an abort notification
    /// sent while already unwinding to an error).
    pub async fn send_once(
        &mut self,
        protocol_id: u16,
        opcode: u8,
        payload: &[u8],
    ) -> Result<(), ExchangeError> {
        let (datagram, our_counter) = self.build(protocol_id, opcode, true, None, payload);
        self.last_sent_counter = Some(our_counter);
        self.transport.send_to(&datagram, self.peer).await?;
        Ok(())
    }

    /// Waits for the next real (non-ack) message on this exchange.
    pub async fn recv(&mut self, timeout: Duration) -> Result<IncomingMessage, ExchangeError> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ExchangeError::Timeout);
            }
            let mut buf = [0u8; MAX_DATAGRAM];
            let Ok(recv) =
                tokio::time::timeout(remaining, self.transport.recv_from(&mut buf)).await
            else {
                return Err(ExchangeError::Timeout);
            };
            let (n, from) = recv?;
            let Some(msg) = self.screen(&buf[..n], from).await? else {
                continue;
            };
            if msg.proto.protocol_id == PROTOCOL_ID_SECURE_CHANNEL
                && msg.proto.opcode == OPCODE_MRP_STANDALONE_ACK
            {
                continue;
            }
            return Ok(msg);
        }
    }
}

/// peer が開始した unsecured exchange の応答側。PASE/CASE の全フローを 1
/// exchange で捌く（spec §4.6, §4.12）。`UnsecuredExchange` の鏡像 —
/// あちらは自分から exchange を開く初期側、こちらは peer から届いた最初の
/// メッセージ（`adopt`）から採番を引き継いで応答する側。
pub struct ResponderExchange<'t> {
    transport: &'t Transport,
    peer: SocketAddr,
    exchange_id: u16,
    counter: TxCounter,
    rx_window: RxWindow,
    /// 直近に受理した peer メッセージの counter。応答の ack piggyback に使う。
    last_peer_counter: u32,
    /// ピアから最後に有効なメッセージを受けた時刻（MRP active/idle 判定用）。
    last_rx: Option<Instant>,
    /// adopt 時点の最初のメッセージが needs_ack を立てていれば、その counter。
    first_needs_ack: Option<u32>,
}

impl<'t> ResponderExchange<'t> {
    /// 受信済みの最初の peer-initiated メッセージから採番を引き継いで作る。
    /// `first` の counter は即座に rx_window へコミットする — 再送されて
    /// きた同一メッセージは `screen` の重複判定に落ちて standalone-ack のみ
    /// 返す。
    pub fn adopt(transport: &'t Transport, peer: SocketAddr, first: &IncomingMessage) -> Self {
        let mut rx_window = RxWindow::new();
        rx_window.check_and_commit(first.header.message_counter);
        let first_needs_ack = first
            .proto
            .needs_ack
            .then_some(first.header.message_counter);
        Self {
            transport,
            peer,
            exchange_id: first.proto.exchange_id,
            counter: TxCounter::new_random(),
            rx_window,
            last_peer_counter: first.header.message_counter,
            last_rx: Some(Instant::now()),
            first_needs_ack,
        }
    }

    /// adopt 時点のメッセージが ack を要求していた場合、その counter。
    /// 最初の応答（`reply_reliable`/`reply_final`）自体が ack を piggyback
    /// するので通常は不要だが、応答までに時間がかかる呼び出し側が先に
    /// standalone ack を出す判断材料として公開する。
    pub fn first_needs_ack(&self) -> Option<u32> {
        self.first_needs_ack
    }

    fn build(
        &mut self,
        protocol_id: u16,
        opcode: u8,
        needs_ack: bool,
        acked_counter: Option<u32>,
        payload: &[u8],
    ) -> (Vec<u8>, u32) {
        let needs_ack = needs_ack && !self.transport.is_reliable();
        let message_counter = self.counter.next();
        let header = MessageHeader {
            session_id: 0,
            security_flags: 0,
            message_counter,
            source_node_id: None,
            destination: Destination::None,
        };
        let proto = ProtocolHeader {
            initiator: false,
            needs_ack,
            acked_counter,
            opcode,
            exchange_id: self.exchange_id,
            protocol_id,
            vendor_id: None,
        };
        let mut buf = header.encoded();
        proto.encode(&mut buf);
        buf.extend_from_slice(payload);
        (buf, message_counter)
    }

    async fn send_standalone_ack(&mut self, acked: u32) -> Result<(), ExchangeError> {
        let (buf, _) = self.build(
            PROTOCOL_ID_SECURE_CHANNEL,
            OPCODE_MRP_STANDALONE_ACK,
            false,
            Some(acked),
            &[],
        );
        self.transport.send_to(&buf, self.peer).await?;
        Ok(())
    }

    /// `UnsecuredExchange::screen` の鏡像: `proto.initiator == false`
    /// （応答側どうしの迷子/偽装トラフィック）と exchange_id 不一致を捨て、
    /// 重複 counter は standalone-ack のみ返して `None`。
    async fn screen(
        &mut self,
        buf: &[u8],
        from: SocketAddr,
    ) -> Result<Option<IncomingMessage>, ExchangeError> {
        if from != self.peer {
            return Ok(None);
        }
        let (header, off) = match MessageHeader::decode(buf) {
            Ok(v) => v,
            Err(_) => return Ok(None), // 不正データグラムは無視（DoS 耐性）
        };
        if header.session_id != 0 || header.security_flags != 0 {
            return Ok(None);
        }
        let (proto, body_off) = match ProtocolHeader::decode(&buf[off..]) {
            Ok(v) => v,
            Err(_) => return Ok(None),
        };
        if proto.exchange_id != self.exchange_id || !proto.initiator {
            return Ok(None);
        }
        self.last_rx = Some(Instant::now());
        if !self.rx_window.check_and_commit(header.message_counter) {
            if proto.needs_ack && !self.transport.is_reliable() {
                self.send_standalone_ack(header.message_counter).await?;
            }
            return Ok(None);
        }
        self.last_peer_counter = header.message_counter;
        if proto.needs_ack && !self.transport.is_reliable() {
            self.send_standalone_ack(header.message_counter).await?;
        }
        Ok(Some(IncomingMessage {
            header,
            proto,
            payload: buf[off + body_off..].to_vec(),
        }))
    }

    /// Waits for the next real (non-ack) peer message on this exchange.
    async fn recv(&mut self, timeout: Duration) -> Result<IncomingMessage, ExchangeError> {
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(ExchangeError::Timeout);
            }
            let mut buf = [0u8; MAX_DATAGRAM];
            let Ok(recv) =
                tokio::time::timeout(remaining, self.transport.recv_from(&mut buf)).await
            else {
                return Err(ExchangeError::Timeout);
            };
            let (n, from) = recv?;
            let Some(msg) = self.screen(&buf[..n], from).await? else {
                continue;
            };
            if msg.proto.protocol_id == PROTOCOL_ID_SECURE_CHANNEL
                && msg.proto.opcode == OPCODE_MRP_STANDALONE_ACK
            {
                continue;
            }
            return Ok(msg);
        }
    }

    /// initiator:false で応答し、同一 exchange の次の peer メッセージを待つ。
    /// 応答には直前に受理した peer counter を ack piggyback する。ピアの
    /// 実応答（または直後の任意の実メッセージ — 受信できたこと自体が我々の
    /// 送信が処理された証拠、という `send_reliable` と同じ簡略化）が届くまで
    /// MRP 再送する。standalone ack のみで確定した場合は `None`。
    /// `UnsecuredExchange::send_reliable` の鏡像。
    pub async fn reply_reliable(
        &mut self,
        protocol_id: u16,
        opcode: u8,
        payload: &[u8],
        cfg: &MrpConfig,
    ) -> Result<Option<IncomingMessage>, ExchangeError> {
        if self.transport.is_reliable() {
            let (datagram, _) = self.build(
                protocol_id,
                opcode,
                false,
                Some(self.last_peer_counter),
                payload,
            );
            self.transport.send_to(&datagram, self.peer).await?;
            let budget = total_budget(cfg);
            return match self.recv(budget).await {
                Ok(msg) => Ok(Some(msg)),
                Err(e) => Err(e),
            };
        }
        let (datagram, our_counter) = self.build(
            protocol_id,
            opcode,
            true,
            Some(self.last_peer_counter),
            payload,
        );
        let mut interval = retrans_base(self.last_rx, cfg);
        let mut attempts = 0u32;
        loop {
            self.transport.send_to(&datagram, self.peer).await?;
            let deadline = Instant::now() + jittered_interval(interval, cfg.jitter, unit_random());
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let mut buf = [0u8; MAX_DATAGRAM];
                let Ok(recv) =
                    tokio::time::timeout(remaining, self.transport.recv_from(&mut buf)).await
                else {
                    break; // interval 経過 → 再送
                };
                let (n, from) = recv?;
                let Some(msg) = self.screen(&buf[..n], from).await? else {
                    continue;
                };
                let acks_us = msg.proto.acked_counter == Some(our_counter);
                let is_standalone_ack = msg.proto.protocol_id == PROTOCOL_ID_SECURE_CHANNEL
                    && msg.proto.opcode == OPCODE_MRP_STANDALONE_ACK;
                if is_standalone_ack {
                    if acks_us {
                        return Ok(None);
                    }
                    continue;
                }
                return Ok(Some(msg));
            }
            attempts += 1;
            if attempts > cfg.max_retries {
                return Err(ExchangeError::Timeout);
            }
            interval = interval.mul_f64(cfg.backoff);
        }
    }

    /// 応答して待たない（StatusReport 終端用）。needs_ack を立て、ack
    /// （standalone または piggyback、どちらも `acked_counter` が我々の
    /// counter と一致していること）を受け取るまで MRP 再送する。
    pub async fn reply_final(
        &mut self,
        protocol_id: u16,
        opcode: u8,
        payload: &[u8],
        cfg: &MrpConfig,
    ) -> Result<(), ExchangeError> {
        if self.transport.is_reliable() {
            let (datagram, _) = self.build(
                protocol_id,
                opcode,
                false,
                Some(self.last_peer_counter),
                payload,
            );
            self.transport.send_to(&datagram, self.peer).await?;
            return Ok(());
        }
        let (datagram, our_counter) = self.build(
            protocol_id,
            opcode,
            true,
            Some(self.last_peer_counter),
            payload,
        );
        let mut interval = retrans_base(self.last_rx, cfg);
        let mut attempts = 0u32;
        loop {
            self.transport.send_to(&datagram, self.peer).await?;
            let deadline = Instant::now() + jittered_interval(interval, cfg.jitter, unit_random());
            loop {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break;
                }
                let mut buf = [0u8; MAX_DATAGRAM];
                let Ok(recv) =
                    tokio::time::timeout(remaining, self.transport.recv_from(&mut buf)).await
                else {
                    break; // interval 経過 → 再送
                };
                let (n, from) = recv?;
                let Some(msg) = self.screen(&buf[..n], from).await? else {
                    continue;
                };
                if msg.proto.acked_counter == Some(our_counter) {
                    return Ok(());
                }
            }
            attempts += 1;
            if attempts > cfg.max_retries {
                return Err(ExchangeError::Timeout);
            }
            interval = interval.mul_f64(cfg.backoff);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{
        Destination, MessageHeader, ProtocolHeader, OPCODE_MRP_STANDALONE_ACK,
        OPCODE_STATUS_REPORT, PROTOCOL_ID_SECURE_CHANNEL,
    };
    use crate::transport::{UdpTransport, MAX_DATAGRAM};
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::time::Duration;

    fn fast_cfg() -> MrpConfig {
        MrpConfig {
            initial_interval: Duration::from_millis(50),
            active_interval: Duration::from_millis(50),
            max_retries: 2,
            backoff: 1.0,
            jitter: 0.0,
        }
    }

    async fn bind_local() -> UdpTransport {
        UdpTransport::bind_addr("[::1]:0".parse().unwrap())
            .await
            .unwrap()
    }

    async fn bind_local_transport() -> Transport {
        Transport::Udp(Arc::new(bind_local().await))
    }

    async fn read_msg(t: &UdpTransport) -> (MessageHeader, ProtocolHeader, SocketAddr) {
        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, from) = t.recv_from(&mut buf).await.unwrap();
        let (h, off) = MessageHeader::decode(&buf[..n]).unwrap();
        let (p, _) = ProtocolHeader::decode(&buf[off..n]).unwrap();
        (h, p, from)
    }

    fn reply_datagram(
        exchange_id: u16,
        opcode: u8,
        acked: Option<u32>,
        needs_ack: bool,
        msg_counter: u32,
    ) -> Vec<u8> {
        let h = MessageHeader {
            session_id: 0,
            security_flags: 0,
            message_counter: msg_counter,
            source_node_id: None,
            destination: Destination::None,
        };
        let p = ProtocolHeader {
            initiator: false,
            needs_ack,
            acked_counter: acked,
            opcode,
            exchange_id,
            protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
            vendor_id: None,
        };
        let mut buf = h.encoded();
        p.encode(&mut buf);
        buf
    }

    /// retrans_base: 直近受信ありなら active interval（SAI）、無ければ/
    /// PEER_ACTIVE_WINDOW より古ければ idle interval（SII）。
    #[tokio::test]
    async fn retrans_base_picks_active_interval_on_recent_rx() {
        let cfg = MrpConfig {
            initial_interval: Duration::from_secs(5),
            active_interval: Duration::from_millis(300),
            ..MrpConfig::default()
        };
        assert_eq!(retrans_base(None, &cfg), Duration::from_secs(5));
        assert_eq!(
            retrans_base(Some(Instant::now()), &cfg),
            Duration::from_millis(300)
        );
        let stale = Instant::now() - (PEER_ACTIVE_WINDOW + Duration::from_millis(50));
        assert_eq!(retrans_base(Some(stale), &cfg), Duration::from_secs(5));
    }

    /// 実機バグの釘: ピアから受信した直後の再送は active interval で行う。
    /// SII=5000ms の Thread デバイスで、喪失した Sigma3 / StatusResponse の
    /// 回復が 5 秒張り付き、デバイス側タイムアウト（≈5s）に負けて購読
    /// priming が 0x80 死していた（2026-07-20 実機ワイヤで確認）。
    #[tokio::test]
    async fn send_reliable_retransmits_at_active_interval_after_peer_rx() {
        let responder = bind_local().await;
        let peer = responder.local_addr().unwrap();
        let transport = bind_local_transport().await;
        let mut ex = UnsecuredExchange::new(&transport, peer);
        let cfg = MrpConfig {
            initial_interval: Duration::from_secs(5), // idle のままなら再送は 5 秒後
            active_interval: Duration::from_millis(50),
            max_retries: 2,
            backoff: 1.0,
            jitter: 0.0,
        };

        let responder_task = tokio::spawn(async move {
            // 1 通目: 実応答（ack 同梱）でピア活動を作る。
            let (h, p, from) = read_msg(&responder).await;
            let reply = reply_datagram(p.exchange_id, 0x99, Some(h.message_counter), false, 7000);
            responder.send_to(&reply, from).await.unwrap();
            // 2 通目: ack しない。active interval なら 1 秒以内に再送が来る。
            let _ = read_msg(&responder).await;
            let mut buf = [0u8; MAX_DATAGRAM];
            let again =
                tokio::time::timeout(Duration::from_secs(1), responder.recv_from(&mut buf)).await;
            assert!(
                again.is_ok(),
                "no retransmission within 1s: active interval not applied"
            );
        });

        let first = ex
            .send_reliable(PROTOCOL_ID_SECURE_CHANNEL, 0x11, b"a", &cfg)
            .await
            .unwrap();
        assert!(first.is_some());
        let t0 = std::time::Instant::now();
        let err = ex
            .send_reliable(PROTOCOL_ID_SECURE_CHANNEL, 0x12, b"b", &cfg)
            .await
            .unwrap_err();
        assert!(matches!(err, ExchangeError::Timeout));
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "timeout took {:?}; idle interval used despite recent rx?",
            t0.elapsed()
        );
        responder_task.await.unwrap();
    }

    #[tokio::test]
    async fn send_reliable_completes_on_standalone_ack() {
        let responder = bind_local().await;
        let peer = responder.local_addr().unwrap();
        let transport = bind_local_transport().await;
        let mut ex = UnsecuredExchange::new(&transport, peer);
        assert_eq!(ex.last_sent_counter(), None);

        let responder_task = tokio::spawn(async move {
            let (h, p, from) = read_msg(&responder).await;
            assert!(p.needs_ack);
            assert!(p.initiator);
            let ack = reply_datagram(
                p.exchange_id,
                OPCODE_MRP_STANDALONE_ACK,
                Some(h.message_counter),
                false,
                7000,
            );
            responder.send_to(&ack, from).await.unwrap();
        });

        let res = ex
            .send_reliable(PROTOCOL_ID_SECURE_CHANNEL, 0x99, b"", &fast_cfg())
            .await
            .unwrap();
        assert!(res.is_none());
        assert!(ex.last_sent_counter().is_some());
        responder_task.await.unwrap();
    }

    #[tokio::test]
    async fn send_reliable_retransmits_same_counter() {
        let responder = bind_local().await;
        let peer = responder.local_addr().unwrap();
        let transport = bind_local_transport().await;
        let mut ex = UnsecuredExchange::new(&transport, peer);

        let responder_task = tokio::spawn(async move {
            let (h1, _, _) = read_msg(&responder).await; // 1通目は握りつぶす
            let (h2, p2, from) = read_msg(&responder).await; // 再送
            assert_eq!(h1.message_counter, h2.message_counter);
            let ack = reply_datagram(
                p2.exchange_id,
                OPCODE_MRP_STANDALONE_ACK,
                Some(h2.message_counter),
                false,
                7000,
            );
            responder.send_to(&ack, from).await.unwrap();
        });

        let res = ex
            .send_reliable(PROTOCOL_ID_SECURE_CHANNEL, 0x99, b"", &fast_cfg())
            .await
            .unwrap();
        assert!(res.is_none());
        responder_task.await.unwrap();
    }

    #[tokio::test]
    async fn send_reliable_times_out_without_ack() {
        let responder = bind_local().await; // 何も返さない
        let peer = responder.local_addr().unwrap();
        let transport = bind_local_transport().await;
        let mut ex = UnsecuredExchange::new(&transport, peer);
        let err = ex
            .send_reliable(PROTOCOL_ID_SECURE_CHANNEL, 0x99, b"", &fast_cfg())
            .await
            .unwrap_err();
        assert!(matches!(err, ExchangeError::Timeout));
    }

    #[tokio::test]
    async fn send_reliable_returns_piggybacked_response_and_acks_it() {
        let responder = bind_local().await;
        let peer = responder.local_addr().unwrap();
        let transport = bind_local_transport().await;
        let mut ex = UnsecuredExchange::new(&transport, peer);

        let responder_task = tokio::spawn(async move {
            let (h, p, from) = read_msg(&responder).await;
            // 実応答（StatusReport）に A フラグを相乗りさせ、こちらも ACK を要求する
            let reply = reply_datagram(
                p.exchange_id,
                OPCODE_STATUS_REPORT,
                Some(h.message_counter),
                true,
                8000,
            );
            responder.send_to(&reply, from).await.unwrap();
            // 相手側 MRP が standalone ack を返してくるはず
            let (_, ack_p, _) = read_msg(&responder).await;
            assert_eq!(ack_p.opcode, OPCODE_MRP_STANDALONE_ACK);
            assert_eq!(ack_p.acked_counter, Some(8000));
        });

        let res = ex
            .send_reliable(PROTOCOL_ID_SECURE_CHANNEL, 0x99, b"", &fast_cfg())
            .await
            .unwrap()
            .expect("real response expected");
        assert_eq!(res.proto.opcode, OPCODE_STATUS_REPORT);
        responder_task.await.unwrap();
    }

    #[tokio::test]
    async fn send_once_sends_a_single_reliable_flagged_datagram() {
        let responder = bind_local().await;
        let peer = responder.local_addr().unwrap();
        let transport = bind_local_transport().await;
        let mut ex = UnsecuredExchange::new(&transport, peer);
        assert_eq!(ex.last_sent_counter(), None);

        ex.send_once(PROTOCOL_ID_SECURE_CHANNEL, OPCODE_STATUS_REPORT, b"abort")
            .await
            .unwrap();
        assert!(ex.last_sent_counter().is_some());

        let (_, p, _) = read_msg(&responder).await;
        assert!(p.needs_ack, "R flag should still be set for peer's MRP");
        assert!(p.initiator);
        assert_eq!(p.opcode, OPCODE_STATUS_REPORT);

        // No retransmission follows even though the peer never acked.
        let mut buf = [0u8; MAX_DATAGRAM];
        let res =
            tokio::time::timeout(Duration::from_millis(150), responder.recv_from(&mut buf)).await;
        assert!(res.is_err(), "send_once must not retransmit");
    }

    #[tokio::test]
    async fn reliable_transport_disables_mrp() {
        use crate::transport::{ReliableChannel, RELIABLE_PEER};
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
            assert!(
                more.is_err(),
                "no standalone ack expected on reliable transport"
            );
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

    #[tokio::test]
    async fn recv_dedups_and_reacks_duplicates() {
        let responder = bind_local().await;
        let peer = responder.local_addr().unwrap();
        let transport = bind_local_transport().await;
        let local = transport.local_addr().unwrap();
        let mut ex = UnsecuredExchange::new(&transport, peer);
        let exchange_id = ex.exchange_id();

        let responder_task = tokio::spawn(async move {
            let msg = reply_datagram(exchange_id, OPCODE_STATUS_REPORT, None, true, 9000);
            // 同一メッセージを2回送る（重複）
            responder.send_to(&msg, local).await.unwrap();
            responder.send_to(&msg, local).await.unwrap();
            // ACK は2回来る（初回 + 重複への再 ACK）が、メッセージ本体は1度しか渡らない
            let (_, a1, _) = read_msg(&responder).await;
            let (_, a2, _) = read_msg(&responder).await;
            assert_eq!(a1.opcode, OPCODE_MRP_STANDALONE_ACK);
            assert_eq!(a1.acked_counter, Some(9000));
            assert_eq!(a2.opcode, OPCODE_MRP_STANDALONE_ACK);
            assert_eq!(a2.acked_counter, Some(9000));
        });

        let first = ex.recv(Duration::from_millis(500)).await.unwrap();
        assert_eq!(first.header.message_counter, 9000);
        // 2通目（重複）は渡ってこない → タイムアウト
        let err = ex.recv(Duration::from_millis(200)).await.unwrap_err();
        assert!(matches!(err, ExchangeError::Timeout));
        responder_task.await.unwrap();
    }

    /// jitter 純関数: r=0 で恒等、jitter=0 で恒等、上限は ×(1+jitter) 未満。
    #[test]
    fn jittered_interval_bounds() {
        let base = Duration::from_millis(300);
        assert_eq!(jittered_interval(base, 0.25, 0.0), base);
        assert_eq!(jittered_interval(base, 0.0, 0.9), base);
        let hi = jittered_interval(base, 0.25, 0.999_999);
        assert!(hi > base && hi < base.mul_f64(1.25));
    }

    /// unit_random: [0,1) に収まり、壊れて定数化していない（16 連続一致は
    /// 実装破損以外で起きない）。
    #[test]
    fn unit_random_in_range_and_varies() {
        let draws: Vec<f64> = (0..16).map(|_| unit_random()).collect();
        assert!(draws.iter().all(|r| (0.0..1.0).contains(r)));
        assert!(
            draws.iter().any(|r| *r != draws[0]),
            "16 draws all identical"
        );
    }

    /// total_budget はジッタ最悪値込みの上界（Issue #16 の op 予算が実待ちより
    /// 短くならない）。jitter=0 なら従来値。
    #[test]
    fn total_budget_includes_jitter_worst_case() {
        let cfg = MrpConfig::default();
        let base: Duration = {
            let mut c = cfg.clone();
            c.jitter = 0.0;
            total_budget(&c)
        };
        assert_eq!(
            total_budget(&cfg).as_millis(),
            base.mul_f64(1.0 + MRP_BACKOFF_JITTER).as_millis()
        );
    }

    // ---- ResponderExchange ----

    /// テスト用: peer（initiator）視点の生データグラムを組み立てる。
    fn initiator_datagram(
        exchange_id: u16,
        opcode: u8,
        msg_counter: u32,
        needs_ack: bool,
        acked: Option<u32>,
        payload: &[u8],
    ) -> Vec<u8> {
        let h = MessageHeader {
            session_id: 0,
            security_flags: 0,
            message_counter: msg_counter,
            source_node_id: None,
            destination: Destination::None,
        };
        let p = ProtocolHeader {
            initiator: true,
            needs_ack,
            acked_counter: acked,
            opcode,
            exchange_id,
            protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
            vendor_id: None,
        };
        let mut buf = h.encoded();
        p.encode(&mut buf);
        buf.extend_from_slice(payload);
        buf
    }

    /// `ResponderExchange::adopt` に渡す最初のメッセージのフィクスチャ。
    fn adopted_first(exchange_id: u16, counter: u32, needs_ack: bool) -> IncomingMessage {
        IncomingMessage {
            header: MessageHeader {
                session_id: 0,
                security_flags: 0,
                message_counter: counter,
                source_node_id: None,
                destination: Destination::None,
            },
            proto: ProtocolHeader {
                initiator: true,
                needs_ack,
                acked_counter: None,
                opcode: 0x20,
                exchange_id,
                protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
                vendor_id: None,
            },
            payload: b"req".to_vec(),
        }
    }

    /// テスト内ヘルパ（brief 記載）: 生 recv から `MessageHeader::decode` +
    /// `ProtocolHeader::decode` で最初の unsecured メッセージを取り出す。
    async fn recv_first_unsecured(t: &Transport) -> IncomingMessage {
        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, _from) = tokio::time::timeout(Duration::from_secs(5), t.recv_from(&mut buf))
            .await
            .expect("timed out waiting for first message")
            .expect("recv_from io error");
        let (header, off) = MessageHeader::decode(&buf[..n]).unwrap();
        let (proto, body_off) = ProtocolHeader::decode(&buf[off..n]).unwrap();
        IncomingMessage {
            header,
            proto,
            payload: buf[off + body_off..n].to_vec(),
        }
    }

    #[tokio::test]
    async fn responder_exchange_round_trips() {
        use crate::transport::{ReliableChannel, RELIABLE_PEER};
        let (a, b) = ReliableChannel::pair();
        let cfg = MrpConfig::default();
        let responder_cfg = cfg.clone();
        let init = tokio::spawn(async move {
            let mut ex = UnsecuredExchange::new(&a, RELIABLE_PEER);
            let reply = ex
                .send_reliable(PROTOCOL_ID_SECURE_CHANNEL, 0x20, b"req1", &cfg)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(reply.proto.opcode, 0x21);
            let fin = ex
                .send_reliable(PROTOCOL_ID_SECURE_CHANNEL, 0x22, b"req2", &cfg)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(fin.proto.opcode, OPCODE_STATUS_REPORT);
        });
        // responder 側: 最初のメッセージを recv → adopt → reply_reliable → reply_final
        let first = recv_first_unsecured(&b).await;
        let mut re = ResponderExchange::adopt(&b, RELIABLE_PEER, &first);
        let next = re
            .reply_reliable(PROTOCOL_ID_SECURE_CHANNEL, 0x21, b"resp1", &responder_cfg)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(next.payload, b"req2");
        re.reply_final(
            PROTOCOL_ID_SECURE_CHANNEL,
            OPCODE_STATUS_REPORT,
            &[0u8; 8],
            &responder_cfg,
        )
        .await
        .unwrap();
        init.await.unwrap();
    }

    #[tokio::test]
    async fn reply_reliable_dedupes_replay_then_retransmits_until_real_message() {
        let peer_sock = bind_local().await;
        let peer_addr = peer_sock.local_addr().unwrap();
        let transport = bind_local_transport().await;
        let local = transport.local_addr().unwrap();
        let exchange_id = 0xABCD;

        let first = adopted_first(exchange_id, 500, true);
        let mut re = ResponderExchange::adopt(&transport, peer_addr, &first);
        assert_eq!(re.first_needs_ack(), Some(500));

        let peer_task = tokio::spawn(async move {
            // resp1（ack piggyback 済み）の初回送出を受ける
            let (h1, p1, from) = read_msg(&peer_sock).await;
            assert!(!p1.initiator);
            assert!(p1.needs_ack);
            assert_eq!(p1.acked_counter, Some(500));

            // 元の req1 の重複を投げる → screen は standalone-ack のみ返すはず
            let dup = initiator_datagram(exchange_id, 0x20, 500, true, None, b"req1");
            peer_sock.send_to(&dup, local).await.unwrap();
            let (_, ack_p, _) = read_msg(&peer_sock).await;
            assert_eq!(ack_p.opcode, OPCODE_MRP_STANDALONE_ACK);
            assert_eq!(ack_p.acked_counter, Some(500));

            // resp1 の再送（同一 counter）を待ってから本物の req2 を返す
            let (h2, p2, _) = read_msg(&peer_sock).await;
            assert_eq!(h1.message_counter, h2.message_counter);
            assert_eq!(p2.opcode, 0x21);
            let req2 = initiator_datagram(
                exchange_id,
                0x22,
                501,
                false,
                Some(h2.message_counter),
                b"req2",
            );
            peer_sock.send_to(&req2, from).await.unwrap();
        });

        let next = re
            .reply_reliable(PROTOCOL_ID_SECURE_CHANNEL, 0x21, b"resp1", &fast_cfg())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(next.payload, b"req2");
        peer_task.await.unwrap();
    }

    #[tokio::test]
    async fn reply_reliable_completes_on_standalone_ack() {
        let peer_sock = bind_local().await;
        let peer_addr = peer_sock.local_addr().unwrap();
        let transport = bind_local_transport().await;
        let exchange_id = 0x9999;
        let first = adopted_first(exchange_id, 10, true);
        let mut re = ResponderExchange::adopt(&transport, peer_addr, &first);

        let peer_task = tokio::spawn(async move {
            let (h, p, from) = read_msg(&peer_sock).await;
            assert!(p.needs_ack);
            assert_eq!(p.acked_counter, Some(10));
            let ack = initiator_datagram(
                exchange_id,
                OPCODE_MRP_STANDALONE_ACK,
                11,
                false,
                Some(h.message_counter),
                &[],
            );
            peer_sock.send_to(&ack, from).await.unwrap();
        });

        let res = re
            .reply_reliable(PROTOCOL_ID_SECURE_CHANNEL, 0x21, b"resp1", &fast_cfg())
            .await
            .unwrap();
        assert!(res.is_none());
        peer_task.await.unwrap();
    }

    #[tokio::test]
    async fn reply_final_retransmits_same_counter_until_acked() {
        let peer_sock = bind_local().await;
        let peer_addr = peer_sock.local_addr().unwrap();
        let transport = bind_local_transport().await;
        let exchange_id = 0x1234;
        let first = adopted_first(exchange_id, 700, true);
        let mut re = ResponderExchange::adopt(&transport, peer_addr, &first);

        let peer_task = tokio::spawn(async move {
            let (h1, _, _) = read_msg(&peer_sock).await; // 1通目は握りつぶす
            let (h2, p2, from) = read_msg(&peer_sock).await; // 再送
            assert_eq!(h1.message_counter, h2.message_counter);
            assert_eq!(p2.acked_counter, Some(700));
            let ack = initiator_datagram(
                exchange_id,
                OPCODE_MRP_STANDALONE_ACK,
                701,
                false,
                Some(h2.message_counter),
                &[],
            );
            peer_sock.send_to(&ack, from).await.unwrap();
        });

        re.reply_final(
            PROTOCOL_ID_SECURE_CHANNEL,
            OPCODE_STATUS_REPORT,
            &[0u8; 8],
            &fast_cfg(),
        )
        .await
        .unwrap();
        peer_task.await.unwrap();
    }

    #[tokio::test]
    async fn reply_final_times_out_without_ack() {
        let peer_sock = bind_local().await; // 何も返さない
        let peer_addr = peer_sock.local_addr().unwrap();
        let transport = bind_local_transport().await;
        let first = adopted_first(0x1234, 700, true);
        let mut re = ResponderExchange::adopt(&transport, peer_addr, &first);
        let err = re
            .reply_final(
                PROTOCOL_ID_SECURE_CHANNEL,
                OPCODE_STATUS_REPORT,
                &[0u8; 8],
                &fast_cfg(),
            )
            .await
            .unwrap_err();
        assert!(matches!(err, ExchangeError::Timeout));
    }

    #[tokio::test]
    async fn screen_drops_non_initiator_and_foreign_exchange_id() {
        let peer_sock = bind_local().await;
        let peer_addr = peer_sock.local_addr().unwrap();
        let transport = bind_local_transport().await;
        let exchange_id = 0x55;
        let first = adopted_first(exchange_id, 1, true);
        let mut re = ResponderExchange::adopt(&transport, peer_addr, &first);

        // initiator == false（応答側どうしの迷子トラフィック）は捨てる
        let mut not_initiator = MessageHeader {
            session_id: 0,
            security_flags: 0,
            message_counter: 2,
            source_node_id: None,
            destination: Destination::None,
        }
        .encoded();
        ProtocolHeader {
            initiator: false,
            needs_ack: false,
            acked_counter: None,
            opcode: 0x20,
            exchange_id,
            protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
            vendor_id: None,
        }
        .encode(&mut not_initiator);
        assert!(re
            .screen(&not_initiator, peer_addr)
            .await
            .unwrap()
            .is_none());

        // exchange_id 不一致は捨てる
        let foreign = initiator_datagram(exchange_id.wrapping_add(1), 0x20, 3, true, None, b"x");
        assert!(re.screen(&foreign, peer_addr).await.unwrap().is_none());
    }
}
