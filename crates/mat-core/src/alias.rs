//! optional な alias 解決（aliases.json）。
//!
//! store 配下の `aliases.json` が**あれば** node / group / endpoint の名前→数値
//! 解決を行い、無ければ完全に従来動作（数値のみ）。ワイヤ・chip-tool / matd に
//! 渡る値は常に数値で、解決は CLI 層の前処理に閉じる。
//!
//! alias 名は純数字・空文字を禁止（数値指定とのシャドーイングを構造的に排除）。
//! `endpoints` はノード配下定義（外側キーはノード alias または node_id の数字
//! 文字列）。endpoint 番号はノードごとに意味が違うため、グローバル辞書にしない。

use std::str::FromStr;

/// `-n/--node` / `--nodes` が受ける「数値 or alias」。clap が [`FromStr`] で受け、
/// resolve 層が `AliasBook` で `Id` へ確定する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeRef {
    Id(u64),
    Alias(String),
}

/// `-g/--group` が受ける「数値 or alias」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupRef {
    Id(u16),
    Alias(String),
}

/// `-e/--endpoint` が受ける「数値 or alias」（ノードを取るコマンドのみ）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndpointRef {
    Id(u16),
    Alias(String),
}

macro_rules! impl_ref {
    ($ty:ident, $num:ty, $what:literal) => {
        impl FromStr for $ty {
            type Err = std::convert::Infallible;
            /// 数値として parse できれば `Id`、できなければ `Alias`（最優先で従来互換）。
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                Ok(s.parse::<$num>()
                    .map($ty::Id)
                    .unwrap_or_else(|_| $ty::Alias(s.to_string())))
            }
        }
        impl $ty {
            /// 解決済み（`Id`）前提で数値を返す。resolve 層通過後にのみ呼ぶ。
            pub fn id(&self) -> $num {
                match self {
                    $ty::Id(n) => *n,
                    $ty::Alias(a) => {
                        unreachable!(
                            "unresolved {} alias '{a}': resolve_command must run first",
                            $what
                        )
                    }
                }
            }
        }
    };
}
impl_ref!(NodeRef, u64, "node");
impl_ref!(GroupRef, u16, "group");
impl_ref!(EndpointRef, u16, "endpoint");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_parses_to_id() {
        assert_eq!("5".parse::<NodeRef>().unwrap(), NodeRef::Id(5));
        assert_eq!("1".parse::<EndpointRef>().unwrap(), EndpointRef::Id(1));
        assert_eq!("258".parse::<GroupRef>().unwrap(), GroupRef::Id(258));
    }

    #[test]
    fn non_numeric_parses_to_alias() {
        assert_eq!(
            "living-light".parse::<NodeRef>().unwrap(),
            NodeRef::Alias("living-light".into())
        );
        // 数字始まりでも数値として parse できなければ alias。
        assert_eq!(
            "2f-light".parse::<NodeRef>().unwrap(),
            NodeRef::Alias("2f-light".into())
        );
    }

    #[test]
    fn out_of_range_number_falls_to_alias() {
        // u16 を溢れる数字列は GroupRef では alias 扱いになり、解決で
        // unknown alias（exit 2）に落ちる（従来の clap 範囲エラーも exit 2）。
        assert_eq!(
            "70000".parse::<GroupRef>().unwrap(),
            GroupRef::Alias("70000".into())
        );
    }

    #[test]
    fn id_returns_inner_value() {
        assert_eq!(NodeRef::Id(7).id(), 7);
        assert_eq!(GroupRef::Id(258).id(), 258);
        assert_eq!(EndpointRef::Id(2).id(), 2);
    }
}
