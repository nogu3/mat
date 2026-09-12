//! hex 文字列 ⇄ バイト列。`hex` crate を足すほどではない小さな往復を、
//! 各クレートで手書きしていたのを一本化する（監査 2026-09-12 Tier 6）。

/// 小文字 hex（`00ff…`）。
pub fn encode_lower(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 大文字 hex（`00FF…`）。CFID や ExtAddress の正準形が使う。
pub fn encode_upper(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02X}")).collect()
}

/// hex 文字列をバイト列へ。奇数長・非 hex 文字は `None`。`0x` 接頭辞は
/// 剥がさない（呼び手の責務）。大文字小文字は不問。
pub fn decode(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_lower_and_upper() {
        assert_eq!(encode_lower(&[0x00, 0xAB, 0xff]), "00abff");
        assert_eq!(encode_upper(&[0x00, 0xAB, 0xff]), "00ABFF");
        assert_eq!(encode_lower(&[]), "");
    }

    #[test]
    fn decode_roundtrips_and_rejects_bad_input() {
        assert_eq!(decode("00abFF"), Some(vec![0x00, 0xab, 0xff]));
        assert_eq!(decode(""), Some(vec![]));
        assert_eq!(decode("abc"), None, "odd length");
        assert_eq!(decode("zz"), None, "non-hex");
        assert_eq!(decode("0x00"), None, "prefix is not stripped here");
        let bytes: Vec<u8> = (0..=255).collect();
        assert_eq!(decode(&encode_lower(&bytes)).unwrap(), bytes);
        assert_eq!(decode(&encode_upper(&bytes)).unwrap(), bytes);
    }
}
