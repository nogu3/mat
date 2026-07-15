//! Message / protocol header codec (spec §4.4).

pub const PROTOCOL_ID_SECURE_CHANNEL: u16 = 0x0000;
pub const PROTOCOL_ID_INTERACTION_MODEL: u16 = 0x0001;
pub const OPCODE_MRP_STANDALONE_ACK: u8 = 0x10;
pub const OPCODE_STATUS_REPORT: u8 = 0x40;
pub const MATTER_PORT: u16 = 5540;

const FLAG_SOURCE_PRESENT: u8 = 0x04;
const EXCHANGE_FLAG_INITIATOR: u8 = 0x01;
const EXCHANGE_FLAG_ACK: u8 = 0x02;
const EXCHANGE_FLAG_RELIABILITY: u8 = 0x04;
const EXCHANGE_FLAG_VENDOR: u8 = 0x10;

/// Message header codec error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageError {
    Truncated,
    UnsupportedVersion(u8),
    ReservedDestination,
}

impl std::fmt::Display for MessageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            MessageError::Truncated => write!(f, "message truncated"),
            MessageError::UnsupportedVersion(v) => write!(f, "unsupported message version {v}"),
            MessageError::ReservedDestination => {
                write!(f, "reserved destination size in message header")
            }
        }
    }
}

impl std::error::Error for MessageError {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Destination {
    None,
    Node(u64),
    Group(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageHeader {
    pub session_id: u16,
    pub security_flags: u8,
    pub message_counter: u32,
    pub source_node_id: Option<u64>,
    pub destination: Destination,
}

impl MessageHeader {
    pub fn encode(&self, out: &mut Vec<u8>) {
        let mut flags = 0u8; // version 0
        if self.source_node_id.is_some() {
            flags |= FLAG_SOURCE_PRESENT;
        }
        flags |= match self.destination {
            Destination::None => 0,
            Destination::Node(_) => 1,
            Destination::Group(_) => 2,
        };
        out.push(flags);
        out.extend_from_slice(&self.session_id.to_le_bytes());
        out.push(self.security_flags);
        out.extend_from_slice(&self.message_counter.to_le_bytes());
        if let Some(src) = self.source_node_id {
            out.extend_from_slice(&src.to_le_bytes());
        }
        match self.destination {
            Destination::None => {}
            Destination::Node(n) => out.extend_from_slice(&n.to_le_bytes()),
            Destination::Group(g) => out.extend_from_slice(&g.to_le_bytes()),
        }
    }

    pub fn encoded(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(26);
        self.encode(&mut out);
        out
    }

    /// Decodes the header; returns it with the offset where the payload starts.
    pub fn decode(buf: &[u8]) -> Result<(MessageHeader, usize), MessageError> {
        let mut c = Cursor { buf, pos: 0 };
        let flags = c.u8()?;
        let version = flags >> 4;
        if version != 0 {
            return Err(MessageError::UnsupportedVersion(version));
        }
        let session_id = c.u16()?;
        let security_flags = c.u8()?;
        let message_counter = c.u32()?;
        let source_node_id = if flags & FLAG_SOURCE_PRESENT != 0 {
            Some(c.u64()?)
        } else {
            None
        };
        let destination = match flags & 0x03 {
            1 => Destination::Node(c.u64()?),
            2 => Destination::Group(c.u16()?),
            3 => return Err(MessageError::ReservedDestination),
            _ => Destination::None,
        };
        Ok((
            MessageHeader {
                session_id,
                security_flags,
                message_counter,
                source_node_id,
                destination,
            },
            c.pos,
        ))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProtocolHeader {
    pub initiator: bool,
    pub needs_ack: bool,
    pub acked_counter: Option<u32>,
    pub opcode: u8,
    pub exchange_id: u16,
    pub protocol_id: u16,
    pub vendor_id: Option<u16>,
}

impl ProtocolHeader {
    pub fn encode(&self, out: &mut Vec<u8>) {
        let mut flags = 0u8;
        if self.initiator {
            flags |= EXCHANGE_FLAG_INITIATOR;
        }
        if self.acked_counter.is_some() {
            flags |= EXCHANGE_FLAG_ACK;
        }
        if self.needs_ack {
            flags |= EXCHANGE_FLAG_RELIABILITY;
        }
        if self.vendor_id.is_some() {
            flags |= EXCHANGE_FLAG_VENDOR;
        }
        out.push(flags);
        out.push(self.opcode);
        out.extend_from_slice(&self.exchange_id.to_le_bytes());
        if let Some(v) = self.vendor_id {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&self.protocol_id.to_le_bytes());
        if let Some(a) = self.acked_counter {
            out.extend_from_slice(&a.to_le_bytes());
        }
    }

    pub fn decode(buf: &[u8]) -> Result<(ProtocolHeader, usize), MessageError> {
        let mut c = Cursor { buf, pos: 0 };
        let flags = c.u8()?;
        let opcode = c.u8()?;
        let exchange_id = c.u16()?;
        let vendor_id = if flags & EXCHANGE_FLAG_VENDOR != 0 {
            Some(c.u16()?)
        } else {
            None
        };
        let protocol_id = c.u16()?;
        let acked_counter = if flags & EXCHANGE_FLAG_ACK != 0 {
            Some(c.u32()?)
        } else {
            None
        };
        Ok((
            ProtocolHeader {
                initiator: flags & EXCHANGE_FLAG_INITIATOR != 0,
                needs_ack: flags & EXCHANGE_FLAG_RELIABILITY != 0,
                acked_counter,
                opcode,
                exchange_id,
                protocol_id,
                vendor_id,
            },
            c.pos,
        ))
    }
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8], MessageError> {
        let end = self.pos + n;
        if end > self.buf.len() {
            return Err(MessageError::Truncated);
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn u8(&mut self) -> Result<u8, MessageError> {
        Ok(self.take(1)?[0])
    }

    fn u16(&mut self) -> Result<u16, MessageError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32, MessageError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn u64(&mut self) -> Result<u64, MessageError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_minimal_unsecured_header() {
        let h = MessageHeader {
            session_id: 0,
            security_flags: 0,
            message_counter: 0x1234_5678,
            source_node_id: None,
            destination: Destination::None,
        };
        assert_eq!(
            h.encoded(),
            vec![0x00, 0x00, 0x00, 0x00, 0x78, 0x56, 0x34, 0x12]
        );
    }

    #[test]
    fn encodes_source_and_dest() {
        let h = MessageHeader {
            session_id: 0x0BB8,
            security_flags: 0,
            message_counter: 1,
            source_node_id: Some(0x0102_0304_0506_0708),
            destination: Destination::Node(0x1111_2222_3333_4444),
        };
        let buf = h.encoded();
        assert_eq!(buf[0], 0x05); // S | DSIZ=1
        assert_eq!(&buf[1..3], &[0xB8, 0x0B]);
        assert_eq!(&buf[8..16], &0x0102_0304_0506_0708u64.to_le_bytes());
        assert_eq!(&buf[16..24], &0x1111_2222_3333_4444u64.to_le_bytes());
        let (dec, off) = MessageHeader::decode(&buf).unwrap();
        assert_eq!(dec, h);
        assert_eq!(off, 24);
    }

    #[test]
    fn roundtrips_group_dest() {
        let h = MessageHeader {
            session_id: 0x0102,
            security_flags: 0x01, // group session type
            message_counter: 7,
            source_node_id: Some(42),
            destination: Destination::Group(0x000A),
        };
        let buf = h.encoded();
        assert_eq!(buf[0], 0x06); // S | DSIZ=2
        let (dec, off) = MessageHeader::decode(&buf).unwrap();
        assert_eq!(dec, h);
        assert_eq!(off, buf.len());
    }

    #[test]
    fn rejects_bad_message_header() {
        assert_eq!(
            MessageHeader::decode(&[0x00, 0x00]),
            Err(MessageError::Truncated)
        );
        assert_eq!(
            MessageHeader::decode(&[0x10, 0, 0, 0, 0, 0, 0, 0]),
            Err(MessageError::UnsupportedVersion(1))
        );
        // S フラグありなのに source が無い
        assert_eq!(
            MessageHeader::decode(&[0x04, 0, 0, 0, 0, 0, 0, 0]),
            Err(MessageError::Truncated)
        );
        // DSIZ 予約値 0b11（spec 4.4.1.2 reserved）は拒否
        assert_eq!(
            MessageHeader::decode(&[0x03, 0, 0, 0, 0, 0, 0, 0]),
            Err(MessageError::ReservedDestination)
        );
    }

    #[test]
    fn roundtrips_protocol_header() {
        let p = ProtocolHeader {
            initiator: true,
            needs_ack: true,
            acked_counter: None,
            opcode: 0x20,
            exchange_id: 0xABCD,
            protocol_id: PROTOCOL_ID_SECURE_CHANNEL,
            vendor_id: None,
        };
        let mut buf = Vec::new();
        p.encode(&mut buf);
        assert_eq!(buf, vec![0x05, 0x20, 0xCD, 0xAB, 0x00, 0x00]);
        let (dec, off) = ProtocolHeader::decode(&buf).unwrap();
        assert_eq!(dec, p);
        assert_eq!(off, 6);
    }

    #[test]
    fn roundtrips_protocol_header_with_ack_and_vendor() {
        let p = ProtocolHeader {
            initiator: false,
            needs_ack: false,
            acked_counter: Some(0xCAFE_F00D),
            opcode: OPCODE_MRP_STANDALONE_ACK,
            exchange_id: 1,
            protocol_id: 0xFC01,
            vendor_id: Some(0xFFF1),
        };
        let mut buf = Vec::new();
        p.encode(&mut buf);
        assert_eq!(buf[0], 0x12); // A | V
        assert_eq!(&buf[4..6], &[0xF1, 0xFF]); // vendor id は protocol id の前
        let (dec, off) = ProtocolHeader::decode(&buf).unwrap();
        assert_eq!(dec, p);
        assert_eq!(off, buf.len());
    }

    #[test]
    fn rejects_truncated_protocol_header() {
        assert_eq!(
            ProtocolHeader::decode(&[0x02, 0x10, 0x01, 0x00, 0x00]),
            Err(MessageError::Truncated)
        );
    }
}
