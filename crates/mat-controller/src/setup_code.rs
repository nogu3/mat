//! Matter setup code（manual pairing code / QR onboarding payload）の
//! parse・生成（spec §5.1.3〜§5.1.4）。

/// setup code の parse エラー。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetupCodeError {
    BadLength,
    BadChar,
    BadCheckDigit,
    BadPrefix,
    ZeroPasscode,
}

impl std::fmt::Display for SetupCodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SetupCodeError::BadLength => write!(f, "setup code has an unexpected length"),
            SetupCodeError::BadChar => write!(f, "setup code contains an invalid character"),
            SetupCodeError::BadCheckDigit => write!(f, "manual code check digit mismatch"),
            SetupCodeError::BadPrefix => write!(f, "QR payload is missing the \"MT:\" prefix"),
            SetupCodeError::ZeroPasscode => write!(f, "setup passcode must not be zero"),
        }
    }
}

impl std::error::Error for SetupCodeError {}

/// QR onboarding payload（spec §5.1.4）をデコードした結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetupPayload {
    pub version: u8,
    pub vendor_id: u16,
    pub product_id: u16,
    pub custom_flow: u8,
    pub discovery_capabilities: u8,
    /// 12-bit long discriminator。
    pub discriminator: u16,
    pub passcode: u32,
}

/// manual pairing code（spec §5.1.3）をデコードした結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ManualCode {
    pub passcode: u32,
    /// 4-bit short discriminator。
    pub short_discriminator: u8,
}

/// QR payload の base38 アルファベット（spec §5.1.4.1）。
const BASE38_ALPHABET: &[u8; 38] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ-.";

const QR_PREFIX: &str = "MT:";

/// onboarding payload のビット幅（spec §5.1.4.2 Table 39）。version から
/// passcode までで 84 bit、4 bit padding を足して計 88 bit = 11 byte。
const PAYLOAD_BYTES: usize = 11;

fn base38_char_value(c: u8) -> Option<u32> {
    BASE38_ALPHABET
        .iter()
        .position(|&x| x == c)
        .map(|i| i as u32)
}

/// bytes を 3 byte 単位の group に分け、group ごとに little-endian u32 を
/// base38 で桁詰め（5 文字。余り 2 byte → 4 文字、1 byte → 2 文字）。
fn base38_encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    let mut i = 0;
    while i < bytes.len() {
        let remaining = bytes.len() - i;
        let (value, chars) = match remaining {
            1 => (u32::from(bytes[i]), 2),
            2 => (u32::from(bytes[i]) | (u32::from(bytes[i + 1]) << 8), 4),
            _ => (
                u32::from(bytes[i])
                    | (u32::from(bytes[i + 1]) << 8)
                    | (u32::from(bytes[i + 2]) << 16),
                5,
            ),
        };
        out.push_str(&encode_base38_chunk(value, chars));
        i += if remaining >= 3 { 3 } else { remaining };
    }
    out
}

fn encode_base38_chunk(mut value: u32, num_chars: usize) -> String {
    let mut chars = Vec::with_capacity(num_chars);
    for _ in 0..num_chars {
        chars.push(BASE38_ALPHABET[(value % 38) as usize]);
        value /= 38;
    }
    // Safety: BASE38_ALPHABET は ASCII のみなので常に有効な UTF-8。
    String::from_utf8(chars).expect("base38 alphabet is ASCII")
}

/// `base38_encode` の逆変換。group サイズ（5/4/2 文字）は文字列長を 5 で
/// 割った余りから一意に決まる（余り 0/4/2 のみ有効）。
fn base38_decode(s: &str) -> Result<Vec<u8>, SetupCodeError> {
    if !s.is_ascii() {
        return Err(SetupCodeError::BadChar);
    }
    let bytes = s.as_bytes();
    let len = bytes.len();
    let tail = len % 5;
    if tail != 0 && tail != 4 && tail != 2 {
        return Err(SetupCodeError::BadLength);
    }
    let full_groups = len / 5;
    let mut out = Vec::with_capacity(full_groups * 3 + 2);
    for g in 0..full_groups {
        let value = decode_base38_chunk(&bytes[g * 5..g * 5 + 5])?;
        out.push((value & 0xFF) as u8);
        out.push(((value >> 8) & 0xFF) as u8);
        out.push(((value >> 16) & 0xFF) as u8);
    }
    let tail_start = full_groups * 5;
    match tail {
        4 => {
            let value = decode_base38_chunk(&bytes[tail_start..tail_start + 4])?;
            out.push((value & 0xFF) as u8);
            out.push(((value >> 8) & 0xFF) as u8);
        }
        2 => {
            let value = decode_base38_chunk(&bytes[tail_start..tail_start + 2])?;
            out.push((value & 0xFF) as u8);
        }
        _ => {}
    }
    Ok(out)
}

fn decode_base38_chunk(chars: &[u8]) -> Result<u64, SetupCodeError> {
    let mut value: u64 = 0;
    for (i, &c) in chars.iter().enumerate() {
        let digit = base38_char_value(c).ok_or(SetupCodeError::BadChar)?;
        value += u64::from(digit) * 38u64.pow(i as u32);
    }
    Ok(value)
}

/// SetupPayload を LSB-first で 11 byte（88 bit）に詰める（spec §5.1.4.2）。
/// フィールド順: version(3) / vendor_id(16) / product_id(16) /
/// custom_flow(2) / discovery_capabilities(8) / discriminator(12) /
/// passcode(27) / padding(4)。
fn pack_payload(p: &SetupPayload) -> [u8; PAYLOAD_BYTES] {
    let mut acc: u128 = 0;
    let mut offset = 0u32;
    let mut push = |value: u32, bits: u32| {
        acc |= u128::from(value) << offset;
        offset += bits;
    };
    push(u32::from(p.version), 3);
    push(u32::from(p.vendor_id), 16);
    push(u32::from(p.product_id), 16);
    push(u32::from(p.custom_flow), 2);
    push(u32::from(p.discovery_capabilities), 8);
    push(u32::from(p.discriminator), 12);
    push(p.passcode, 27);
    // padding(4) は 0 のまま。

    let le = acc.to_le_bytes();
    let mut out = [0u8; PAYLOAD_BYTES];
    out.copy_from_slice(&le[..PAYLOAD_BYTES]);
    out
}

fn extract_bits(acc: u128, offset: u32, bits: u32) -> u128 {
    let mask = (1u128 << bits) - 1;
    (acc >> offset) & mask
}

/// `pack_payload` の逆変換。先頭 11 byte のみを見る。
fn unpack_payload(bytes: &[u8]) -> SetupPayload {
    let mut buf = [0u8; 16];
    buf[..PAYLOAD_BYTES].copy_from_slice(&bytes[..PAYLOAD_BYTES]);
    let acc = u128::from_le_bytes(buf);

    let mut offset = 0u32;
    let mut take = |bits: u32| -> u128 {
        let v = extract_bits(acc, offset, bits);
        offset += bits;
        v
    };
    let version = take(3) as u8;
    let vendor_id = take(16) as u16;
    let product_id = take(16) as u16;
    let custom_flow = take(2) as u8;
    let discovery_capabilities = take(8) as u8;
    let discriminator = take(12) as u16;
    let passcode = take(27) as u32;

    SetupPayload {
        version,
        vendor_id,
        product_id,
        custom_flow,
        discovery_capabilities,
        discriminator,
        passcode,
    }
}

/// QR onboarding payload（`MT:` プレフィックス必須）を parse する
/// （spec §5.1.4）。TLV の optional data が続いていても先頭 11 byte
/// だけを見て読み捨てる。
pub fn parse_qr(s: &str) -> Result<SetupPayload, SetupCodeError> {
    let body = s.strip_prefix(QR_PREFIX).ok_or(SetupCodeError::BadPrefix)?;
    let bytes = base38_decode(body)?;
    if bytes.len() < PAYLOAD_BYTES {
        return Err(SetupCodeError::BadLength);
    }
    let p = unpack_payload(&bytes);
    if p.passcode == 0 {
        return Err(SetupCodeError::ZeroPasscode);
    }
    Ok(p)
}

/// `SetupPayload` を `MT:` プレフィックス付き QR payload 文字列に encode
/// する。optional TLV は付与しない。
pub fn encode_qr(p: &SetupPayload) -> String {
    let bytes = pack_payload(p);
    format!("{QR_PREFIX}{}", base38_encode(&bytes))
}

/// Verhoeff の乗算表 d / 順列表 p / 逆元表 inv（標準テーブルをそのまま
/// 定数化）。
const D: [[u8; 10]; 10] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
    [1, 2, 3, 4, 0, 6, 7, 8, 9, 5],
    [2, 3, 4, 0, 1, 7, 8, 9, 5, 6],
    [3, 4, 0, 1, 2, 8, 9, 5, 6, 7],
    [4, 0, 1, 2, 3, 9, 5, 6, 7, 8],
    [5, 9, 8, 7, 6, 0, 4, 3, 2, 1],
    [6, 5, 9, 8, 7, 1, 0, 4, 3, 2],
    [7, 6, 5, 9, 8, 2, 1, 0, 4, 3],
    [8, 7, 6, 5, 9, 3, 2, 1, 0, 4],
    [9, 8, 7, 6, 5, 4, 3, 2, 1, 0],
];
const P: [[u8; 10]; 8] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9],
    [1, 5, 7, 6, 2, 8, 3, 0, 9, 4],
    [5, 8, 0, 3, 7, 9, 6, 1, 4, 2],
    [8, 9, 1, 6, 0, 4, 3, 5, 2, 7],
    [9, 4, 5, 3, 1, 2, 6, 8, 7, 0],
    [4, 2, 8, 6, 5, 7, 3, 9, 0, 1],
    [2, 7, 9, 3, 8, 0, 6, 4, 1, 5],
    [7, 0, 4, 6, 9, 1, 3, 2, 5, 8],
];
const INV: [u8; 10] = [0, 4, 3, 2, 1, 5, 6, 7, 8, 9];

/// 検査桁の生成: payload（検査桁を含まない）に対し右端から位置 1,2,... で畳み込む。
fn verhoeff_check_digit(payload: &[u8]) -> u8 {
    let mut c = 0u8;
    for (i, d) in payload.iter().rev().enumerate() {
        c = D[usize::from(c)][usize::from(P[(i + 1) % 8][usize::from(*d)])];
    }
    INV[usize::from(c)]
}

/// manual pairing code の 11/21 桁の数字列を数字配列に変換する。数字
/// 以外の文字があれば `BadChar`。
fn digits_of(s: &str) -> Result<Vec<u8>, SetupCodeError> {
    s.bytes()
        .map(|b| {
            if b.is_ascii_digit() {
                Ok(b - b'0')
            } else {
                Err(SetupCodeError::BadChar)
            }
        })
        .collect()
}

fn digits_to_u32(digits: &[u8]) -> u32 {
    digits.iter().fold(0u32, |acc, &d| acc * 10 + u32::from(d))
}

/// manual pairing code（11 桁 / 21 桁両対応。VID/PID は読み捨て）を
/// parse する（spec §5.1.3）。
pub fn parse_manual_code(s: &str) -> Result<ManualCode, SetupCodeError> {
    if s.len() != 11 && s.len() != 21 {
        return Err(SetupCodeError::BadLength);
    }
    let digits = digits_of(s)?;
    let (payload, check_digit) = digits.split_at(digits.len() - 1);
    let check_digit = check_digit[0];
    if verhoeff_check_digit(payload) != check_digit {
        return Err(SetupCodeError::BadCheckDigit);
    }

    let digit1 = u32::from(digits[0]);
    let short_disc_top2 = digit1 & 0x3;

    let digits2_6 = digits_to_u32(&digits[1..6]);
    let short_disc_bottom2 = (digits2_6 >> 14) & 0x3;
    let passcode_low14 = digits2_6 & 0x3FFF;

    let digits7_10 = digits_to_u32(&digits[6..10]);
    let passcode = (digits7_10 << 14) | passcode_low14;

    let short_discriminator = ((short_disc_top2 << 2) | short_disc_bottom2) as u8;

    if passcode == 0 {
        return Err(SetupCodeError::ZeroPasscode);
    }

    Ok(ManualCode {
        passcode,
        short_discriminator,
    })
}

/// `ManualCode` を 11 桁の manual pairing code 文字列に encode する
/// （VID/PID は付与しない）。
pub fn encode_manual_code(passcode: u32, short_discriminator: u8) -> String {
    let short_disc = short_discriminator & 0xF;
    let digit1 = u32::from(short_disc >> 2); // vid_pid_present = 0
    let digits2_6 = (u32::from(short_disc & 0x3) << 14) | (passcode & 0x3FFF);
    let digits7_10 = passcode >> 14;

    let body = format!("{digit1}{digits2_6:05}{digits7_10:04}");
    let digits = digits_of(&body).expect("format! only emits ascii digits");
    let check = verhoeff_check_digit(&digits);
    format!("{body}{check}")
}

#[cfg(test)]
mod tests {
    use super::*;

    // 周知のテストペア: chip 全クラスタアプリの onboarding payload。
    // passcode 20202021 / long discriminator 3840 / VID 0xFFF1 / PID 0x8001。
    const QR: &str = "MT:-24J0AFN00KA0648G00";

    #[test]
    fn parses_known_qr() {
        let p = parse_qr(QR).unwrap();
        assert_eq!(p.version, 0);
        assert_eq!(p.vendor_id, 0xFFF1);
        assert_eq!(p.product_id, 0x8001);
        assert_eq!(p.custom_flow, 0);
        assert_eq!(p.discovery_capabilities, 0x04); // on-network
        assert_eq!(p.discriminator, 3840);
        assert_eq!(p.passcode, 20202021);
    }

    #[test]
    fn qr_round_trip() {
        let p = parse_qr(QR).unwrap();
        assert_eq!(encode_qr(&p), QR);
    }

    // 周知のテストペア: manual code 34970112332
    // = passcode 20202021 / short discriminator 15。
    #[test]
    fn parses_known_manual_code() {
        let m = parse_manual_code("34970112332").unwrap();
        assert_eq!(m.passcode, 20202021);
        assert_eq!(m.short_discriminator, 15);
    }

    #[test]
    fn manual_code_round_trip() {
        assert_eq!(encode_manual_code(20202021, 15), "34970112332");
    }

    #[test]
    fn manual_code_rejects_bad_check_digit() {
        assert!(matches!(
            parse_manual_code("34970112331"),
            Err(SetupCodeError::BadCheckDigit)
        ));
    }

    #[test]
    fn qr_rejects_missing_prefix() {
        assert!(matches!(
            parse_qr("-24J0AFN00KA0648G00"),
            Err(SetupCodeError::BadPrefix)
        ));
    }
}
