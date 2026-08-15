//! OnOff クラスタサーバ (spec §1.5, cluster 0x0006)。M2 スコープ: On/Off/
//! Toggle と OnOff 属性のみ（effect 付きコマンド・GlobalSceneControl 等は
//! スコープ外）。状態は Arc<AtomicBool> — net 側ランタイム/matv がクローン
//! を持ち、購読レポートとログに使う。
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use mat_controller::im;
use mat_controller::tlv::{Tag, Writer};

use crate::core::datamodel::{ClusterHandler, InvokeCtx, InvokeReply};

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

    fn read(&self, attribute: u32) -> Option<Vec<u8>> {
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
        self.state.store(new, Ordering::SeqCst);
        let _ = ctx; // Task 12 でここに変更通知が入る
        InvokeReply::Status(im::STATUS_SUCCESS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::datamodel::{ClusterHandler, InvokeCtx, InvokeReply};
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
        let tlv = h.read(im::ATTR_ON_OFF).unwrap();
        let mut r = mat_controller::tlv::Reader::new(&tlv);
        assert_eq!(
            r.next().unwrap().unwrap().value,
            mat_controller::tlv::Value::Bool(false)
        );
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
