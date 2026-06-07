//! 子ランナー: `chip-tool` をサブプロセス起動し、stdout/stderr を捕捉する。
//!
//! `mat` はプロトコルを直接喋らない。read/write/invoke/commission の実体は
//! すべて `chip-tool` に委譲し、ここはその起動と出力捕捉だけを担う。

use std::ffi::OsString;
use std::path::PathBuf;
use std::process::Command;

use crate::error::{ErrorKind, MatError};

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

/// chip-tool 出力に混じる ANSI エスケープ列（色付け等）を除去する。
///
/// 対象は CSI シーケンス（`ESC [ … 終端バイト`、SGR の `m` を含む）。終端は
/// 0x40–0x7E のバイト。ESC 単独や非 CSI シーケンスは ESC のみ落とす。これを
/// 通さないと `Data = true,\x1b[0m` のように値末尾へ色リセットが残り、bool/数値の
/// 正規化が崩れる。
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push(c);
            continue;
        }
        // ESC。CSI（`[`）なら終端バイト 0x40–0x7E まで読み飛ばす。
        if chars.clone().next() == Some('[') {
            chars.next(); // '[' を消費
            for d in chars.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&d) {
                    break;
                }
            }
        }
        // 非 CSI の ESC は単に捨てる。
    }
    out
}

/// `chip-tool` の失敗出力から `mat` の失敗種別を分類する。
///
/// `chip-tool` の exit code は粗いため、stdout/stderr のテキストから
/// timeout / unreachable / device_rejected を推定する。判定できなければ
/// 呼び出し側が `ChildFailed` / `CommissionFailed` 等にフォールバックできるよう
/// `None` を返す。
pub fn classify_failure(stdout: &str, stderr: &str) -> Option<ErrorKind> {
    let hay = format!("{stdout}\n{stderr}").to_ascii_lowercase();

    // 順序に意味あり: より具体的なシグナルを先に見る。
    if hay.contains("timeout") || hay.contains("timed out") || hay.contains("chip error 0x00000032")
    {
        return Some(ErrorKind::Timeout);
    }
    if hay.contains("no route to host")
        || hay.contains("host is unreachable")
        || hay.contains("network is unreachable")
        || hay.contains("unreachable")
        || hay.contains("could not find an operational node")
        || hay.contains("couldn't reach")
    {
        return Some(ErrorKind::Unreachable);
    }
    if hay.contains("status 0x81") // IM Status: Failure
        || hay.contains("unsupported")
        || hay.contains("constraint")
        || hay.contains("rejected")
        || hay.contains("access denied")
        || hay.contains("invalid command")
    {
        return Some(ErrorKind::DeviceRejected);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_timeout() {
        let s = "[1656][CHIP:DMG] CHIP Error 0x00000032: Timeout";
        assert_eq!(classify_failure(s, ""), Some(ErrorKind::Timeout));
    }

    #[test]
    fn classifies_unreachable() {
        assert_eq!(
            classify_failure("", "connect: No route to host"),
            Some(ErrorKind::Unreachable)
        );
    }

    #[test]
    fn classifies_device_rejected() {
        let s = "Received Command Response Status ... status 0x81 (Failure)";
        assert_eq!(classify_failure(s, ""), Some(ErrorKind::DeviceRejected));
    }

    #[test]
    fn unknown_failure_is_none() {
        assert_eq!(classify_failure("some other gibberish", ""), None);
    }

    #[test]
    fn strip_ansi_removes_sgr_sequences() {
        // 実機 chip-tool は値末尾に色リセットを残す（read が `true, \x1b[0m` を返した）。
        assert_eq!(strip_ansi("true, \x1b[0m"), "true, ");
        // 行頭の色付け + リセットの両方を除去。
        assert_eq!(
            strip_ansi("\x1b[0;34m[1780817887.948] foo\x1b[0m"),
            "[1780817887.948] foo"
        );
        // ANSI を含まない行はそのまま。
        assert_eq!(strip_ansi("plain text"), "plain text");
    }
}
