//! stdout は純粋な構造化 JSON のみ。人間装飾は混ぜない。
//! 全レスポンスに ISO 8601 の `timestamp`（`mat` が応答を組み立てた時刻）を付ける。

use chrono::{Duration, Local};
use serde_json::Value;

/// 現在時刻を ISO 8601（ローカルタイムゾーン、オフセット付き）で返す。
pub fn now_iso8601() -> String {
    Local::now().to_rfc3339()
}

/// 現在時刻 + `seconds` 秒を ISO 8601 で返す（`open-window` の `expires_at` 用）。
pub fn expires_in(seconds: i64) -> String {
    (Local::now() + Duration::seconds(seconds)).to_rfc3339()
}

/// `timestamp` を先頭に差し込んで stdout へ1行 JSON を出す。
///
/// `body` はオブジェクトを想定。オブジェクトでなければ `data` キーに包む。
pub fn emit(body: Value) {
    let out = match body {
        Value::Object(mut map) => {
            // 既存の timestamp は尊重しつつ、無ければ付与。
            map.entry("timestamp".to_string())
                .or_insert_with(|| Value::String(now_iso8601()));
            Value::Object(map)
        }
        other => serde_json::json!({
            "timestamp": now_iso8601(),
            "data": other,
        }),
    };
    println!("{out}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn iso8601_has_offset() {
        let ts = now_iso8601();
        // 例: 2026-06-06T12:34:56+09:00 — 'T' 区切りとオフセット記号を含む。
        assert!(ts.contains('T'));
        assert!(ts.contains('+') || ts.contains('-'));
    }
}
