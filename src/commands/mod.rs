//! サブコマンド実装。各 `run` は副作用（chip-tool 起動・ストア更新・stdout 出力）
//! を行い、成功なら `Ok(())`、失敗なら [`crate::error::MatError`] を返す。

pub mod commission;
pub mod describe;
pub mod discover;
pub mod invoke;
pub mod read;
pub mod write;
