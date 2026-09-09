//! codec / device_codec 共有の TLV struct フィールド走査ヘルパ（context tag →
//! 値の map 化、範囲付き整数の取り出し）。

use std::collections::BTreeMap;

use crate::tlv::{Element, Reader, Tag, Value};

use super::CommissionError;

/// `fields` の次要素を読み、TLV 復号エラー / 末尾切れをまとめて
/// `CommissionError::Malformed` に変換する。
pub(super) fn next_el<'a>(
    r: &mut Reader<'a>,
    step: &'static str,
) -> Result<Element<'a>, CommissionError> {
    r.next()
        .map_err(|_| CommissionError::Malformed {
            step,
            detail: "tlv decode error",
        })?
        .ok_or(CommissionError::Malformed {
            step,
            detail: "truncated",
        })
}

/// 先頭要素が struct start であることを確認して読み捨てる。
pub(super) fn expect_struct(r: &mut Reader, step: &'static str) -> Result<(), CommissionError> {
    match next_el(r, step)?.value {
        Value::StructStart => Ok(()),
        _ => Err(CommissionError::Malformed {
            step,
            detail: "expected struct",
        }),
    }
}

/// 未知のタグに付随するコンテナを、対応する `ContainerEnd` まで読み飛ばす
/// （深さ 1 の状態、つまり start 要素は読み終わっている前提）。
pub(super) fn skip_container(r: &mut Reader, step: &'static str) -> Result<(), CommissionError> {
    let mut depth = 1usize;
    while depth > 0 {
        match next_el(r, step)?.value {
            Value::StructStart | Value::ArrayStart | Value::ListStart => depth += 1,
            Value::ContainerEnd => depth -= 1,
            _ => {}
        }
    }
    Ok(())
}

/// [`scan_struct_fields`] が集める TLV leaf 要素の値。以下の decoder が読む
/// 応答はすべて「フラットな struct 直下に leaf だけが並ぶ」形（ネストした
/// struct/array/list を持つフィールドは無い）なのでコンテナ型は持たない
/// ——コンテナタグは `skip_container` で読み飛ばされ、この enum には現れない。
pub(super) enum FieldValue {
    Uint(u64),
    Bytes(Vec<u8>),
    Utf8(String),
}

/// `fields` の TLV struct 1 段をスキャンし、直下の leaf 要素を
/// `{contextTag: value}` に集める（同じタグが複数回現れたら後勝ち——1 回の
/// ループで都度代入していた旧実装と同じ挙動）。Task 9 で書かれた 6 個の
/// decoder はどれも「struct を開いて直下のタグ別 leaf を拾い、知らない
/// コンテナは読み飛ばす」という同型のループだった。ここに 1 箇所へ集約し、
/// 各 decoder は `take_*` でタグを引くだけにする。
pub(super) fn scan_struct_fields(
    fields: &[u8],
    step: &'static str,
) -> Result<BTreeMap<u8, FieldValue>, CommissionError> {
    let mut r = Reader::new(fields);
    expect_struct(&mut r, step)?;
    let mut out = BTreeMap::new();
    loop {
        let el = next_el(&mut r, step)?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(t), Value::Uint(v)) => {
                out.insert(t, FieldValue::Uint(v));
            }
            (Tag::Context(t), Value::Bytes(b)) => {
                out.insert(t, FieldValue::Bytes(b.to_vec()));
            }
            (Tag::Context(t), Value::Utf8(s)) => {
                out.insert(t, FieldValue::Utf8(s.to_string()));
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r, step)?;
            }
            _ => {}
        }
    }
    Ok(out)
}

/// `map` からタグ `tag` を u8 として取り出す。タグが無い、または値が
/// `Uint` 以外の型なら「無かった」扱いで `Ok(None)`（旧実装で型不一致の
/// 分岐が黙って読み捨てられていたのと同じ）。`Uint` ではあるが u8 に収まら
/// ない場合だけ `range_detail` で `Malformed` を返す。
pub(super) fn take_u8(
    map: &mut BTreeMap<u8, FieldValue>,
    tag: u8,
    step: &'static str,
    range_detail: &'static str,
) -> Result<Option<u8>, CommissionError> {
    match map.remove(&tag) {
        Some(FieldValue::Uint(v)) => Ok(Some(u8::try_from(v).map_err(|_| {
            CommissionError::Malformed {
                step,
                detail: range_detail,
            }
        })?)),
        _ => Ok(None),
    }
}

/// `map` からタグ `tag` を `Vec<u8>` として取り出す（型不一致は「無かっ
/// た」扱い、[`take_u8`] と同じ方針）。
pub(super) fn take_bytes(map: &mut BTreeMap<u8, FieldValue>, tag: u8) -> Option<Vec<u8>> {
    match map.remove(&tag) {
        Some(FieldValue::Bytes(b)) => Some(b),
        _ => None,
    }
}

/// `map` からタグ `tag` を `String` として取り出す（型不一致は「無かっ
/// た」扱い、[`take_u8`] と同じ方針）。
pub(super) fn take_utf8(map: &mut BTreeMap<u8, FieldValue>, tag: u8) -> Option<String> {
    match map.remove(&tag) {
        Some(FieldValue::Utf8(s)) => Some(s),
        _ => None,
    }
}

/// [`take_u8`] の u16 版（Task 9 のデバイス側 decoder が使う——
/// ExpiryLengthSeconds / AdminVendorId は u16 幅）。
pub(super) fn take_u16(
    map: &mut BTreeMap<u8, FieldValue>,
    tag: u8,
    step: &'static str,
    range_detail: &'static str,
) -> Result<Option<u16>, CommissionError> {
    match map.remove(&tag) {
        Some(FieldValue::Uint(v)) => Ok(Some(u16::try_from(v).map_err(|_| {
            CommissionError::Malformed {
                step,
                detail: range_detail,
            }
        })?)),
        _ => Ok(None),
    }
}

/// [`take_u8`] の u32 版（`OpenCommissioningWindow` の Iterations は u32
/// 幅）。
pub(super) fn take_u32(
    map: &mut BTreeMap<u8, FieldValue>,
    tag: u8,
    step: &'static str,
    range_detail: &'static str,
) -> Result<Option<u32>, CommissionError> {
    match map.remove(&tag) {
        Some(FieldValue::Uint(v)) => Ok(Some(u32::try_from(v).map_err(|_| {
            CommissionError::Malformed {
                step,
                detail: range_detail,
            }
        })?)),
        _ => Ok(None),
    }
}

/// `map` からタグ `tag` を `u64` として取り出す（型不一致は「無かった」
/// 扱い、[`take_u8`] と同じ方針。u64 はそのままなので範囲チェック不要）。
pub(super) fn take_u64(map: &mut BTreeMap<u8, FieldValue>, tag: u8) -> Option<u64> {
    match map.remove(&tag) {
        Some(FieldValue::Uint(v)) => Some(v),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        decode_add_noc, decode_arm_fail_safe, decode_attestation_response,
        decode_commissioning_status_response, decode_noc_response,
    };
    use super::*;
    use crate::tlv::{Tag, Writer};

    // ---- 監査レーン D: デコーダの負系・境界（実機不要） ----

    /// 全デコーダ共通の「構造が壊れた入力」— 空 / 先頭が struct でない /
    /// ContainerEnd が無い / 制御バイトが不正。どれも `Malformed` になること
    /// （panic しない・Ok にならない）を代表デコーダ 5 本で確認する。
    #[test]
    fn decoders_reject_structurally_broken_input() {
        type Decoder = Box<dyn Fn(&[u8]) -> Result<(), CommissionError>>;
        let decoders: Vec<(&str, Decoder)> = vec![
            (
                "noc_response",
                Box::new(|b: &[u8]| decode_noc_response(b).map(|_| ())),
            ),
            (
                "attestation_response",
                Box::new(|b: &[u8]| decode_attestation_response(b).map(|_| ())),
            ),
            (
                "add_noc",
                Box::new(|b: &[u8]| decode_add_noc(b).map(|_| ())),
            ),
            (
                "arm_fail_safe",
                Box::new(|b: &[u8]| decode_arm_fail_safe(b).map(|_| ())),
            ),
            (
                "commissioning_status_response",
                Box::new(|b: &[u8]| decode_commissioning_status_response(b).map(|_| ())),
            ),
        ];

        let not_struct = {
            let mut w = Writer::new();
            w.put_uint(Tag::Anonymous, 1);
            w.finish()
        };
        let unterminated = {
            let mut w = Writer::new();
            w.start_struct(Tag::Anonymous);
            w.put_uint(Tag::Context(0), 0);
            w.end_container();
            let mut b = w.finish();
            b.pop(); // ContainerEnd (0x18) を落とす
            b
        };
        let inputs: Vec<(&str, Vec<u8>)> = vec![
            ("empty", Vec::new()),
            ("not-struct", not_struct),
            ("unterminated", unterminated),
            ("garbage", vec![0xFF, 0xFF, 0xFF]),
        ];

        for (dname, dec) in &decoders {
            for (iname, input) in &inputs {
                match dec(input) {
                    Err(CommissionError::Malformed { .. }) => {}
                    other => panic!("{dname} on {iname}: expected Malformed, got {other:?}"),
                }
            }
        }
    }

    #[test]
    fn decoders_treat_wrong_field_type_as_missing() {
        // Bytes 期待のタグに Utf8 → 「無い」扱いで missing。
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_str(Tag::Context(0), "not bytes");
        w.put_bytes(Tag::Context(1), &[0u8; 64]);
        w.end_container();
        match decode_attestation_response(&w.finish()) {
            Err(CommissionError::Malformed { detail, .. }) => {
                assert_eq!(detail, "missing elements")
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
        // Uint 期待のタグに Bytes → missing statusCode。
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(0), &[0]);
        w.end_container();
        match decode_noc_response(&w.finish()) {
            Err(CommissionError::Malformed { detail, .. }) => {
                assert_eq!(detail, "missing statusCode")
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn take_helpers_reject_out_of_range_uints() {
        // take_u8 経由: statusCode = 256
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 256);
        w.end_container();
        match decode_noc_response(&w.finish()) {
            Err(CommissionError::Malformed { detail, .. }) => {
                assert_eq!(detail, "statusCode out of range")
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
        // take_u16 経由: expiry = 65536
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 65_536);
        w.end_container();
        match decode_arm_fail_safe(&w.finish()) {
            Err(CommissionError::Malformed { detail, .. }) => {
                assert_eq!(detail, "expiry out of range")
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
        // 上限ぴったりは通る。
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 65_535);
        w.end_container();
        assert_eq!(decode_arm_fail_safe(&w.finish()).unwrap(), (65_535, 0));
    }

    #[test]
    fn scan_skips_nested_containers_under_unknown_tags() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 7);
        w.start_struct(Tag::Context(9)); // 未知タグのネスト
        w.start_array(Tag::Context(1));
        w.start_list(Tag::Anonymous);
        w.put_uint(Tag::Anonymous, 1);
        w.end_container();
        w.end_container();
        w.put_str(Tag::Context(2), "inner");
        w.end_container();
        w.put_str(Tag::Context(1), "x");
        w.end_container();
        let fields = w.finish();
        assert_eq!(
            decode_commissioning_status_response(&fields).unwrap(),
            (7, "x".to_string())
        );

        // 入れ子の閉じが無い → truncated。内側 struct の直後で切れるよう、
        // leaf 1 個だけの入れ子を作り末尾の ContainerEnd 2 個（内・外）を落とす。
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 7);
        w.start_struct(Tag::Context(9));
        w.put_uint(Tag::Context(1), 1);
        w.end_container();
        w.end_container();
        let mut cut = w.finish();
        cut.truncate(cut.len() - 2);
        match decode_commissioning_status_response(&cut) {
            Err(CommissionError::Malformed { detail, .. }) => assert_eq!(detail, "truncated"),
            other => panic!("expected Malformed(truncated), got {other:?}"),
        }
    }

    #[test]
    fn scan_struct_fields_duplicate_tag_last_wins() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 1);
        w.put_uint(Tag::Context(0), 5);
        w.end_container();
        assert_eq!(
            decode_commissioning_status_response(&w.finish()).unwrap().0,
            5
        );
    }
}
