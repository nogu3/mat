//! Boolean State クラスタサーバ (spec §1.7, cluster 0x0045)。Contact Sensor
//! デバイスタイプの本体クラスタ — `StateValue` を `stimulate(SetState)` で
//! 動かし、変化した時だけ `StateChange` イベントを積む。コマンドは無い。
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use mat_controller::im::{self, EventPriority};
use mat_controller::tlv::{Tag, Writer};

use crate::core::datamodel::{ClusterHandler, InvokeCtx, InvokeReply, ReadCtx};
use crate::core::events::EmittedEvent;
use crate::core::stimulus::{Stimulus, StimulusReply};

pub struct BooleanStateHandler {
    state: Arc<AtomicBool>,
}

impl BooleanStateHandler {
    /// Creates a fresh handler (initial `StateValue`: false) plus a shared
    /// handle to its state.
    pub fn new() -> (Self, Arc<AtomicBool>) {
        let state = Arc::new(AtomicBool::new(false));
        (
            Self {
                state: Arc::clone(&state),
            },
            state,
        )
    }
}

impl ClusterHandler for BooleanStateHandler {
    fn cluster_id(&self) -> u32 {
        im::CLUSTER_BOOLEAN_STATE
    }

    /// ClusterRevision (spec §7.13): Boolean State cluster spec revision 1
    /// (Matter 1.4).
    fn revision(&self) -> u16 {
        1
    }

    fn attributes(&self) -> Vec<u32> {
        vec![im::ATTR_BS_STATE_VALUE]
    }

    fn events(&self) -> Vec<u32> {
        vec![im::EVENT_BS_STATE_CHANGE]
    }

    fn read(&self, attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
        match attribute {
            im::ATTR_BS_STATE_VALUE => {
                let mut w = Writer::new();
                w.put_bool(Tag::Anonymous, self.state.load(Ordering::SeqCst));
                Some(w.finish())
            }
            _ => None,
        }
    }

    fn invoke(&mut self, _command: u32, _fields_tlv: &[u8], _ctx: &mut InvokeCtx) -> InvokeReply {
        InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
    }

    fn stimulate(&mut self, stimulus: &Stimulus, ctx: &mut InvokeCtx) -> StimulusReply {
        let Stimulus::SetState(new) = stimulus else {
            return StimulusReply::Unsupported;
        };
        // `swap` (not `store`) so "did it actually change" is decided
        // atomically with the write — mirrors `OnOffHandler::invoke`.
        let previous = self.state.swap(*new, Ordering::SeqCst);
        if previous != *new {
            ctx.changed.push(im::ATTR_BS_STATE_VALUE);
            let mut w = Writer::new();
            w.start_struct(Tag::Anonymous);
            w.put_bool(Tag::Context(0), *new);
            w.end_container();
            ctx.events.push(EmittedEvent {
                event: im::EVENT_BS_STATE_CHANGE,
                priority: EventPriority::Info,
                data_tlv: Some(w.finish()),
            });
        }
        StimulusReply::Applied
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::stimulus::PressKind;

    #[test]
    fn set_state_changes_attribute_and_emits_state_change_only_on_transition() {
        let (mut h, state) = BooleanStateHandler::new();
        let mut ctx = InvokeCtx::default();
        assert_eq!(
            h.stimulate(&Stimulus::SetState(true), &mut ctx),
            StimulusReply::Applied
        );
        assert_eq!(ctx.changed, vec![im::ATTR_BS_STATE_VALUE]);
        assert_eq!(ctx.events.len(), 1);
        assert_eq!(ctx.events[0].event, im::EVENT_BS_STATE_CHANGE);
        assert_eq!(
            mat_controller::im::tlv_to_json(ctx.events[0].data_tlv.as_ref().unwrap()).unwrap(),
            serde_json::json!({"0": true})
        );
        assert!(state.load(Ordering::SeqCst));
        let mut ctx = InvokeCtx::default();
        h.stimulate(&Stimulus::SetState(true), &mut ctx);
        assert!(ctx.changed.is_empty() && ctx.events.is_empty());
        assert_eq!(
            h.stimulate(
                &Stimulus::Press(PressKind::Short),
                &mut InvokeCtx::default()
            ),
            StimulusReply::Unsupported
        );
        assert_eq!(h.cluster_id(), im::CLUSTER_BOOLEAN_STATE);
        assert_eq!(h.attributes(), vec![im::ATTR_BS_STATE_VALUE]);
        assert_eq!(h.events(), vec![im::EVENT_BS_STATE_CHANGE]);
    }
}
