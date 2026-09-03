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

use serde_json::Value;

use crate::error::MatError;

/// Matter AccessControl の privilege。3 = Operate（Administer は authMode=Group と
/// 組み合わせ不可のため、group エントリは Operate 固定）。
pub const PRIVILEGE_OPERATE: u8 = 3;
/// Matter AccessControl の authMode。3 = Group。
pub const AUTH_MODE_GROUP: u8 = 3;

/// ACL エントリの target（クラスタ / エンドポイント / デバイス種別の限定）。
/// `mat` 自身は targets: null（全許可）しか生成しないが、既存エントリの保全のため
/// read 側は非 null も解釈できる必要がある。
/// IM read の数値キー規約（`{0: cluster, 1: endpoint, 2: deviceType}`）から解釈される。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AclTarget {
    pub cluster: Option<u32>,
    pub endpoint: Option<u16>,
    pub device_type: Option<u32>,
}

/// ACL エントリ。IM 直経路の `AccessControlEntryStruct` 列（数値キー JSON）から解釈される。
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// 既存 ACL に group エントリを追記した全リストを返す。既に authMode=Group かつ
/// privilege が Operate 以上のエントリがあれば `None`（冪等、write 不要）。
/// privilege が Operate 未満（例: View）の Group エントリは groupcast をまだ拒否
/// するため「付与済み」とみなさず、Operate エントリを追記する（ACL は加算的な
/// ので、弱いエントリの隣に強いエントリを足すのは正当な修復）。fabricIndex は
/// 既存エントリの先頭から引き継ぐ（read 値をそのまま渡す方針。エントリ 0 件は
/// 起きない想定だが、その場合は 0 — サーバ側で置換される）。
///
/// **前提**: `entries` は fabric-filtered read（IsFabricFiltered=true）で得た
/// **自 fabric のみ**のエントリである前提（呼び出し元 `mat-native::ops::
/// ensure_group_acl` が渡す read 結果は `mat-controller::im::
/// encode_read_request` 経由で常にこれを満たす）。非フィルタの集合を渡すと、
/// 他 fabric の Group エントリを「付与済み」と誤認して冪等 skip し、自 fabric
/// に Group ACL が無いまま groupcast が届かなくなる（マルチ admin デバイス、
/// 例えば自宅 fabric + Home Assistant fabric 併存で顕在化）。
pub fn merge_group_entry(entries: &[AclEntry], group_id: u16) -> Option<Vec<AclEntry>> {
    let gid = u64::from(group_id);
    if entries.iter().any(|e| {
        e.auth_mode == AUTH_MODE_GROUP
            && e.subjects.contains(&gid)
            && e.privilege >= PRIVILEGE_OPERATE
    }) {
        return None;
    }
    let fabric_index = entries.first().map(|e| e.fabric_index).unwrap_or(0);
    let mut merged = entries.to_vec();
    merged.push(group_acl_entry(group_id, fabric_index));
    Some(merged)
}

/// Group エントリ（authMode=Group かつ subjects == [group_id]）を除いた全リスト。
/// 該当が 1 件も無ければ `None`（冪等、write 不要）。`merge_group_entry` と同じく
/// fabric-filtered read の結果を渡すこと。
pub fn without_group_entry(entries: &[AclEntry], group_id: u16) -> Option<Vec<AclEntry>> {
    let keep: Vec<AclEntry> = entries
        .iter()
        .filter(|e| !(e.auth_mode == AUTH_MODE_GROUP && e.subjects == [u64::from(group_id)]))
        .cloned()
        .collect();
    (keep.len() != entries.len()).then_some(keep)
}

/// native（IM）直経路の `AccessControlEntryStruct` 列 —— `tlv_to_json` の数値
/// キー規約（`{1: privilege, 2: authMode, 3: subjects, 4: targets, 254:
/// fabricIndex}`）から `AclEntry` 列へ。targets 内は `"0"`=cluster,
/// `"1"`=endpoint, `"2"`=deviceType。解釈不能なら `ParseError`（read できなければ
/// write しない既存方針、モジュール冒頭のコメント参照）。
pub fn entries_from_im_json(v: &Value) -> Result<Vec<AclEntry>, MatError> {
    let arr = v
        .as_array()
        .ok_or_else(|| MatError::parse_error(format!("ACL ws value is not an array: {v}")))?;
    arr.iter().map(ws_entry).collect()
}

/// fail-closed ポリシー: 既知キー以外が 1 つでもあれば `ParseError`。
/// 黙って落とすと、全置換 write で劣化したエントリ（未知フィールドが欠落した状態）を
/// 書き込んでしまうため、未知フィールドは拒否する必要がある。
/// テスト例：`ws_value_unknown_entry_field_is_parse_error`。
fn reject_unknown_keys(
    obj: &serde_json::Map<String, Value>,
    known: &[&str],
    what: &str,
) -> Result<(), MatError> {
    for key in obj.keys() {
        if !known.iter().any(|k| k == key) {
            return Err(MatError::parse_error(format!(
                "ACL ws {what} has unexpected field {key}"
            )));
        }
    }
    Ok(())
}

fn ws_entry(v: &Value) -> Result<AclEntry, MatError> {
    let obj = v
        .as_object()
        .ok_or_else(|| MatError::parse_error(format!("ACL ws entry is not an object: {v}")))?;
    reject_unknown_keys(obj, &["1", "2", "3", "4", "254"], "entry")?;
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
    reject_unknown_keys(obj, &["0", "1", "2"], "target")?;
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
    fn merge_appends_operate_entry_when_existing_group_entry_is_view_only() {
        // 既存の Group エントリが View（1）しか無い場合、groupcast はまだ拒否される
        // ため「付与済み」とみなさず Operate エントリを追記する（ACL は加算的なので
        // 弱い既存エントリの隣に強いエントリを足すのは正当な修復）。
        let view_only_group = AclEntry {
            privilege: 1, // View
            auth_mode: AUTH_MODE_GROUP,
            subjects: vec![10],
            targets: None,
            fabric_index: 4,
        };
        let merged = merge_group_entry(std::slice::from_ref(&view_only_group), 10)
            .expect("must repair with Operate");
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0], view_only_group);
        assert_eq!(merged[1], group_acl_entry(10, 4));
    }

    use crate::error::ErrorKind;
    use serde_json::json;

    #[test]
    fn ws_value_numeric_keys_parse() {
        // 実機で確定済みの ws 応答形: 数値フィールド ID がキー
        // （"1"=privilege, "2"=authMode, "3"=subjects, "4"=targets, "254"=fabricIndex）。
        let v = json!([{"1":5,"2":2,"3":[112233],"4":null,"254":4}]);
        assert_eq!(entries_from_im_json(&v).unwrap(), vec![admin()]);
    }

    #[test]
    fn ws_value_parses_admin_and_group() {
        let v = json!([
            {"1":5,"2":2,"3":[112233],"4":null,"254":4},
            {"1":3,"2":3,"3":[10],"4":null,"254":4}
        ]);
        assert_eq!(
            entries_from_im_json(&v).unwrap(),
            vec![admin(), group_acl_entry(10, 4)]
        );
    }

    #[test]
    fn ws_value_targets_non_null() {
        // targets 内は "0"=cluster, "1"=endpoint, "2"=deviceType。
        let v = json!([{"1":3,"2":2,"3":[112233],"4":[{"0":6,"1":1,"2":null}],"254":4}]);
        let entries = entries_from_im_json(&v).unwrap();
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
            let err = entries_from_im_json(&v).expect_err(&format!("must fail: {v}"));
            assert_eq!(err.kind, ErrorKind::ParseError, "input: {v}");
        }
    }

    #[test]
    fn ws_value_unknown_entry_field_is_parse_error() {
        // fail-closed を ws 変換に要求する。未知フィールドを黙って落とすと
        // 全置換 write で劣化したエントリを書き込んでしまう。
        let v = json!([{"1":5,"2":2,"3":[1],"4":null,"254":1,"99":7}]);
        let err = entries_from_im_json(&v).expect_err("unknown entry field must fail closed");
        assert_eq!(err.kind, ErrorKind::ParseError);
    }

    #[test]
    fn ws_value_unknown_target_field_is_parse_error() {
        let v = json!([{"1":5,"2":2,"3":[1],"4":[{"0":6,"1":1,"2":null,"9":1}],"254":1}]);
        let err = entries_from_im_json(&v).expect_err("unknown target field must fail closed");
        assert_eq!(err.kind, ErrorKind::ParseError);
    }

    #[test]
    fn entries_from_im_json_maps_numeric_keys() {
        let v = serde_json::json!([
            {"1": 5, "2": 2, "3": [112233445566u64], "4": null, "254": 2}
        ]);
        let e = entries_from_im_json(&v).unwrap();
        assert_eq!(e[0].privilege, 5);
        assert_eq!(e[0].auth_mode, 2);
        assert_eq!(e[0].subjects, vec![112233445566]);
        assert!(e[0].targets.is_none());
        assert_eq!(e[0].fabric_index, 2);
        // 解釈不能（privilege 欠落）は Err — read できなければ write しない方針の要。
        assert!(entries_from_im_json(&serde_json::json!([{"2": 2}])).is_err());
    }

    #[test]
    fn without_group_entry_removes_only_matching_group_rows() {
        let admin = AclEntry {
            privilege: 5,
            auth_mode: 2,
            subjects: vec![112233],
            targets: None,
            fabric_index: 1,
        };
        let g1 = group_acl_entry(1, 1);
        let g2 = group_acl_entry(2, 1);
        let out = without_group_entry(&[admin.clone(), g1, g2.clone()], 1).unwrap();
        assert_eq!(out, vec![admin.clone(), g2]);
        assert!(
            without_group_entry(&[admin], 1).is_none(),
            "無ければ None（write 不要）"
        );
    }
}
