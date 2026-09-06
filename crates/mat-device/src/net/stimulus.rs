//! 外から走っているデバイスへ刺激（`core::stimulus::Stimulus`）を届ける
//! チャネル。`core::stimulus` が「何を加えるか」の語彙だけを持つのに対し、
//! こちらは「走っている `net::runtime` のループへどう渡すか」— I/O 側の
//! 都合を持つ。
//!
//! 経路が要る理由: `Node` は `net::runtime` の `serve_forever` ループが
//! 排他所有していて（`select!` の 1 分岐だけが 1 度に触る、`net::runtime`
//! のモジュール doc を参照）、外から `&mut Node` を取る手段は無い。刺激は
//! `mpsc` でループへ送り、`oneshot` で結果を受け取る — ループ側では
//! `Runtime::on_stimulus` が 1 分岐として処理するので、データグラム処理や
//! 購読レポート送信と競合しない。
//!
//! `Device::stimulus_handle()` が送信側（`StimulusHandle`、`Clone` 可 —
//! 複数の刺激元が同じデバイスを叩ける）を、`Device::run` が受信側を
//! ランタイムへ渡す。

use std::collections::HashMap;

use tokio::sync::{mpsc, oneshot};

use crate::core::stimulus::{Stimulus, StimulusError, StimulusOutcome};

/// ランタイム側の受け口: チャネルの受信側と、`[[device]]` の `id` →
/// endpoint 番号の解決表。2 つは常に一緒に動く（受信できても宛先を
/// 引けなければ刺激は適用できない）ので、`net::runtime::run` へは組で
/// 渡す — `GroupRxDeps` / `ServeState` と同じまとめ方。
pub struct StimulusIntake {
    pub requests: mpsc::Receiver<StimulusRequest>,
    pub endpoint_by_device: HashMap<String, u16>,
}

/// ループへ渡す 1 件の刺激。`device_id` は設定ファイル（`[[device]]` の
/// `id`）の名前で、endpoint 番号ではない — 番号は台帳
/// (`net::endpoint_ledger`) が決めるので刺激元は知らなくてよい。解決は
/// ランタイム側（`Runtime::on_stimulus` の `endpoint_by_device`）。
pub struct StimulusRequest {
    pub device_id: String,
    pub stimulus: Stimulus,
    pub reply: oneshot::Sender<Result<StimulusOutcome, StimulusApplyError>>,
}

/// 刺激が適用できなかった理由。`Node(..)` は `core` 側の判断
/// （endpoint はあるが誰も受け取らない等）、残り 2 つはこのチャネル層の
/// 事情 — 名前の解決に失敗した (`UnknownDevice`) か、ランタイムがもう
/// 動いていない (`Closed`)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StimulusApplyError {
    UnknownDevice(String),
    Node(StimulusError),
    Closed,
}

impl std::fmt::Display for StimulusApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StimulusApplyError::UnknownDevice(id) => write!(f, "no device with id {id}"),
            StimulusApplyError::Node(e) => write!(f, "{e}"),
            StimulusApplyError::Closed => write!(f, "the device runtime is not running"),
        }
    }
}

impl std::error::Error for StimulusApplyError {}

/// 刺激の送信ハンドル。`Clone` はチャネルの複製なので、複数の刺激元
/// （CLI、HTTP、テスト）が同じデバイスへ並行して送れる。
#[derive(Clone)]
pub struct StimulusHandle {
    tx: mpsc::Sender<StimulusRequest>,
}

impl StimulusHandle {
    /// 送信ハンドルと、ランタイムへ渡す受信側の組。`capacity` は
    /// バックプレッシャの深さ — ループが 1 件処理する間に溜められる件数
    /// で、超えたら `apply` が待つ（落とさない）。
    pub fn channel(capacity: usize) -> (Self, mpsc::Receiver<StimulusRequest>) {
        let (tx, rx) = mpsc::channel(capacity);
        (Self { tx }, rx)
    }

    /// 1 件の刺激をランタイムへ届け、適用結果を待つ。ランタイムが落ちて
    /// いる（受信側 drop）／応答前に消えた場合は `Closed` — 呼び側は
    /// 「届いたが失敗した」と「そもそも届かなかった」を区別できる。
    pub async fn apply(
        &self,
        device_id: &str,
        stimulus: Stimulus,
    ) -> Result<StimulusOutcome, StimulusApplyError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(StimulusRequest {
                device_id: device_id.to_string(),
                stimulus,
                reply,
            })
            .await
            .map_err(|_| StimulusApplyError::Closed)?;
        rx.await.map_err(|_| StimulusApplyError::Closed)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::stimulus::PressKind;

    #[tokio::test]
    async fn apply_delivers_the_request_and_returns_the_reply() {
        let (handle, mut rx) = StimulusHandle::channel(4);
        let task = tokio::spawn(async move {
            let req = rx.recv().await.expect("one request");
            assert_eq!(req.device_id, "front_button");
            assert_eq!(req.stimulus, Stimulus::Press(PressKind::Short));
            let _ = req.reply.send(Ok(StimulusOutcome {
                changed: vec![(2, 0x003B, 0x0001)],
                event_numbers: vec![7],
            }));
        });
        let out = handle
            .apply("front_button", Stimulus::Press(PressKind::Short))
            .await
            .expect("applied");
        assert_eq!(out.event_numbers, vec![7]);
        task.await.unwrap();
    }

    /// 受信側が落ちていれば `Closed`（送信できない）。応答を返さずに
    /// `reply` を drop した場合も同じ — どちらも「ランタイムが応えない」。
    #[tokio::test]
    async fn a_dead_runtime_is_reported_as_closed() {
        let (handle, rx) = StimulusHandle::channel(1);
        drop(rx);
        assert_eq!(
            handle
                .apply("front_button", Stimulus::SetState(true))
                .await
                .unwrap_err(),
            StimulusApplyError::Closed
        );

        let (handle, mut rx) = StimulusHandle::channel(1);
        let task = tokio::spawn(async move {
            let req = rx.recv().await.expect("one request");
            drop(req.reply); // 応答せずに消える
        });
        assert_eq!(
            handle
                .apply("front_button", Stimulus::SetState(true))
                .await
                .unwrap_err(),
            StimulusApplyError::Closed
        );
        task.await.unwrap();
    }

    #[test]
    fn display_distinguishes_the_three_failures() {
        assert_eq!(
            StimulusApplyError::UnknownDevice("porch".into()).to_string(),
            "no device with id porch"
        );
        assert_eq!(
            StimulusApplyError::Node(StimulusError::Unsupported).to_string(),
            StimulusError::Unsupported.to_string()
        );
        assert_eq!(
            StimulusApplyError::Closed.to_string(),
            "the device runtime is not running"
        );
    }
}
