//! Secure unicast session and MRP-reliable exchanges over it (spec §4.7, §4.12).
//!
//! Mirrors the M1 unsecured exchange semantics — retransmit, standalone ack,
//! RxWindow dedup — but seals every datagram with the session keys. Message
//! counters and the replay window are session-scoped (not per exchange), so
//! this type owns them and exchanges are just an `exchange_id` argument.

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use crate::counter::{RxWindow, TxCounter};
use crate::crypto::{open_message, seal_message, CryptoError, OpenError};
use crate::exchange::{IncomingMessage, MrpConfig};
use crate::message::{
    Destination, MessageError, MessageHeader, ProtocolHeader, OPCODE_MRP_STANDALONE_ACK,
    PROTOCOL_ID_SECURE_CHANNEL,
};
use crate::transport::{Transport, MAX_DATAGRAM};

/// How long to wait for a ReportData/InvokeResponse/StatusResponse after the
/// request's own reliable send already completed (i.e. the response didn't
/// piggyback on the ack). Generous relative to MRP's own retry budget since
/// the device may be doing real work (e.g. actuating a relay) before replying.
const IM_RECV_TIMEOUT: Duration = Duration::from_secs(10);

/// MRP の全リトライを使い切るまでの待ち時間合計。reliable transport では
/// 「同じ体感タイムアウト」で実応答を待つ予算として使う（`exchange.rs` の
/// 同名関数と同じ計算——secure 経路にも同じゲーティングを対称に適用する）。
fn total_budget(cfg: &MrpConfig) -> Duration {
    let mut total = Duration::ZERO;
    let mut interval = cfg.initial_interval;
    for _ in 0..=cfg.max_retries {
        total += interval;
        interval = interval.mul_f64(cfg.backoff);
    }
    total
}

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
    Im(crate::im::ImError),
    UnexpectedOpcode(u8),
}

impl std::fmt::Display for SessionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SessionError::Timeout => write!(f, "no acknowledgement within MRP retry budget"),
            SessionError::Io(e) => write!(f, "transport error: {e}"),
            SessionError::Message(e) => write!(f, "peer sent malformed message: {e}"),
            SessionError::Crypto(e) => write!(f, "session crypto error: {e}"),
            SessionError::Im(e) => write!(f, "interaction model error: {e}"),
            SessionError::UnexpectedOpcode(op) => {
                write!(f, "unexpected protocol opcode 0x{op:02X} on secure session")
            }
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
pub struct SecureSession {
    transport: Arc<Transport>,
    peer: SocketAddr,
    local_session_id: u16,
    peer_session_id: u16,
    keys: SessionKeys,
    local_node_id: u64,
    peer_node_id: u64,
    counter: TxCounter,
    rx_window: RxWindow,
}

impl SecureSession {
    pub fn new(
        transport: Arc<Transport>,
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

    /// PASE で確立したセッションの Attestation Challenge (spec §11.17.5.4 が
    /// attestation 署名の対象に含める)。
    pub fn attestation_challenge(&self) -> [u8; 16] {
        self.keys.attestation_challenge
    }

    /// Generates a random exchange id for a new exchange on this session.
    pub fn new_exchange_id() -> u16 {
        let mut b = [0u8; 2];
        getrandom::getrandom(&mut b).expect("os rng");
        u16::from_le_bytes(b)
    }

    /// Seals a message for the peer; returns the datagram and the plaintext
    /// message counter used (so callers can match it against an ack).
    ///
    /// `initiator` marks our role on `exchange_id`. Our own requests always
    /// carry `true` (we are always the exchange initiator in M2). Standalone
    /// acks may need `false`: when acking a message on an exchange the
    /// *device* initiated (e.g. a device-initiated secured StatusReport), we
    /// are the non-initiator of that exchange, and the peer can only match
    /// our ack if it carries that role correctly.
    #[allow(clippy::too_many_arguments)]
    fn seal(
        &mut self,
        exchange_id: u16,
        initiator: bool,
        protocol_id: u16,
        opcode: u8,
        needs_ack: bool,
        acked_counter: Option<u32>,
        payload: &[u8],
    ) -> Result<(Vec<u8>, u32), SessionError> {
        let needs_ack = needs_ack && !self.transport.is_reliable();
        let message_counter = self.counter.next();
        let header = MessageHeader {
            session_id: self.peer_session_id,
            security_flags: 0,
            message_counter,
            source_node_id: None,
            destination: Destination::None,
        };
        let proto = ProtocolHeader {
            initiator,
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

    /// Sends a standalone ack for `acked` on `exchange_id`, with our role on
    /// that exchange given explicitly by `initiator` (see `seal`'s doc for
    /// why this can't just be assumed to be `true`).
    async fn send_standalone_ack(
        &mut self,
        exchange_id: u16,
        initiator: bool,
        acked: u32,
    ) -> Result<(), SessionError> {
        let (datagram, _) = self.seal(
            exchange_id,
            initiator,
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
        // RxWindow の重複検知はセッション単位（exchange 単位ではない）なので、
        // exchange フィルタより前にコミットする（コメント: この順序は意図的）。
        if !self.rx_window.check_and_commit(header.message_counter) {
            // 重複の再送。ack は必ずメッセージ自身の exchange id / role で
            // 発行する — 呼び出し元の filter exchange ではない。他 exchange
            // （デバイス起点など）宛の重複を誤った exchange/role で ack する
            // と相手が突合できず、再送予算を使い切るまでリトライし続ける。
            if proto.needs_ack && !self.transport.is_reliable() {
                self.send_standalone_ack(
                    proto.exchange_id,
                    !proto.initiator,
                    header.message_counter,
                )
                .await?;
            }
            return Ok(None);
        }
        // 認証済みの新規メッセージは、こちらの exchange 宛かどうかに関わらず
        // needs_ack ならここで ack する（初回配送の時点で ack する — 重複の
        // 再送を待たない）。exchange フィルタは配送の可否だけを決め、ack の
        // 有無には影響しない。
        if proto.needs_ack && !self.transport.is_reliable() {
            self.send_standalone_ack(proto.exchange_id, !proto.initiator, header.message_counter)
                .await?;
        }
        if proto.exchange_id != exchange_id || proto.initiator {
            return Ok(None);
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
        if self.transport.is_reliable() {
            // BTP: transport が信頼性を持つ。1 回送って実応答を待つだけ。
            let (datagram, _) =
                self.seal(exchange_id, true, protocol_id, opcode, false, None, payload)?;
            self.transport.send_to(&datagram, self.peer).await?;
            let budget = total_budget(cfg);
            return match self.recv(exchange_id, budget).await {
                Ok(msg) => Ok(Some(msg)),
                Err(e) => Err(e),
            };
        }
        let (datagram, our_counter) =
            self.seal(exchange_id, true, protocol_id, opcode, true, None, payload)?;
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

    /// Reads a single attribute over the Interaction Model (spec §8.9.2).
    /// If the device's ReportData doesn't suppress the response, this also
    /// sends back the required StatusResponse(SUCCESS) — best-effort: the
    /// read has already succeeded from our side, so we don't fail it if
    /// that ack round-trip itself times out.
    pub async fn read_attribute(
        &mut self,
        endpoint: u16,
        cluster: u32,
        attribute: u32,
        cfg: &MrpConfig,
    ) -> Result<crate::im::ImValue, SessionError> {
        use crate::im::{self, ImError};
        let exchange_id = Self::new_exchange_id();
        let req = im::encode_read_request(endpoint, cluster, attribute);
        let resp = self
            .send_reliable(
                exchange_id,
                im::PROTOCOL_ID_IM,
                im::OPCODE_READ_REQUEST,
                &req,
                cfg,
            )
            .await?;
        let msg = match resp {
            Some(m) => m,
            None => self.recv(exchange_id, IM_RECV_TIMEOUT).await?,
        };
        match msg.proto.opcode {
            im::OPCODE_REPORT_DATA => {
                let rd = im::decode_report_data(&msg.payload).map_err(SessionError::Im)?;
                if !rd.suppress_response {
                    // Best-effort close: the read already succeeded (we
                    // have `rd` in hand), so a lost ack on this trailing
                    // StatusResponse must not turn it into an error here —
                    // it's the peer's retransmit problem, not ours.
                    let ok = im::encode_status_response(0);
                    let _ = self
                        .send_reliable(
                            exchange_id,
                            im::PROTOCOL_ID_IM,
                            im::OPCODE_STATUS_RESPONSE,
                            &ok,
                            cfg,
                        )
                        .await;
                }
                if let Some(status) = rd.status {
                    return Err(SessionError::Im(ImError::AttributeStatus(status)));
                }
                rd.value
                    .ok_or(SessionError::Im(ImError::Malformed("no value")))
            }
            im::OPCODE_STATUS_RESPONSE => {
                let s = im::decode_status_response(&msg.payload).map_err(SessionError::Im)?;
                Err(SessionError::Im(ImError::StatusResponse(s)))
            }
            op => Err(SessionError::UnexpectedOpcode(op)),
        }
    }

    /// Invokes a single command over the Interaction Model (spec §8.9.4).
    ///
    /// The interaction ends with the InvokeResponse itself: for a
    /// non-chunked response (M2's only case) we send no closing
    /// StatusResponse — this mirrors CHIP's `CommandSender`, where the MRP
    /// ack of the InvokeResponse is what closes the exchange, not a
    /// follow-up message. The InvokeResponseMessage's own `SuppressResponse`
    /// field is intentionally ignored in M2.
    pub async fn invoke(
        &mut self,
        endpoint: u16,
        cluster: u32,
        command: u32,
        fields_tlv: Option<&[u8]>,
        cfg: &MrpConfig,
    ) -> Result<crate::im::InvokeOutcome, SessionError> {
        use crate::im::{self, ImError};
        let exchange_id = Self::new_exchange_id();
        let req = im::encode_invoke_request(endpoint, cluster, command, fields_tlv);
        let resp = self
            .send_reliable(
                exchange_id,
                im::PROTOCOL_ID_IM,
                im::OPCODE_INVOKE_REQUEST,
                &req,
                cfg,
            )
            .await?;
        let msg = match resp {
            Some(m) => m,
            None => self.recv(exchange_id, IM_RECV_TIMEOUT).await?,
        };
        match msg.proto.opcode {
            im::OPCODE_INVOKE_RESPONSE => {
                let outcome = im::decode_invoke_response(&msg.payload).map_err(SessionError::Im)?;
                if outcome.status != 0 {
                    return Err(SessionError::Im(ImError::CommandStatus {
                        status: outcome.status,
                        cluster_status: outcome.cluster_status,
                    }));
                }
                Ok(outcome)
            }
            im::OPCODE_STATUS_RESPONSE => {
                let s = im::decode_status_response(&msg.payload).map_err(SessionError::Im)?;
                Err(SessionError::Im(ImError::StatusResponse(s)))
            }
            op => Err(SessionError::UnexpectedOpcode(op)),
        }
    }

    /// Invokes a single command over the Interaction Model, optionally as a
    /// *timed* invoke (spec §8.5, タイムド呼び出し), and returns the full
    /// `InvokeResponseData` (status plus any CommandFields the device sent
    /// back) rather than the fields-discarding `InvokeOutcome` that `invoke`
    /// returns.
    ///
    /// When `timed_timeout_ms` is `Some(t)`, a `TimedRequest(t)` is sent
    /// first on a freshly allocated exchange, and the following
    /// `InvokeRequest` (TimedRequest flag set) is sent on that *same*
    /// exchange — spec §8.5.1 requires the timed action to arrive on the
    /// exchange the TimedRequest opened, within the window it establishes.
    /// A non-SUCCESS `StatusResponse` to the TimedRequest itself aborts
    /// before the InvokeRequest is ever sent. When `timed_timeout_ms` is
    /// `None`, this sends a plain (non-timed) InvokeRequest, identical to
    /// `invoke`'s own request — only the response decoding differs.
    pub async fn invoke_for_data(
        &mut self,
        endpoint: u16,
        cluster: u32,
        command: u32,
        fields_tlv: Option<&[u8]>,
        timed_timeout_ms: Option<u16>,
        cfg: &MrpConfig,
    ) -> Result<crate::im::InvokeResponseData, SessionError> {
        use crate::im::{self, ImError};
        let exchange_id = Self::new_exchange_id();

        if let Some(timeout_ms) = timed_timeout_ms {
            let timed_req = im::encode_timed_request(timeout_ms);
            let resp = self
                .send_reliable(
                    exchange_id,
                    im::PROTOCOL_ID_IM,
                    im::OPCODE_TIMED_REQUEST,
                    &timed_req,
                    cfg,
                )
                .await?;
            let msg = match resp {
                Some(m) => m,
                None => self.recv(exchange_id, IM_RECV_TIMEOUT).await?,
            };
            match msg.proto.opcode {
                im::OPCODE_STATUS_RESPONSE => {
                    let s = im::decode_status_response(&msg.payload).map_err(SessionError::Im)?;
                    if s != 0 {
                        return Err(SessionError::Im(ImError::StatusResponse(s)));
                    }
                }
                op => return Err(SessionError::UnexpectedOpcode(op)),
            }
        }

        let req = if timed_timeout_ms.is_some() {
            im::encode_invoke_request_timed(endpoint, cluster, command, fields_tlv)
        } else {
            im::encode_invoke_request(endpoint, cluster, command, fields_tlv)
        };
        let resp = self
            .send_reliable(
                exchange_id,
                im::PROTOCOL_ID_IM,
                im::OPCODE_INVOKE_REQUEST,
                &req,
                cfg,
            )
            .await?;
        let msg = match resp {
            Some(m) => m,
            None => self.recv(exchange_id, IM_RECV_TIMEOUT).await?,
        };
        match msg.proto.opcode {
            im::OPCODE_INVOKE_RESPONSE => {
                let data =
                    im::decode_invoke_response_data(&msg.payload).map_err(SessionError::Im)?;
                if data.status != 0 {
                    return Err(SessionError::Im(ImError::CommandStatus {
                        status: data.status,
                        cluster_status: data.cluster_status,
                    }));
                }
                Ok(data)
            }
            im::OPCODE_STATUS_RESPONSE => {
                let s = im::decode_status_response(&msg.payload).map_err(SessionError::Im)?;
                Err(SessionError::Im(ImError::StatusResponse(s)))
            }
            op => Err(SessionError::UnexpectedOpcode(op)),
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
        protocol_id: u16,
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
            protocol_id,
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
        let transport = Arc::new(Transport::Udp(Arc::new(bind_local().await)));
        let mut s = SecureSession::new(
            Arc::clone(&transport),
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
                PROTOCOL_ID_SECURE_CHANNEL,
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
        let transport = Arc::new(Transport::Udp(Arc::new(bind_local().await)));
        let local = transport.local_addr().unwrap();
        let mut s = SecureSession::new(
            Arc::clone(&transport),
            peer,
            LOCAL_SID,
            PEER_SID,
            keys(),
            OUR_NODE,
            DEV_NODE,
        );
        let ex = SecureSession::new_exchange_id();

        let dev = tokio::spawn(async move {
            let msg = device_datagram(
                ex,
                PROTOCOL_ID_SECURE_CHANNEL,
                OPCODE_STATUS_REPORT,
                None,
                true,
                500,
                b"report",
            );
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
        let transport = Arc::new(Transport::Udp(Arc::new(bind_local().await)));
        let local = transport.local_addr().unwrap();
        let mut s = SecureSession::new(
            Arc::clone(&transport),
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
                PROTOCOL_ID_SECURE_CHANNEL,
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

    /// Regression for the MRP ack-attribution bug: a needs-ack message that
    /// arrives on an exchange the *device* initiated (foreign to our own
    /// `exchange_id` filter — e.g. a device-initiated secured StatusReport)
    /// must still be acked, and the ack must carry that message's own
    /// exchange id with us as the non-initiator of THEIR exchange — not our
    /// filter's exchange id with `initiator: true`, which the peer could
    /// never match.
    #[tokio::test]
    async fn acks_foreign_exchange_needs_ack_message_with_its_own_exchange_id() {
        let device = bind_local().await;
        let peer = device.local_addr().unwrap();
        let transport = Arc::new(Transport::Udp(Arc::new(bind_local().await)));
        let local = transport.local_addr().unwrap();
        let mut s = SecureSession::new(
            Arc::clone(&transport),
            peer,
            LOCAL_SID,
            PEER_SID,
            keys(),
            OUR_NODE,
            DEV_NODE,
        );
        let ex = SecureSession::new_exchange_id();
        let foreign_ex = ex.wrapping_add(1);

        let dev = tokio::spawn(async move {
            // デバイス起点の別 exchange 上のメッセージ（例: セキュアな
            // StatusReport をデバイス側から自分の exchange で送ってくる
            // ケース）。initiator: true はデバイスが「その exchange の」
            // initiator であることを示す。
            let header = MessageHeader {
                session_id: LOCAL_SID,
                security_flags: 0,
                message_counter: 700,
                source_node_id: None,
                destination: Destination::None,
            };
            let proto = ProtocolHeader {
                initiator: true,
                needs_ack: true,
                acked_counter: None,
                opcode: OPCODE_STATUS_REPORT,
                exchange_id: foreign_ex,
                protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
                vendor_id: None,
            };
            let msg = seal_message(&R2I, &header, &proto, b"foreign", DEV_NODE).unwrap();
            device.send_to(&msg, local).await.unwrap();

            // controller の standalone ack は、そのメッセージ自身の
            // exchange id で、こちらが「その exchange の」non-initiator
            // として返ってくるはず。
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, _) = device.recv_from(&mut buf).await.unwrap();
            let (_, p, _) = open_from_controller(&buf[..n]);
            assert_eq!(p.opcode, OPCODE_MRP_STANDALONE_ACK);
            assert_eq!(p.exchange_id, foreign_ex);
            assert!(!p.initiator);
            assert_eq!(p.acked_counter, Some(700));
        });

        // こちらの exchange (`ex`) にはこのメッセージは配送されない —
        // 他 exchange 宛だから。
        assert!(matches!(
            s.recv(ex, Duration::from_millis(300)).await,
            Err(SessionError::Timeout)
        ));
        dev.await.unwrap();
    }

    /// ReportData shaped like Task 8's `im.rs` test fixture: a single
    /// AttributeReportIB for onoff's `OnOff` bool attribute.
    fn report_data_payload(value: bool, suppress: bool) -> Vec<u8> {
        use crate::tlv::{Tag, Writer};
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(1)); // AttributeReportIBs
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(1)); // AttributeData
        w.put_uint(Tag::Context(0), 1); // DataVersion
        w.start_list(Tag::Context(1)); // Path
        w.put_uint(Tag::Context(2), 1);
        w.put_uint(Tag::Context(3), 6);
        w.put_uint(Tag::Context(4), 0);
        w.end_container();
        w.put_bool(Tag::Context(2), value); // Data
        w.end_container();
        w.end_container();
        w.end_container();
        if suppress {
            w.put_bool(Tag::Context(4), true);
        }
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        w.finish()
    }

    /// InvokeResponse shaped like Task 8's `im.rs` test fixture: a single
    /// successful InvokeResponseIB (status 0, no cluster status).
    fn invoke_response_success_payload() -> Vec<u8> {
        use crate::tlv::{Tag, Writer};
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bool(Tag::Context(0), false);
        w.start_array(Tag::Context(1));
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(1)); // Status = CommandStatusIB
        w.start_list(Tag::Context(0)); // Path
        w.end_container();
        w.start_struct(Tag::Context(1)); // StatusIB
        w.put_uint(Tag::Context(0), 0);
        w.end_container();
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        w.finish()
    }

    #[tokio::test]
    async fn read_attribute_roundtrip() {
        let device = bind_local().await;
        let peer = device.local_addr().unwrap();
        let transport = Arc::new(Transport::Udp(Arc::new(bind_local().await)));
        let mut s = SecureSession::new(
            Arc::clone(&transport),
            peer,
            LOCAL_SID,
            PEER_SID,
            keys(),
            OUR_NODE,
            DEV_NODE,
        );

        let dev = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, from) = device.recv_from(&mut buf).await.unwrap();
            let (h, p, _body) = open_from_controller(&buf[..n]);
            assert_eq!(p.protocol_id, crate::im::PROTOCOL_ID_IM);
            assert_eq!(p.opcode, crate::im::OPCODE_READ_REQUEST);
            // ack the request while carrying the real ReportData reply.
            let resp = device_datagram(
                p.exchange_id,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_REPORT_DATA,
                Some(h.message_counter),
                true,
                9100,
                &report_data_payload(true, true), // suppress=true: no StatusResponse expected back
            );
            device.send_to(&resp, from).await.unwrap();
        });

        let value = s
            .read_attribute(
                1,
                crate::im::CLUSTER_ON_OFF,
                crate::im::ATTR_ON_OFF,
                &fast_cfg(),
            )
            .await
            .unwrap();
        assert_eq!(value, crate::im::ImValue::Bool(true));
        dev.await.unwrap();
    }

    /// InvokeResponse (error) shaped like Task 8's `im.rs` test fixture:
    /// CommandStatusIB carrying `StatusIB{0: status, 1: cluster_status}`.
    fn invoke_response_error_payload(status: u8, cluster_status: Option<u8>) -> Vec<u8> {
        use crate::tlv::{Tag, Writer};
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bool(Tag::Context(0), false);
        w.start_array(Tag::Context(1));
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(1)); // Status = CommandStatusIB
        w.start_list(Tag::Context(0)); // Path
        w.end_container();
        w.start_struct(Tag::Context(1)); // StatusIB
        w.put_uint(Tag::Context(0), u64::from(status));
        if let Some(cs) = cluster_status {
            w.put_uint(Tag::Context(1), u64::from(cs));
        }
        w.end_container();
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        w.finish()
    }

    /// Regression for the read closing-ack: `read_attribute`'s own read has
    /// already succeeded once we've decoded the device's ReportData. The
    /// trailing StatusResponse(SUCCESS) we send back is a courtesy close of
    /// the exchange — if the device never acks it, that must NOT turn an
    /// already-successful read into a `Timeout` error.
    #[tokio::test]
    async fn read_attribute_succeeds_even_if_closing_status_response_unacked() {
        let device = bind_local().await;
        let peer = device.local_addr().unwrap();
        let transport = Arc::new(Transport::Udp(Arc::new(bind_local().await)));
        let mut s = SecureSession::new(
            Arc::clone(&transport),
            peer,
            LOCAL_SID,
            PEER_SID,
            keys(),
            OUR_NODE,
            DEV_NODE,
        );

        // Fast MRP so the unacked closing send exhausts its retry budget
        // quickly instead of stalling the test.
        let cfg = MrpConfig {
            initial_interval: Duration::from_millis(50),
            max_retries: 1,
            backoff: 1.0,
        };

        let dev = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, from) = device.recv_from(&mut buf).await.unwrap();
            let (h, p, _body) = open_from_controller(&buf[..n]);
            assert_eq!(p.opcode, crate::im::OPCODE_READ_REQUEST);
            // Ack the request while carrying the real ReportData reply.
            // suppress_response = false → the controller will try to close
            // the exchange with a StatusResponse(SUCCESS) that we
            // deliberately never ack, and never send anything else.
            let resp = device_datagram(
                p.exchange_id,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_REPORT_DATA,
                Some(h.message_counter),
                true,
                9400,
                &report_data_payload(true, false),
            );
            device.send_to(&resp, from).await.unwrap();

            // Drain (and discard) whatever the controller sends next: the
            // standalone ack for this ReportData, then the closing
            // StatusResponse retried per MrpConfig. Read them so the
            // in-flight sends complete, but never ack any of them — that's
            // the whole point of the test.
            loop {
                let mut b2 = [0u8; MAX_DATAGRAM];
                let recv =
                    tokio::time::timeout(Duration::from_millis(500), device.recv_from(&mut b2))
                        .await;
                let Ok(Ok((n2, _))) = recv else { break };
                let _ = open_from_controller(&b2[..n2]);
            }
        });

        let value = s
            .read_attribute(1, crate::im::CLUSTER_ON_OFF, crate::im::ATTR_ON_OFF, &cfg)
            .await
            .unwrap();
        assert_eq!(value, crate::im::ImValue::Bool(true));
        dev.await.unwrap();
    }

    /// Regression: a non-zero command status in the InvokeResponse must map
    /// to `SessionError::Im(ImError::CommandStatus { .. })`, carrying both
    /// the IM status and (when present) the cluster-specific status.
    #[tokio::test]
    async fn invoke_maps_nonzero_status_to_command_status_error() {
        let device = bind_local().await;
        let peer = device.local_addr().unwrap();
        let transport = Arc::new(Transport::Udp(Arc::new(bind_local().await)));
        let mut s = SecureSession::new(
            Arc::clone(&transport),
            peer,
            LOCAL_SID,
            PEER_SID,
            keys(),
            OUR_NODE,
            DEV_NODE,
        );

        let dev = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, from) = device.recv_from(&mut buf).await.unwrap();
            let (h, p, _body) = open_from_controller(&buf[..n]);
            assert_eq!(p.opcode, crate::im::OPCODE_INVOKE_REQUEST);
            let resp = device_datagram(
                p.exchange_id,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_INVOKE_RESPONSE,
                Some(h.message_counter),
                true,
                9500,
                &invoke_response_error_payload(0x81, Some(0x42)),
            );
            device.send_to(&resp, from).await.unwrap();
        });

        let err = s
            .invoke(
                1,
                crate::im::CLUSTER_ON_OFF,
                crate::im::CMD_ON_OFF_TOGGLE,
                None,
                &fast_cfg(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            SessionError::Im(crate::im::ImError::CommandStatus {
                status: 0x81,
                cluster_status: Some(0x42),
            })
        ));
        dev.await.unwrap();
    }

    #[tokio::test]
    async fn invoke_roundtrip_and_status_response_error() {
        // Scenario 1: InvokeRequest -> InvokeResponse(status 0) -> Ok.
        {
            let device = bind_local().await;
            let peer = device.local_addr().unwrap();
            let transport = Arc::new(Transport::Udp(Arc::new(bind_local().await)));
            let mut s = SecureSession::new(
                Arc::clone(&transport),
                peer,
                LOCAL_SID,
                PEER_SID,
                keys(),
                OUR_NODE,
                DEV_NODE,
            );

            let dev = tokio::spawn(async move {
                let mut buf = [0u8; MAX_DATAGRAM];
                let (n, from) = device.recv_from(&mut buf).await.unwrap();
                let (h, p, _body) = open_from_controller(&buf[..n]);
                assert_eq!(p.protocol_id, crate::im::PROTOCOL_ID_IM);
                assert_eq!(p.opcode, crate::im::OPCODE_INVOKE_REQUEST);
                let resp = device_datagram(
                    p.exchange_id,
                    crate::im::PROTOCOL_ID_IM,
                    crate::im::OPCODE_INVOKE_RESPONSE,
                    Some(h.message_counter),
                    true,
                    9200,
                    &invoke_response_success_payload(),
                );
                device.send_to(&resp, from).await.unwrap();
            });

            let out = s
                .invoke(
                    1,
                    crate::im::CLUSTER_ON_OFF,
                    crate::im::CMD_ON_OFF_TOGGLE,
                    None,
                    &fast_cfg(),
                )
                .await
                .unwrap();
            assert_eq!(out.status, 0);
            assert_eq!(out.cluster_status, None);
            dev.await.unwrap();
        }

        // Scenario 2: ReadRequest -> StatusResponse(0x7E ACCESS_DENIED) -> Err.
        {
            let device = bind_local().await;
            let peer = device.local_addr().unwrap();
            let transport = Arc::new(Transport::Udp(Arc::new(bind_local().await)));
            let mut s = SecureSession::new(
                Arc::clone(&transport),
                peer,
                LOCAL_SID,
                PEER_SID,
                keys(),
                OUR_NODE,
                DEV_NODE,
            );

            let dev = tokio::spawn(async move {
                let mut buf = [0u8; MAX_DATAGRAM];
                let (n, from) = device.recv_from(&mut buf).await.unwrap();
                let (h, p, _body) = open_from_controller(&buf[..n]);
                assert_eq!(p.protocol_id, crate::im::PROTOCOL_ID_IM);
                assert_eq!(p.opcode, crate::im::OPCODE_READ_REQUEST);
                let resp = device_datagram(
                    p.exchange_id,
                    crate::im::PROTOCOL_ID_IM,
                    crate::im::OPCODE_STATUS_RESPONSE,
                    Some(h.message_counter),
                    true,
                    9300,
                    &crate::im::encode_status_response(0x7E),
                );
                device.send_to(&resp, from).await.unwrap();
            });

            let err = s
                .read_attribute(
                    1,
                    crate::im::CLUSTER_ON_OFF,
                    crate::im::ATTR_ON_OFF,
                    &fast_cfg(),
                )
                .await
                .unwrap_err();
            assert!(matches!(
                err,
                SessionError::Im(crate::im::ImError::StatusResponse(0x7E))
            ));
            dev.await.unwrap();
        }
    }

    /// InvokeResponse carrying CommandFields (a data-returning command),
    /// shaped like Task 7's `im.rs` fixture: a single successful
    /// InvokeResponseIB whose Command is a CommandDataIB with fields.
    fn invoke_response_with_fields_payload() -> Vec<u8> {
        use crate::tlv::{Tag, Writer};
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bool(Tag::Context(0), false);
        w.start_array(Tag::Context(1));
        w.start_struct(Tag::Anonymous); // InvokeResponseIB
        w.start_struct(Tag::Context(0)); // CommandDataIB
        w.start_list(Tag::Context(0)); // CommandPathIB
        w.put_uint(Tag::Context(0), 1);
        w.put_uint(Tag::Context(1), 0x0300);
        w.put_uint(Tag::Context(2), 0x00);
        w.end_container();
        w.start_struct(Tag::Context(1)); // CommandFields
        w.put_uint(Tag::Context(0), 42);
        w.end_container();
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        w.finish()
    }

    /// `invoke_for_data` without a timed timeout must send an ordinary
    /// (non-timed) InvokeRequest — same wire shape as `invoke` — and decode
    /// a data-carrying InvokeResponse into `fields_tlv`, which `invoke`'s
    /// `InvokeOutcome` cannot represent.
    #[tokio::test]
    async fn invoke_for_data_untimed_returns_command_fields() {
        let device = bind_local().await;
        let peer = device.local_addr().unwrap();
        let transport = Arc::new(Transport::Udp(Arc::new(bind_local().await)));
        let mut s = SecureSession::new(
            Arc::clone(&transport),
            peer,
            LOCAL_SID,
            PEER_SID,
            keys(),
            OUR_NODE,
            DEV_NODE,
        );

        let dev = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, from) = device.recv_from(&mut buf).await.unwrap();
            let (h, p, body) = open_from_controller(&buf[..n]);
            assert_eq!(p.protocol_id, crate::im::PROTOCOL_ID_IM);
            assert_eq!(p.opcode, crate::im::OPCODE_INVOKE_REQUEST);
            // TimedRequest フラグ (tag 1) が false のままであること（timed 無し）。
            let mut r = crate::tlv::Reader::new(&body);
            r.next().unwrap(); // struct
            r.next().unwrap(); // SuppressResponse
            let flag = r.next().unwrap().unwrap();
            assert_eq!(
                (flag.tag, flag.value),
                (crate::tlv::Tag::Context(1), crate::tlv::Value::Bool(false))
            );
            let resp = device_datagram(
                p.exchange_id,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_INVOKE_RESPONSE,
                Some(h.message_counter),
                true,
                9600,
                &invoke_response_with_fields_payload(),
            );
            device.send_to(&resp, from).await.unwrap();
        });

        let data = s
            .invoke_for_data(
                1,
                crate::im::CLUSTER_COLOR_CONTROL,
                0x00,
                None,
                None,
                &fast_cfg(),
            )
            .await
            .unwrap();
        assert_eq!(data.status, 0);
        let fields = data.fields_tlv.expect("fields present");
        let mut fr = crate::tlv::Reader::new(&fields);
        assert_eq!(
            fr.next().unwrap().unwrap().value,
            crate::tlv::Value::StructStart
        );
        let e = fr.next().unwrap().unwrap();
        assert_eq!(
            (e.tag, e.value),
            (crate::tlv::Tag::Context(0), crate::tlv::Value::Uint(42))
        );
        dev.await.unwrap();
    }

    /// `invoke_for_data` with a timed timeout must, on the same exchange:
    /// send `TimedRequest(t)` first, wait for `StatusResponse(0)`, then send
    /// the InvokeRequest with its TimedRequest flag set (spec §8.5.1).
    #[tokio::test]
    async fn invoke_for_data_timed_sends_timed_request_then_invoke_with_flag() {
        let device = bind_local().await;
        let peer = device.local_addr().unwrap();
        let transport = Arc::new(Transport::Udp(Arc::new(bind_local().await)));
        let mut s = SecureSession::new(
            Arc::clone(&transport),
            peer,
            LOCAL_SID,
            PEER_SID,
            keys(),
            OUR_NODE,
            DEV_NODE,
        );

        let dev = tokio::spawn(async move {
            // 1. TimedRequest -> StatusResponse(0)
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, from) = device.recv_from(&mut buf).await.unwrap();
            let (h, p, _body) = open_from_controller(&buf[..n]);
            assert_eq!(p.protocol_id, crate::im::PROTOCOL_ID_IM);
            assert_eq!(p.opcode, crate::im::OPCODE_TIMED_REQUEST);
            let first_ex = p.exchange_id;
            let resp = device_datagram(
                p.exchange_id,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_STATUS_RESPONSE,
                Some(h.message_counter),
                true,
                9700,
                &crate::im::encode_status_response(0),
            );
            device.send_to(&resp, from).await.unwrap();

            // The StatusResponse we sent asked for its own ack
            // (needs_ack=true, matching real MRP traffic) — drain the
            // controller's standalone ack for it before the next real
            // message.
            let mut ack_buf = [0u8; MAX_DATAGRAM];
            let (ack_n, _) = device.recv_from(&mut ack_buf).await.unwrap();
            let (_, ack_p, _) = open_from_controller(&ack_buf[..ack_n]);
            assert_eq!(ack_p.opcode, OPCODE_MRP_STANDALONE_ACK);

            // 2. InvokeRequest (same exchange, TimedRequest flag true) -> InvokeResponse(status 0)
            let mut buf2 = [0u8; MAX_DATAGRAM];
            let (n2, from2) = device.recv_from(&mut buf2).await.unwrap();
            let (h2, p2, body2) = open_from_controller(&buf2[..n2]);
            assert_eq!(p2.opcode, crate::im::OPCODE_INVOKE_REQUEST);
            assert_eq!(p2.exchange_id, first_ex, "same exchange as TimedRequest");
            let mut r = crate::tlv::Reader::new(&body2);
            r.next().unwrap(); // struct
            r.next().unwrap(); // SuppressResponse
            let flag = r.next().unwrap().unwrap();
            assert_eq!(
                (flag.tag, flag.value),
                (crate::tlv::Tag::Context(1), crate::tlv::Value::Bool(true))
            );
            let resp2 = device_datagram(
                p2.exchange_id,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_INVOKE_RESPONSE,
                Some(h2.message_counter),
                true,
                9701,
                &invoke_response_success_payload(),
            );
            device.send_to(&resp2, from2).await.unwrap();
        });

        let data = s
            .invoke_for_data(
                1,
                crate::im::CLUSTER_ON_OFF,
                crate::im::CMD_ON_OFF_ON,
                None,
                Some(5000),
                &fast_cfg(),
            )
            .await
            .unwrap();
        assert_eq!(data.status, 0);
        assert_eq!(data.fields_tlv, None);
        dev.await.unwrap();
    }

    /// If the device rejects the TimedRequest itself (non-zero
    /// StatusResponse), `invoke_for_data` must abort right there and must
    /// never send the InvokeRequest.
    #[tokio::test]
    async fn invoke_for_data_timed_request_rejected_aborts_before_invoke() {
        let device = bind_local().await;
        let peer = device.local_addr().unwrap();
        let transport = Arc::new(Transport::Udp(Arc::new(bind_local().await)));
        let mut s = SecureSession::new(
            Arc::clone(&transport),
            peer,
            LOCAL_SID,
            PEER_SID,
            keys(),
            OUR_NODE,
            DEV_NODE,
        );

        let dev = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, from) = device.recv_from(&mut buf).await.unwrap();
            let (h, p, _body) = open_from_controller(&buf[..n]);
            assert_eq!(p.opcode, crate::im::OPCODE_TIMED_REQUEST);
            let resp = device_datagram(
                p.exchange_id,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_STATUS_RESPONSE,
                Some(h.message_counter),
                true,
                9800,
                &crate::im::encode_status_response(0x7E), // ACCESS_DENIED
            );
            device.send_to(&resp, from).await.unwrap();

            // The controller still owes us a standalone ack for that
            // needs_ack=true StatusResponse — drain it — but nothing else:
            // no InvokeRequest follows a rejected TimedRequest.
            let mut ack_buf = [0u8; MAX_DATAGRAM];
            let (ack_n, _) = device.recv_from(&mut ack_buf).await.unwrap();
            let (_, ack_p, _) = open_from_controller(&ack_buf[..ack_n]);
            assert_eq!(ack_p.opcode, OPCODE_MRP_STANDALONE_ACK);

            let mut b2 = [0u8; MAX_DATAGRAM];
            let recv =
                tokio::time::timeout(Duration::from_millis(200), device.recv_from(&mut b2)).await;
            assert!(
                recv.is_err(),
                "no further message expected after timed request rejection"
            );
        });

        let err = s
            .invoke_for_data(
                1,
                crate::im::CLUSTER_ON_OFF,
                crate::im::CMD_ON_OFF_ON,
                None,
                Some(5000),
                &fast_cfg(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            SessionError::Im(crate::im::ImError::StatusResponse(0x7E))
        ));
        dev.await.unwrap();
    }
}
