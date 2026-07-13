//! Exchange layer and MRP reliability (spec §4.6, §4.12).

use std::net::SocketAddr;
use std::time::Duration;

use tokio::time::Instant;

use crate::counter::{RxWindow, TxCounter};
use crate::message::{
    Destination, MessageError, MessageHeader, ProtocolHeader, OPCODE_MRP_STANDALONE_ACK,
    PROTOCOL_ID_SECURE_CHANNEL,
};
use crate::transport::{UdpTransport, MAX_DATAGRAM};

/// MRP retransmission parameters (spec 4.12; defaults follow chip defaults).
#[derive(Debug, Clone)]
pub struct MrpConfig {
    pub initial_interval: Duration,
    pub max_retries: u32,
    pub backoff: f64,
}

impl Default for MrpConfig {
    fn default() -> Self {
        Self {
            initial_interval: Duration::from_millis(300),
            max_retries: 4,
            backoff: 1.6,
        }
    }
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
    transport: &'t UdpTransport,
    peer: SocketAddr,
    exchange_id: u16,
    source_node_id: u64,
    counter: TxCounter,
    rx_window: RxWindow,
    last_sent_counter: Option<u32>,
}

impl<'t> UnsecuredExchange<'t> {
    pub fn new(transport: &'t UdpTransport, peer: SocketAddr) -> Self {
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
        if !self.rx_window.check_and_commit(header.message_counter) {
            if proto.needs_ack {
                self.send_standalone_ack(header.message_counter).await?;
            }
            return Ok(None);
        }
        if proto.needs_ack {
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
        let (datagram, our_counter) = self.build(protocol_id, opcode, true, None, payload);
        self.last_sent_counter = Some(our_counter);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::message::{
        Destination, MessageHeader, ProtocolHeader, OPCODE_MRP_STANDALONE_ACK,
        OPCODE_STATUS_REPORT, PROTOCOL_ID_SECURE_CHANNEL,
    };
    use crate::transport::{UdpTransport, MAX_DATAGRAM};
    use std::net::SocketAddr;
    use std::time::Duration;

    fn fast_cfg() -> MrpConfig {
        MrpConfig {
            initial_interval: Duration::from_millis(50),
            max_retries: 2,
            backoff: 1.0,
        }
    }

    async fn bind_local() -> UdpTransport {
        UdpTransport::bind_addr("[::1]:0".parse().unwrap())
            .await
            .unwrap()
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

    #[tokio::test]
    async fn send_reliable_completes_on_standalone_ack() {
        let responder = bind_local().await;
        let peer = responder.local_addr().unwrap();
        let transport = bind_local().await;
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
        let transport = bind_local().await;
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
        let transport = bind_local().await;
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
        let transport = bind_local().await;
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
        let transport = bind_local().await;
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
    async fn recv_dedups_and_reacks_duplicates() {
        let responder = bind_local().await;
        let peer = responder.local_addr().unwrap();
        let transport = bind_local().await;
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
}
