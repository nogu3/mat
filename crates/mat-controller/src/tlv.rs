//! Matter TLV codec (Matter Core Spec 1.4, Appendix A).

/// TLV tag (Matter spec Appendix A.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tag {
    Anonymous,
    Context(u8),
    CommonProfile16(u16),
    CommonProfile32(u32),
    ImplicitProfile16(u16),
    ImplicitProfile32(u32),
    FullyQualified48 { vendor: u16, profile: u16, tag: u16 },
    FullyQualified64 { vendor: u16, profile: u16, tag: u32 },
}

/// Streaming TLV encoder. Panics on unbalanced containers at `finish()`
/// (programmer error, not wire data).
pub struct Writer {
    buf: Vec<u8>,
    depth: usize,
}

impl Writer {
    pub fn new() -> Self {
        Self {
            buf: Vec::new(),
            depth: 0,
        }
    }

    fn control_and_tag(&mut self, type_bits: u8, tag: Tag) {
        match tag {
            Tag::Anonymous => self.buf.push(type_bits),
            Tag::Context(t) => {
                self.buf.push(0x20 | type_bits);
                self.buf.push(t);
            }
            Tag::CommonProfile16(t) => {
                self.buf.push(0x40 | type_bits);
                self.buf.extend_from_slice(&t.to_le_bytes());
            }
            Tag::CommonProfile32(t) => {
                self.buf.push(0x60 | type_bits);
                self.buf.extend_from_slice(&t.to_le_bytes());
            }
            Tag::ImplicitProfile16(t) => {
                self.buf.push(0x80 | type_bits);
                self.buf.extend_from_slice(&t.to_le_bytes());
            }
            Tag::ImplicitProfile32(t) => {
                self.buf.push(0xA0 | type_bits);
                self.buf.extend_from_slice(&t.to_le_bytes());
            }
            Tag::FullyQualified48 {
                vendor,
                profile,
                tag,
            } => {
                self.buf.push(0xC0 | type_bits);
                self.buf.extend_from_slice(&vendor.to_le_bytes());
                self.buf.extend_from_slice(&profile.to_le_bytes());
                self.buf.extend_from_slice(&tag.to_le_bytes());
            }
            Tag::FullyQualified64 {
                vendor,
                profile,
                tag,
            } => {
                self.buf.push(0xE0 | type_bits);
                self.buf.extend_from_slice(&vendor.to_le_bytes());
                self.buf.extend_from_slice(&profile.to_le_bytes());
                self.buf.extend_from_slice(&tag.to_le_bytes());
            }
        }
    }

    pub fn put_uint(&mut self, tag: Tag, v: u64) {
        if v <= u64::from(u8::MAX) {
            self.control_and_tag(0x04, tag);
            self.buf.push(v as u8);
        } else if v <= u64::from(u16::MAX) {
            self.control_and_tag(0x05, tag);
            self.buf.extend_from_slice(&(v as u16).to_le_bytes());
        } else if v <= u64::from(u32::MAX) {
            self.control_and_tag(0x06, tag);
            self.buf.extend_from_slice(&(v as u32).to_le_bytes());
        } else {
            self.control_and_tag(0x07, tag);
            self.buf.extend_from_slice(&v.to_le_bytes());
        }
    }

    pub fn put_int(&mut self, tag: Tag, v: i64) {
        if let Ok(v) = i8::try_from(v) {
            self.control_and_tag(0x00, tag);
            self.buf.extend_from_slice(&v.to_le_bytes());
        } else if let Ok(v) = i16::try_from(v) {
            self.control_and_tag(0x01, tag);
            self.buf.extend_from_slice(&v.to_le_bytes());
        } else if let Ok(v) = i32::try_from(v) {
            self.control_and_tag(0x02, tag);
            self.buf.extend_from_slice(&v.to_le_bytes());
        } else {
            self.control_and_tag(0x03, tag);
            self.buf.extend_from_slice(&v.to_le_bytes());
        }
    }

    pub fn put_bool(&mut self, tag: Tag, v: bool) {
        self.control_and_tag(if v { 0x09 } else { 0x08 }, tag);
    }

    pub fn put_f32(&mut self, tag: Tag, v: f32) {
        self.control_and_tag(0x0A, tag);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    pub fn put_f64(&mut self, tag: Tag, v: f64) {
        self.control_and_tag(0x0B, tag);
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    fn put_len_prefixed(&mut self, base_type: u8, tag: Tag, data: &[u8]) {
        let len = data.len();
        if let Ok(len) = u8::try_from(len) {
            self.control_and_tag(base_type, tag);
            self.buf.push(len);
        } else if let Ok(len) = u16::try_from(len) {
            self.control_and_tag(base_type + 1, tag);
            self.buf.extend_from_slice(&len.to_le_bytes());
        } else if let Ok(len) = u32::try_from(len) {
            self.control_and_tag(base_type + 2, tag);
            self.buf.extend_from_slice(&len.to_le_bytes());
        } else {
            self.control_and_tag(base_type + 3, tag);
            self.buf.extend_from_slice(&(len as u64).to_le_bytes());
        }
        self.buf.extend_from_slice(data);
    }

    pub fn put_str(&mut self, tag: Tag, v: &str) {
        self.put_len_prefixed(0x0C, tag, v.as_bytes());
    }

    pub fn put_bytes(&mut self, tag: Tag, v: &[u8]) {
        self.put_len_prefixed(0x10, tag, v);
    }

    pub fn put_null(&mut self, tag: Tag) {
        self.control_and_tag(0x14, tag);
    }

    pub fn start_struct(&mut self, tag: Tag) {
        self.control_and_tag(0x15, tag);
        self.depth += 1;
    }

    pub fn start_array(&mut self, tag: Tag) {
        self.control_and_tag(0x16, tag);
        self.depth += 1;
    }

    pub fn start_list(&mut self, tag: Tag) {
        self.control_and_tag(0x17, tag);
        self.depth += 1;
    }

    pub fn end_container(&mut self) {
        assert!(self.depth > 0, "end_container without open container");
        self.buf.push(0x18);
        self.depth -= 1;
    }

    pub fn finish(self) -> Vec<u8> {
        assert_eq!(self.depth, 0, "finish with unbalanced containers");
        self.buf
    }
}

impl Default for Writer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
        let mut w = Writer::new();
        f(&mut w);
        w.finish()
    }

    #[test]
    fn writes_uints_minimal_width() {
        assert_eq!(one(|w| w.put_uint(Tag::Anonymous, 42)), vec![0x04, 0x2A]);
        assert_eq!(
            one(|w| w.put_uint(Tag::Anonymous, 420)),
            vec![0x05, 0xA4, 0x01]
        );
        assert_eq!(
            one(|w| w.put_uint(Tag::Anonymous, 70000)),
            [vec![0x06], 70000u32.to_le_bytes().to_vec()].concat()
        );
        assert_eq!(
            one(|w| w.put_uint(Tag::Anonymous, u64::MAX)),
            [vec![0x07], u64::MAX.to_le_bytes().to_vec()].concat()
        );
    }

    #[test]
    fn writes_ints_minimal_width() {
        assert_eq!(one(|w| w.put_int(Tag::Anonymous, -17)), vec![0x00, 0xEF]);
        assert_eq!(
            one(|w| w.put_int(Tag::Anonymous, -40000)),
            [vec![0x02], (-40000i32).to_le_bytes().to_vec()].concat()
        );
        assert_eq!(one(|w| w.put_int(Tag::Anonymous, 127)), vec![0x00, 0x7F]);
    }

    #[test]
    fn writes_bool_null_floats() {
        assert_eq!(one(|w| w.put_bool(Tag::Anonymous, false)), vec![0x08]);
        assert_eq!(one(|w| w.put_bool(Tag::Anonymous, true)), vec![0x09]);
        assert_eq!(one(|w| w.put_null(Tag::Anonymous)), vec![0x14]);
        assert_eq!(
            one(|w| w.put_f32(Tag::Anonymous, 17.9)),
            [vec![0x0A], 17.9f32.to_le_bytes().to_vec()].concat()
        );
        assert_eq!(
            one(|w| w.put_f64(Tag::Anonymous, 17.9)),
            [vec![0x0B], 17.9f64.to_le_bytes().to_vec()].concat()
        );
    }

    #[test]
    fn writes_strings_and_bytes() {
        assert_eq!(
            one(|w| w.put_str(Tag::Anonymous, "Hello!")),
            vec![0x0C, 0x06, b'H', b'e', b'l', b'l', b'o', b'!']
        );
        assert_eq!(
            one(|w| w.put_bytes(Tag::Anonymous, &[0, 1, 2, 3, 4])),
            vec![0x10, 0x05, 0x00, 0x01, 0x02, 0x03, 0x04]
        );
        // 256 バイトは 2 バイト長になる
        let long = vec![0xAB; 256];
        let enc = one(|w| w.put_bytes(Tag::Anonymous, &long));
        assert_eq!(&enc[..3], &[0x11, 0x00, 0x01]);
        assert_eq!(enc.len(), 3 + 256);
    }

    #[test]
    fn writes_tag_forms() {
        assert_eq!(
            one(|w| w.put_uint(Tag::Context(1), 42)),
            vec![0x24, 0x01, 0x2A]
        );
        assert_eq!(
            one(|w| w.put_uint(Tag::CommonProfile16(0x0100), 42)),
            vec![0x44, 0x00, 0x01, 0x2A]
        );
        assert_eq!(
            one(|w| w.put_uint(Tag::ImplicitProfile16(0x0200), 42)),
            vec![0x84, 0x00, 0x02, 0x2A]
        );
        assert_eq!(
            one(|w| w.put_uint(
                Tag::FullyQualified48 {
                    vendor: 0xFFF1,
                    profile: 0xDEED,
                    tag: 1
                },
                42
            )),
            vec![0xC4, 0xF1, 0xFF, 0xED, 0xDE, 0x01, 0x00, 0x2A]
        );
    }

    #[test]
    fn writes_containers() {
        assert_eq!(
            one(|w| {
                w.start_struct(Tag::Anonymous);
                w.end_container();
            }),
            vec![0x15, 0x18]
        );
        assert_eq!(
            one(|w| {
                w.start_array(Tag::Anonymous);
                for v in 0..3 {
                    w.put_uint(Tag::Anonymous, v);
                }
                w.end_container();
            }),
            vec![0x16, 0x04, 0x00, 0x04, 0x01, 0x04, 0x02, 0x18]
        );
        assert_eq!(
            one(|w| {
                w.start_struct(Tag::Anonymous);
                w.put_uint(Tag::Context(0), 42);
                w.end_container();
            }),
            vec![0x15, 0x24, 0x00, 0x2A, 0x18]
        );
    }

    #[test]
    #[should_panic]
    fn finish_panics_on_unbalanced_container() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.finish();
    }
}
