//! Administrator Commissioning command handlers (spec §11.19.8):
//! OpenCommissioningWindow / RevokeCommissioning.

use mat_controller::commissioning::decode_open_commissioning_window;
use mat_controller::im;

use crate::core::datamodel::{InvokeCtx, InvokeReply};

use super::{
    AdminWindow, Inner, WindowRequest, AC_STATUS_BUSY, AC_STATUS_PAKE_PARAMETER_ERROR,
    AC_STATUS_WINDOW_NOT_OPEN,
};

impl Inner {
    /// OpenCommissioningWindow（spec §11.19.8.1, ECM — this server never
    /// serves the legacy basic-commissioning-method window）: validates the
    /// PAKE parameters, rejects if a window is already open, then records
    /// `admin_window` (for the AC attributes) and stages a `WindowRequest`
    /// for the runtime to turn into an actual PASE listener. Requires a
    /// timed invoke in the real protocol (spec §11.19.8.1) — enforced by
    /// the IM layer upstream of this handler, not re-checked here.
    pub(super) fn handle_open_commissioning_window(
        &mut self,
        fields_tlv: &[u8],
        ctx: &InvokeCtx,
    ) -> InvokeReply {
        let Ok((timeout_s, verifier, discriminator, iterations, salt)) =
            decode_open_commissioning_window(fields_tlv)
        else {
            return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
        };
        if !(180..=900).contains(&timeout_s) {
            return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
        }
        if verifier.len() != 97
            || !(1000..=100_000).contains(&iterations)
            || !(16..=32).contains(&salt.len())
        {
            return InvokeReply::ClusterStatus {
                status: im::STATUS_FAILURE,
                cluster_status: AC_STATUS_PAKE_PARAMETER_ERROR,
            };
        }
        if self.admin_window.is_some() {
            return InvokeReply::ClusterStatus {
                status: im::STATUS_FAILURE,
                cluster_status: AC_STATUS_BUSY,
            };
        }

        // AdminVendorID (spec §11.19.5.3) reflects the vendor id of the
        // fabric that opened this window — the vendor id the *invoking*
        // admin's own AddNOC recorded (`FabricEntry::admin_vendor_id`), not
        // anything from this command's own fields (OpenCommissioningWindow
        // carries no vendor id). Unassigned (0) if the invoking session's
        // fabric index isn't in the table — shouldn't happen for a CASE
        // session past AddNOC, but this handler doesn't assume it.
        let vendor_id = self
            .store
            .entries()
            .iter()
            .find(|e| e.fabric_index == ctx.fabric_index)
            .map_or(0, |e| e.admin_vendor_id);
        self.admin_window = Some(AdminWindow {
            fabric_index: ctx.fabric_index,
            vendor_id,
        });
        let verifier: [u8; 97] = verifier
            .try_into()
            .expect("verifier length already checked == 97 above");
        self.pending_window_request = Some(WindowRequest {
            verifier,
            discriminator,
            iterations,
            salt,
            timeout_s,
        });
        InvokeReply::Status(im::STATUS_SUCCESS)
    }

    /// RevokeCommissioning（spec §11.19.8.2）: closes the window if one is
    /// open, or `WindowNotOpen` if not. The actual PASE listener teardown
    /// happens in the net runtime (Task 4), which reads
    /// `admin_window_is_open()` after this dispatches to notice the
    /// closure — this handler only owns the AC attribute state.
    pub(super) fn handle_revoke_commissioning(&mut self) -> InvokeReply {
        if self.admin_window.is_none() {
            return InvokeReply::ClusterStatus {
                status: im::STATUS_FAILURE,
                cluster_status: AC_STATUS_WINDOW_NOT_OPEN,
            };
        }
        self.admin_window = None;
        InvokeReply::Status(im::STATUS_SUCCESS)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::{commissioned_server, test_ctx};
    use super::super::{
        ATTR_AC_ADMIN_FABRIC_INDEX, ATTR_AC_ADMIN_VENDOR_ID, ATTR_AC_WINDOW_STATUS,
    };
    use super::*;
    use crate::core::datamodel::ReadCtx;
    use mat_controller::commissioning::{
        encode_open_commissioning_window, CLUSTER_ADMIN_COMMISSIONING,
        CMD_OPEN_COMMISSIONING_WINDOW, CMD_REVOKE_COMMISSIONING,
    };
    use mat_controller::tlv::{Reader, Tag, Value, Writer};

    /// OCW 成功: WindowStatus=1、Admin 属性が呼び出し元 fabric を反映、
    /// WindowRequest が stage される。
    #[test]
    fn open_commissioning_window_stages_request_and_updates_attrs() {
        let server = commissioned_server(); // fabric_index=1 が入っている既存ヘルパ
        let material = [0x42u8; 97];
        let fields = encode_open_commissioning_window(300, &material, 0x0ABC, 1000, &[0x5A; 16]);
        let reply = server.invoke_command(
            CLUSTER_ADMIN_COMMISSIONING,
            CMD_OPEN_COMMISSIONING_WINDOW,
            &fields,
            &InvokeCtx {
                fabric_index: 1,
                ..test_ctx()
            },
        );
        assert_eq!(reply, InvokeReply::Status(im::STATUS_SUCCESS));
        let req = server.take_pending_window_request().expect("staged");
        assert_eq!(req.discriminator, 0x0ABC);
        assert_eq!(req.timeout_s, 300);
        assert_eq!(req.verifier, material);
        // 属性: WindowStatus=1(EnhancedWindowOpen), AdminFabricIndex=1,
        // AdminVendorId=登録済み fabric の admin_vendor_id(0xFFF1)
        let (_, _, ac) = server.into_cluster_handlers();
        let tlv = ac.read(ATTR_AC_WINDOW_STATUS, &ReadCtx::default()).unwrap();
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(1));
        let tlv = ac
            .read(ATTR_AC_ADMIN_FABRIC_INDEX, &ReadCtx::default())
            .unwrap();
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(1));
        let tlv = ac
            .read(ATTR_AC_ADMIN_VENDOR_ID, &ReadCtx::default())
            .unwrap();
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(0xFFF1));
    }

    /// 窓が既に開いていれば Busy(2)。
    #[test]
    fn open_commissioning_window_while_open_returns_busy() {
        let server = commissioned_server();
        let fields = encode_open_commissioning_window(300, &[0x42; 97], 0x0ABC, 1000, &[0x5A; 16]);
        let ctx = InvokeCtx {
            fabric_index: 1,
            ..test_ctx()
        };
        server.invoke_command(
            CLUSTER_ADMIN_COMMISSIONING,
            CMD_OPEN_COMMISSIONING_WINDOW,
            &fields,
            &ctx,
        );
        let reply = server.invoke_command(
            CLUSTER_ADMIN_COMMISSIONING,
            CMD_OPEN_COMMISSIONING_WINDOW,
            &fields,
            &ctx,
        );
        assert_eq!(
            reply,
            InvokeReply::ClusterStatus {
                status: im::STATUS_FAILURE,
                cluster_status: 2
            }
        );
    }

    /// パラメータ検証: verifier 長 ≠97 / iterations 範囲外(1000..=100000) /
    /// salt 長範囲外(16..=32) は PAKEParameterError(3)。timeout 範囲外
    /// (180..=900) は INVALID_COMMAND。
    #[test]
    fn open_commissioning_window_rejects_bad_parameters() {
        let server = commissioned_server();
        let ctx = InvokeCtx {
            fabric_index: 1,
            ..test_ctx()
        };
        let bad_iter = encode_open_commissioning_window(300, &[0x42; 97], 0x0ABC, 999, &[0x5A; 16]);
        assert_eq!(
            server.invoke_command(
                CLUSTER_ADMIN_COMMISSIONING,
                CMD_OPEN_COMMISSIONING_WINDOW,
                &bad_iter,
                &ctx
            ),
            InvokeReply::ClusterStatus {
                status: im::STATUS_FAILURE,
                cluster_status: 3
            }
        );
        let bad_salt = encode_open_commissioning_window(300, &[0x42; 97], 0x0ABC, 1000, &[0x5A; 8]);
        assert_eq!(
            server.invoke_command(
                CLUSTER_ADMIN_COMMISSIONING,
                CMD_OPEN_COMMISSIONING_WINDOW,
                &bad_salt,
                &ctx
            ),
            InvokeReply::ClusterStatus {
                status: im::STATUS_FAILURE,
                cluster_status: 3
            }
        );
        let bad_timeout =
            encode_open_commissioning_window(60, &[0x42; 97], 0x0ABC, 1000, &[0x5A; 16]);
        assert_eq!(
            server.invoke_command(
                CLUSTER_ADMIN_COMMISSIONING,
                CMD_OPEN_COMMISSIONING_WINDOW,
                &bad_timeout,
                &ctx
            ),
            InvokeReply::Status(im::STATUS_INVALID_COMMAND)
        );

        // `encode_open_commissioning_window` takes `verifier: &[u8; 97]`, so
        // a wrong-length verifier can't be produced through it — build the
        // fields TLV directly (same technique as
        // `add_noc_accepts_empty_icac_value_as_absent`) with a 96-byte
        // verifier to exercise the third PAKEParameterError disjunct
        // (verifier.len() != 97) independently of iterations/salt.
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(0), 300);
        w.put_bytes(Tag::Context(1), &[0x42; 96]);
        w.put_uint(Tag::Context(2), 0x0ABC);
        w.put_uint(Tag::Context(3), 1000);
        w.put_bytes(Tag::Context(4), &[0x5A; 16]);
        w.end_container();
        let bad_verifier = w.finish();
        assert_eq!(
            server.invoke_command(
                CLUSTER_ADMIN_COMMISSIONING,
                CMD_OPEN_COMMISSIONING_WINDOW,
                &bad_verifier,
                &ctx
            ),
            InvokeReply::ClusterStatus {
                status: im::STATUS_FAILURE,
                cluster_status: 3
            }
        );
    }

    /// Revoke: 開いていれば閉じ、閉じていれば WindowNotOpen(4)。
    #[test]
    fn revoke_commissioning_closes_or_rejects() {
        let server = commissioned_server();
        let ctx = InvokeCtx {
            fabric_index: 1,
            ..test_ctx()
        };
        assert_eq!(
            server.invoke_command(
                CLUSTER_ADMIN_COMMISSIONING,
                CMD_REVOKE_COMMISSIONING,
                &[],
                &ctx
            ),
            InvokeReply::ClusterStatus {
                status: im::STATUS_FAILURE,
                cluster_status: 4
            }
        );
        let fields = encode_open_commissioning_window(300, &[0x42; 97], 0x0ABC, 1000, &[0x5A; 16]);
        server.invoke_command(
            CLUSTER_ADMIN_COMMISSIONING,
            CMD_OPEN_COMMISSIONING_WINDOW,
            &fields,
            &ctx,
        );
        assert_eq!(
            server.invoke_command(
                CLUSTER_ADMIN_COMMISSIONING,
                CMD_REVOKE_COMMISSIONING,
                &[],
                &ctx
            ),
            InvokeReply::Status(im::STATUS_SUCCESS)
        );
        // 閉じた後の属性は WindowStatus=0 / Admin* は null
        let (_, _, ac) = server.into_cluster_handlers();
        let tlv = ac.read(ATTR_AC_WINDOW_STATUS, &ReadCtx::default()).unwrap();
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(0));
        let tlv = ac
            .read(ATTR_AC_ADMIN_FABRIC_INDEX, &ReadCtx::default())
            .unwrap();
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Null);
        let tlv = ac
            .read(ATTR_AC_ADMIN_VENDOR_ID, &ReadCtx::default())
            .unwrap();
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Null);
    }
}
