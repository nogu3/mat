//! `matd` のライブラリ面。バイナリ（`main.rs`）と統合テストの両方から使う。
//!
//! 中身は ARCHITECTURE.md の Phase 4 を参照。バイナリ crate のモジュールは外部
//! テストから見えないため、ロジックはここに公開し `main.rs` は薄い起動層に保つ。

pub mod backend;
pub mod lock;
pub mod protocol;
pub mod server;
