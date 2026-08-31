//! `mat` のスキーマへ正規化するための共有パーサ / データ型。
//!
//! chip-tool 撤去（M8c-3）に伴い、chip-tool のログ志向テキスト出力を読むパーサ群は
//! 撤去済み。ここに残るのは native 経路でも使う汎用の値正規化と、native mDNS 探索
//! 結果を `mat` のスキーマへ写すためのデータ型のみ。

use serde::Serialize;

/// `mat discover` が返す1デバイス分。
#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
pub struct DiscoveredDevice {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hostname: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub addresses: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub discriminator: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vendor_id: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub product_id: Option<u32>,
}

/// chip-tool の生テキスト値（または write の CLI 入力値）を `mat` の JSON 値へ
/// 正規化する。read と write で同じ型付けを使い、出力 value の型を一貫させる。
pub fn normalize_value(raw: &str) -> serde_json::Value {
    // 文字列リテラル。実機 chip-tool は文字列に長さ注釈を付ける
    // （`"ha-thread-6562" (14 chars)`）ため、最初の閉じ引用符までを値とし、後続注釈は
    // 捨てる。注釈なし（`"living room"`）も同じ経路で通る。
    if let Some(rest) = raw.strip_prefix('"') {
        if let Some(end) = rest.find('"') {
            return serde_json::Value::String(rest[..end].to_string());
        }
    }
    // 実機 chip-tool は数値に型注釈を付ける（`191 (unsigned)` / `-5 (signed)`）。
    // 先頭トークンを値とみなす。注釈なし（`191`）も同じ経路で通る。bool/null は
    // そもそも注釈が付かないが、先頭トークン基準でも同じ結果になる。
    let head = raw.split_whitespace().next().unwrap_or(raw);
    match head.to_ascii_lowercase().as_str() {
        "true" => return serde_json::Value::Bool(true),
        "false" => return serde_json::Value::Bool(false),
        "null" => return serde_json::Value::Null,
        _ => {}
    }
    if let Ok(i) = head.parse::<i64>() {
        return serde_json::Value::from(i);
    }
    if let Ok(u) = head.parse::<u64>() {
        return serde_json::Value::from(u);
    }
    if let Ok(f) = head.parse::<f64>() {
        if let Some(n) = serde_json::Number::from_f64(f) {
            return serde_json::Value::Number(n);
        }
    }
    serde_json::Value::String(raw.to_string())
}
