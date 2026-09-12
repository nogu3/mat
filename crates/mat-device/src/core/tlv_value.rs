//! Standalone scalar TLV values — the `ClusterHandler::read` contract is
//! "one `Tag::Anonymous`-tagged element", and every cluster used to carry
//! its own three-line `Writer::new(); put_*; finish()` copy of this.

use mat_controller::tlv::{Reader, Tag, Value, Writer};

/// One anonymous unsigned-integer element.
pub fn uint(v: u64) -> Vec<u8> {
    let mut w = Writer::new();
    w.put_uint(Tag::Anonymous, v);
    w.finish()
}

/// One anonymous UTF-8 string element.
pub fn str(v: &str) -> Vec<u8> {
    let mut w = Writer::new();
    w.put_str(Tag::Anonymous, v);
    w.finish()
}

/// One anonymous boolean element.
pub fn bool(v: bool) -> Vec<u8> {
    let mut w = Writer::new();
    w.put_bool(Tag::Anonymous, v);
    w.finish()
}

/// One anonymous `null` element — for nullable attributes that must read
/// back distinct from a valid `0` (e.g. `AdminFabricIndex` while the
/// Administrator Commissioning window is closed).
pub fn null() -> Vec<u8> {
    let mut w = Writer::new();
    w.put_null(Tag::Anonymous);
    w.finish()
}

/// Decodes the full-replace form of a list-attribute write: `data_tlv` is
/// an anonymous array whose elements are structs, and `body` reads one
/// element's fields (its `StructStart` already consumed) up to and
/// including its `ContainerEnd`. Any shape mismatch is `None` — callers
/// map that to `STATUS_CONSTRAINT_ERROR`.
pub fn decode_struct_list<T>(
    data_tlv: &[u8],
    body: impl Fn(&mut Reader<'_>) -> Option<T>,
) -> Option<Vec<T>> {
    let mut r = Reader::new(data_tlv);
    let el = r.next().ok()??;
    if el.value != Value::ArrayStart {
        return None;
    }
    let mut entries = Vec::new();
    loop {
        let el = r.next().ok()??;
        match el.value {
            Value::ContainerEnd => break,
            Value::StructStart => entries.push(body(&mut r)?),
            _ => return None,
        }
    }
    Some(entries)
}

/// Decodes the `ListIndex = null` append form: `data_tlv` is one bare
/// struct (not wrapped in an array). Same `body` contract as
/// [`decode_struct_list`].
pub fn decode_single_struct<T>(
    data_tlv: &[u8],
    body: impl Fn(&mut Reader<'_>) -> Option<T>,
) -> Option<T> {
    let mut r = Reader::new(data_tlv);
    let el = r.next().ok()??;
    if el.value != Value::StructStart {
        return None;
    }
    body(&mut r)
}

/// Reads `Context(field)` (an unsigned integer) off the **top level** of a
/// command-fields struct — `{0: GroupID}`, `{0: IdentifyTime}`, `{0:
/// GroupKeySetID}` all share this shape. Nested containers are skipped
/// wholesale (a same-numbered tag inside one is not the field). A repeated
/// tag: last one wins. Malformed TLV or a missing field is `None`; callers
/// map that to `STATUS_INVALID_COMMAND`.
pub fn decode_struct_uint_field(fields_tlv: &[u8], field: u8) -> Option<u64> {
    let mut r = Reader::new(fields_tlv);
    match r.next() {
        Ok(Some(el)) if el.value == Value::StructStart => {}
        _ => return None,
    }
    let mut found = None;
    loop {
        match r.next() {
            Ok(Some(el)) => match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(t), Value::Uint(v)) if t == field => found = Some(v),
                (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                    mat_controller::tlv::skip_container(&mut r).ok()?;
                }
                _ => {}
            },
            _ => return None,
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_are_single_anonymous_elements() {
        // Control tag 0x04 = uint8, 0x0C = utf8 string 1-byte length,
        // 0x08/0x09 = false/true, 0x14 = null (spec §A.7.1 / §A.8).
        assert_eq!(uint(7), vec![0x04, 0x07]);
        assert_eq!(str("ab"), vec![0x0C, 0x02, b'a', b'b']);
        assert_eq!(bool(false), vec![0x08]);
        assert_eq!(bool(true), vec![0x09]);
        assert_eq!(null(), vec![0x14]);
    }

    #[test]
    fn struct_uint_field_reads_top_level_only_and_skips_nested() {
        // {0: 7, 1: {0: 99}} — the nested Context(0) must not win.
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 7);
        w.start_struct(Tag::Context(1));
        w.put_uint(Tag::Context(0), 99);
        w.end_container();
        w.end_container();
        assert_eq!(decode_struct_uint_field(&w.finish(), 0), Some(7));
        assert_eq!(decode_struct_uint_field(&[0x15, 0x18], 0), None); // {} — missing
        assert_eq!(decode_struct_uint_field(&[0x04, 0x01], 0), None); // not a struct
    }

    #[test]
    fn struct_list_and_single_struct_share_one_body_reader() {
        let body = |r: &mut Reader<'_>| -> Option<u64> {
            let mut v = None;
            loop {
                let el = r.next().ok()??;
                match (el.tag, el.value) {
                    (_, Value::ContainerEnd) => break,
                    (Tag::Context(1), Value::Uint(x)) => v = Some(x),
                    _ => {}
                }
            }
            v
        };
        let mut w = Writer::new();
        w.start_array(Tag::Anonymous);
        for x in [1u64, 2] {
            w.start_struct(Tag::Anonymous);
            w.put_uint(Tag::Context(1), x);
            w.end_container();
        }
        w.end_container();
        assert_eq!(decode_struct_list(&w.finish(), body), Some(vec![1, 2]));

        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(1), 5);
        w.end_container();
        let single = w.finish();
        assert_eq!(decode_single_struct(&single, body), Some(5));
        assert_eq!(decode_struct_list(&single, body), None); // bare struct is not a list
    }
}
