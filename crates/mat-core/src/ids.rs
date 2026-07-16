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

/// write / invoke 引数のスカラー値（mat-controller の ImValue と同形。
/// mat-core は mat-controller に依存できないため別型で持ち、mat-native 側で写す）。
#[derive(Debug, Clone, PartialEq)]
pub enum ScalarValue {
    Bool(bool),
    UInt(u64),
    Int(i64),
    Str(String),
    Bytes(Vec<u8>),
    Null,
}

fn parse_hex_bytes(s: &str) -> Result<Vec<u8>, String> {
    let h = s
        .strip_prefix("hex:")
        .ok_or("bytes value must use hex: prefix")?;
    if h.len() % 2 != 0 {
        return Err(format!("odd-length hex literal: {s:?}"));
    }
    (0..h.len())
        .step_by(2)
        .map(|i| {
            u8::from_str_radix(&h[i..i + 2], 16).map_err(|_| format!("invalid hex literal: {s:?}"))
        })
        .collect()
}

/// 型タグに従って CLI 入力文字列をスカラーへ。Err は人間可読の理由
/// （そのまま parse_error detail に使える）。
pub fn parse_scalar_typed(input: &str, ty: TypeTag) -> Result<ScalarValue, String> {
    let s = input.trim();
    if s == "null" {
        return Ok(ScalarValue::Null); // nullable 属性の消去 write。
    }
    match ty {
        TypeTag::Bool => match s {
            "true" | "1" => Ok(ScalarValue::Bool(true)),
            "false" | "0" => Ok(ScalarValue::Bool(false)),
            _ => Err(format!("not a bool literal: {s:?}")),
        },
        TypeTag::UInt => parse_num(s)
            .map(ScalarValue::UInt)
            .ok_or(format!("not an unsigned integer: {s:?}")),
        TypeTag::Int => s
            .parse::<i64>()
            .map(ScalarValue::Int)
            .map_err(|_| format!("not an integer: {s:?}")),
        TypeTag::Str => Ok(ScalarValue::Str(s.to_string())),
        TypeTag::Bytes => parse_hex_bytes(s).map(ScalarValue::Bytes),
        TypeTag::List => Err(
            "this attribute is a list type; generic native write supports scalars only (M8a)"
                .into(),
        ),
        TypeTag::Struct => Err(
            "this attribute is a struct type; generic native write supports scalars only (M8a)"
                .into(),
        ),
        TypeTag::Float => {
            Err("float attributes are not supported by generic native write (M8a)".into())
        }
        TypeTag::Unknown => Err("attribute type unknown; cannot encode value".into()),
    }
}

/// 数値 ID 直指定（def 無し）用: JSON リテラル風に型推定する。
/// true/false→Bool, null→Null, 整数→UInt(負なら Int), "hex:AABB"→Bytes, その他→Str。
pub fn parse_scalar_inferred(input: &str) -> ScalarValue {
    let s = input.trim();
    match s {
        "true" => return ScalarValue::Bool(true),
        "false" => return ScalarValue::Bool(false),
        "null" => return ScalarValue::Null,
        _ => {}
    }
    if let Ok(b) = parse_hex_bytes(s) {
        return ScalarValue::Bytes(b);
    }
    if let Some(u) = parse_num(s) {
        return ScalarValue::UInt(u);
    }
    if let Ok(i) = s.parse::<i64>() {
        return ScalarValue::Int(i);
    }
    ScalarValue::Str(s.to_string())
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

    #[test]
    fn parse_scalar_typed_scalars() {
        use ScalarValue as V;
        assert_eq!(parse_scalar_typed("true", TypeTag::Bool), Ok(V::Bool(true)));
        assert_eq!(parse_scalar_typed("0", TypeTag::Bool), Ok(V::Bool(false)));
        assert_eq!(parse_scalar_typed("1", TypeTag::Bool), Ok(V::Bool(true)));
        assert_eq!(parse_scalar_typed("128", TypeTag::UInt), Ok(V::UInt(128)));
        assert_eq!(parse_scalar_typed("0x80", TypeTag::UInt), Ok(V::UInt(128)));
        assert_eq!(parse_scalar_typed("-5", TypeTag::Int), Ok(V::Int(-5)));
        assert_eq!(
            parse_scalar_typed("hello", TypeTag::Str),
            Ok(V::Str("hello".into()))
        );
        assert_eq!(
            parse_scalar_typed("hex:d0d1", TypeTag::Bytes),
            Ok(V::Bytes(vec![0xd0, 0xd1]))
        );
        assert_eq!(parse_scalar_typed("null", TypeTag::UInt), Ok(V::Null));
    }

    #[test]
    fn parse_scalar_typed_rejects_unsupported_and_bad_literals() {
        assert!(parse_scalar_typed("[]", TypeTag::List).is_err());
        assert!(parse_scalar_typed("{}", TypeTag::Struct).is_err());
        assert!(parse_scalar_typed("1.5", TypeTag::Float).is_err()); // float write は M8a 未対応
        assert!(parse_scalar_typed("abc", TypeTag::UInt).is_err());
        assert!(parse_scalar_typed("xyz", TypeTag::Bool).is_err());
        assert!(parse_scalar_typed("hex:zz", TypeTag::Bytes).is_err());
        assert!(parse_scalar_typed("1", TypeTag::Unknown).is_err());
        // エラーメッセージは型名を含む（spec 受け入れ5: AI が判断できる detail）。
        let e = parse_scalar_typed("[]", TypeTag::List).unwrap_err();
        assert!(e.contains("list"), "{e}");
    }

    #[test]
    fn parse_scalar_inferred_literals() {
        use ScalarValue as V;
        assert_eq!(parse_scalar_inferred("true"), V::Bool(true));
        assert_eq!(parse_scalar_inferred("null"), V::Null);
        assert_eq!(parse_scalar_inferred("42"), V::UInt(42));
        assert_eq!(parse_scalar_inferred("-1"), V::Int(-1));
        assert_eq!(parse_scalar_inferred("hex:00ff"), V::Bytes(vec![0, 0xff]));
        assert_eq!(parse_scalar_inferred("foo"), V::Str("foo".into()));
    }
}
