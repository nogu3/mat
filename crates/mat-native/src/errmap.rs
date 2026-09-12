//! mat-controller のエラー（dnssd / session / commissioning / CASE 確立）を
//! mat の `ErrorKind` へ写像する表。経路（mat 直経路 / matd）によらず分類を
//! 揃えるため 1 箇所に置く。

use mat_controller::{case, dnssd};
use mat_core::error::{ErrorKind, MatError};

/// `map_establish_err` の detail 前置き分岐（op / 購読でログ・detail の
/// 文言を従来どおり出し分ける）。
#[derive(Clone, Copy)]
pub(crate) enum EstablishRole {
    Op,
    Subscription,
}

impl EstablishRole {
    /// "op transport bound …" / "subscription transport bound …" のログ用。
    pub(crate) fn log_label(self) -> &'static str {
        match self {
            EstablishRole::Op => "op",
            EstablishRole::Subscription => "subscription",
        }
    }
}

/// `case::establish_any` の失敗を mat のエラー種別へ写す。種別の対応は
/// 逐次ループ時代と同じ: 候補ゼロ = unreachable、CASE 全滅 = session_failed、
/// bind 失敗 = other。detail は全候補のエラーを列挙する（旧実装は最後の
/// 1 本だけだった）。
pub(crate) fn map_establish_err(
    node_id: u64,
    role: EstablishRole,
    e: case::EstablishAnyError,
) -> MatError {
    use case::EstablishAnyError as E;
    let (bind_role, fail_prefix) = match role {
        EstablishRole::Op => ("op", ""),
        EstablishRole::Subscription => ("subscription", "subscription "),
    };
    match &e {
        E::NoAddresses => MatError::new(
            ErrorKind::Unreachable,
            format!("native: no addresses resolved for node {node_id}"),
        ),
        E::Bind(err) => MatError::new(
            ErrorKind::Other,
            format!("native: bind {bind_role} udp: {err}"),
        ),
        E::AllFailed(_) => MatError::new(
            ErrorKind::SessionFailed,
            format!("native: {fail_prefix}{e}"),
        ),
    }
}

/// operational mDNS resolve のエラーを mat の ErrorKind へ写像する。
/// Timeout は「窓内に広告が取れなかっただけ」（OTBR proxy の ~30s 周期広告は
/// リトライで跨げば通ることが多い）→ `timeout`(exit 3)。それ以外
/// （socket I/O 等の構造的失敗）→ `unreachable`(exit 5)。mat 直経路と matd
/// （常駐キャッシュのミス）は同じ establish を通るので分類は経路で割れない。
pub(crate) fn map_resolve_err(node_id: u64, e: dnssd::DnssdError) -> MatError {
    let kind = match e {
        dnssd::DnssdError::Timeout { .. } => ErrorKind::Timeout,
        // 非 timeout は構造的失敗 → unreachable。variant 追加時にここで分類を
        // 決めさせるため wildcard にしない。
        dnssd::DnssdError::Io(_) | dnssd::DnssdError::Malformed(_) => ErrorKind::Unreachable,
    };
    MatError::new(kind, format!("native: mDNS resolve node {node_id}: {e}"))
}

/// SecureSession のエラーを mat の ErrorKind へ写像する（経路によらず分類を揃える）。
pub(crate) fn map_session_err(e: mat_controller::session::SessionError) -> MatError {
    use mat_controller::im::ImError;
    use mat_controller::session::SessionError;
    match e {
        // MRP 再送尽き。session が死んでいる兆候 → 上位が1回だけ再確立を試みる。
        SessionError::Timeout => MatError::new(ErrorKind::Timeout, format!("native: {e}")),
        // 購読の無音 deadline 切れ。通常は SessionConn::next_report_full が
        // Ok(None) に写像するのでここへは来ないが、防御的に Timeout kind へ。
        SessionError::Silence => MatError::new(ErrorKind::Timeout, format!("native: {e}")),
        // デバイスがコマンド/読みを IM ステータスで拒否 → コマンドは届いた。
        // デコード失敗（Tlv/Malformed/UnsupportedValue）は「応答は来たが解釈
        // 不能」= parse_error（Message(_) と同じ規律）。内側 match は wildcard
        // なしの全 variant 列挙 — ImError の variant 追加時にここがコンパイル
        // エラーになり分類を決めさせる（外側の `_` に黙って落とさない）。
        SessionError::Im(ref im) => {
            let kind = match im {
                ImError::StatusResponse(_)
                | ImError::AttributeStatus(_)
                | ImError::CommandStatus { .. } => ErrorKind::DeviceRejected,
                ImError::Tlv(_) | ImError::Malformed(_) | ImError::UnsupportedValue => {
                    ErrorKind::ParseError
                }
            };
            MatError::new(kind, format!("native: {e}"))
        }
        SessionError::Io(_) => MatError::new(ErrorKind::Unreachable, format!("native: {e}")),
        // ピアの応答がメッセージ層で壊れている → 応答は来た（不達ではない）が
        // 解釈不能 = parse_error（v1 品質修正 4）。
        SessionError::Message(_) => MatError::new(ErrorKind::ParseError, format!("native: {e}")),
        _ => MatError::new(ErrorKind::Other, format!("native: {e}")),
    }
}

/// `open_commissioning_window`（既存 CASE セッション上の invoke）のエラーを
/// mat の ErrorKind へ写像する。実質的な失敗経路は `Session`（invoke の
/// SessionError と同分類）と `CommandStatus`（デバイスが拒否）に限られる
/// （PASE/attestation 等は既存 operational セッション上では発生しない）が、
/// 網羅性のため他 variant も `Other` へ落とす。
pub(crate) fn map_commission_err(e: mat_controller::commissioning::CommissionError) -> MatError {
    use mat_controller::commissioning::CommissionError;
    match e {
        CommissionError::Session(se) => map_session_err(se),
        CommissionError::CommandStatus { .. } => {
            MatError::new(ErrorKind::DeviceRejected, format!("native: {e}"))
        }
        CommissionError::Timeout(_) => MatError::new(ErrorKind::Timeout, format!("native: {e}")),
        CommissionError::InvalidArgument { .. } => {
            MatError::new(ErrorKind::ParseError, format!("native: {e}"))
        }
        _ => MatError::new(ErrorKind::Other, format!("native: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_timeout_maps_to_timeout_kind() {
        // resolve timeout は「時間内に広告が取れなかっただけ」（OTBR proxy の
        // ~30s 周期広告はリトライで跨げば通ることが多い）→ timeout(exit 3)。
        // socket I/O 等の構造的失敗は unreachable(exit 5) のまま。
        use mat_controller::dnssd::DnssdError;
        let e = map_resolve_err(
            5,
            DnssdError::Timeout {
                instance: "x".into(),
            },
        );
        assert_eq!(e.kind, ErrorKind::Timeout);
        assert!(e.detail.contains("node 5"), "detail: {}", e.detail);
        let e = map_resolve_err(5, DnssdError::Io(std::io::Error::other("boom")));
        assert_eq!(e.kind, ErrorKind::Unreachable);
        let e = map_resolve_err(5, DnssdError::Malformed("bad"));
        assert_eq!(e.kind, ErrorKind::Unreachable);
    }

    #[test]
    fn map_session_err_maps_malformed_message_to_parse_error() {
        // v1 品質修正 4: ピアの壊れた応答（Message 層のパース失敗）は「応答は来た
        // が解釈不能」= `parse_error`。旧実装は catch-all で `other` に落ちていた。
        let e = map_session_err(mat_controller::session::SessionError::Message(
            mat_controller::message::MessageError::Truncated,
        ));
        assert_eq!(e.kind, ErrorKind::ParseError);
    }

    #[test]
    fn map_session_err_splits_im_decode_failure_from_device_rejection() {
        // 監査⑨: デコード失敗（Tlv/Malformed/UnsupportedValue）は「応答は来たが
        // 解釈不能」= parse_error（Message(_) と同じ規律）。device_rejected は
        // 本当のデバイス拒否（StatusResponse/AttributeStatus/CommandStatus）だけ。
        use mat_controller::im::ImError;
        use mat_controller::session::SessionError;
        let e = map_session_err(SessionError::Im(ImError::Malformed(
            "truncated report data",
        )));
        assert_eq!(e.kind, ErrorKind::ParseError);
        let e = map_session_err(SessionError::Im(ImError::UnsupportedValue));
        assert_eq!(e.kind, ErrorKind::ParseError);
        let e = map_session_err(SessionError::Im(ImError::Tlv(
            mat_controller::tlv::TlvError::InvalidType(0xFF),
        )));
        assert_eq!(e.kind, ErrorKind::ParseError);
        let e = map_session_err(SessionError::Im(ImError::StatusResponse(0x80)));
        assert_eq!(e.kind, ErrorKind::DeviceRejected);
        let e = map_session_err(SessionError::Im(ImError::AttributeStatus(0x86)));
        assert_eq!(e.kind, ErrorKind::DeviceRejected);
        let e = map_session_err(SessionError::Im(ImError::CommandStatus {
            status: 0x01,
            cluster_status: None,
        }));
        assert_eq!(e.kind, ErrorKind::DeviceRejected);
    }

    #[test]
    fn invalid_argument_maps_to_parse_error() {
        let e = map_commission_err(
            mat_controller::commissioning::CommissionError::InvalidArgument {
                what: "iterations must be in 1000..=100000",
            },
        );
        assert_eq!(e.kind, ErrorKind::ParseError);
    }
}
