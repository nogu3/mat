//! Generic Switch クラスタサーバ (spec §1.13, cluster 0x003B)。momentary switch
//! （MS|MSR|MSL|MSM）。ボタン押下は `stimulate(Press)` で注入され、spec §1.13.6 の
//! イベント列を `InvokeCtx::events` に積む。コマンドは無い。
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

use mat_controller::im::{self, EventPriority};
use mat_controller::tlv::{Tag, Writer};

use crate::core::datamodel::{ClusterHandler, InvokeCtx, InvokeReply, ReadCtx};
use crate::core::events::EmittedEvent;
use crate::core::stimulus::{PressKind, Stimulus, StimulusReply};
use crate::core::tlv_value;

pub struct GenericSwitchHandler {
    position: Arc<AtomicU8>,
}

impl GenericSwitchHandler {
    pub const NUMBER_OF_POSITIONS: u8 = 2;
    pub const MULTI_PRESS_MAX: u8 = 3;

    /// Creates a fresh handler (initial position: 0, released) plus a
    /// shared handle to its `CurrentPosition` state.
    pub fn new() -> (Self, Arc<AtomicU8>) {
        let position = Arc::new(AtomicU8::new(0));
        (
            Self {
                position: Arc::clone(&position),
            },
            position,
        )
    }
}

/// A single-field event payload struct `{0: v}` (e.g. `NewPosition` /
/// `PreviousPosition`).
fn one_field(v: u8) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(v));
    w.end_container();
    w.finish()
}

/// A two-field event payload struct `{0: a, 1: b}` (MultiPressOngoing's
/// `NewPosition`/`CurrentNumberOfPressesCounted`, MultiPressComplete's
/// `PreviousPosition`/`TotalNumberOfPressesCounted` — spec §1.13.6).
fn two_fields(a: u8, b: u8) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(a));
    w.put_uint(Tag::Context(1), u64::from(b));
    w.end_container();
    w.finish()
}

fn info(event: u32, data_tlv: Vec<u8>) -> EmittedEvent {
    EmittedEvent {
        event,
        priority: EventPriority::Info,
        data_tlv: Some(data_tlv),
    }
}

impl ClusterHandler for GenericSwitchHandler {
    fn cluster_id(&self) -> u32 {
        im::CLUSTER_SWITCH
    }

    /// ClusterRevision (spec §7.13): Switch cluster spec revision 2 (Matter
    /// 1.4).
    fn revision(&self) -> u16 {
        2
    }

    fn feature_map(&self) -> u32 {
        im::SWITCH_FEATURE_MOMENTARY
            | im::SWITCH_FEATURE_MOMENTARY_RELEASE
            | im::SWITCH_FEATURE_MOMENTARY_LONG_PRESS
            | im::SWITCH_FEATURE_MOMENTARY_MULTI_PRESS
    }

    fn attributes(&self) -> Vec<u32> {
        vec![
            im::ATTR_SWITCH_NUMBER_OF_POSITIONS,
            im::ATTR_SWITCH_CURRENT_POSITION,
            im::ATTR_SWITCH_MULTI_PRESS_MAX,
        ]
    }

    fn events(&self) -> Vec<u32> {
        vec![
            im::EVENT_SWITCH_INITIAL_PRESS,
            im::EVENT_SWITCH_LONG_PRESS,
            im::EVENT_SWITCH_SHORT_RELEASE,
            im::EVENT_SWITCH_LONG_RELEASE,
            im::EVENT_SWITCH_MULTI_PRESS_ONGOING,
            im::EVENT_SWITCH_MULTI_PRESS_COMPLETE,
        ]
    }

    fn read(&self, attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
        match attribute {
            im::ATTR_SWITCH_NUMBER_OF_POSITIONS => {
                Some(tlv_value::uint(u64::from(Self::NUMBER_OF_POSITIONS)))
            }
            im::ATTR_SWITCH_CURRENT_POSITION => Some(tlv_value::uint(u64::from(
                self.position.load(Ordering::SeqCst),
            ))),
            im::ATTR_SWITCH_MULTI_PRESS_MAX => {
                Some(tlv_value::uint(u64::from(Self::MULTI_PRESS_MAX)))
            }
            _ => None,
        }
    }

    fn invoke(&mut self, _command: u32, _fields_tlv: &[u8], _ctx: &mut InvokeCtx) -> InvokeReply {
        InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
    }

    fn stimulate(&mut self, stimulus: &Stimulus, ctx: &mut InvokeCtx) -> StimulusReply {
        let Stimulus::Press(kind) = stimulus else {
            return StimulusReply::Unsupported;
        };
        // 押下中は position 1、離すと 0。1 刺激で往復するので CurrentPosition は変化なし
        // （changed に積まない）— イベント列が本体。
        match kind {
            PressKind::Short => {
                ctx.events
                    .push(info(im::EVENT_SWITCH_INITIAL_PRESS, one_field(1)));
                ctx.events
                    .push(info(im::EVENT_SWITCH_SHORT_RELEASE, one_field(1)));
            }
            PressKind::Long => {
                ctx.events
                    .push(info(im::EVENT_SWITCH_INITIAL_PRESS, one_field(1)));
                ctx.events
                    .push(info(im::EVENT_SWITCH_LONG_PRESS, one_field(1)));
                ctx.events
                    .push(info(im::EVENT_SWITCH_LONG_RELEASE, one_field(1)));
            }
            PressKind::Multi(n) => {
                if *n < 2 {
                    return StimulusReply::Rejected("multi press count must be at least 2");
                }
                if *n > Self::MULTI_PRESS_MAX {
                    return StimulusReply::Rejected("multi press count exceeds MultiPressMax");
                }
                ctx.events
                    .push(info(im::EVENT_SWITCH_INITIAL_PRESS, one_field(1)));
                ctx.events
                    .push(info(im::EVENT_SWITCH_SHORT_RELEASE, one_field(1)));
                for i in 2..=*n {
                    ctx.events
                        .push(info(im::EVENT_SWITCH_INITIAL_PRESS, one_field(1)));
                    ctx.events
                        .push(info(im::EVENT_SWITCH_MULTI_PRESS_ONGOING, two_fields(1, i)));
                    ctx.events
                        .push(info(im::EVENT_SWITCH_SHORT_RELEASE, one_field(1)));
                }
                ctx.events.push(info(
                    im::EVENT_SWITCH_MULTI_PRESS_COMPLETE,
                    two_fields(1, *n),
                ));
            }
        }
        StimulusReply::Applied
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn numbers(ctx: &InvokeCtx) -> Vec<u32> {
        ctx.events.iter().map(|e| e.event).collect()
    }
    fn field(ctx: &InvokeCtx, i: usize, tag: u8) -> u64 {
        let tlv = ctx.events[i].data_tlv.as_ref().unwrap();
        let v = mat_controller::im::tlv_to_json(tlv).unwrap();
        v[tag.to_string()].as_u64().unwrap()
    }

    #[test]
    fn short_press_emits_initial_press_then_short_release() {
        let (mut h, pos) = GenericSwitchHandler::new();
        let mut ctx = InvokeCtx::default();
        assert_eq!(
            h.stimulate(&Stimulus::Press(PressKind::Short), &mut ctx),
            StimulusReply::Applied
        );
        assert_eq!(
            numbers(&ctx),
            vec![
                im::EVENT_SWITCH_INITIAL_PRESS,
                im::EVENT_SWITCH_SHORT_RELEASE
            ]
        );
        assert_eq!(field(&ctx, 0, 0), 1); // NewPosition
        assert_eq!(field(&ctx, 1, 0), 1); // PreviousPosition
        assert!(ctx.events.iter().all(|e| e.priority == EventPriority::Info));
        assert!(
            ctx.changed.is_empty(),
            "CurrentPosition ends where it started"
        );
        assert_eq!(pos.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn long_press_emits_initial_long_press_long_release() {
        let (mut h, _) = GenericSwitchHandler::new();
        let mut ctx = InvokeCtx::default();
        h.stimulate(&Stimulus::Press(PressKind::Long), &mut ctx);
        assert_eq!(
            numbers(&ctx),
            vec![
                im::EVENT_SWITCH_INITIAL_PRESS,
                im::EVENT_SWITCH_LONG_PRESS,
                im::EVENT_SWITCH_LONG_RELEASE
            ]
        );
    }

    #[test]
    fn double_press_follows_the_msm_sequence() {
        let (mut h, _) = GenericSwitchHandler::new();
        let mut ctx = InvokeCtx::default();
        h.stimulate(&Stimulus::Press(PressKind::Multi(2)), &mut ctx);
        assert_eq!(
            numbers(&ctx),
            vec![
                im::EVENT_SWITCH_INITIAL_PRESS,
                im::EVENT_SWITCH_SHORT_RELEASE,
                im::EVENT_SWITCH_INITIAL_PRESS,
                im::EVENT_SWITCH_MULTI_PRESS_ONGOING,
                im::EVENT_SWITCH_SHORT_RELEASE,
                im::EVENT_SWITCH_MULTI_PRESS_COMPLETE,
            ]
        );
        assert_eq!(field(&ctx, 3, 1), 2); // CurrentNumberOfPressesCounted
        assert_eq!(field(&ctx, 5, 0), 1); // PreviousPosition
        assert_eq!(field(&ctx, 5, 1), 2); // TotalNumberOfPressesCounted
    }

    #[test]
    fn multi_press_out_of_range_is_rejected_and_set_state_unsupported() {
        let (mut h, _) = GenericSwitchHandler::new();
        assert!(matches!(
            h.stimulate(
                &Stimulus::Press(PressKind::Multi(1)),
                &mut InvokeCtx::default()
            ),
            StimulusReply::Rejected(_)
        ));
        assert!(matches!(
            h.stimulate(
                &Stimulus::Press(PressKind::Multi(4)),
                &mut InvokeCtx::default()
            ),
            StimulusReply::Rejected(_)
        ));
        assert_eq!(
            h.stimulate(&Stimulus::SetState(true), &mut InvokeCtx::default()),
            StimulusReply::Unsupported
        );
    }

    #[test]
    fn attributes_feature_map_and_events_match_the_spec_shape() {
        let (h, _) = GenericSwitchHandler::new();
        assert_eq!(h.cluster_id(), im::CLUSTER_SWITCH);
        assert_eq!(h.revision(), 2);
        assert_eq!(
            h.feature_map(),
            im::SWITCH_FEATURE_MOMENTARY
                | im::SWITCH_FEATURE_MOMENTARY_RELEASE
                | im::SWITCH_FEATURE_MOMENTARY_LONG_PRESS
                | im::SWITCH_FEATURE_MOMENTARY_MULTI_PRESS
        );
        assert_eq!(
            h.attributes(),
            vec![
                im::ATTR_SWITCH_NUMBER_OF_POSITIONS,
                im::ATTR_SWITCH_CURRENT_POSITION,
                im::ATTR_SWITCH_MULTI_PRESS_MAX
            ]
        );
        assert_eq!(
            h.events(),
            vec![
                im::EVENT_SWITCH_INITIAL_PRESS,
                im::EVENT_SWITCH_LONG_PRESS,
                im::EVENT_SWITCH_SHORT_RELEASE,
                im::EVENT_SWITCH_LONG_RELEASE,
                im::EVENT_SWITCH_MULTI_PRESS_ONGOING,
                im::EVENT_SWITCH_MULTI_PRESS_COMPLETE
            ]
        );
        let v = mat_controller::im::tlv_to_json(
            &h.read(im::ATTR_SWITCH_MULTI_PRESS_MAX, &ReadCtx::default())
                .unwrap(),
        )
        .unwrap();
        assert_eq!(v, serde_json::json!(3));
    }
}
