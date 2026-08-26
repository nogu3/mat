//! Groups クラスタサーバ (spec §1.3, cluster 0x0004)。On/Off Light
//! デバイスタイプ（Device Library §4.1）の必須クラスタ — Identify と同じく
//! Apple Home の interview 対策で、グループ束縛の実配線（groupcast 受信は
//! net 層の別機構）ではなく fabric スコープの membership 帳簿を持つ。
//! FeatureMap GN=0（グループ名は保存しない — NameSupport も 0）。
//! 永続化は M3 送り: テーブルは in-memory で、再起動で消える。
use mat_controller::im;
use mat_controller::tlv::{Reader, Tag, Value, Writer};

use crate::core::datamodel::{ClusterHandler, InvokeCtx, InvokeReply, ReadCtx};
use crate::core::identify::IdentifyState;

/// Group table capacity (spec §1.3.4 leaves the size implementation-
/// defined). Shared across fabrics; `GetGroupMembership` reports the
/// remaining headroom as its `Capacity`.
const GROUP_TABLE_CAPACITY: usize = 16;

const RESP_ADD_GROUP: u32 = 0x00;
const RESP_VIEW_GROUP: u32 = 0x01;
const RESP_GET_GROUP_MEMBERSHIP: u32 = 0x02;
const RESP_REMOVE_GROUP: u32 = 0x03;

pub struct GroupsHandler {
    identify: IdentifyState,
    /// `(fabric_index, group_id)` memberships, insertion-ordered (spec
    /// §1.3.6: entries are fabric-scoped).
    members: Vec<(u8, u16)>,
}

impl GroupsHandler {
    pub fn new(identify: IdentifyState) -> Self {
        Self {
            identify,
            members: Vec::new(),
        }
    }

    fn contains(&self, fabric: u8, group_id: u16) -> bool {
        self.members.contains(&(fabric, group_id))
    }

    /// AddGroup 本体 (spec §1.3.7.1): 返す status は response struct 用。
    fn add(&mut self, fabric: u8, group_id: u16) -> u8 {
        if group_id == 0 {
            return im::STATUS_CONSTRAINT_ERROR;
        }
        if self.contains(fabric, group_id) {
            return im::STATUS_SUCCESS;
        }
        if self.members.len() >= GROUP_TABLE_CAPACITY {
            return im::STATUS_RESOURCE_EXHAUSTED;
        }
        self.members.push((fabric, group_id));
        im::STATUS_SUCCESS
    }

    fn status_response(response_command: u32, status: u8, group_id: u16) -> InvokeReply {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), u64::from(status));
        w.put_uint(Tag::Context(1), u64::from(group_id));
        if response_command == RESP_VIEW_GROUP {
            // ViewGroupResponse の GroupName (spec §1.3.7.9) — GN=0 なので
            // 常に空文字列だが、フィールド自体は mandatory。
            w.put_str(Tag::Context(2), "");
        }
        w.end_container();
        InvokeReply::Data {
            response_command,
            fields_tlv: w.finish(),
        }
    }
}

impl ClusterHandler for GroupsHandler {
    fn cluster_id(&self) -> u32 {
        im::CLUSTER_GROUPS
    }

    /// ClusterRevision (spec §7.13): Groups cluster spec revision 4
    /// (Matter 1.4).
    fn revision(&self) -> u16 {
        4
    }

    fn attributes(&self) -> Vec<u32> {
        vec![im::ATTR_GROUPS_NAME_SUPPORT]
    }

    fn read(&self, attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
        match attribute {
            im::ATTR_GROUPS_NAME_SUPPORT => {
                let mut w = Writer::new();
                w.put_uint(Tag::Anonymous, 0);
                Some(w.finish())
            }
            _ => None,
        }
    }

    fn invoke(&mut self, command: u32, fields_tlv: &[u8], ctx: &mut InvokeCtx) -> InvokeReply {
        let fabric = ctx.fabric_index;
        match command {
            im::CMD_ADD_GROUP => {
                let Some(group_id) = decode_group_id(fields_tlv) else {
                    return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
                };
                let status = self.add(fabric, group_id);
                Self::status_response(RESP_ADD_GROUP, status, group_id)
            }
            im::CMD_VIEW_GROUP => {
                let Some(group_id) = decode_group_id(fields_tlv) else {
                    return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
                };
                let status = if group_id == 0 {
                    im::STATUS_CONSTRAINT_ERROR
                } else if self.contains(fabric, group_id) {
                    im::STATUS_SUCCESS
                } else {
                    im::STATUS_NOT_FOUND
                };
                Self::status_response(RESP_VIEW_GROUP, status, group_id)
            }
            im::CMD_GET_GROUP_MEMBERSHIP => {
                let Some(requested) = decode_group_list(fields_tlv) else {
                    return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
                };
                let matching: Vec<u16> = self
                    .members
                    .iter()
                    .filter(|(f, g)| {
                        *f == fabric && (requested.is_empty() || requested.contains(g))
                    })
                    .map(|(_, g)| *g)
                    .collect();
                let mut w = Writer::new();
                w.start_struct(Tag::Anonymous);
                w.put_uint(
                    Tag::Context(0),
                    (GROUP_TABLE_CAPACITY - self.members.len()) as u64,
                );
                w.start_array(Tag::Context(1));
                for g in matching {
                    w.put_uint(Tag::Anonymous, u64::from(g));
                }
                w.end_container();
                w.end_container();
                InvokeReply::Data {
                    response_command: RESP_GET_GROUP_MEMBERSHIP,
                    fields_tlv: w.finish(),
                }
            }
            im::CMD_REMOVE_GROUP => {
                let Some(group_id) = decode_group_id(fields_tlv) else {
                    return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
                };
                let status = if group_id == 0 {
                    im::STATUS_CONSTRAINT_ERROR
                } else if self.contains(fabric, group_id) {
                    self.members.retain(|entry| *entry != (fabric, group_id));
                    im::STATUS_SUCCESS
                } else {
                    im::STATUS_NOT_FOUND
                };
                Self::status_response(RESP_REMOVE_GROUP, status, group_id)
            }
            im::CMD_REMOVE_ALL_GROUPS => {
                self.members.retain(|(f, _)| *f != fabric);
                InvokeReply::Status(im::STATUS_SUCCESS)
            }
            im::CMD_ADD_GROUP_IF_IDENTIFYING => {
                let Some(group_id) = decode_group_id(fields_tlv) else {
                    return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
                };
                if group_id == 0 {
                    return InvokeReply::Status(im::STATUS_CONSTRAINT_ERROR);
                }
                if !self.identify.is_identifying() {
                    // spec §1.3.7.6: identify 中でなければ「何もせず成功」。
                    return InvokeReply::Status(im::STATUS_SUCCESS);
                }
                InvokeReply::Status(self.add(fabric, group_id))
            }
            _ => InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND),
        }
    }

    fn accepted_commands(&self) -> Vec<u32> {
        vec![
            im::CMD_ADD_GROUP,
            im::CMD_VIEW_GROUP,
            im::CMD_GET_GROUP_MEMBERSHIP,
            im::CMD_REMOVE_GROUP,
            im::CMD_REMOVE_ALL_GROUPS,
            im::CMD_ADD_GROUP_IF_IDENTIFYING,
        ]
    }

    fn generated_commands(&self) -> Vec<u32> {
        vec![
            RESP_ADD_GROUP,
            RESP_VIEW_GROUP,
            RESP_GET_GROUP_MEMBERSHIP,
            RESP_REMOVE_GROUP,
        ]
    }
}

/// `{0: GroupID (uint16), ...}` — AddGroup/ViewGroup/RemoveGroup/
/// AddGroupIfIdentifying に共通の先頭フィールド。
fn decode_group_id(fields_tlv: &[u8]) -> Option<u16> {
    let mut r = Reader::new(fields_tlv);
    match r.next() {
        Ok(Some(el)) if el.value == Value::StructStart => {}
        _ => return None,
    }
    let mut group_id = None;
    let mut depth = 0u32;
    loop {
        match r.next() {
            Ok(Some(el)) => match (el.tag, el.value) {
                (_, Value::ContainerEnd) => {
                    if depth == 0 {
                        break;
                    }
                    depth -= 1;
                }
                (_, Value::StructStart | Value::ArrayStart | Value::ListStart) => depth += 1,
                (Tag::Context(0), Value::Uint(v)) if depth == 0 => group_id = u16::try_from(v).ok(),
                _ => {}
            },
            _ => return None,
        }
    }
    group_id
}

/// GetGroupMembership の `{0: GroupList (array of uint16)}` (spec §1.3.7.3)。
fn decode_group_list(fields_tlv: &[u8]) -> Option<Vec<u16>> {
    let mut r = Reader::new(fields_tlv);
    match r.next() {
        Ok(Some(el)) if el.value == Value::StructStart => {}
        _ => return None,
    }
    let mut groups = Vec::new();
    loop {
        match r.next() {
            Ok(Some(el)) => match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(0), Value::ArrayStart) => loop {
                    match r.next() {
                        Ok(Some(inner)) => match inner.value {
                            Value::ContainerEnd => break,
                            Value::Uint(v) => groups.push(u16::try_from(v).ok()?),
                            _ => return None,
                        },
                        _ => return None,
                    }
                },
                _ => {}
            },
            _ => return None,
        }
    }
    Some(groups)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::identify::IdentifyHandler;

    fn handler() -> GroupsHandler {
        let (_identify, state) = IdentifyHandler::new();
        GroupsHandler::new(state)
    }

    fn fabric_ctx(fabric_index: u8) -> InvokeCtx {
        InvokeCtx {
            fabric_index,
            ..InvokeCtx::default()
        }
    }

    fn group_fields(group_id: u16) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), u64::from(group_id));
        w.put_str(Tag::Context(1), "");
        w.end_container();
        w.finish()
    }

    /// Decodes a `{0: status, 1: group_id, ...}` response struct.
    fn decode_status_response(reply: &InvokeReply, expected_command: u32) -> (u8, u16) {
        let InvokeReply::Data {
            response_command,
            fields_tlv,
        } = reply
        else {
            panic!("expected Data reply, got {reply:?}");
        };
        assert_eq!(*response_command, expected_command);
        let mut r = Reader::new(fields_tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let (mut status, mut group_id) = (None, None);
        loop {
            let el = r.next().unwrap().expect("truncated response");
            match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(0), Value::Uint(v)) => status = Some(v as u8),
                (Tag::Context(1), Value::Uint(v)) => group_id = Some(v as u16),
                _ => {}
            }
        }
        (status.expect("status"), group_id.unwrap_or(0))
    }

    fn add_group(h: &mut GroupsHandler, fabric: u8, group_id: u16) -> u8 {
        let reply = h.invoke(
            im::CMD_ADD_GROUP,
            &group_fields(group_id),
            &mut fabric_ctx(fabric),
        );
        decode_status_response(&reply, RESP_ADD_GROUP).0
    }

    #[test]
    fn name_support_reads_zero() {
        let h = handler();
        assert_eq!(h.attributes(), vec![im::ATTR_GROUPS_NAME_SUPPORT]);
        let tlv = h
            .read(im::ATTR_GROUPS_NAME_SUPPORT, &ReadCtx::default())
            .expect("NameSupport");
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(0));
    }

    #[test]
    fn add_then_view_group_is_fabric_scoped() {
        let mut h = handler();
        assert_eq!(add_group(&mut h, 1, 0x0042), im::STATUS_SUCCESS);

        let reply = h.invoke(
            im::CMD_VIEW_GROUP,
            &group_fields(0x0042),
            &mut fabric_ctx(1),
        );
        let (status, group_id) = decode_status_response(&reply, RESP_VIEW_GROUP);
        assert_eq!((status, group_id), (im::STATUS_SUCCESS, 0x0042));

        // 別 fabric からは見えない (spec §1.3.6: fabric-scoped)。
        let reply = h.invoke(
            im::CMD_VIEW_GROUP,
            &group_fields(0x0042),
            &mut fabric_ctx(2),
        );
        assert_eq!(
            decode_status_response(&reply, RESP_VIEW_GROUP).0,
            im::STATUS_NOT_FOUND
        );
    }

    #[test]
    fn add_group_rejects_id_zero_and_reports_exhaustion() {
        let mut h = handler();
        assert_eq!(add_group(&mut h, 1, 0), im::STATUS_CONSTRAINT_ERROR);

        for i in 0..GROUP_TABLE_CAPACITY {
            assert_eq!(add_group(&mut h, 1, (i + 1) as u16), im::STATUS_SUCCESS);
        }
        assert_eq!(add_group(&mut h, 1, 0x1000), im::STATUS_RESOURCE_EXHAUSTED);

        // 既存グループへの再 Add は冪等に SUCCESS（満杯でも）。
        assert_eq!(add_group(&mut h, 1, 1), im::STATUS_SUCCESS);
    }

    #[test]
    fn get_group_membership_lists_the_fabric_memberships() {
        let mut h = handler();
        add_group(&mut h, 1, 0x0010);
        add_group(&mut h, 1, 0x0020);
        add_group(&mut h, 2, 0x0030);

        // 空リスト = 「この fabric の全 membership」(spec §1.3.7.3)。
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.start_array(Tag::Context(0));
        w.end_container();
        w.end_container();
        let reply = h.invoke(
            im::CMD_GET_GROUP_MEMBERSHIP,
            &w.finish(),
            &mut fabric_ctx(1),
        );
        let InvokeReply::Data {
            response_command,
            fields_tlv,
        } = &reply
        else {
            panic!("expected Data reply, got {reply:?}");
        };
        assert_eq!(*response_command, RESP_GET_GROUP_MEMBERSHIP);
        let mut r = Reader::new(fields_tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let mut groups = Vec::new();
        let mut capacity = None;
        loop {
            let el = r.next().unwrap().expect("truncated response");
            match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(0), Value::Uint(v)) => capacity = Some(v),
                (Tag::Context(1), Value::ArrayStart) => loop {
                    match r.next().unwrap().expect("truncated group list").value {
                        Value::ContainerEnd => break,
                        Value::Uint(v) => groups.push(v as u16),
                        other => panic!("expected uint group id, got {other:?}"),
                    }
                },
                _ => {}
            }
        }
        assert_eq!(groups, vec![0x0010, 0x0020]);
        assert_eq!(capacity, Some((GROUP_TABLE_CAPACITY - 3) as u64));
    }

    #[test]
    fn remove_group_and_remove_all_groups() {
        let mut h = handler();
        add_group(&mut h, 1, 0x0042);
        add_group(&mut h, 2, 0x0042);

        let reply = h.invoke(
            im::CMD_REMOVE_GROUP,
            &group_fields(0x0042),
            &mut fabric_ctx(1),
        );
        assert_eq!(
            decode_status_response(&reply, RESP_REMOVE_GROUP),
            (im::STATUS_SUCCESS, 0x0042)
        );
        // 二重削除は NOT_FOUND。
        let reply = h.invoke(
            im::CMD_REMOVE_GROUP,
            &group_fields(0x0042),
            &mut fabric_ctx(1),
        );
        assert_eq!(
            decode_status_response(&reply, RESP_REMOVE_GROUP).0,
            im::STATUS_NOT_FOUND
        );

        // RemoveAllGroups は自 fabric のみ、応答はステータスのみ (spec §1.3.7.5)。
        assert_eq!(
            h.invoke(im::CMD_REMOVE_ALL_GROUPS, &[], &mut fabric_ctx(2)),
            InvokeReply::Status(im::STATUS_SUCCESS)
        );
        let reply = h.invoke(
            im::CMD_VIEW_GROUP,
            &group_fields(0x0042),
            &mut fabric_ctx(2),
        );
        assert_eq!(
            decode_status_response(&reply, RESP_VIEW_GROUP).0,
            im::STATUS_NOT_FOUND
        );
    }

    #[test]
    fn add_group_if_identifying_requires_identify_in_progress() {
        let (mut identify, state) = IdentifyHandler::new();
        let mut h = GroupsHandler::new(state);

        // identify していない間は成功扱いで何も追加しない (spec §1.3.7.6)。
        assert_eq!(
            h.invoke(
                im::CMD_ADD_GROUP_IF_IDENTIFYING,
                &group_fields(0x0042),
                &mut fabric_ctx(1)
            ),
            InvokeReply::Status(im::STATUS_SUCCESS)
        );
        let reply = h.invoke(
            im::CMD_VIEW_GROUP,
            &group_fields(0x0042),
            &mut fabric_ctx(1),
        );
        assert_eq!(
            decode_status_response(&reply, RESP_VIEW_GROUP).0,
            im::STATUS_NOT_FOUND
        );

        // identify 中は追加される。
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 30);
        w.end_container();
        identify.invoke(im::CMD_IDENTIFY, &w.finish(), &mut InvokeCtx::default());
        assert_eq!(
            h.invoke(
                im::CMD_ADD_GROUP_IF_IDENTIFYING,
                &group_fields(0x0042),
                &mut fabric_ctx(1)
            ),
            InvokeReply::Status(im::STATUS_SUCCESS)
        );
        let reply = h.invoke(
            im::CMD_VIEW_GROUP,
            &group_fields(0x0042),
            &mut fabric_ctx(1),
        );
        assert_eq!(
            decode_status_response(&reply, RESP_VIEW_GROUP).0,
            im::STATUS_SUCCESS
        );
    }

    #[test]
    fn malformed_and_unknown_commands_are_rejected_and_lists_declared() {
        let mut h = handler();
        assert_eq!(
            h.invoke(im::CMD_ADD_GROUP, &[], &mut fabric_ctx(1)),
            InvokeReply::Status(im::STATUS_INVALID_COMMAND)
        );
        assert_eq!(
            h.invoke(0x7F, &[], &mut fabric_ctx(1)),
            InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
        );
        assert_eq!(
            h.accepted_commands(),
            vec![
                im::CMD_ADD_GROUP,
                im::CMD_VIEW_GROUP,
                im::CMD_GET_GROUP_MEMBERSHIP,
                im::CMD_REMOVE_GROUP,
                im::CMD_REMOVE_ALL_GROUPS,
                im::CMD_ADD_GROUP_IF_IDENTIFYING
            ]
        );
        assert_eq!(
            h.generated_commands(),
            vec![
                RESP_ADD_GROUP,
                RESP_VIEW_GROUP,
                RESP_GET_GROUP_MEMBERSHIP,
                RESP_REMOVE_GROUP
            ]
        );
    }
}
