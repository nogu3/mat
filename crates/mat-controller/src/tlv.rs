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

    /// Deep-copies one complete, well-formed TLV element (and, if a
    /// container, its full subtree) from `element`, re-tagging only the
    /// top-level element as `tag`. Used to splice a caller-provided,
    /// already-encoded value (e.g. an IM `Data` element) into a larger
    /// message without the caller needing to know the target tag up front.
    ///
    /// Panics if `element` is not exactly one well-formed TLV element — a
    /// caller/programmer error (the element is always something `mat` itself
    /// encoded earlier), not device input to validate defensively.
    pub fn put_raw_element(&mut self, tag: Tag, element: &[u8]) {
        let mut r = Reader::new(element);
        let el = r
            .next()
            .expect("element must be valid tlv")
            .expect("element must not be empty");
        copy_value(self, &mut r, tag, el.value).expect("element must be one well-formed TLV value");
    }
}

/// Deep-copies one TLV element (and, if a container, its full subtree) from
/// `r` into `w`, re-tagging only the top-level element as `tag`. Shared by
/// `Writer::put_raw_element` (splicing a standalone pre-encoded element) and
/// callers that need to copy a value already positioned mid-stream (e.g.
/// `im::decode_command_data_ib`'s CommandFields echo).
pub(crate) fn copy_value(
    w: &mut Writer,
    r: &mut Reader,
    tag: Tag,
    value: Value,
) -> Result<(), TlvError> {
    match value {
        Value::Int(v) => w.put_int(tag, v),
        Value::Uint(v) => w.put_uint(tag, v),
        Value::Bool(v) => w.put_bool(tag, v),
        Value::F32(v) => w.put_f32(tag, v),
        Value::F64(v) => w.put_f64(tag, v),
        Value::Utf8(v) => w.put_str(tag, v),
        Value::Bytes(v) => w.put_bytes(tag, v),
        Value::Null => w.put_null(tag),
        Value::StructStart => {
            w.start_struct(tag);
            return copy_container(w, r);
        }
        Value::ArrayStart => {
            w.start_array(tag);
            return copy_container(w, r);
        }
        Value::ListStart => {
            w.start_list(tag);
            return copy_container(w, r);
        }
        Value::ContainerEnd => {}
    }
    Ok(())
}

fn copy_container(w: &mut Writer, r: &mut Reader) -> Result<(), TlvError> {
    loop {
        let el = r.next()?.ok_or(TlvError::Truncated)?;
        if el.value == Value::ContainerEnd {
            w.end_container();
            return Ok(());
        }
        copy_value(w, r, el.tag, el.value)?;
    }
}

impl Default for Writer {
    fn default() -> Self {
        Self::new()
    }
}

/// TLV decode error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TlvError {
    Truncated,
    InvalidType(u8),
    InvalidUtf8,
    LengthOverflow,
}

impl std::fmt::Display for TlvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TlvError::Truncated => write!(f, "tlv truncated"),
            TlvError::InvalidType(t) => write!(f, "invalid tlv element type 0x{t:02X}"),
            TlvError::InvalidUtf8 => write!(f, "invalid utf-8 in tlv string"),
            TlvError::LengthOverflow => write!(f, "tlv length exceeds buffer"),
        }
    }
}

impl std::error::Error for TlvError {}

/// Decoded TLV value. Strings/bytes borrow from the input buffer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Value<'a> {
    Int(i64),
    Uint(u64),
    Bool(bool),
    F32(f32),
    F64(f64),
    Utf8(&'a str),
    Bytes(&'a [u8]),
    Null,
    StructStart,
    ArrayStart,
    ListStart,
    ContainerEnd,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Element<'a> {
    pub tag: Tag,
    pub value: Value<'a>,
}

/// Streaming TLV decoder returning a flat event sequence.
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, pos: 0 }
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8], TlvError> {
        let end = self.pos.checked_add(n).ok_or(TlvError::LengthOverflow)?;
        if end > self.buf.len() {
            return Err(TlvError::Truncated);
        }
        let s = &self.buf[self.pos..end];
        self.pos = end;
        Ok(s)
    }

    fn take_u16(&mut self) -> Result<u16, TlvError> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn take_u32(&mut self) -> Result<u32, TlvError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }

    fn take_u64(&mut self) -> Result<u64, TlvError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }

    fn read_tag(&mut self, control: u8) -> Result<Tag, TlvError> {
        match control & 0xE0 {
            0x00 => Ok(Tag::Anonymous),
            0x20 => Ok(Tag::Context(self.take(1)?[0])),
            0x40 => Ok(Tag::CommonProfile16(self.take_u16()?)),
            0x60 => Ok(Tag::CommonProfile32(self.take_u32()?)),
            0x80 => Ok(Tag::ImplicitProfile16(self.take_u16()?)),
            0xA0 => Ok(Tag::ImplicitProfile32(self.take_u32()?)),
            0xC0 => {
                let vendor = self.take_u16()?;
                let profile = self.take_u16()?;
                let tag = self.take_u16()?;
                Ok(Tag::FullyQualified48 {
                    vendor,
                    profile,
                    tag,
                })
            }
            0xE0 => {
                let vendor = self.take_u16()?;
                let profile = self.take_u16()?;
                let tag = self.take_u32()?;
                Ok(Tag::FullyQualified64 {
                    vendor,
                    profile,
                    tag,
                })
            }
            _ => unreachable!("3-bit mask covers all cases"),
        }
    }

    fn read_len(&mut self, width_selector: u8) -> Result<usize, TlvError> {
        let len = match width_selector {
            0 => u64::from(self.take(1)?[0]),
            1 => u64::from(self.take_u16()?),
            2 => u64::from(self.take_u32()?),
            _ => self.take_u64()?,
        };
        usize::try_from(len).map_err(|_| TlvError::LengthOverflow)
    }

    /// Returns the next element, `Ok(None)` at end of input.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Result<Option<Element<'a>>, TlvError> {
        if self.pos >= self.buf.len() {
            return Ok(None);
        }
        let control = self.take(1)?[0];
        let type_bits = control & 0x1F;
        let tag = self.read_tag(control)?;
        let value = match type_bits {
            0x00 => Value::Int(i64::from(self.take(1)?[0] as i8)),
            0x01 => Value::Int(i64::from(i16::from_le_bytes(
                self.take(2)?.try_into().unwrap(),
            ))),
            0x02 => Value::Int(i64::from(i32::from_le_bytes(
                self.take(4)?.try_into().unwrap(),
            ))),
            0x03 => Value::Int(i64::from_le_bytes(self.take(8)?.try_into().unwrap())),
            0x04 => Value::Uint(u64::from(self.take(1)?[0])),
            0x05 => Value::Uint(u64::from(self.take_u16()?)),
            0x06 => Value::Uint(u64::from(self.take_u32()?)),
            0x07 => Value::Uint(self.take_u64()?),
            0x08 => Value::Bool(false),
            0x09 => Value::Bool(true),
            0x0A => Value::F32(f32::from_le_bytes(self.take(4)?.try_into().unwrap())),
            0x0B => Value::F64(f64::from_le_bytes(self.take(8)?.try_into().unwrap())),
            0x0C..=0x0F => {
                let len = self.read_len(type_bits - 0x0C)?;
                let bytes = self.take(len)?;
                Value::Utf8(std::str::from_utf8(bytes).map_err(|_| TlvError::InvalidUtf8)?)
            }
            0x10..=0x13 => {
                let len = self.read_len(type_bits - 0x10)?;
                Value::Bytes(self.take(len)?)
            }
            0x14 => Value::Null,
            0x15 => Value::StructStart,
            0x16 => Value::ArrayStart,
            0x17 => Value::ListStart,
            0x18 => Value::ContainerEnd,
            t => return Err(TlvError::InvalidType(t)),
        };
        Ok(Some(Element { tag, value }))
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

    fn read_all(buf: &[u8]) -> Vec<Element<'_>> {
        let mut r = Reader::new(buf);
        let mut out = Vec::new();
        while let Some(el) = r.next().expect("valid tlv") {
            out.push(el);
        }
        out
    }

    #[test]
    fn reads_scalars() {
        assert_eq!(
            read_all(&[0x04, 0x2A]),
            vec![Element {
                tag: Tag::Anonymous,
                value: Value::Uint(42)
            }]
        );
        assert_eq!(
            read_all(&[0x00, 0xEF]),
            vec![Element {
                tag: Tag::Anonymous,
                value: Value::Int(-17)
            }]
        );
        assert_eq!(
            read_all(&[0x08]),
            vec![Element {
                tag: Tag::Anonymous,
                value: Value::Bool(false)
            }]
        );
        assert_eq!(
            read_all(&[0x14]),
            vec![Element {
                tag: Tag::Anonymous,
                value: Value::Null
            }]
        );
        assert_eq!(
            read_all(&[0x24, 0x01, 0x2A]),
            vec![Element {
                tag: Tag::Context(1),
                value: Value::Uint(42)
            }]
        );
    }

    #[test]
    fn reads_strings_bytes_containers() {
        assert_eq!(
            read_all(&[0x0C, 0x02, b'h', b'i']),
            vec![Element {
                tag: Tag::Anonymous,
                value: Value::Utf8("hi")
            }]
        );
        assert_eq!(
            read_all(&[0x15, 0x24, 0x00, 0x2A, 0x18]),
            vec![
                Element {
                    tag: Tag::Anonymous,
                    value: Value::StructStart
                },
                Element {
                    tag: Tag::Context(0),
                    value: Value::Uint(42)
                },
                Element {
                    tag: Tag::Anonymous,
                    value: Value::ContainerEnd
                },
            ]
        );
    }

    #[test]
    fn roundtrips_writer_output() {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 0xDEAD_BEEF);
        w.put_int(Tag::Context(1), -40000);
        w.put_str(Tag::Context(2), "Hello!");
        w.put_bytes(Tag::Context(3), &[1, 2, 3]);
        w.put_f64(Tag::Context(4), 17.9);
        w.start_array(Tag::Context(5));
        w.put_bool(Tag::Anonymous, true);
        w.end_container();
        w.put_uint(
            Tag::FullyQualified48 {
                vendor: 0xFFF1,
                profile: 0xDEED,
                tag: 7,
            },
            1,
        );
        w.end_container();
        let buf = w.finish();
        let els = read_all(&buf);
        assert_eq!(els[1].value, Value::Uint(0xDEAD_BEEF));
        assert_eq!(els[2].value, Value::Int(-40000));
        assert_eq!(els[3].value, Value::Utf8("Hello!"));
        assert_eq!(els[4].value, Value::Bytes(&[1, 2, 3]));
        assert_eq!(els[5].value, Value::F64(17.9));
        assert_eq!(els[6].value, Value::ArrayStart);
        assert_eq!(els[7].value, Value::Bool(true));
        assert_eq!(els[8].value, Value::ContainerEnd);
        assert_eq!(
            els[9].tag,
            Tag::FullyQualified48 {
                vendor: 0xFFF1,
                profile: 0xDEED,
                tag: 7
            }
        );
        assert_eq!(els.len(), 11);
    }

    #[test]
    fn rejects_malformed_input() {
        // 値が足りない
        assert_eq!(Reader::new(&[0x04]).next(), Err(TlvError::Truncated));
        // 長さプレフィクスより実データが短い
        assert_eq!(
            Reader::new(&[0x0C, 0x05, b'h', b'i']).next(),
            Err(TlvError::Truncated)
        );
        // 予約 element type (0x19-0x1F)
        assert_eq!(
            Reader::new(&[0x19]).next(),
            Err(TlvError::InvalidType(0x19))
        );
        // 不正 UTF-8
        assert_eq!(
            Reader::new(&[0x0C, 0x02, 0xFF, 0xFE]).next(),
            Err(TlvError::InvalidUtf8)
        );
        // tag バイトが足りない
        assert_eq!(Reader::new(&[0x24]).next(), Err(TlvError::Truncated));
    }
}
