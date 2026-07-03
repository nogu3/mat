//! `mat-core` — `mat`（one-shot CLI）と `matd`（常駐デーモン）が共有するコア。
//!
//! 壊れやすい chip-tool 出力パーサ（`Data = ...` 形式）、`mat` の JSON スキーマと
//! `timestamp` 付与、エラー種別と exit code 分類、認証情報ストア（KVS）を一箇所で
//! 保守する。バージョン更新で chip-tool の出力が変わってもここのテストで気づける。

pub mod alias;
pub mod diag;
pub mod error;
pub mod group;
pub mod normalize;
pub mod output;
pub mod parse;
pub mod reachability;
pub mod socket;
pub mod store;
