//! Node Operational Credentials — the fabric-table half (spec §11.17.6.8,
//! §11.17.6.11, §11.17.6.12): AddNOC / UpdateFabricLabel / RemoveFabric.

use mat_controller::cert::{verify_noc_chain, MatterCert};
use mat_controller::commissioning::{
    decode_add_noc, decode_remove_fabric, decode_update_fabric_label, encode_noc_response,
};
use mat_controller::fabric::{compressed_fabric_id, derive_ipk_operational};
use mat_controller::im;

use crate::core::datamodel::{InvokeCtx, InvokeReply};
use crate::core::fabric_store::FabricEntry;

use super::{
    Inner, PendingCommissioning, NOC_STATUS_INVALID_ADMIN_SUBJECT, NOC_STATUS_INVALID_FABRIC_INDEX,
    NOC_STATUS_INVALID_NOC, NOC_STATUS_INVALID_PUBLIC_KEY, NOC_STATUS_MISSING_CSR, NOC_STATUS_OK,
    NOC_STATUS_TABLE_FULL, RESP_NOC, SUPPORTED_FABRICS,
};

impl Inner {
    /// AddNOC（spec §11.17.6.13）: verifies the NOC's chain against the
    /// staged RCAC, cross-checks its public key against the staged CSR
    /// keypair, derives the operational IPK, and installs a `FabricEntry`.
    /// Requires the fail-safe to be armed (spec: most General/Operational
    /// Credentials commissioning commands do; `mat-device` enforces it here
    /// since `AddNOC` is the one with externally observable side effects —
    /// a fabric actually gets installed).
    pub(super) fn handle_add_noc(&mut self, fields_tlv: &[u8]) -> InvokeReply {
        if !self.fail_safe.is_armed() {
            return InvokeReply::Status(im::STATUS_FAILSAFE_REQUIRED);
        }
        let Ok((noc_tlv, icac_tlv, ipk_epoch, case_admin_subject, admin_vendor_id)) =
            decode_add_noc(fields_tlv)
        else {
            return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
        };
        // matter.js 系コントローラ（HA matter-server 等）は「ICAC なし」を
        // フィールド省略ではなく空バイト列の ICACValue で表現する。chip 本家の
        // デバイス実装も空 ICAC は「無し」扱いなので、ここで正規化する。
        let icac_tlv = icac_tlv.filter(|v| !v.is_empty());

        let noc_status = |status: u8| InvokeReply::Data {
            response_command: RESP_NOC,
            fields_tlv: encode_noc_response(status, None),
        };

        // spec §11.17.6.13.1: past `SupportedFabrics` the answer is
        // `TableFull`, and it comes before any of the certificate checks —
        // there is no point verifying a chain for a fabric that has nowhere
        // to go, and the capacity is the same number the `SupportedFabrics`
        // attribute reports.
        if self.store.entries().len() >= usize::from(SUPPORTED_FABRICS) {
            tracing::debug!(
                supported_fabrics = SUPPORTED_FABRICS,
                "AddNOC rejected: TableFull"
            );
            return noc_status(NOC_STATUS_TABLE_FULL);
        }

        // spec §11.17.6.8.1: the admin subject must be a real operational
        // node id or a CAT with a non-zero version — anything else would
        // install an Administer ACL entry nobody can ever match (a fabric
        // whose only admin is locked out). Checked before the certificate
        // work, like `TableFull`: no point verifying a chain for a fabric
        // that can't be administered. `pending` is left intact so the same
        // session can retry with a valid subject.
        if crate::core::access_control::subject_kind(case_admin_subject).is_none() {
            tracing::debug!(
                reason = "invalid admin subject",
                case_admin_subject = format_args!("{case_admin_subject:#x}"),
                "AddNOC rejected: InvalidAdminSubject"
            );
            return noc_status(NOC_STATUS_INVALID_ADMIN_SUBJECT);
        }

        let (Some(root_tlv), Some(op_private_key), Some(op_public_key)) = (
            self.pending.trusted_root_tlv.clone(),
            self.pending.op_private_key,
            self.pending.op_public_key,
        ) else {
            return noc_status(NOC_STATUS_MISSING_CSR);
        };

        // InvalidNOC は複数分岐から返る。どの検証で落ちたかは応答からは
        // 区別できないため、拒否時は理由と素材（TLV hex）を debug ログに残す。
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        let rcac = match MatterCert::parse(&root_tlv) {
            Ok(cert) => cert,
            Err(e) => {
                tracing::debug!(reason = "rcac parse", error = %e, rcac_tlv = %hex(&root_tlv), "AddNOC rejected: InvalidNOC");
                return noc_status(NOC_STATUS_INVALID_NOC);
            }
        };
        let noc = match MatterCert::parse(&noc_tlv) {
            Ok(cert) => cert,
            Err(e) => {
                tracing::debug!(reason = "noc parse", error = %e, noc_tlv = %hex(&noc_tlv), "AddNOC rejected: InvalidNOC");
                return noc_status(NOC_STATUS_INVALID_NOC);
            }
        };
        let icac = match icac_tlv.as_deref().map(MatterCert::parse) {
            Some(Ok(cert)) => Some(cert),
            Some(Err(e)) => {
                tracing::debug!(reason = "icac parse", error = %e, icac_tlv = %hex(icac_tlv.as_deref().unwrap_or_default()), "AddNOC rejected: InvalidNOC");
                return noc_status(NOC_STATUS_INVALID_NOC);
            }
            None => None,
        };
        if let Err(e) = verify_noc_chain(&noc, icac.as_ref(), &rcac) {
            tracing::debug!(reason = "chain verify", error = %e, has_icac = icac.is_some(), noc_tlv = %hex(&noc_tlv), rcac_tlv = %hex(&root_tlv), "AddNOC rejected: InvalidNOC");
            return noc_status(NOC_STATUS_INVALID_NOC);
        }
        if noc.pub_key != op_public_key {
            tracing::debug!(
                reason = "public key mismatch",
                "AddNOC rejected: InvalidPublicKey"
            );
            return noc_status(NOC_STATUS_INVALID_PUBLIC_KEY);
        }
        let (Some(node_id), Some(fabric_id)) = (noc.node_id(), noc.fabric_id()) else {
            tracing::debug!(reason = "node/fabric id missing", node_id = ?noc.node_id(), fabric_id = ?noc.fabric_id(), noc_tlv = %hex(&noc_tlv), "AddNOC rejected: InvalidNOC");
            return noc_status(NOC_STATUS_INVALID_NOC);
        };

        let cfid = compressed_fabric_id(&rcac.pub_key, fabric_id);
        let ipk_operational = derive_ipk_operational(&ipk_epoch, &cfid);
        let fabric_index = self.store.next_fabric_index();
        let entry = FabricEntry {
            fabric_index,
            root_tlv,
            noc_tlv,
            icac_tlv,
            op_private_key,
            ipk_operational,
            node_id,
            fabric_id,
            root_public_key: rcac.pub_key,
            admin_subject: case_admin_subject,
            admin_vendor_id,
            label: String::new(),
        };
        if self.store.insert(entry).is_err() {
            return noc_status(NOC_STATUS_TABLE_FULL);
        }

        // Prerequisite steps are one-shot: a second AddNOC on this session
        // must re-CSR / re-AddTrustedRoot, not silently reuse stale material.
        self.pending = PendingCommissioning::default();
        // Installed, but not yet confirmed — `handle_commissioning_complete`
        // clears this marker on success; a fail-safe expiry or a fresh/early
        // `ArmFailSafe` before that rolls the fabric back (spec §11.10.7.2).
        self.uncommitted_fabric_index = Some(fabric_index);

        // spec §11.17.6.8: AddNOC installs an automatic ACL entry granting
        // Administer privilege to the CASE admin subject — without it, the
        // commissioner that just wrote this fabric could never read/write
        // its own ACL again (nothing on the device would authorize it to).
        if let Some(store) = &self.acl_store {
            store.add_case_admin(fabric_index, case_admin_subject);
        }

        InvokeReply::Data {
            response_command: RESP_NOC,
            fields_tlv: encode_noc_response(NOC_STATUS_OK, Some(fabric_index)),
        }
    }

    /// UpdateFabricLabel（spec §11.17.6.11）: fabric-scoped — always targets
    /// the *invoking session's own* fabric (`ctx.fabric_index`), never a
    /// fabric index named in the command fields (the command has none; only
    /// `RemoveFabric` takes an explicit `FabricIndex`). No fail-safe
    /// requirement, unlike `AddNOC` — this runs against a fabric that's
    /// already fully commissioned.
    pub(super) fn handle_update_fabric_label(
        &mut self,
        fields_tlv: &[u8],
        ctx: &InvokeCtx,
    ) -> InvokeReply {
        let Ok(label) = decode_update_fabric_label(fields_tlv) else {
            return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
        };

        let fabric_index = ctx.fabric_index;
        // `update_label` itself distinguishes "no such fabric" (`Ok(false)`)
        // from "found it, but the write-through to disk failed" (`Err`) —
        // branch on that directly instead of a separate existence pre-scan
        // (the pre-scan and `update_label`'s own lookup would otherwise walk
        // `self.store.entries()` twice to answer the same question).
        match self.store.update_label(fabric_index, label) {
            Ok(true) => InvokeReply::Data {
                response_command: RESP_NOC,
                fields_tlv: encode_noc_response(NOC_STATUS_OK, Some(fabric_index)),
            },
            Ok(false) => InvokeReply::Data {
                response_command: RESP_NOC,
                fields_tlv: encode_noc_response(NOC_STATUS_INVALID_FABRIC_INDEX, None),
            },
            // No `NodeOperationalCertStatusEnum` member means "storage
            // write error" (`AddNOC` fakes one with `NOC_STATUS_TABLE_FULL`
            // — that borrowed meaning doesn't fit here without misleading
            // the caller about *which* fabric failed). The global `FAILURE`
            // status is the honest signal instead: the label already applies
            // to the in-memory table (so the next `Fabrics` read reflects it
            // regardless of this branch), but the caller must not be told
            // `OK` when the change may not survive a restart.
            Err(e) => {
                tracing::debug!(error = %e, fabric_index, "UpdateFabricLabel: persist failed");
                InvokeReply::Status(im::STATUS_FAILURE)
            }
        }
    }

    /// RemoveFabric（spec §11.17.6.15）: unlike `UpdateFabricLabel`, targets
    /// the `FabricIndex` named in the command's own fields, not necessarily
    /// the invoking session's fabric — an administrator (or, per this
    /// branch's motivating case, a commissioner phone that just handed a
    /// device off via `OpenCommissioningWindow`) can remove any fabric,
    /// including its own. Looks the entry up (and clones it) *before*
    /// calling `store.remove` — `FabricStore::remove` only reports whether
    /// something was removed, not what it was, and the runtime needs the
    /// full entry (root public key, fabric id, node id) to retract the
    /// mDNS operational advert and decide whether to drop the session (see
    /// `removed_fabric`'s doc comment). Mirrors
    /// `rollback_uncommitted_fabric`'s find-then-remove shape.
    pub(super) fn handle_remove_fabric(&mut self, fields_tlv: &[u8]) -> InvokeReply {
        let Ok(fabric_index) = decode_remove_fabric(fields_tlv) else {
            return InvokeReply::Status(im::STATUS_INVALID_COMMAND);
        };

        let Some(entry) = self
            .store
            .entries()
            .iter()
            .find(|e| e.fabric_index == fabric_index)
            .cloned()
        else {
            return InvokeReply::Data {
                response_command: RESP_NOC,
                fields_tlv: encode_noc_response(NOC_STATUS_INVALID_FABRIC_INDEX, None),
            };
        };

        match self.store.remove(fabric_index) {
            Ok(true) => {
                self.removed_fabric = Some(entry);
                self.purge_fabric_stores(fabric_index);
                InvokeReply::Data {
                    response_command: RESP_NOC,
                    fields_tlv: encode_noc_response(NOC_STATUS_OK, Some(fabric_index)),
                }
            }
            // `entry` was just found above under the same `&mut self`
            // borrow — nothing can have removed it out from under us
            // between the two calls, so `remove` reporting "wasn't there"
            // here would mean the two lookups disagree, which can't happen.
            Ok(false) => unreachable!(
                "fabric_index {fabric_index} found in store.entries() moments ago, \
                 store.remove() must still find it"
            ),
            // Same asymmetry as `UpdateFabricLabel`: the in-memory removal
            // already happened (`FabricStore::remove`'s doc comment — it
            // does *not* roll the removal back on a save failure), so the
            // fabric is really gone from this device's perspective even
            // though the reply below is `STATUS_FAILURE` rather than a
            // success `NOCResponse` — the caller must not be told `OK` when
            // the removal may not survive a restart. `removed_fabric` is
            // still staged, though (unlike the reply, which must stay
            // honest about durability): the runtime's mDNS retract and
            // same-session drop must follow the in-memory truth, not the
            // reply. Leaving it unstaged would strand a stale operational
            // advert and, worse, leave a session alive against a fabric
            // that no longer resolves — spec §2.5.11 requires removing a
            // fabric to terminate its sessions, and that's true regardless
            // of whether the removal also made it to disk. Same reasoning
            // forces the ACL purge here too (precedent: 31f4b44 did this
            // for `removed_fabric` itself) — `next_fabric_index` reissues a
            // removed index as `max(existing)+1`, so an unpurged entry here
            // would apply the previous occupant's ACL to whatever
            // unrelated fabric a later `AddNOC` installs at that index
            // (cross-fabric ACL leak), regardless of whether this removal
            // made it to disk.
            Err(e) => {
                tracing::debug!(error = %e, fabric_index, "RemoveFabric: persist failed");
                self.removed_fabric = Some(entry);
                self.purge_fabric_stores(fabric_index);
                InvokeReply::Status(im::STATUS_FAILURE)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::{
        commissioned_server, drive_invoke, expect_data, install_fabric, install_fabric_with_admin,
        read_ctx, test_ctx, test_server,
    };
    use super::super::{CommissioningServer, ATTR_OC_FABRICS};
    use super::*;
    use crate::core::access_control::AclStore;
    use crate::core::datamodel::{ClusterHandler, ReadCtx};
    use crate::core::fabric_store::{FabricPersist, FabricStore};
    use crate::core::group_key_management::GroupKeyStore;
    use crate::core::group_membership::GroupMembershipStore;
    use mat_controller::commissioning::{
        decode_csr_response, decode_noc_response, encode_add_noc, encode_add_trusted_root,
        encode_arm_fail_safe, encode_csr_request, encode_remove_fabric, encode_update_fabric_label,
        parse_nocsr_elements, CommissioningFabric, CLUSTER_GENERAL_COMMISSIONING,
        CLUSTER_OPERATIONAL_CREDENTIALS, CMD_ADD_NOC, CMD_ADD_TRUSTED_ROOT, CMD_ARM_FAIL_SAFE,
        CMD_CSR_REQUEST, CMD_REMOVE_FABRIC, CMD_UPDATE_FABRIC_LABEL,
    };
    use mat_controller::tlv::{Reader, Tag, Value, Writer};
    use mat_controller::x509::{generate_dev_attestation, parse_csr};

    #[test]
    fn add_noc_installs_fabric() {
        let mut server = test_server();
        let (_, resp) = expect_data(install_fabric(&mut server, 0x1122, 0x5001));
        let (status, fabric_index) = decode_noc_response(&resp).unwrap();
        assert_eq!(status, 0);
        assert_eq!(fabric_index, Some(1));
        assert_eq!(server.fabrics().len(), 1);
        assert_eq!(server.fabrics()[0].node_id, 0x5001);
        assert_eq!(server.fabrics()[0].fabric_id, 0x1122);
        assert_eq!(server.fabrics()[0].admin_vendor_id, 0xFFF1);
    }

    /// matter.js（Home Assistant の matter-server 等）は「ICAC なし」を
    /// フィールド省略ではなく**空バイト列の ICACValue** で送る（chip-tool は
    /// 省略）。chip 本家のデバイス実装も空 ICAC は「無し」として扱うため、
    /// 空 ICACValue 付きの AddNOC は省略時と同様に成功しなければならない。
    /// 実測: HA matter-server 1.1.7 からの commissioning が本ケースで
    /// InvalidNOC(3) になり中断していた（2026-08-18）。
    #[test]
    fn add_noc_accepts_empty_icac_value_as_absent() {
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
            &encode_csr_request(&[3u8; 32]),
        ));
        let (elements, _sig) = decode_csr_response(&csr_resp).unwrap();
        let (csr_der, _nonce) = parse_nocsr_elements(&elements).unwrap();
        let device_pub = parse_csr(&csr_der).unwrap();
        let noc = fabric.issue_device_noc(&device_pub, 0x5001).unwrap();
        assert_eq!(
            drive_invoke(
                &mut server,
                CLUSTER_OPERATIONAL_CREDENTIALS,
                CMD_ADD_TRUSTED_ROOT,
                &encode_add_trusted_root(&fabric.rcac_tlv),
            ),
            InvokeReply::Status(im::STATUS_SUCCESS)
        );

        // AddNOC {0: NOCValue, 1: ICACValue(空), 2: IPKValue, 3:
        // CaseAdminSubject, 4: AdminVendorId} — encode_add_noc は ICAC を
        // 書かないので、matter.js が送る形をここで直接組み立てる。
        let mut w = Writer::new();
        w.start_struct(Tag::Anonymous);
        w.put_bytes(Tag::Context(0), &noc);
        w.put_bytes(Tag::Context(1), &[]);
        w.put_bytes(Tag::Context(2), &fabric.ipk_epoch);
        w.put_uint(Tag::Context(3), 0xAA);
        w.put_uint(Tag::Context(4), 0xFFF1);
        w.end_container();

        let (_, resp) = expect_data(drive_invoke(
            &mut server,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_ADD_NOC,
            &w.finish(),
        ));
        let (status, fabric_index) = decode_noc_response(&resp).unwrap();
        assert_eq!(status, 0, "empty ICACValue must be treated as absent");
        assert_eq!(fabric_index, Some(1));
        assert_eq!(server.fabrics().len(), 1);
    }

    /// UpdateFabricLabel: NOCResponse(Ok) を返し、store に永続化され、
    /// Fabrics 属性の読みに Label が反映される。
    #[test]
    fn update_fabric_label_persists_and_reflects_in_fabrics_attr() {
        let server = commissioned_server(); // fabric_index=1
        let fields = encode_update_fabric_label("Alexa-1");
        let ctx = InvokeCtx {
            fabric_index: 1,
            ..test_ctx()
        };
        let (_, resp) = expect_data(server.invoke_command(
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_UPDATE_FABRIC_LABEL,
            &fields,
            &ctx,
        ));
        let (status, fabric_index) = decode_noc_response(&resp).unwrap();
        assert_eq!(status, 0);
        assert_eq!(fabric_index, Some(1));
        assert_eq!(server.fabrics()[0].label, "Alexa-1");

        // Fabrics 属性（ATTR_OC_FABRICS）の TLV に "Alexa-1" が Label
        // フィールド（context tag 5, spec §11.17.5.20 FabricDescriptorStruct）
        // として現れることを Reader で確認する。
        let (_, oc, _) = server.into_cluster_handlers();
        let fabrics_tlv = oc
            .read(ATTR_OC_FABRICS, &ReadCtx::unfiltered(0))
            .expect("Fabrics");
        let mut r = Reader::new(&fabrics_tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::ArrayStart);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let mut label = None;
        loop {
            let el = r
                .next()
                .unwrap()
                .expect("truncated FabricDescriptor struct");
            match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(5), Value::Utf8(s)) => label = Some(s.to_string()),
                _ => {}
            }
        }
        assert_eq!(label.as_deref(), Some("Alexa-1"));
    }

    /// 対象は「呼び出しセッションの fabric」（spec: fabric-scoped コマンド）。
    /// ctx.fabric_index の fabric が存在しなければ InvalidFabricIndex(0x0A)。
    #[test]
    fn update_fabric_label_unknown_fabric_returns_invalid_fabric_index() {
        let server = test_server(); // fabric なし
        let fields = encode_update_fabric_label("x");
        let ctx = InvokeCtx {
            fabric_index: 7,
            ..test_ctx()
        };
        let (_, resp) = expect_data(server.invoke_command(
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_UPDATE_FABRIC_LABEL,
            &fields,
            &ctx,
        ));
        let (status, _) = decode_noc_response(&resp).unwrap();
        assert_eq!(status, 0x0A);
    }

    /// Label 長 >32 は INVALID_COMMAND（グローバルステータス）。
    #[test]
    fn update_fabric_label_too_long_returns_invalid_command() {
        let server = commissioned_server();
        let fields = encode_update_fabric_label(&"x".repeat(33));
        let ctx = InvokeCtx {
            fabric_index: 1,
            ..test_ctx()
        };
        let reply = server.invoke_command(
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_UPDATE_FABRIC_LABEL,
            &fields,
            &ctx,
        );
        assert_eq!(reply, InvokeReply::Status(im::STATUS_INVALID_COMMAND));
    }

    /// A `FabricPersist` whose `save` can be toggled to fail after
    /// construction — same shape as `fabric_store`'s own `FlakySavePersist`
    /// (that one is private to `fabric_store`'s test module, so this is a
    /// separate copy). `load` always returns empty; tests using this get
    /// their one fabric in via `install_fabric` (which needs `save` to
    /// succeed) before flipping `fail_save` on.
    struct FlakySavePersist {
        fail_save: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl FabricPersist for FlakySavePersist {
        fn save(&self, _entries: &[FabricEntry]) -> Result<(), String> {
            if self.fail_save.load(std::sync::atomic::Ordering::SeqCst) {
                Err("disk full".to_string())
            } else {
                Ok(())
            }
        }
        fn load(&self) -> Result<Vec<FabricEntry>, String> {
            Ok(Vec::new())
        }
    }

    /// UpdateFabricLabel: a persist failure must not be reported as
    /// `NOCResponse(OK)` — the caller has no other signal that the label
    /// change won't survive a restart. `update_label`'s in-memory write
    /// (`FabricStore::update_label` sets the field before attempting the
    /// save) still applies regardless — same asymmetry `FabricStore::remove`
    /// already documents for the fail-safe-rollback path — but the *reply*
    /// must be honest, so this asserts the global `STATUS_FAILURE`, not a
    /// success `NOCResponse`.
    #[test]
    fn update_fabric_label_persist_failure_returns_status_failure() {
        let fail_save = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let store = FabricStore::with_persist(Box::new(FlakySavePersist {
            fail_save: std::sync::Arc::clone(&fail_save),
        }));
        let dev = generate_dev_attestation(0xFFF1, 0x8000).unwrap();
        let mut server = CommissioningServer::new(dev, store);
        install_fabric(&mut server, 0x1122, 0x5001);

        fail_save.store(true, std::sync::atomic::Ordering::SeqCst);
        let fields = encode_update_fabric_label("Alexa-1");
        let ctx = InvokeCtx {
            fabric_index: 1,
            ..test_ctx()
        };
        let reply = server.invoke_command(
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_UPDATE_FABRIC_LABEL,
            &fields,
            &ctx,
        );
        assert_eq!(reply, InvokeReply::Status(im::STATUS_FAILURE));
        // In-memory table still got the label — the reply is what must not
        // lie, not the (already-established) in-memory-vs-disk asymmetry.
        assert_eq!(server.fabrics()[0].label, "Alexa-1");
    }

    /// RemoveFabric: NOCResponse(Ok) + store から消え、removed が stage される。
    #[test]
    fn remove_fabric_removes_and_stages_entry() {
        let server = commissioned_server();
        let ctx = InvokeCtx {
            fabric_index: 1,
            ..test_ctx()
        };
        let (_, resp) = expect_data(server.invoke_command(
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_REMOVE_FABRIC,
            &encode_remove_fabric(1),
            &ctx,
        ));
        let (status, fabric_index) = decode_noc_response(&resp).unwrap();
        assert_eq!(status, 0);
        assert_eq!(fabric_index, Some(1));
        assert!(server.fabrics().is_empty());
        assert_eq!(
            server.take_removed_fabric().map(|e| e.fabric_index),
            Some(1)
        );
    }

    /// AddNOC が case admin subject に Administer の自動 ACL エントリを
    /// 発行し、その後の RemoveFabric がそのエントリを purge することを
    /// `set_acl_store` で配線した `AclStore` 越しに検証する
    /// （Task 3 rulings）。あわせて `set_group_key_store` で配線した
    /// `GroupKeyStore` の KeySet/GroupKeyMap も同じ RemoveFabric で
    /// purge されることを確認する（Task 2、`handle_remove_fabric`の
    /// 成功径路 = purge 3箇所のうちの1つ）。`set_group_membership_store`
    /// で配線した `GroupMembershipStore` の membership も同じ経路で purge
    /// される（groupcast レーン A フェーズ 2 Task 2）。
    #[test]
    fn add_noc_installs_case_admin_acl_and_remove_fabric_purges_it() {
        use crate::core::access_control::{decode_entries_for_test, AccessControlHandler};

        let dev = generate_dev_attestation(0xFFF1, 0x8000).unwrap();
        let mut server = CommissioningServer::new(dev, FabricStore::new());
        let acl_store = AclStore::new();
        server.set_acl_store(acl_store.clone());
        let gk_store = GroupKeyStore::new();
        server.set_group_key_store(gk_store.clone());
        let membership = GroupMembershipStore::new();
        server.set_group_membership_store(membership.clone());

        // install_fabric drives ArmFailSafe/CSR/AddTrustedRoot/AddNOC with
        // admin subject fixed at 0xAA (see its doc comment).
        install_fabric(&mut server, 0x1122, 0x5001);

        let handler = AccessControlHandler::new(acl_store.clone());
        let entries = decode_entries_for_test(&handler.read(im::ATTR_ACL, &read_ctx(1)).unwrap());
        assert_eq!(entries, vec![(5u8, 2u8, vec![0xAAu64], 1u8)]);

        // GroupKeyStore side of the same fabric: a KeySet and a GroupKeyMap
        // entry, both scoped to fabric_index 1.
        gk_store.upsert_keyset(1, 7, [9u8; 16], 0).unwrap();
        gk_store.replace_fabric_map(1, vec![(0x000A, 7)]);
        assert!(gk_store.keyset_exists(1, 7));
        assert_eq!(gk_store.map_entries_for(1), vec![(0x000A, 7)]);

        // GroupMembershipStore side of the same fabric.
        membership.add(1, 10, 2).unwrap();

        let ctx = InvokeCtx {
            fabric_index: 1,
            ..test_ctx()
        };
        let (_, resp) = expect_data(server.invoke_command(
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_REMOVE_FABRIC,
            &encode_remove_fabric(1),
            &ctx,
        ));
        let (status, _) = decode_noc_response(&resp).unwrap();
        assert_eq!(status, 0);

        let entries = decode_entries_for_test(&handler.read(im::ATTR_ACL, &read_ctx(1)).unwrap());
        assert!(entries.is_empty());
        assert!(!gk_store.keyset_exists(1, 7));
        assert!(gk_store.map_entries_for(1).is_empty());
        assert!(
            membership.groups_by_fabric().is_empty(),
            "purge must drop the fabric's memberships"
        );
    }

    /// 存在しない index は InvalidFabricIndex(0x0A)。
    #[test]
    fn remove_fabric_unknown_index_returns_invalid_fabric_index() {
        let server = commissioned_server();
        let ctx = InvokeCtx {
            fabric_index: 1,
            ..test_ctx()
        };
        let (_, resp) = expect_data(server.invoke_command(
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_REMOVE_FABRIC,
            &encode_remove_fabric(9),
            &ctx,
        ));
        let (status, _) = decode_noc_response(&resp).unwrap();
        assert_eq!(status, 0x0A);
        assert_eq!(server.fabrics().len(), 1);
    }

    /// RemoveFabric: a persist failure must still stage `removed_fabric`
    /// for the runtime — `FabricStore::remove`'s in-memory removal already
    /// happened (same asymmetry `update_fabric_label_persist_failure_
    /// returns_status_failure` documents for `update_label`), so the mDNS
    /// retract and (if it were this session's own fabric) session drop
    /// must follow that in-memory truth, not the reply. The reply itself
    /// still stays honest (`STATUS_FAILURE`, not a success `NOCResponse`).
    #[test]
    fn remove_fabric_persist_failure_still_stages_removal() {
        let fail_save = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let store = FabricStore::with_persist(Box::new(FlakySavePersist {
            fail_save: std::sync::Arc::clone(&fail_save),
        }));
        let dev = generate_dev_attestation(0xFFF1, 0x8000).unwrap();
        let mut server = CommissioningServer::new(dev, store);
        install_fabric(&mut server, 0x1122, 0x5001);

        fail_save.store(true, std::sync::atomic::Ordering::SeqCst);
        let ctx = InvokeCtx {
            fabric_index: 1,
            ..test_ctx()
        };
        let reply = server.invoke_command(
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_REMOVE_FABRIC,
            &encode_remove_fabric(1),
            &ctx,
        );
        assert_eq!(reply, InvokeReply::Status(im::STATUS_FAILURE));
        assert!(server.fabrics().is_empty());
        assert_eq!(
            server.take_removed_fabric().map(|e| e.fabric_index),
            Some(1)
        );
    }

    /// RemoveFabric: a persist failure must still purge the removed
    /// fabric's ACL entries — `FabricStore::remove`'s in-memory removal
    /// already happened (same asymmetry the sibling
    /// `remove_fabric_persist_failure_still_stages_removal` test documents
    /// for `removed_fabric`), and `next_fabric_index` reissues a removed
    /// index as `max(existing)+1` — an unpurged entry here would let a
    /// later `AddNOC` at that same index inherit the previous occupant's
    /// ACL (cross-fabric leak). Same reasoning applies to `GroupKeyStore`
    /// (Task 2's purge site, `handle_remove_fabric`'s error branch) and to
    /// `GroupMembershipStore` (groupcast レーン A フェーズ 2 Task 2).
    #[test]
    fn remove_fabric_persist_failure_still_purges_acl() {
        use crate::core::access_control::{decode_entries_for_test, AccessControlHandler};

        let fail_save = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let store = FabricStore::with_persist(Box::new(FlakySavePersist {
            fail_save: std::sync::Arc::clone(&fail_save),
        }));
        let dev = generate_dev_attestation(0xFFF1, 0x8000).unwrap();
        let mut server = CommissioningServer::new(dev, store);
        let acl_store = AclStore::new();
        server.set_acl_store(acl_store.clone());
        let gk_store = GroupKeyStore::new();
        server.set_group_key_store(gk_store.clone());
        let membership = GroupMembershipStore::new();
        server.set_group_membership_store(membership.clone());
        install_fabric(&mut server, 0x1122, 0x5001);

        let handler = AccessControlHandler::new(acl_store.clone());
        assert_eq!(
            decode_entries_for_test(&handler.read(im::ATTR_ACL, &read_ctx(1)).unwrap()).len(),
            1
        );
        gk_store.upsert_keyset(1, 7, [9u8; 16], 0).unwrap();
        gk_store.replace_fabric_map(1, vec![(0x000A, 7)]);
        membership.add(1, 10, 2).unwrap();

        fail_save.store(true, std::sync::atomic::Ordering::SeqCst);
        let ctx = InvokeCtx {
            fabric_index: 1,
            ..test_ctx()
        };
        let reply = server.invoke_command(
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_REMOVE_FABRIC,
            &encode_remove_fabric(1),
            &ctx,
        );
        assert_eq!(reply, InvokeReply::Status(im::STATUS_FAILURE));
        assert!(
            decode_entries_for_test(&handler.read(im::ATTR_ACL, &read_ctx(1)).unwrap()).is_empty()
        );
        assert!(!gk_store.keyset_exists(1, 7));
        assert!(gk_store.map_entries_for(1).is_empty());
        assert!(
            membership.groups_by_fabric().is_empty(),
            "purge must drop the fabric's memberships"
        );
    }

    /// spec §11.17.5.2: `SupportedFabrics` is the device's capacity, and
    /// `AddNOC` past it must answer `NOCResponse(TableFull=5)` rather than
    /// installing a fabric the attribute says can't exist.
    #[test]
    fn add_noc_rejects_sixth_fabric_with_table_full() {
        let mut server = test_server();
        for i in 0..u64::from(SUPPORTED_FABRICS) {
            let (_, resp) = expect_data(install_fabric(&mut server, 0x1122 + i, 0x5001 + i));
            let (status, _) = decode_noc_response(&resp).unwrap();
            assert_eq!(status, NOC_STATUS_OK, "fabric {} must install", i + 1);
        }
        assert_eq!(server.fabrics().len(), usize::from(SUPPORTED_FABRICS));

        let (response_command, resp) = expect_data(install_fabric(&mut server, 0x9999, 0x5999));
        assert_eq!(response_command, RESP_NOC);
        let (status, fabric_index) = decode_noc_response(&resp).unwrap();
        assert_eq!(status, NOC_STATUS_TABLE_FULL);
        assert_eq!(fabric_index, None);
        assert_eq!(
            server.fabrics().len(),
            usize::from(SUPPORTED_FABRICS),
            "a rejected AddNOC must not install anything"
        );
    }

    /// spec §11.17.6.8.1: `CaseAdminSubject` は operational node id か
    /// CAT（version ≠ 0）でなければ `NOCResponse(InvalidAdminSubject=0x0B)`。
    /// fabric も ACL エントリも作られず、pending（CSR/root）は残るので
    /// 同じセッションで正しい subject の AddNOC をやり直せる。
    #[test]
    fn add_noc_rejects_invalid_case_admin_subject_and_allows_retry() {
        use crate::core::access_control::{
            cat_subject, AclStore, Subject, OPERATIONAL_NODE_ID_MAX, PRIVILEGE_ADMINISTER,
        };
        for bad in [
            0u64,
            cat_subject(0xABCD_0000),    // CAT version 0
            OPERATIONAL_NODE_ID_MAX + 1, // 予約域の先頭
            0xFFFF_FFFF_FFFF_0001,       // group 域
        ] {
            let mut server = test_server();
            let acl_store = AclStore::new();
            server.set_acl_store(acl_store.clone());

            let (reply, noc, fabric) = install_fabric_with_admin(&mut server, 0x1122, 0x5001, bad);
            let (response_command, resp) = expect_data(reply);
            assert_eq!(response_command, RESP_NOC);
            let (status, fabric_index) = decode_noc_response(&resp).unwrap();
            assert_eq!(status, NOC_STATUS_INVALID_ADMIN_SUBJECT, "subject {bad:#x}");
            assert_eq!(fabric_index, None);
            assert!(
                server.fabrics().is_empty(),
                "rejected AddNOC must not install a fabric"
            );
            assert!(
                !acl_store.check(1, Subject::node(0x5001), PRIVILEGE_ADMINISTER, 0, 0x001F),
                "rejected AddNOC must not add an admin ACL entry"
            );

            // 同じ pending（CSR keypair / trusted root）で正しい subject なら通る。
            let (_, resp) = expect_data(drive_invoke(
                &mut server,
                CLUSTER_OPERATIONAL_CREDENTIALS,
                CMD_ADD_NOC,
                &encode_add_noc(&noc, &fabric.ipk_epoch, 0xAA, 0xFFF1),
            ));
            let (status, fabric_index) = decode_noc_response(&resp).unwrap();
            assert_eq!(status, NOC_STATUS_OK, "retry after {bad:#x}");
            assert_eq!(fabric_index, Some(1));
            assert_eq!(server.fabrics().len(), 1);
        }
    }

    /// CAT 形の admin subject（Apple Home が送る形）は version ≠ 0 なら受理。
    #[test]
    fn add_noc_accepts_cat_case_admin_subject() {
        use crate::core::access_control::cat_subject;
        let mut server = test_server();
        let (reply, _, _) =
            install_fabric_with_admin(&mut server, 0x1122, 0x5001, cat_subject(0xABCD_0002));
        let (_, resp) = expect_data(reply);
        let (status, fabric_index) = decode_noc_response(&resp).unwrap();
        assert_eq!(status, NOC_STATUS_OK);
        assert_eq!(fabric_index, Some(1));
    }
}
