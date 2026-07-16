//! chip-tool 記法の cluster/attribute/command 名 → Matter 数値 ID の解決。
//!
//! テーブルは `ids_gen.rs`（scripts/gen-ids.py で connectedhomeip v1.4.2.0 から
//! 生成、チェックイン）。名前の意味論は chip-tool 記法のまま（CLAUDE.md）。
//! 数値直指定（"10" / "0x0A"）は常に許可 — その場合 `def` は `None` で、
//! write の型推定は値リテラルから行う（Task 3）。

use super::ids_gen::CLUSTERS;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeTag {
    Bool,
    UInt,
    Int,
    Float,
    Str,
    Bytes,
    List,
    Struct,
    Unknown,
}

pub struct ClusterDef {
    pub name: &'static str,
    pub id: u32,
    pub attrs: &'static [AttrDef],
    pub cmds: &'static [CmdDef],
}
pub struct AttrDef {
    pub name: &'static str,
    pub id: u32,
    pub ty: TypeTag,
    pub writable: bool,
    pub timed_write: bool,
}
pub struct CmdDef {
    pub name: &'static str,
    pub id: u32,
    pub timed: bool,
    pub fields: &'static [FieldDef],
}
/// TLV context tag は `CmdDef::fields` 内の添字（0-based）。
pub struct FieldDef {
    pub name: &'static str,
    pub ty: TypeTag,
    pub optional: bool,
}

pub struct AttrRef {
    pub id: u32,
    pub def: Option<&'static AttrDef>,
}
pub struct CmdRef {
    pub id: u32,
    pub def: Option<&'static CmdDef>,
}

pub fn parse_num(input: &str) -> Option<u64> {
    let s = input.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        return u64::from_str_radix(hex, 16).ok();
    }
    s.parse::<u64>().ok()
}

pub fn resolve_cluster(input: &str) -> Option<u32> {
    if let Some(n) = parse_num(input) {
        return u32::try_from(n).ok();
    }
    CLUSTERS
        .binary_search_by(|c| c.name.cmp(input))
        .ok()
        .map(|i| CLUSTERS[i].id)
}

pub fn find_cluster(id: u32) -> Option<&'static ClusterDef> {
    CLUSTERS.iter().find(|c| c.id == id)
}

pub fn resolve_attribute(cluster: u32, input: &str) -> Option<AttrRef> {
    if let Some(n) = parse_num(input) {
        return u32::try_from(n).ok().map(|id| AttrRef { id, def: None });
    }
    let def = find_cluster(cluster)?
        .attrs
        .iter()
        .find(|a| a.name == input)?;
    Some(AttrRef {
        id: def.id,
        def: Some(def),
    })
}

pub fn resolve_command(cluster: u32, input: &str) -> Option<CmdRef> {
    if let Some(n) = parse_num(input) {
        return u32::try_from(n).ok().map(|id| CmdRef { id, def: None });
    }
    let def = find_cluster(cluster)?
        .cmds
        .iter()
        .find(|c| c.name == input)?;
    Some(CmdRef {
        id: def.id,
        def: Some(def),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_known_cluster_names_and_ids() {
        // 生成テーブルのスポットチェック: 既知の chip-tool 名 → 既知 ID。
        assert_eq!(resolve_cluster("onoff"), Some(0x0006));
        assert_eq!(resolve_cluster("colorcontrol"), Some(0x0300));
        assert_eq!(resolve_cluster("threadnetworkdiagnostics"), Some(0x0035));
        assert_eq!(resolve_cluster("accesscontrol"), Some(0x001F));
        assert_eq!(resolve_cluster("descriptor"), Some(0x001D));
        assert_eq!(resolve_cluster("groupkeymanagement"), Some(0x003F));
        assert_eq!(resolve_cluster("groups"), Some(0x0004));
        assert_eq!(resolve_cluster("levelcontrol"), Some(0x0008));
        // 数値直指定（10進 / 16進）。
        assert_eq!(resolve_cluster("6"), Some(6));
        assert_eq!(resolve_cluster("0x0300"), Some(0x0300));
        // 未知名は None。
        assert_eq!(resolve_cluster("nosuchcluster"), None);
    }

    #[test]
    fn resolves_known_attributes_with_types() {
        let a = resolve_attribute(0x0006, "on-off").unwrap();
        assert_eq!(a.id, 0x0000);
        assert_eq!(a.def.unwrap().ty, TypeTag::Bool);
        let a = resolve_attribute(0x0300, "color-temperature-mireds").unwrap();
        assert_eq!(a.id, 0x0007);
        assert_eq!(a.def.unwrap().ty, TypeTag::UInt);
        let a = resolve_attribute(0x0035, "neighbor-table").unwrap();
        assert_eq!(a.id, 0x0007);
        assert_eq!(a.def.unwrap().ty, TypeTag::List);
        let a = resolve_attribute(0x001F, "acl").unwrap();
        assert_eq!(a.id, 0x0000);
        assert_eq!(a.def.unwrap().ty, TypeTag::List);
        let a = resolve_attribute(0x003F, "group-key-map").unwrap();
        assert_eq!(a.id, 0x0000);
        assert_eq!(a.def.unwrap().ty, TypeTag::List);
        let a = resolve_attribute(0x001D, "parts-list").unwrap();
        assert_eq!(a.id, 0x0003);
        // descriptor server-list。
        let a = resolve_attribute(0x001D, "server-list").unwrap();
        assert_eq!(a.id, 0x0001);
        // 数値直指定は def なしで通る。
        let a = resolve_attribute(0x0006, "0x4001").unwrap();
        assert_eq!(a.id, 0x4001);
        assert!(a.def.is_none());
    }

    #[test]
    fn resolves_known_commands_with_fields() {
        let c = resolve_command(0x0006, "on").unwrap();
        assert_eq!(c.id, 0x01);
        assert!(c.def.unwrap().fields.is_empty());
        let c = resolve_command(0x0300, "move-to-color-temperature").unwrap();
        assert_eq!(c.id, 0x0A);
        // fields: ColorTemperatureMireds, TransitionTime, OptionsMask, OptionsOverride
        assert_eq!(c.def.unwrap().fields.len(), 4);
        assert_eq!(c.def.unwrap().fields[0].ty, TypeTag::UInt);
        let c = resolve_command(0x003F, "key-set-write").unwrap();
        assert_eq!(c.id, 0x00);
        // KeySetWrite の field 0 は GroupKeySetStruct。
        assert_eq!(c.def.unwrap().fields[0].ty, TypeTag::Struct);
        let c = resolve_command(0x0004, "add-group").unwrap();
        assert_eq!(c.id, 0x00);
        // open-commissioning-window は timed invoke 必須。
        let c = resolve_command(0x003C, "open-commissioning-window").unwrap();
        assert!(c.def.unwrap().timed);
    }

    #[test]
    fn parse_num_accepts_dec_and_hex() {
        assert_eq!(parse_num("10"), Some(10));
        assert_eq!(parse_num("0x0A"), Some(10));
        assert_eq!(parse_num("0X0a"), Some(10));
        assert_eq!(parse_num(""), None);
        assert_eq!(parse_num("-1"), None);
        assert_eq!(parse_num("on-off"), None);
    }

    #[test]
    fn global_attributes_resolve_on_every_cluster() {
        // global ZCL 属性は全クラスタで名前解決できる（chip-tool は全クラスタで受ける）。
        for cluster in [0x0006u32, 0x0300, 0x0035, 0x001D] {
            let a = resolve_attribute(cluster, "feature-map").unwrap();
            assert_eq!(a.id, 0xFFFC);
            assert_eq!(a.def.unwrap().ty, TypeTag::UInt);
            let a = resolve_attribute(cluster, "cluster-revision").unwrap();
            assert_eq!(a.id, 0xFFFD);
            let a = resolve_attribute(cluster, "attribute-list").unwrap();
            assert_eq!(a.id, 0xFFFB);
            assert_eq!(a.def.unwrap().ty, TypeTag::List);
        }
    }

    #[test]
    fn numeric_ids_beyond_u32_are_rejected() {
        assert!(resolve_attribute(0x0006, "0x100000001").is_none());
        assert_eq!(resolve_cluster("0x100000001"), None);
    }
}
