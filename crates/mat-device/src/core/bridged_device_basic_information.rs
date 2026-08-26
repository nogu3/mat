//! Bridged Device Basic Information クラスタサーバ (spec §9.13, cluster
//! 0x0039)。BasicInformation (spec §11.1) の bridged 版 —
//! Aggregator 配下の各 bridged endpoint（matv の各デバイス）が持つ、その
//! 名前と到達性。NodeLabel/UniqueID は BasicInformation と同じ属性 id
//! （`ATTR_BI_NODE_LABEL`/`ATTR_BI_UNIQUE_ID`、Task 1 で追加済み）を再利用
//! する — spec 上も同じ意味の属性が別クラスタに存在するだけで、値の型・
//! 意味は共通。
//!
//! NodeLabel は設定ファイルの device name が正本の read-only 値
//! （write はデフォルトのまま拒否 = `STATUS_UNSUPPORTED_WRITE`）—
//! コントローラ側 write を許すと mat 再起動で設定ファイルの値に巻き戻り、
//! 「コントローラで付けた名前が消える」混乱を招く。コントローラは自分の
//! ローカル名を別途持てるので、これは実害にならない。
//!
//! Reachable (spec §9.13.4) は M3 では常に true 固定 — mando 側の不達
//! 判定（本当に reachable かどうかの追跡）は M4 スコープ。
use mat_controller::im;
use mat_controller::tlv::{Tag, Writer};

use crate::core::datamodel::{ClusterHandler, InvokeCtx, InvokeReply, ReadCtx};

/// Encodes a standalone, `Tag::Anonymous`-tagged TLV string element (the
/// `ClusterHandler::read` contract) — mirrors `datamodel::str_value`, which
/// is private to that module.
fn str_value(v: &str) -> Vec<u8> {
    let mut w = Writer::new();
    w.put_str(Tag::Anonymous, v);
    w.finish()
}

/// Encodes a standalone, `Tag::Anonymous`-tagged TLV bool element.
fn bool_value(v: bool) -> Vec<u8> {
    let mut w = Writer::new();
    w.put_bool(Tag::Anonymous, v);
    w.finish()
}

pub struct BridgedDeviceBasicInformationHandler {
    node_label: String,
    unique_id: String,
}

impl BridgedDeviceBasicInformationHandler {
    pub fn new(node_label: &str, unique_id: &str) -> Self {
        Self {
            node_label: node_label.to_string(),
            unique_id: unique_id.to_string(),
        }
    }
}

impl ClusterHandler for BridgedDeviceBasicInformationHandler {
    fn cluster_id(&self) -> u32 {
        im::CLUSTER_BRIDGED_DEVICE_BASIC_INFORMATION
    }

    /// ClusterRevision (spec §7.13): Bridged Device Basic Information
    /// cluster spec revision 3 (Matter 1.4).
    fn revision(&self) -> u16 {
        3
    }

    fn attributes(&self) -> Vec<u32> {
        vec![
            im::ATTR_BI_NODE_LABEL,
            im::ATTR_BDBI_REACHABLE,
            im::ATTR_BI_UNIQUE_ID,
        ]
    }

    fn read(&self, attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
        match attribute {
            im::ATTR_BI_NODE_LABEL => Some(str_value(&self.node_label)),
            im::ATTR_BDBI_REACHABLE => Some(bool_value(true)),
            im::ATTR_BI_UNIQUE_ID => Some(str_value(&self.unique_id)),
            _ => None,
        }
    }

    fn invoke(&mut self, _command: u32, _fields_tlv: &[u8], _ctx: &mut InvokeCtx) -> InvokeReply {
        // Bridged Device Basic Information declares no commands.
        InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
    }

    // write はデフォルト（`STATUS_UNSUPPORTED_WRITE` で全属性拒否）のまま
    // — 上のモジュールコメントの通り、NodeLabel も含め write を許さない。
}

#[cfg(test)]
mod tests {
    use super::*;
    use mat_controller::tlv::{Reader, Value};

    fn read_str(h: &BridgedDeviceBasicInformationHandler, attribute: u32) -> String {
        let tlv = h.read(attribute, &ReadCtx::default()).expect("attribute");
        let mut r = Reader::new(&tlv);
        match r.next().unwrap().unwrap().value {
            Value::Utf8(v) => v.to_string(),
            other => panic!("expected string, got {other:?}"),
        }
    }

    fn read_bool(h: &BridgedDeviceBasicInformationHandler, attribute: u32) -> bool {
        let tlv = h.read(attribute, &ReadCtx::default()).expect("attribute");
        let mut r = Reader::new(&tlv);
        match r.next().unwrap().unwrap().value {
            Value::Bool(v) => v,
            other => panic!("expected bool, got {other:?}"),
        }
    }

    #[test]
    fn declares_the_three_attributes() {
        let h = BridgedDeviceBasicInformationHandler::new("living-light", "unique-abc123");
        assert_eq!(
            h.attributes(),
            vec![
                im::ATTR_BI_NODE_LABEL,
                im::ATTR_BDBI_REACHABLE,
                im::ATTR_BI_UNIQUE_ID,
            ]
        );
    }

    #[test]
    fn reads_node_label_from_constructor() {
        let h = BridgedDeviceBasicInformationHandler::new("living-light", "unique-abc123");
        assert_eq!(read_str(&h, im::ATTR_BI_NODE_LABEL), "living-light");
    }

    #[test]
    fn reads_unique_id_from_constructor() {
        let h = BridgedDeviceBasicInformationHandler::new("living-light", "unique-abc123");
        assert_eq!(read_str(&h, im::ATTR_BI_UNIQUE_ID), "unique-abc123");
    }

    #[test]
    fn reachable_is_always_true() {
        let h = BridgedDeviceBasicInformationHandler::new("living-light", "unique-abc123");
        assert!(read_bool(&h, im::ATTR_BDBI_REACHABLE));
    }

    #[test]
    fn cluster_id_and_revision() {
        let h = BridgedDeviceBasicInformationHandler::new("living-light", "unique-abc123");
        assert_eq!(h.cluster_id(), im::CLUSTER_BRIDGED_DEVICE_BASIC_INFORMATION);
        assert_eq!(h.revision(), 3);
    }

    #[test]
    fn unimplemented_attribute_reads_as_none() {
        let h = BridgedDeviceBasicInformationHandler::new("living-light", "unique-abc123");
        assert!(h.read(0x7777, &ReadCtx::default()).is_none());
    }

    #[test]
    fn no_commands_are_accepted_and_invoke_rejects_everything() {
        let mut h = BridgedDeviceBasicInformationHandler::new("living-light", "unique-abc123");
        assert_eq!(h.accepted_commands(), Vec::<u32>::new());
        assert_eq!(h.generated_commands(), Vec::<u32>::new());
        assert_eq!(
            h.invoke(0x00, &[], &mut InvokeCtx::default()),
            InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
        );
    }

    #[test]
    fn write_is_rejected_by_default() {
        let mut h = BridgedDeviceBasicInformationHandler::new("living-light", "unique-abc123");
        let data = str_value("new-name");
        assert_eq!(
            h.write(
                im::ATTR_BI_NODE_LABEL,
                &data,
                false,
                &mut InvokeCtx::default()
            ),
            Err(im::STATUS_UNSUPPORTED_WRITE)
        );
    }
}
