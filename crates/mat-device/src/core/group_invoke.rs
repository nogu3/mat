//! group 宛 InvokeRequest（spec §8.2.5 / §8.9.4）のデコーダ。
//! `mat_controller::im::decode_invoke_request` は CommandPath に Endpoint を
//! 要求する（unicast 形）が、groupcast の CommandPath は endpoint を持たない
//! （`im::encode_group_invoke_request` が作る形）ので、同じ構造を endpoint
//! 任意で読む独自版。複数 CommandDataIB を全部返す（controller は 1 件）。
use mat_controller::im::ImError;
use mat_controller::tlv::{copy_value, skip_container, Reader, Tag, Value, Writer};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupInvokeIn {
    pub cluster: u32,
    pub command: u32,
    /// CommandFields を `Tag::Anonymous` に再タグした raw TLV（空 = フィールド無し）。
    pub fields_tlv: Vec<u8>,
}

pub fn decode_group_invoke_request(payload: &[u8]) -> Result<Vec<GroupInvokeIn>, ImError> {
    let mut r = Reader::new(payload);
    let el = r
        .next()?
        .ok_or(ImError::Malformed("empty invoke request"))?;
    if el.value != Value::StructStart {
        return Err(ImError::Malformed("invoke request is not a struct"));
    }
    let mut out = Vec::new();
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated invoke request"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(2), Value::ArrayStart) => loop {
                let e = r
                    .next()?
                    .ok_or(ImError::Malformed("truncated invoke requests"))?;
                match e.value {
                    Value::ContainerEnd => break,
                    Value::StructStart => out.push(decode_command_data_ib(&mut r)?),
                    Value::ArrayStart | Value::ListStart => skip_container(&mut r)?,
                    _ => {}
                }
            },
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                skip_container(&mut r)?
            }
            _ => {}
        }
    }
    Ok(out)
}

fn decode_command_data_ib(r: &mut Reader) -> Result<GroupInvokeIn, ImError> {
    let (mut cluster, mut command, mut fields_tlv) = (None, None, Vec::new());
    loop {
        let el = r
            .next()?
            .ok_or(ImError::Malformed("truncated command data ib"))?;
        match (el.tag, el.value) {
            (_, Value::ContainerEnd) => break,
            (Tag::Context(0), Value::ListStart) => loop {
                let e = r
                    .next()?
                    .ok_or(ImError::Malformed("truncated command path"))?;
                match (e.tag, e.value) {
                    (_, Value::ContainerEnd) => break,
                    // Context(0) = Endpoint: group 形には無く、あっても無視
                    (Tag::Context(1), Value::Uint(v)) => {
                        cluster = Some(
                            u32::try_from(v)
                                .map_err(|_| ImError::Malformed("cluster out of range"))?,
                        )
                    }
                    (Tag::Context(2), Value::Uint(v)) => {
                        command = Some(
                            u32::try_from(v)
                                .map_err(|_| ImError::Malformed("command out of range"))?,
                        )
                    }
                    (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => {
                        skip_container(r)?
                    }
                    _ => {}
                }
            },
            (Tag::Context(1), Value::StructStart) => {
                let mut w = Writer::new();
                copy_value(&mut w, r, Tag::Anonymous, Value::StructStart)?;
                fields_tlv = w.finish();
            }
            (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => skip_container(r)?,
            _ => {}
        }
    }
    Ok(GroupInvokeIn {
        cluster: cluster.ok_or(ImError::Malformed("command path without cluster"))?,
        command: command.ok_or(ImError::Malformed("command path without command"))?,
        fields_tlv,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mat_controller::im;
    use mat_controller::tlv::{Tag, Writer};

    #[test]
    fn decodes_the_controllers_group_invoke_with_and_without_fields() {
        let payload = im::encode_group_invoke_request(im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON, None);
        assert_eq!(
            decode_group_invoke_request(&payload).unwrap(),
            vec![GroupInvokeIn {
                cluster: im::CLUSTER_ON_OFF,
                command: im::CMD_ON_OFF_ON,
                fields_tlv: Vec::new()
            }]
        );
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 3);
        w.end_container();
        let fields = w.finish();
        let payload = im::encode_group_invoke_request(0x0008, 0x04, Some(&fields));
        let out = decode_group_invoke_request(&payload).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].cluster, out[0].command), (0x0008, 0x04));
        assert_eq!(out[0].fields_tlv, fields);
    }

    /// endpoint 付き CommandPath（unicast 形）も受理し endpoint は無視する。
    #[test]
    fn tolerates_an_endpoint_in_the_command_path() {
        let payload = im::encode_invoke_request(2, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_TOGGLE, None);
        let out = decode_group_invoke_request(&payload).unwrap();
        assert_eq!(
            (out[0].cluster, out[0].command),
            (im::CLUSTER_ON_OFF, im::CMD_ON_OFF_TOGGLE)
        );
    }

    #[test]
    fn malformed_or_pathless_requests_are_errors() {
        assert!(decode_group_invoke_request(&[0xFF, 0x00]).is_err());
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(2));
        w.start_struct(Tag::Anonymous); // CommandDataIB without a CommandPath
        w.end_container();
        w.end_container();
        w.end_container();
        assert!(decode_group_invoke_request(&w.finish()).is_err());
    }
}
