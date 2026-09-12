use super::*;

/// Persistence boundary `core` calls through instead of touching a
/// filesystem directly — same shape as `core::access_control::AclPersist`/
/// `core::fabric_store::FabricPersist` (see those traits' docs for the
/// `: Send`/object-safety rationale), but save-only: NodeLabel/Location's
/// *initial* value is whatever the constructor is handed (loaded by the
/// caller, e.g. `net::store::load_basic_info`), not something this trait
/// loads itself. A save failure is logged and ignored, same disposition as
/// `AclPersist` (see `BasicInformationHandler::persist_state`'s doc).
pub trait BasicInfoPersist: Send {
    fn save(&self, node_label: &str, location: &str) -> Result<(), String>;
}

/// BasicInformation cluster (spec §11.1), mandatory on endpoint 0. Task 5
/// filled in every attribute Apple Home's post-commissioning interview
/// reads (beyond the identity attributes M1 already served); Task 10 adds
/// disk persistence for the two writable ones (NodeLabel/Location) via an
/// injected `BasicInfoPersist` — everything else here is either a fixed
/// value or, for UniqueID, whatever the constructor was handed
/// (`device::Device::new` threads through the value persisted at
/// `<store_dir>/unique_id`, a separate file predating this task).
pub(super) struct BasicInformationHandler {
    pub(super) vendor_id: u16,
    pub(super) product_id: u16,
    pub(super) unique_id: String,
    /// NodeLabel (spec §11.1.6.2) — writable, persisted via `persist` on
    /// change. `write` mutates it directly since `Node::handle_write`
    /// already gives every `ClusterHandler::write` call `&mut self`.
    pub(super) node_label: String,
    /// Location (spec §11.1.6.6, `CountryCode`) — writable, persisted via
    /// `persist` on change. Spec default `"XX"` (unknown/unset).
    pub(super) location: String,
    /// Save backend for NodeLabel/Location — `None` for the ~15 existing
    /// `with_root_endpoint`/`with_root_endpoint_unique` call sites that
    /// never asked for persistence.
    pub(super) persist: Option<Box<dyn BasicInfoPersist>>,
}

impl BasicInformationHandler {
    /// Saves the current NodeLabel/Location to `persist`, if any (no-op for
    /// the non-persisted constructors). Called only when a write actually
    /// changes a value — dedup writes never reach here. A save failure is
    /// `tracing::warn`ed and otherwise ignored: the write that triggered it
    /// has already succeeded and updated in-memory state, which stays
    /// authoritative; the next write to either attribute retries the save
    /// (same disposition `AclStore::save` documents for `AclPersist`).
    fn persist_state(&self) {
        if let Some(persist) = &self.persist {
            if let Err(e) = persist.save(&self.node_label, &self.location) {
                tracing::warn!("basic information store save failed: {e}");
            }
        }
    }
}

/// CaseSessionsPerFabric/SubscriptionsPerFabric (spec §11.1.6.16,
/// CapabilityMinimaStruct fields, context tags 0/1) — fixed floor values
/// `mat-device` comfortably supports; not tracked against any real
/// resource-exhaustion path (M2/M3 scope never gets close).
const CAPABILITY_MINIMA_CASE_SESSIONS_PER_FABRIC: u64 = 3;

const CAPABILITY_MINIMA_SUBSCRIPTIONS_PER_FABRIC: u64 = 3;

/// SpecificationVersion (spec §11.1.6.18, attribute id 0x0015): Matter 1.4,
/// encoded per spec §7.1.9 as `(major << 24) | (minor << 16)`.
const SPECIFICATION_VERSION: u64 = 0x0104_0000;

/// NodeLabel's upper bound (spec §11.1.6.2, `string32`) — measured in UTF-8
/// **characters**, not bytes.
const NODE_LABEL_MAX_CHARS: usize = 32;

/// Location's fixed length (spec §11.1.6.6, `CountryCode` — ISO 3166-1
/// alpha-2, or `"XX"` for unset/unknown) — measured in UTF-8 **characters**,
/// not bytes. Unlike NodeLabel this isn't an upper bound: any length other
/// than exactly 2 is rejected.
const LOCATION_CHARS: usize = 2;

/// Decodes a write payload expected to be a single anonymous UTF-8 TLV
/// element — the shape both NodeLabel and Location writes take. Shared by
/// `BasicInformationHandler::write`'s two branches so the
/// malformed-TLV/wrong-type rejection (`STATUS_CONSTRAINT_ERROR`) is
/// written once.
fn decode_utf8_write(data_tlv: &[u8]) -> Result<String, u8> {
    let mut r = Reader::new(data_tlv);
    let Ok(Some(element)) = r.next() else {
        return Err(im::STATUS_CONSTRAINT_ERROR);
    };
    let Value::Utf8(s) = element.value else {
        return Err(im::STATUS_CONSTRAINT_ERROR);
    };
    Ok(s.to_string())
}

impl ClusterHandler for BasicInformationHandler {
    fn cluster_id(&self) -> u32 {
        im::CLUSTER_BASIC_INFORMATION
    }

    /// ClusterRevision (spec §7.13): Basic Information cluster spec
    /// revision 3 (Matter 1.4).
    fn revision(&self) -> u16 {
        3
    }

    fn attributes(&self) -> Vec<u32> {
        vec![
            im::ATTR_DATA_MODEL_REVISION,
            im::ATTR_VENDOR_ID,
            im::ATTR_PRODUCT_ID,
            im::ATTR_VENDOR_NAME,
            im::ATTR_PRODUCT_NAME,
            im::ATTR_BI_NODE_LABEL,
            im::ATTR_BI_LOCATION,
            im::ATTR_BI_HARDWARE_VERSION,
            im::ATTR_BI_HARDWARE_VERSION_STRING,
            im::ATTR_BI_SOFTWARE_VERSION,
            im::ATTR_BI_SOFTWARE_VERSION_STRING,
            im::ATTR_BI_UNIQUE_ID,
            im::ATTR_BI_CAPABILITY_MINIMA,
            im::ATTR_BI_SPECIFICATION_VERSION,
            im::ATTR_BI_MAX_PATHS_PER_INVOKE,
        ]
    }

    fn read(&self, attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
        match attribute {
            im::ATTR_DATA_MODEL_REVISION => Some(tlv_value::uint(DATA_MODEL_REVISION)),
            im::ATTR_VENDOR_ID => Some(tlv_value::uint(u64::from(self.vendor_id))),
            im::ATTR_PRODUCT_ID => Some(tlv_value::uint(u64::from(self.product_id))),
            im::ATTR_VENDOR_NAME => Some(tlv_value::str("mat")),
            im::ATTR_PRODUCT_NAME => Some(tlv_value::str("matv")),
            im::ATTR_BI_NODE_LABEL => Some(tlv_value::str(&self.node_label)),
            im::ATTR_BI_LOCATION => Some(tlv_value::str(&self.location)),
            im::ATTR_BI_HARDWARE_VERSION => Some(tlv_value::uint(1)),
            im::ATTR_BI_HARDWARE_VERSION_STRING => Some(tlv_value::str("matv")),
            im::ATTR_BI_SOFTWARE_VERSION => Some(tlv_value::uint(1)),
            im::ATTR_BI_SOFTWARE_VERSION_STRING => Some(tlv_value::str(env!("CARGO_PKG_VERSION"))),
            im::ATTR_BI_UNIQUE_ID => Some(tlv_value::str(&self.unique_id)),
            im::ATTR_BI_CAPABILITY_MINIMA => Some(encode_capability_minima()),
            im::ATTR_BI_SPECIFICATION_VERSION => Some(tlv_value::uint(SPECIFICATION_VERSION)),
            im::ATTR_BI_MAX_PATHS_PER_INVOKE => Some(tlv_value::uint(1)),
            _ => None,
        }
    }

    fn invoke(&mut self, _command: u32, _fields_tlv: &[u8], _ctx: &mut InvokeCtx) -> InvokeReply {
        // BasicInformation declares no commands.
        InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
    }

    /// NodeLabel and Location are the two writable attributes (spec
    /// §11.1.6.2/§11.1.6.6); every other BasicInformation attribute keeps
    /// the default `write` (`STATUS_UNSUPPORTED_WRITE`). Both branches
    /// dedup (a write equal to the current value is `Ok(())` but neither
    /// reports `ctx.changed` nor calls `persist.save` — Apple Home's
    /// post-commissioning interview writes these unconditionally on every
    /// connect, and a same-value write is not a change worth a dirty
    /// report or a disk write) and, on an actual change, persist the new
    /// `(node_label, location)` pair (`persist_state`'s doc covers save
    /// failure handling). `list_append` is irrelevant to either — neither
    /// is a list — so it's ignored, matching the trait doc's guidance for
    /// clusters with no list attribute.
    fn write(
        &mut self,
        attribute: u32,
        data_tlv: &[u8],
        _list_append: bool,
        ctx: &mut InvokeCtx,
    ) -> Result<(), u8> {
        match attribute {
            im::ATTR_BI_NODE_LABEL => {
                let s = decode_utf8_write(data_tlv)?;
                if s.chars().count() > NODE_LABEL_MAX_CHARS {
                    return Err(im::STATUS_CONSTRAINT_ERROR);
                }
                if s == self.node_label {
                    return Ok(());
                }
                self.node_label = s;
                ctx.changed.push(im::ATTR_BI_NODE_LABEL);
                self.persist_state();
                Ok(())
            }
            im::ATTR_BI_LOCATION => {
                let s = decode_utf8_write(data_tlv)?;
                if s.chars().count() != LOCATION_CHARS {
                    return Err(im::STATUS_CONSTRAINT_ERROR);
                }
                if s == self.location {
                    return Ok(());
                }
                self.location = s;
                ctx.changed.push(im::ATTR_BI_LOCATION);
                self.persist_state();
                Ok(())
            }
            _ => Err(im::STATUS_UNSUPPORTED_WRITE),
        }
    }

    /// spec §11.1.6 のアクセス表: NodeLabel / Location の write は Manage
    /// （read は View のまま = trait default）。書けるのはこの 2 属性だけ
    /// なので属性を問わず Manage を要求する。
    fn write_privilege(&self, _attribute: u32) -> u8 {
        crate::core::access_control::PRIVILEGE_MANAGE
    }
}

/// CapabilityMinima (spec §11.1.6.16, attribute id 0x0013): a
/// `CapabilityMinimaStruct{CaseSessionsPerFabric: uint16, Subscriptions
/// PerFabric: uint16}`, context tags 0/1 in field-declaration order.
fn encode_capability_minima() -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), CAPABILITY_MINIMA_CASE_SESSIONS_PER_FABRIC);
    w.put_uint(Tag::Context(1), CAPABILITY_MINIMA_SUBSCRIPTIONS_PER_FABRIC);
    w.end_container();
    w.finish()
}
