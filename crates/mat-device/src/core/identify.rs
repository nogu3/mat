//! Identify クラスタサーバ (spec §1.2, cluster 0x0003)。On/Off Light
//! デバイスタイプ（Device Library §4.1）の必須クラスタ — chip-tool/HA は
//! 不在を咎めないが、Apple Home は commissioning 直後の interview で
//! デバイスタイプ適合性を検査する。仮想デバイスに視覚的な identify 手段は
//! 無いので IdentifyType は None、Identify コマンドは残り秒数
//! （IdentifyTime）の帳簿付けだけを行う。
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use mat_controller::im;
use mat_controller::tlv::{Reader, Tag, Value, Writer};

use crate::core::datamodel::{ClusterHandler, InvokeCtx, InvokeReply, ReadCtx};

/// IdentifyType = None (spec §1.2.5.2): no visible/audible identification
/// output — the honest answer for a headless virtual device.
const IDENTIFY_TYPE_NONE: u64 = 0x00;

/// Shared "is the device currently identifying" handle — Groups'
/// `AddGroupIfIdentifying` (spec §1.3.7.6) is conditioned on it.
#[derive(Clone)]
pub struct IdentifyState(Arc<Mutex<Option<Instant>>>);

impl IdentifyState {
    pub fn is_identifying(&self) -> bool {
        self.0
            .lock()
            .expect("identify state mutex poisoned")
            .is_some_and(|deadline| deadline > Instant::now())
    }
}

pub struct IdentifyHandler {
    deadline: Arc<Mutex<Option<Instant>>>,
}

impl IdentifyHandler {
    /// Creates a fresh handler (not identifying) plus the shared observe
    /// handle `GroupsHandler` needs.
    pub fn new() -> (Self, IdentifyState) {
        let deadline = Arc::new(Mutex::new(None));
        (
            Self {
                deadline: Arc::clone(&deadline),
            },
            IdentifyState(deadline),
        )
    }

    fn remaining_secs(&self) -> u64 {
        self.deadline
            .lock()
            .expect("identify state mutex poisoned")
            .map(|deadline| deadline.saturating_duration_since(Instant::now()).as_secs())
            .unwrap_or(0)
    }
}

impl ClusterHandler for IdentifyHandler {
    fn cluster_id(&self) -> u32 {
        im::CLUSTER_IDENTIFY
    }

    /// ClusterRevision (spec §7.13): Identify cluster spec revision 4
    /// (Matter 1.4).
    fn revision(&self) -> u16 {
        4
    }

    fn attributes(&self) -> Vec<u32> {
        vec![im::ATTR_IDENTIFY_TIME, im::ATTR_IDENTIFY_TYPE]
    }

    fn read(&self, attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
        let value = match attribute {
            im::ATTR_IDENTIFY_TIME => self.remaining_secs(),
            im::ATTR_IDENTIFY_TYPE => IDENTIFY_TYPE_NONE,
            _ => return None,
        };
        let mut w = Writer::new();
        w.put_uint(Tag::Anonymous, value);
        Some(w.finish())
    }

    fn invoke(&mut self, command: u32, fields_tlv: &[u8], ctx: &mut InvokeCtx) -> InvokeReply {
        match command {
            im::CMD_IDENTIFY => {
                let Some(secs) = decode_identify_time(fields_tlv) else {
                    return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
                };
                *self.deadline.lock().expect("identify state mutex poisoned") = if secs == 0 {
                    None
                } else {
                    Some(Instant::now() + Duration::from_secs(u64::from(secs)))
                };
                ctx.changed.push(im::ATTR_IDENTIFY_TIME);
                InvokeReply::Status(im::STATUS_SUCCESS)
            }
            // ヘッドレス仮想デバイス: 発光エフェクトは表現できないので
            // 受理だけする（拒否すると spec 必須コマンド欠落になる）。
            im::CMD_IDENTIFY_TRIGGER_EFFECT => InvokeReply::Status(im::STATUS_SUCCESS),
            _ => InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND),
        }
    }

    fn accepted_commands(&self) -> Vec<u32> {
        vec![im::CMD_IDENTIFY, im::CMD_IDENTIFY_TRIGGER_EFFECT]
    }
}

/// `Identify` request fields (spec §1.2.7.1): `{0: IdentifyTime (uint16)}`.
fn decode_identify_time(fields_tlv: &[u8]) -> Option<u16> {
    let mut r = Reader::new(fields_tlv);
    match r.next() {
        Ok(Some(el)) if el.value == Value::StructStart => {}
        _ => return None,
    }
    let mut time = None;
    loop {
        match r.next() {
            Ok(Some(el)) => match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(0), Value::Uint(v)) => time = u16::try_from(v).ok(),
                _ => {}
            },
            _ => return None,
        }
    }
    time
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identify_fields(secs: u16) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), u64::from(secs));
        w.end_container();
        w.finish()
    }

    fn read_uint(h: &IdentifyHandler, attribute: u32) -> u64 {
        let tlv = h.read(attribute, &ReadCtx::default()).expect("attribute");
        let mut r = Reader::new(&tlv);
        match r.next().unwrap().unwrap().value {
            Value::Uint(v) => v,
            other => panic!("expected uint, got {other:?}"),
        }
    }

    #[test]
    fn serves_identify_time_and_type() {
        let (h, state) = IdentifyHandler::new();
        assert_eq!(
            h.attributes(),
            vec![im::ATTR_IDENTIFY_TIME, im::ATTR_IDENTIFY_TYPE]
        );
        assert_eq!(read_uint(&h, im::ATTR_IDENTIFY_TIME), 0);
        assert_eq!(read_uint(&h, im::ATTR_IDENTIFY_TYPE), IDENTIFY_TYPE_NONE);
        assert!(!state.is_identifying());
        assert!(h.read(0x7777, &ReadCtx::default()).is_none());
    }

    #[test]
    fn identify_command_starts_and_stops_the_countdown() {
        let (mut h, state) = IdentifyHandler::new();
        let mut ctx = InvokeCtx::default();
        assert_eq!(
            h.invoke(im::CMD_IDENTIFY, &identify_fields(30), &mut ctx),
            InvokeReply::Status(im::STATUS_SUCCESS)
        );
        assert_eq!(ctx.changed, vec![im::ATTR_IDENTIFY_TIME]);
        assert!(state.is_identifying());
        let remaining = read_uint(&h, im::ATTR_IDENTIFY_TIME);
        assert!((1..=30).contains(&remaining), "remaining={remaining}");

        // Identify(0) は identify の即時停止 (spec §1.2.7.1)。
        ctx.changed.clear();
        assert_eq!(
            h.invoke(im::CMD_IDENTIFY, &identify_fields(0), &mut ctx),
            InvokeReply::Status(im::STATUS_SUCCESS)
        );
        assert!(!state.is_identifying());
        assert_eq!(read_uint(&h, im::ATTR_IDENTIFY_TIME), 0);
    }

    #[test]
    fn trigger_effect_is_accepted_as_a_noop() {
        let (mut h, state) = IdentifyHandler::new();
        // TriggerEffect fields (spec §1.2.7.3): {0: EffectIdentifier, 1: EffectVariant}
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 0x00); // Blink
        w.put_uint(Tag::Context(1), 0x00);
        w.end_container();
        assert_eq!(
            h.invoke(
                im::CMD_IDENTIFY_TRIGGER_EFFECT,
                &w.finish(),
                &mut InvokeCtx::default()
            ),
            InvokeReply::Status(im::STATUS_SUCCESS)
        );
        assert!(!state.is_identifying());
    }

    #[test]
    fn malformed_identify_fields_are_rejected() {
        let (mut h, _state) = IdentifyHandler::new();
        assert_eq!(
            h.invoke(im::CMD_IDENTIFY, &[], &mut InvokeCtx::default()),
            InvokeReply::Status(im::STATUS_INVALID_COMMAND)
        );
    }

    #[test]
    fn unknown_command_is_rejected_and_command_lists_are_declared() {
        let (mut h, _state) = IdentifyHandler::new();
        assert_eq!(
            h.invoke(0x7F, &[], &mut InvokeCtx::default()),
            InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
        );
        assert_eq!(
            h.accepted_commands(),
            vec![im::CMD_IDENTIFY, im::CMD_IDENTIFY_TRIGGER_EFFECT]
        );
        assert_eq!(h.generated_commands(), Vec::<u32>::new());
    }
}
