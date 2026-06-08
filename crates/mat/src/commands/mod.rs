//! サブコマンド実装。各 `run` は副作用（chip-tool 起動・ストア更新・stdout 出力）
//! を行い、成功なら `Ok(())`、失敗なら [`mat_core::error::MatError`] を返す。

pub mod commission;
pub mod describe;
pub mod discover;
pub mod group;
pub mod invoke;
pub mod open_window;
pub mod read;
pub mod write;
