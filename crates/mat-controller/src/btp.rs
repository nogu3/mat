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

impl std::fmt::Display for BtpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BtpError::Handshake(msg) => write!(f, "btp handshake error: {msg}"),
            BtpError::Protocol(msg) => write!(f, "btp protocol error: {msg}"),
            BtpError::Closed => write!(f, "btp channel closed"),
            BtpError::Timeout(msg) => write!(f, "btp timeout: {msg}"),
            BtpError::Gatt(msg) => write!(f, "gatt error: {msg}"),
        }
    }
}

impl std::error::Error for BtpError {}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BtpParams {
    pub version: u8,
    pub segment_size: u16,
    pub window_size: u8,
}

pub fn handshake_request(window: u8) -> [u8; 9] {
    [
        FLAG_H | FLAG_M | FLAG_B | FLAG_E,
        MGMT_OPCODE_HANDSHAKE,
        BTP_VERSION,
        0,
        0,
        0, // 対応 version は 4 のみ（先頭 nibble スロット）
        0,
        0, // 観測 ATT_MTU 不明 = 0
        window,
    ]
}

pub fn decode_handshake_response(buf: &[u8]) -> Result<BtpParams, BtpError> {
    if buf.len() < 6 {
        return Err(BtpError::Handshake("short handshake response"));
    }
    if buf[0] != (FLAG_H | FLAG_M | FLAG_B | FLAG_E) || buf[1] != MGMT_OPCODE_HANDSHAKE {
        return Err(BtpError::Handshake("not a handshake response"));
    }
    // SDK cross-check (BleLayer.cpp BleTransportCapabilitiesResponseMessage::Decode):
    // mSelectedProtocolVersion is a plain Read8, not nibble-masked (nibble packing
    // only applies to the request's multi-version array). Full-byte read here.
    let version = buf[2];
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
    Ok(BtpParams {
        version,
        segment_size,
        window_size,
    })
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
            let b = buf
                .get(i..i + 2)
                .ok_or(BtpError::Protocol("truncated len"))?;
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

// --- Session actor (Task 3) -------------------------------------------

use std::time::Duration;
use tokio::sync::mpsc;

use crate::transport::{ReliableChannel, Transport};

pub const PROPOSED_WINDOW: u8 = 6;
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
pub const ACK_TIMEOUT: Duration = Duration::from_secs(15);
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_millis(2500);

/// GATT リンクの土管。ble.rs（feature "ble"、未実装）が実体を、テストが
/// fake を作る。
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
    Ok((
        params,
        Transport::Reliable(ReliableChannel::new(app_tx, app_rx)),
    ))
}

struct SessionState {
    tx_seq: u8,        // 次に使う自分の sequence
    peer_acked: u8,    // peer が ack 済みの自分の最新 seq（初期値 0 = 未 ack 相当）
    unacked: u8,       // 未 ack の自分のフレーム数
    last_rx_seq: u8,   // 受信した最新の peer seq
    pending_ack: bool, // peer へ ack を返す義務があるか
    reasm: Reassembler,
    // レビュー指摘対応（fix wave 1）:
    last_ack_progress: tokio::time::Instant, // 直近で unacked が減った時刻
    segs_since_ack: u8,                      // 直近の送信ack以降に受信した「実データ」segment数
}

impl SessionState {
    fn new() -> Self {
        Self {
            tx_seq: 0,
            peer_acked: 0,
            unacked: 0,
            last_rx_seq: 0,
            pending_ack: false,
            reasm: Reassembler::new(),
            last_ack_progress: tokio::time::Instant::now(),
            segs_since_ack: 0,
        }
    }
}

/// 受信フレームを処理してウィンドウ会計・pending_ack・再構成を進める。
/// メッセージが完成すれば `Ok(Some(msg))`。run_session のメインループと
/// send_message のウィンドウ待ちループの双方から使う共通ロジック。
fn process_incoming(pkt: &Packet, st: &mut SessionState) -> Result<Option<Vec<u8>>, BtpError> {
    if let Some(a) = pkt.ack {
        // a..=直前 tx_seq-1 のうち ack された分を勘定（u8 wrap 対応）。
        let newly = a.wrapping_sub(st.peer_acked);
        if newly > 0 {
            st.last_ack_progress = tokio::time::Instant::now();
        }
        st.unacked = st.unacked.saturating_sub(newly);
        st.peer_acked = a;
    }
    if let Some(s) = pkt.seq {
        st.last_rx_seq = s;
        st.pending_ack = true;
        // payload が空 = standalone ack/keepalive（Reassembler::push と同じ
        // 判定基準）。実データを運ぶ segment だけを積算対象にする — でない
        // と「ack への ack」が無限に連鎖してしまう（純粋な ack 交換は
        // 完了/keepalive 任せのままでよい。brief の対象は複数segmentに
        // またがる実メッセージの詰まり）。
        if !pkt.payload.is_empty() {
            st.segs_since_ack = st.segs_since_ack.saturating_add(1);
        }
    }
    st.reasm.push(pkt)
}

/// 受信segment数が閾値（ウィンドウの半分、最低1）に達していたら、メッセージ
/// 完成を待たず即座に ack を返す。相手側のウィンドウがこちらの ack 待ちで
/// 詰まるのを防ぐ（brief: 複数segmentに跨るメッセージのstall対策）。
/// keepalive と同じ理由でこちらのウィンドウが満杯なら送らない。
async fn maybe_send_proactive_ack(
    link: &mut GattLink,
    st: &mut SessionState,
    params: &BtpParams,
) -> Result<(), BtpError> {
    let threshold = std::cmp::max(1, params.window_size / 2);
    if st.pending_ack && st.segs_since_ack >= threshold && st.unacked < params.window_size {
        send_standalone(link, st).await?;
    }
    Ok(())
}

async fn run_session(
    mut link: GattLink,
    params: BtpParams,
    mut out_rx: mpsc::Receiver<Vec<u8>>,
    in_tx: mpsc::Sender<Vec<u8>>,
) {
    let mut st = SessionState::new();
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
                match process_incoming(&pkt, &mut st) {
                    Ok(Some(msg)) => {
                        if in_tx.send(msg).await.is_err() { break; }
                        // 即時 ack（実装簡素化: 受信メッセージ完成ごと）
                        if send_standalone(&mut link, &mut st).await.is_err() { break; }
                        keepalive.reset();
                    }
                    Ok(None) => {
                        // 複数segmentメッセージの途中: 閾値超えなら早めにack
                        // (Finding 2)。
                        if maybe_send_proactive_ack(&mut link, &mut st, &params).await.is_err() {
                            break;
                        }
                    }
                    Err(e) => { tracing::warn!(error=%e, "btp reassembly failed"); break; }
                }
            }
            msg = out_rx.recv() => {
                let Some(msg) = msg else { break }; // アプリ側 drop → 終了
                if send_message(&mut link, &mut st, &params, &msg, &in_tx).await.is_err() { break; }
                keepalive.reset();
            }
            _ = keepalive.tick() => {
                // アイドル中に unacked が全く進捗しないまま ACK_TIMEOUT を
                // 超えたら見込みなしと判断してセッションを閉じる
                // (Finding 1: send_message のウィンドウ待ちループ以外では
                // ACK_TIMEOUT が効いていなかった)。
                if st.unacked > 0 && st.last_ack_progress.elapsed() >= ACK_TIMEOUT {
                    tracing::warn!("btp: no ack progress within ACK_TIMEOUT — closing session");
                    break;
                }
                // ウィンドウが満杯なら keepalive も送らない（unacked の
                // 無制限増加 = u8 オーバーフローを防ぐ。Finding 1）。
                if st.unacked < params.window_size
                    && send_standalone(&mut link, &mut st).await.is_err()
                {
                    break;
                }
            }
        }
    }
    // actor 終了 = チャネル閉鎖 → Transport 側は BrokenPipe を観測する
}

async fn send_standalone(link: &mut GattLink, st: &mut SessionState) -> Result<(), BtpError> {
    let seq = st.tx_seq;
    st.tx_seq = st.tx_seq.wrapping_add(1);
    st.unacked = st.unacked.saturating_add(1); // defense in depth (Finding 1)
    st.pending_ack = false;
    st.segs_since_ack = 0; // ack を出したので起点をリセット（Finding 2）
    link.writes
        .send(encode_standalone_ack(seq, st.last_rx_seq).to_vec())
        .await
        .map_err(|_| BtpError::Closed)
}

/// メッセージを 1 通送信する。ウィンドウが満杯なら peer からの ack を待つ
/// （その間に届いたデータフレームは `in_tx` 経由でアプリへ配送し、捨てない）。
async fn send_message(
    link: &mut GattLink,
    st: &mut SessionState,
    params: &BtpParams,
    msg: &[u8],
    in_tx: &mpsc::Sender<Vec<u8>>,
) -> Result<(), BtpError> {
    let mut off = 0usize;
    let mut first = true;
    while first || off < msg.len() {
        // ウィンドウ満杯なら ack を待つ。待機中に受けたデータメッセージは
        // 落とさず in_tx へ配送する。
        while st.unacked >= params.window_size {
            let frame = tokio::time::timeout(ACK_TIMEOUT, link.indications.recv())
                .await
                .map_err(|_| BtpError::Timeout("window ack"))?
                .ok_or(BtpError::Closed)?;
            let pkt = Packet::decode(&frame)?;
            match process_incoming(&pkt, st)? {
                Some(m) => {
                    if in_tx.send(m).await.is_err() {
                        return Err(BtpError::Closed);
                    }
                    send_standalone(link, st).await?;
                }
                None => {
                    // ここで unacked が減っていれば直後にゲートが開くので、
                    // 相手の次メッセージを待たせず即座にackを返す
                    // (Finding 2)。
                    maybe_send_proactive_ack(link, st, params).await?;
                }
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
        st.unacked = st.unacked.saturating_add(1); // defense in depth (Finding 1)
        let ack = if with_ack {
            st.pending_ack = false;
            st.segs_since_ack = 0; // ack を出したので起点をリセット（Finding 2）
            Some(st.last_rx_seq)
        } else {
            None
        };
        let frame = encode_data_packet(
            seq,
            ack,
            pos,
            if first { Some(msg.len() as u16) } else { None },
            &msg[off..end],
        );
        link.writes
            .send(frame)
            .await
            .map_err(|_| BtpError::Closed)?;
        off = end;
        first = false;
    }
    Ok(())
}

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
        // flags,opcode,version(full byte, not nibble-masked — see SDK cross-check),segment_size=244 LE,window=4
        let p = decode_handshake_response(&[0x65, 0x6C, 0x04, 0xF4, 0x00, 0x04]).unwrap();
        assert_eq!(p.version, 4);
        assert_eq!(p.segment_size, 244);
        assert_eq!(p.window_size, 4);
        // version 不一致は拒否
        assert!(decode_handshake_response(&[0x65, 0x6C, 0x03, 0xF4, 0x00, 0x04]).is_err());
        // version はフルバイト読み（nibbleマスクではない）: 上位nibbleが立っていれば
        // 4と一致しない別値として拒否される（SDK cross-check: Read8、下位nibbleマスク不使用）
        assert!(decode_handshake_response(&[0x65, 0x6C, 0x14, 0xF4, 0x00, 0x04]).is_err());
        // 短すぎ
        assert!(decode_handshake_response(&[0x65, 0x6C, 0x04]).is_err());
    }

    #[test]
    fn single_segment_packet_roundtrip() {
        // B|E、seq=0、msg_len=5、ackなし
        let bytes = encode_data_packet(
            0,
            None,
            SegmentPos::First { ending: true },
            Some(5),
            b"hello",
        );
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
        let bytes = encode_data_packet(
            7,
            Some(3),
            SegmentPos::First { ending: true },
            Some(2),
            b"ab",
        );
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
            0,
            None,
            SegmentPos::First { ending: false },
            Some(100),
            &msg[..40],
        ))
        .unwrap();
        let p2 = Packet::decode(&encode_data_packet(
            1,
            None,
            SegmentPos::Middle,
            None,
            &msg[40..80],
        ))
        .unwrap();
        let p3 = Packet::decode(&encode_data_packet(
            2,
            None,
            SegmentPos::Last,
            None,
            &msg[80..],
        ))
        .unwrap();
        assert!(r.push(&p1).unwrap().is_none());
        assert!(r.push(&p2).unwrap().is_none());
        assert_eq!(r.push(&p3).unwrap().unwrap(), msg);
    }

    #[test]
    fn reassembler_rejects_length_mismatch() {
        let mut r = Reassembler::new();
        let p = Packet::decode(&encode_data_packet(
            0,
            None,
            SegmentPos::First { ending: true },
            Some(10),
            b"short",
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

    // --- Reassembler edge branches (Task 2 レビュー指摘の未テスト分岐) ---

    #[test]
    fn reassembler_rejects_beginning_segment_mid_message() {
        let mut r = Reassembler::new();
        let p1 = Packet::decode(&encode_data_packet(
            0,
            None,
            SegmentPos::First { ending: false },
            Some(10),
            b"12345",
        ))
        .unwrap();
        assert!(r.push(&p1).unwrap().is_none());
        // まだ 5 バイト待ちなのに、また beginning が来た
        let p2 = Packet::decode(&encode_data_packet(
            1,
            None,
            SegmentPos::First { ending: true },
            Some(3),
            b"abc",
        ))
        .unwrap();
        assert!(r.push(&p2).is_err());
    }

    #[test]
    fn reassembler_rejects_continuation_without_beginning_when_nonempty() {
        let mut r = Reassembler::new();
        // beginning なしで payload 入りの continuation が来る
        let p = Packet::decode(&encode_data_packet(
            0,
            None,
            SegmentPos::Middle,
            None,
            b"xyz",
        ))
        .unwrap();
        assert!(r.push(&p).is_err());
    }

    #[test]
    fn reassembler_rejects_payload_exceeding_declared_len_mid_stream() {
        let mut r = Reassembler::new();
        let p1 = Packet::decode(&encode_data_packet(
            0,
            None,
            SegmentPos::First { ending: false },
            Some(5),
            b"12345", // 早くも宣言長ぴったり届いてしまう
        ))
        .unwrap();
        // まだ ending ではないのに、buf.len() == expected に達している状態から
        // さらに追加payloadが来ると expected を超える（末尾での短さとは別分岐）。
        assert!(r.push(&p1).unwrap().is_none());
        let p2 =
            Packet::decode(&encode_data_packet(1, None, SegmentPos::Last, None, b"6")).unwrap();
        assert!(r.push(&p2).is_err());
    }

    #[test]
    fn reassembler_standalone_ack_through_push_is_ok_none() {
        let mut r = Reassembler::new();
        // 空 payload、B/E なし = スタンドアロン ack / keepalive
        let p = Packet::decode(&encode_standalone_ack(3, 9)).unwrap();
        assert!(p.payload.is_empty());
        assert!(!p.beginning && !p.ending);
        assert_eq!(r.push(&p).unwrap(), None);
    }

    // --- ack accounting (u8 wrap) ---

    #[test]
    fn ack_accounting_handles_seq_wrap_past_255() {
        let mut st = SessionState::new();
        st.tx_seq = 2;
        st.peer_acked = 254;
        st.unacked = 4;
        // peer acks value 1: wraps past 255 → 0 → 1, covering 3 new frames
        // (255, 0, 1) relative to the previous peer_acked=254.
        let pkt = Packet {
            beginning: false,
            ending: false,
            ack: Some(1),
            seq: Some(0),
            msg_len: None,
            payload: vec![],
        };
        let msg = process_incoming(&pkt, &mut st).unwrap();
        assert!(msg.is_none());
        assert_eq!(st.peer_acked, 1);
        assert_eq!(st.unacked, 1); // 4 - 3
    }

    // --- Session actor: fake peripheral + connect() tests (Task 3) ---

    /// テスト用 BTP peripheral。GattLink の裏側を演じる。
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
            GattLink {
                writes: wtx,
                indications: irx,
            },
            FakePeripheral {
                from_client: wrx,
                to_client: itx,
                tx_seq: 0,
                reasm: Reassembler::new(),
            },
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
            self.to_client
                .send(encode_standalone_ack(seq, ack).to_vec())
                .await
                .unwrap();
        }

        /// メッセージを segment_size で分割して indication する（ack 相乗り付き）。
        async fn send_message(&mut self, msg: &[u8], segment_size: u16, ack: Option<u8>) {
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
        t.send_to(b"ping-message", crate::transport::RELIABLE_PEER)
            .await
            .unwrap();
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
        t.send_to(&msg, crate::transport::RELIABLE_PEER)
            .await
            .unwrap();
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
        t.send_to(&msg, crate::transport::RELIABLE_PEER)
            .await
            .unwrap();
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

    #[tokio::test]
    async fn btp_delivers_message_received_while_window_full() {
        // window=2: 3枚目は ack 待ちでブロックされる。その最中に peripheral が
        // 単発 ack ではなく「データメッセージ」（ack 相乗り）を送ってきても、
        // client 側はそれを取りこぼさず recv_from で配送する。
        //
        // client はメッセージ受信への応答として自分の standalone ack を返す
        // が、それ自体も 1 フレームとしてウィンドウを消費する（brief 通り:
        // standalone ack も sequenced でウィンドウ会計に数える）。したがって
        // 3 枚目が出てくるには、peripheral がその standalone ack にもう一度
        // ack を返してやる必要がある — 相乗り ack 1 回だけでは足りない。
        let (link, mut p) = fake_link();
        let msg: Vec<u8> = (0u8..60).collect(); // segment 30 → 3 フレーム
        let peripheral = tokio::spawn(async move {
            p.do_handshake(30, 2).await;
            let f1 = p.from_client.recv().await.unwrap();
            let f2 = p.from_client.recv().await.unwrap();
            let s2 = Packet::decode(&f2).unwrap().seq.unwrap();
            // 単発 ack の代わりに、ack 相乗りの完全なメッセージを送る。
            p.send_message(b"unsolicited", 244, Some(s2)).await;
            // client が返す standalone ack（これもウィンドウを消費する）。
            let client_ack = p.from_client.recv().await.unwrap();
            let client_ack_pkt = Packet::decode(&client_ack).unwrap();
            assert!(client_ack_pkt.payload.is_empty(), "expected standalone ack");
            // それに ack を返してやって初めてウィンドウが解放される。
            p.send_ack(client_ack_pkt.seq.unwrap()).await;
            // ここでようやく 3 枚目（元メッセージの最終セグメント）が届く。
            let f3 = p.from_client.recv().await.unwrap();
            let mut completed = None;
            for f in [f1, f2, f3] {
                let pkt = Packet::decode(&f).unwrap();
                if let Some(m) = p.reasm.push(&pkt).unwrap() {
                    completed = Some(m);
                }
            }
            assert_eq!(completed.expect("message should reassemble").len(), 60);
        });
        let (_, t) = connect(link, PROPOSED_WINDOW).await.unwrap();
        let send_fut = t.send_to(&msg, crate::transport::RELIABLE_PEER);
        let mut buf = [0u8; 1280];
        let recv_fut = t.recv_from(&mut buf);
        let (send_res, recv_res) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(send_fut, recv_fut)
        })
        .await
        .expect("send/recv must not hang");
        send_res.unwrap();
        let (n, _) = recv_res.unwrap();
        assert_eq!(&buf[..n], b"unsolicited");
        tokio::time::timeout(std::time::Duration::from_secs(2), peripheral)
            .await
            .expect("peripheral task must not hang")
            .unwrap();
    }

    // --- Fix wave 1: keepalive/ack-timeout bounding, proactive inbound ack ---

    #[tokio::test(start_paused = true)]
    async fn btp_keepalive_gated_by_window_then_closes_after_ack_timeout() {
        // window=2, peer never acks anything: keepalives must stop once
        // unacked reaches the window (no unbounded growth / u8 overflow),
        // and the actor must close on its own once ACK_TIMEOUT elapses with
        // zero ack progress (idle-but-unacked watchdog).
        let (link, mut p) = fake_link();
        let peripheral = tokio::spawn(async move {
            p.do_handshake(244, 2).await;
            for _ in 0..2 {
                let frame = p.from_client.recv().await.expect("keepalive within window");
                let pkt = Packet::decode(&frame).unwrap();
                assert!(pkt.payload.is_empty());
                assert!(pkt.ack.is_some());
            }
            // Window is now full (unacked == window_size == 2): further
            // keepalive ticks must be skipped, not queued, well before the
            // eventual ACK_TIMEOUT close.
            let blocked = tokio::time::timeout(KEEPALIVE_INTERVAL * 3, p.from_client.recv()).await;
            assert!(blocked.is_err(), "no third keepalive: window-gated");
        });
        let (_, t) = connect(link, PROPOSED_WINDOW).await.unwrap();
        let mut buf = [0u8; 16];
        let err = tokio::time::timeout(ACK_TIMEOUT + KEEPALIVE_INTERVAL * 2, t.recv_from(&mut buf))
            .await
            .expect("actor must close within ACK_TIMEOUT of no ack progress")
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
        peripheral.await.unwrap();
    }

    #[tokio::test]
    async fn btp_sends_proactive_ack_before_message_completes() {
        // window=2: proactive-ack threshold = max(1, 2/2) = 1 inbound
        // segment. A peer that itself only tolerates 2 unacked frames sends
        // segment 1+2 up front, then needs an ack from us before segment 3.
        // Without the fix this only unblocks via the 2.5s keepalive; with
        // the fix we ack after just 1 segment, so the whole exchange
        // completes with no time advance at all (bounded by a short
        // real-time timeout below).
        let (link, mut p) = fake_link();
        let msg: Vec<u8> = (0u8..60).collect(); // segment_size 30 → 3 segments (26/28/6)
        let expect = msg.clone();
        let peripheral = tokio::spawn(async move {
            p.do_handshake(30, 2).await;
            let seg1 = encode_data_packet(
                0,
                None,
                SegmentPos::First { ending: false },
                Some(60),
                &msg[..26],
            );
            let seg2 = encode_data_packet(1, None, SegmentPos::Middle, None, &msg[26..54]);
            p.to_client.send(seg1).await.unwrap();
            p.to_client.send(seg2).await.unwrap();
            // Must see a proactive ack after segment 1 (threshold=1) before
            // sending the final segment — no keepalive wait needed.
            let ack_frame = p.from_client.recv().await.expect("proactive ack");
            let ack_pkt = Packet::decode(&ack_frame).unwrap();
            assert!(ack_pkt.payload.is_empty(), "expected standalone ack");
            assert!(ack_pkt.ack.is_some());
            let seg3 = encode_data_packet(2, None, SegmentPos::Last, None, &msg[54..]);
            p.to_client.send(seg3).await.unwrap();
            p
        });
        let (_, t) = connect(link, PROPOSED_WINDOW).await.unwrap();
        let mut buf = [0u8; 1280];
        let (n, _) = tokio::time::timeout(std::time::Duration::from_secs(1), t.recv_from(&mut buf))
            .await
            .expect("message must complete without waiting on keepalive")
            .unwrap();
        assert_eq!(&buf[..n], &expect[..]);
        tokio::time::timeout(std::time::Duration::from_secs(1), peripheral)
            .await
            .expect("peripheral must not hang")
            .unwrap();
    }
}
