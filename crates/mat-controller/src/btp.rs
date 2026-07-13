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
}
