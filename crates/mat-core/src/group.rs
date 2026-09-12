//! group（groupcast）の共有ロジック。`mat group`（one-shot）と `matd` の group op が
//! 同じ epoch 鍵の検証・生成を使うよう、一箇所で保守する。
//!
//! group state（鍵束・GroupKeyMap）自体は `mat`/`matd` 独自台帳を持たず、mat が
//! 所有する chip-tool INI 互換 KVS（`mat-controller::group_settings`）に置く
//! （設計ルール 4）。ここにあるのは値の検証・生成・整形だけ。

use crate::error::{ErrorKind, MatError};

/// `--epoch-key` の妥当性検証（16バイト = 32桁 hex）。小文字へ正規化して返す。
pub fn validate_epoch_key(key: &str) -> Result<String, MatError> {
    let trimmed = key.strip_prefix("0x").unwrap_or(key);
    if trimmed.len() == 32 && trimmed.chars().all(|c| c.is_ascii_hexdigit()) {
        Ok(trimmed.to_ascii_lowercase())
    } else {
        Err(MatError::new(
            ErrorKind::Other,
            format!(
                "invalid --epoch-key: expected 32 hex chars (16 bytes), got {} chars",
                trimmed.len()
            ),
        ))
    }
}

/// ランダムな 16 バイトの epoch key。
pub fn generate_epoch_key_bytes() -> [u8; 16] {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).expect("getrandom failed to fill epoch key");
    bytes
}

/// ランダムな 16 バイトの epoch key を生成し 32桁 hex で返す（CLI 表示・
/// ワイヤ用。バイト列が要る呼び手は [`generate_epoch_key_bytes`]）。
pub fn generate_epoch_key() -> String {
    crate::hex::encode_lower(&generate_epoch_key_bytes())
}

/// epoch key を決める: 明示指定があれば検証して採用、無ければランダム生成。
pub fn resolve_epoch_key(epoch_key: Option<&str>) -> Result<String, MatError> {
    match epoch_key {
        Some(k) => validate_epoch_key(k),
        None => Ok(generate_epoch_key()),
    }
}

/// [`resolve_epoch_key`] のバイト列版。hex → bytes の往復を呼び手（provision /
/// rotate-ipk）が各自やっていたのを一本化する。
pub fn resolve_epoch_key_bytes(epoch_key: Option<&str>) -> Result<[u8; 16], MatError> {
    match epoch_key {
        Some(k) => {
            let hex = validate_epoch_key(k)?;
            let bytes = crate::hex::decode(&hex).expect("validated as 32 hex chars");
            Ok(<[u8; 16]>::try_from(bytes).expect("32 hex chars = 16 bytes"))
        }
        None => Ok(generate_epoch_key_bytes()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_epoch_key_accepts_32_hex() {
        let k = "00112233445566778899aabbccddeeff";
        assert_eq!(validate_epoch_key(k).unwrap(), k);
        // 0x 接頭辞と大文字も受ける（小文字へ正規化）。
        assert_eq!(
            validate_epoch_key("0x00112233445566778899AABBCCDDEEFF").unwrap(),
            k
        );
    }

    #[test]
    fn validate_epoch_key_rejects_bad_length_or_chars() {
        assert_eq!(
            validate_epoch_key("dead").unwrap_err().kind,
            ErrorKind::Other
        );
        // 32桁だが非 hex 文字。
        let bad = "zz112233445566778899aabbccddeeff";
        assert_eq!(validate_epoch_key(bad).unwrap_err().kind, ErrorKind::Other);
    }

    #[test]
    fn generated_epoch_key_is_32_hex() {
        let k = generate_epoch_key();
        assert_eq!(k.len(), 32);
        assert!(k.chars().all(|c| c.is_ascii_hexdigit()));
        // 2回生成して異なる（乱数であること）。
        assert_ne!(k, generate_epoch_key());
    }

    #[test]
    fn generated_epoch_key_bytes_are_random_and_hex_form_matches() {
        let a = generate_epoch_key_bytes();
        let b = generate_epoch_key_bytes();
        assert_ne!(a, b);
        // string 版は bytes 版の小文字 hex（両 API の一致を釘打ち）。
        let s = generate_epoch_key();
        assert_eq!(s.len(), 32);
        assert!(s
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn resolve_epoch_key_bytes_decodes_explicit_key_and_normalizes_case() {
        let k = resolve_epoch_key_bytes(Some("0x00112233445566778899AABBCCDDEEFF")).unwrap();
        assert_eq!(
            k,
            [
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff
            ]
        );
        assert_eq!(
            resolve_epoch_key_bytes(Some("dead")).unwrap_err().kind,
            ErrorKind::Other
        );
        // None = 生成（2 回で異なる）。
        assert_ne!(
            resolve_epoch_key_bytes(None).unwrap(),
            resolve_epoch_key_bytes(None).unwrap()
        );
    }
}
