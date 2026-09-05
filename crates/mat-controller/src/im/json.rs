//! IM の TLV 値 → JSON 変換（read / listen の出力形）。struct のキーは
//! context tag の 10 進文字列、bytes は小文字 hex 文字列。

use crate::tlv::{Element, Reader, Tag, Value};

use super::{skip_container, ImError};

/// TLV 単一要素（コンテナ含む）を JSON へ。`first` は既に読んだ先頭要素。
/// JSON 化規約（固定）: Bool→bool, Uint/Int→number, F32/F64→number,
/// Utf8→string, Bytes→小文字hex文字列, Null→null, Array/List→JSON array,
/// Struct→JSON object（キーは context tag 番号の10進文字列。名前付けは
/// 上位層の責務）。
pub(super) fn tlv_element_to_json(
    r: &mut Reader,
    first: Element,
) -> Result<serde_json::Value, ImError> {
    tlv_element_to_json_impl(r, first, 0)
}

/// 1 要素の well-formed TLV（container 可）を JSON へ。`tlv_element_to_json` の
/// 公開入口 — 汎用 write の値ツリー符号化が read 側の JSON 化と同形になることを
/// mat-native のテストで固定するために使う。
pub fn tlv_to_json(tlv: &[u8]) -> Result<serde_json::Value, ImError> {
    let mut r = Reader::new(tlv);
    let first = r.next()?.ok_or(ImError::Malformed("empty tlv"))?;
    tlv_element_to_json(&mut r, first)
}

fn tlv_element_to_json_impl(
    r: &mut Reader,
    first: Element,
    depth: usize,
) -> Result<serde_json::Value, ImError> {
    const MAX_DEPTH: usize = 32;
    if depth > MAX_DEPTH {
        return Err(ImError::Malformed("tlv nesting too deep"));
    }

    use serde_json::Value as J;
    Ok(match first.value {
        Value::Bool(b) => J::Bool(b),
        Value::Uint(u) => J::from(u),
        Value::Int(i) => J::from(i),
        Value::F32(f) => serde_json::json!(f),
        Value::F64(f) => serde_json::json!(f),
        Value::Utf8(s) => J::String(s.to_string()),
        Value::Bytes(b) => J::String(hex_lower(b)),
        Value::Null => J::Null,
        Value::ArrayStart | Value::ListStart => {
            let mut items = Vec::new();
            loop {
                let el = r.next()?.ok_or(ImError::Malformed("truncated array"))?;
                if el.value == Value::ContainerEnd {
                    break;
                }
                items.push(tlv_element_to_json_impl(r, el, depth + 1)?);
            }
            J::Array(items)
        }
        Value::StructStart => {
            let mut map = serde_json::Map::new();
            loop {
                let el = r.next()?.ok_or(ImError::Malformed("truncated struct"))?;
                if el.value == Value::ContainerEnd {
                    break;
                }
                let key = match el.tag {
                    Tag::Context(n) => n.to_string(),
                    _ => {
                        // 想定外タグはスキップ（前方互換）。ただしそれがコンテナ開始ならば
                        // 中身を読み飛ばす（そうでないと兄弟フィールドの解釈が壊れる）。
                        if matches!(
                            el.value,
                            Value::StructStart | Value::ArrayStart | Value::ListStart
                        ) {
                            skip_container(r)?;
                        }
                        continue;
                    }
                };
                map.insert(key, tlv_element_to_json_impl(r, el, depth + 1)?);
            }
            J::Object(map)
        }
        Value::ContainerEnd => return Err(ImError::Malformed("dangling container end")),
    })
}

fn hex_lower(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use crate::im::*;
    use crate::tlv::{Tag, Writer};

    #[test]
    fn tlv_to_json_rejects_pathological_nesting() {
        // Construct deeply nested TLV (100 levels) to test stack overflow protection.
        // ArrayStart × 100 without matching ContainerEnd should fail with Malformed
        // when depth limit (32) is exceeded.
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(1));
        w.start_struct(Tag::Anonymous);
        w.start_struct(Tag::Context(1));
        w.start_list(Tag::Context(1));
        w.put_uint(Tag::Context(4), 1);
        w.end_container();
        // Data (Context(2)) with 100 nested arrays
        for _ in 0..100 {
            w.start_array(Tag::Context(2));
        }
        for _ in 0..100 {
            w.end_container();
        }
        w.end_container();
        w.end_container();
        w.end_container();
        w.end_container();
        let err = decode_report_data_message(&w.finish()).unwrap_err();
        assert!(
            matches!(err, ImError::Malformed(_)),
            "Expected Malformed error, got {err:?}"
        );
    }
}
