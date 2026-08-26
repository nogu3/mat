//! GeneralDiagnostics クラスタサーバ (spec §11.11, cluster 0x0033) —
//! RootNode デバイスタイプの必須クラスタ（Device Library §9.2.2）。Apple
//! Home は commissioning 直後の interview でこのクラスタの不在を咎める。
//!
//! mat-device は最小実装: `NetworkInterfaces`/`RebootCount`/`UpTime` の
//! 読み取り専用ステータス属性のみを本物の値（自プロセスの起動時刻から算出
//! した `UpTime`、常に 0 の `RebootCount` — 再起動検知の永続化は M3 送り）
//! で返し、`TestEventTrigger` は常に拒否する。`TestEventTriggersEnabled`
//! が spec の enable key と一致する秘密を一切持たない実装であることの
//! 素直な帰結（spec §11.11.6.1: enable key が一致しない `TestEventTrigger`
//! は `CONSTRAINT_ERROR`）。
use std::time::Instant;

use mat_controller::im;
use mat_controller::tlv::{Tag, Writer};

use crate::core::datamodel::{ClusterHandler, InvokeCtx, InvokeReply, ReadCtx};

/// `NetworkInterface.type` (spec §11.11.5.1, InterfaceTypeEnum): Ethernet.
/// mat-device only ever serves a single wired interface (mirrors
/// `NetworkCommissioningHandler`'s Ethernet-only feature map).
const INTERFACE_TYPE_ETHERNET: u64 = 2;

pub struct GeneralDiagnosticsHandler {
    iface: String,
    started: Instant,
}

impl GeneralDiagnosticsHandler {
    pub fn new(iface: &str) -> Self {
        Self {
            iface: iface.to_string(),
            started: Instant::now(),
        }
    }
}

impl ClusterHandler for GeneralDiagnosticsHandler {
    fn cluster_id(&self) -> u32 {
        im::CLUSTER_GENERAL_DIAGNOSTICS
    }

    /// ClusterRevision (spec §7.13): General Diagnostics cluster spec
    /// revision 2 (Matter 1.4).
    fn revision(&self) -> u16 {
        2
    }

    fn attributes(&self) -> Vec<u32> {
        vec![
            im::ATTR_GD_NETWORK_INTERFACES,
            im::ATTR_GD_REBOOT_COUNT,
            im::ATTR_GD_UP_TIME,
            im::ATTR_GD_TEST_EVENT_TRIGGERS_ENABLED,
        ]
    }

    fn read(&self, attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
        match attribute {
            im::ATTR_GD_NETWORK_INTERFACES => {
                let mut w = Writer::new();
                w.start_array(Tag::Anonymous);
                w.start_struct(Tag::Anonymous); // NetworkInterface
                w.put_str(Tag::Context(0), &self.iface); // Name
                w.put_bool(Tag::Context(1), true); // IsOperational
                w.put_bytes(Tag::Context(4), &[0u8; 6]); // HardwareAddress
                w.start_array(Tag::Context(5)); // IPv4Addresses
                w.end_container();
                w.start_array(Tag::Context(6)); // IPv6Addresses
                w.end_container();
                w.put_uint(Tag::Context(7), INTERFACE_TYPE_ETHERNET); // Type
                w.end_container();
                w.end_container();
                Some(w.finish())
            }
            im::ATTR_GD_REBOOT_COUNT => {
                let mut w = Writer::new();
                w.put_uint(Tag::Anonymous, 0);
                Some(w.finish())
            }
            im::ATTR_GD_UP_TIME => {
                let mut w = Writer::new();
                w.put_uint(Tag::Anonymous, self.started.elapsed().as_secs());
                Some(w.finish())
            }
            im::ATTR_GD_TEST_EVENT_TRIGGERS_ENABLED => {
                let mut w = Writer::new();
                w.put_bool(Tag::Anonymous, false);
                Some(w.finish())
            }
            _ => None,
        }
    }

    fn invoke(&mut self, command: u32, _fields_tlv: &[u8], _ctx: &mut InvokeCtx) -> InvokeReply {
        match command {
            // spec §11.11.6.1: enable key が一致しない TestEventTrigger は
            // CONSTRAINT_ERROR。このデバイスは enable key を一切持たない
            // ので、常にこの分岐になる。
            im::CMD_TEST_EVENT_TRIGGER => InvokeReply::Status(im::STATUS_CONSTRAINT_ERROR),
            _ => InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND),
        }
    }

    fn accepted_commands(&self) -> Vec<u32> {
        vec![im::CMD_TEST_EVENT_TRIGGER]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mat_controller::tlv::{Reader, Value};

    fn read(h: &GeneralDiagnosticsHandler, attribute: u32) -> Vec<u8> {
        h.read(attribute, &ReadCtx::default())
            .expect("attribute implemented")
    }

    #[test]
    fn declares_attributes_and_accepted_commands() {
        let h = GeneralDiagnosticsHandler::new("eth0");
        assert_eq!(
            h.attributes(),
            vec![
                im::ATTR_GD_NETWORK_INTERFACES,
                im::ATTR_GD_REBOOT_COUNT,
                im::ATTR_GD_UP_TIME,
                im::ATTR_GD_TEST_EVENT_TRIGGERS_ENABLED,
            ]
        );
        assert_eq!(h.accepted_commands(), vec![im::CMD_TEST_EVENT_TRIGGER]);
        assert_eq!(h.generated_commands(), Vec::<u32>::new());
    }

    #[test]
    fn network_interfaces_reports_one_operational_ethernet_entry() {
        let h = GeneralDiagnosticsHandler::new("eth0");
        let tlv = read(&h, im::ATTR_GD_NETWORK_INTERFACES);
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::ArrayStart);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);

        let e = r.next().unwrap().unwrap();
        assert_eq!(e.tag, Tag::Context(0));
        assert!(matches!(e.value, Value::Utf8(s) if s == "eth0"));

        let e = r.next().unwrap().unwrap();
        assert_eq!(e.tag, Tag::Context(1));
        assert_eq!(e.value, Value::Bool(true));

        let e = r.next().unwrap().unwrap();
        assert_eq!(e.tag, Tag::Context(4));
        assert!(matches!(e.value, Value::Bytes(b) if b == [0u8; 6]));

        let e = r.next().unwrap().unwrap();
        assert_eq!(e.tag, Tag::Context(5));
        assert_eq!(e.value, Value::ArrayStart);
        assert_eq!(r.next().unwrap().unwrap().value, Value::ContainerEnd);

        let e = r.next().unwrap().unwrap();
        assert_eq!(e.tag, Tag::Context(6));
        assert_eq!(e.value, Value::ArrayStart);
        assert_eq!(r.next().unwrap().unwrap().value, Value::ContainerEnd);

        let e = r.next().unwrap().unwrap();
        assert_eq!(e.tag, Tag::Context(7));
        // spec §11.11.5.1 InterfaceTypeEnum: Ethernet=2 — literal (not
        // `INTERFACE_TYPE_ETHERNET`) so this test still catches a wrong
        // constant value.
        assert_eq!(e.value, Value::Uint(2));

        assert_eq!(r.next().unwrap().unwrap().value, Value::ContainerEnd); // struct
        assert_eq!(r.next().unwrap().unwrap().value, Value::ContainerEnd); // array
    }

    #[test]
    fn reboot_count_is_zero() {
        let h = GeneralDiagnosticsHandler::new("eth0");
        let tlv = read(&h, im::ATTR_GD_REBOOT_COUNT);
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(0));
    }

    #[test]
    fn up_time_counts_seconds_since_construction() {
        let h = GeneralDiagnosticsHandler::new("eth0");
        std::thread::sleep(std::time::Duration::from_millis(10));
        let tlv = read(&h, im::ATTR_GD_UP_TIME);
        let mut r = Reader::new(&tlv);
        match r.next().unwrap().unwrap().value {
            Value::Uint(v) => assert!(v < 5, "unexpectedly large uptime: {v}"),
            other => panic!("expected uint, got {other:?}"),
        }
    }

    #[test]
    fn test_event_triggers_enabled_is_false() {
        let h = GeneralDiagnosticsHandler::new("eth0");
        let tlv = read(&h, im::ATTR_GD_TEST_EVENT_TRIGGERS_ENABLED);
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Bool(false));
    }

    #[test]
    fn test_event_trigger_is_always_rejected_with_constraint_error() {
        let mut h = GeneralDiagnosticsHandler::new("eth0");
        assert_eq!(
            h.invoke(im::CMD_TEST_EVENT_TRIGGER, &[], &mut InvokeCtx::default()),
            InvokeReply::Status(im::STATUS_CONSTRAINT_ERROR)
        );
    }

    #[test]
    fn unknown_attribute_and_command_are_rejected() {
        let mut h = GeneralDiagnosticsHandler::new("eth0");
        assert!(h.read(0x7777, &ReadCtx::default()).is_none());
        assert_eq!(
            h.invoke(0x7F, &[], &mut InvokeCtx::default()),
            InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
        );
    }
}
