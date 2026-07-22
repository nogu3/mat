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

/// 汎用 write の分類結果（mat 直経路 `native_direct::classify_strict` の
/// `Command::Write` 判定を移設・一本化 — M8a Task10）。
#[derive(Debug, Clone, PartialEq)]
pub enum WriteClass {
    /// native で実行可能。`cluster` / `attribute` は数値 ID、`value` は型に沿って
    /// 符号化済みのスカラー。cluster ID を含めるのは、呼び手に resolve_cluster の
    /// 再解決(classifier との drift で panic し得る)をさせないため。
    Native {
        cluster: u32,
        attribute: u32,
        value: ScalarValue,
        timed: bool,
    },
    /// cluster/attribute 名は解決できたが値が符号化不能（list/struct/float 等）。
    /// 呼び出し側は chip-tool へフォールバックせず即 parse_error を返すこと
    /// （spec 決定: opt-in 下の意図した縮小）。
    Reject(String),
    /// cluster/attribute 名を解決できない（chip-tool へフォールバック）。
    NotNative,
}

/// write op の分類: cluster/attribute 名を解決し、値を属性の型（数値直指定なら
/// 推定型）でスカラー化する。挙動は移設元（`native_direct::classify_strict` の
/// `Command::Write` 腕）と同一 — エラーメッセージ文言も維持。
pub fn classify_write(cluster: &str, attribute: &str, value: &str) -> WriteClass {
    let Some(cluster_id) = resolve_cluster(cluster) else {
        return WriteClass::NotNative;
    };
    let Some(attr) = resolve_attribute(cluster_id, attribute) else {
        return WriteClass::NotNative;
    };
    let timed = attr.def.map(|d| d.timed_write).unwrap_or(false);
    let parsed = match attr.def {
        Some(def) => parse_scalar_typed(value, def.ty),
        None => Ok(parse_scalar_inferred(value)),
    };
    match parsed {
        Ok(v) => WriteClass::Native {
            cluster: cluster_id,
            attribute: attr.id,
            value: v,
            timed,
        },
        Err(msg) => WriteClass::Reject(format!("write {cluster}/{attribute}: {msg}")),
    }
}

/// 汎用 invoke の分類結果（mat 直経路 `native_direct::classify_strict` の
/// `Command::Invoke` / `GroupCommand::Invoke` 判定を移設・一本化 — M8a Task10。
/// 単体 invoke と group invoke の判定ロジックはこれまで ~50 行重複していた
/// — この型がその一本化の受け皿）。
#[derive(Debug, Clone, PartialEq)]
pub enum InvokeClass {
    /// native で実行可能。`cluster` は数値 ID、`command` は数値 ID、`fields` は
    /// 引数を位置順にスカラー化した列（呼び出し側が CommandFields TLV へ符号化
    /// する）。cluster ID を含めるのは、呼び手に resolve_cluster の再解決
    /// （classifier との drift で panic し得る）をさせないため。
    Native {
        cluster: u32,
        command: u32,
        fields: Vec<ScalarValue>,
        timed: bool,
    },
    /// cluster/command 名は解決できたが引数が符号化不能（多すぎる/非スカラー型）。
    /// 呼び出し側は chip-tool へフォールバックせず即 parse_error を返すこと。
    Reject(String),
    /// cluster/command 名を解決できない（chip-tool へフォールバック）。
    NotNative,
}

/// invoke op の分類: cluster/command 名を解決し、引数をコマンド定義の field 型で
/// 順にスカラー化する。数値 ID 直指定（def なし）は引数なしのみ native
/// （引数ありは型不明のため非対象）。エラーメッセージ文言は移設元と同一
/// （"invoke ..." プレフィックス — 旧 group invoke 経路の "group invoke ..."
/// 文言とは統合により差異が生じるが、その文言を検査する既存テストは無い）。
pub fn classify_invoke(cluster: &str, command: &str, args: &[String]) -> InvokeClass {
    let Some(cluster_id) = resolve_cluster(cluster) else {
        return InvokeClass::NotNative;
    };
    let Some(cmd) = resolve_command(cluster_id, command) else {
        return InvokeClass::NotNative;
    };
    match cmd.def {
        Some(def) => {
            if args.len() > def.fields.len() {
                return InvokeClass::Reject(format!(
                    "invoke {cluster}/{command}: too many arguments ({} > {})",
                    args.len(),
                    def.fields.len()
                ));
            }
            let mut values = Vec::with_capacity(args.len());
            for (i, arg) in args.iter().enumerate() {
                match parse_scalar_typed(arg, def.fields[i].ty) {
                    Ok(v) => values.push(v),
                    Err(msg) => {
                        return InvokeClass::Reject(format!(
                            "invoke {cluster}/{command} arg {i} ({}): {msg}",
                            def.fields[i].name
                        ));
                    }
                }
            }
            InvokeClass::Native {
                cluster: cluster_id,
                command: cmd.id,
                fields: values,
                timed: def.timed,
            }
        }
        // 数値直指定（def なし）: 引数の型が不明なので、引数ありは native
        // 対象外（chip-tool へ）。引数なしのみ native。
        None => {
            if !args.is_empty() {
                return InvokeClass::NotNative;
            }
            InvokeClass::Native {
                cluster: cluster_id,
                command: cmd.id,
                fields: Vec::new(),
                timed: false,
            }
        }
    }
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
    fn classify_write_native_for_known_scalar_attribute() {
        let c = classify_write("levelcontrol", "on-level", "128");
        assert_eq!(
            c,
            WriteClass::Native {
                cluster: 0x0008,
                attribute: 0x0011,
                value: ScalarValue::UInt(128),
                timed: false,
            }
        );
    }

    #[test]
    fn classify_write_not_native_for_unknown_names() {
        assert_eq!(
            classify_write("nosuchcluster", "x", "1"),
            WriteClass::NotNative
        );
        assert_eq!(
            classify_write("onoff", "nosuchattr", "1"),
            WriteClass::NotNative
        );
    }

    #[test]
    fn classify_write_rejects_list_type_with_parse_error_message() {
        let c = classify_write("accesscontrol", "acl", "[]");
        match c {
            WriteClass::Reject(msg) => {
                assert!(msg.contains("list"), "{msg}");
                assert!(msg.starts_with("write accesscontrol/acl:"), "{msg}");
            }
            other => panic!("expected Reject, got {other:?}"),
        }
    }

    #[test]
    fn classify_invoke_native_for_known_command_with_scalar_args() {
        let c = classify_invoke(
            "levelcontrol",
            "move-to-level",
            &["128".into(), "0".into(), "0".into(), "0".into()],
        );
        assert_eq!(
            c,
            InvokeClass::Native {
                cluster: 0x0008,
                command: 0x00,
                fields: vec![
                    ScalarValue::UInt(128),
                    ScalarValue::UInt(0),
                    ScalarValue::UInt(0),
                    ScalarValue::UInt(0),
                ],
                timed: false,
            }
        );
    }

    #[test]
    fn classify_invoke_not_native_for_unknown_names() {
        assert_eq!(
            classify_invoke("nosuchcluster", "x", &[]),
            InvokeClass::NotNative
        );
        assert_eq!(
            classify_invoke("onoff", "nosuchcmd", &[]),
            InvokeClass::NotNative
        );
    }

    #[test]
    fn classify_invoke_rejects_too_many_or_non_scalar_args() {
        // 引数過多。
        let c = classify_invoke("onoff", "on", &["1".into()]);
        match c {
            InvokeClass::Reject(msg) => assert!(msg.contains("too many arguments"), "{msg}"),
            other => panic!("expected Reject, got {other:?}"),
        }
        // struct field を要求するコマンドへの引数。
        let c = classify_invoke("groupkeymanagement", "key-set-write", &["{}".into()]);
        assert!(matches!(c, InvokeClass::Reject(_)));
    }

    #[test]
    fn classify_invoke_numeric_id_without_args_is_native() {
        // 数値直指定（def なし）: 引数なしのみ native。
        let c = classify_invoke("6", "1", &[]);
        assert_eq!(
            c,
            InvokeClass::Native {
                cluster: 6,
                command: 1,
                fields: vec![],
                timed: false,
            }
        );
        assert_eq!(
            classify_invoke("6", "1", &["1".into()]),
            InvokeClass::NotNative
        );
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
