//! SecureChannel protocol (spec §4.11, §4.13, §4.14): the wire constants and
//! the StatusReport codec every SecureChannel state machine shares — the
//! PASE initiator (`pase`), the CASE initiator (`case`), the CASE responder
//! (`case_responder`), `SecureSession::send_close_session` and the test
//! responders. One definition here; the state machines only re-export.
//!
//! PASE's own opcodes (`OPCODE_PBKDF_PARAM_REQUEST` …) stay in `pase`
//! because `tests/btp_pase_plumbing.rs` and mat-device import them from
//! there; the CASE opcodes live here because both `case` and
//! `case_responder` need them.

/// CASE opcodes (spec §4.14.1).
pub const OPCODE_SIGMA1: u8 = 0x30;
pub const OPCODE_SIGMA2: u8 = 0x31;
pub const OPCODE_SIGMA3: u8 = 0x32;
// StatusReport is `message::OPCODE_STATUS_REPORT` (0x40).

/// StatusReport `GeneralCode` (spec §4.11.3, Table 22).
pub const GENERAL_CODE_SUCCESS: u16 = 0;
pub const GENERAL_CODE_FAILURE: u16 = 1;

/// SecureChannel-specific `ProtocolCode`s (spec §4.11.3.1). Note that the
/// value 2 means `CloseSession` under `GENERAL_CODE_SUCCESS` and
/// `InvalidParameter` under `GENERAL_CODE_FAILURE` — the general code
/// disambiguates, so both constants exist.
pub const SC_PROTOCOL_CODE_NO_SHARED_TRUST_ROOTS: u16 = 1;
pub const SC_PROTOCOL_CODE_INVALID_PARAMETER: u16 = 2;
pub const SC_PROTOCOL_CODE_CLOSE_SESSION: u16 = 2;
pub const SC_PROTOCOL_CODE_BUSY: u16 = 4;

/// The `(general_code, protocol_id, protocol_code)` triple of a successful
/// StatusReport (session established).
pub const STATUS_REPORT_SUCCESS: (u16, u32, u16) = (0, 0, 0);

/// `parse_status_report`'s only failure: fewer than 8 payload bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StatusReportTruncated;

impl std::fmt::Display for StatusReportTruncated {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "status report truncated")
    }
}

impl std::error::Error for StatusReportTruncated {}

/// Parses a StatusReport payload: 8 bytes LE `{general_code: u16,
/// protocol_id: u32, protocol_code: u16}` (spec §4.11.3). Trailing
/// protocol-specific data is ignored.
pub fn parse_status_report(payload: &[u8]) -> Result<(u16, u32, u16), StatusReportTruncated> {
    if payload.len() < 8 {
        return Err(StatusReportTruncated);
    }
    let general_code = u16::from_le_bytes(payload[0..2].try_into().expect("2 bytes"));
    let protocol_id = u32::from_le_bytes(payload[2..6].try_into().expect("4 bytes"));
    let protocol_code = u16::from_le_bytes(payload[6..8].try_into().expect("2 bytes"));
    Ok((general_code, protocol_id, protocol_code))
}

/// Encodes a StatusReport payload (the inverse of [`parse_status_report`]):
/// 8 bytes LE `{general_code, protocol_id, protocol_code}`. Used to abort a
/// handshake explicitly (PASE confirm mismatch, bad PBKDF params) — dropping
/// the exchange silently leaves the responder holding its establishment
/// slot until its Pake3/Sigma3 timeout (spec §4.11.3 / §4.13.1.4) — and by
/// responders for their success/failure replies.
pub fn encode_status_report(general_code: u16, protocol_id: u32, protocol_code: u16) -> Vec<u8> {
    let mut buf = Vec::with_capacity(8);
    buf.extend_from_slice(&general_code.to_le_bytes());
    buf.extend_from_slice(&protocol_id.to_le_bytes());
    buf.extend_from_slice(&protocol_code.to_le_bytes());
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_report() {
        let ok = [0u8, 0, 0, 0, 0, 0, 0, 0];
        assert_eq!(parse_status_report(&ok).unwrap(), (0, 0, 0));
        let busy = [1u8, 0, 0, 0, 0, 0, 4, 0]; // FAILURE / SC / BUSY
        assert_eq!(parse_status_report(&busy).unwrap(), (1, 0, 4));
        assert_eq!(parse_status_report(&[0u8; 4]), Err(StatusReportTruncated));
    }

    #[test]
    fn encode_status_report_round_trips_through_parse() {
        let buf = encode_status_report(GENERAL_CODE_FAILURE, 0, SC_PROTOCOL_CODE_BUSY);
        assert_eq!(buf, [1u8, 0, 0, 0, 0, 0, 4, 0]);
        assert_eq!(parse_status_report(&buf).unwrap(), (1, 0, 4));
    }

    #[test]
    fn constants_match_spec_tables() {
        assert_eq!(
            (OPCODE_SIGMA1, OPCODE_SIGMA2, OPCODE_SIGMA3),
            (0x30, 0x31, 0x32)
        );
        assert_eq!(GENERAL_CODE_SUCCESS, 0);
        assert_eq!(GENERAL_CODE_FAILURE, 1);
        assert_eq!(SC_PROTOCOL_CODE_NO_SHARED_TRUST_ROOTS, 1);
        assert_eq!(SC_PROTOCOL_CODE_INVALID_PARAMETER, 2);
        assert_eq!(SC_PROTOCOL_CODE_CLOSE_SESSION, 2);
        assert_eq!(SC_PROTOCOL_CODE_BUSY, 4);
        assert_eq!(STATUS_REPORT_SUCCESS, (0, 0, 0));
    }
}
