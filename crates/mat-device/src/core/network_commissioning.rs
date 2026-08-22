//! NetworkCommissioning クラスタサーバ (spec §11.9, cluster 0x0031) —
//! RootNode デバイスタイプの必須クラスタ（Device Library §9.2.2）。Apple
//! Home は commissioning 直後の interview でこのクラスタの不在を咎める。
//!
//! mat-device は有線の仮想デバイスで、実行時にスキャン/切替対象になる無線
//! ネットワークが無い — `FeatureMap`=Ethernet(0x04) の読み取り専用実装
//! （コマンド無し、spec §11.9.4: Ethernet feature はコマンドを一切要求しな
//! い）に絞り、唯一の "network" として自分自身の egress interface を 1 件
//! だけ常時 connected として報告する。
use mat_controller::im;
use mat_controller::tlv::{Tag, Writer};

use crate::core::datamodel::{ClusterHandler, InvokeCtx, InvokeReply, ReadCtx};

/// Ethernet feature bit (spec §11.9.4, Table 82: EthernetNetworkInterface).
const FEATURE_MAP_ETHERNET: u32 = 0x04;

pub struct NetworkCommissioningHandler {
    /// このデバイスの唯一の "network" の NetworkID — egress interface 名
    /// のバイト列（spec §11.9.7.1: Ethernet の NetworkID は実装定義）。
    network_id: Vec<u8>,
}

impl NetworkCommissioningHandler {
    pub fn new(network_id: &str) -> Self {
        Self {
            network_id: network_id.as_bytes().to_vec(),
        }
    }
}

impl ClusterHandler for NetworkCommissioningHandler {
    fn cluster_id(&self) -> u32 {
        im::CLUSTER_NETWORK_COMMISSIONING
    }

    fn attributes(&self) -> Vec<u32> {
        vec![
            im::ATTR_NC_MAX_NETWORKS,
            im::ATTR_NC_NETWORKS,
            im::ATTR_NC_INTERFACE_ENABLED,
            im::ATTR_NC_LAST_NETWORKING_STATUS,
            im::ATTR_NC_LAST_NETWORK_ID,
            im::ATTR_NC_LAST_CONNECT_ERROR_VALUE,
        ]
    }

    fn read(&self, attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
        match attribute {
            im::ATTR_NC_MAX_NETWORKS => {
                let mut w = Writer::new();
                w.put_uint(Tag::Anonymous, 1);
                Some(w.finish())
            }
            im::ATTR_NC_NETWORKS => {
                let mut w = Writer::new();
                w.start_array(Tag::Anonymous);
                w.start_struct(Tag::Anonymous); // NetworkInfoStruct
                w.put_bytes(Tag::Context(0), &self.network_id); // NetworkID
                w.put_bool(Tag::Context(1), true); // Connected
                w.end_container();
                w.end_container();
                Some(w.finish())
            }
            im::ATTR_NC_INTERFACE_ENABLED => {
                let mut w = Writer::new();
                w.put_bool(Tag::Anonymous, true);
                Some(w.finish())
            }
            im::ATTR_NC_LAST_NETWORKING_STATUS
            | im::ATTR_NC_LAST_NETWORK_ID
            | im::ATTR_NC_LAST_CONNECT_ERROR_VALUE => {
                let mut w = Writer::new();
                w.put_null(Tag::Anonymous);
                Some(w.finish())
            }
            _ => None,
        }
    }

    fn invoke(&mut self, _command: u32, _fields_tlv: &[u8], _ctx: &mut InvokeCtx) -> InvokeReply {
        // Ethernet feature はコマンドを一切要求しない（spec §11.9.4）。
        InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
    }

    fn feature_map(&self) -> u32 {
        FEATURE_MAP_ETHERNET
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mat_controller::tlv::{Reader, Value};

    fn read(h: &NetworkCommissioningHandler, attribute: u32) -> Vec<u8> {
        h.read(attribute, &ReadCtx::default())
            .expect("attribute implemented")
    }

    #[test]
    fn declares_ethernet_feature_map_and_attributes() {
        let h = NetworkCommissioningHandler::new("eth0");
        // spec §11.9.4, Table 82: Ethernet feature bit — literal (not
        // `FEATURE_MAP_ETHERNET`) so this test still catches a wrong
        // constant value.
        assert_eq!(h.feature_map(), 0x04);
        assert_eq!(
            h.attributes(),
            vec![
                im::ATTR_NC_MAX_NETWORKS,
                im::ATTR_NC_NETWORKS,
                im::ATTR_NC_INTERFACE_ENABLED,
                im::ATTR_NC_LAST_NETWORKING_STATUS,
                im::ATTR_NC_LAST_NETWORK_ID,
                im::ATTR_NC_LAST_CONNECT_ERROR_VALUE,
            ]
        );
    }

    #[test]
    fn max_networks_is_one() {
        let h = NetworkCommissioningHandler::new("eth0");
        let tlv = read(&h, im::ATTR_NC_MAX_NETWORKS);
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(1));
    }

    #[test]
    fn networks_reports_one_connected_entry_named_after_the_iface() {
        let h = NetworkCommissioningHandler::new("eth0");
        let tlv = read(&h, im::ATTR_NC_NETWORKS);
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::ArrayStart);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let e = r.next().unwrap().unwrap();
        assert_eq!(e.tag, Tag::Context(0));
        assert!(matches!(e.value, Value::Bytes(b) if b == b"eth0"));
        let e = r.next().unwrap().unwrap();
        assert_eq!(e.tag, Tag::Context(1));
        assert_eq!(e.value, Value::Bool(true));
        assert_eq!(r.next().unwrap().unwrap().value, Value::ContainerEnd); // struct end
        assert_eq!(r.next().unwrap().unwrap().value, Value::ContainerEnd); // array end
    }

    #[test]
    fn interface_enabled_is_true() {
        let h = NetworkCommissioningHandler::new("eth0");
        let tlv = read(&h, im::ATTR_NC_INTERFACE_ENABLED);
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Bool(true));
    }

    #[test]
    fn last_network_status_id_and_error_are_null() {
        let h = NetworkCommissioningHandler::new("eth0");
        for attr in [
            im::ATTR_NC_LAST_NETWORKING_STATUS,
            im::ATTR_NC_LAST_NETWORK_ID,
            im::ATTR_NC_LAST_CONNECT_ERROR_VALUE,
        ] {
            let tlv = read(&h, attr);
            let mut r = Reader::new(&tlv);
            assert_eq!(r.next().unwrap().unwrap().value, Value::Null);
        }
    }

    #[test]
    fn unknown_attribute_and_command_are_rejected() {
        let mut h = NetworkCommissioningHandler::new("eth0");
        assert!(h.read(0x7777, &ReadCtx::default()).is_none());
        assert_eq!(
            h.invoke(0x00, &[], &mut InvokeCtx::default()),
            InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
        );
        assert_eq!(h.accepted_commands(), Vec::<u32>::new());
        assert_eq!(h.generated_commands(), Vec::<u32>::new());
    }
}
