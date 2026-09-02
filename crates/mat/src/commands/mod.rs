//! サブコマンド実装。各 `run` は副作用（ストア更新・stdout 出力）を行い、
//! 成功なら `Ok(())`、失敗なら [`mat_core::error::MatError`] を返す。

pub mod commission;
pub mod diag;
pub mod discover;
pub mod fabric;
