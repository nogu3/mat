//! matd の native バックエンド（Phase 5 M4、M8c-3 で唯一の実行経路に）。
//!
//! mat-controller の warm CASE セッションを matd プロセス内に保持し、read/write/
//! invoke/on/off/色/色温度/describe/group を in-process で処理する。名前解決
//! できない cluster/attribute/command や、native 構築そのものの失敗は
//! server 層が per-op のハードエラーへ変換する（chip-tool フォールバックは
//! M8c-3 で撤去済み）。
//!
//! 確立器・group 送信のコアロジックは `mat-native`（mat one-shot と共有）に
//! 集約されている。ここに残るのは warm session を per-node に保持する責務のみ。

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;

use tokio::sync::Mutex;

use mat_controller::im::{
    self, CLUSTER_COLOR_CONTROL, CLUSTER_ON_OFF, CMD_MOVE_TO_COLOR_TEMPERATURE,
    CMD_MOVE_TO_HUE_AND_SATURATION, CMD_ON_OFF_OFF, CMD_ON_OFF_ON,
};
use mat_core::error::{ErrorKind, MatError};

pub use mat_native::group::{GroupCtx, GroupOutcome};
pub use mat_native::{Establisher, NativeConfig, NodeConn};

#[cfg(test)]
pub(crate) use mat_native::test_support;

/// per-node の warm session slot。`None` = 未確立 or 破棄済み（次回確立）。
/// 外側 `Arc` を短時間の外側ロック下で clone し、往復は内側 `Mutex` で直列化する。
type NodeSlot = Arc<Mutex<Option<Box<dyn NodeConn>>>>;

/// warm CASE セッションを per-node に保持する native バックエンド。
/// エンジン（確立・group 送信）は mat-native と共有し、warm 保持だけが matd の責務。
pub struct NativeBackend {
    engine: mat_native::Engine,
    sessions: Mutex<HashMap<u64, NodeSlot>>,
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

    fn from_engine(engine: mat_native::Engine) -> Self {
        Self {
            engine,
            sessions: Mutex::new(HashMap::new()),
        }
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

    /// この node の per-node slot（`Arc<Mutex<Option<..>>>`）を得る。外側ロックは
    /// slot 取得の間だけ保持して即解放する（ノード間の並行性を保つ）。
    async fn slot(&self, node_id: u64) -> NodeSlot {
        let mut map = self.sessions.lock().await;
        Arc::clone(
            map.entry(node_id)
                .or_insert_with(|| Arc::new(Mutex::new(None))),
        )
    }

    /// warm セッションで `op` を実行する。slot が空なら確立。送信が Timeout
    /// （MRP 尽き=session が死んでいる兆候）なら slot を捨てて1回だけ再確立し再送する。
    /// DeviceRejected / ParseError（コマンドは届き session は健全）は slot 維持で即 Err。
    /// それ以外（Other/Unreachable 等 = session 致命の疑い）は再送せず slot を捨てて
    /// 次コマンドでの遅延再確立に委ねる（死んだ session の持ち越しによる恒久 wedge 防止）。
    async fn with_session<F, T>(&self, node_id: u64, op: F) -> Result<T, MatError>
    where
        F: for<'a> Fn(
            &'a mut Box<dyn NodeConn>,
        ) -> Pin<
            Box<dyn std::future::Future<Output = Result<T, MatError>> + Send + 'a>,
        >,
    {
        let slot = self.slot(node_id).await;
        let mut guard = slot.lock().await;
        if guard.is_none() {
            *guard = Some(self.engine.establisher.establish(node_id).await?);
        }
        let result = op(guard.as_mut().expect("established above")).await;
        match result {
            Ok(v) => Ok(v),
            Err(e) if e.kind == ErrorKind::Timeout => {
                // MRP 再送尽き=未達の可能性大。捨てて1回だけ再確立→再送。
                tracing::info!(
                    node_id,
                    "native session send timed out; re-establishing once"
                );
                *guard = None;
                *guard = Some(self.engine.establisher.establish(node_id).await?);
                op(guard.as_mut().expect("re-established")).await
            }
            // DeviceRejected（IM status 拒否=届いて処理された、session 健全）と
            // ParseError（値デコード問題、session 健全）は slot 維持で即 Err。
            Err(e) if matches!(e.kind, ErrorKind::DeviceRejected | ErrorKind::ParseError) => Err(e),
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
                *guard = None;
                Err(e)
            }
        }
    }

    pub async fn read_onoff(&self, node_id: u64, endpoint: u16) -> Result<bool, MatError> {
        self.with_session(node_id, |c| c.read_onoff(endpoint)).await
    }

    pub async fn on(&self, node_id: u64, endpoint: u16) -> Result<(), MatError> {
        self.with_session(node_id, |c| {
            c.invoke(endpoint, CLUSTER_ON_OFF, CMD_ON_OFF_ON, None, false)
        })
        .await
    }

    pub async fn off(&self, node_id: u64, endpoint: u16) -> Result<(), MatError> {
        self.with_session(node_id, |c| {
            c.invoke(endpoint, CLUSTER_ON_OFF, CMD_ON_OFF_OFF, None, false)
        })
        .await
    }

    pub async fn color(
        &self,
        node_id: u64,
        endpoint: u16,
        hue_raw: u8,
        saturation_raw: u8,
        transition: u16,
    ) -> Result<(), MatError> {
        let fields =
            im::encode_move_to_hue_and_saturation_fields(hue_raw, saturation_raw, transition);
        self.with_session(node_id, move |c| {
            c.invoke(
                endpoint,
                CLUSTER_COLOR_CONTROL,
                CMD_MOVE_TO_HUE_AND_SATURATION,
                Some(fields.clone()),
                false,
            )
        })
        .await
    }

    pub async fn color_temp(
        &self,
        node_id: u64,
        endpoint: u16,
        mireds: u16,
        transition: u16,
    ) -> Result<(), MatError> {
        let fields = im::encode_move_to_color_temperature_fields(mireds, transition);
        self.with_session(node_id, move |c| {
            c.invoke(
                endpoint,
                CLUSTER_COLOR_CONTROL,
                CMD_MOVE_TO_COLOR_TEMPERATURE,
                Some(fields.clone()),
                false,
            )
        })
        .await
    }

    /// 単一属性を任意形状で JSON 読み取る（汎用 read、M8a Task10）。
    pub async fn read_json(
        &self,
        node_id: u64,
        endpoint: u16,
        cluster: u32,
        attribute: u32,
    ) -> Result<serde_json::Value, MatError> {
        self.with_session(node_id, move |c| c.read_json(endpoint, cluster, attribute))
            .await
    }

    /// 単一属性へ 1 個の TLV 要素を書き込む（汎用 write、M8a Task10）。
    pub async fn write_tlv(
        &self,
        node_id: u64,
        endpoint: u16,
        cluster: u32,
        attribute: u32,
        data_tlv: Vec<u8>,
        timed: bool,
    ) -> Result<(), MatError> {
        self.with_session(node_id, move |c| {
            c.write_tlv(endpoint, cluster, attribute, data_tlv.clone(), timed)
        })
        .await
    }

    /// 任意のクラスタコマンドを実行する（汎用 invoke、M8a Task10）。
    pub async fn invoke_generic(
        &self,
        node_id: u64,
        endpoint: u16,
        cluster: u32,
        command: u32,
        fields: Option<Vec<u8>>,
        timed: bool,
    ) -> Result<(), MatError> {
        self.with_session(node_id, move |c| {
            c.invoke(endpoint, cluster, command, fields.clone(), timed)
        })
        .await
    }

    /// ノードを introspect する（`mat describe` 相当、M8a Task10）。
    pub async fn describe(&self, node_id: u64) -> Result<Vec<(u16, Vec<u64>)>, MatError> {
        self.with_session(node_id, |c| Box::pin(mat_native::ops::describe(c.as_mut())))
            .await
    }

    /// group provision のデバイス側 4 ステップを 1 ノードへ実行する
    /// （`mat group provision` のデバイス側相当、M8a Task10）。
    ///
    /// `p` は closure ごとに clone して `async move` ブロックへ渡す ——
    /// `with_session` の `F: for<'a> Fn(...) -> Pin<Box<dyn Future + 'a>>` は
    /// 戻り値の Future が `'a`（= 引数の conn の借用）以外の外部借用を持てない
    /// （closure 環境への参照は self の匿名生存期間に縛られ 'a と無関係なため
    /// コンパイルが通らない）。値を async ブロックへ move すれば Future 自身が
    /// 所有するため 'a のみで閉じる。
    pub async fn provision_node(
        &self,
        node_id: u64,
        p: &mat_native::ops::ProvisionNodeParams,
    ) -> Result<(), MatError> {
        let p = p.clone();
        self.with_session(node_id, move |c| {
            let p = p.clone();
            Box::pin(async move { mat_native::ops::provision_node(c.as_mut(), &p).await })
        })
        .await
    }

    /// group へ groupcast を 1 発送る。native で送れない事情（未 provision・
    /// KVS 不備・counter 初期化不能）は `Unavailable` で返し、送出自体の失敗
    /// （socket）だけを Err にする。
    pub async fn group_invoke(
        &self,
        group_id: u16,
        cluster: u32,
        command: u32,
        fields: Option<Vec<u8>>,
    ) -> Result<GroupOutcome, MatError> {
        let Some(ctx) = &self.engine.group else {
            return Ok(GroupOutcome::Unavailable(
                "native group context not configured".into(),
            ));
        };
        mat_native::group::send(ctx, group_id, cluster, command, fields).await
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn reuses_warm_session_for_same_node() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let est = FakeEstablisher {
            calls: std::sync::Arc::clone(&calls),
            fail_first_send: false,
            fail_kind: ErrorKind::Timeout,
        };
        let backend = NativeBackend::with_establisher(Box::new(est));
        backend.read_onoff(0x1234, 1).await.unwrap();
        backend.read_onoff(0x1234, 1).await.unwrap();
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
        };
        let backend = NativeBackend::with_establisher(Box::new(est));
        // 1 回目の send が Timeout → slot 破棄 → 再確立 → 再送成功。
        let v = backend.read_onoff(0x1234, 1).await.unwrap();
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
        };
        let backend = NativeBackend::with_establisher(Box::new(est));
        // 1 回目の send が DeviceRejected（コマンドは届いている）→ 再確立せず
        // そのままエラーを返す契約 (3)。
        let err = backend
            .read_onoff(0x1234, 1)
            .await
            .expect_err("device rejected must surface as an error");
        assert_eq!(err.kind, ErrorKind::DeviceRejected);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // slot は破棄されず維持される: 同ノードへの 2 回目のコマンドは warm 再利用で
        // 成功し、establish は 1 のまま（session 健全なので捨てない）。
        let v = backend
            .read_onoff(0x1234, 1)
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
        };
        let backend = NativeBackend::with_establisher(Box::new(est));
        // 1 回目の send が session 致命エラー（Other=復号失敗/counter desync 等）。
        // (a) エラー kind は Other、(b) 再送しない → establish は 1 回のみ。
        let err = backend
            .read_onoff(0x1234, 1)
            .await
            .expect_err("session-fatal error must surface");
        assert_eq!(err.kind, ErrorKind::Other);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        // (c) 死んだ session は破棄済み → 2 回目のコマンドで再確立して成功。
        let v = backend
            .read_onoff(0x1234, 1)
            .await
            .expect("session must be lazily re-established after fatal error");
        assert!(v);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn group_invoke_without_ctx_is_unavailable() {
        let b = NativeBackend::with_establisher(Box::new(FakeEstablisher::default()));
        let r = b
            .group_invoke(10, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON, None)
            .await
            .unwrap();
        assert!(matches!(r, GroupOutcome::Unavailable(_)));
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
}
