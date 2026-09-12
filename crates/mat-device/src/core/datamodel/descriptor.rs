use super::*;

/// Descriptor cluster (spec §9.5), mandatory on every endpoint. Carries the
/// endpoint's `DeviceTypeList` entries (`device_types` — `DEVICE_TYPE_ROOT_NODE`
/// on endpoint 0, `DEVICE_TYPE_ON_OFF_LIGHT` on endpoint 1; a bridged endpoint
/// carries two entries — its "real" device type plus `DEVICE_TYPE_BRIDGED_NODE`,
/// spec §9.13) since that's the one piece of per-endpoint Descriptor state
/// this flat, non-composed data model needs; `ServerList`/endpoint-0
/// `PartsList` are derived from the registry by `Node::read_attribute_value`
/// instead (see its doc comment) because they depend on sibling/other-endpoint
/// state this handler can't see.
pub struct DescriptorHandler {
    device_types: Vec<u32>,
    /// この endpoint 自身の PartsList（EP0 以外用 — EP0 は従来どおり
    /// `Node::read_attribute_value` が registry から導出して intercept）。
    /// EP1 Aggregator が bridged EP 群を静的に持つ（設定反映は再起動のみ
    /// なので動的導出は不要 — YAGNI）。
    parts: Vec<u16>,
}

impl DescriptorHandler {
    /// A Descriptor handler for an endpoint whose `DeviceTypeList` is the
    /// single entry `device_type` (revision 1 — M2 scope has no device type
    /// revisions beyond the first), with an empty `PartsList`.
    pub fn for_device(device_type: u32) -> Self {
        Self {
            device_types: vec![device_type],
            parts: Vec::new(),
        }
    }

    /// A Descriptor handler for an endpoint whose `DeviceTypeList` carries
    /// multiple entries (each revision 1) — a bridged endpoint's "real"
    /// device type plus `DEVICE_TYPE_BRIDGED_NODE`, per spec §9.13.
    pub fn for_device_types(device_types: &[u32]) -> Self {
        Self {
            device_types: device_types.to_vec(),
            parts: Vec::new(),
        }
    }

    /// Builder: sets a static `PartsList` — the Aggregator endpoint's
    /// bridged children. Only meaningful on a non-zero endpoint (endpoint
    /// 0's `PartsList` is always derived by `Node::read_attribute_value`,
    /// which intercepts it before this handler's `read` runs).
    pub fn with_parts(mut self, parts: Vec<u16>) -> Self {
        self.parts = parts;
        self
    }
}

impl ClusterHandler for DescriptorHandler {
    fn cluster_id(&self) -> u32 {
        im::CLUSTER_DESCRIPTOR
    }

    /// ClusterRevision (spec §7.13): Descriptor cluster spec revision 2
    /// (Matter 1.4).
    fn revision(&self) -> u16 {
        2
    }

    fn attributes(&self) -> Vec<u32> {
        vec![
            im::ATTR_DEVICE_TYPE_LIST,
            im::ATTR_SERVER_LIST,
            im::ATTR_PARTS_LIST,
        ]
    }

    fn read(&self, attribute: u32, _ctx: &ReadCtx) -> Option<Vec<u8>> {
        match attribute {
            im::ATTR_DEVICE_TYPE_LIST => {
                let mut w = Writer::new();
                w.start_array(Tag::Anonymous);
                for device_type in &self.device_types {
                    w.start_struct(Tag::Anonymous); // DeviceTypeStruct
                    w.put_uint(Tag::Context(0), u64::from(*device_type));
                    w.put_uint(Tag::Context(1), 1); // Revision
                    w.end_container();
                }
                w.end_container();
                Some(w.finish())
            }
            // ATTR_SERVER_LIST, and ATTR_PARTS_LIST on endpoint 0, are
            // intercepted and derived from the `Node`'s registry by
            // `Node::read_attribute_value` — never reach here (see that
            // override's doc comment). This is endpoint != 0's PartsList
            // (`self.parts` — empty unless `with_parts` set it, as on the
            // EP1 Aggregator) and endpoint 0's own fallback, which
            // `read_attribute_value` never takes.
            im::ATTR_PARTS_LIST => {
                let mut w = Writer::new();
                w.start_array(Tag::Anonymous);
                for id in &self.parts {
                    w.put_uint(Tag::Anonymous, u64::from(*id));
                }
                w.end_container();
                Some(w.finish())
            }
            _ => None,
        }
    }

    fn invoke(&mut self, _command: u32, _fields_tlv: &[u8], _ctx: &mut InvokeCtx) -> InvokeReply {
        // Descriptor declares no commands.
        InvokeReply::Status(im::STATUS_UNSUPPORTED_COMMAND)
    }
}
