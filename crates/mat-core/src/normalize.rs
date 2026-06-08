//! chip-tool の生出力を正規化・分類する共通ロジック。
//!
//! `chip-tool` の exit code は粗い（おおむね `1`）ため、stdout/stderr のテキストから
//! 失敗種別を分類する。ANSI 除去と合わせて、one-shot の `mat` と常駐の `matd` の
//! 両方が同じパイプラインを通せるよう core に置く。

use crate::error::ErrorKind;

/// chip-tool 出力に混じる ANSI エスケープ列（色付け等）を除去する。
///
/// 対象は CSI シーケンス（`ESC [ … 終端バイト`、SGR の `m` を含む）。終端は
/// 0x40–0x7E のバイト。ESC 単独や非 CSI シーケンスは ESC のみ落とす。これを
/// 通さないと `Data = true,\x1b[0m` のように値末尾へ色リセットが残り、bool/数値の
/// 正規化が崩れる。
pub fn strip_ansi(s: &str) -> String {
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
        || hay.contains("invalid command") // 実機は `INVALID_COMMAND`（小文字化で `_`）
        || hay.contains("invalid_command")
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
    fn classifies_invalid_command_as_rejected() {
        // 実機 open-window 拒否: `Status=0x85` + `INVALID_COMMAND`（アンダースコア）。
        let s = "IM Error 0x00000585: General error: 0x85 (INVALID_COMMAND)";
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
