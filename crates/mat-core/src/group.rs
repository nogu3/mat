//! group（groupcast）の共有ロジック。`mat group`（one-shot）と `matd` の group op が
//! 同じ鍵生成・宛先 node-id 組み立てを使うよう、一箇所で保守する。
//!
//! group state（鍵束・GroupKeyMap）自体は `mat`/`matd` 独自台帳を持たず chip-tool の
//! 永続ストレージに委ねる（設計ルール 4）。ここにあるのは値の検証・生成・整形だけ。

use crate::error::{ErrorKind, MatError};

/// GroupKeySecurityPolicy。0 = TrustFirst（最初に来た鍵を信頼）。
pub const KEY_SECURITY_POLICY: &str = "0";

/// epoch 鍵の有効開始時刻（EpochStartTime0）。コントローラ側 groupsettings の
/// `add-keysets <keysetId> <keyPolicy> <validityTime> <EpochKey>` の validityTime と、
/// デバイス側 KeySetWrite の epochStartTime0 はこの値で一致させる必要がある
/// （ずれると両者が選ぶ有効 epoch 鍵が食い違い groupcast が復号できない）。
pub const EPOCH_START_TIME: &str = "1";

/// group multicast 宛先の node-id ベース。実 node-id は `BASE | group_id`。
/// 上位48bitが全1（`0xffffffffffff....`）なら group 宛と解釈される。
const GROUP_NODE_ID_BASE: u64 = 0xffff_ffff_ffff_0000;

/// group multicast 宛先の node-id を `0x...` 16桁 hex 文字列で組み立てる。
pub fn group_node_id(group_id: u16) -> String {
    format!("0x{:016x}", GROUP_NODE_ID_BASE | u64::from(group_id))
}

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

/// ランダムな 16 バイトの epoch key を生成し 32桁 hex で返す。
pub fn generate_epoch_key() -> String {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).expect("getrandom failed to fill epoch key");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// epoch key を決める: 明示指定があれば検証して採用、無ければランダム生成。
pub fn resolve_epoch_key(epoch_key: Option<&str>) -> Result<String, MatError> {
    match epoch_key {
        Some(k) => validate_epoch_key(k),
        None => Ok(generate_epoch_key()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn group_node_id_packs_group_into_low_bits() {
        assert_eq!(group_node_id(1), "0xffffffffffff0001");
        assert_eq!(group_node_id(0x1234), "0xffffffffffff1234");
        assert_eq!(group_node_id(0), "0xffffffffffff0000");
    }

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
}
