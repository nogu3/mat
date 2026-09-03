//! chip-tool 記法の cluster/attribute/command 名 → Matter 数値 ID の解決。
//!
//! テーブルは `ids_gen.rs`（scripts/gen-ids.py で connectedhomeip v1.4.2.0 から
//! 生成、チェックイン）。名前の意味論は chip-tool 記法のまま（CLAUDE.md）。
//! 数値直指定（"10" / "0x0A"）は常に許可 — その場合 `def` は `None` で、
//! write の型推定は値リテラルから行う（Task 3）。

use super::ids_gen::CLUSTERS;

/// スカラー型。list / struct の形は `Ty` が持つ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeTag {
    Bool,
    UInt,
    Int,
    /// Matter `single`（TLV 0x0A）。
    F32,
    /// Matter `double`（TLV 0x0B）。
    F64,
    Str,
    Bytes,
    /// 生成テーブルが型を解決できなかった（符号化は拒否）。
    Unknown,
}

/// 属性 / コマンド引数 / struct フィールドに共通の型記述。
#[derive(Clone, Copy)]
pub enum Ty {
    Scalar(TypeTag),
    Struct(&'static StructDef),
    /// スカラー要素の list（Matter の `array`、TLV Array）。
    List(TypeTag),
    ListOfStruct(&'static StructDef),
}

// 派生 Debug はネストした struct 定義を丸ごと展開してしまうので手書きする。
impl std::fmt::Debug for Ty {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Ty::Scalar(t) => write!(f, "Scalar({t:?})"),
            Ty::Struct(d) => write!(f, "Struct({})", d.name),
            Ty::List(t) => write!(f, "List({t:?})"),
            Ty::ListOfStruct(d) => write!(f, "ListOfStruct({})", d.name),
        }
    }
}

impl Ty {
    /// 人間可読の型名（parse_error detail 用）。
    pub fn describe(&self) -> String {
        format!("{self:?}").to_lowercase()
    }
}

/// struct 型の定義（ids_gen.rs の `S_*` static）。`fields` は fieldId 昇順。
pub struct StructDef {
    pub name: &'static str,
    pub fields: &'static [StructField],
}
/// struct のフィールド。`id` は TLV context tag。fabric-scoped struct には生成側が
/// `fabric-index`(254, optional) を末尾に付ける — read 出力の `"254"` をそのまま
/// 書き戻せるようにするため（サーバは書込時にこの値を無視・置換する）。
pub struct StructField {
    pub name: &'static str,
    pub id: u8,
    pub ty: Ty,
    pub optional: bool,
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
    pub ty: Ty,
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
    pub ty: Ty,
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

/// write / invoke 引数の値ツリー。名前は歴史的に `ScalarValue` だが list /
/// struct も持つ（改名は mat-native::op と同時に行う — 直列後回しの可読性分割）。
/// mat-controller の ImValue と同形のスカラー + container。mat-core は
/// mat-controller に依存できないため別型で持ち、mat-native 側で TLV に写す。
#[derive(Debug, Clone, PartialEq)]
pub enum ScalarValue {
    Bool(bool),
    UInt(u64),
    Int(i64),
    F32(f32),
    F64(f64),
    Str(String),
    Bytes(Vec<u8>),
    Null,
    /// TLV Array。要素は全て同じ型記述。
    List(Vec<ScalarValue>),
    /// TLV Struct。(context tag = fieldId, 値)、id 昇順に整列済み。
    Struct(Vec<(u8, ScalarValue)>),
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

/// float リテラル（`1.5` / `-3` / `2e-3`）。nan / inf は拒否 — TLV には載るが
/// デバイス側は CONSTRAINT_ERROR にしかならないので早期に parse_error にする。
fn parse_finite_f64(s: &str) -> Result<f64, String> {
    match s.parse::<f64>() {
        Ok(f) if f.is_finite() => Ok(f),
        _ => Err(format!("not a finite float literal: {s:?}")),
    }
}

/// f64 → f32。f32 の範囲を超える値（`1e300` 等）は inf に飽和するので拒否する
/// （spec §2.2: nan / inf は送らない）。
fn to_finite_f32(f: f64) -> Result<f32, String> {
    let x = f as f32;
    if x.is_finite() {
        Ok(x)
    } else {
        Err(format!(
            "float value {f} is out of range for a single-precision (f32) attribute"
        ))
    }
}

/// 型記述に従って CLI 入力文字列を値へ。Err は人間可読の理由（そのまま
/// parse_error detail に使える）。スカラーはリテラル構文、list / struct は
/// JSON（名前キー・番号キーの両方を受け付ける）。
pub fn parse_value_typed(input: &str, ty: &Ty) -> Result<ScalarValue, String> {
    let s = input.trim();
    match ty {
        Ty::Scalar(tag) => parse_scalar_literal(s, *tag),
        other => {
            let json: serde_json::Value = serde_json::from_str(s)
                .map_err(|e| format!("{} value must be JSON: {e}", other.describe()))?;
            json_to_value(&json, other, "value")
        }
    }
}

/// JSON を型記述で検査しつつ値ツリーへ。`at` はエラー位置（`value[0].targets` 等）。
fn json_to_value(j: &serde_json::Value, ty: &Ty, at: &str) -> Result<ScalarValue, String> {
    if j.is_null() {
        return Ok(ScalarValue::Null); // nullable かどうかはデバイスが検証する。
    }
    match *ty {
        Ty::Scalar(tag) => json_scalar(j, tag, at),
        Ty::List(elem) => json_list(j, &Ty::Scalar(elem), at),
        Ty::ListOfStruct(def) => json_list(j, &Ty::Struct(def), at),
        Ty::Struct(def) => {
            let obj = j
                .as_object()
                .ok_or_else(|| format!("{at}: expected a JSON object ({}), got {j}", def.name))?;
            json_struct(obj, def, at)
        }
    }
}

fn json_list(j: &serde_json::Value, elem: &Ty, at: &str) -> Result<ScalarValue, String> {
    let arr = j.as_array().ok_or_else(|| {
        format!(
            "{at}: expected a JSON array (list of {}), got {j}",
            elem.describe()
        )
    })?;
    arr.iter()
        .enumerate()
        .map(|(i, e)| json_to_value(e, elem, &format!("{at}[{i}]")))
        .collect::<Result<Vec<_>, _>>()
        .map(ScalarValue::List)
}

fn json_scalar(j: &serde_json::Value, tag: TypeTag, at: &str) -> Result<ScalarValue, String> {
    use serde_json::Value as J;
    let bad = |want: &str| format!("{at}: expected {want}, got {j}");
    match tag {
        TypeTag::Bool => j
            .as_bool()
            .map(ScalarValue::Bool)
            .ok_or_else(|| bad("a bool")),
        TypeTag::UInt => match j {
            J::Number(n) => n.as_u64().map(ScalarValue::UInt),
            J::String(s) => parse_num(s).map(ScalarValue::UInt),
            _ => None,
        }
        .ok_or_else(|| bad("an unsigned integer (number or \"0x..\" string)")),
        TypeTag::Int => match j {
            J::Number(n) => n.as_i64().map(ScalarValue::Int),
            J::String(s) => s.trim().parse::<i64>().ok().map(ScalarValue::Int),
            _ => None,
        }
        .ok_or_else(|| bad("an integer")),
        TypeTag::F32 => match j.as_f64() {
            Some(f) => to_finite_f32(f)
                .map(ScalarValue::F32)
                .map_err(|e| format!("{at}: {e}")),
            None => Err(bad("a number")),
        },
        TypeTag::F64 => j
            .as_f64()
            .map(ScalarValue::F64)
            .ok_or_else(|| bad("a number")),
        TypeTag::Str => j
            .as_str()
            .map(|s| ScalarValue::Str(s.to_string()))
            .ok_or_else(|| bad("a string")),
        TypeTag::Bytes => {
            let s = j.as_str().ok_or_else(|| bad("a hex string"))?;
            let h = s.strip_prefix("hex:").unwrap_or(s);
            parse_hex_bytes(&format!("hex:{h}"))
                .map(ScalarValue::Bytes)
                .map_err(|e| format!("{at}: {e}"))
        }
        TypeTag::Unknown => Err(format!("{at}: type unknown; cannot encode value")),
    }
}

fn json_struct(
    obj: &serde_json::Map<String, serde_json::Value>,
    def: &StructDef,
    at: &str,
) -> Result<ScalarValue, String> {
    let mut out: Vec<(u8, ScalarValue)> = Vec::with_capacity(obj.len());
    for (k, v) in obj {
        let field = def
            .fields
            .iter()
            .find(|f| f.name == k)
            .or_else(|| {
                k.parse::<u8>()
                    .ok()
                    .and_then(|id| def.fields.iter().find(|f| f.id == id))
            })
            .ok_or_else(|| {
                let valid: Vec<&str> = def.fields.iter().map(|f| f.name).collect();
                format!(
                    "{at}: unknown field {k:?} of {}; valid fields: {}",
                    def.name,
                    valid.join(", ")
                )
            })?;
        if out.iter().any(|(id, _)| *id == field.id) {
            return Err(format!(
                "{at}: field {:?} given twice (by name and by id)",
                field.name
            ));
        }
        out.push((
            field.id,
            json_to_value(v, &field.ty, &format!("{at}.{}", field.name))?,
        ));
    }
    for f in def.fields {
        if !f.optional && !out.iter().any(|(id, _)| *id == f.id) {
            return Err(format!(
                "{at}: missing required field {:?} of {}",
                f.name, def.name
            ));
        }
    }
    out.sort_by_key(|(id, _)| *id);
    Ok(ScalarValue::Struct(out))
}

fn parse_scalar_literal(s: &str, ty: TypeTag) -> Result<ScalarValue, String> {
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
        TypeTag::F32 => parse_finite_f64(s).and_then(|f| to_finite_f32(f).map(ScalarValue::F32)),
        TypeTag::F64 => parse_finite_f64(s).map(ScalarValue::F64),
        TypeTag::Str => Ok(ScalarValue::Str(s.to_string())),
        TypeTag::Bytes => parse_hex_bytes(s).map(ScalarValue::Bytes),
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
    if (s.contains('.') || s.contains(['e', 'E'])) && !s.starts_with("0x") {
        if let Ok(f) = parse_finite_f64(s) {
            return ScalarValue::F64(f);
        }
    }
    ScalarValue::Str(s.to_string())
}

/// 汎用 write の分類結果（旧 mat 直経路 `native_direct::classify_strict` の
/// `Command::Write` 判定を移設・一本化 — M8a Task10。現在の呼び出し口は
/// `mat_native::op::NodeOpKind::write`）。
#[derive(Debug, Clone, PartialEq)]
pub enum WriteClass {
    /// native で実行可能。`cluster` / `attribute` は数値 ID、`value` は型記述に沿って
    /// 値ツリー化済み（scalar / list / struct）。cluster ID を含めるのは、呼び手に resolve_cluster の
    /// 再解決(classifier との drift で panic し得る)をさせないため。
    Native {
        cluster: u32,
        attribute: u32,
        value: ScalarValue,
        timed: bool,
    },
    /// cluster/attribute 名は解決できたが値が型記述に合わない（不正なリテラル /
    /// JSON 形不一致 / 未知・欠落フィールド、数値 ID 直指定での list/struct 等）。
    /// 呼び出し側は chip-tool へフォールバックせず即 parse_error を返すこと
    /// （spec 決定: opt-in 下の意図した縮小）。
    Reject(String),
    /// cluster/attribute 名を解決できない（chip-tool へフォールバック）。
    NotNative,
}

/// 数値 ID 直指定には型情報が無い — list/struct 風の値は符号化できないので拒否。
fn reject_container_literal(what: &str, v: &str) -> Option<String> {
    let t = v.trim();
    (t.starts_with('[') || t.starts_with('{')).then(|| {
        format!(
            "{what}: list/struct values need a cluster/attribute name the table knows (numeric ids carry no schema)"
        )
    })
}

/// write op の分類: cluster/attribute 名を解決し、値を属性の型（数値直指定なら
/// 推定型）でスカラー化する。挙動は移設元（旧 `native_direct::classify_strict` の
/// `Command::Write` 腕）と同一 — エラーメッセージ文言も維持。呼び出し口は
/// `mat_native::op::NodeOpKind::write`。
pub fn classify_write(cluster: &str, attribute: &str, value: &str) -> WriteClass {
    let Some(cluster_id) = resolve_cluster(cluster) else {
        return WriteClass::NotNative;
    };
    let Some(attr) = resolve_attribute(cluster_id, attribute) else {
        return WriteClass::NotNative;
    };
    let timed = attr.def.map(|d| d.timed_write).unwrap_or(false);
    let parsed = match attr.def {
        Some(def) => parse_value_typed(value, &def.ty),
        None => match reject_container_literal(&format!("write {cluster}/{attribute}"), value) {
            Some(msg) => return WriteClass::Reject(msg),
            None => Ok(parse_scalar_inferred(value)),
        },
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

/// 汎用 invoke の分類結果（旧 mat 直経路 `native_direct::classify_strict` の
/// `Command::Invoke` / `GroupCommand::Invoke` 判定を移設・一本化 — M8a Task10。
/// 現在の呼び出し口は `mat_native::op::NodeOpKind::invoke` /
/// `GroupOpKind::invoke`。
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
/// 順にスカラー化する。数値 ID 直指定（def なし）は引数を値リテラルから
/// 型推定してスカラー化する（write の数値直指定と同じ）。エラーメッセージ文言は移設元と同一
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
                match parse_value_typed(arg, &def.fields[i].ty) {
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
        // 数値直指定（def なし）: 引数は write の数値直指定と同じく値リテラル
        // から型推定してスカラー化する（推定は失敗しない）。timed は定義が
        // 無いので false 固定 — 上書きしたい場合の CLI フラグは未提供。
        None => {
            for (i, a) in args.iter().enumerate() {
                let what = format!("invoke {cluster}/{command} arg {i}");
                if let Some(msg) = reject_container_literal(&what, a) {
                    return InvokeClass::Reject(msg);
                }
            }
            InvokeClass::Native {
                cluster: cluster_id,
                command: cmd.id,
                fields: args.iter().map(|a| parse_scalar_inferred(a)).collect(),
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
        assert!(matches!(a.def.unwrap().ty, Ty::Scalar(TypeTag::Bool)));
        let a = resolve_attribute(0x0300, "color-temperature-mireds").unwrap();
        assert_eq!(a.id, 0x0007);
        assert!(matches!(a.def.unwrap().ty, Ty::Scalar(TypeTag::UInt)));
        let a = resolve_attribute(0x0035, "neighbor-table").unwrap();
        assert_eq!(a.id, 0x0007);
        assert!(matches!(a.def.unwrap().ty, Ty::ListOfStruct(_)));
        let a = resolve_attribute(0x001F, "acl").unwrap();
        assert_eq!(a.id, 0x0000);
        assert!(matches!(a.def.unwrap().ty, Ty::ListOfStruct(_)));
        let a = resolve_attribute(0x003F, "group-key-map").unwrap();
        assert_eq!(a.id, 0x0000);
        assert!(matches!(a.def.unwrap().ty, Ty::ListOfStruct(_)));
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
        assert!(matches!(
            c.def.unwrap().fields[0].ty,
            Ty::Scalar(TypeTag::UInt)
        ));
        let c = resolve_command(0x003F, "key-set-write").unwrap();
        assert_eq!(c.id, 0x00);
        // KeySetWrite の field 0 は GroupKeySetStruct。
        assert!(matches!(c.def.unwrap().fields[0].ty, Ty::Struct(_)));
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
            assert!(matches!(a.def.unwrap().ty, Ty::Scalar(TypeTag::UInt)));
            let a = resolve_attribute(cluster, "cluster-revision").unwrap();
            assert_eq!(a.id, 0xFFFD);
            let a = resolve_attribute(cluster, "attribute-list").unwrap();
            assert_eq!(a.id, 0xFFFB);
            assert!(matches!(a.def.unwrap().ty, Ty::List(_)));
        }
    }

    #[test]
    fn numeric_ids_beyond_u32_are_rejected() {
        assert!(resolve_attribute(0x0006, "0x100000001").is_none());
        assert_eq!(resolve_cluster("0x100000001"), None);
    }

    #[test]
    fn parse_value_typed_scalars() {
        use ScalarValue as V;
        assert_eq!(
            parse_value_typed("true", &Ty::Scalar(TypeTag::Bool)),
            Ok(V::Bool(true))
        );
        assert_eq!(
            parse_value_typed("0", &Ty::Scalar(TypeTag::Bool)),
            Ok(V::Bool(false))
        );
        assert_eq!(
            parse_value_typed("1", &Ty::Scalar(TypeTag::Bool)),
            Ok(V::Bool(true))
        );
        assert_eq!(
            parse_value_typed("128", &Ty::Scalar(TypeTag::UInt)),
            Ok(V::UInt(128))
        );
        assert_eq!(
            parse_value_typed("0x80", &Ty::Scalar(TypeTag::UInt)),
            Ok(V::UInt(128))
        );
        assert_eq!(
            parse_value_typed("-5", &Ty::Scalar(TypeTag::Int)),
            Ok(V::Int(-5))
        );
        assert_eq!(
            parse_value_typed("hello", &Ty::Scalar(TypeTag::Str)),
            Ok(V::Str("hello".into()))
        );
        assert_eq!(
            parse_value_typed("hex:d0d1", &Ty::Scalar(TypeTag::Bytes)),
            Ok(V::Bytes(vec![0xd0, 0xd1]))
        );
        assert_eq!(
            parse_value_typed("null", &Ty::Scalar(TypeTag::UInt)),
            Ok(V::Null)
        );
    }

    #[test]
    fn parse_value_typed_rejects_unsupported_and_bad_literals() {
        // 不正な JSON 形（list が来るはずの場所に struct）は拒否。
        assert!(parse_value_typed("{}", &Ty::List(TypeTag::UInt)).is_err());
        assert!(parse_value_typed("abc", &Ty::Scalar(TypeTag::UInt)).is_err());
        assert!(parse_value_typed("xyz", &Ty::Scalar(TypeTag::Bool)).is_err());
        assert!(parse_value_typed("hex:zz", &Ty::Scalar(TypeTag::Bytes)).is_err());
        assert!(parse_value_typed("1", &Ty::Scalar(TypeTag::Unknown)).is_err());
        // エラーメッセージは型名を含む（spec 受け入れ5: AI が判断できる detail）。
        let e = parse_value_typed("{}", &Ty::List(TypeTag::UInt)).unwrap_err();
        assert!(e.contains("array"), "{e}");
    }

    #[test]
    fn float_attributes_resolve_to_f32_or_f64() {
        // unittesting (0xFFF1FC05): FloatSingle = single → F32, FloatDouble = double → F64。
        let a = resolve_attribute(0xFFF1FC05, "float-single").unwrap();
        assert!(matches!(a.def.unwrap().ty, Ty::Scalar(TypeTag::F32)));
        let a = resolve_attribute(0xFFF1FC05, "float-double").unwrap();
        assert!(matches!(a.def.unwrap().ty, Ty::Scalar(TypeTag::F64)));
    }

    #[test]
    fn parse_value_typed_floats() {
        use ScalarValue as V;
        assert_eq!(
            parse_value_typed("1.5", &Ty::Scalar(TypeTag::F64)),
            Ok(V::F64(1.5))
        );
        assert_eq!(
            parse_value_typed("-3", &Ty::Scalar(TypeTag::F64)),
            Ok(V::F64(-3.0))
        );
        assert_eq!(
            parse_value_typed("2e-3", &Ty::Scalar(TypeTag::F32)),
            Ok(V::F32(2e-3))
        );
        assert_eq!(
            parse_value_typed("null", &Ty::Scalar(TypeTag::F32)),
            Ok(V::Null)
        );
        // nan / inf / 非数値は拒否（TLV には載るがデバイスは CONSTRAINT_ERROR)。
        assert!(parse_value_typed("nan", &Ty::Scalar(TypeTag::F64)).is_err());
        assert!(parse_value_typed("inf", &Ty::Scalar(TypeTag::F32)).is_err());
        assert!(parse_value_typed("abc", &Ty::Scalar(TypeTag::F64)).is_err());
        // f64 としては有限でも f32 の範囲を超える値は inf に飽和するので拒否
        // （f64 はそのまま通る）。
        assert!(parse_value_typed("1e300", &Ty::Scalar(TypeTag::F32)).is_err());
        assert!(parse_value_typed("1e300", &Ty::Scalar(TypeTag::F64)).is_ok());
        // JSON 経路（list of f32）でも同じ範囲検査がかかる。
        assert!(parse_value_typed("[1e300]", &Ty::List(TypeTag::F32)).is_err());
        assert!(parse_value_typed("[1.5]", &Ty::List(TypeTag::F32)).is_ok());
    }

    #[test]
    fn parse_scalar_inferred_float_literal() {
        // 数値 ID 直指定: 小数点 / 指数を含む数値リテラルは F64 に推定する。
        assert_eq!(parse_scalar_inferred("1.5"), ScalarValue::F64(1.5));
        assert_eq!(parse_scalar_inferred("2e3"), ScalarValue::F64(2000.0));
        // 整数はこれまでどおり UInt / Int。
        assert_eq!(parse_scalar_inferred("42"), ScalarValue::UInt(42));
        assert_eq!(parse_scalar_inferred("-1"), ScalarValue::Int(-1));
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
        // 数値直指定（def なし）: 引数なし。
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
    }

    #[test]
    fn classify_invoke_numeric_id_with_args_infers_types() {
        // 数値直指定（def なし）+ 引数あり: write と同じ推定スカラー化で native。
        let c = classify_invoke("8", "0", &["128".into(), "true".into(), "hex:00ff".into()]);
        assert_eq!(
            c,
            InvokeClass::Native {
                cluster: 8,
                command: 0,
                fields: vec![
                    ScalarValue::UInt(128),
                    ScalarValue::Bool(true),
                    ScalarValue::Bytes(vec![0, 0xff]),
                ],
                timed: false,
            }
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

    #[test]
    fn struct_schema_is_generated_for_acl_and_group_key_map() {
        // acl = list of AccessControlEntryStruct（cluster 0x001F スコープ）。
        let a = resolve_attribute(0x001F, "acl").unwrap();
        let Ty::ListOfStruct(entry) = a.def.unwrap().ty else {
            panic!("acl should be ListOfStruct, got {:?}", a.def.unwrap().ty);
        };
        assert_eq!(entry.name, "AccessControlEntryStruct");
        let names: Vec<&str> = entry.fields.iter().map(|f| f.name).collect();
        assert_eq!(
            names,
            [
                "privilege",
                "auth-mode",
                "subjects",
                "targets",
                "fabric-index"
            ]
        );
        let subjects = &entry.fields[2];
        assert_eq!(subjects.id, 3);
        assert!(matches!(subjects.ty, Ty::List(TypeTag::UInt)));
        let targets = &entry.fields[3];
        assert_eq!(targets.id, 4);
        let Ty::ListOfStruct(t) = targets.ty else {
            panic!("targets")
        };
        assert_eq!(t.name, "AccessControlTargetStruct");
        assert_eq!(t.fields.len(), 3); // Cluster / Endpoint / DeviceType（fabric-scoped ではない）
                                       // fabric-index は生成側で付けた暗黙 optional フィールド（read 出力の "254" を書き戻せる）。
        let fi = &entry.fields[4];
        assert_eq!((fi.id, fi.optional), (254, true));
        assert!(matches!(fi.ty, Ty::Scalar(TypeTag::UInt)));

        let a = resolve_attribute(0x003F, "group-key-map").unwrap();
        let Ty::ListOfStruct(m) = a.def.unwrap().ty else {
            panic!("group-key-map")
        };
        assert_eq!(m.name, "GroupKeyMapStruct");
        assert_eq!(m.fields[0].name, "group-id");
        assert_eq!(m.fields[1].name, "group-key-set-id");
        assert_eq!(m.fields[1].id, 2);
    }

    #[test]
    fn command_struct_args_and_scalar_lists_are_typed() {
        let c = resolve_command(0x003F, "key-set-write").unwrap();
        let Ty::Struct(ks) = c.def.unwrap().fields[0].ty else {
            panic!("key-set-write arg0")
        };
        assert_eq!(ks.name, "GroupKeySetStruct");
        assert_eq!(ks.fields.len(), 8);
        assert!(matches!(ks.fields[2].ty, Ty::Scalar(TypeTag::Bytes))); // EpochKey0
                                                                        // スカラー list 属性: descriptor server-list = list<cluster_id>。
        let a = resolve_attribute(0x001D, "server-list").unwrap();
        assert!(matches!(a.def.unwrap().ty, Ty::List(TypeTag::UInt)));
        // 同名 struct のクラスタスコープ解決: modeselect の supported-modes は
        // modeselect 自身の ModeOptionStruct（Label/Mode/SemanticTags）。
        let a = resolve_attribute(0x0050, "supported-modes").unwrap();
        let Ty::ListOfStruct(mo) = a.def.unwrap().ty else {
            panic!("supported-modes")
        };
        assert_eq!(mo.fields[0].name, "label");
        assert_eq!(mo.fields[1].name, "mode");
    }

    #[test]
    fn global_attributes_keep_scalar_list_shape() {
        let a = resolve_attribute(0x0006, "attribute-list").unwrap();
        assert!(matches!(a.def.unwrap().ty, Ty::List(TypeTag::UInt)));
        let a = resolve_attribute(0x0006, "feature-map").unwrap();
        assert!(matches!(a.def.unwrap().ty, Ty::Scalar(TypeTag::UInt)));
    }

    fn acl_ty() -> Ty {
        resolve_attribute(0x001F, "acl").unwrap().def.unwrap().ty
    }

    #[test]
    fn parse_value_typed_struct_list_by_name_and_by_id() {
        use ScalarValue as V;
        // 名前キー。
        let v = parse_value_typed(
            r#"[{"privilege":5,"auth-mode":2,"subjects":[112233,"0x1122"],"targets":null}]"#,
            &acl_ty(),
        )
        .unwrap();
        let expect = V::List(vec![V::Struct(vec![
            (1, V::UInt(5)),
            (2, V::UInt(2)),
            (3, V::List(vec![V::UInt(112233), V::UInt(0x1122)])),
            (4, V::Null),
        ])]);
        assert_eq!(v, expect);
        // 番号キー（read 出力の形、fabric-index 254 込み）でも同じ — 順序は id 昇順に整列。
        let v = parse_value_typed(
            r#"[{"4":null,"254":1,"3":[112233,4386],"2":2,"1":5}]"#,
            &acl_ty(),
        )
        .unwrap();
        let V::List(items) = &v else { panic!() };
        let V::Struct(fields) = &items[0] else {
            panic!()
        };
        let ids: Vec<u8> = fields.iter().map(|(id, _)| *id).collect();
        assert_eq!(ids, [1, 2, 3, 4, 254]);
        // targets の struct 要素（nullable スカラー）。
        let v = parse_value_typed(
            r#"[{"privilege":3,"auth-mode":2,"subjects":[7],"targets":[{"cluster":6,"endpoint":null,"device-type":null}]}]"#,
            &acl_ty(),
        )
        .unwrap();
        let V::List(items) = &v else { panic!() };
        let V::Struct(fields) = &items[0] else {
            panic!()
        };
        assert_eq!(
            fields[3].1,
            V::List(vec![V::Struct(vec![
                (0, V::UInt(6)),
                (1, V::Null),
                (2, V::Null)
            ])])
        );
    }

    #[test]
    fn parse_value_typed_bytes_accept_both_hex_forms_inside_json() {
        use ScalarValue as V;
        let ks = resolve_command(0x003F, "key-set-write")
            .unwrap()
            .def
            .unwrap()
            .fields[0]
            .ty;
        let j = r#"{"group-key-set-id":1,"group-key-security-policy":0,"epoch-key0":"hex:00112233445566778899aabbccddeeff","epoch-start-time0":1,"epoch-key1":null,"epoch-start-time1":null,"epoch-key2":"00112233445566778899aabbccddeeff","epoch-start-time2":null}"#;
        let V::Struct(f) = parse_value_typed(j, &ks).unwrap() else {
            panic!()
        };
        assert_eq!(
            f[2].1,
            V::Bytes(vec![
                0x00, 0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd,
                0xee, 0xff
            ])
        );
        assert_eq!(f[6].1, f[2].1);
    }

    #[test]
    fn parse_value_typed_scalar_list_and_top_level_null() {
        use ScalarValue as V;
        let t = Ty::List(TypeTag::UInt);
        assert_eq!(
            parse_value_typed("[1, 2, 3]", &t),
            Ok(V::List(vec![V::UInt(1), V::UInt(2), V::UInt(3)]))
        );
        assert_eq!(parse_value_typed("[]", &t), Ok(V::List(vec![])));
        assert_eq!(parse_value_typed("null", &t), Ok(V::Null));
    }

    #[test]
    fn parse_value_typed_rejects_bad_json_shapes() {
        let t = acl_ty();
        let err = |s: &str| parse_value_typed(s, &t).unwrap_err();
        assert!(err("{}").contains("array"), "struct given for list");
        assert!(err("not json").contains("JSON"));
        assert!(
            err(r#"[{"privilege":5,"auth-mode":2,"subjects":[],"targets":null,"bogus":1}]"#)
                .contains("bogus")
        );
        assert!(
            err(r#"[{"privilege":5,"auth-mode":2,"subjects":[]}]"#).contains("targets"),
            "missing required"
        );
        assert!(
            err(r#"[{"privilege":5,"1":5,"auth-mode":2,"subjects":[],"targets":null}]"#)
                .contains("twice")
        );
        assert!(
            err(r#"[{"privilege":-1,"auth-mode":2,"subjects":[],"targets":null}]"#)
                .contains("unsigned")
        );
        assert!(
            err(r#"[{"privilege":"five","auth-mode":2,"subjects":[],"targets":null}]"#)
                .contains("unsigned")
        );
    }

    #[test]
    fn parse_value_typed_optional_fields_may_be_omitted_but_required_may_not() {
        // unittesting list-nullables-and-optionals-struct: NullableInt(0) は必須
        // (nullable だが optional ではない)、OptionalInt(1) / NullableOptionalInt(2)
        // 等は optional。fieldId 無しの struct なので id は出現順。
        let ty = resolve_attribute(0xFFF1FC05, "list-nullables-and-optionals-struct")
            .unwrap()
            .def
            .unwrap()
            .ty;
        let Ty::ListOfStruct(def) = ty else {
            panic!("{ty:?}")
        };
        assert_eq!(def.fields[0].name, "nullable-int");
        assert_eq!((def.fields[0].id, def.fields[0].optional), (0, false));
        assert_eq!((def.fields[1].id, def.fields[1].optional), (1, true));
        let ok = r#"[{"nullable-int":null,"nullable-string":null,"nullable-struct":null,"nullable-list":null}]"#;
        assert!(
            parse_value_typed(ok, &ty).is_ok(),
            "{:?}",
            parse_value_typed(ok, &ty)
        );
        let err = parse_value_typed("[{}]", &ty).unwrap_err();
        assert!(err.contains("nullable-int"), "{err}");
    }

    #[test]
    fn classify_write_accepts_list_and_rejects_numeric_id_containers() {
        match classify_write("accesscontrol", "acl", "[]") {
            WriteClass::Native {
                cluster: 0x001F,
                attribute: 0,
                value: ScalarValue::List(v),
                timed: false,
            } => assert!(v.is_empty()),
            other => panic!("{other:?}"),
        }
        match classify_write(
            "groupkeymanagement",
            "group-key-map",
            r#"[{"group-id":1,"group-key-set-id":2}]"#,
        ) {
            WriteClass::Native {
                value: ScalarValue::List(v),
                ..
            } => assert_eq!(v.len(), 1),
            other => panic!("{other:?}"),
        }
        // 数値 ID 直指定には型情報が無いので list/struct は Reject（従来は Str で送っていた）。
        match classify_write("0x1F", "0", "[]") {
            WriteClass::Reject(msg) => assert!(msg.contains("numeric"), "{msg}"),
            other => panic!("{other:?}"),
        }
        match classify_invoke("0x3F", "0", &["{}".into()]) {
            InvokeClass::Reject(msg) => assert!(msg.contains("numeric"), "{msg}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn classify_invoke_accepts_struct_arg() {
        let j = r#"{"group-key-set-id":1,"group-key-security-policy":0,"epoch-key0":"hex:00112233445566778899aabbccddeeff","epoch-start-time0":1,"epoch-key1":null,"epoch-start-time1":null,"epoch-key2":null,"epoch-start-time2":null}"#;
        match classify_invoke("groupkeymanagement", "key-set-write", &[j.into()]) {
            InvokeClass::Native { fields, .. } => {
                assert!(matches!(fields[0], ScalarValue::Struct(_)))
            }
            other => panic!("{other:?}"),
        }
        // 必須欠落は依然 Reject（op.rs の既存テストが "{}" の Reject を期待している）。
        assert!(matches!(
            classify_invoke("groupkeymanagement", "key-set-write", &["{}".into()]),
            InvokeClass::Reject(_)
        ));
    }
}
