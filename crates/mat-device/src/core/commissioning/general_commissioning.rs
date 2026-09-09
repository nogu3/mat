//! General Commissioning command handlers (spec §11.10.6): ArmFailSafe /
//! SetRegulatoryConfig / CommissioningComplete, plus the fail-safe expiry
//! and uncommitted-fabric rollback they drive (spec §11.10.7.2).

use mat_controller::commissioning::{
    decode_arm_fail_safe, decode_set_regulatory_config, encode_commissioning_status_response,
};
use mat_controller::im;

use crate::core::datamodel::InvokeReply;
use crate::core::fabric_store::FabricEntry;

use super::{
    Inner, PendingCommissioning, RESP_ARM_FAIL_SAFE, RESP_COMMISSIONING_COMPLETE,
    RESP_SET_REGULATORY_CONFIG,
};

impl Inner {
    /// ArmFailSafe（spec §11.10.6.2）: records the timer. An
    /// `ExpiryLengthSeconds` of 0 disarms early (spec-legal way to release
    /// the fail-safe without waiting for it to expire). Either way — a
    /// fresh (re-)arm or an early disarm — the CSR/AddTrustedRoot material
    /// staged by a previous attempt is discarded (spec §11.10.7.2.1: that
    /// state must not survive a fail-safe transition without a completed
    /// `AddNOC`).
    pub(super) fn handle_arm_fail_safe(&mut self, fields_tlv: &[u8]) -> InvokeReply {
        let Ok((expiry_s, _breadcrumb)) = decode_arm_fail_safe(fields_tlv) else {
            return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
        };
        // spec §11.10.6.2: armed 中の非ゼロ ArmFailSafe は**タイマー延長**。
        // 進行中の試行の staged 素材（CSR/RCAC）と未確定 fabric は生かす —
        // matter.js は AddNOC 後に再アームしてから CASE で
        // CommissioningComplete を送るため、ここで巻き戻すと commissioning が
        // 完了できない（2026-08-18 実測）。zombie fabric（未確定のまま残る
        // AddNOC 結果, spec §11.10.7.2.1）の回収は fail-safe 満了
        // （`expire_fail_safe`）と早期 disarm（expiry=0）が担保する。
        // 未対応エッジ: 別セッションからの armed 中 ArmFailSafe は spec 上
        // BUSY を返すべきだが、セッション同一性の追跡は未実装（M3 で拾う）。
        let rolled_back = if expiry_s == 0 || !self.fail_safe.is_armed() {
            let rolled_back = self.rollback_uncommitted_fabric();
            self.pending = PendingCommissioning::default();
            rolled_back
        } else {
            None
        };
        if expiry_s == 0 {
            self.fail_safe.disarm();
        } else {
            self.fail_safe.arm(expiry_s);
        }
        // Debug-only, no behavior change (Echo interop observability): says
        // which of arm/disarm/re-arm this call took and whether it rolled
        // back a zombie fabric from a previous attempt.
        tracing::debug!(
            expiry_s,
            disarm = expiry_s == 0,
            rolled_back_fabric_index = ?rolled_back.as_ref().map(|e| e.fabric_index),
            "ArmFailSafe"
        );
        InvokeReply::Data {
            response_command: RESP_ARM_FAIL_SAFE,
            fields_tlv: encode_commissioning_status_response(0, ""),
        }
    }

    /// SetRegulatoryConfig（spec §11.10.6.4）: record-only, always succeeds
    /// once the fields decode (mat-device has no real regulatory table).
    pub(super) fn handle_set_regulatory_config(&mut self, fields_tlv: &[u8]) -> InvokeReply {
        if decode_set_regulatory_config(fields_tlv).is_err() {
            return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
        }
        InvokeReply::Data {
            response_command: RESP_SET_REGULATORY_CONFIG,
            fields_tlv: encode_commissioning_status_response(0, ""),
        }
    }

    /// CommissioningComplete（spec §11.10.6.6）: disarms the fail-safe and
    /// discards any staged CSR/AddTrustedRoot material — a completed
    /// commissioning has already consumed it via `AddNOC` (which clears
    /// `pending` itself on success), so nothing legitimate is lost.
    pub(super) fn handle_commissioning_complete(&mut self) -> InvokeReply {
        self.fail_safe.disarm();
        self.pending = PendingCommissioning::default();
        // The AddNOC (if any) that installed a fabric within this window is
        // now confirmed — clear the marker without removing anything.
        // Contrast `rollback_uncommitted_fabric`, which `expire_fail_safe`
        // and `handle_arm_fail_safe` use to discard an *unconfirmed* one.
        self.uncommitted_fabric_index = None;
        InvokeReply::Data {
            response_command: RESP_COMMISSIONING_COMPLETE,
            fields_tlv: encode_commissioning_status_response(0, ""),
        }
    }

    /// Removes whatever fabric `uncommitted_fabric_index` marks (if any) —
    /// a fabric `AddNOC` installed within the current/most-recent fail-safe
    /// window that hasn't yet been confirmed by `CommissioningComplete`.
    /// Shared by `expire_fail_safe` (the window's deadline passed) and
    /// `handle_arm_fail_safe` (a fresh attempt must not inherit the
    /// previous attempt's zombie fabric). Returns the removed entry, if
    /// any — only `expire_fail_safe`'s caller needs it (Task 8's runtime
    /// needs `fabric_id`/`node_id` off it for the mDNS goodbye before it's
    /// gone from the store).
    fn rollback_uncommitted_fabric(&mut self) -> Option<FabricEntry> {
        let fabric_index = self.uncommitted_fabric_index.take()?;
        let removed = self
            .store
            .entries()
            .iter()
            .find(|e| e.fabric_index == fabric_index)
            .cloned();
        // See `FabricStore::remove`'s doc comment for why a save failure
        // here is swallowed rather than propagated: the removal from
        // memory already happened either way, and there's no `InvokeReply`
        // to report it through at this call site (both callers are
        // fire-and-forget housekeeping, not itself the response to a
        // command).
        let _ = self.store.remove(fabric_index);
        if let Some(store) = &self.acl_store {
            store.purge_fabric(fabric_index);
        }
        if let Some(store) = &self.group_key_store {
            store.purge_fabric(fabric_index);
        }
        if let Some(store) = &self.group_membership_store {
            store.purge_fabric(fabric_index);
        }
        removed
    }

    /// spec §11.10.7.2: if the fail-safe's deadline has passed, rolls back
    /// whatever `AddNOC` installed during that window without a following
    /// `CommissioningComplete`. See `CommissioningServer::expire_fail_safe`
    /// (the public entry point this backs) for the full contract.
    pub(super) fn expire_fail_safe(&mut self) -> Option<FabricEntry> {
        if !self.fail_safe.is_expired() {
            return None;
        }
        self.fail_safe.disarm();
        self.pending = PendingCommissioning::default();
        self.rollback_uncommitted_fabric()
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::{
        admin_allowed, assert_fabric_state_purged, drive_invoke, expect_data, install_fabric,
        seed_fabric_state, server_with_stores, test_server,
    };
    use super::*;
    use mat_controller::commissioning::{
        decode_commissioning_status_response, decode_csr_response, decode_noc_response,
        encode_add_noc, encode_add_trusted_root, encode_arm_fail_safe, encode_attestation_request,
        encode_csr_request, parse_nocsr_elements, CommissioningFabric,
        CLUSTER_GENERAL_COMMISSIONING, CLUSTER_OPERATIONAL_CREDENTIALS, CMD_ADD_NOC,
        CMD_ADD_TRUSTED_ROOT, CMD_ARM_FAIL_SAFE, CMD_ATTESTATION_REQUEST,
        CMD_COMMISSIONING_COMPLETE, CMD_CSR_REQUEST,
    };
    use mat_controller::x509::parse_csr;

    #[test]
    fn arm_fail_safe_response_roundtrips() {
        let mut server = test_server();
        let (response_command, fields_tlv) = expect_data(drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(120, 7),
        ));
        assert_eq!(response_command, RESP_ARM_FAIL_SAFE);
        let (code, _text) = decode_commissioning_status_response(&fields_tlv).unwrap();
        assert_eq!(code, 0);
    }

    #[test]
    fn add_noc_rejected_without_fail_safe() {
        // AddNOC checks `is_armed()` before decoding its fields at all, so
        // a never-armed server rejects it outright — no valid CSR/NOC is
        // even reachable without a fail-safe window (CSRRequest/
        // AddTrustedRootCertificate are gated too, see
        // `csr_request_rejected_without_fail_safe` below).
        let mut server = test_server();
        let reply = drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ADD_NOC,
            &[],
        );
        assert_eq!(reply, InvokeReply::Status(im::STATUS_FAILSAFE_REQUIRED));
        assert!(server.fabrics().is_empty());
    }

    #[test]
    fn csr_request_rejected_without_fail_safe() {
        let mut server = test_server();
        let reply = drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CSR_REQUEST,
            &encode_csr_request(&[1u8; 32]),
        );
        assert_eq!(reply, InvokeReply::Status(im::STATUS_FAILSAFE_REQUIRED));
        assert!(server.pending_is_empty());
    }

    #[test]
    fn attestation_request_rejected_without_fail_safe() {
        let mut server = test_server();
        let reply = drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ATTESTATION_REQUEST,
            &encode_attestation_request(&[1u8; 32]),
        );
        assert_eq!(reply, InvokeReply::Status(im::STATUS_FAILSAFE_REQUIRED));
    }

    #[test]
    fn add_trusted_root_rejected_without_fail_safe() {
        let mut server = test_server();
        let reply = drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ADD_TRUSTED_ROOT,
            &encode_add_trusted_root(b"rcac"),
        );
        assert_eq!(reply, InvokeReply::Status(im::STATUS_FAILSAFE_REQUIRED));
        assert!(server.pending_is_empty());
    }

    #[test]
    fn disarm_clears_pending_commissioning_state() {
        let mut server = test_server();
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(120, 1),
        );
        drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CSR_REQUEST,
            &encode_csr_request(&[1u8; 32]),
        );
        assert!(!server.pending_is_empty());

        // ArmFailSafe(expiry=0) disarms early (spec-legal, spec §11.10.7.2.1)
        // — the CSR keypair staged above must not survive it.
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(0, 2),
        );
        assert!(server.pending_is_empty());
    }

    /// 同一窓内の再アームは同じ試行の延長なので staged CSR を維持する
    /// （matter.js の BLE フローは CSR と AddNOC の間でも定期再アームする、
    /// spec §11.10.6.2）。窓をまたいだ fresh arm（disarm 後の新規試行）では
    /// 前の試行の CSR keypair を持ち越してはいけない。
    #[test]
    fn rearm_keeps_pending_within_window_but_fresh_arm_resets_it() {
        let mut server = test_server();
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(120, 1),
        );
        drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CSR_REQUEST,
            &encode_csr_request(&[1u8; 32]),
        );
        assert!(!server.pending_is_empty());

        // 同一窓内の再アーム: staged CSR は生きたまま
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(120, 2),
        );
        assert!(!server.pending_is_empty());

        // disarm → fresh arm: 新規試行に前の CSR を持ち越さない
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(0, 3),
        );
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(120, 4),
        );
        assert!(server.pending_is_empty());
    }

    #[test]
    fn commissioning_complete_clears_pending_commissioning_state() {
        let mut server = test_server();
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(120, 1),
        );
        drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CSR_REQUEST,
            &encode_csr_request(&[1u8; 32]),
        );
        assert!(!server.pending_is_empty());
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_COMMISSIONING_COMPLETE,
            &[],
        );
        assert!(server.pending_is_empty());
    }

    #[test]
    fn commissioning_complete_disarms_fail_safe_so_add_noc_is_then_rejected() {
        let mut server = test_server();
        let fabric = CommissioningFabric::generate(0x1122, 0xAA).unwrap();

        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(120, 1),
        );
        let (_, csr_resp) = expect_data(drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CSR_REQUEST,
            &encode_csr_request(&[2u8; 32]),
        ));
        let (elements, _) = decode_csr_response(&csr_resp).unwrap();
        let (csr_der, _) = parse_nocsr_elements(&elements).unwrap();
        let device_pub = parse_csr(&csr_der).unwrap();
        let noc = fabric.issue_device_noc(&device_pub, 0x5001).unwrap();
        drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ADD_TRUSTED_ROOT,
            &encode_add_trusted_root(&fabric.rcac_tlv),
        );

        // CommissioningComplete disarms early — before AddNOC.
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_COMMISSIONING_COMPLETE,
            &[],
        );

        let reply = drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ADD_NOC,
            &encode_add_noc(&noc, &fabric.ipk_epoch, 0xAA, 0xFFF1),
        );
        assert_eq!(reply, InvokeReply::Status(im::STATUS_FAILSAFE_REQUIRED));
    }

    #[test]
    fn fail_safe_deadline_reflects_armed_state() {
        let mut server = test_server();
        assert!(server.fail_safe_deadline().is_none());

        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(120, 1),
        );
        assert!(server.fail_safe_deadline().is_some());

        // ExpiryLengthSeconds=0 disarms early (spec §11.10.7.2.1).
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(0, 2),
        );
        assert!(server.fail_safe_deadline().is_none());
    }

    #[test]
    fn fail_safe_expiry_rolls_back_uncommitted_fabric() {
        let (mut server, acl, gk, membership) = server_with_stores();
        install_fabric(&mut server, 0x1122, 0x5001);
        assert_eq!(server.fabrics().len(), 1);
        assert!(admin_allowed(&acl), "AddNOC installs the admin ACL entry");
        seed_fabric_state(&gk, &membership);

        server.force_expire_fail_safe();
        let removed = server.expire_fail_safe();
        assert_eq!(removed.map(|e| e.fabric_index), Some(1));
        assert!(server.fabrics().is_empty());
        assert!(server.fail_safe_deadline().is_none());
        assert_fabric_state_purged(&acl, &gk, &membership);

        // Idempotent: the marker and the timer are both already cleared.
        assert!(server.expire_fail_safe().is_none());
    }

    /// 早期 disarm（`ArmFailSafe(0)`）も満了と同じ rollback 経路: 未確定
    /// fabric と 3 store の状態が消える。
    #[test]
    fn arm_fail_safe_zero_rolls_back_uncommitted_fabric_and_purges_stores() {
        let (mut server, acl, gk, membership) = server_with_stores();
        install_fabric(&mut server, 0x1122, 0x5001);
        seed_fabric_state(&gk, &membership);

        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(0, 3),
        );
        assert!(server.fabrics().is_empty());
        assert!(server.fail_safe_deadline().is_none());
        assert_fabric_state_purged(&acl, &gk, &membership);
    }

    #[test]
    fn commissioning_complete_commits_the_fabric() {
        let mut server = test_server();
        install_fabric(&mut server, 0x1122, 0x5001);
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_COMMISSIONING_COMPLETE,
            &[],
        );

        // CommissioningComplete already disarmed, so there's no deadline to
        // expire — but even if there were, the fabric is confirmed, not
        // rolled back.
        assert!(server.expire_fail_safe().is_none());
        assert_eq!(server.fabrics().len(), 1);
    }

    /// spec §11.10.6.2: armed 中の非ゼロ ArmFailSafe は**タイマー延長**で
    /// あって新規試行の開始ではない。未確定 fabric を巻き戻してはいけない。
    /// 実測: matter.js（HA matter-server 1.1.7）は AddNOC 成功後に
    /// fail-safe を再アームしてから CASE を張り直し、その上で
    /// CommissioningComplete を送る。旧実装は再アームで fabric を
    /// 巻き戻してしまい、直後の Sigma1 が「destination id matched no
    /// fabric」で永遠に失敗していた（2026-08-18）。zombie fabric 対策は
    /// 満了時（`fail_safe_expiry_rolls_back_uncommitted_fabric`）と
    /// disarm 時の rollback が引き続き担保する。
    #[test]
    fn rearm_keeps_uncommitted_fabric_and_complete_commits_it() {
        let (mut server, acl, gk, membership) = server_with_stores();
        install_fabric(&mut server, 0x1122, 0x5001);
        seed_fabric_state(&gk, &membership);

        // AddNOC 後の再アーム（matter.js の Reconnect ステップ相当）
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(313, 2),
        );
        assert_eq!(
            server.fabrics().len(),
            1,
            "re-arm while armed must not roll back the pending fabric"
        );
        assert!(admin_allowed(&acl));
        assert!(gk.keyset_exists(1, 7));
        assert_eq!(membership.endpoints_for(1, 0x000A), vec![2]);

        // CASE 再接続後の CommissioningComplete で確定する
        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_COMMISSIONING_COMPLETE,
            &[],
        );
        assert!(server.expire_fail_safe().is_none());
        assert_eq!(server.fabrics().len(), 1);
    }

    /// 早期 disarm（expiry=0）は従来どおり未確定 fabric を巻き戻し、
    /// 解放された index は次の試行で再利用される。
    #[test]
    fn early_disarm_rolls_back_uncommitted_fabric() {
        let mut server = test_server();
        install_fabric(&mut server, 0x1122, 0x5001);
        assert_eq!(server.fabrics().len(), 1);

        drive_invoke(
            &mut server,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            &encode_arm_fail_safe(0, 2),
        );
        assert!(server.fabrics().is_empty());

        // The freed index (1) must be reusable, not skipped.
        let (_, resp) = expect_data(install_fabric(&mut server, 0x3344, 0x6002));
        let (status, fabric_index) = decode_noc_response(&resp).unwrap();
        assert_eq!(status, 0);
        assert_eq!(fabric_index, Some(1));
    }
}
