//! Secure unicast session and MRP-reliable exchanges over it (spec §4.7, §4.12).
//!
//! Mirrors the M1 unsecured exchange semantics — retransmit, standalone ack,
//! RxWindow dedup — but seals every datagram with the session keys. Message
//! counters and the replay window are session-scoped (not per exchange), so
//! this type owns them and exchanges are just an `exchange_id` argument.

use std::net::SocketAddr;
use std::time::Duration;

use tokio::time::Instant;

use crate::counter::{RxWindow, TxCounter};
use crate::crypto::{open_message, seal_message, CryptoError, OpenError};
use crate::exchange::{IncomingMessage, MrpConfig};
use crate::message::{
    Destination, MessageError, MessageHeader, ProtocolHeader, OPCODE_MRP_STANDALONE_ACK,
    PROTOCOL_ID_SECURE_CHANNEL,
};
use crate::transport::{UdpTransport, MAX_DATAGRAM};

/// The three session keys derived during CASE/PASE (spec §4.7, §4.13).
pub struct SessionKeys {
    pub i2r: [u8; 16],
    pub r2i: [u8; 16],
    pub attestation_challenge: [u8; 16],
}

#[derive(Debug)]
pub enum SessionError {
    Timeout,
    Io(std::io::Error),
    Message(MessageError),
    Crypto(CryptoError),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Timeout => write!(f, "no acknowledgement within MRP retry budget"),
            SessionError::Io(e) => write!(f, "transport error: {e}"),
            SessionError::Message(e) => write!(f, "peer sent malformed message: {e}"),
            SessionError::Crypto(e) => write!(f, "session crypto error: {e}"),
        }
    }
}

impl std::error::Error for SessionError {}

impl From<std::io::Error> for SessionError {
    fn from(e: std::io::Error) -> Self {
        SessionError::Io(e)
    }
}

impl From<MessageError> for SessionError {
    fn from(e: MessageError) -> Self {
        SessionError::Message(e)
    }
}

impl From<CryptoError> for SessionError {
    fn from(e: CryptoError) -> Self {
        SessionError::Crypto(e)
    }
}

/// One secured unicast session, this side as the exchange initiator, with MRP.
pub struct SecureSession<'t> {
    transport: &'t UdpTransport,
    peer: SocketAddr,
    local_session_id: u16,
    peer_session_id: u16,
    keys: SessionKeys,
    local_node_id: u64,
    peer_node_id: u64,
    counter: TxCounter,
    rx_window: RxWindow,
}

impl<'t> SecureSession<'t> {
    pub fn new(
        transport: &'t UdpTransport,
        peer: SocketAddr,
        local_session_id: u16,
        peer_session_id: u16,
        keys: SessionKeys,
        local_node_id: u64,
        peer_node_id: u64,
    ) -> Self {
        Self {
            transport,
            peer,
            local_session_id,
            peer_session_id,
            keys,
            local_node_id,
            peer_node_id,
            counter: TxCounter::new_random(),
            rx_window: RxWindow::new(),
        }
    }

    pub fn peer_node_id(&self) -> u64 {
        self.peer_node_id
    }

    /// Generates a random exchange id for a new exchange on this session.
    pub fn new_exchange_id() -> u16 {
        let mut b = [0u8; 2];
        getrandom::getrandom(&mut b).expect("os rng");
        u16::from_le_bytes(b)
    }

    /// Seals a message for the peer; returns the datagram and the plaintext
    /// message counter used (so callers can match it against an ack).
    #[allow(clippy::too_many_arguments)]
    fn seal(
        &mut self,
        exchange_id: u16,
        protocol_id: u16,
        opcode: u8,
        needs_ack: bool,
        acked_counter: Option<u32>,
        payload: &[u8],
    ) -> Result<(Vec<u8>, u32), SessionError> {
        let message_counter = self.counter.next();
        let header = MessageHeader {
            session_id: self.peer_session_id,
            security_flags: 0,
            message_counter,
            source_node_id: None,
            destination: Destination::None,
        };
        let proto = ProtocolHeader {
            initiator: true,
            needs_ack,
            acked_counter,
            opcode,
            exchange_id,
            protocol_id,
            vendor_id: None,
        };
        let datagram = seal_message(&self.keys.i2r, &header, &proto, payload, self.local_node_id)?;
        Ok((datagram, message_counter))
    }

    async fn send_standalone_ack(
        &mut self,
        exchange_id: u16,
        acked: u32,
    ) -> Result<(), SessionError> {
        let (datagram, _) = self.seal(
            exchange_id,
            PROTOCOL_ID_SECURE_CHANNEL,
            OPCODE_MRP_STANDALONE_ACK,
            false,
            Some(acked),
            &[],
        )?;
        self.transport.send_to(&datagram, self.peer).await?;
        Ok(())
    }

    /// Decrypts a datagram and screens it for the given exchange. Returns
    /// `None` for foreign or duplicate traffic the caller should skip
    /// (duplicates are re-acked here). Standalone acks pass screening and
    /// are returned as `Some`; callers filter them by opcode.
    async fn screen(
        &mut self,
        buf: &[u8],
        from: SocketAddr,
        exchange_id: u16,
    ) -> Result<Option<IncomingMessage>, SessionError> {
        if from != self.peer {
            return Ok(None);
        }
        // 平文ヘッダだけ先に見て session id を確認する（復号前フィルタ）。
        // decode 失敗（DSIZ 予約値含む）や session id 不一致は不正/他セッション
        // のデータグラムとして無視する（DoS 耐性、エラーを伝播しない）。
        let header_peek = match MessageHeader::decode(buf) {
            Ok((h, _)) => h,
            Err(_) => return Ok(None),
        };
        if header_peek.session_id != self.local_session_id {
            return Ok(None);
        }
        let (header, proto, payload) = match open_message(&self.keys.r2i, buf, self.peer_node_id) {
            Ok(v) => v,
            Err(OpenError::Message(_)) | Err(OpenError::Crypto(_)) => return Ok(None),
        };
        if !self.rx_window.check_and_commit(header.message_counter) {
            if proto.needs_ack {
                self.send_standalone_ack(exchange_id, header.message_counter)
                    .await?;
            }
            return Ok(None);
        }
        if proto.exchange_id != exchange_id || proto.initiator {
            return Ok(None);
        }
        if proto.needs_ack {
            self.send_standalone_ack(exchange_id, header.message_counter)
                .await?;
        }
        Ok(Some(IncomingMessage {
            header,
            proto,
            payload,
        }))
    }

    /// Sends a reliability-flagged message and retransmits until the peer
    /// acknowledges it. Returns the peer's real response if one carried the
    /// ack (or arrived on the exchange), `None` for a standalone ack.
    pub async fn send_reliable(
        &mut self,
        exchange_id: u16,
        protocol_id: u16,
        opcode: u8,
        payload: &[u8],
        cfg: &MrpConfig,
    ) -> Result<Option<IncomingMessage>, SessionError> {
        let (datagram, our_counter) =
            self.seal(exchange_id, protocol_id, opcode, true, None, payload)?;
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
                let Some(msg) = self.screen(&buf[..n], from, exchange_id).await? else {
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
                return Err(SessionError::Timeout);
            }
            interval = interval.mul_f64(cfg.backoff);
        }
    }

    /// Waits for the next real (non-ack) message on the given exchange.
    pub async fn recv(
        &mut self,
        exchange_id: u16,
        timeout: Duration,
    ) -> Result<IncomingMessage, SessionError> {
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
            let Some(msg) = self.screen(&buf[..n], from, exchange_id).await? else {
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
    use crate::crypto::seal_message;
    use crate::message::{
        Destination, MessageHeader, ProtocolHeader, OPCODE_MRP_STANDALONE_ACK,
        OPCODE_STATUS_REPORT, PROTOCOL_ID_SECURE_CHANNEL,
    };
    use crate::transport::{UdpTransport, MAX_DATAGRAM};
    use std::time::Duration;

    const I2R: [u8; 16] = [0x11; 16];
    const R2I: [u8; 16] = [0x22; 16];
    const OUR_NODE: u64 = 0xAAAA;
    const DEV_NODE: u64 = 0xBBBB;
    const LOCAL_SID: u16 = 0x1234;
    const PEER_SID: u16 = 0x5678;

    fn keys() -> SessionKeys {
        SessionKeys {
            i2r: I2R,
            r2i: R2I,
            attestation_challenge: [0; 16],
        }
    }

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

    /// デバイス→controller のセキュアデータグラムを作る。
    fn device_datagram(
        exchange_id: u16,
        opcode: u8,
        acked: Option<u32>,
        needs_ack: bool,
        counter: u32,
        payload: &[u8],
    ) -> Vec<u8> {
        let header = MessageHeader {
            session_id: LOCAL_SID, // デバイスは「こちらの」session id 宛に送る
            security_flags: 0,
            message_counter: counter,
            source_node_id: None,
            destination: Destination::None,
        };
        let proto = ProtocolHeader {
            initiator: false,
            needs_ack,
            acked_counter: acked,
            opcode,
            exchange_id,
            protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
            vendor_id: None,
        };
        seal_message(&R2I, &header, &proto, payload, DEV_NODE).unwrap()
    }

    /// デバイス側で受信 → 復号して (header, proto) を返す。
    fn open_from_controller(buf: &[u8]) -> (MessageHeader, ProtocolHeader, Vec<u8>) {
        crate::crypto::open_message(&I2R, buf, OUR_NODE).unwrap()
    }

    #[tokio::test]
    async fn send_reliable_encrypts_and_completes_on_sealed_ack() {
        let device = bind_local().await;
        let peer = device.local_addr().unwrap();
        let transport = bind_local().await;
        let mut s = SecureSession::new(
            &transport,
            peer,
            LOCAL_SID,
            PEER_SID,
            keys(),
            OUR_NODE,
            DEV_NODE,
        );
        let ex = SecureSession::new_exchange_id();

        let dev = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, from) = device.recv_from(&mut buf).await.unwrap();
            // 平文では読めない（先頭ヘッダ以外は暗号化されている）
            let (h, p, body) = open_from_controller(&buf[..n]);
            assert_eq!(h.session_id, PEER_SID); // デバイス側 session id 宛
            assert!(p.needs_ack);
            assert_eq!(body, b"ping");
            let ack = device_datagram(
                p.exchange_id,
                OPCODE_MRP_STANDALONE_ACK,
                Some(h.message_counter),
                false,
                9000,
                &[],
            );
            device.send_to(&ack, from).await.unwrap();
        });

        let res = s
            .send_reliable(ex, PROTOCOL_ID_SECURE_CHANNEL, 0x99, b"ping", &fast_cfg())
            .await
            .unwrap();
        assert!(res.is_none());
        dev.await.unwrap();
    }

    #[tokio::test]
    async fn recv_decrypts_dedups_and_acks() {
        let device = bind_local().await;
        let peer = device.local_addr().unwrap();
        let transport = bind_local().await;
        let local = transport.local_addr().unwrap();
        let mut s = SecureSession::new(
            &transport,
            peer,
            LOCAL_SID,
            PEER_SID,
            keys(),
            OUR_NODE,
            DEV_NODE,
        );
        let ex = SecureSession::new_exchange_id();

        let dev = tokio::spawn(async move {
            let msg = device_datagram(ex, OPCODE_STATUS_REPORT, None, true, 500, b"report");
            device.send_to(&msg, local).await.unwrap();
            device.send_to(&msg, local).await.unwrap(); // 重複
                                                        // ACK は暗号化されて 2 回返る
            for _ in 0..2 {
                let mut buf = [0u8; MAX_DATAGRAM];
                let (n, _) = device.recv_from(&mut buf).await.unwrap();
                let (_, p, _) = open_from_controller(&buf[..n]);
                assert_eq!(p.opcode, OPCODE_MRP_STANDALONE_ACK);
                assert_eq!(p.acked_counter, Some(500));
            }
        });

        let got = s.recv(ex, Duration::from_millis(500)).await.unwrap();
        assert_eq!(got.payload, b"report");
        // 重複は渡ってこない
        assert!(matches!(
            s.recv(ex, Duration::from_millis(200)).await,
            Err(SessionError::Timeout)
        ));
        dev.await.unwrap();
    }

    #[tokio::test]
    async fn ignores_wrong_key_wrong_session_and_wrong_exchange() {
        let device = bind_local().await;
        let peer = device.local_addr().unwrap();
        let transport = bind_local().await;
        let local = transport.local_addr().unwrap();
        let mut s = SecureSession::new(
            &transport,
            peer,
            LOCAL_SID,
            PEER_SID,
            keys(),
            OUR_NODE,
            DEV_NODE,
        );
        let ex = SecureSession::new_exchange_id();

        let dev = tokio::spawn(async move {
            // 鍵違い（I2R で封緘 = 復号失敗）
            let header = MessageHeader {
                session_id: LOCAL_SID,
                security_flags: 0,
                message_counter: 1,
                source_node_id: None,
                destination: Destination::None,
            };
            let proto = ProtocolHeader {
                initiator: false,
                needs_ack: true,
                acked_counter: None,
                opcode: OPCODE_STATUS_REPORT,
                exchange_id: ex,
                protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
                vendor_id: None,
            };
            let bad_key = seal_message(&I2R, &header, &proto, b"x", DEV_NODE).unwrap();
            device.send_to(&bad_key, local).await.unwrap();
            // session id 違い
            let mut h2 = header;
            h2.session_id = 0x9999;
            let bad_sid = seal_message(&R2I, &h2, &proto, b"x", DEV_NODE).unwrap();
            device.send_to(&bad_sid, local).await.unwrap();
            // exchange 違い（正しく封緘されるが screening で落ちる）
            let other_ex = device_datagram(
                ex.wrapping_add(1),
                OPCODE_STATUS_REPORT,
                None,
                true,
                7,
                b"x",
            );
            device.send_to(&other_ex, local).await.unwrap();
        });

        assert!(matches!(
            s.recv(ex, Duration::from_millis(300)).await,
            Err(SessionError::Timeout)
        ));
        dev.await.unwrap();
    }
}
