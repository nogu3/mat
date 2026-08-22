//! GroupKeyManagement クラスタサーバ (spec §11.2, cluster 0x003F) —
//! RootNode デバイスタイプの必須クラスタ（Device Library §9.2.2）。Apple
//! Home は commissioning 直後の interview でこのクラスタの不在を咎める。
//!
//! mat-device は最小実装: `GroupKeyMap`/`GroupTable` を常に空、容量属性を
//! 固定値で読ませるだけ — グループキーの発行・配布そのもの
//! （`KeySetWrite`/`KeySetRead`/`KeySetRemove` 等のコマンド）は未実装（既知
//! ギャップ、spec §11.2 の申し送り節に記録）。Groups クラスタ
//! （`core::groups`）自体も永続グループ束縛を持たない M2 実装なので、
//! groupcast の実配線が無い今はコマンド欠落の実害が無い。
use mat_controller::im;
use mat_controller::tlv::{Tag, Writer};

use crate::core::datamodel::{ClusterHandler, InvokeCtx, InvokeReply, ReadCtx};

/// `MaxGroupsPerFabric`/`MaxGroupKeysPerFabric` (spec §11.2.7.5) — 固定値を
/// 返すのみで実容量は追跡しない（`AccessControlHandler`の容量属性と同じ
/// 割り切り）。
const MAX_GROUPS_PER_FABRIC: u64 = 16;
const MAX_GROUP_KEYS_PER_FABRIC: u64 = 1;

#[derive(Default)]
pub struct GroupKeyManagementHandler;

impl GroupKeyManagementHandler {
    pub fn new() -> Self {
        Self
    }
}

impl ClusterHandler for GroupKeyManagementHandler {
    fn cluster_id(&self) -> u32 {
        im::CLUSTER_GROUP_KEY_MANAGEMENT
    }

    fn attributes(&self) -> Vec<u32> {
        vec![
            im::ATTR_GROUP_KEY_MAP,
            im::ATTR_GROUP_TABLE,
            im::ATTR_MAX_GROUPS_PER_FABRIC,
            im::ATTR_MAX_GROUP_KEYS_PER_FABRIC,
        ]
    }

    fn read(&self, attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
        match attribute {
            im::ATTR_GROUP_KEY_MAP | im::ATTR_GROUP_TABLE => {
                let mut w = Writer::new();
                w.start_array(Tag::Anonymous);
                w.end_container();
                Some(w.finish())
            }
            im::ATTR_MAX_GROUPS_PER_FABRIC => {
                let mut w = Writer::new();
                w.put_uint(Tag::Anonymous, MAX_GROUPS_PER_FABRIC);
                Some(w.finish())
            }
            im::ATTR_MAX_GROUP_KEYS_PER_FABRIC => {
                let mut w = Writer::new();
                w.put_uint(Tag::Anonymous, MAX_GROUP_KEYS_PER_FABRIC);
                Some(w.finish())
            }
            _ => None,
        }
    }

    fn invoke(&mut self, _command: u32, _fields_tlv: &[u8], _ctx: &mut InvokeCtx) -> InvokeReply {
        // KeySetWrite 等（spec §11.2.7）は未実装 — 既知ギャップ（モジュール
        // doc 参照）。
        InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mat_controller::tlv::{Reader, Value};

    fn read(h: &GroupKeyManagementHandler, attribute: u32) -> Vec<u8> {
        h.read(attribute, &ReadCtx::default())
            .expect("attribute implemented")
    }

    #[test]
    fn declares_attributes_with_no_commands() {
        let h = GroupKeyManagementHandler::new();
        assert_eq!(
            h.attributes(),
            vec![
                im::ATTR_GROUP_KEY_MAP,
                im::ATTR_GROUP_TABLE,
                im::ATTR_MAX_GROUPS_PER_FABRIC,
                im::ATTR_MAX_GROUP_KEYS_PER_FABRIC,
            ]
        );
        assert_eq!(h.accepted_commands(), Vec::<u32>::new());
        assert_eq!(h.generated_commands(), Vec::<u32>::new());
        assert_eq!(h.feature_map(), 0);
    }

    #[test]
    fn group_key_map_and_group_table_are_empty_arrays() {
        let h = GroupKeyManagementHandler::new();
        for attr in [im::ATTR_GROUP_KEY_MAP, im::ATTR_GROUP_TABLE] {
            let tlv = read(&h, attr);
            let mut r = Reader::new(&tlv);
            assert_eq!(r.next().unwrap().unwrap().value, Value::ArrayStart);
            assert_eq!(r.next().unwrap().unwrap().value, Value::ContainerEnd);
        }
    }

    #[test]
    fn capacity_attributes_report_fixed_values() {
        let h = GroupKeyManagementHandler::new();
        // Literal brief values (not `MAX_GROUPS_PER_FABRIC`/
        // `MAX_GROUP_KEYS_PER_FABRIC`) so this test still catches a wrong
        // constant value.
        let tlv = read(&h, im::ATTR_MAX_GROUPS_PER_FABRIC);
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(16));

        let tlv = read(&h, im::ATTR_MAX_GROUP_KEYS_PER_FABRIC);
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(1));
    }

    #[test]
    fn unknown_attribute_and_every_command_are_rejected() {
        let mut h = GroupKeyManagementHandler::new();
        assert!(h.read(0x7777, &ReadCtx::default()).is_none());
        assert_eq!(
            h.invoke(0x00, &[], &mut InvokeCtx::default()),
            InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
        );
    }
}
