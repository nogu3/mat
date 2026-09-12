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
    if getrandom::fill(&mut b).is_err() {
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

/// `true` for an MRP standalone ack (SecureChannel `0x10`), which carries no
/// payload and is never a "real" reply.
pub(crate) fn is_standalone_ack(proto: &ProtocolHeader) -> bool {
    proto.protocol_id == PROTOCOL_ID_SECURE_CHANNEL && proto.opcode == OPCODE_MRP_STANDALONE_ACK
}

/// Screening outcome for one received datagram inside [`mrp_send_loop`] /
/// [`recv_until`]: keep waiting, or finish with a value.
pub(crate) enum Verdict<T> {
    Ignore,
    Done(T),
}

/// The future a screening callback hands back to [`mrp_send_loop`] /
/// [`recv_until`], boxed.
///
/// **Why boxed and not `AsyncFnMut`**: the natural spelling
/// `impl AsyncFnMut(&mut E, &[u8], SocketAddr) -> ...` does not type-check
/// once the endpoint type carries a lifetime (`ExchangeCore<'t>`) and the
/// resulting future is `tokio::spawn`ed, as `pase`'s self-handshake test and
/// `mat-device` both do: rustc universally quantifies the endpoint's own
/// lifetime while checking the spawned future's auto traits and reports
/// "implementation of `AsyncFnMut` is not general enough" (it wants the impl
/// for `&mut ExchangeCore<'1>` for *any* `'1`). A `dyn Future` erases the
/// closure's higher-ranked signature, so the check succeeds. The allocation
/// is one `Box` per received datagram — irrelevant next to a UDP round trip.
pub(crate) type BoxScreen<'a, T, Er> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<Verdict<T>, Er>> + Send + 'a>>;

/// Boxes a screening future for [`BoxScreen`] (keeps call sites free of
/// `Box::pin(..) as _` coercions).
pub(crate) fn boxed_screen<'a, T, Er>(
    fut: impl std::future::Future<Output = Result<Verdict<T>, Er>> + Send + 'a,
) -> BoxScreen<'a, T, Er> {
    Box::pin(fut)
}

/// What the generic MRP loops need from an endpoint — implemented by
/// [`ExchangeCore`] (unsecured) and `session::SecureSession` (secured). The
/// per-call screening logic stays with the caller (closure), so the loops
/// only own the retransmit schedule / deadline arithmetic.
pub(crate) trait MrpEndpoint {
    type Error: From<std::io::Error>;
    fn transport(&self) -> &Transport;
    fn peer(&self) -> SocketAddr;
    /// Time of the last valid message from the peer (spec 4.12.8 active/idle).
    fn last_rx(&self) -> Option<Instant>;
    /// The endpoint's "MRP retry budget exhausted" error.
    fn timeout_error() -> Self::Error;
}

/// MRP retransmission loop (spec §4.12): sends `datagram` once, then again
/// after each (jittered, backed-off) interval until `on_datagram` reports
/// `Done` or `max_retries` retransmissions have gone unanswered
/// (`E::timeout_error()`). `on_datagram` is called for every datagram read
/// off the socket and is expected to do the screening (decode / dedup /
/// ack) itself.
pub(crate) async fn mrp_send_loop<E: MrpEndpoint, T>(
    ep: &mut E,
    datagram: &[u8],
    cfg: &MrpConfig,
    mut on_datagram: impl for<'a> FnMut(&'a mut E, &'a [u8], SocketAddr) -> BoxScreen<'a, T, E::Error>,
) -> Result<T, E::Error> {
    let mut interval = retrans_base(ep.last_rx(), cfg);
    let mut attempts = 0u32;
    loop {
        ep.transport().send_to(datagram, ep.peer()).await?;
        let deadline = Instant::now() + jittered_interval(interval, cfg.jitter, unit_random());
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            let mut buf = [0u8; MAX_DATAGRAM];
            let Ok(recv) =
                tokio::time::timeout(remaining, ep.transport().recv_from(&mut buf)).await
            else {
                break; // interval 経過 → 再送
            };
            let (n, from) = recv?;
            if let Verdict::Done(v) = on_datagram(ep, &buf[..n], from).await? {
                return Ok(v);
            }
        }
        attempts += 1;
        if attempts > cfg.max_retries {
            return Err(E::timeout_error());
        }
        interval = interval.mul_f64(cfg.backoff);
    }
}

/// Receive loop with a fixed deadline: reads datagrams until `on_datagram`
/// reports `Done`; returns `on_timeout()` once `timeout` has elapsed.
pub(crate) async fn recv_until<E: MrpEndpoint, T>(
    ep: &mut E,
    timeout: Duration,
    on_timeout: impl Fn() -> E::Error,
    mut on_datagram: impl for<'a> FnMut(&'a mut E, &'a [u8], SocketAddr) -> BoxScreen<'a, T, E::Error>,
) -> Result<T, E::Error> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(on_timeout());
        }
        let mut buf = [0u8; MAX_DATAGRAM];
        let Ok(recv) = tokio::time::timeout(remaining, ep.transport().recv_from(&mut buf)).await
        else {
            return Err(on_timeout());
        };
        let (n, from) = recv?;
        if let Verdict::Done(v) = on_datagram(ep, &buf[..n], from).await? {
            return Ok(v);
        }
    }
}

/// Which side of the unsecured exchange we are (spec §4.6.1 / §4.4.1.2).
/// The role decides three things: the `I` flag on what we send, which
/// `I` flag we accept on what we receive, and where the initiator's
/// ephemeral node id is carried (initiator: `source`; responder:
/// `destination`).
enum Role {
    /// We opened the exchange (`UnsecuredExchange::new`).
    Initiator { source_node_id: u64 },
    /// The peer opened it (`ResponderExchange::adopt`).
    Responder {
        /// initiator が名乗った ephemeral node id（unsecured セッションの
        /// 最初のメッセージの source node id）。応答では **destination** に
        /// 載せ替える（spec §4.4.1.2 / §4.6.1.5）。`build` の doc 参照。
        /// `None` は相手が source を載せていない場合 — 両方省略する。
        peer_ephemeral_node_id: Option<u64>,
        /// 直近に受理した peer メッセージの counter。応答の ack piggyback に使う。
        last_peer_counter: u32,
    },
}

/// One unsecured (session id 0) exchange with MRP, either role. The public
/// API is the two newtypes below; this is the single implementation.
struct ExchangeCore<'t> {
    transport: &'t Transport,
    peer: SocketAddr,
    exchange_id: u16,
    counter: TxCounter,
    rx_window: RxWindow,
    last_sent_counter: Option<u32>,
    /// ピアから最後に有効なメッセージを受けた時刻（MRP active/idle 判定用）。
    last_rx: Option<Instant>,
    role: Role,
}

impl MrpEndpoint for ExchangeCore<'_> {
    type Error = ExchangeError;
    fn transport(&self) -> &Transport {
        self.transport
    }
    fn peer(&self) -> SocketAddr {
        self.peer
    }
    fn last_rx(&self) -> Option<Instant> {
        self.last_rx
    }
    fn timeout_error() -> ExchangeError {
        ExchangeError::Timeout
    }
}

impl<'t> ExchangeCore<'t> {
    fn is_initiator(&self) -> bool {
        matches!(self.role, Role::Initiator { .. })
    }

    /// ack to piggyback on our next send: the responder always acks the
    /// latest accepted peer message; the initiator piggybacks nothing.
    fn piggyback_ack(&self) -> Option<u32> {
        match self.role {
            Role::Initiator { .. } => None,
            Role::Responder {
                last_peer_counter, ..
            } => Some(last_peer_counter),
        }
    }

    /// unsecured セッションのメッセージを組む。
    ///
    /// **ヘッダのアドレス指定**: unsecured セッションでは initiator の
    /// ephemeral node id をメッセージ 1 通につきちょうど 1 箇所に載せる
    /// （spec §4.4.1.2 / §4.6.1.5）。initiator は source に、responder は
    /// destination に載せる。両方載せる／両方省くのはプロトコル違反で、
    /// 参照実装（chip の `SessionManager::UnauthenticatedMessageDispatch`）
    /// はその場でデータグラムを捨てる — 応答が「届いているのに無かったこと
    /// にされる」ので、症状は上位プロトコルのエラーではなく無言のタイムアウト
    /// になる。M2 ゲート 1 で実際にこれを踏んだ（`docs/superpowers/plans/
    /// m2-chip-tool-probe.md`）。
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
        let (source_node_id, destination, initiator) = match self.role {
            Role::Initiator { source_node_id } => (Some(source_node_id), Destination::None, true),
            Role::Responder {
                peer_ephemeral_node_id,
                ..
            } => (
                None,
                peer_ephemeral_node_id.map_or(Destination::None, Destination::Node),
                false,
            ),
        };
        let header = MessageHeader {
            session_id: 0,
            security_flags: 0,
            message_counter,
            source_node_id,
            destination,
        };
        let proto = ProtocolHeader {
            initiator,
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
    /// are re-acked here). Traffic with our own role's `I` flag (an
    /// initiator seeing `initiator: true`, a responder seeing `false`) is
    /// stray/spoofed and dropped. Standalone acks pass screening and are
    /// returned as `Some`; callers filter them by opcode.
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
        if proto.exchange_id != self.exchange_id || proto.initiator == self.is_initiator() {
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
        if let Role::Responder {
            last_peer_counter, ..
        } = &mut self.role
        {
            *last_peer_counter = header.message_counter;
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
    /// ack (or arrived on the exchange), `None` for a standalone ack. The
    /// responder role piggybacks the ack for the peer's latest message.
    async fn send_reliable(
        &mut self,
        protocol_id: u16,
        opcode: u8,
        payload: &[u8],
        cfg: &MrpConfig,
    ) -> Result<Option<IncomingMessage>, ExchangeError> {
        let ack = self.piggyback_ack();
        if self.transport.is_reliable() {
            // BTP: transport が信頼性を持つ。1 回送って実応答を待つだけ。
            let (datagram, our_counter) = self.build(protocol_id, opcode, false, ack, payload);
            self.last_sent_counter = Some(our_counter);
            self.transport.send_to(&datagram, self.peer).await?;
            let budget = total_budget(cfg);
            return self.recv(budget).await.map(Some);
        }
        let (datagram, our_counter) = self.build(protocol_id, opcode, true, ack, payload);
        self.last_sent_counter = Some(our_counter);
        mrp_send_loop(
            self,
            &datagram,
            cfg,
            move |ex: &mut ExchangeCore<'_>, buf: &[u8], from: SocketAddr| {
                boxed_screen(async move {
                    let Some(msg) = ex.screen(buf, from).await? else {
                        // ack-only の可能性: screen は standalone ack も Some で返す
                        return Ok(Verdict::Ignore);
                    };
                    if is_standalone_ack(&msg.proto) {
                        return Ok(if msg.proto.acked_counter == Some(our_counter) {
                            Verdict::Done(None)
                        } else {
                            Verdict::Ignore
                        });
                    }
                    // exchange 上の実メッセージは応答とみなす（相手が処理した証拠）
                    Ok(Verdict::Done(Some(msg)))
                })
            },
        )
        .await
    }

    /// Sends a reliability-flagged message exactly once and returns without
    /// waiting for an ack (see `UnsecuredExchange::send_once`).
    async fn send_once(
        &mut self,
        protocol_id: u16,
        opcode: u8,
        payload: &[u8],
    ) -> Result<(), ExchangeError> {
        let ack = self.piggyback_ack();
        let (datagram, our_counter) = self.build(protocol_id, opcode, true, ack, payload);
        self.last_sent_counter = Some(our_counter);
        self.transport.send_to(&datagram, self.peer).await?;
        Ok(())
    }

    /// Sends a final message and waits only for its ack (standalone or
    /// piggybacked). On a reliable transport there is no MRP, so it returns
    /// right after the send — unlike `send_reliable`, which waits for the
    /// peer's *real* reply on both transports; a "final" message expects no
    /// reply, so on BTP there is nothing to wait for (the asymmetry is
    /// intentional).
    async fn send_final(
        &mut self,
        protocol_id: u16,
        opcode: u8,
        payload: &[u8],
        cfg: &MrpConfig,
    ) -> Result<(), ExchangeError> {
        let ack = self.piggyback_ack();
        if self.transport.is_reliable() {
            // 意図的に last_sent_counter を更新しない: 更新するのは
            // send_reliable/send_once のみで、そのアクセサは
            // UnsecuredExchange にしか出ていない（final 送信を持たないため）。
            let (datagram, _) = self.build(protocol_id, opcode, false, ack, payload);
            self.transport.send_to(&datagram, self.peer).await?;
            return Ok(());
        }
        // 意図的に last_sent_counter を更新しない: 更新するのは
        // send_reliable/send_once のみで、そのアクセサは
        // UnsecuredExchange にしか出ていない（final 送信を持たないため）。
        let (datagram, our_counter) = self.build(protocol_id, opcode, true, ack, payload);
        mrp_send_loop(
            self,
            &datagram,
            cfg,
            move |ex: &mut ExchangeCore<'_>, buf: &[u8], from: SocketAddr| {
                boxed_screen(async move {
                    let Some(msg) = ex.screen(buf, from).await? else {
                        return Ok(Verdict::Ignore);
                    };
                    Ok(if msg.proto.acked_counter == Some(our_counter) {
                        Verdict::Done(())
                    } else {
                        Verdict::Ignore
                    })
                })
            },
        )
        .await
    }

    /// Waits for the next real (non-ack) message on this exchange.
    async fn recv(&mut self, timeout: Duration) -> Result<IncomingMessage, ExchangeError> {
        recv_until(
            self,
            timeout,
            || ExchangeError::Timeout,
            |ex: &mut ExchangeCore<'_>, buf: &[u8], from: SocketAddr| {
                boxed_screen(async move {
                    let Some(msg) = ex.screen(buf, from).await? else {
                        return Ok(Verdict::Ignore);
                    };
                    Ok(if is_standalone_ack(&msg.proto) {
                        Verdict::Ignore
                    } else {
                        Verdict::Done(msg)
                    })
                })
            },
        )
        .await
    }
}

/// One unsecured (session id 0) exchange, this side as initiator, with MRP.
pub struct UnsecuredExchange<'t>(ExchangeCore<'t>);

impl<'t> UnsecuredExchange<'t> {
    pub fn new(transport: &'t Transport, peer: SocketAddr) -> Self {
        let mut b = [0u8; 10];
        getrandom::fill(&mut b).expect("os rng");
        Self(ExchangeCore {
            transport,
            peer,
            exchange_id: u16::from_le_bytes([b[0], b[1]]),
            counter: TxCounter::new_random(),
            rx_window: RxWindow::new(),
            last_sent_counter: None,
            last_rx: None,
            role: Role::Initiator {
                source_node_id: u64::from_le_bytes(b[2..10].try_into().expect("8 bytes")),
            },
        })
    }

    pub fn exchange_id(&self) -> u16 {
        self.0.exchange_id
    }

    /// The message counter used by the most recent `send_reliable` /
    /// `send_once` call, if any.
    pub fn last_sent_counter(&self) -> Option<u32> {
        self.0.last_sent_counter
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
        self.0
            .send_reliable(protocol_id, opcode, payload, cfg)
            .await
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
        self.0.send_once(protocol_id, opcode, payload).await
    }

    /// Waits for the next real (non-ack) message on this exchange.
    pub async fn recv(&mut self, timeout: Duration) -> Result<IncomingMessage, ExchangeError> {
        self.0.recv(timeout).await
    }
}

/// peer が開始した unsecured exchange の応答側。PASE/CASE の全フローを 1
/// exchange で捌く（spec §4.6, §4.12）。`UnsecuredExchange` の鏡像 —
/// あちらは自分から exchange を開く初期側、こちらは peer から届いた最初の
/// メッセージ（`adopt`）から採番を引き継いで応答する側。実装は共通の
/// `ExchangeCore`（役割だけが違う）。
pub struct ResponderExchange<'t>(ExchangeCore<'t>);

impl<'t> ResponderExchange<'t> {
    /// 受信済みの最初の peer-initiated メッセージから採番を引き継いで作る。
    /// `first` の counter は即座に rx_window へコミットする — 再送されて
    /// きた同一メッセージは `screen` の重複判定に落ちて standalone-ack のみ
    /// 返す。
    pub fn adopt(transport: &'t Transport, peer: SocketAddr, first: &IncomingMessage) -> Self {
        let mut rx_window = RxWindow::new();
        rx_window.check_and_commit(first.header.message_counter);
        Self(ExchangeCore {
            transport,
            peer,
            exchange_id: first.proto.exchange_id,
            counter: TxCounter::new_random(),
            rx_window,
            last_sent_counter: None,
            last_rx: Some(Instant::now()),
            role: Role::Responder {
                peer_ephemeral_node_id: first.header.source_node_id,
                last_peer_counter: first.header.message_counter,
            },
        })
    }

    /// Waits for the next real (non-ack) peer message on this exchange.
    ///
    /// `pub` (not private): mirrors `UnsecuredExchange::recv`, which
    /// `pase::establish()` calls whenever its own `send_reliable` returns
    /// `None` (peer's eager standalone ack observed, real reply still
    /// pending) — see that function's `None => ex.recv(RECV_TIMEOUT)...`
    /// branches. `reply_reliable`/`reply_final` hit the exact same "only
    /// got a standalone ack" case (their doc comments call it out
    /// explicitly), so a `ResponderExchange`-side caller needs the same
    /// fallback. Re-`adopt`ing a fresh `ResponderExchange` per message is
    /// *not* a substitute: `adopt` reseeds `counter` with a brand new
    /// `TxCounter::new_random()`, and a fresh random value has roughly even
    /// odds of landing below the peer's already-established high-water
    /// mark — `RxWindow::check_and_commit` then silently rejects it as
    /// "too old" (while still, misleadingly, acking it), and the peer's own
    /// MRP retransmits instead of ever seeing the reply as answered
    /// (`mat-device`'s Task 6 PASE responder hit this: ~50% handshake
    /// failure rate until switched to this `recv`, keeping one
    /// `ResponderExchange` — and its single incrementing `TxCounter` —
    /// alive for the whole exchange).
    pub async fn recv(&mut self, timeout: Duration) -> Result<IncomingMessage, ExchangeError> {
        self.0.recv(timeout).await
    }

    /// initiator:false で応答し、同一 exchange の次の peer メッセージを待つ。
    /// 応答には直前に受理した peer counter を ack piggyback する。ピアの
    /// 実応答（または直後の任意の実メッセージ — 受信できたこと自体が我々の
    /// 送信が処理された証拠、という `send_reliable` と同じ簡略化）が届くまで
    /// MRP 再送する。standalone ack のみで確定した場合は `None`。
    /// `UnsecuredExchange::send_reliable` と同じ `ExchangeCore::send_reliable`
    /// を responder の役割で呼ぶだけ（peer の直近メッセージへの ack を
    /// piggyback する）。
    pub async fn reply_reliable(
        &mut self,
        protocol_id: u16,
        opcode: u8,
        payload: &[u8],
        cfg: &MrpConfig,
    ) -> Result<Option<IncomingMessage>, ExchangeError> {
        self.0
            .send_reliable(protocol_id, opcode, payload, cfg)
            .await
    }

    /// 応答して待たない（StatusReport 終端用）。needs_ack を立て、ack
    /// （standalone または piggyback、どちらも `acked_counter` が我々の
    /// counter と一致していること）を受け取るまで MRP 再送する。Reliable
    /// transport では 1 回送って即 return（`ExchangeCore::send_final` 参照）。
    pub async fn reply_final(
        &mut self,
        protocol_id: u16,
        opcode: u8,
        payload: &[u8],
        cfg: &MrpConfig,
    ) -> Result<(), ExchangeError> {
        self.0.send_final(protocol_id, opcode, payload, cfg).await
    }

    /// Test hook: screen one datagram through the shared core.
    #[cfg(test)]
    async fn screen(
        &mut self,
        buf: &[u8],
        from: SocketAddr,
    ) -> Result<Option<IncomingMessage>, ExchangeError> {
        self.0.screen(buf, from).await
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

    /// spec §4.4.1.2 / §4.6.1.5: an *unsecured* session message carries the
    /// initiator's ephemeral node id exactly once — as the **source** node
    /// id when the initiator sends, and as the **destination** node id when
    /// the responder replies. The reference implementation enforces this as
    /// a hard drop, not a lenient warning: chip's
    /// `SessionManager::UnauthenticatedMessageDispatch` rejects any
    /// unsecured datagram where source and destination are both present or
    /// both absent with `Received malformed unsecure packet`.
    ///
    /// This is the nail for a real interop failure: every `matv` PASE reply
    /// (and standalone ack) went out with neither field set, so chip-tool
    /// dropped all of them at the transport layer and `pairing
    /// onnetwork-long` timed out in PBKDFParamRequest retries without ever
    /// logging a protocol-level complaint (M2 gate 1, see
    /// `docs/superpowers/plans/m2-chip-tool-probe.md`).
    #[tokio::test]
    async fn responder_replies_address_the_initiators_ephemeral_node_id() {
        const EPHEMERAL: u64 = 0x0011_2233_4455_6677;
        let peer_sock = bind_local().await;
        let peer_addr = peer_sock.local_addr().unwrap();
        let transport = bind_local_transport().await;
        let exchange_id = 0x4242;

        let mut first = adopted_first(exchange_id, 100, true);
        first.header.source_node_id = Some(EPHEMERAL);
        let mut re = ResponderExchange::adopt(&transport, peer_addr, &first);

        let peer_task = tokio::spawn(async move {
            let (h, p, from) = read_msg(&peer_sock).await;
            assert_eq!(
                h.destination,
                Destination::Node(EPHEMERAL),
                "reply must be addressed to the initiator's ephemeral node id"
            );
            assert_eq!(
                h.source_node_id, None,
                "reply must not also carry a source node id"
            );
            let ack = initiator_datagram(
                exchange_id,
                OPCODE_MRP_STANDALONE_ACK,
                101,
                false,
                Some(h.message_counter),
                &[],
            );
            peer_sock.send_to(&ack, from).await.unwrap();
            p.opcode
        });

        re.reply_reliable(PROTOCOL_ID_SECURE_CHANNEL, 0x21, b"resp1", &fast_cfg())
            .await
            .unwrap();
        assert_eq!(peer_task.await.unwrap(), 0x21);
    }

    /// The standalone ack `screen` fires for a duplicate goes through the
    /// same `build` — it must be addressed the same way, or chip drops the
    /// ack too (and its MRP keeps retransmitting forever).
    #[tokio::test]
    async fn responder_standalone_acks_address_the_initiators_ephemeral_node_id() {
        const EPHEMERAL: u64 = 0x00AA_BBCC_DDEE_FF00;
        let peer_sock = bind_local().await;
        let peer_addr = peer_sock.local_addr().unwrap();
        let transport = bind_local_transport().await;
        let exchange_id = 0x4243;

        let mut first = adopted_first(exchange_id, 200, true);
        first.header.source_node_id = Some(EPHEMERAL);
        let mut re = ResponderExchange::adopt(&transport, peer_addr, &first);

        // A *new* peer message (not the adopted one) that demands an ack:
        // `screen` emits the standalone ack for it inline.
        let mut dg = MessageHeader {
            session_id: 0,
            security_flags: 0,
            message_counter: 201,
            source_node_id: Some(EPHEMERAL),
            destination: Destination::None,
        }
        .encoded();
        ProtocolHeader {
            initiator: true,
            needs_ack: true,
            acked_counter: None,
            opcode: 0x22,
            exchange_id,
            protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
            vendor_id: None,
        }
        .encode(&mut dg);
        assert!(re.screen(&dg, peer_addr).await.unwrap().is_some());

        let mut buf = [0u8; MAX_DATAGRAM];
        let (n, _) = tokio::time::timeout(Duration::from_secs(1), peer_sock.recv_from(&mut buf))
            .await
            .expect("no standalone ack arrived")
            .unwrap();
        let (h, off) = MessageHeader::decode(&buf[..n]).unwrap();
        let (p, _) = ProtocolHeader::decode(&buf[off..n]).unwrap();
        assert_eq!(p.opcode, OPCODE_MRP_STANDALONE_ACK);
        assert_eq!(h.destination, Destination::Node(EPHEMERAL));
        assert_eq!(h.source_node_id, None);
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

    /// `mrp_send_loop` retransmits the same datagram `max_retries` + 1 times
    /// and returns the endpoint's timeout error when nothing arrives.
    #[tokio::test]
    async fn mrp_send_loop_retransmits_then_times_out() {
        let responder = bind_local().await;
        let peer = responder.local_addr().unwrap();
        let transport = bind_local_transport().await;
        let mut ex = UnsecuredExchange::new(&transport, peer);
        let cfg = fast_cfg(); // max_retries 2 → 3 sends
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = counter.clone();
        let responder_task = tokio::spawn(async move {
            let mut buf = [0u8; MAX_DATAGRAM];
            while tokio::time::timeout(Duration::from_millis(300), responder.recv_from(&mut buf))
                .await
                .is_ok()
            {
                seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
        });
        let err = mrp_send_loop(
            &mut ex.0,
            b"payload",
            &cfg,
            |_ex: &mut ExchangeCore<'_>, _buf: &[u8], _from: SocketAddr| {
                boxed_screen(async { Ok::<Verdict<()>, ExchangeError>(Verdict::Ignore) })
            },
        )
        .await
        .unwrap_err();
        assert!(matches!(err, ExchangeError::Timeout));
        responder_task.await.unwrap();
        assert_eq!(counter.load(std::sync::atomic::Ordering::SeqCst), 3);
    }
}
