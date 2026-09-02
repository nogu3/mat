//! matd の native バックエンド（Phase 5 M4、M8c-3 で唯一の実行経路に）。
//!
//! op の実体は `mat_native::op` / `runner`（mat one-shot と共有）。確立器・
//! group 送信のコアロジックも `mat-native` に集約されている。ここに残るのは
//! warm session を per-node に保持する責務（`NodeRunner` 実装）のみ。

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::Mutex;

use mat_core::error::{ErrorKind, MatError};
use mat_native::runner::NodeRunner;

pub use mat_native::group::{GroupCtx, GroupOutcome};
pub use mat_native::{Establisher, NativeConfig, NodeConn, Resolver};

#[cfg(test)]
pub(crate) use mat_native::test_support;

/// per-node の warm session slot。`None` = 未確立 or 破棄済み（次回確立）。
/// 外側 `Arc` を短時間の外側ロック下で clone し、往復は内側 `Mutex` で直列化する。
type NodeSlot = Arc<Mutex<Option<Box<dyn NodeConn>>>>;

/// Timeout 腕の再確立+再送に最低限必要な残り予算。warm-cache の mDNS 解決 +
/// CASE 往復（典型 ~1s）+ MRP 一巡（`mat_controller::exchange::total_budget`
/// 既定 ≈ 5.93s、`worst_case_send_budget` ≈ 15.93s の内数）+ 応答余裕。
/// これ未満なら再送が成功しても呼び出し側の予算内に応答を返せない
/// （Issue #16: 「1 回だけ再確立して再送」が構造的に無駄だった側）。
pub(crate) const RETRY_MIN_BUDGET: Duration = Duration::from_secs(10);

/// deadline までの残り。None = 無制限。既に過ぎていれば ZERO。
fn remaining(deadline: Option<Instant>) -> Option<Duration> {
    deadline.map(|d| d.saturating_duration_since(Instant::now()))
}

/// future を残り予算で包む。予算超過はフェーズと経過 ms 入りの Timeout。
async fn bounded<T>(
    deadline: Option<Instant>,
    started: Instant,
    phase: &str,
    fut: impl std::future::Future<Output = Result<T, MatError>>,
) -> Result<T, MatError> {
    match remaining(deadline) {
        None => fut.await,
        Some(rem) => match tokio::time::timeout(rem, fut).await {
            Ok(r) => r,
            Err(_) => Err(MatError::new(
                ErrorKind::Timeout,
                format!(
                    "op deadline exceeded after {}ms in {phase}",
                    started.elapsed().as_millis()
                ),
            )),
        },
    }
}

/// slot の session を手放す前に best-effort CloseSession（Issue #20: 放置
/// セッションは FP300 系の常駐購読を黙殺する）。Timeout 腕（相手死亡疑い）でも
/// 送る — 待ちゼロなのでコスト無し。
async fn close_and_clear(guard: &mut Option<Box<dyn NodeConn>>) {
    if let Some(conn) = guard.as_mut() {
        conn.close().await;
    }
    *guard = None;
}

/// warm CASE セッションを per-node に保持する native バックエンド。
/// エンジン（確立・group 送信）は mat-native と共有し、warm 保持だけが matd の責務。
pub struct NativeBackend {
    engine: mat_native::Engine,
    sessions: Mutex<HashMap<u64, NodeSlot>>,
    /// 新規 CASE セッション確立を外側へ知らせる汎用フック（Issue #20 経路2）。
    /// native.rs は subscription.rs を知らないため node_id だけを渡す薄い
    /// コールバックにする — 呼び出し元（main.rs）が `SubHealth::note_touched`
    /// を注入する。`OnceLock` は `&self` で set できる（プロセス起動時に
    /// `Arc<NativeBackend>` 経由で 1 回だけ注入する用途に合う）。
    on_new_session: std::sync::OnceLock<Box<dyn Fn(u64) + Send + Sync>>,
}

/// 手動 `Debug`: `Engine` / warm セッションは `Debug` を持たず、
/// また表示すべき秘密（鍵）を内包し得るため中身は出さない。`Result::expect_err`
/// が `NativeBackend: Debug` を要求する（build のテスト）ためだけに提供する。
impl std::fmt::Debug for NativeBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NativeBackend").finish_non_exhaustive()
    }
}

impl NativeBackend {
    /// KVS から資格情報を1回読み、NOC を自己発行し、UDP transport を bind、
    /// iface の scope_id を解決して実確立器を構築する。プロセス寿命で不変。
    pub async fn build(cfg: &NativeConfig) -> Result<Self, MatError> {
        Ok(Self::from_engine(mat_native::Engine::build(cfg).await?))
    }

    /// [`build`] と同じだが Resolver を注入する（matd が CachingResolver を渡す）。
    pub async fn build_with_resolver(
        cfg: &NativeConfig,
        resolver: std::sync::Arc<dyn mat_native::Resolver>,
    ) -> Result<Self, MatError> {
        Ok(Self::from_engine(
            mat_native::Engine::build_with_resolver(cfg, resolver).await?,
        ))
    }

    fn from_engine(engine: mat_native::Engine) -> Self {
        Self {
            engine,
            sessions: Mutex::new(HashMap::new()),
            on_new_session: std::sync::OnceLock::new(),
        }
    }

    /// 新規 CASE セッション確立（cold establish / resend-establish）のたびに
    /// `cb(node_id)` を呼ぶよう登録する。プロセス寿命で 1 回だけ呼ぶ想定
    /// （2 回目以降の呼び出しは無視 — `OnceLock` の性質）。
    pub fn set_on_new_session(&self, cb: Box<dyn Fn(u64) + Send + Sync>) {
        let _ = self.on_new_session.set(cb);
    }

    /// テスト用: 任意の Establisher を注入する（group 送信は無効）。`pub`（cfg(test)
    /// 非gate）なのは `tests/integration.rs`（外部テストクレート = 別コンパイル
    /// 単位で `#[cfg(test)]` 項目は見えない）から fake establisher で matd の socket
    /// 経路を end-to-end 検証するため — `mat_native::Engine::with_parts` 自体も
    /// 元から同じ理由で常時 pub。
    pub fn with_establisher(establisher: Box<dyn Establisher>) -> Self {
        Self::from_engine(mat_native::Engine::with_parts(establisher, None))
    }

    /// テスト用: Establisher と group 送信コンテキストの両方を注入する（`pub` の
    /// 理由は [`with_establisher`](Self::with_establisher) と同じ）。
    pub fn with_parts(establisher: Box<dyn Establisher>, group: Option<GroupCtx>) -> Self {
        Self::from_engine(mat_native::Engine::with_parts(establisher, group))
    }

    /// テスト用: with_parts + group_settings 注入（`pub` の理由は
    /// [`with_establisher`](Self::with_establisher) と同じ）。
    pub fn with_parts_gs(
        establisher: Box<dyn Establisher>,
        group: Option<GroupCtx>,
        gs: Option<mat_native::group_settings::GroupSettingsCtx>,
    ) -> Self {
        let mut engine = mat_native::Engine::with_parts(establisher, group);
        engine.group_settings = gs;
        Self::from_engine(engine)
    }

    /// controller 側 group state の KVS 書込資材（M8c-2）。None = native 構築が
    /// 未完（テスト注入等; 本番 `Engine::build` では常に `Some`）— 呼び出し側
    /// （`server::group_provision`）は internal エラーとして拒否する（M8c-3）。
    pub fn group_settings_ctx(&self) -> Option<&mat_native::group_settings::GroupSettingsCtx> {
        self.engine.group_settings.as_ref()
    }

    /// group 送信 / group_settings / 確立器を持つ共有エンジン（`mat_native::op`
    /// の group 系関数が受ける）。
    pub fn engine(&self) -> &mat_native::Engine {
        &self.engine
    }

    /// この node の per-node slot（`Arc<Mutex<Option<..>>>`）を得る。外側ロックは
    /// slot 取得の間だけ保持して即解放する（ノード間の並行性を保つ）。
    async fn slot(&self, node_id: u64) -> NodeSlot {
        let mut map = self.sessions.lock().await;
        Arc::clone(
            map.entry(node_id)
                .or_insert_with(|| Arc::new(Mutex::new(None))),
        )
    }

    /// クライアント切断で進行中 op が破棄された後の始末。中途 exchange の
    /// session を次 op に持ち越さないよう slot を破棄する（次回 lazy 再確立）。
    /// try_lock なのは、取れない = 別接続の op が session を使用中 = 破棄された
    /// op は Mutex 待ちのまま session に触れていなかったケースだから: 健全な
    /// warm session を巻き添えにせず、実行中 op の完了待ちで滞留もしない。
    pub async fn drop_session(&self, node_id: u64) {
        let slot = self.slot(node_id).await;
        let attempt = slot.try_lock();
        if let Ok(mut guard) = attempt {
            close_and_clear(&mut guard).await;
        }
    }

    /// warm セッションで `op` を実行する。slot が空なら確立。送信が Timeout
    /// （MRP 尽き=session が死んでいる兆候）なら slot を捨てて1回だけ再確立し再送する。
    /// DeviceRejected / ParseError（コマンドは届き session は健全）は slot 維持で即 Err。
    /// それ以外（Other/Unreachable 等 = session 致命の疑い）は再送せず slot を捨てて
    /// 次コマンドでの遅延再確立に委ねる（死んだ session の持ち越しによる恒久 wedge 防止）。
    ///
    /// `deadline`（Issue #16）: `Some` なら establish/send の各フェーズを残り
    /// 予算で打ち切る（超過は `ErrorKind::Timeout` + フェーズ名 + 経過 ms 入りの
    /// detail）。フェーズ超過は MRP 尽きと同様に slot を破棄する。ただし
    /// 再確立+再送は残り予算が [`RETRY_MIN_BUDGET`] 以上のときだけ行う —
    /// 満たない場合は Timeout をそのまま返し、再確立は撃たない（無駄な
    /// リトライで応答が確実に遅延超過するのを防ぐ）。`None` = 無制限（従来どおり）。
    async fn with_session<F, T>(
        &self,
        node_id: u64,
        deadline: Option<Instant>,
        op: F,
    ) -> Result<T, MatError>
    where
        F: for<'a> Fn(
            &'a mut Box<dyn NodeConn>,
        ) -> Pin<
            Box<dyn std::future::Future<Output = Result<T, MatError>> + Send + 'a>,
        >,
    {
        let (result, established_new) = self.with_session_inner(node_id, deadline, op).await;
        // セッションを新設した = デバイスの「最新セッション」が op 用に変わった。
        // FP300 はここへ購読レポートを付け替えるので、購読側へ即時張り直しを
        // 合図する（Issue #20 経路2 — 2026-07-30 17:09 hold-time write 事故の
        // 再発防止）。op の成否確定後・関数を抜ける直前に高々1回だけ発火する。
        //
        // 既知の限界: この warm op セッションは張り直した購読セッションより
        // 新しいまま残り続けるため、直後にさらにレポートがこちらへ向く余地は
        // 残る。それでも matd の warm ソケットは MRP ack を返す生きたソケット
        // なので黒穴にはならず、次の張り直しで購読側が最新化される
        // （セッション統合は spec の非目標 — ここではしない）。
        if established_new {
            if let Some(cb) = self.on_new_session.get() {
                cb(node_id);
            }
        }
        result
    }

    /// [`with_session`] の本体。戻り値の第2要素は、この呼び出し中に新規
    /// CASE セッションを確立したか（cold establish / resend-establish の
    /// いずれか）— 呼び出し元がコールバック発火を1箇所に集約するための
    /// シグナル。
    async fn with_session_inner<F, T>(
        &self,
        node_id: u64,
        deadline: Option<Instant>,
        op: F,
    ) -> (Result<T, MatError>, bool)
    where
        F: for<'a> Fn(
            &'a mut Box<dyn NodeConn>,
        ) -> Pin<
            Box<dyn std::future::Future<Output = Result<T, MatError>> + Send + 'a>,
        >,
    {
        let mut established_new = false;
        let started = Instant::now();
        let slot = self.slot(node_id).await;
        let mut guard = slot.lock().await;
        if guard.is_none() {
            // cold: warm セッションが無いので CASE から張る。同じノードで
            // これが繰り返し出るなら session churn（op ログの elapsed_ms が
            // 伸びる原因）。再確立側は下の Timeout / その他エラー腕で既に
            // info を出しているので、これで確立の両側が揃う。
            tracing::info!(node_id, "no warm session; establishing");
            match bounded(
                deadline,
                started,
                "establish",
                self.engine.establisher.establish(node_id),
            )
            .await
            {
                Ok(conn) => {
                    *guard = Some(conn);
                    established_new = true;
                }
                Err(e) => return (Err(e), established_new),
            }
        }
        let result = bounded(
            deadline,
            started,
            "send",
            op(guard.as_mut().expect("established above")),
        )
        .await;
        match result {
            Ok(v) => (Ok(v), established_new),
            Err(e) if e.kind == ErrorKind::Timeout => {
                // MRP 再送尽き or deadline 超過=未達の可能性大。どちらも session
                // は持ち越さない。
                close_and_clear(&mut guard).await;
                // 残り予算が RETRY_MIN_BUDGET 未満なら再確立を撃たない（Issue #16:
                // 呼び出し側の予算内に応答できない再送は最初から無駄）。
                if let Some(rem) = remaining(deadline) {
                    if rem < RETRY_MIN_BUDGET {
                        tracing::info!(
                            node_id,
                            remaining_ms = u64::try_from(rem.as_millis()).unwrap_or(u64::MAX),
                            "skipping re-establish; insufficient budget"
                        );
                        return (Err(e), established_new);
                    }
                }
                tracing::info!(
                    node_id,
                    "native session send timed out; re-establishing once"
                );
                match bounded(
                    deadline,
                    started,
                    "resend-establish",
                    self.engine.establisher.establish(node_id),
                )
                .await
                {
                    Ok(conn) => {
                        *guard = Some(conn);
                        established_new = true;
                    }
                    Err(e2) => return (Err(e2), established_new),
                }
                let retried = bounded(
                    deadline,
                    started,
                    "resend",
                    op(guard.as_mut().expect("re-established")),
                )
                .await;
                if let Err(e2) = &retried {
                    // 再送側も slot 衛生を揃える: session 健全なエラー
                    // （DeviceRejected/ParseError）以外は持ち越さない。
                    if !matches!(e2.kind, ErrorKind::DeviceRejected | ErrorKind::ParseError) {
                        close_and_clear(&mut guard).await;
                    }
                }
                (retried, established_new)
            }
            // DeviceRejected（IM status 拒否=届いて処理された、session 健全）と
            // ParseError（値デコード問題、session 健全）は slot 維持で即 Err。
            Err(e) if matches!(e.kind, ErrorKind::DeviceRejected | ErrorKind::ParseError) => {
                (Err(e), established_new)
            }
            // それ以外（Other/Unreachable 等 = 復号失敗・カウンタ desync・不正フレーム
            // 等で session が死んだ疑い）。応答が受かった可能性があるので再送はしないが、
            // 死んだ session を持ち続けると恒久 wedge になる。slot を捨てて次コマンドで
            // 自然に再確立させる。
            Err(e) => {
                tracing::info!(
                    node_id,
                    kind = ?e.kind,
                    "native session error; dropping session for lazy re-establish"
                );
                close_and_clear(&mut guard).await;
                (Err(e), established_new)
            }
        }
    }

    /// 購読専用コネクション（専用ソケット + 専用 CASE）を確立する。warm session
    /// slot（`with_session`）とは独立 — 購読ポンプが独占する。
    pub async fn establish_subscription(
        &self,
        node_id: u64,
    ) -> Result<Box<dyn mat_native::SubscribeConn>, MatError> {
        self.engine
            .establisher
            .establish_subscription(node_id)
            .await
    }
}

/// warm セッション戦略の差し替え点: `with_session`（per-node slot、Timeout で
/// 1 回だけ再確立、deadline 予算、`on_new_session` 発火）をそのまま使う。
#[async_trait]
impl NodeRunner for NativeBackend {
    async fn with_node<T, F>(
        &self,
        node_id: u64,
        deadline: Option<Instant>,
        f: F,
    ) -> Result<T, MatError>
    where
        T: Send,
        F: for<'a> Fn(
                &'a mut Box<dyn NodeConn>,
            ) -> Pin<
                Box<dyn std::future::Future<Output = Result<T, MatError>> + Send + 'a>,
            > + Send
            + Sync,
    {
        self.with_session(node_id, deadline, f).await
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    /// 旧 `NativeBackend::read_onoff` 相当（with_session の挙動テスト用）。
    async fn read_onoff(
        b: &NativeBackend,
        node_id: u64,
        endpoint: u16,
        deadline: Option<Instant>,
    ) -> Result<bool, MatError> {
        b.with_node(node_id, deadline, move |c| c.read_onoff(endpoint))
            .await
    }

    #[tokio::test]
    async fn deadline_cuts_send_and_drops_slot() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let est = FakeEstablisher {
            calls: std::sync::Arc::clone(&calls),
            conn_delay: Some(Duration::from_millis(200)),
            ..Default::default()
        };
        let backend = NativeBackend::with_establisher(Box::new(est));
        let deadline = Some(Instant::now() + Duration::from_millis(50));
        let err = read_onoff(&backend, 0x1234, 1, deadline)
            .await
            .expect_err("deadline must cut the slow send");
        assert_eq!(err.kind, ErrorKind::Timeout);
        assert!(err.detail.contains("in send"), "detail: {}", err.detail);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // slot は破棄済み: 無制限の次 op は再確立してから成功する。
        read_onoff(&backend, 0x1234, 1, None).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn deadline_cuts_establish_phase() {
        let est = FakeEstablisher {
            establish_delay: Some(Duration::from_millis(200)),
            ..Default::default()
        };
        let backend = NativeBackend::with_establisher(Box::new(est));
        let deadline = Some(Instant::now() + Duration::from_millis(50));
        let err = read_onoff(&backend, 0x1234, 1, deadline)
            .await
            .expect_err("deadline must cut the slow establish");
        assert_eq!(err.kind, ErrorKind::Timeout);
        assert!(
            err.detail.contains("in establish"),
            "detail: {}",
            err.detail
        );
    }

    #[tokio::test]
    async fn insufficient_budget_skips_re_establish() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let est = FakeEstablisher {
            calls: std::sync::Arc::clone(&calls),
            fail_first_send: true,
            fail_kind: ErrorKind::Timeout,
            ..Default::default()
        };
        let backend = NativeBackend::with_establisher(Box::new(est));
        // fake の失敗は即時なので、残り予算 ≈ 5s < RETRY_MIN_BUDGET(10s)。
        let deadline = Some(Instant::now() + Duration::from_secs(5));
        let err = read_onoff(&backend, 0x1234, 1, deadline)
            .await
            .expect_err("timeout must surface when retry is skipped");
        assert_eq!(err.kind, ErrorKind::Timeout);
        // 再確立していない: establish は初回の 1 回だけ。
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // slot は破棄済み（MRP 尽きの session は持ち越さない）。
        read_onoff(&backend, 0x1234, 1, None).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn sufficient_budget_still_re_establishes() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let est = FakeEstablisher {
            calls: std::sync::Arc::clone(&calls),
            fail_first_send: true,
            fail_kind: ErrorKind::Timeout,
            ..Default::default()
        };
        let backend = NativeBackend::with_establisher(Box::new(est));
        // 残り予算 ≈ 60s ≥ RETRY_MIN_BUDGET → 従来どおり再確立+再送。
        let deadline = Some(Instant::now() + Duration::from_secs(60));
        let v = read_onoff(&backend, 0x1234, 1, deadline).await.unwrap();
        assert!(v);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn reuses_warm_session_for_same_node() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let est = FakeEstablisher {
            calls: std::sync::Arc::clone(&calls),
            fail_first_send: false,
            fail_kind: ErrorKind::Timeout,
            ..Default::default()
        };
        let backend = NativeBackend::with_establisher(Box::new(est));
        read_onoff(&backend, 0x1234, 1, None).await.unwrap();
        read_onoff(&backend, 0x1234, 1, None).await.unwrap();
        // 2 回のコマンドで establish は 1 回だけ（warm 再利用）。
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn re_establishes_once_on_send_timeout() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let est = FakeEstablisher {
            calls: std::sync::Arc::clone(&calls),
            fail_first_send: true,
            fail_kind: ErrorKind::Timeout,
            ..Default::default()
        };
        let backend = NativeBackend::with_establisher(Box::new(est));
        // 1 回目の send が Timeout → slot 破棄 → 再確立 → 再送成功。
        let v = read_onoff(&backend, 0x1234, 1, None).await.unwrap();
        assert!(v);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn does_not_re_establish_on_device_rejected() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let est = FakeEstablisher {
            calls: std::sync::Arc::clone(&calls),
            fail_first_send: true,
            fail_kind: ErrorKind::DeviceRejected,
            ..Default::default()
        };
        let backend = NativeBackend::with_establisher(Box::new(est));
        // 1 回目の send が DeviceRejected（コマンドは届いている）→ 再確立せず
        // そのままエラーを返す契約 (3)。
        let err = read_onoff(&backend, 0x1234, 1, None)
            .await
            .expect_err("device rejected must surface as an error");
        assert_eq!(err.kind, ErrorKind::DeviceRejected);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // slot は破棄されず維持される: 同ノードへの 2 回目のコマンドは warm 再利用で
        // 成功し、establish は 1 のまま（session 健全なので捨てない）。
        let v = read_onoff(&backend, 0x1234, 1, None)
            .await
            .expect("warm session must be reused after device_rejected");
        assert!(v);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn drops_session_on_session_fatal_error_without_retry() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let est = FakeEstablisher {
            calls: std::sync::Arc::clone(&calls),
            fail_first_send: true,
            fail_kind: ErrorKind::Other,
            ..Default::default()
        };
        let backend = NativeBackend::with_establisher(Box::new(est));
        // 1 回目の send が session 致命エラー（Other=復号失敗/counter desync 等）。
        // (a) エラー kind は Other、(b) 再送しない → establish は 1 回のみ。
        let err = read_onoff(&backend, 0x1234, 1, None)
            .await
            .expect_err("session-fatal error must surface");
        assert_eq!(err.kind, ErrorKind::Other);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // (c) 死んだ session は破棄済み → 2 回目のコマンドで再確立して成功。
        let v = read_onoff(&backend, 0x1234, 1, None)
            .await
            .expect("session must be lazily re-established after fatal error");
        assert!(v);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn group_settings_ctx_reflects_injected_value() {
        let gs = mat_native::group_settings::GroupSettingsCtx {
            main_ini: std::path::PathBuf::from("/tmp/does-not-exist.ini"),
            fabric_index: 2,
            cfid: [7u8; 8],
        };
        let backend =
            NativeBackend::with_parts_gs(Box::new(FakeEstablisher::default()), None, Some(gs));
        let ctx = backend
            .group_settings_ctx()
            .expect("injected group_settings must be reflected");
        assert_eq!(ctx.fabric_index, 2);
        assert_eq!(ctx.cfid, [7u8; 8]);
    }

    #[test]
    fn group_settings_ctx_is_none_without_injection() {
        let backend = NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
        assert!(backend.group_settings_ctx().is_none());
    }

    #[tokio::test]
    async fn drop_session_skips_when_another_op_holds_the_lock() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let est = FakeEstablisher {
            calls: std::sync::Arc::clone(&calls),
            ..Default::default()
        };
        let backend = std::sync::Arc::new(NativeBackend::with_establisher(Box::new(est)));
        // warm session を作る（establish 1 回目）。
        read_onoff(&backend, 0x1234, 1, None).await.unwrap();
        // 別 op が実行中のように slot を握った状態で drop_session を呼ぶ。
        let slot = backend.slot(0x1234).await;
        let guard = slot.lock().await;
        backend.drop_session(0x1234).await; // 待たず・破棄せず即返る
        drop(guard);
        // session は生きている: 次 op は warm 再利用で establish は 1 回のまま。
        read_onoff(&backend, 0x1234, 1, None).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    /// Timeout で slot を捨てる前に close が呼ばれる（Issue #20: 放置セッションを
    /// FP300 系が常駐購読の再アンカー先にしてしまう）。
    #[tokio::test]
    async fn with_session_closes_before_dropping_on_timeout() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let close_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let est = FakeEstablisher {
            calls: std::sync::Arc::clone(&calls),
            fail_first_send: true,
            fail_kind: ErrorKind::Timeout,
            conn_close_calls: std::sync::Arc::clone(&close_calls),
            ..Default::default()
        };
        let backend = NativeBackend::with_establisher(Box::new(est));
        // 1 回目の send が Timeout → 旧 conn を close してから捨て、再確立して再送成功。
        let v = read_onoff(&backend, 0x1234, 1, None).await.unwrap();
        assert!(v);
        assert_eq!(close_calls.load(Ordering::SeqCst), 1);
    }

    /// drop_session でも捨てる前に close が呼ばれる（Issue #20）。
    #[tokio::test]
    async fn drop_session_closes_warm_conn() {
        let close_calls = std::sync::Arc::new(AtomicUsize::new(0));
        let est = FakeEstablisher {
            conn_close_calls: std::sync::Arc::clone(&close_calls),
            ..Default::default()
        };
        let backend = NativeBackend::with_establisher(Box::new(est));
        read_onoff(&backend, 0x1234, 1, None).await.unwrap();
        backend.drop_session(0x1234).await;
        assert_eq!(close_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn drop_session_clears_when_uncontended() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let est = FakeEstablisher {
            calls: std::sync::Arc::clone(&calls),
            ..Default::default()
        };
        let backend = NativeBackend::with_establisher(Box::new(est));
        read_onoff(&backend, 0x1234, 1, None).await.unwrap();
        backend.drop_session(0x1234).await;
        // slot は破棄済み: 次 op は再確立する。
        read_onoff(&backend, 0x1234, 1, None).await.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// cold establish で op を完了するとコールバックが1回発火する
    /// （Issue #20 経路2: 内部トリガ）。
    #[tokio::test]
    async fn cold_establish_fires_on_new_session() {
        let backend = NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
        let fired = std::sync::Arc::new(AtomicUsize::new(0));
        let fired2 = std::sync::Arc::clone(&fired);
        backend.set_on_new_session(Box::new(move |_node_id| {
            fired2.fetch_add(1, Ordering::SeqCst);
        }));
        read_onoff(&backend, 0x1234, 1, None).await.unwrap();
        assert_eq!(fired.load(Ordering::SeqCst), 1);
    }

    /// warm 再利用（同一 node への 2 回目の op）ではコールバックは発火しない
    /// — 発火は新設時のみ。
    #[tokio::test]
    async fn warm_reuse_does_not_fire() {
        let backend = NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
        let fired = std::sync::Arc::new(AtomicUsize::new(0));
        let fired2 = std::sync::Arc::clone(&fired);
        backend.set_on_new_session(Box::new(move |_node_id| {
            fired2.fetch_add(1, Ordering::SeqCst);
        }));
        read_onoff(&backend, 0x1234, 1, None).await.unwrap();
        read_onoff(&backend, 0x1234, 1, None).await.unwrap();
        // cold 1回 → 2回目の op（warm 再利用）→ 計1回のまま。
        assert_eq!(fired.load(Ordering::SeqCst), 1);
    }

    /// send Timeout 後の resend-establish（1 op 内での cold + 再確立）でも
    /// コールバックは発火する（op 完了後・1回のみ）。
    #[tokio::test]
    async fn resend_establish_fires_on_new_session() {
        let est = FakeEstablisher {
            fail_first_send: true,
            fail_kind: ErrorKind::Timeout,
            ..Default::default()
        };
        let backend = NativeBackend::with_establisher(Box::new(est));
        let fired = std::sync::Arc::new(AtomicUsize::new(0));
        let fired2 = std::sync::Arc::clone(&fired);
        backend.set_on_new_session(Box::new(move |_node_id| {
            fired2.fetch_add(1, Ordering::SeqCst);
        }));
        read_onoff(&backend, 0x1234, 1, None).await.unwrap();
        // 同一 with_session 呼び出し内で cold + resend-establish の 2 回
        // 新設が起きても発火は 1 回のみ（op 完了後に一度だけ判定するため）。
        assert_eq!(fired.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn build_with_resolver_accepts_caching_resolver() {
        // 実 KVS が無いので build 自体は Err になるが、CachingResolver を
        // Arc<dyn Resolver> として渡せる（型・API の結線）ことを確認する。
        let (cache, _rx) = mat_controller::dnssd::OperationalCache::new();
        let resolver: std::sync::Arc<dyn mat_native::Resolver> =
            std::sync::Arc::new(mat_native::CachingResolver::new(cache));
        let cfg = NativeConfig {
            store: std::path::PathBuf::from("/nonexistent"),
            iface: "lo".into(),
            thread_iface: None,
            fabric_index: 1,
            issuer_index: 0,
        };
        let r = NativeBackend::build_with_resolver(&cfg, resolver).await;
        assert!(r.is_err()); // KVS 不在で store_missing。型結線が通ることが要点。
    }
}
