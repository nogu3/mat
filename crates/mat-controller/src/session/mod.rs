//! Secure unicast session and MRP-reliable exchanges over it (spec §4.7, §4.12).
//!
//! Mirrors the M1 unsecured exchange semantics — retransmit, standalone ack,
//! RxWindow dedup — but seals every datagram with the session keys. Message
//! counters and the replay window are session-scoped (not per exchange), so
//! this type owns them and exchanges are just an `exchange_id` argument.
//!
//! Layout: this file holds the session type, its keys / error types and the
//! constructors + accessors. The behaviour lives in one submodule per role:
//! `mrp` (seal / open, dedup, acks, reliable send / recv), `client` (the
//! controller-side IM ops: read / invoke / write), `responder` (device-side
//! request intake and reliable replies, plus StatusResponse) and `subscribe`
//! (the resident Subscribe handshake and report pump).

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use tokio::time::Instant;

use crate::counter::{RxWindow, TxCounter};
use crate::crypto::CryptoError;
use crate::exchange::IncomingMessage;
use crate::message::MessageError;
use crate::transport::Transport;

/// ack に応答が piggyback しなかった場合の IM 応答待ち。op 予算設計の成分。
pub const IM_RECV_TIMEOUT: Duration = Duration::from_secs(10);

/// 単一 op 送信の最悪所要（MRP 再送総和 + IM 応答待ち ≈ 15.93s（ジッタ最悪値込み））。
/// matd の呼び出し側予算（Issue #16）はこの値を前提に設計する。
pub fn worst_case_send_budget() -> Duration {
    crate::exchange::total_budget(&crate::exchange::MrpConfig::default()) + IM_RECV_TIMEOUT
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
    /// 購読 pump の無音 deadline 切れ（受信ゼロ）。MRP 送信 ack 切れの
    /// `Timeout` とはログ上の意味が全く違うため分離（born-dead 切り分け）。
    Silence,
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
            SessionError::Silence => {
                write!(
                    f,
                    "no device-initiated message within the subscription deadline"
                )
            }
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
    /// The peer's CASE Authenticated Tags (spec §6.6.2.1.2), read off its
    /// NOC by the CASE responder. Only meaningful on a device-role CASE
    /// session (set via `with_peer_cats`); empty on every other session —
    /// PASE (no NOC), and the controller role, whose ACL decisions are made
    /// by the device, not by us.
    peer_cats: crate::cert::CaseAuthTags,
    counter: TxCounter,
    rx_window: RxWindow,
    /// screen のフィルタ落ちで捨てると永久喪失する device 発 ReportData の待避
    /// バッファ（screen は認証済み needs_ack メッセージをフィルタ前に ack するため、
    /// ack 済みをドロップしてはならない）。購読 API だけが消費する。
    peer_initiated: std::collections::VecDeque<IncomingMessage>,
    /// ピアから最後に認証済みメッセージを受けた時刻（MRP active/idle 判定用、
    /// spec 4.12.8: 直近受信ありなら SAI で再送）。
    last_rx: Option<Instant>,
    /// 購読 pump: デコード済み report を返した後に respond_status が失敗した
    /// ときの持ち越しエラー。次の next_subscription_report 呼び出しが返す
    /// （report を道連れにせず、セッション死の即時検知も保つ — 監査#1 経路A）。
    deferred_sub_err: Option<SessionError>,
    /// 最後に受けたピア発 needs_ack メッセージの (exchange_id, counter)。
    /// seal が同一 exchange への次の送信に piggyback ack として自動添付する
    /// （spec 4.12.5.2.2 / Issue #21: standalone ack の RF 喪失時、これが
    /// 無いとピアの MRP が未充足のまま残り、FP300 は購読を silent 破棄する）。
    pending_peer_ack: Option<(u16, u32)>,
    /// ピアから最後に見た acked_counter（メッセージカウンタはセッション
    /// スコープなので exchange をまたいでも意味を持つ — spec 4.12.5.2.2）。
    /// chip/Echo 系コミッショナーは standalone ack を単独で送らず、次の
    /// リクエストの ack フィールドに piggyback することがあり、それが別
    /// exchange に乗ることも珍しくない（レビュー指摘: cross-exchange
    /// secured request の ack-then-drop）。`screen_with` は配送フィルタに
    /// 関わらず認証済みメッセージなら常にこれを更新するので、
    /// `reply_reliable`/`send_reliable` の ack 待ちは自分の exchange 宛の
    /// メッセージが来なくても、この値と自分の送信 counter が一致した時点で
    /// 完了とみなせる。
    last_peer_ack: Option<u32>,
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
            peer_cats: crate::cert::CaseAuthTags::default(),
            counter: TxCounter::new_random(),
            rx_window: RxWindow::new(),
            peer_initiated: std::collections::VecDeque::new(),
            last_rx: None,
            deferred_sub_err: None,
            pending_peer_ack: None,
            last_peer_ack: None,
        }
    }

    /// デバイス役の構築: `new()` はこちらが exchange initiator（コントローラ）
    /// であることを前提にしている（`seal` は常に `keys.i2r` で封じ、`screen` は
    /// 常に `keys.r2i` で開ける）。デバイス役は鍵の使い方が逆（自分が送るのは
    /// `r2i`、相手から届くのは `i2r`）なので、`keys` の i2r/r2i を入れ替えて
    /// `new()` に渡す薄い糖衣。session id / node id は呼び出し側が「自分（デバイス）
    /// の local/peer」として渡す通常どおりの値でよい（`new()` と同じ意味）。
    pub fn new_device_role(
        transport: Arc<Transport>,
        peer: SocketAddr,
        local_session_id: u16,
        peer_session_id: u16,
        keys: SessionKeys,
        local_node_id: u64,
        peer_node_id: u64,
    ) -> Self {
        let swapped = SessionKeys {
            i2r: keys.r2i,
            r2i: keys.i2r,
            attestation_challenge: keys.attestation_challenge,
        };
        Self::new(
            transport,
            peer,
            local_session_id,
            peer_session_id,
            swapped,
            local_node_id,
            peer_node_id,
        )
    }

    pub fn peer_node_id(&self) -> u64 {
        self.peer_node_id
    }

    /// Attaches the peer's CASE Authenticated Tags (from
    /// `CaseOutput::Established::peer_cats`) to a device-role session so
    /// `peer_cats()` can complete the ACL identity `peer_node_id()` starts.
    pub fn with_peer_cats(mut self, peer_cats: crate::cert::CaseAuthTags) -> Self {
        self.peer_cats = peer_cats;
        self
    }

    /// The peer's CASE Authenticated Tags — empty unless `with_peer_cats`
    /// attached some (see the field doc for when that happens).
    pub fn peer_cats(&self) -> crate::cert::CaseAuthTags {
        self.peer_cats
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
}

mod client;
mod mrp;
mod responder;
mod subscribe;
#[cfg(test)]
mod test_util;

#[cfg(test)]
mod tests {
    use super::*;

    /// Issue #16: op 予算の設計根拠になる成分値を釘打ちする。上流の MRP/IM 既定値を
    /// 変えるとここが割れ、matd の RETRY_MIN_BUDGET / mat の --op-timeout-ms 既定の
    /// 再検討が強制される。
    #[test]
    fn budget_components_are_pinned() {
        let mrp = crate::exchange::total_budget(&crate::exchange::MrpConfig::default());
        // ジッタ最悪値込み: 4742.88 × 1.25 = 5928.6ms（5 送信の間隔総和 × (1 + jitter)）
        assert_eq!(mrp.as_millis(), 5928);
        assert_eq!(IM_RECV_TIMEOUT.as_secs(), 10);
        // 単一 op 送信の最悪 = MRP 総和（ジッタ最悪値込み） + IM 応答待ち
        // = 4742.88 × 1.25 + 10000 = 15928.6ms（切り捨て）
        assert_eq!(worst_case_send_budget().as_millis(), 15928);
        assert_eq!(crate::case::RECV_TIMEOUT.as_secs(), 10);
    }
}
