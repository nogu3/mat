//! `--stdin-control`: stdin の JSON 行をデバイスへの刺激に変換する開発用フック。
//! 1 行 = 1 刺激。成功は stdout に JSON 1 行、失敗は stderr に mat 形式の
//! error JSON 1 行（mat の流儀: stdout=JSON、診断=stderr）。フックは 1 行
//! 失敗しても読み続ける — 対話的に叩く開発用の入口なので、打ち間違いで
//! プロセスが落ちない方が使いやすい。
//!
//! Matter のプロトコル面ではない: ここで扱うのは `core::stimulus::Stimulus`
//! （物理世界の代わり）で、コントローラから来る Invoke / Write とは別経路。

use mat_device::core::stimulus::{PressKind, Stimulus};
use mat_device::net::stimulus::{StimulusApplyError, StimulusHandle};

/// stdin の 1 行を解釈した結果。`device` は `[[device]]` の `id`
/// （endpoint 番号は台帳が決めるので、刺激元は知らなくてよい）。
#[derive(Debug, PartialEq, Eq)]
pub struct ControlLine {
    pub device: String,
    pub stimulus: Stimulus,
}

/// stdin の 1 行（JSON オブジェクト）を `ControlLine` へ。
/// `{"device": "btn", "press": "short"|"long"|"multi", "count": n}` または
/// `{"device": "door", "state": true|false}`。エラーはそのまま
/// `error_json("parse_error", ..)` の `detail` になるので、何が悪いのかが
/// 分かる文にする。
pub fn parse_control_line(line: &str) -> Result<ControlLine, String> {
    let v: serde_json::Value =
        serde_json::from_str(line).map_err(|e| format!("control line is not JSON: {e}"))?;
    let device = v
        .get("device")
        .and_then(|d| d.as_str())
        .ok_or("control line needs a string \"device\"")?
        .to_string();
    let press = v.get("press").and_then(|p| p.as_str());
    let state = v.get("state").and_then(|s| s.as_bool());
    let stimulus = match (press, state) {
        (Some(_), Some(_)) => {
            return Err("control line has both \"press\" and \"state\"; use one".into());
        }
        (None, None) => return Err("control line needs \"press\" or \"state\"".into()),
        (None, Some(s)) => Stimulus::SetState(s),
        (Some("short"), None) => Stimulus::Press(PressKind::Short),
        (Some("long"), None) => Stimulus::Press(PressKind::Long),
        (Some("multi"), None) => {
            let n = v
                .get("count")
                .and_then(|c| c.as_u64())
                .ok_or("\"press\":\"multi\" needs an integer \"count\"")?;
            Stimulus::Press(PressKind::Multi(
                u8::try_from(n).map_err(|_| "\"count\" out of range".to_string())?,
            ))
        }
        (Some(other), None) => {
            return Err(format!("unknown press kind {other:?} (short|long|multi)"))
        }
    };
    Ok(ControlLine { device, stimulus })
}

/// 適用できた刺激の stdout 行。`event_numbers` は刺激が採番させた
/// EventNumber 群（購読者へは EventReport として届く）。
pub fn applied_json(device: &str, stimulus: &Stimulus, event_numbers: &[u64]) -> serde_json::Value {
    let applied = match stimulus {
        Stimulus::Press(_) => "press",
        Stimulus::SetState(_) => "state",
    };
    serde_json::json!({"device": device, "applied": applied, "event_numbers": event_numbers})
}

/// stderr へ出す `mat` 形式のエラー行。
pub fn error_json(kind: &str, detail: &str) -> serde_json::Value {
    serde_json::json!({"error": {"kind": kind, "detail": detail}})
}

/// stdin を 1 行ずつ読んで刺激として適用し続ける。EOF（stdin が閉じた）で
/// 戻る — デバイス本体は `Device::run` 側で走り続けるので、フックの終了は
/// プロセスの終了ではない。
pub async fn run_stdin_control(handle: StimulusHandle) {
    use tokio::io::AsyncBufReadExt;
    let mut lines = tokio::io::BufReader::new(tokio::io::stdin()).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let parsed = match parse_control_line(&line) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("{}", error_json("parse_error", &e));
                continue;
            }
        };
        match handle.apply(&parsed.device, parsed.stimulus).await {
            Ok(out) => println!(
                "{}",
                applied_json(&parsed.device, &parsed.stimulus, &out.event_numbers)
            ),
            Err(StimulusApplyError::UnknownDevice(d)) => eprintln!(
                "{}",
                error_json("not_found", &format!("no [[device]] with id {d:?}"))
            ),
            Err(StimulusApplyError::Node(e)) => {
                eprintln!("{}", error_json("other", &e.to_string()))
            }
            Err(StimulusApplyError::Closed) => {
                eprintln!("{}", error_json("other", "device runtime is gone"));
                return;
            }
        }
    }
    tracing::info!("matv: stdin closed, control hook stopped");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_press_and_state_lines() {
        assert_eq!(
            parse_control_line(r#"{"device":"btn","press":"short"}"#).unwrap(),
            ControlLine {
                device: "btn".into(),
                stimulus: Stimulus::Press(PressKind::Short)
            }
        );
        assert_eq!(
            parse_control_line(r#"{"device":"btn","press":"long"}"#)
                .unwrap()
                .stimulus,
            Stimulus::Press(PressKind::Long)
        );
        assert_eq!(
            parse_control_line(r#"{"device":"btn","press":"multi","count":2}"#)
                .unwrap()
                .stimulus,
            Stimulus::Press(PressKind::Multi(2))
        );
        assert_eq!(
            parse_control_line(r#"{"device":"door","state":false}"#)
                .unwrap()
                .stimulus,
            Stimulus::SetState(false)
        );
    }

    #[test]
    fn rejects_malformed_lines_with_a_reason() {
        assert!(parse_control_line("not json").unwrap_err().contains("JSON"));
        assert!(parse_control_line(r#"{"press":"short"}"#)
            .unwrap_err()
            .contains("device"));
        assert!(parse_control_line(r#"{"device":"btn"}"#)
            .unwrap_err()
            .contains("press"));
        assert!(parse_control_line(r#"{"device":"btn","press":"multi"}"#)
            .unwrap_err()
            .contains("count"));
        assert!(parse_control_line(r#"{"device":"btn","press":"triple"}"#)
            .unwrap_err()
            .contains("triple"));
        assert!(
            parse_control_line(r#"{"device":"btn","press":"short","state":true}"#)
                .unwrap_err()
                .contains("both")
        );
    }

    #[test]
    fn json_shapes() {
        assert_eq!(
            applied_json("btn", &Stimulus::Press(PressKind::Short), &[5, 6]),
            serde_json::json!({"device":"btn","applied":"press","event_numbers":[5,6]})
        );
        assert_eq!(
            applied_json("door", &Stimulus::SetState(true), &[]),
            serde_json::json!({"device":"door","applied":"state","event_numbers":[]})
        );
        assert_eq!(
            error_json("not_found", "x"),
            serde_json::json!({"error":{"kind":"not_found","detail":"x"}})
        );
    }
}
