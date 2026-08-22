//! OnOff クラスタサーバ (spec §1.5, cluster 0x0006)。M2 スコープ: On/Off/
//! Toggle と OnOff 属性のみ（effect 付きコマンド・GlobalSceneControl 等は
//! スコープ外）。状態は Arc<AtomicBool> — net 側ランタイム/matv がクローン
//! を持ち、購読レポートとログに使う。
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use mat_controller::im;
use mat_controller::tlv::{Tag, Writer};

use crate::core::datamodel::{ClusterHandler, InvokeCtx, InvokeReply, ReadCtx};

pub struct OnOffHandler {
    state: Arc<AtomicBool>,
}

impl OnOffHandler {
    /// Creates a fresh handler (initial state: off) plus a shared handle to
    /// its state. The returned `Arc<AtomicBool>` is a read/observe handle
    /// for callers outside this cluster — Task 12 subscribes to it for
    /// dirty-report notifications, and `matv` reads it for its own status
    /// logging.
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

impl ClusterHandler for OnOffHandler {
    fn cluster_id(&self) -> u32 {
        im::CLUSTER_ON_OFF
    }

    fn attributes(&self) -> Vec<u32> {
        vec![im::ATTR_ON_OFF]
    }

    fn read(&self, attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
        match attribute {
            im::ATTR_ON_OFF => {
                let mut w = Writer::new();
                w.put_bool(Tag::Anonymous, self.state.load(Ordering::SeqCst));
                Some(w.finish())
            }
            _ => None,
        }
    }

    fn invoke(&mut self, command: u32, _fields_tlv: &[u8], ctx: &mut InvokeCtx) -> InvokeReply {
        let new = match command {
            im::CMD_ON_OFF_ON => true,
            im::CMD_ON_OFF_OFF => false,
            im::CMD_ON_OFF_TOGGLE => !self.state.load(Ordering::SeqCst),
            _ => return InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND),
        };
        // `swap` (not `store`) so "did the value actually change" is decided
        // atomically with the write — On on an already-on light must not be
        // reported as a change (spec §8.10 reporting is value-change driven;
        // otherwise every redundant command wakes every subscriber).
        let previous = self.state.swap(new, Ordering::SeqCst);
        if previous != new {
            ctx.changed.push(im::ATTR_ON_OFF);
        }
        InvokeReply::Status(im::STATUS_SUCCESS)
    }

    fn accepted_commands(&self) -> Vec<u32> {
        vec![im::CMD_ON_OFF_OFF, im::CMD_ON_OFF_ON, im::CMD_ON_OFF_TOGGLE]
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::datamodel::{ClusterHandler, InvokeCtx, InvokeReply, ReadCtx};
    use mat_controller::im;

    #[test]
    fn on_off_toggle_flip_state_and_read_reflects_it() {
        let (mut h, state) = OnOffHandler::new();
        assert!(!state.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(
            h.invoke(im::CMD_ON_OFF_ON, &[], &mut InvokeCtx::default()),
            InvokeReply::Status(im::STATUS_SUCCESS)
        );
        assert!(state.load(std::sync::atomic::Ordering::SeqCst));
        h.invoke(im::CMD_ON_OFF_TOGGLE, &[], &mut InvokeCtx::default());
        assert!(!state.load(std::sync::atomic::Ordering::SeqCst));
        // read は TLV bool
        let tlv = h.read(im::ATTR_ON_OFF, &ReadCtx::default()).unwrap();
        let mut r = mat_controller::tlv::Reader::new(&tlv);
        assert_eq!(
            r.next().unwrap().unwrap().value,
            mat_controller::tlv::Value::Bool(false)
        );
    }

    /// Task 12: the handler is what tells `Node` (and through it the
    /// subscription machinery) that OnOff's value moved — and only when it
    /// really moved.
    #[test]
    fn invoke_reports_changed_only_on_an_actual_flip() {
        let (mut h, _state) = OnOffHandler::new();
        let mut ctx = InvokeCtx::default();
        h.invoke(im::CMD_ON_OFF_ON, &[], &mut ctx);
        assert_eq!(ctx.changed, vec![im::ATTR_ON_OFF]);

        // On again on an already-on light: no change to report.
        ctx.changed.clear();
        h.invoke(im::CMD_ON_OFF_ON, &[], &mut ctx);
        assert!(ctx.changed.is_empty());

        // Toggle always flips, so it always changes.
        h.invoke(im::CMD_ON_OFF_TOGGLE, &[], &mut ctx);
        assert_eq!(ctx.changed, vec![im::ATTR_ON_OFF]);

        // A command the cluster doesn't implement touches nothing.
        ctx.changed.clear();
        h.invoke(0x7F, &[], &mut ctx);
        assert!(ctx.changed.is_empty());
    }

    #[test]
    fn unknown_command_is_rejected() {
        let (mut h, _state) = OnOffHandler::new();
        assert_eq!(
            h.invoke(0x7F, &[], &mut InvokeCtx::default()),
            InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
        );
    }
}
