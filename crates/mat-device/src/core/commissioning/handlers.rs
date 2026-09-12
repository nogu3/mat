//! The three thin `ClusterHandler` adapters over the shared `Inner`, plus
//! `Inner`'s command dispatch, attribute reads, and list encoders (spec
//! §11.10.5 / §11.17.5 / §11.19.6). Per-command handlers live in the
//! sibling modules (`general_commissioning` / `attestation` / `noc` /
//! `admin_commissioning`).

use std::sync::{Arc, Mutex};

use mat_controller::commissioning::{
    CLUSTER_ADMIN_COMMISSIONING, CLUSTER_GENERAL_COMMISSIONING, CLUSTER_OPERATIONAL_CREDENTIALS,
    CMD_ADD_NOC, CMD_ADD_TRUSTED_ROOT, CMD_ARM_FAIL_SAFE, CMD_ATTESTATION_REQUEST,
    CMD_CERT_CHAIN_REQUEST, CMD_COMMISSIONING_COMPLETE, CMD_CSR_REQUEST,
    CMD_OPEN_COMMISSIONING_WINDOW, CMD_REMOVE_FABRIC, CMD_REVOKE_COMMISSIONING,
    CMD_SET_REGULATORY_CONFIG, CMD_UPDATE_FABRIC_LABEL,
};
use mat_controller::im;
use mat_controller::sync::locked;
use mat_controller::tlv::{Tag, Writer};

use crate::core::datamodel::{ClusterHandler, InvokeCtx, InvokeReply, ReadCtx};
use crate::core::fabric_store::FabricEntry;
use crate::core::tlv_value;

use super::{
    Inner, ATTR_AC_ADMIN_FABRIC_INDEX, ATTR_AC_ADMIN_VENDOR_ID, ATTR_AC_WINDOW_STATUS,
    ATTR_GC_BASIC_COMMISSIONING_INFO, ATTR_GC_BREADCRUMB, ATTR_GC_LOCATION_CAPABILITY,
    ATTR_GC_REGULATORY_CONFIG, ATTR_GC_SUPPORTS_CONCURRENT_CONNECTION,
    ATTR_OC_COMMISSIONED_FABRICS, ATTR_OC_CURRENT_FABRIC_INDEX, ATTR_OC_FABRICS, ATTR_OC_NOCS,
    ATTR_OC_SUPPORTED_FABRICS, ATTR_OC_TRUSTED_ROOT_CERTIFICATES, FAIL_SAFE_EXPIRY_LENGTH_SECONDS,
    FAIL_SAFE_MAX_CUMULATIVE_SECONDS, RESP_ARM_FAIL_SAFE, RESP_ATTESTATION, RESP_CERT_CHAIN,
    RESP_COMMISSIONING_COMPLETE, RESP_CSR, RESP_NOC, RESP_SET_REGULATORY_CONFIG, SUPPORTED_FABRICS,
};

/// Thin `ClusterHandler` adapter for General Commissioning (0x0030).
pub(super) struct GeneralCommissioningHandler(pub(super) Arc<Mutex<Inner>>);

impl ClusterHandler for GeneralCommissioningHandler {
    fn cluster_id(&self) -> u32 {
        CLUSTER_GENERAL_COMMISSIONING
    }

    /// ClusterRevision (spec §7.13): General Commissioning cluster spec
    /// revision 1 (Matter 1.4).
    fn revision(&self) -> u16 {
        1
    }

    fn attributes(&self) -> Vec<u32> {
        vec![
            ATTR_GC_BREADCRUMB,
            ATTR_GC_BASIC_COMMISSIONING_INFO,
            ATTR_GC_REGULATORY_CONFIG,
            ATTR_GC_LOCATION_CAPABILITY,
            ATTR_GC_SUPPORTS_CONCURRENT_CONNECTION,
        ]
    }

    fn read(&self, attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
        locked(&self.0).read_general_commissioning(attribute)
    }

    fn invoke(&mut self, command: u32, fields_tlv: &[u8], _ctx: &mut InvokeCtx) -> InvokeReply {
        locked(&self.0).handle_general_commissioning(command, fields_tlv)
    }

    fn accepted_commands(&self) -> Vec<u32> {
        vec![
            CMD_ARM_FAIL_SAFE,
            CMD_SET_REGULATORY_CONFIG,
            CMD_COMMISSIONING_COMPLETE,
        ]
    }

    fn generated_commands(&self) -> Vec<u32> {
        vec![
            RESP_ARM_FAIL_SAFE,
            RESP_SET_REGULATORY_CONFIG,
            RESP_COMMISSIONING_COMPLETE,
        ]
    }

    /// spec §11.10.5: General Commissioning のコマンドは全て Administer。
    /// commissioning 本番の呼び出しは PASE（fabric 0 = implicit
    /// Administer、`datamodel::acl_allows`）か、AddNOC 後の CASE で
    /// commissioner 自身の admin エントリ（`AclStore::add_case_admin`）
    /// 経由なので、この要求で正規フローが塞がることはない。
    fn invoke_privilege(&self, _command: u32) -> u8 {
        crate::core::access_control::PRIVILEGE_ADMINISTER
    }
}

/// Thin `ClusterHandler` adapter for Node Operational Credentials (0x003E).
pub(super) struct OperationalCredentialsHandler(pub(super) Arc<Mutex<Inner>>);

impl ClusterHandler for OperationalCredentialsHandler {
    fn cluster_id(&self) -> u32 {
        CLUSTER_OPERATIONAL_CREDENTIALS
    }

    /// ClusterRevision (spec §7.13): Operational Credentials cluster spec
    /// revision 1 (Matter 1.4).
    fn revision(&self) -> u16 {
        1
    }

    fn attributes(&self) -> Vec<u32> {
        vec![
            ATTR_OC_NOCS,
            ATTR_OC_FABRICS,
            ATTR_OC_SUPPORTED_FABRICS,
            ATTR_OC_COMMISSIONED_FABRICS,
            ATTR_OC_TRUSTED_ROOT_CERTIFICATES,
            ATTR_OC_CURRENT_FABRIC_INDEX,
        ]
    }

    fn read(&self, attribute: u32, ctx: &ReadCtx) -> Option<Vec<u8>> {
        locked(&self.0).read_operational_credentials(attribute, ctx)
    }

    fn invoke(&mut self, command: u32, fields_tlv: &[u8], ctx: &mut InvokeCtx) -> InvokeReply {
        locked(&self.0).handle_operational_credentials(command, fields_tlv, ctx)
    }

    fn accepted_commands(&self) -> Vec<u32> {
        vec![
            CMD_ATTESTATION_REQUEST,
            CMD_CERT_CHAIN_REQUEST,
            CMD_CSR_REQUEST,
            CMD_ADD_NOC,
            CMD_UPDATE_FABRIC_LABEL,
            CMD_REMOVE_FABRIC,
            CMD_ADD_TRUSTED_ROOT,
        ]
    }

    fn generated_commands(&self) -> Vec<u32> {
        vec![RESP_ATTESTATION, RESP_CERT_CHAIN, RESP_CSR, RESP_NOC]
    }

    /// spec §11.17.5: Operational Credentials のコマンドは全て Administer
    /// （AddNOC / RemoveFabric は fabric 資格そのものの操作）。理由は
    /// `GeneralCommissioningHandler::invoke_privilege` の doc と同じ。
    fn invoke_privilege(&self, _command: u32) -> u8 {
        crate::core::access_control::PRIVILEGE_ADMINISTER
    }

    /// spec §11.17.5 のアクセス表: `NOCs` は read が Administer（NOC/ICAC
    /// はそれ自体クレデンシャルであり、`ACL` と同様にその fabric の管理者
    /// だけが読める — `access_control.rs` の `read_privilege` と同じ書き
    /// 方）。`Fabrics`/`TrustedRootCertificates`/`CurrentFabricIndex`/容量系
    /// は trait default の View のまま。
    fn read_privilege(&self, attribute: u32) -> u8 {
        match attribute {
            ATTR_OC_NOCS => crate::core::access_control::PRIVILEGE_ADMINISTER,
            _ => crate::core::access_control::PRIVILEGE_VIEW,
        }
    }
}

/// Thin `ClusterHandler` adapter for Administrator Commissioning (0x003C).
pub(super) struct AdminCommissioningHandler(pub(super) Arc<Mutex<Inner>>);

impl ClusterHandler for AdminCommissioningHandler {
    fn cluster_id(&self) -> u32 {
        CLUSTER_ADMIN_COMMISSIONING
    }

    /// ClusterRevision (spec §7.13): Administrator Commissioning cluster
    /// spec revision 1 (Matter 1.4).
    fn revision(&self) -> u16 {
        1
    }

    fn attributes(&self) -> Vec<u32> {
        vec![
            ATTR_AC_WINDOW_STATUS,
            ATTR_AC_ADMIN_FABRIC_INDEX,
            ATTR_AC_ADMIN_VENDOR_ID,
        ]
    }

    fn read(&self, attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
        locked(&self.0).read_admin_commissioning(attribute)
    }

    fn invoke(&mut self, command: u32, fields_tlv: &[u8], ctx: &mut InvokeCtx) -> InvokeReply {
        locked(&self.0).handle_admin_commissioning(command, fields_tlv, ctx)
    }

    fn accepted_commands(&self) -> Vec<u32> {
        vec![CMD_OPEN_COMMISSIONING_WINDOW, CMD_REVOKE_COMMISSIONING]
    }

    /// spec §11.19.5: Administrator Commissioning のコマンドは全て
    /// Administer（別 admin を招き入れる窓の開閉なので、Manage 止まりの
    /// controller には出させない）。
    fn invoke_privilege(&self, _command: u32) -> u8 {
        crate::core::access_control::PRIVILEGE_ADMINISTER
    }
}

impl Inner {
    pub(super) fn handle_general_commissioning(
        &mut self,
        command: u32,
        fields_tlv: &[u8],
    ) -> InvokeReply {
        match command {
            CMD_ARM_FAIL_SAFE => self.handle_arm_fail_safe(fields_tlv),
            CMD_SET_REGULATORY_CONFIG => self.handle_set_regulatory_config(fields_tlv),
            CMD_COMMISSIONING_COMPLETE => self.handle_commissioning_complete(),
            _ => InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND),
        }
    }

    pub(super) fn handle_operational_credentials(
        &mut self,
        command: u32,
        fields_tlv: &[u8],
        ctx: &InvokeCtx,
    ) -> InvokeReply {
        match command {
            CMD_ATTESTATION_REQUEST => self.handle_attestation_request(fields_tlv, ctx),
            CMD_CERT_CHAIN_REQUEST => self.handle_cert_chain_request(fields_tlv),
            CMD_CSR_REQUEST => self.handle_csr_request(fields_tlv, ctx),
            CMD_ADD_TRUSTED_ROOT => self.handle_add_trusted_root(fields_tlv),
            CMD_ADD_NOC => self.handle_add_noc(fields_tlv),
            CMD_UPDATE_FABRIC_LABEL => self.handle_update_fabric_label(fields_tlv, ctx),
            CMD_REMOVE_FABRIC => self.handle_remove_fabric(fields_tlv),
            _ => InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND),
        }
    }

    pub(super) fn handle_admin_commissioning(
        &mut self,
        command: u32,
        fields_tlv: &[u8],
        ctx: &InvokeCtx,
    ) -> InvokeReply {
        match command {
            CMD_OPEN_COMMISSIONING_WINDOW => self.handle_open_commissioning_window(fields_tlv, ctx),
            CMD_REVOKE_COMMISSIONING => self.handle_revoke_commissioning(),
            _ => InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND),
        }
    }

    /// General Commissioning attribute reads (spec §11.10.5). Answers the
    /// fixed set chip-tool/Echo read during and right after commissioning.
    fn read_general_commissioning(&self, attribute: u32) -> Option<Vec<u8>> {
        match attribute {
            ATTR_GC_BREADCRUMB => Some(tlv_value::uint(0)),
            ATTR_GC_BASIC_COMMISSIONING_INFO => {
                let mut w = Writer::new();
                w.start_struct(Tag::Anonymous);
                w.put_uint(Tag::Context(0), u64::from(FAIL_SAFE_EXPIRY_LENGTH_SECONDS));
                w.put_uint(Tag::Context(1), u64::from(FAIL_SAFE_MAX_CUMULATIVE_SECONDS));
                w.end_container();
                Some(w.finish())
            }
            ATTR_GC_REGULATORY_CONFIG => Some(tlv_value::uint(0)),
            ATTR_GC_LOCATION_CAPABILITY => Some(tlv_value::uint(2)),
            ATTR_GC_SUPPORTS_CONCURRENT_CONNECTION => Some(tlv_value::bool(true)),
            _ => None,
        }
    }

    /// Node Operational Credentials attribute reads (spec §11.17.5),
    /// reflecting whatever `AddNOC` has installed into `self.store` so far.
    /// `CurrentFabricIndex` (spec §11.17.5.3) is the reading session's own
    /// fabric index — carried in `ctx` (`ReadCtx`), not derivable from
    /// `self.store` alone. `NOCs`/`Fabrics`/`TrustedRootCertificates` are
    /// fabric-scoped lists, so `ctx.fabric_filtered` decides whether they
    /// answer with the accessing fabric's entry alone or the whole table
    /// (see `fabric_scoped_entries`); the two counters are plain scalars and
    /// are never filtered.
    fn read_operational_credentials(&self, attribute: u32, ctx: &ReadCtx) -> Option<Vec<u8>> {
        match attribute {
            ATTR_OC_NOCS => Some(self.encode_nocs(ctx)),
            ATTR_OC_FABRICS => Some(self.encode_fabrics(ctx)),
            ATTR_OC_SUPPORTED_FABRICS => Some(tlv_value::uint(u64::from(SUPPORTED_FABRICS))),
            ATTR_OC_COMMISSIONED_FABRICS => {
                Some(tlv_value::uint(self.store.entries().len() as u64))
            }
            ATTR_OC_TRUSTED_ROOT_CERTIFICATES => Some(self.encode_trusted_root_certificates(ctx)),
            ATTR_OC_CURRENT_FABRIC_INDEX => Some(tlv_value::uint(u64::from(ctx.fabric_index))),
            _ => None,
        }
    }

    /// Administrator Commissioning attribute reads (spec §11.19.5), backed
    /// by `admin_window`. `AdminFabricIndex`/`AdminVendorID` are `null`
    /// while the window is closed (spec §11.19.5.1/.2) rather than 0 —
    /// `Writer::put_null` distinguishes "no admin" from fabric index 0
    /// (never a real fabric index) or vendor id 0 (unassigned but legal).
    fn read_admin_commissioning(&self, attribute: u32) -> Option<Vec<u8>> {
        match attribute {
            ATTR_AC_WINDOW_STATUS => Some(tlv_value::uint(if self.admin_window.is_some() {
                1
            } else {
                0
            })),
            ATTR_AC_ADMIN_FABRIC_INDEX => Some(match self.admin_window {
                Some(w) => tlv_value::uint(u64::from(w.fabric_index)),
                None => tlv_value::null(),
            }),
            ATTR_AC_ADMIN_VENDOR_ID => Some(match self.admin_window {
                Some(w) => tlv_value::uint(u64::from(w.vendor_id)),
                None => tlv_value::null(),
            }),
            _ => None,
        }
    }

    /// The fabric table as one read should see it: every entry when the
    /// request asked for the unfiltered view, otherwise only the accessing
    /// fabric's (spec §8.9.2.4 — a fabric-filtered read of a fabric-scoped
    /// list returns just the accessing fabric's entries). A PASE session
    /// (`fabric_index` 0, never a valid index) therefore matches nothing and
    /// gets an empty list, which is exactly right: it has no fabric whose
    /// credentials it is entitled to see.
    fn fabric_scoped_entries(&self, ctx: &ReadCtx) -> impl Iterator<Item = &FabricEntry> {
        // Copied out of `ctx` rather than borrowed: the returned iterator
        // must only borrow `self`, not the caller's `ReadCtx`.
        let (filtered, accessing) = (ctx.fabric_filtered, ctx.fabric_index);
        self.store
            .entries()
            .iter()
            .filter(move |e| !filtered || e.fabric_index == accessing)
    }

    /// NOCs(0): `array[ struct{1: NOCValue, 2: ICACValue?, 254:
    /// FabricIndex} ]` (spec §11.17.5.3, `NOCStruct`). Fabric-scoped — see
    /// `fabric_scoped_entries`.
    fn encode_nocs(&self, ctx: &ReadCtx) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_array(Tag::Anonymous);
        for entry in self.fabric_scoped_entries(ctx) {
            w.start_struct(Tag::Anonymous);
            w.put_bytes(Tag::Context(1), &entry.noc_tlv);
            if let Some(icac) = &entry.icac_tlv {
                w.put_bytes(Tag::Context(2), icac);
            }
            w.put_uint(Tag::Context(254), u64::from(entry.fabric_index));
            w.end_container();
        }
        w.end_container();
        w.finish()
    }

    /// Fabrics(1): `array[ struct{1: RootPublicKey, 2: VendorID, 3:
    /// FabricID, 4: NodeID, 5: Label, 254: FabricIndex} ]` (spec
    /// §11.17.5.3, `FabricDescriptorStruct`). `Label` reflects whatever
    /// `UpdateFabricLabel` (`handle_update_fabric_label`) has set — empty
    /// until then. Fabric-scoped — see `fabric_scoped_entries`.
    fn encode_fabrics(&self, ctx: &ReadCtx) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_array(Tag::Anonymous);
        for entry in self.fabric_scoped_entries(ctx) {
            w.start_struct(Tag::Anonymous);
            w.put_bytes(Tag::Context(1), &entry.root_public_key);
            w.put_uint(Tag::Context(2), u64::from(entry.admin_vendor_id));
            w.put_uint(Tag::Context(3), entry.fabric_id);
            w.put_uint(Tag::Context(4), entry.node_id);
            w.put_str(Tag::Context(5), &entry.label);
            w.put_uint(Tag::Context(254), u64::from(entry.fabric_index));
            w.end_container();
        }
        w.end_container();
        w.finish()
    }

    /// TrustedRootCertificates(4): `array[ bytes(RootCACertificate TLV) ]`
    /// (spec §11.17.5.3) — one entry per installed fabric's RCAC.
    /// Fabric-scoped — see `fabric_scoped_entries`. Its entries carry no
    /// `FabricIndex` field of their own (they're bare certificate blobs),
    /// which is precisely why filtering matters here: an unfiltered read
    /// hands out every commissioner's root with nothing to tell them apart.
    fn encode_trusted_root_certificates(&self, ctx: &ReadCtx) -> Vec<u8> {
        let mut w = Writer::new();
        w.start_array(Tag::Anonymous);
        for entry in self.fabric_scoped_entries(ctx) {
            w.put_bytes(Tag::Anonymous, &entry.root_tlv);
        }
        w.end_container();
        w.finish()
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_util::{
        commissioned_server, drive_invoke, install_fabric, read_ctx, test_ctx, test_server,
    };
    use super::super::CommissioningServer;
    use super::*;
    use crate::core::access_control::AclStore;
    use crate::core::fabric_store::FabricStore;
    use mat_controller::commissioning::{
        decode_commissioning_status_response, encode_arm_fail_safe, encode_cert_chain_request,
        encode_open_commissioning_window, CERT_TYPE_DAC,
    };
    use mat_controller::tlv::{Reader, Value};
    use mat_controller::x509::generate_dev_attestation;

    #[test]
    fn gc_serves_basic_commissioning_info() {
        let server = test_server();
        let (gc, ..) = server.into_cluster_handlers();
        let tlv = gc
            .read(ATTR_GC_BASIC_COMMISSIONING_INFO, &ReadCtx::default())
            .expect("BasicCommissioningInfo");

        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let mut expiry = None;
        let mut max_cumulative = None;
        loop {
            let el = r.next().unwrap().expect("truncated BasicCommissioningInfo");
            match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(0), Value::Uint(v)) => expiry = Some(v),
                (Tag::Context(1), Value::Uint(v)) => max_cumulative = Some(v),
                _ => {}
            }
        }
        assert_eq!(expiry, Some(60));
        assert_eq!(max_cumulative, Some(900));
    }

    /// AcceptedCommandList/GeneratedCommandList (spec §7.13) for the three
    /// commissioning clusters must name their real command sets —
    /// conformance-checking controllers (Apple Home) read them during the
    /// post-commissioning interview.
    #[test]
    fn commissioning_handlers_declare_their_command_lists() {
        let server = test_server();
        let (gc, oc, ac) = server.into_cluster_handlers();

        assert_eq!(
            gc.accepted_commands(),
            vec![
                CMD_ARM_FAIL_SAFE,
                CMD_SET_REGULATORY_CONFIG,
                CMD_COMMISSIONING_COMPLETE
            ]
        );
        assert_eq!(
            gc.generated_commands(),
            vec![
                RESP_ARM_FAIL_SAFE,
                RESP_SET_REGULATORY_CONFIG,
                RESP_COMMISSIONING_COMPLETE
            ]
        );

        assert_eq!(
            oc.accepted_commands(),
            vec![
                CMD_ATTESTATION_REQUEST,
                CMD_CERT_CHAIN_REQUEST,
                CMD_CSR_REQUEST,
                CMD_ADD_NOC,
                CMD_UPDATE_FABRIC_LABEL,
                CMD_REMOVE_FABRIC,
                CMD_ADD_TRUSTED_ROOT
            ]
        );
        assert_eq!(
            oc.generated_commands(),
            vec![RESP_ATTESTATION, RESP_CERT_CHAIN, RESP_CSR, RESP_NOC]
        );

        assert_eq!(
            ac.accepted_commands(),
            vec![CMD_OPEN_COMMISSIONING_WINDOW, CMD_REVOKE_COMMISSIONING]
        );
        assert_eq!(ac.generated_commands(), Vec::<u32>::new());
    }

    #[test]
    fn gc_serves_other_scalar_attributes() {
        let server = test_server();
        let (gc, ..) = server.into_cluster_handlers();

        let breadcrumb = gc
            .read(ATTR_GC_BREADCRUMB, &ReadCtx::default())
            .expect("Breadcrumb");
        let mut r = Reader::new(&breadcrumb);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(0));

        let regulatory = gc
            .read(ATTR_GC_REGULATORY_CONFIG, &ReadCtx::default())
            .expect("RegulatoryConfig");
        let mut r = Reader::new(&regulatory);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(0));

        let location = gc
            .read(ATTR_GC_LOCATION_CAPABILITY, &ReadCtx::default())
            .expect("LocationCapability");
        let mut r = Reader::new(&location);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(2));

        let concurrent = gc
            .read(ATTR_GC_SUPPORTS_CONCURRENT_CONNECTION, &ReadCtx::default())
            .expect("SupportsConcurrentConnection");
        let mut r = Reader::new(&concurrent);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Bool(true));
    }

    #[test]
    fn oc_fabrics_and_nocs_reflect_installed_fabric() {
        let server = commissioned_server(); // fabric_id=0x1122, node=0x5001, admin_vendor_id=0xFFF1
        let (_, oc, _) = server.into_cluster_handlers();

        // NOCs(0): array[ struct{1: noc_tlv, 2: icac_tlv?, 254: fabric_index} ]
        let nocs_tlv = oc
            .read(ATTR_OC_NOCS, &ReadCtx::unfiltered(0))
            .expect("NOCs");
        let mut r = Reader::new(&nocs_tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::ArrayStart);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let mut noc_tlv = None;
        let mut fabric_index = None;
        loop {
            let el = r.next().unwrap().expect("truncated NOC struct");
            match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(1), Value::Bytes(b)) => noc_tlv = Some(b.to_vec()),
                (Tag::Context(254), Value::Uint(v)) => fabric_index = Some(v),
                _ => {}
            }
        }
        assert!(noc_tlv.is_some());
        assert_eq!(fabric_index, Some(1));

        // Fabrics(1): array[ struct{1: root_public_key, 2: admin_vendor_id,
        // 3: fabric_id, 4: node_id, 5: label, 254: fabric_index} ]
        let fabrics_tlv = oc
            .read(ATTR_OC_FABRICS, &ReadCtx::unfiltered(0))
            .expect("Fabrics");
        let mut r = Reader::new(&fabrics_tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::ArrayStart);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let mut admin_vendor_id = None;
        let mut fabric_id = None;
        let mut node_id = None;
        let mut fidx = None;
        loop {
            let el = r
                .next()
                .unwrap()
                .expect("truncated FabricDescriptor struct");
            match (el.tag, el.value) {
                (_, Value::ContainerEnd) => break,
                (Tag::Context(2), Value::Uint(v)) => admin_vendor_id = Some(v),
                (Tag::Context(3), Value::Uint(v)) => fabric_id = Some(v),
                (Tag::Context(4), Value::Uint(v)) => node_id = Some(v),
                (Tag::Context(254), Value::Uint(v)) => fidx = Some(v),
                _ => {}
            }
        }
        assert_eq!(admin_vendor_id, Some(0xFFF1));
        assert_eq!(fabric_id, Some(0x1122));
        assert_eq!(node_id, Some(0x5001));
        assert_eq!(fidx, Some(1));

        // SupportedFabrics / CommissionedFabrics are plain scalars.
        let supported = oc
            .read(ATTR_OC_SUPPORTED_FABRICS, &ReadCtx::default())
            .expect("SupportedFabrics");
        let mut r = Reader::new(&supported);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(5));

        let commissioned = oc
            .read(ATTR_OC_COMMISSIONED_FABRICS, &ReadCtx::default())
            .expect("CommissionedFabrics");
        let mut r = Reader::new(&commissioned);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(1));

        // TrustedRootCertificates(4): array[ bytes(root_tlv) ]
        let roots_tlv = oc
            .read(ATTR_OC_TRUSTED_ROOT_CERTIFICATES, &ReadCtx::unfiltered(0))
            .expect("TrustedRootCertificates");
        let mut r = Reader::new(&roots_tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::ArrayStart);
        let el = r.next().unwrap().expect("one root cert");
        assert!(matches!(el.value, Value::Bytes(_)));
    }

    /// `CurrentFabricIndex` (spec §11.17.5.3) echoes back the *reading
    /// session's* fabric index from `ReadCtx`, not anything derived from
    /// the fabric table — it's session-scoped, so two different sessions
    /// against the same installed fabric would report their own selected
    /// index (M2 has one fabric, but the attribute's whole point is not
    /// assuming that).
    #[test]
    fn oc_current_fabric_index_reflects_read_ctx() {
        let server = commissioned_server();
        let (_, oc, _) = server.into_cluster_handlers();

        let tlv = oc
            .read(ATTR_OC_CURRENT_FABRIC_INDEX, &read_ctx(1))
            .expect("CurrentFabricIndex");
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(1));
    }

    /// Reads every `FabricIndex` (context tag 254) of an
    /// `array[ struct{ …, 254: FabricIndex } ]` attribute straight off the
    /// encoded bytes with the TLV `Reader` — no decode helper in between,
    /// so the assertion is about what actually goes on the wire. Returns one
    /// entry per array element (and panics if an element carries none, which
    /// would itself be a spec violation for a fabric-scoped list).
    fn fabric_indices_of_list(tlv: &[u8]) -> Vec<u64> {
        let mut r = Reader::new(tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::ArrayStart);
        let mut out = Vec::new();
        loop {
            match r.next().unwrap().expect("truncated list").value {
                Value::ContainerEnd => break, // end of the array itself
                Value::StructStart => {}
                other => panic!("unexpected array element: {other:?}"),
            }
            let mut fabric_index = None;
            loop {
                let el = r.next().unwrap().expect("truncated list entry struct");
                match (el.tag, el.value) {
                    (_, Value::ContainerEnd) => break,
                    (Tag::Context(254), Value::Uint(v)) => fabric_index = Some(v),
                    _ => {}
                }
            }
            out.push(fabric_index.expect("list entry without FabricIndex(254)"));
        }
        out
    }

    /// Number of elements in an `array[ bytes ]` attribute
    /// (`TrustedRootCertificates`), read off the encoded bytes directly —
    /// its entries carry no `FabricIndex`, so the count is all there is to
    /// assert.
    fn byte_list_len(tlv: &[u8]) -> usize {
        let mut r = Reader::new(tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::ArrayStart);
        let mut n = 0;
        loop {
            match r.next().unwrap().expect("truncated byte list").value {
                Value::ContainerEnd => break,
                Value::Bytes(_) => n += 1,
                other => panic!("unexpected array element: {other:?}"),
            }
        }
        n
    }

    /// spec §11.17.5 / §8.9.2.4: `NOCs`/`Fabrics`/`TrustedRootCertificates`
    /// are fabric-scoped lists, so a read with `IsFabricFiltered=true` must
    /// only see the accessing fabric's own entry — anything else leaks one
    /// commissioner's credentials to another (Apple alone establishes two
    /// fabrics in practice). A PASE session (fabric index 0, no fabric yet)
    /// sees an empty list. `IsFabricFiltered=false` keeps the unfiltered
    /// view.
    #[test]
    fn oc_reads_are_fabric_scoped_when_fabric_filtered() {
        let mut server = test_server();
        install_fabric(&mut server, 0x1122, 0x5001); // fabric_index 1
        install_fabric(&mut server, 0x3344, 0x5002); // fabric_index 2
        assert_eq!(server.fabrics().len(), 2);
        let (_, oc, _) = server.into_cluster_handlers();

        let read = |attribute: u32, ctx: &ReadCtx| oc.read(attribute, ctx).expect("OC attribute");

        for accessing in [1u8, 2] {
            let ctx = ReadCtx {
                fabric_index: accessing,
                fabric_filtered: true,
                ..ReadCtx::default()
            };
            assert_eq!(
                fabric_indices_of_list(&read(ATTR_OC_NOCS, &ctx)),
                vec![u64::from(accessing)],
                "NOCs must only carry fabric {accessing}'s entry"
            );
            assert_eq!(
                fabric_indices_of_list(&read(ATTR_OC_FABRICS, &ctx)),
                vec![u64::from(accessing)],
                "Fabrics must only carry fabric {accessing}'s entry"
            );
            assert_eq!(
                byte_list_len(&read(ATTR_OC_TRUSTED_ROOT_CERTIFICATES, &ctx)),
                1,
                "TrustedRootCertificates must only carry fabric {accessing}'s root"
            );
        }

        // PASE (fabric index 0 — never a valid fabric index): nothing matches.
        let pase = ReadCtx {
            fabric_index: 0,
            fabric_filtered: true,
            ..ReadCtx::default()
        };
        assert!(fabric_indices_of_list(&read(ATTR_OC_NOCS, &pase)).is_empty());
        assert!(fabric_indices_of_list(&read(ATTR_OC_FABRICS, &pase)).is_empty());
        assert_eq!(
            byte_list_len(&read(ATTR_OC_TRUSTED_ROOT_CERTIFICATES, &pase)),
            0
        );

        // IsFabricFiltered=false: unfiltered, as before (regression guard).
        let unfiltered = ReadCtx {
            fabric_index: 1,
            fabric_filtered: false,
            ..ReadCtx::default()
        };
        assert_eq!(
            fabric_indices_of_list(&read(ATTR_OC_NOCS, &unfiltered)),
            vec![1, 2]
        );
        assert_eq!(
            fabric_indices_of_list(&read(ATTR_OC_FABRICS, &unfiltered)),
            vec![1, 2]
        );
        assert_eq!(
            byte_list_len(&read(ATTR_OC_TRUSTED_ROOT_CERTIFICATES, &unfiltered)),
            2
        );

        // Scalars stay unfiltered either way (spec: not fabric-scoped).
        for ctx in [&pase, &unfiltered] {
            let commissioned = read(ATTR_OC_COMMISSIONED_FABRICS, ctx);
            let mut r = Reader::new(&commissioned);
            assert_eq!(r.next().unwrap().unwrap().value, Value::Uint(2));
            let supported = read(ATTR_OC_SUPPORTED_FABRICS, ctx);
            let mut r = Reader::new(&supported);
            assert_eq!(
                r.next().unwrap().unwrap().value,
                Value::Uint(u64::from(SUPPORTED_FABRICS))
            );
        }
    }

    /// spec §11.17.5 のアクセス表: `NOCs` の read は Administer — Operate
    /// までしか持たない subject には `STATUS_UNSUPPORTED_ACCESS` が
    /// per-entry status で返り、Administer を持つ subject には通常どおり
    /// data が返る（`access_control.rs` の
    /// `acl_read_privilege_is_per_attribute_and_global_attributes_stay_view`
    /// と同じ検証手法を `Node`/ACL 経由で行う）。
    #[test]
    fn oc_nocs_read_requires_administer() {
        let mut server = test_server();
        install_fabric(&mut server, 0x1122, 0x5001); // fabric_index 1
        let (_, oc, _) = server.into_cluster_handlers();

        let acl = AclStore::new();
        acl.set_entries_for_test(
            1,
            vec![
                crate::core::access_control::AclDeviceEntry {
                    privilege: crate::core::access_control::PRIVILEGE_OPERATE,
                    auth_mode: crate::core::access_control::AUTH_MODE_CASE,
                    subjects: vec![7],
                    targets_raw: None,
                    fabric_index: 1,
                },
                crate::core::access_control::AclDeviceEntry {
                    privilege: crate::core::access_control::PRIVILEGE_ADMINISTER,
                    auth_mode: crate::core::access_control::AUTH_MODE_CASE,
                    subjects: vec![9],
                    targets_raw: None,
                    fabric_index: 1,
                },
            ],
        );

        let mut node = crate::core::datamodel::Node::new();
        node.add_endpoint(0, vec![oc]);
        node.set_acl_store(acl);

        let nocs_path = [im::AttrPathIn {
            endpoint: Some(0),
            cluster: Some(CLUSTER_OPERATIONAL_CREDENTIALS),
            attribute: Some(ATTR_OC_NOCS),
        }];

        // Operate だけの subject: per-entry UNSUPPORTED_ACCESS status。
        let operate_ctx = ReadCtx {
            fabric_index: 1,
            subject: crate::core::access_control::Subject::node(7),
            ..ReadCtx::default()
        };
        let entries = node.read_entries(&nocs_path, &operate_ctx);
        assert!(matches!(
            &entries[..],
            [im::ReportEntryOut::Status { status, .. }]
                if *status == im::STATUS_UNSUPPORTED_ACCESS
        ));

        // Administer を持つ subject: data が返る。
        let admin_ctx = ReadCtx {
            fabric_index: 1,
            subject: crate::core::access_control::Subject::node(9),
            ..ReadCtx::default()
        };
        let entries = node.read_entries(&nocs_path, &admin_ctx);
        assert!(matches!(&entries[..], [im::ReportEntryOut::Data(_)]));
    }

    #[test]
    fn unknown_command_and_cluster_are_rejected() {
        let mut server = test_server();
        assert_eq!(
            drive_invoke(&mut server, CLUSTER_GENERAL_COMMISSIONING, 0x7F, &[]),
            InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
        );
        assert_eq!(
            drive_invoke(&mut server, 0x9999, CMD_ARM_FAIL_SAFE, &[]),
            InvokeReply::Status(im::STATUS_UNSUPPORTED_CLUSTER)
        );
    }

    /// End-to-end proof that `into_cluster_handlers` wires all three
    /// clusters into the same shared state through real `Node`/IM wire
    /// framing (not just the direct `invoke_command` shortcut the tests
    /// above use).
    #[test]
    fn wired_into_node_dispatches_both_clusters() {
        let dev = generate_dev_attestation(0xFFF1, 0x8000).unwrap();
        let server = CommissioningServer::new(dev, FabricStore::new());
        let (gc, oc, ac) = server.into_cluster_handlers();
        let mut node = crate::core::datamodel::Node::new();
        node.add_endpoint(0, vec![gc, oc, ac]);
        let mut ctx = test_ctx();

        let req = im::encode_invoke_request(
            0,
            CLUSTER_GENERAL_COMMISSIONING,
            CMD_ARM_FAIL_SAFE,
            Some(&encode_arm_fail_safe(120, 1)),
        );
        let outcome = node
            .handle_im(
                im::OPCODE_INVOKE_REQUEST,
                &req,
                &mut ctx,
                &crate::core::datamodel::ReadCtx::default(),
            )
            .unwrap();
        let (opcode, payload) = (outcome.opcode, outcome.payload);
        assert_eq!(opcode, im::OPCODE_INVOKE_RESPONSE);
        let out = im::decode_invoke_response_data(&payload).unwrap();
        assert_eq!(out.status, im::STATUS_SUCCESS);
        let (code, _) =
            decode_commissioning_status_response(out.fields_tlv.as_deref().unwrap()).unwrap();
        assert_eq!(code, 0);

        let req = im::encode_invoke_request(
            0,
            CLUSTER_OPERATIONAL_CREDENTIALS,
            CMD_CERT_CHAIN_REQUEST,
            Some(&encode_cert_chain_request(CERT_TYPE_DAC)),
        );
        let outcome = node
            .handle_im(
                im::OPCODE_INVOKE_REQUEST,
                &req,
                &mut ctx,
                &crate::core::datamodel::ReadCtx::default(),
            )
            .unwrap();
        let (opcode, payload) = (outcome.opcode, outcome.payload);
        assert_eq!(opcode, im::OPCODE_INVOKE_RESPONSE);
        let out = im::decode_invoke_response_data(&payload).unwrap();
        assert_eq!(out.status, im::STATUS_SUCCESS);
        assert!(out.fields_tlv.is_some());

        let req = im::encode_invoke_request(
            0,
            CLUSTER_ADMIN_COMMISSIONING,
            CMD_OPEN_COMMISSIONING_WINDOW,
            Some(&encode_open_commissioning_window(
                300,
                &[0x42; 97],
                0x0ABC,
                1000,
                &[0x5A; 16],
            )),
        );
        let outcome = node
            .handle_im(
                im::OPCODE_INVOKE_REQUEST,
                &req,
                &mut ctx,
                &crate::core::datamodel::ReadCtx::default(),
            )
            .unwrap();
        let (opcode, payload) = (outcome.opcode, outcome.payload);
        assert_eq!(opcode, im::OPCODE_INVOKE_RESPONSE);
        let out = im::decode_invoke_response_data(&payload).unwrap();
        assert_eq!(out.status, im::STATUS_SUCCESS);
    }
}
