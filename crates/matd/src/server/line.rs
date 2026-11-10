//! クライアント 1 行の上限付き読み取り（[`BoundedLines`]）。
//!
//! `tokio::io::Lines` は 1 行の長さに上限が無く、改行を送らないクライアントに
//! matd のメモリを際限なく溜めさせられる。ここでは上限
//! （[`MAX_REQUEST_LINE_BYTES`]）を超えた時点で読むのをやめ、それ以上は
//! バッファしない。

use tokio::io::{AsyncBufRead, AsyncBufReadExt};

/// クライアントから受ける 1 リクエスト行（改行を除く）の上限バイト数: 1 MiB。
///
/// 正当なリクエストは小さな JSON 1 行（最大でも generic `write` の list/struct
/// 値や `group_provision` のノード列で数 KiB 程度）なので、桁違いの余裕を
/// 持たせつつ、1 接続が占有できるメモリを有限にする。
pub const MAX_REQUEST_LINE_BYTES: usize = 1024 * 1024;

/// [`BoundedLines::next_line`] の失敗。
#[derive(Debug)]
pub(super) enum LineError {
    /// 1 行が上限を超えた。超過分は読み捨て済みとは限らない（呼び手は接続を閉じる）。
    TooLong,
    /// I/O エラー、または UTF-8 でない行（`tokio::io::Lines` と同じく
    /// `InvalidData`）。
    Io(std::io::Error),
}

/// 上限付きの行リーダ。`tokio::io::Lines::next_line` の代替で、改行（`\n`、
/// 直前の `\r` も除く）を外した行を返す。EOF 直前の改行なし断片も 1 行として
/// 返す（`Lines` と同じ）。
///
/// cancel safe: `tokio::select!` の分岐で使われる（op 実行中の先読み・listen
/// 中の切断検知）。await 点は `fill_buf` だけで、読んだバイトの移動と `consume`
/// は await を挟まず同期的に行い、途中まで読んだ行は `buf` に保持する。
pub(super) struct BoundedLines<R> {
    reader: R,
    buf: Vec<u8>,
    max: usize,
}

impl<R: AsyncBufRead + Unpin> BoundedLines<R> {
    pub(super) fn new(reader: R, max: usize) -> Self {
        Self {
            reader,
            buf: Vec::new(),
            max,
        }
    }

    pub(super) async fn next_line(&mut self) -> Result<Option<String>, LineError> {
        loop {
            let available = self.reader.fill_buf().await.map_err(LineError::Io)?;
            if available.is_empty() {
                // EOF。
                if self.buf.is_empty() {
                    return Ok(None);
                }
                return finish(std::mem::take(&mut self.buf), false).map(Some);
            }
            let newline = available.iter().position(|&b| b == b'\n');
            let take = newline.unwrap_or(available.len());
            if self.buf.len() + take > self.max {
                self.buf = Vec::new();
                return Err(LineError::TooLong);
            }
            self.buf.extend_from_slice(&available[..take]);
            match newline {
                Some(pos) => {
                    self.reader.consume(pos + 1);
                    return finish(std::mem::take(&mut self.buf), true).map(Some);
                }
                None => self.reader.consume(take),
            }
        }
    }
}

/// UTF-8 として返す。改行で終わった行は `\r\n` の `\r` も外す（`Lines` と同じく
/// EOF で終わった断片の `\r` は残す）。
fn finish(mut line: Vec<u8>, newline_terminated: bool) -> Result<String, LineError> {
    if newline_terminated && line.last() == Some(&b'\r') {
        line.pop();
    }
    String::from_utf8(line)
        .map_err(|e| LineError::Io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn read_all(input: &[u8], max: usize) -> Vec<Result<String, String>> {
        // 小さい内部バッファで、行が fill_buf を跨ぐ経路も通す。
        let reader = tokio::io::BufReader::with_capacity(4, input);
        let mut lines = BoundedLines::new(reader, max);
        let mut out = Vec::new();
        loop {
            match lines.next_line().await {
                Ok(Some(l)) => out.push(Ok(l)),
                Ok(None) => break,
                Err(LineError::TooLong) => {
                    out.push(Err("too long".into()));
                    break;
                }
                Err(LineError::Io(e)) => {
                    out.push(Err(e.to_string()));
                    break;
                }
            }
        }
        out
    }

    #[tokio::test]
    async fn splits_lines_like_tokio_lines() {
        assert_eq!(
            read_all(b"ab\r\ncdefgh\n\nlast", 16).await,
            vec![
                Ok("ab".to_string()),
                Ok("cdefgh".to_string()),
                Ok(String::new()),
                Ok("last".to_string()),
            ]
        );
    }

    #[tokio::test]
    async fn line_of_exactly_max_is_accepted_and_max_plus_one_is_rejected() {
        assert_eq!(
            read_all(b"12345678\nx\n", 8).await,
            vec![Ok("12345678".to_string()), Ok("x".to_string())]
        );
        assert_eq!(
            read_all(b"ok\n123456789\nnever\n", 8).await,
            vec![Ok("ok".to_string()), Err("too long".to_string())]
        );
        // 改行が来ないまま上限を超える（EOF 前）のも拒否。
        assert_eq!(
            read_all(b"123456789", 8).await,
            vec![Err("too long".to_string())]
        );
    }

    #[tokio::test]
    async fn non_utf8_line_is_invalid_data() {
        let out = read_all(b"\xff\xfe\n", 8).await;
        assert_eq!(out.len(), 1);
        assert!(out[0].is_err());
    }
}
