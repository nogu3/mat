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

/// チャンク読みの上限。実デバイスの wildcard read は数チャンクで収まる。
/// 上限到達は「デバイスが more_chunks を返し続けている」異常で、打ち切って
/// エラーにする（無限拘束の防止）。
const MAX_REPORT_CHUNKS: usize = 64;
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

/// `screen_with` の配送フィルタ。ack/dedup はフィルタに依らず常に行う。
#[derive(Clone, Copy)]
enum ScreenFilter {
    /// 自分が initiator の exchange 宛て（従来動作）。
    OurExchange(u16),
    /// デバイスが initiator の特定 exchange 宛て（購読 report への応答 ack 待ち用）。
    PeerExchange(u16),
    /// デバイス起点 exchange 全部（購読ポンプの report 待ち用）。
    AnyPeerInitiated,
}

/// `peer_initiated` バッファの上限。超過時は最古を捨てる。
const MAX_PEER_INITIATED_BUFFER: usize = 32;

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
    /// screen のフィルタ落ちで捨てると永久喪失する device 発 ReportData の待避
    /// バッファ（screen は認証済み needs_ack メッセージをフィルタ前に ack するため、
    /// ack 済みをドロップしてはならない）。購読 API だけが消費する。
    peer_initiated: std::collections::VecDeque<IncomingMessage>,
    /// ピアから最後に認証済みメッセージを受けた時刻（MRP active/idle 判定用、
    /// spec 4.12.8: 直近受信ありなら SAI で再送）。
    last_rx: Option<Instant>,
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
            peer_initiated: std::collections::VecDeque::new(),
            last_rx: None,
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
        tracing::debug!(
            exchange_id,
            initiator,
            acked,
            peer = %self.peer,
            "sending standalone ack"
        );
        self.transport.send_to(&datagram, self.peer).await?;
        Ok(())
    }

    /// Decrypts a datagram and screens it for the given exchange. Returns
    /// `None` for foreign or duplicate traffic the caller should skip
    /// (duplicates are re-acked here). Standalone acks pass screening and
    /// are returned as `Some`; callers filter them by opcode. Thin wrapper
    /// around `screen_with` for the common "our own exchange" case, kept so
    /// `send_reliable`/`recv` are unaffected by the filter generalization.
    async fn screen(
        &mut self,
        buf: &[u8],
        from: SocketAddr,
        exchange_id: u16,
    ) -> Result<Option<IncomingMessage>, SessionError> {
        self.screen_with(buf, from, ScreenFilter::OurExchange(exchange_id))
            .await
    }

    /// Decrypts a datagram and screens it per `filter`. Returns `None` for
    /// foreign or duplicate traffic, or traffic that fails the delivery
    /// filter (duplicates are re-acked here). Ack/dedup happen unconditionally
    /// before the filter is applied — an authenticated needs_ack message is
    /// acked as soon as it's decoded, regardless of whether it will be
    /// delivered. A device-initiated ReportData that fails the filter is
    /// therefore *buffered* (`peer_initiated`) rather than dropped: it has
    /// already been acked, so dropping it here would be a permanent loss.
    async fn screen_with(
        &mut self,
        buf: &[u8],
        from: SocketAddr,
        filter: ScreenFilter,
    ) -> Result<Option<IncomingMessage>, SessionError> {
        if from != self.peer {
            // 共有 op socket では他セッション宛の cross-traffic で正常に起きる。
            // 購読専用 socket では「デバイスが別ソースアドレスから送っている」
            // 兆候なので、切り分け時は trace で可視化する。
            tracing::trace!(%from, peer = %self.peer, "screen: datagram from foreign address; ignored");
            return Ok(None);
        }
        // 平文ヘッダだけ先に見て session id を確認する（復号前フィルタ）。
        // decode 失敗（DSIZ 予約値含む）や session id 不一致は不正/他セッション
        // のデータグラムとして無視する（DoS 耐性、エラーを伝播しない）。
        let header_peek = match MessageHeader::decode(buf) {
            Ok((h, _)) => h,
            Err(_) => {
                tracing::trace!(%from, "screen: undecodable header; ignored");
                return Ok(None);
            }
        };
        if header_peek.session_id != self.local_session_id {
            tracing::trace!(
                session_id = header_peek.session_id,
                ours = self.local_session_id,
                "screen: session id mismatch; ignored"
            );
            return Ok(None);
        }
        let (header, proto, payload) = match open_message(&self.keys.r2i, buf, self.peer_node_id) {
            Ok(v) => v,
            Err(OpenError::Message(_)) | Err(OpenError::Crypto(_)) => {
                tracing::trace!(%from, "screen: authenticated decrypt failed; ignored");
                return Ok(None);
            }
        };
        // 認証済み受信 = ピアは active。MRP 再送間隔の active/idle 判定に使う
        // （重複再送でも「ピアが生きている」証拠として記録してよい）。
        self.last_rx = Some(Instant::now());
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
        let deliver = match filter {
            ScreenFilter::OurExchange(ex) => proto.exchange_id == ex && !proto.initiator,
            ScreenFilter::PeerExchange(ex) => proto.exchange_id == ex && proto.initiator,
            ScreenFilter::AnyPeerInitiated => proto.initiator,
        };
        if !deliver {
            tracing::trace!(
                exchange_id = proto.exchange_id,
                initiator = proto.initiator,
                opcode = proto.opcode,
                "screen: delivery filter miss"
            );
            // フィルタ落ちでも device 発 ReportData は ack 済みなので待避する。
            if proto.initiator
                && proto.protocol_id == crate::im::PROTOCOL_ID_IM
                && proto.opcode == crate::im::OPCODE_REPORT_DATA
            {
                if self.peer_initiated.len() >= MAX_PEER_INITIATED_BUFFER {
                    tracing::warn!("peer-initiated report buffer full; dropping oldest");
                    self.peer_initiated.pop_front();
                }
                self.peer_initiated.push_back(IncomingMessage {
                    header,
                    proto,
                    payload,
                });
            }
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
        let mut interval = crate::exchange::retrans_base(self.last_rx, cfg);
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
            self.send_timed_request(exchange_id, timeout_ms, cfg)
                .await?;
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

    /// Sends `TimedRequest(timeout_ms)` on `exchange_id` and waits for its
    /// `StatusResponse` (spec §8.5.1), erroring on anything but SUCCESS (0).
    /// Shared by `invoke_for_data`'s and `write_attribute_tlv`'s timed
    /// pre-step — both must open the timeout window on the same exchange the
    /// following InvokeRequest/WriteRequest is sent on.
    async fn send_timed_request(
        &mut self,
        exchange_id: u16,
        timeout_ms: u16,
        cfg: &MrpConfig,
    ) -> Result<(), SessionError> {
        use crate::im::{self, ImError};
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
                Ok(())
            }
            op => Err(SessionError::UnexpectedOpcode(op)),
        }
    }

    /// Drives a ReadRequest's response through any `MoreChunkedMessages`
    /// continuation (spec §8.9.2), collecting every `ReportDataMessage`
    /// chunk. `first` is the message already received in response to the
    /// initial request (either the piggybacked reply or the standalone
    /// `recv` fallback, same pattern as every other IM exchange here).
    async fn collect_reports(
        &mut self,
        exchange_id: u16,
        first: IncomingMessage,
        cfg: &MrpConfig,
    ) -> Result<Vec<crate::im::ReportDataMessage>, SessionError> {
        use crate::im;
        let mut msgs = Vec::new();
        let mut msg = first;
        loop {
            match msg.proto.opcode {
                im::OPCODE_REPORT_DATA => {
                    let rd =
                        im::decode_report_data_message(&msg.payload).map_err(SessionError::Im)?;
                    let more = rd.more_chunks;
                    let suppress = rd.suppress_response;
                    msgs.push(rd);
                    if msgs.len() > MAX_REPORT_CHUNKS {
                        return Err(SessionError::Im(im::ImError::Malformed(
                            "too many report chunks",
                        )));
                    }
                    if more {
                        // Chunk continuation: a StatusResponse(0) prompts
                        // the device to send the next chunk.
                        let ok = im::encode_status_response(0);
                        let resp = self
                            .send_reliable(
                                exchange_id,
                                im::PROTOCOL_ID_IM,
                                im::OPCODE_STATUS_RESPONSE,
                                &ok,
                                cfg,
                            )
                            .await?;
                        msg = match resp {
                            Some(m) => m,
                            None => self.recv(exchange_id, IM_RECV_TIMEOUT).await?,
                        };
                        continue;
                    }
                    if !suppress {
                        // Best-effort close of the final chunk, same
                        // rationale as `read_attribute`'s trailing
                        // StatusResponse: the data is already in hand, so a
                        // lost ack here must not turn a successful read into
                        // an error.
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
                    return Ok(msgs);
                }
                im::OPCODE_STATUS_RESPONSE => {
                    let s = im::decode_status_response(&msg.payload).map_err(SessionError::Im)?;
                    return Err(SessionError::Im(im::ImError::StatusResponse(s)));
                }
                op => return Err(SessionError::UnexpectedOpcode(op)),
            }
        }
    }

    /// Reads a single attribute (spec §8.9.2), chunk-aware, returning its
    /// value as JSON via `im::tlv_element_to_json`'s conventions (see
    /// `im::merge_reports`). Unlike `read_attribute` (M2, scalar-only), this
    /// accepts any TLV shape (struct/array/list) and reassembles
    /// `MoreChunkedMessages` chunks. A status-only report (device rejected
    /// the read) surfaces as `ImError::AttributeStatus`.
    pub async fn read_attribute_json(
        &mut self,
        endpoint: u16,
        cluster: u32,
        attribute: u32,
        cfg: &MrpConfig,
    ) -> Result<serde_json::Value, SessionError> {
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
        let msgs = self.collect_reports(exchange_id, msg, cfg).await?;
        if let Some((_, value)) = im::merge_reports(&msgs).into_iter().next() {
            return Ok(value);
        }
        // No data reports: a single-attribute read that came back
        // status-only means the device rejected it — surface that status
        // rather than a generic "no value" if we have one in hand.
        let status = msgs
            .iter()
            .flat_map(|m| m.reports.iter())
            .find_map(|r| r.status);
        Err(match status {
            Some(s) => SessionError::Im(ImError::AttributeStatus(s)),
            None => SessionError::Im(ImError::Malformed("no value")),
        })
    }

    /// Wildcard-reads every attribute of a cluster (spec §8.9.2), chunk-aware
    /// (see `read_attribute_json`). Returns `(attribute_id, JSON value)`
    /// pairs in first-seen order, per `im::merge_reports`.
    pub async fn read_cluster_json(
        &mut self,
        endpoint: u16,
        cluster: u32,
        cfg: &MrpConfig,
    ) -> Result<Vec<(u32, serde_json::Value)>, SessionError> {
        use crate::im;
        let exchange_id = Self::new_exchange_id();
        let req = im::encode_read_request_cluster(endpoint, cluster);
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
        let msgs = self.collect_reports(exchange_id, msg, cfg).await?;
        Ok(im::merge_reports(&msgs))
    }

    /// Writes a single attribute (spec §8.9.2.4). `data_tlv` must be one
    /// complete, well-formed TLV element holding the new value (any
    /// top-level tag; `im::encode_write_request_tlv` re-tags it). When
    /// `timed_ms` is `Some(t)`, sends `TimedRequest(t)` first on the same
    /// exchange (spec §8.5.1) — same pre-step as `invoke_for_data`'s timed
    /// path, via `send_timed_request`. A non-zero `AttributeStatusIB` status
    /// in the WriteResponse is returned as `ImError::AttributeStatus`.
    pub async fn write_attribute_tlv(
        &mut self,
        endpoint: u16,
        cluster: u32,
        attribute: u32,
        data_tlv: &[u8],
        timed_ms: Option<u16>,
        cfg: &MrpConfig,
    ) -> Result<(), SessionError> {
        use crate::im::{self, ImError};
        let exchange_id = Self::new_exchange_id();

        if let Some(timeout_ms) = timed_ms {
            self.send_timed_request(exchange_id, timeout_ms, cfg)
                .await?;
        }

        let req = if timed_ms.is_some() {
            im::encode_write_request_tlv_timed(endpoint, cluster, attribute, data_tlv)
        } else {
            im::encode_write_request_tlv(endpoint, cluster, attribute, data_tlv)
        };
        let resp = self
            .send_reliable(
                exchange_id,
                im::PROTOCOL_ID_IM,
                im::OPCODE_WRITE_REQUEST,
                &req,
                cfg,
            )
            .await?;
        let msg = match resp {
            Some(m) => m,
            None => self.recv(exchange_id, IM_RECV_TIMEOUT).await?,
        };
        match msg.proto.opcode {
            im::OPCODE_WRITE_RESPONSE => {
                let status = im::decode_write_response(&msg.payload).map_err(SessionError::Im)?;
                if status != 0 {
                    return Err(SessionError::Im(ImError::AttributeStatus(status)));
                }
                Ok(())
            }
            im::OPCODE_STATUS_RESPONSE => {
                let s = im::decode_status_response(&msg.payload).map_err(SessionError::Im)?;
                Err(SessionError::Im(ImError::StatusResponse(s)))
            }
            op => Err(SessionError::UnexpectedOpcode(op)),
        }
    }

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
                exchange_id,
                false,
                im::PROTOCOL_ID_IM,
                im::OPCODE_STATUS_RESPONSE,
                false,
                None,
                &payload,
            )?;
            self.transport.send_to(&datagram, self.peer).await?;
            return Ok(());
        }
        let (datagram, our_counter) = self.seal(
            exchange_id,
            false,
            im::PROTOCOL_ID_IM,
            im::OPCODE_STATUS_RESPONSE,
            true,
            None,
            &payload,
        )?;
        let mut interval = crate::exchange::retrans_base(self.last_rx, cfg);
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
    ) -> Result<
        (
            crate::im::SubscribeResponse,
            Vec<crate::im::ReportDataMessage>,
        ),
        SessionError,
    > {
        use crate::im::{self, ImError};
        let exchange_id = Self::new_exchange_id();
        let req = im::encode_subscribe_request(
            min_interval_floor_s,
            max_interval_ceiling_s,
            keep_subscriptions,
            &[],
        );
        let resp = self
            .send_reliable(
                exchange_id,
                im::PROTOCOL_ID_IM,
                im::OPCODE_SUBSCRIBE_REQUEST,
                &req,
                cfg,
            )
            .await?;
        let mut msg = match resp {
            Some(m) => m,
            None => self.recv(exchange_id, IM_RECV_TIMEOUT).await?,
        };
        let mut priming = Vec::new();
        loop {
            match msg.proto.opcode {
                im::OPCODE_REPORT_DATA => {
                    let rd =
                        im::decode_report_data_message(&msg.payload).map_err(SessionError::Im)?;
                    tracing::debug!(
                        exchange_id,
                        reports = rd.reports.len(),
                        more_chunks = rd.more_chunks,
                        "subscribe: priming report chunk"
                    );
                    priming.push(rd);
                    if priming.len() > MAX_REPORT_CHUNKS {
                        return Err(SessionError::Im(ImError::Malformed(
                            "too many report chunks",
                        )));
                    }
                    // priming の各チャンクに StatusResponse(0)。最終チャンク後は
                    // SubscribeResponse が同 exchange で続く。
                    let ok = im::encode_status_response(0);
                    let resp = self
                        .send_reliable(
                            exchange_id,
                            im::PROTOCOL_ID_IM,
                            im::OPCODE_STATUS_RESPONSE,
                            &ok,
                            cfg,
                        )
                        .await?;
                    msg = match resp {
                        Some(m) => m,
                        None => self.recv(exchange_id, IM_RECV_TIMEOUT).await?,
                    };
                }
                im::OPCODE_SUBSCRIBE_RESPONSE => {
                    let sr =
                        im::decode_subscribe_response(&msg.payload).map_err(SessionError::Im)?;
                    tracing::debug!(
                        exchange_id,
                        subscription_id = sr.subscription_id,
                        max_interval_s = sr.max_interval_s,
                        needs_ack = msg.proto.needs_ack,
                        counter = msg.header.message_counter,
                        "subscribe: SubscribeResponse received"
                    );
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
                tracing::debug!(len = n, %from, "sub pump: datagram received");
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
        tracing::debug!(
            exchange_id = msg.proto.exchange_id,
            subscription_id = rd.subscription_id,
            reports = rd.reports.len(),
            suppress_response = rd.suppress_response,
            "sub pump: report delivered"
        );
        if !rd.suppress_response {
            self.respond_status(msg.proto.exchange_id, 0, cfg).await?;
        }
        Ok(rd)
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
    use crate::transport::{ReliableChannel, UdpTransport, MAX_DATAGRAM, RELIABLE_PEER};
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
            active_interval: Duration::from_millis(50),
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

    /// 実機バグの釘（secure 経路）: priming チャンク受信直後＝ピア active の
    /// 再送（respond_status / send_reliable 共通の base 選択）は active
    /// interval で行う。SII=5000ms のまま再送するとデバイス側 chunk
    /// タイムアウトに負けて購読が 0x80 死する（2026-07-20 実機ワイヤ確認）。
    #[tokio::test]
    async fn respond_status_retransmits_fast_after_recent_peer_rx() {
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
        s.last_rx = Some(Instant::now()); // チャンク受信直後の状況を注入
        let cfg = MrpConfig {
            initial_interval: Duration::from_secs(5),
            active_interval: Duration::from_millis(50),
            max_retries: 2,
            backoff: 1.0,
        };
        let dev = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            let _ = device.recv_from(&mut buf).await.unwrap();
            let again =
                tokio::time::timeout(Duration::from_secs(1), device.recv_from(&mut buf)).await;
            assert!(
                again.is_ok(),
                "no retransmission within 1s: active interval not applied"
            );
        });
        let t0 = std::time::Instant::now();
        let err = s.respond_status(1234, 0, &cfg).await.unwrap_err();
        assert!(matches!(err, SessionError::Timeout));
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "timeout took {:?}; idle interval used despite recent rx?",
            t0.elapsed()
        );
        dev.await.unwrap();
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
            active_interval: Duration::from_millis(50),
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

    /// WriteResponse shaped like Task 5's `im.rs` fixture
    /// (`decode_write_response_returns_first_status`): a single
    /// AttributeStatusIB with the given status.
    fn write_response_payload(status: u8) -> Vec<u8> {
        use crate::tlv::{Tag, Writer};
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(0));
        w.start_struct(Tag::Anonymous);
        w.start_list(Tag::Context(0)); // path
        w.end_container();
        w.start_struct(Tag::Context(1)); // StatusIB
        w.put_uint(Tag::Context(0), u64::from(status));
        w.end_container();
        w.end_container();
        w.end_container();
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        w.finish()
    }

    #[tokio::test]
    async fn write_attribute_reports_status_zero_as_ok() {
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
            assert_eq!(p.opcode, crate::im::OPCODE_WRITE_REQUEST);
            let resp = device_datagram(
                p.exchange_id,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_WRITE_RESPONSE,
                Some(h.message_counter),
                true,
                9500,
                &write_response_payload(0),
            );
            device.send_to(&resp, from).await.unwrap();
        });

        let mut w = crate::tlv::Writer::new();
        w.put_uint(crate::tlv::Tag::Anonymous, 128);
        let data_tlv = w.finish();

        s.write_attribute_tlv(
            1,
            crate::im::CLUSTER_ON_OFF,
            crate::im::ATTR_ON_OFF,
            &data_tlv,
            None,
            &fast_cfg(),
        )
        .await
        .unwrap();
        dev.await.unwrap();
    }

    #[tokio::test]
    async fn write_attribute_maps_nonzero_status_to_attribute_status_error() {
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
            assert_eq!(p.opcode, crate::im::OPCODE_WRITE_REQUEST);
            let resp = device_datagram(
                p.exchange_id,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_WRITE_RESPONSE,
                Some(h.message_counter),
                true,
                9501,
                &write_response_payload(0x87), // CONSTRAINT_ERROR
            );
            device.send_to(&resp, from).await.unwrap();
        });

        let mut w = crate::tlv::Writer::new();
        w.put_uint(crate::tlv::Tag::Anonymous, 999);
        let data_tlv = w.finish();

        let err = s
            .write_attribute_tlv(
                1,
                crate::im::CLUSTER_ON_OFF,
                crate::im::ATTR_ON_OFF,
                &data_tlv,
                None,
                &fast_cfg(),
            )
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            SessionError::Im(crate::im::ImError::AttributeStatus(0x87))
        ));
        dev.await.unwrap();
    }
    /// デバイスが報告チャンク上限を超えて more_chunks を返し続ける場合、
    /// `collect_reports` はエラーで打ち切る（無限拘束防止）。
    #[tokio::test]
    async fn read_cluster_json_aborts_on_endless_chunks() {
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

        const ATTR: u32 = 0x0005;

        let dev = tokio::spawn(async move {
            // ReadRequest -> stream of ReportData chunks (all with MoreChunkedMessages=true)
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, from) = device.recv_from(&mut buf).await.unwrap();
            let (h, p, _body) = open_from_controller(&buf[..n]);
            assert_eq!(p.protocol_id, crate::im::PROTOCOL_ID_IM);
            assert_eq!(p.opcode, crate::im::OPCODE_READ_REQUEST);

            // Send 70 chunks, each with more_chunks=true
            for chunk_idx in 0..70 {
                let resp = device_datagram(
                    p.exchange_id,
                    crate::im::PROTOCOL_ID_IM,
                    crate::im::OPCODE_REPORT_DATA,
                    if chunk_idx == 0 {
                        Some(h.message_counter)
                    } else {
                        None
                    },
                    true,
                    9999 + chunk_idx,
                    &report_data_message_attr(
                        1,
                        crate::im::CLUSTER_ON_OFF,
                        ATTR,
                        chunk_idx as u64,
                        true, // more_chunks = true for all chunks
                        false,
                    ),
                );
                device.send_to(&resp, from).await.unwrap();

                // Drain standalone ack for needs_ack=true
                let mut ack_buf = [0u8; MAX_DATAGRAM];
                let ack_recv = tokio::time::timeout(
                    Duration::from_millis(500),
                    device.recv_from(&mut ack_buf),
                )
                .await;
                if ack_recv.is_err() {
                    break; // controller already errored
                }

                // Wait for StatusResponse prompt (if not the last chunk)
                let mut prompt_buf = [0u8; MAX_DATAGRAM];
                let prompt_recv = tokio::time::timeout(
                    Duration::from_millis(500),
                    device.recv_from(&mut prompt_buf),
                )
                .await;
                if prompt_recv.is_err() {
                    break; // controller stopped
                }
            }
        });

        let err = s
            .read_cluster_json(1, crate::im::CLUSTER_ON_OFF, &fast_cfg())
            .await
            .unwrap_err();

        // Should fail with "too many report chunks"
        assert!(matches!(
            err,
            SessionError::Im(crate::im::ImError::Malformed("too many report chunks"))
        ));
        dev.await.unwrap();
    }

    /// Single-attribute ReportData for `read_cluster_json_merges_two_chunks`'s
    /// first chunk: one AttributeDataIB (Replace, scalar value).
    fn report_data_message_attr(
        endpoint: u16,
        cluster: u32,
        attr: u32,
        value: u64,
        more_chunks: bool,
        suppress: bool,
    ) -> Vec<u8> {
        use crate::tlv::{Tag, Writer};
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(1)); // AttributeReportIBs
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(1)); // AttributeDataIB
        w.put_uint(Tag::Context(0), 1); // DataVersion
        w.start_list(Tag::Context(1)); // Path
        w.put_uint(Tag::Context(2), u64::from(endpoint));
        w.put_uint(Tag::Context(3), u64::from(cluster));
        w.put_uint(Tag::Context(4), u64::from(attr));
        w.end_container();
        w.put_uint(Tag::Context(2), value); // Data
        w.end_container();
        w.end_container();
        w.end_container();
        if more_chunks {
            w.put_bool(Tag::Context(3), true);
        }
        if suppress {
            w.put_bool(Tag::Context(4), true);
        }
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        w.finish()
    }

    /// ReportData for `read_cluster_json_merges_two_chunks`'s second (final)
    /// chunk: 2 AttributeReportIBs for the same attribute, both list-append
    /// (ListIndex = null), matching Task 4's
    /// `merge_reports_joins_chunked_list_appends` fixture shape.
    fn report_data_message_attr_list_append_2(
        endpoint: u16,
        cluster: u32,
        attr: u32,
        v1: u64,
        v2: u64,
        suppress: bool,
    ) -> Vec<u8> {
        use crate::tlv::{Tag, Writer};
        fn path(w: &mut Writer, endpoint: u16, cluster: u32, attr: u32) {
            w.start_list(Tag::Context(1));
            w.put_uint(Tag::Context(2), u64::from(endpoint));
            w.put_uint(Tag::Context(3), u64::from(cluster));
            w.put_uint(Tag::Context(4), u64::from(attr));
            w.put_null(Tag::Context(5)); // ListIndex = null -> append
            w.end_container();
        }
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(1));
        for v in [v1, v2] {
            w.start_struct(Tag::Anonymous);
            w.start_struct(Tag::Context(1));
            path(&mut w, endpoint, cluster, attr);
            w.put_uint(Tag::Context(2), v);
            w.end_container();
            w.end_container();
        }
        w.end_container();
        if suppress {
            w.put_bool(Tag::Context(4), true);
        }
        w.put_uint(Tag::Context(255), 12);
        w.end_container();
        w.finish()
    }

    /// `read_cluster_json` must follow a `MoreChunkedMessages` continuation
    /// (StatusResponse(0) to prompt the next chunk, per spec §8.9.2) and
    /// merge the resulting reports across chunks via `im::merge_reports`.
    #[tokio::test]
    async fn read_cluster_json_merges_two_chunks() {
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

        const ATTR_A: u32 = 0x0005;
        const ATTR_B: u32 = 0x0006;

        let dev = tokio::spawn(async move {
            // 1. ReadRequest -> ReportData chunk1 (attr A, MoreChunkedMessages=true)
            let mut buf = [0u8; MAX_DATAGRAM];
            let (n, from) = device.recv_from(&mut buf).await.unwrap();
            let (h, p, _body) = open_from_controller(&buf[..n]);
            assert_eq!(p.protocol_id, crate::im::PROTOCOL_ID_IM);
            assert_eq!(p.opcode, crate::im::OPCODE_READ_REQUEST);
            let resp1 = device_datagram(
                p.exchange_id,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_REPORT_DATA,
                Some(h.message_counter),
                true,
                9900,
                &report_data_message_attr(1, crate::im::CLUSTER_ON_OFF, ATTR_A, 7, true, false),
            );
            device.send_to(&resp1, from).await.unwrap();

            // Our ReportData chunk1 asked for its own ack (needs_ack=true,
            // matching real MRP traffic) — drain the controller's
            // standalone ack for it before the next real message.
            let mut ack_buf = [0u8; MAX_DATAGRAM];
            let (ack_n, _) = device.recv_from(&mut ack_buf).await.unwrap();
            let (_, ack_p, _) = open_from_controller(&ack_buf[..ack_n]);
            assert_eq!(ack_p.opcode, OPCODE_MRP_STANDALONE_ACK);

            // 2. controller prompts the next chunk with StatusResponse(0)
            let mut buf2 = [0u8; MAX_DATAGRAM];
            let (n2, from2) = device.recv_from(&mut buf2).await.unwrap();
            let (h2, p2, _body2) = open_from_controller(&buf2[..n2]);
            assert_eq!(p2.opcode, crate::im::OPCODE_STATUS_RESPONSE);

            // 3. ReportData chunk2 (attr B, list-append x2, final chunk, suppressed)
            let resp2 = device_datagram(
                p2.exchange_id,
                crate::im::PROTOCOL_ID_IM,
                crate::im::OPCODE_REPORT_DATA,
                Some(h2.message_counter),
                true,
                9901,
                &report_data_message_attr_list_append_2(
                    1,
                    crate::im::CLUSTER_ON_OFF,
                    ATTR_B,
                    10,
                    20,
                    true,
                ),
            );
            device.send_to(&resp2, from2).await.unwrap();
        });

        let got = s
            .read_cluster_json(1, crate::im::CLUSTER_ON_OFF, &fast_cfg())
            .await
            .unwrap();
        assert_eq!(
            got,
            vec![
                (ATTR_A, serde_json::json!(7)),
                (ATTR_B, serde_json::json!([10, 20])),
            ]
        );
        dev.await.unwrap();
    }

    /// ReliableChannel ペアで SecureSession（controller 側）と生 Transport（device 側）を組む。
    fn reliable_session_pair() -> (SecureSession, Transport) {
        let (a, b) = ReliableChannel::pair();
        let s = SecureSession::new(
            Arc::new(a),
            RELIABLE_PEER,
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

        let (resp, priming) = s
            .subscribe_wildcard(0, 3600, false, &fast_cfg())
            .await
            .unwrap();
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
                &R2I,
                &header,
                &proto,
                &subscription_report_payload(42, true, false),
                DEV_NODE,
            )
            .unwrap();
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
            s.next_subscription_report(Duration::from_millis(100), &fast_cfg())
                .await,
            Err(SessionError::Timeout)
        ));
    }

    /// UDP: device 発 needs_ack ReportData は screen が ack し、購読 API で取り出せる
    /// （ack 済みメッセージの取り落とし=永久喪失が無いこと）。
    #[tokio::test]
    async fn udp_device_initiated_report_is_acked_and_delivered() {
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
                &R2I,
                &header,
                &proto,
                &subscription_report_payload(9, true, false),
                DEV_NODE,
            )
            .unwrap();
            device.send_to(&d, local).await.unwrap();
            // standalone ack と StatusResponse(needs_ack) が来る。StatusResponse は ack を返す。
            loop {
                let mut buf = [0u8; MAX_DATAGRAM];
                let Ok(Ok((n, from))) =
                    tokio::time::timeout(Duration::from_secs(2), device.recv_from(&mut buf)).await
                else {
                    break;
                };
                let (h, p, _) = open_from_controller(&buf[..n]);
                if p.opcode == crate::im::OPCODE_STATUS_RESPONSE {
                    let ack = device_datagram(
                        p.exchange_id,
                        PROTOCOL_ID_SECURE_CHANNEL,
                        OPCODE_MRP_STANDALONE_ACK,
                        Some(h.message_counter),
                        false,
                        9900,
                        &[],
                    );
                    // device は自 exchange の initiator。ack の initiator は device 視点で true。
                    // device_datagram は initiator=false 固定なので直接 seal する。
                    let header2 = MessageHeader {
                        session_id: LOCAL_SID,
                        security_flags: 0,
                        message_counter: 9900,
                        source_node_id: None,
                        destination: Destination::None,
                    };
                    let proto2 = ProtocolHeader {
                        initiator: true,
                        needs_ack: false,
                        acked_counter: Some(h.message_counter),
                        opcode: OPCODE_MRP_STANDALONE_ACK,
                        exchange_id: p.exchange_id,
                        protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
                        vendor_id: None,
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

    /// buffer-then-drain: `screen_with` が（`OurExchange`/`PeerExchange` フィルタ
    /// 中に届いた device 発 ReportData を）`peer_initiated` へ待避した状況を直接
    /// 再現し、`next_subscription_report` がソケットを読む前にそれを drain して
    /// 返すことを示す（"pop_front で先に drain" の回帰検知）。dev 側は何も送らない
    /// ため、drain が外れれば pop_front 後の分岐に落ちてソケット待ちになり、
    /// 短い timeout で `SessionError::Timeout` になって assert が落ちる。
    #[tokio::test]
    async fn next_subscription_report_drains_buffered_report_before_reading_socket() {
        let (mut s, _dev) = reliable_session_pair();

        let header = MessageHeader {
            session_id: LOCAL_SID,
            security_flags: 0,
            message_counter: 500,
            source_node_id: None,
            destination: Destination::None,
        };
        let proto = ProtocolHeader {
            initiator: true,
            needs_ack: false,
            acked_counter: None,
            opcode: crate::im::OPCODE_REPORT_DATA,
            exchange_id: 0x9999,
            protocol_id: crate::im::PROTOCOL_ID_IM,
            vendor_id: None,
        };
        // screen_with の buffer push (session.rs 内 push_back) と同じ形の
        // IncomingMessage を、フィルタ落ちで待避済みだった体で直接 peer_initiated
        // に積む（`tests` は `session` のサブモジュールなので private field に届く）。
        s.peer_initiated.push_back(IncomingMessage {
            header,
            proto,
            payload: subscription_report_payload(77, true, false),
        });

        let rd = s
            .next_subscription_report(Duration::from_millis(200), &fast_cfg())
            .await
            .unwrap();
        assert_eq!(rd.subscription_id, Some(77));
        assert_eq!(rd.reports.len(), 1);
        assert!(s.peer_initiated.is_empty());
    }
}
