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
}
