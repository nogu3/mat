//! Standalone scalar TLV values — the `ClusterHandler::read` contract is
//! "one `Tag::Anonymous`-tagged element", and every cluster used to carry
//! its own three-line `Writer::new(); put_*; finish()` copy of this.

use mat_controller::tlv::{Tag, Writer};

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
}
