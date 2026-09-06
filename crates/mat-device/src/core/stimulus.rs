//! 外部からデバイスへ加える「刺激」— 仮想デバイス（`matv`）に人が
//! ボタンを押させたり、センサの値を動かさせたりする入口。Matter の
//! プロトコルではない（コントローラから来るのは Invoke / Write であって
//! 刺激ではない）: これは物理世界の代わりで、`ClusterHandler::stimulate`
//! が受けて属性変化とイベント発火に翻訳する。I/O-free — 型だけ。

/// ボタン押下の種類（spec §1.12 Switch cluster の
/// InitialPress/ShortRelease/LongPress/MultiPressComplete に対応する
/// 「人の操作」側の語彙）。`Multi(n)` の `n` は押した回数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressKind {
    Short,
    Long,
    Multi(u8),
}

/// 1 つの刺激。endpoint は `Node::stimulate` の引数で指定するので、ここには
/// 「何をしたか」だけが入る。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stimulus {
    Press(PressKind),
    SetState(bool),
}

/// `ClusterHandler::stimulate` の返答。`Unsupported` は「このクラスタの
/// 担当ではない」（`Node` は同じ endpoint の次のクラスタを試す）、
/// `Rejected` は「担当だが今は受けられない」（理由付きで即エラー）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StimulusReply {
    Applied,
    Unsupported,
    Rejected(&'static str),
}

/// 刺激が実際に起こしたこと: 変化した `(endpoint, cluster, attribute)` と
/// 採番された EventNumber。購読の dirty 判定とイベントレポートの両方が
/// これを見る。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct StimulusOutcome {
    pub changed: Vec<(u16, u32, u32)>,
    pub event_numbers: Vec<u64>,
}

/// 刺激が適用できなかった理由。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StimulusError {
    UnknownEndpoint,
    Unsupported,
    Rejected(&'static str),
}

impl std::fmt::Display for StimulusError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StimulusError::UnknownEndpoint => write!(f, "no such endpoint"),
            StimulusError::Unsupported => {
                write!(f, "stimulus not supported by any cluster on this endpoint")
            }
            StimulusError::Rejected(reason) => write!(f, "{reason}"),
        }
    }
}

impl std::error::Error for StimulusError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_spells_out_each_reason() {
        assert_eq!(
            StimulusError::UnknownEndpoint.to_string(),
            "no such endpoint"
        );
        assert_eq!(
            StimulusError::Unsupported.to_string(),
            "stimulus not supported by any cluster on this endpoint"
        );
        assert_eq!(
            StimulusError::Rejected("button is held").to_string(),
            "button is held"
        );
    }
}
