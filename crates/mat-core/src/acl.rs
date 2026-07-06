//! ACL（AccessControl クラスタ）値の解釈・変換。group provision / `mat group grant`
//! の read-merge-write ステップが使う。状態は持たない（設計ルール 4）。
//!
//! groupcast は authMode=Group で届くため、デバイス ACL に
//! `{privilege: Operate, authMode: Group, subjects: [<GroupId>]}` のエントリが無いと
//! デバイスが黙って捨てる（commissioning が作るのは CASE 管理者エントリだけ）。
//!
//! ACL の attribute write は**全置換**。write する値は必ず「read できたリスト + 追記」
//! だけから組み立てる。read が解釈できないときは `ErrorKind::ParseError` を返し、
//! 呼び出し側はそこで停止する（blind write は管理者エントリを失いデバイスが管理
//! 不能になるため、失敗側に倒す）。

use serde::Serialize;
use serde_json::Value;

use crate::error::MatError;
use crate::parse::strip_log_prefix;

/// Matter AccessControl の privilege。3 = Operate（Administer は authMode=Group と
/// 組み合わせ不可のため、group エントリは Operate 固定）。
pub const PRIVILEGE_OPERATE: u8 = 3;
/// Matter AccessControl の authMode。3 = Group。
pub const AUTH_MODE_GROUP: u8 = 3;

/// ACL エントリの target（クラスタ / エンドポイント / デバイス種別の限定）。
/// `mat` 自身は targets: null（全許可）しか生成しないが、既存エントリの保全のため
/// read 側は非 null も解釈できる必要がある。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AclTarget {
    pub cluster: Option<u32>,
    pub endpoint: Option<u16>,
    pub device_type: Option<u32>,
}

/// ACL エントリ。chip-tool の read 出力（TOO ログ / ws 値）から解釈し、write 用
/// JSON へ変換できる最小限の表現。Serialize は write JSON（名前付き camelCase キー、
/// chip-tool の accesscontrol write が受ける形）を直接生成する。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AclEntry {
    pub privilege: u8,
    pub auth_mode: u8,
    pub subjects: Vec<u64>,
    pub targets: Option<Vec<AclTarget>>,
    /// read で得た値をそのまま write に渡す（サーバ側で無視・置換されるため
    /// ハードコード不要）。
    pub fabric_index: u8,
}

/// groupcast 許可用の ACL エントリを組み立てる。
pub fn group_acl_entry(group_id: u16, fabric_index: u8) -> AclEntry {
    AclEntry {
        privilege: PRIVILEGE_OPERATE,
        auth_mode: AUTH_MODE_GROUP,
        subjects: vec![u64::from(group_id)],
        targets: None,
        fabric_index,
    }
}

/// 既存 ACL に group エントリを追記した全リストを返す。既にあれば `None`（冪等、
/// write 不要）。fabricIndex は既存エントリの先頭から引き継ぐ（read 値をそのまま
/// 渡す方針。エントリ 0 件は起きない想定だが、その場合は 0 — サーバ側で置換される）。
pub fn merge_group_entry(entries: &[AclEntry], group_id: u16) -> Option<Vec<AclEntry>> {
    let gid = u64::from(group_id);
    if entries
        .iter()
        .any(|e| e.auth_mode == AUTH_MODE_GROUP && e.subjects.contains(&gid))
    {
        return None;
    }
    let fabric_index = entries.first().map(|e| e.fabric_index).unwrap_or(0);
    let mut merged = entries.to_vec();
    merged.push(group_acl_entry(group_id, fabric_index));
    Some(merged)
}

/// `accesscontrol write acl` の引数用 compact JSON。matd の ws コマンド行は空白が
/// 引数区切りのため、空白なしであることが必須（serde_json の to_string は compact）。
pub fn to_chip_write_json(entries: &[AclEntry]) -> String {
    serde_json::to_string(entries).expect("AclEntry serialization cannot fail")
}

/// chip-tool 直経路の `accesscontrol read acl <node> 0` stdout（TOO ログ形式）を
/// 解釈する。
///
/// 想定形（NeighborTable 等と同じ DataModelLogger の list-of-struct 整形。ただし
/// Subjects / Targets の**ネストしたリスト**を含むため `parse_struct_list` では
/// 表現できず専用パーサとする）:
/// ```text
///   ACL: 2 entries
///     [1]: {
///       Privilege: 5
///       AuthMode: 2
///       Subjects: 1 entries
///         [1]: 112233
///       Targets: null
///       FabricIndex: 4
///      }
///     [2]: { ... }
/// ```
/// 安全弁（いずれも `ParseError`）: `ACL: n entries` ヘッダが無い / パースできた
/// エントリ数がヘッダと合わない（途中で切れた出力）/ 必須フィールド欠け /
/// ネストした `Subjects: n entries` / `Targets: n entries` の宣言件数と実際の
/// 件数が合わない（サブリストが途中で切れた出力）。ACL write は全置換なので、
/// 部分的にしか読めていないリストで write すると読み落としたエントリ（または
/// target 限定が外れて広がった権限）を書き込んでしまう — 迷ったら失敗させる。
///
/// **境界**: エントリ内（`{ ... }` の中、target の中も含む）の未知のキーや
/// colon の無い行は garbled なデータとみなし `ParseError` にする。一方で
/// エントリ外（まだどのエントリにも入っていない状態）のログ雑音は無視する
/// （実 chip-tool の出力は TOO ブロックの前後に無関係な DMG/log 行が混ざる
/// ため、この範囲のリニエンシーは意図的）。
pub fn parse_acl_from_chip_log(stdout: &str) -> Result<Vec<AclEntry>, MatError> {
    let mut declared: Option<usize> = None;
    let mut entries: Vec<AclEntry> = Vec::new();
    let mut cur: Option<EntryBuilder> = None;
    let mut cur_target: Option<TargetBuilder> = None;
    let mut section = Section::Fields;

    for line in stdout.lines() {
        let Some(payload) = strip_log_prefix(line) else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() {
            continue;
        }

        // ヘッダ `ACL: n entries`（エントリ外でのみ現れる）。
        if cur.is_none() {
            if let Some(rest) = payload.strip_prefix("ACL:") {
                let n = rest
                    .split_whitespace()
                    .next()
                    .and_then(|t| t.parse::<usize>().ok())
                    .ok_or_else(|| {
                        MatError::parse_error(format!("unparseable ACL header: {payload}"))
                    })?;
                declared = Some(n);
                continue;
            }
        }

        // インデックス行 `[i]: ...`（エントリ開始 / subject / target 開始）。
        if let Some(rest) = index_line(payload) {
            match (&mut cur, &section) {
                (None, _) if rest.starts_with('{') => {
                    cur = Some(EntryBuilder::default());
                    section = Section::Fields;
                }
                (Some(b), Section::Subjects) => {
                    b.subjects.push(field_num::<u64>(rest, "subject")?);
                }
                (Some(_), Section::Targets) if rest.starts_with('{') => {
                    cur_target = Some(TargetBuilder::default());
                }
                _ => {
                    return Err(MatError::parse_error(format!(
                        "unexpected line in ACL output: {payload}"
                    )))
                }
            }
            continue;
        }

        // 閉じ括弧: target → entry の順で内側から閉じる。
        if payload.starts_with('}') {
            if let Some(t) = cur_target.take() {
                let Some(b) = cur.as_mut() else {
                    return Err(MatError::parse_error("ACL target outside an entry"));
                };
                b.targets.get_or_insert_with(Vec::new).push(AclTarget {
                    cluster: t.cluster,
                    endpoint: t.endpoint,
                    device_type: t.device_type,
                });
            } else if let Some(b) = cur.take() {
                entries.push(b.build()?);
                section = Section::Fields;
            }
            continue;
        }

        // フィールド行 `Key: Value`。エントリ外（cur が None）の無関係行は無視するが、
        // エントリ内の colon 無し行は garbled データとして fail-closed にする。
        let Some(colon) = payload.find(':') else {
            if cur.is_none() {
                continue;
            }
            return Err(MatError::parse_error(format!(
                "unexpected line in ACL entry: {payload}"
            )));
        };
        let key = payload[..colon].trim();
        let val = payload[colon + 1..].trim().trim_end_matches(',').trim();

        if let Some(t) = cur_target.as_mut() {
            match key {
                "Cluster" => t.cluster = field_opt_num(val, "target Cluster")?,
                "Endpoint" => t.endpoint = field_opt_num(val, "target Endpoint")?,
                "DeviceType" => t.device_type = field_opt_num(val, "target DeviceType")?,
                _ => {
                    return Err(MatError::parse_error(format!(
                        "unexpected line in ACL entry: {payload}"
                    )))
                }
            }
            continue;
        }
        let Some(b) = cur.as_mut() else { continue };
        match key {
            "Privilege" => b.privilege = Some(field_num(val, "Privilege")?),
            "AuthMode" => b.auth_mode = Some(field_num(val, "AuthMode")?),
            "FabricIndex" => {
                b.fabric_index = Some(field_num(val, "FabricIndex")?);
                section = Section::Fields;
            }
            "Subjects" => {
                if val.starts_with("null") {
                    section = Section::Fields;
                } else {
                    b.expected_subjects = Some(field_num::<usize>(val, "Subjects")?);
                    section = Section::Subjects;
                }
            }
            "Targets" => {
                if val.starts_with("null") {
                    b.targets = None;
                    section = Section::Fields;
                } else {
                    b.expected_targets = Some(field_num::<usize>(val, "Targets")?);
                    b.targets = Some(Vec::new());
                    section = Section::Targets;
                }
            }
            _ => {
                return Err(MatError::parse_error(format!(
                    "unexpected line in ACL entry: {payload}"
                )))
            }
        }
    }

    let declared = declared
        .ok_or_else(|| MatError::parse_error("no `ACL: n entries` header in chip-tool output"))?;
    if entries.len() != declared || cur.is_some() || cur_target.is_some() {
        return Err(MatError::parse_error(format!(
            "ACL parse mismatch: header declared {declared} entries, parsed {} (refusing to write a possibly truncated list)",
            entries.len()
        )));
    }
    Ok(entries)
}

/// matd（ws）経路の `accesscontrol read acl` 応答 `results[0].value` を解釈する。
///
/// ws 値は数値フィールド ID キーのオブジェクト配列（実機で確定済みの形）:
/// `[{"1":5,"2":2,"3":[112233],"4":null,"254":4}]`
/// （`"1"`=privilege, `"2"`=authMode, `"3"`=subjects, `"4"`=targets,
/// `"254"`=fabricIndex。targets 内は `"0"`=cluster, `"1"`=endpoint,
/// `"2"`=deviceType）。解釈不能は `ParseError`（write を止める）。
pub fn acl_entries_from_ws_value(value: &Value) -> Result<Vec<AclEntry>, MatError> {
    let arr = value
        .as_array()
        .ok_or_else(|| MatError::parse_error(format!("ACL ws value is not an array: {value}")))?;
    arr.iter().map(ws_entry).collect()
}

fn ws_entry(v: &Value) -> Result<AclEntry, MatError> {
    let obj = v
        .as_object()
        .ok_or_else(|| MatError::parse_error(format!("ACL ws entry is not an object: {v}")))?;
    let subjects = match obj.get("3") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a
            .iter()
            .map(|s| {
                s.as_u64().ok_or_else(|| {
                    MatError::parse_error(format!("ACL ws subject is not an integer: {s}"))
                })
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(other) => {
            return Err(MatError::parse_error(format!(
                "ACL ws subjects (field 3) is not an array: {other}"
            )))
        }
    };
    let targets = match obj.get("4") {
        None | Some(Value::Null) => None,
        Some(Value::Array(a)) => Some(a.iter().map(ws_target).collect::<Result<Vec<_>, _>>()?),
        Some(other) => {
            return Err(MatError::parse_error(format!(
                "ACL ws targets (field 4) is not an array: {other}"
            )))
        }
    };
    Ok(AclEntry {
        privilege: ws_u8(obj, "1", "privilege")?,
        auth_mode: ws_u8(obj, "2", "authMode")?,
        subjects,
        targets,
        fabric_index: ws_u8(obj, "254", "fabricIndex")?,
    })
}

fn ws_u8(obj: &serde_json::Map<String, Value>, key: &str, what: &str) -> Result<u8, MatError> {
    obj.get(key)
        .and_then(Value::as_u64)
        .and_then(|v| u8::try_from(v).ok())
        .ok_or_else(|| {
            MatError::parse_error(format!("ACL ws entry missing/invalid {what} (field {key})"))
        })
}

fn ws_target(v: &Value) -> Result<AclTarget, MatError> {
    let obj = v
        .as_object()
        .ok_or_else(|| MatError::parse_error(format!("ACL ws target is not an object: {v}")))?;
    Ok(AclTarget {
        cluster: ws_opt_num(obj, "0")?,
        endpoint: ws_opt_num(obj, "1")?,
        device_type: ws_opt_num(obj, "2")?,
    })
}

fn ws_opt_num<T: TryFrom<u64>>(
    obj: &serde_json::Map<String, Value>,
    key: &str,
) -> Result<Option<T>, MatError> {
    match obj.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v
            .as_u64()
            .and_then(|n| T::try_from(n).ok())
            .map(Some)
            .ok_or_else(|| {
                MatError::parse_error(format!("ACL ws target field {key} is invalid: {v}"))
            }),
    }
}

/// パーサ内部: 構築途中のエントリ。
#[derive(Default)]
struct EntryBuilder {
    privilege: Option<u8>,
    auth_mode: Option<u8>,
    subjects: Vec<u64>,
    targets: Option<Vec<AclTarget>>,
    fabric_index: Option<u8>,
    /// `Subjects: n entries` で宣言された件数。実際に読めた `subjects.len()` と
    /// 一致しなければ途中で切れた出力とみなし `ParseError`（build 時に検証）。
    expected_subjects: Option<usize>,
    /// `Targets: n entries` で宣言された件数。`expected_subjects` と同様に検証する。
    expected_targets: Option<usize>,
}

impl EntryBuilder {
    fn build(self) -> Result<AclEntry, MatError> {
        if let Some(expected) = self.expected_subjects {
            let actual = self.subjects.len();
            if actual != expected {
                return Err(MatError::parse_error(format!(
                    "ACL entry Subjects count mismatch: declared {expected} entries, parsed {actual}"
                )));
            }
        }
        if let Some(expected) = self.expected_targets {
            let actual = self.targets.as_ref().map_or(0, Vec::len);
            if actual != expected {
                return Err(MatError::parse_error(format!(
                    "ACL entry Targets count mismatch: declared {expected} entries, parsed {actual}"
                )));
            }
        }
        Ok(AclEntry {
            privilege: self
                .privilege
                .ok_or_else(|| MatError::parse_error("ACL entry missing Privilege"))?,
            auth_mode: self
                .auth_mode
                .ok_or_else(|| MatError::parse_error("ACL entry missing AuthMode"))?,
            subjects: self.subjects,
            targets: self.targets,
            fabric_index: self
                .fabric_index
                .ok_or_else(|| MatError::parse_error("ACL entry missing FabricIndex"))?,
        })
    }
}

/// パーサ内部: 構築途中の target。
#[derive(Default)]
struct TargetBuilder {
    cluster: Option<u32>,
    endpoint: Option<u16>,
    device_type: Option<u32>,
}

/// 現エントリ内でインデックス行（`[i]: ...`）が属するリスト。
enum Section {
    Fields,
    Subjects,
    Targets,
}

/// `[i]: <rest>` 形（i は数値）の行なら `<rest>` を返す。
fn index_line(payload: &str) -> Option<&str> {
    let inner = payload.strip_prefix('[')?;
    let close = inner.find(']')?;
    inner[..close].trim().parse::<u64>().ok()?;
    inner[close + 1..]
        .trim_start()
        .strip_prefix(':')
        .map(str::trim)
}

/// フィールド値の数値解釈。実機の名前注釈付き（`5 (Administer)`）も先頭トークンで読む。
fn field_num<T: TryFrom<u64>>(val: &str, what: &str) -> Result<T, MatError> {
    let head = val.split_whitespace().next().unwrap_or(val);
    head.parse::<u64>()
        .ok()
        .and_then(|v| T::try_from(v).ok())
        .ok_or_else(|| MatError::parse_error(format!("unparseable ACL {what}: {val}")))
}

/// `null` 許容の数値フィールド。
fn field_opt_num<T: TryFrom<u64>>(val: &str, what: &str) -> Result<Option<T>, MatError> {
    if val.starts_with("null") {
        return Ok(None);
    }
    field_num(val, what).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// commissioning が作る CASE 管理者エントリ相当（subject 112233 は chip-tool の
    /// 既定 controller node id。ダミー値）。
    fn admin() -> AclEntry {
        AclEntry {
            privilege: 5,
            auth_mode: 2,
            subjects: vec![112233],
            targets: None,
            fabric_index: 4,
        }
    }

    #[test]
    fn group_acl_entry_is_operate_group() {
        let e = group_acl_entry(10, 4);
        assert_eq!(e.privilege, PRIVILEGE_OPERATE);
        assert_eq!(e.auth_mode, AUTH_MODE_GROUP);
        assert_eq!(e.subjects, vec![10]);
        assert_eq!(e.targets, None);
        assert_eq!(e.fabric_index, 4);
    }

    #[test]
    fn merge_appends_group_entry_preserving_existing() {
        let merged = merge_group_entry(&[admin()], 10).expect("should append");
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0], admin());
        assert_eq!(merged[1], group_acl_entry(10, 4));
    }

    #[test]
    fn merge_is_none_when_group_entry_exists() {
        let entries = [admin(), group_acl_entry(10, 4)];
        assert!(merge_group_entry(&entries, 10).is_none());
    }

    #[test]
    fn merge_preserves_other_groups_entries() {
        // 同一デバイスへの複数グループ provision で先行グループを壊さない
        // （固定 2 エントリの blind write を不採用にした理由の回帰ガード）。
        let entries = [admin(), group_acl_entry(9, 4)];
        let merged = merge_group_entry(&entries, 10).expect("group 10 is new");
        assert_eq!(merged.len(), 3);
        assert!(merged.contains(&group_acl_entry(9, 4)));
        assert!(merged.contains(&group_acl_entry(10, 4)));
    }

    #[test]
    fn merge_ignores_case_entry_with_same_numeric_subject() {
        // subjects に同じ数値がいても authMode が Group でなければ「既存」とみなさない。
        let mut case_entry = admin();
        case_entry.subjects = vec![10];
        let merged = merge_group_entry(&[case_entry], 10).expect("must still append");
        assert_eq!(merged.len(), 2);
    }

    #[test]
    fn write_json_is_compact_named_keys() {
        let s = to_chip_write_json(&[admin(), group_acl_entry(10, 4)]);
        // ws コマンド行で 1 引数として渡すため空白なしが必須。
        assert!(!s.contains(' '), "must be compact: {s}");
        assert!(s.contains("\"privilege\":5"));
        assert!(s.contains("\"authMode\":2"));
        assert!(s.contains("\"authMode\":3"));
        assert!(s.contains("\"subjects\":[112233]"));
        assert!(s.contains("\"subjects\":[10]"));
        assert!(s.contains("\"targets\":null"));
        assert!(s.contains("\"fabricIndex\":4"));
    }

    #[test]
    fn write_json_round_trips_targets() {
        let entries = vec![AclEntry {
            privilege: 3,
            auth_mode: 3,
            subjects: vec![10],
            targets: Some(vec![AclTarget {
                cluster: Some(6),
                endpoint: None,
                device_type: None,
            }]),
            fabric_index: 1,
        }];
        let v: serde_json::Value = serde_json::from_str(&to_chip_write_json(&entries)).unwrap();
        assert_eq!(v[0]["targets"][0]["cluster"], serde_json::json!(6));
        assert_eq!(v[0]["targets"][0]["endpoint"], serde_json::Value::Null);
        assert_eq!(v[0]["targets"][0]["deviceType"], serde_json::Value::Null);
    }

    use crate::error::ErrorKind;
    use serde_json::json;

    #[test]
    fn ws_value_numeric_keys_parse() {
        // 実機で確定済みの ws 応答形: 数値フィールド ID がキー
        // （"1"=privilege, "2"=authMode, "3"=subjects, "4"=targets, "254"=fabricIndex）。
        let v = json!([{"1":5,"2":2,"3":[112233],"4":null,"254":4}]);
        assert_eq!(acl_entries_from_ws_value(&v).unwrap(), vec![admin()]);
    }

    #[test]
    fn ws_value_parses_admin_and_group() {
        let v = json!([
            {"1":5,"2":2,"3":[112233],"4":null,"254":4},
            {"1":3,"2":3,"3":[10],"4":null,"254":4}
        ]);
        assert_eq!(
            acl_entries_from_ws_value(&v).unwrap(),
            vec![admin(), group_acl_entry(10, 4)]
        );
    }

    #[test]
    fn ws_value_targets_non_null() {
        // targets 内は "0"=cluster, "1"=endpoint, "2"=deviceType。
        let v = json!([{"1":3,"2":2,"3":[112233],"4":[{"0":6,"1":1,"2":null}],"254":4}]);
        let entries = acl_entries_from_ws_value(&v).unwrap();
        assert_eq!(
            entries[0].targets,
            Some(vec![AclTarget {
                cluster: Some(6),
                endpoint: Some(1),
                device_type: None,
            }])
        );
    }

    #[test]
    fn ws_value_bad_shape_is_parse_error() {
        for v in [
            json!(true),                            // 配列ですらない
            json!([42]),                            // 要素がオブジェクトでない
            json!([{"2":2,"254":1}]),               // privilege（"1"）欠け
            json!([{"1":5,"2":2,"3":"x","254":1}]), // subjects が配列でない
        ] {
            let err = acl_entries_from_ws_value(&v).expect_err(&format!("must fail: {v}"));
            assert_eq!(err.kind, ErrorKind::ParseError, "input: {v}");
        }
    }

    /// 実機 chip-tool の `accesscontrol read acl` TOO ログ（admin 1 エントリ）。
    /// この形式は 2026-07-06 の実機デバッグ（jarvis）に基づく想定形。upstream の
    /// バージョン変化はこのテストで検知する（CLAUDE.md の fragile-parse ルール）。
    const ACL_ADMIN_ONLY: &str = "\
[1656][CHIP:TOO]   ACL: 1 entries
[1656][CHIP:TOO]     [1]: {
[1656][CHIP:TOO]       Privilege: 5
[1656][CHIP:TOO]       AuthMode: 2
[1656][CHIP:TOO]       Subjects: 1 entries
[1656][CHIP:TOO]         [1]: 112233
[1656][CHIP:TOO]       Targets: null
[1656][CHIP:TOO]       FabricIndex: 4
[1656][CHIP:TOO]      }
";

    const ACL_ADMIN_AND_GROUP: &str = "\
[1656][CHIP:TOO]   ACL: 2 entries
[1656][CHIP:TOO]     [1]: {
[1656][CHIP:TOO]       Privilege: 5
[1656][CHIP:TOO]       AuthMode: 2
[1656][CHIP:TOO]       Subjects: 1 entries
[1656][CHIP:TOO]         [1]: 112233
[1656][CHIP:TOO]       Targets: null
[1656][CHIP:TOO]       FabricIndex: 4
[1656][CHIP:TOO]      }
[1656][CHIP:TOO]     [2]: {
[1656][CHIP:TOO]       Privilege: 3
[1656][CHIP:TOO]       AuthMode: 3
[1656][CHIP:TOO]       Subjects: 1 entries
[1656][CHIP:TOO]         [1]: 10
[1656][CHIP:TOO]       Targets: null
[1656][CHIP:TOO]       FabricIndex: 4
[1656][CHIP:TOO]      }
";

    #[test]
    fn too_log_parses_admin_only() {
        let entries = parse_acl_from_chip_log(ACL_ADMIN_ONLY).unwrap();
        assert_eq!(entries, vec![admin()]);
    }

    #[test]
    fn too_log_parses_admin_and_group() {
        let entries = parse_acl_from_chip_log(ACL_ADMIN_AND_GROUP).unwrap();
        assert_eq!(entries, vec![admin(), group_acl_entry(10, 4)]);
    }

    #[test]
    fn too_log_parses_non_null_targets() {
        // 他 admin が書いた targets 限定エントリも保全のため解釈できること。
        let s = "\
[1656][CHIP:TOO]   ACL: 1 entries
[1656][CHIP:TOO]     [1]: {
[1656][CHIP:TOO]       Privilege: 3
[1656][CHIP:TOO]       AuthMode: 2
[1656][CHIP:TOO]       Subjects: 1 entries
[1656][CHIP:TOO]         [1]: 112233
[1656][CHIP:TOO]       Targets: 1 entries
[1656][CHIP:TOO]         [1]: {
[1656][CHIP:TOO]           Cluster: 6
[1656][CHIP:TOO]           Endpoint: null
[1656][CHIP:TOO]           DeviceType: null
[1656][CHIP:TOO]          }
[1656][CHIP:TOO]       FabricIndex: 4
[1656][CHIP:TOO]      }
";
        let entries = parse_acl_from_chip_log(s).unwrap();
        assert_eq!(
            entries[0].targets,
            Some(vec![AclTarget {
                cluster: Some(6),
                endpoint: None,
                device_type: None,
            }])
        );
    }

    #[test]
    fn too_log_zero_entries_is_ok_empty() {
        let entries = parse_acl_from_chip_log("[1656][CHIP:TOO]   ACL: 0 entries\n").unwrap();
        assert!(entries.is_empty());
    }

    #[test]
    fn too_log_realworld_prefix_format() {
        // 実機 v1.4.2.0 のログ接頭辞（小数点 ts + pid:tid + CHIP: 無しタグ）。
        let s = "\
[1780817887.948] [32231:32235] [TOO]   ACL: 1 entries
[1780817887.948] [32231:32235] [TOO]     [1]: {
[1780817887.948] [32231:32235] [TOO]       Privilege: 5
[1780817887.948] [32231:32235] [TOO]       AuthMode: 2
[1780817887.948] [32231:32235] [TOO]       Subjects: 1 entries
[1780817887.948] [32231:32235] [TOO]         [1]: 112233
[1780817887.948] [32231:32235] [TOO]       Targets: null
[1780817887.948] [32231:32235] [TOO]       FabricIndex: 4
[1780817887.948] [32231:32235] [TOO]      }
";
        assert_eq!(parse_acl_from_chip_log(s).unwrap(), vec![admin()]);
    }

    #[test]
    fn too_log_subjects_count_mismatch_is_parse_error() {
        // Subjects: 2 entries と宣言しつつ実際は 1 件しか無い（途中で切れた出力）。
        let s = "\
[1656][CHIP:TOO]   ACL: 1 entries
[1656][CHIP:TOO]     [1]: {
[1656][CHIP:TOO]       Privilege: 5
[1656][CHIP:TOO]       AuthMode: 2
[1656][CHIP:TOO]       Subjects: 2 entries
[1656][CHIP:TOO]         [1]: 112233
[1656][CHIP:TOO]       Targets: null
[1656][CHIP:TOO]       FabricIndex: 4
[1656][CHIP:TOO]      }
";
        let err = parse_acl_from_chip_log(s).expect_err("declared 2 subjects but only 1 present");
        assert_eq!(err.kind, ErrorKind::ParseError);
    }

    #[test]
    fn too_log_targets_count_mismatch_is_parse_error() {
        // Targets: 2 entries と宣言しつつ実際は 1 件しか無い。
        let s = "\
[1656][CHIP:TOO]   ACL: 1 entries
[1656][CHIP:TOO]     [1]: {
[1656][CHIP:TOO]       Privilege: 3
[1656][CHIP:TOO]       AuthMode: 2
[1656][CHIP:TOO]       Subjects: 1 entries
[1656][CHIP:TOO]         [1]: 112233
[1656][CHIP:TOO]       Targets: 2 entries
[1656][CHIP:TOO]         [1]: {
[1656][CHIP:TOO]           Cluster: 6
[1656][CHIP:TOO]           Endpoint: null
[1656][CHIP:TOO]           DeviceType: null
[1656][CHIP:TOO]          }
[1656][CHIP:TOO]       FabricIndex: 4
[1656][CHIP:TOO]      }
";
        let err = parse_acl_from_chip_log(s).expect_err("declared 2 targets but only 1 present");
        assert_eq!(err.kind, ErrorKind::ParseError);
    }

    #[test]
    fn too_log_unknown_key_inside_entry_is_parse_error() {
        // エントリ内の未知キーは黙殺せず fail-closed。
        let s = "\
[1656][CHIP:TOO]   ACL: 1 entries
[1656][CHIP:TOO]     [1]: {
[1656][CHIP:TOO]       Privilege: 5
[1656][CHIP:TOO]       AuthMode: 2
[1656][CHIP:TOO]       Wibble: 3
[1656][CHIP:TOO]       Subjects: 1 entries
[1656][CHIP:TOO]         [1]: 112233
[1656][CHIP:TOO]       Targets: null
[1656][CHIP:TOO]       FabricIndex: 4
[1656][CHIP:TOO]      }
";
        let err =
            parse_acl_from_chip_log(s).expect_err("unknown key inside entry must fail closed");
        assert_eq!(err.kind, ErrorKind::ParseError);
    }

    #[test]
    fn too_log_unknown_key_inside_target_is_parse_error() {
        // target 内の未知キー（garbled な Cluster 相当）も fail-closed。黙って
        // target が None（全許可）に劣化するのを防ぐ。
        let s = "\
[1656][CHIP:TOO]   ACL: 1 entries
[1656][CHIP:TOO]     [1]: {
[1656][CHIP:TOO]       Privilege: 3
[1656][CHIP:TOO]       AuthMode: 2
[1656][CHIP:TOO]       Subjects: 1 entries
[1656][CHIP:TOO]         [1]: 112233
[1656][CHIP:TOO]       Targets: 1 entries
[1656][CHIP:TOO]         [1]: {
[1656][CHIP:TOO]           Clusterz: 6
[1656][CHIP:TOO]           Endpoint: null
[1656][CHIP:TOO]           DeviceType: null
[1656][CHIP:TOO]          }
[1656][CHIP:TOO]       FabricIndex: 4
[1656][CHIP:TOO]      }
";
        let err =
            parse_acl_from_chip_log(s).expect_err("unknown key inside target must fail closed");
        assert_eq!(err.kind, ErrorKind::ParseError);
    }

    #[test]
    fn too_log_colonless_garbage_inside_entry_is_parse_error() {
        // エントリ内の colon 無し行はログ雑音ではなく garbled データとして扱う。
        let s = "\
[1656][CHIP:TOO]   ACL: 1 entries
[1656][CHIP:TOO]     [1]: {
[1656][CHIP:TOO]       Privilege: 5
[1656][CHIP:TOO]       garbage line no colon
[1656][CHIP:TOO]       AuthMode: 2
[1656][CHIP:TOO]       Subjects: 1 entries
[1656][CHIP:TOO]         [1]: 112233
[1656][CHIP:TOO]       Targets: null
[1656][CHIP:TOO]       FabricIndex: 4
[1656][CHIP:TOO]      }
";
        let err =
            parse_acl_from_chip_log(s).expect_err("colon-less line inside entry must fail closed");
        assert_eq!(err.kind, ErrorKind::ParseError);
    }

    #[test]
    fn too_log_broken_output_is_parse_error() {
        // ヘッダ無し / エントリ数不一致（途中で切れた出力）はどちらも ParseError。
        // 解釈できないまま write すると管理者エントリを失いかねないため、失敗側に倒す。
        for s in [
            "no acl here",
            "[1656][CHIP:TOO] something unparseable",
            // ヘッダは 2 entries だが 1 つしか無い（truncated）。
            "[1656][CHIP:TOO]   ACL: 2 entries\n\
             [1656][CHIP:TOO]     [1]: {\n\
             [1656][CHIP:TOO]       Privilege: 5\n\
             [1656][CHIP:TOO]       AuthMode: 2\n\
             [1656][CHIP:TOO]       Targets: null\n\
             [1656][CHIP:TOO]       FabricIndex: 4\n\
             [1656][CHIP:TOO]      }\n",
            // 必須フィールド欠け（Privilege 無し）。
            "[1656][CHIP:TOO]   ACL: 1 entries\n\
             [1656][CHIP:TOO]     [1]: {\n\
             [1656][CHIP:TOO]       AuthMode: 2\n\
             [1656][CHIP:TOO]       Targets: null\n\
             [1656][CHIP:TOO]       FabricIndex: 4\n\
             [1656][CHIP:TOO]      }\n",
        ] {
            let err = parse_acl_from_chip_log(s).expect_err(&format!("must fail: {s}"));
            assert_eq!(err.kind, ErrorKind::ParseError, "input: {s}");
        }
    }
}
