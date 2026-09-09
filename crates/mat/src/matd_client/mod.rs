//! mat → matd クライアント経路。
//!
//! 経路は 3 状態: `--matd` / `MAT_MATD=truthy` で**強制 matd**（接続失敗はエラー、
//! フォールバック無し）、`MAT_MATD=falsy` で**強制 native 直経路**、どちらも無ければ
//! **自動検出**（既定ソケットへ connect を試み、matd がいればそちら、いなければ
//! native 直経路にフォールバック）。`MAT_MATD_SOCKET` は「どのソケットか」の指定のみで
//! 経路は変えない。
//!
//! matd は unix socket 上で newline-delimited JSON を喋る（1 行 = 1 リクエスト = 1
//! レスポンス）。ここはサブコマンドを matd の op JSON に変換して 1 行送り、返ってきた
//! 1 行（mat スキーマ）を stdout（成功）/ stderr（エラー）へ出すだけの薄い口。
//!
//! mat 本体は同期コードなので接続も std の [`UnixStream`] を使う（tokio は matd 内部
//! の native エンジン用で、上流 ⇔ matd は unix socket）。M8c-3 で chip-tool は撤去済み
//! — この経路も native 直経路も、プロトコルは全て mat-controller / mat-native
//! （in-process）が担う。

use std::ffi::{OsStr, OsString};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

// module doc の [`UnixStream`] link を、実際の接続コードが stream.rs へ移った後も
// 解決させる（rustdoc ビルド限定）。
#[cfg(doc)]
use std::os::unix::net::UnixStream;

use serde_json::{json, Value};

use crate::device_op::DeviceOp;
use mat_core::error::{ErrorKind, MatError};
use mat_core::socket::default_socket_candidates;

mod hint;
mod listen;
mod stream;
mod to_op;

pub(crate) use hint::{hint_node_touched, hint_reload, MatdReload};
pub use listen::dispatch_listen;
use stream::{connect_candidates, emit_response, exchange_on_stream};
use to_op::to_op;

/// matd の構造化エラーを待つ read timeout の余裕。matd は予算ちょうどで構造化
/// timeout を返すので、こちらは予算 + slack まで待って必ず先に受け取る。
/// slack を使い切る（= matd が予算内に応答しない）のは旧 matd か matd 停止。
const CLIENT_SLACK: Duration = Duration::from_secs(2);

/// 予算対象 op（`DeviceOp::budget_applies`）へ deadline_ms を付与し、
/// 適用時の read timeout を返す。非対象は無変更・read timeout なし。
/// 0 = 明示無制限（matd 既定 60s の適用を止める）— read timeout も掛けない。
fn attach_deadline(op: &mut Value, applies: bool, op_timeout_ms: u64) -> Option<Duration> {
    if !applies {
        return None;
    }
    if let Value::Object(map) = op {
        map.insert("deadline_ms".into(), json!(op_timeout_ms));
    }
    (op_timeout_ms > 0).then(|| Duration::from_millis(op_timeout_ms) + CLIENT_SLACK)
}

/// mat の実行経路。`resolve_route` が決める。socket は探索候補リスト
/// （明示指定は 1 本、既定は subdir 新既定 → flat 旧既定の順で connect 試行）。
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    /// 明示有効化（`--matd` / `MAT_MATD=truthy`）: matd 固定。全候補接続失敗は
    /// エラー、非対応 op は exit 2。フォールバックしない。
    Forced(Vec<PathBuf>),
    /// 既定（どちらも未設定）: 候補へ順に connect を試み、成功なら matd、
    /// 全滅なら mat 自身の native 直経路にフォールバック。
    Auto(Vec<PathBuf>),
    /// 明示無効化（`MAT_MATD=falsy`）: 常に native 直経路。probe もしない。
    Direct,
}

/// 経路と socket 候補を決める（純粋関数; env は注入）。
///
/// - `--matd [<path>]` or `MAT_MATD=truthy` → `Forced`
/// - `MAT_MATD=falsy`（`0`/`false`/`no`/`off`） → `Direct`
/// - どちらも無し（truthy/falsy どちらでもない値も同じ） → `Auto`
///
/// socket 候補の優先順: `--matd <path>`（明示、1 本）> `MAT_MATD_SOCKET=<path>`（非空、
/// 1 本）> 既定候補（subdir → flat）。`MAT_MATD_SOCKET` はパス指定のみで経路は変えない。
pub fn resolve_route(
    flag: &Option<Option<PathBuf>>,
    env_socket: Option<OsString>,
    env_enable: Option<OsString>,
) -> Route {
    match flag {
        // --matd <path> → 明示パスで強制 matd（候補 1 本）。
        Some(Some(path)) => Route::Forced(vec![path.clone()]),
        // --matd（値省略）→ 強制 matd。パスは MAT_MATD_SOCKET > 既定候補。
        Some(None) => Route::Forced(sockets_from_env_or_default(env_socket)),
        None => match env_enable.as_deref() {
            Some(v) if is_truthy(v) => Route::Forced(sockets_from_env_or_default(env_socket)),
            Some(v) if is_falsy(v) => Route::Direct,
            // 未設定（or 解釈不能な値）→ 自動検出。
            _ => Route::Auto(sockets_from_env_or_default(env_socket)),
        },
    }
}

/// 有効化済みのときに使う socket 候補: `MAT_MATD_SOCKET`（非空、1 本）> 既定候補。
fn sockets_from_env_or_default(env_socket: Option<OsString>) -> Vec<PathBuf> {
    env_socket
        .filter(|s| !s.is_empty())
        .map(|s| vec![PathBuf::from(s)])
        .unwrap_or_else(default_socket_candidates)
}

/// `MAT_MATD` の真偽判定。`1` / `true` / `yes` / `on`（大小無視）を有効とみなす。
fn is_truthy(v: &OsStr) -> bool {
    matches!(
        v.to_str().map(str::to_ascii_lowercase).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// `MAT_MATD` の否定判定。`0` / `false` / `no` / `off`（大小無視）を無効化とみなす。
/// truthy とも falsy とも解釈できない値は「未設定」と同じ（自動検出）。
fn is_falsy(v: &OsStr) -> bool {
    matches!(
        v.to_str().map(str::to_ascii_lowercase).as_deref(),
        Some("0" | "false" | "no" | "off")
    )
}

/// `--matd` 強制時の非対応 op。kind=other だが exit 2 を返すのは「2 = CLI
/// 引数エラー」の documented シグナルを保つ意図的な例外（spec B 節）。
pub fn unsupported_exit(name: &str) -> ExitCode {
    MatError::new(ErrorKind::Other, unsupported_detail(name)).emit();
    ExitCode::from(2)
}

fn unsupported_detail(name: &str) -> String {
    format!(
        "`mat --matd` does not support the `{name}` subcommand; run it without --matd (direct native path)"
    )
}

/// `--matd` 指定時のディスパッチ。非対応 op は CLI 利用の誤り（exit 2）。
/// alias / color spec 解決は `device_op::classify` が既に済ませているため、
/// ここでの唯一の失敗理由は matd 非対応 op（`to_op` の `Err`）。
pub fn dispatch(sockets: &[PathBuf], op: &DeviceOp, op_timeout_ms: u64) -> ExitCode {
    let mut op_json = match to_op(op) {
        Ok(v) => v,
        // 非対応 op は CLI 利用誤り。kind=other(exit_code()=1) だが exit 2 を
        // 返すのは「2 = CLI 引数エラー」の documented シグナルを保つ意図的な
        // 例外（spec B 節、テストでピン留め）。
        Err(detail) => {
            MatError::new(ErrorKind::Other, &detail).emit();
            return ExitCode::from(2);
        }
    };

    let (stream, socket) = match connect_candidates(sockets) {
        Ok(s) => s,
        Err(detail) => {
            MatError::new(ErrorKind::MatdUnavailable, &detail).emit();
            return ExitCode::from(ErrorKind::MatdUnavailable.exit_code());
        }
    };
    tracing::info!(socket = %socket.display(), "using matd (forced)");

    let read_timeout = attach_deadline(&mut op_json, op.budget_applies(), op_timeout_ms);
    match exchange_on_stream(stream, &op_json, read_timeout) {
        Ok(resp) => emit_response(resp),
        Err(e) => {
            e.emit();
            ExitCode::from(e.kind.exit_code())
        }
    }
}

/// 自動検出モードのディスパッチ。matd 経路で完結した場合のみ `Some(exit code)`。
/// `None` = 呼び出し側が native 直経路で実行すべき（matd 非対応 op / connect 失敗）。
///
/// connect した stream をそのまま本リクエストに使う（probe 後の再接続はしない）ので、
/// フォールバックが起きるのは 1 バイトも送る前だけ。接続後のエラーは matd 経路の
/// エラーとしてそのまま返し、直経路で再実行しない（write / invoke の二重実行防止）。
pub fn dispatch_auto(sockets: &[PathBuf], op: &DeviceOp, op_timeout_ms: u64) -> Option<ExitCode> {
    // matd 非対応 op（open-window / diag thread / grant）は probe せず直経路。
    let mut op_json = match to_op(op) {
        Ok(v) => v,
        Err(_) => return None,
    };

    let (stream, socket) = match connect_candidates(sockets) {
        Ok(s) => s,
        Err(detail) => {
            tracing::info!(
                error = %detail,
                "matd not reachable, falling back to direct native backend"
            );
            return None;
        }
    };
    tracing::info!(socket = %socket.display(), "using matd (auto-detected)");

    let read_timeout = attach_deadline(&mut op_json, op.budget_applies(), op_timeout_ms);
    Some(match exchange_on_stream(stream, &op_json, read_timeout) {
        Ok(resp) => emit_response(resp),
        Err(e) => {
            e.emit();
            ExitCode::from(e.kind.exit_code())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_route_three_states() {
        let some_path = PathBuf::from("/x/y.sock");
        let dflt = mat_core::socket::default_socket_candidates();

        // --matd <path> → 強制 matd（明示パスが MAT_MATD_SOCKET より優先、候補 1 本）。
        assert_eq!(
            resolve_route(
                &Some(Some(some_path.clone())),
                Some("/env.sock".into()),
                None
            ),
            Route::Forced(vec![some_path])
        );
        // --matd（値省略）→ 強制 matd。パスは MAT_MATD_SOCKET（1 本）> 既定候補。
        assert_eq!(
            resolve_route(&Some(None), None, None),
            Route::Forced(dflt.clone())
        );
        assert_eq!(
            resolve_route(&Some(None), Some("/env.sock".into()), None),
            Route::Forced(vec![PathBuf::from("/env.sock")])
        );
        // MAT_MATD=truthy → 強制 matd。
        assert_eq!(
            resolve_route(&None, None, Some("1".into())),
            Route::Forced(dflt.clone())
        );
        // MAT_MATD=falsy → 強制直。socket env が設定されていても probe しない。
        assert_eq!(resolve_route(&None, None, Some("0".into())), Route::Direct);
        assert_eq!(
            resolve_route(&None, Some("/env.sock".into()), Some("off".into())),
            Route::Direct
        );
        // 未設定 → 自動。probe 先は MAT_MATD_SOCKET（非空、1 本）> 既定候補。
        assert_eq!(resolve_route(&None, None, None), Route::Auto(dflt.clone()));
        assert_eq!(
            resolve_route(&None, Some("/env.sock".into()), None),
            Route::Auto(vec![PathBuf::from("/env.sock")])
        );
        // truthy でも falsy でもない値 → 未設定と同じ（自動）。
        assert_eq!(
            resolve_route(&None, None, Some("abc".into())),
            Route::Auto(dflt)
        );
    }

    #[test]
    fn attach_deadline_only_when_budget_applies() {
        let mut op =
            json!({"op":"read","node_id":1,"endpoint":1,"cluster":"onoff","attribute":"on-off"});
        let rt = attach_deadline(&mut op, true, 15_000);
        assert_eq!(op["deadline_ms"], json!(15_000));
        assert_eq!(
            rt,
            Some(std::time::Duration::from_millis(15_000) + CLIENT_SLACK)
        );
        // 0 = 明示無制限: フィールドは付く（matd の既定 60s を止める）が read timeout なし。
        let mut op = json!({"op":"on","node_id":3,"endpoint":1});
        assert_eq!(attach_deadline(&mut op, true, 0), None);
        assert_eq!(op["deadline_ms"], json!(0));
        // 対象外（group 系・bump）: 無変更・read timeout なし。
        let mut op = json!({"op":"group_bump"});
        assert_eq!(attach_deadline(&mut op, false, 15_000), None);
        assert!(op.get("deadline_ms").is_none());
    }
}
