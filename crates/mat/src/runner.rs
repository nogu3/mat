//! 子ランナー: `chip-tool` をサブプロセス起動し、stdout/stderr を捕捉する。
//!
//! `mat` はプロトコルを直接喋らない。read/write/invoke/commission の実体は
//! すべて `chip-tool` に委譲し、ここはその起動と出力捕捉だけを担う。

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

use mat_core::error::{ErrorKind, MatError};
use mat_core::normalize::strip_ansi;

/// `chip-tool` の一回分の実行結果（生のテキスト）。
pub struct RunOutput {
    pub stdout: String,
    pub stderr: String,
    /// プロセス終了コード（不明なら `None`）。
    pub code: Option<i32>,
}

impl RunOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0)
    }
}

/// `chip-tool` バイナリへのハンドル。
pub struct ChipTool {
    bin: OsString,
    storage_dir: PathBuf,
}

impl ChipTool {
    /// バイナリを解決する。`MAT_CHIP_TOOL_BIN` があればフルパス上書き、無ければ
    /// PATH 上の `chip-tool`。`storage_dir` は `chip-tool` の永続ストレージ。
    pub fn new(storage_dir: impl Into<PathBuf>) -> Self {
        let bin =
            std::env::var_os("MAT_CHIP_TOOL_BIN").unwrap_or_else(|| OsString::from("chip-tool"));
        ChipTool {
            bin,
            storage_dir: storage_dir.into(),
        }
    }

    /// 引数を渡して `chip-tool` を実行。`--storage-directory` を自動付与する。
    ///
    /// バイナリが見つからない / 実行不可なら [`ErrorKind::ChildNotFound`]（exit 12）。
    pub fn run<I, S>(&self, args: I) -> Result<RunOutput, MatError>
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        let mut cmd = Command::new(&self.bin);
        for a in args {
            cmd.arg(a.into());
        }
        cmd.arg("--storage-directory").arg(&self.storage_dir);

        tracing::debug!(bin = ?self.bin, "spawning chip-tool");

        let output = cmd.output().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                MatError::child_not_found(format!(
                    "chip-tool binary not found ({:?}); set MAT_CHIP_TOOL_BIN or add it to PATH",
                    self.bin
                ))
            } else if e.kind() == std::io::ErrorKind::PermissionDenied {
                MatError::child_not_found(format!(
                    "chip-tool binary not executable ({:?}): {e}",
                    self.bin
                ))
            } else {
                MatError::new(
                    ErrorKind::ChildFailed,
                    format!("failed to spawn chip-tool: {e}"),
                )
            }
        })?;

        // chip-tool は TTY 非接続でも ANSI の色付け（SGR）を出す版がある。これが
        // 残ると値（`true, \x1b[0m`）や discover の hostname/address を汚すため、
        // パースに回す前にここで一括除去する（出力正規化はランナーの責務）。
        let stdout = strip_ansi(&String::from_utf8_lossy(&output.stdout));
        let stderr = strip_ansi(&String::from_utf8_lossy(&output.stderr));

        // chip-tool の出力は呑まず、少なくとも debug で残す。診断の大半は stdout に
        // 出るため stdout も残す（パース失敗時の切り分けに必要）。
        if !stdout.trim().is_empty() {
            tracing::debug!(target: "chip_tool::stdout", "{}", stdout.trim_end());
        }
        if !stderr.trim().is_empty() {
            tracing::debug!(target: "chip_tool::stderr", "{}", stderr.trim_end());
        }

        Ok(RunOutput {
            stdout,
            stderr,
            code: output.status.code(),
        })
    }
}
