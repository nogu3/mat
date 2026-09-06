# matd reload — IPK のホットリロード設計（2026-09-06）

## 1. 目的と背景

`mat fabric rotate-ipk` が commit した後、稼働中の `matd` はメモリの
`FabricCredentials`（旧 epoch 由来の `ipk_operational`）で新規 CASE を張り続ける。
デバイスは {E_prev, E_cur} の 2 epoch を受理するので 1 世代は動くが、**次回
ローテーション前に必ず restart** が必要（2026-09-05 IPK rotation 設計 §6）。
restart は常駐購読（本番 19 本）の張り直しを伴い約 2 分の盲目期間と mando の
prime 競合 WARN を生む。本設計は restart を `matd reload`（socket admin op）へ
置き換える。

### 1.1 調査で確定した前提（スコープを決める事実）

- **matd が起動時に読んで抱え込むのは fabric 資格情報（IPK を含む）だけ。**
  group 送信は `mat-native::group::send` が**毎回** `kvs::read_group_credentials`
  で keyset / keymap を読み直す（docs/commands.md「Pick one group sender」節に
  既に明記）。`GroupSender` が保持するのは counter と egress だけで鍵は持たない。
- したがって `mat group provision` / `mat group remove` の直経路 note
  「if matd is running, restart it to reload group state」は**現状で既に誤り**
  （restart 不要）。本設計では reload の対象を IPK に限定し、この note を削除する。
- `Establisher` は `Engine.establisher: Box<dyn Establisher>` として
  `Arc<NativeState>` 越しに共有され、`CaseEstablisher.creds: Arc<FabricCredentials>`
  が構築時に固定される。差し替え点はここ 1 箇所。

## 2. スコープ

含む:

1. matd socket admin op `{"op":"reload"}` と CLI `matd reload`。
2. `mat-native::Engine::reload_credentials` — KVS から資格情報を読み直し、
   `CaseEstablisher` の `Arc<FabricCredentials>` を原子的に差し替える。
3. `mat fabric rotate-ipk` が commit（`status: "rotated"`）後に matd へ reload を
   撃ち、ack を読んで body の `matd_reload` に載せる。
4. `matd status` に `reloads` を追加。
5. note / docs の「restart matd」文言の更新（provision / remove は削除、
   rotate は reload 案内へ）。
6. e2e-device-m3 に reload ステップ（unchanged 経路 + rotate 経由の changed 経路）。

含まない（YAGNI / 別設計）:

- group 状態の再読込（不要、§1.1）。
- `NativeState::Unavailable`（起動時 KVS 不在）からの reload による復帰。
  reload は `Ready` のときだけ意味を持ち、`Unavailable` は起動時エラーをそのまま
  返す（restart が唯一の復帰手段のまま）。
- fabric identity（fabric_id / node_id / root 公開鍵）が変わる store 差し替えの
  受理。これは reload で**拒否**し restart を案内する（§4.2）。
- `provision` / `group remove` からの reload 送信（不要）。
- 購読の張り直し。既存 warm session / 購読 session は触らない（session 鍵は
  確立時に導出済み、次回確立から新 IPK）。

## 3. ワイヤ / CLI

### 3.1 `Op::Reload`（matd protocol）

- `crates/matd/src/protocol.rs` の `Op` に単位 variant `Reload` を追加。
  `Ping` / `Status` / `Shutdown` / `NodeTouched` と同じ admin op 群:
  `node_id()` は `None`、`require_node` 対象外、`to_device_op` は admin 群の腕に
  合流（`parse_error` にならない）、`log_op` の op 名は `"reload"`。
- `server::dispatch` で `Status` / `NodeTouched` と同じく短絡する（`run_op` を
  通さない）。ただし native には触る（`NativeState::Ready(backend)` の
  `backend.engine().reload_credentials(cfg)`）。per-node Mutex は取らない —
  確立器内部の swap だけで足りる。
- `NativeState::Unavailable(e)` のときは `e` をそのまま返す（他 op と同じ規律）。

応答（成功、`dispatch` が `timestamp` を補う）:

```json
{"reloaded": true, "ipk": "changed", "reload_count": 2, "timestamp": "..."}
```

- `ipk`: `"changed"` | `"unchanged"`（新旧 `ipk_operational` の比較。鍵そのものは
  出さない）。
- `reload_count`: 起動後の成功 reload 累計（この回を含む）。

失敗は通常の `{"error": {...}}`:

| 状況 | kind | detail の要点 |
|---|---|---|
| KVS が読めない | `store_missing` | `load_fabric_credentials` のエラー文そのまま（`native: read KVS credentials: ...`） |
| NOC 自己発行失敗 | `store_parse` | 同上 |
| fabric identity 不一致 | `other` | `fabric identity changed (fabric_id/node_id/root key); restart matd` |
| native が起動時 Unavailable | 起動時のエラー | 変更なし |
| 確立器が reload 非対応（テスト用 Fake） | `other` | `credential reload not supported by this establisher` |

失敗時は `reload_count` を進めず、メモリの資格情報も変えない。

### 3.2 `matd reload`（CLI）

`crates/matd/src/main.rs` の `Command` に `Reload` を追加し、`stop` / `status` と
同じ `admin_op(cli.socket, "reload")` で送る。成功応答は stdout（純粋 JSON）、
matd 不在は「not running」で exit 1（既存の `send_admin_op` 規律）。

### 3.3 `matd status` の追加フィールド

```json
"reloads": {"count": 2, "last_at": "2026-09-06T12:34:56+09:00"}
```

`last_at` は未 reload なら `null`。`DaemonInfo` に `reloads: AtomicU64` と
`last_reload: Mutex<Option<String>>`（ISO 8601、`mat_core::output::now_iso8601`）
を持たせ、`dispatch` の Reload 成功腕で更新する。

### 3.4 `mat fabric rotate-ipk` の自動 reload

- 場所: `crates/mat/src/commands/fabric.rs::run_rotate_ipk`。`rotate_ipk::run` が
  `RotateStatus::Rotated` を返したときだけ（= controller 側 epoch が commit
  されたとき。`Pending` / `CatchUp*` / `Aborted` は controller の現行 epoch が
  変わらないので撃たない）、`matd_client::hint_reload()` を呼び、その結果を
  `outcome.body(...)` の Value に `matd_reload` として追記してから `emit`。
  `mat-native::rotate_ipk` は matd を知らないまま（body の組み立てに matd の
  語彙を入れない）。
- `matd_client::hint_reload`: `hint_node_touched` と同じ socket 候補
  （`MAT_MATD_SOCKET` / 既定）・同じ接続失敗の扱い（debug ログ）・同じ 300 ms
  read timeout で `{"op":"reload"}` を 1 行送り、応答 1 行を**読む**（ここが
  node_touched との差）。判定:
  - 接続不能 → `not_running`
  - 応答が JSON で `reloaded == true` → `reloaded`
  - それ以外（`{"error":...}`、旧 matd の `parse_error`、timeout、非 JSON）
    → `failed`（detail は `tracing::warn!` に出す。body には状態語だけ）
- body:

```json
"matd_reload": "reloaded" | "not_running" | "failed"
```

- rotate の note（`Rotated` のとき）を次へ変更:
  `"matd_reload says whether a running matd picked up the new IPK; if it is
  failed, run `matd reload` (or restart matd) before the next rotation; nodes
  left out of --nodes need `mat fabric rotate-ipk --catch-up --nodes <N>`"`。
  `rotate_ipk.rs` のテスト `contains("restart")` は `contains("matd reload")`
  に差し替える。
- 「mat は matd に依存しない」原則との整合: 直経路 op の完了後に matd へ
  `node_touched` を撃つ既存パターン（Issue #20）と同じ後処理であり、rotate
  の成否・exit code には影響しない（`matd_reload` は情報フィールド、
  `failed` でも exit は rotate の結果どおり）。

## 4. mat-native 側

### 4.1 `Establisher::reload_credentials`

```rust
/// 資格情報を差し替える（IPK ローテーション後の matd reload）。既定は非対応。
/// 戻り値は「ipk_operational が変わったか」。
fn reload_credentials(&self, _creds: FabricCredentials) -> Result<bool, MatError> {
    Err(MatError::new(ErrorKind::Other,
        "credential reload not supported by this establisher"))
}
```

同期メソッド（I/O なし、swap のみ）。`CaseEstablisher` だけが上書きする。
`FakeEstablisher` / `ScriptedEstablisher` 等のテスト確立器は無変更。

### 4.2 `CaseEstablisher`

- `creds: Arc<FabricCredentials>` → `creds: std::sync::RwLock<Arc<FabricCredentials>>`。
  `establish` / `establish_subscription` は冒頭で `Arc::clone(&*read())` を取り、
  以後そのクローンで完走する（確立中の swap は次回から効く）。
- `reload_credentials`: `write()` で Arc を差し替え、旧 `ipk_operational` と新の
  比較結果を返す。lock poison は `unwrap_or_else(PoisonError::into_inner)`
  （SubHealth の poison 耐性化と同じ規律）。

### 4.3 `Engine::reload_credentials(&self, cfg: &NativeConfig) -> Result<bool, MatError>`

1. `load_fabric_credentials(cfg)`（既存。KVS 読みは `kvs` の flock 規律に従う）。
2. identity 検証: `Engine` が build 時に控える `identity: Option<(fabric_id, node_id,
   root_public_key)>` と比較。不一致は `other`「fabric identity changed ...;
   restart matd」。`with_parts` 構築（テスト）は `None` = 検証スキップ。
3. `self.establisher.reload_credentials(creds)` の結果を返す。

`NativeBackend`（matd）は `engine()` を既に公開しているので追加 API は不要。
matd は `NativeConfig` を `serve_daemon` で組み立てて `NativeBackend::build_with_resolver`
へ渡しているので、`DaemonInfo` に `native_cfg: NativeConfig` を持たせて
`dispatch` へ届ける（`NativeConfig: Clone`）。

### 4.4 原子性と既存セッション

- 差し替えは `RwLock<Arc<_>>` の write 1 回。読み手は Arc クローンで走るので
  「半分新しい資格情報」は存在しない。
- warm session / 購読 session / 進行中の CASE は無影響（設計 §2「含まない」）。
  reload 後の最初の establish（cold / resend / 購読再確立）から新 IPK。

## 5. 文言・docs の変更一覧

| 場所 | 変更 |
|---|---|
| `crates/mat-native/src/rotate_ipk.rs` note（Rotated） | §3.4 の文言へ |
| `crates/mat/src/native_direct.rs` の provision note 定数 | 削除（直経路 provision は note 無し = matd 経由と同じ形） |
| `crates/mat-native/src/runner.rs` テストの `"restart matd"` | 任意文字列でよい（note 引数は残す）。文言だけ `"note text"` 等に |
| `docs/commands.md` rotate-ipk 出力例（~L186）と matd 段落（~L236） | `matd_reload` フィールド、`matd reload` の説明、restart 不要 |
| `docs/commands.md` provision 出力例（~L910-915）と `--rebind` 節（~L971） | restart 文を削除、「matd は送信ごとに KVS を読むので追加操作不要」へ |
| `docs/commands.md` matd 節（status の出力例、admin op 一覧） | `reloads` と `matd reload` を追加 |
| `docs/superpowers/specs/2026-09-05-ipk-rotation-design.md` §6 | 本設計への参照を 1 行追記（歴史文書なので本文は残す） |
| `CLAUDE.md` Backend 節の rotate-ipk 説明 | 「matd は `matd reload` で新 IPK を取り込む（rotate が自動で撃つ）」を 1 文 |

## 6. テスト

### 6.1 ユニット

- `matd::protocol`: `{"op":"reload"}` がパースでき、`node_id()` = None、admin 群の
  既存テスト（`to_device_op` が admin 群を弾かない等）に `Reload` を追加。
- `matd::server::dispatch`:
  - Ready + Fake 確立器 → 確立器が非対応なので `other` エラー、`reloads.count` 不変。
  - Ready + reload 対応の Fake（テスト用に `reload_credentials` を上書きした
    確立器）→ `reloaded: true`、`ipk` が Fake の返した bool に対応、
    `reload_count` 1、status の `reloads.count` 1 / `last_at` 非 null。
  - Unavailable → 起動時エラーそのまま。
- `mat_native::Engine::reload_credentials`（`test_support` の KVS フィクスチャ +
  実 `CaseEstablisher`）:
  - 同じ KVS → `Ok(false)`。
  - `f/<idx>/k/0` の epoch を差し替えた KVS → `Ok(true)`、以後 `establish` が
    新 `ipk_operational` を使う（`CaseEstablisher` から現在の creds を覗く
    `#[cfg(test)]` アクセサで確認）。
  - fabric_id を変えた KVS → `other`、creds 不変。
  - KVS 不在 → `store_missing`。
- `mat::matd_client::hint_reload`: `hint_node_touched_sends_op_line_to_matd` と
  同型のスレッド listener で `reloaded` / `not_running`（socket 無し）/ `failed`
  （`{"error":...}` 応答、非 JSON 応答）。
- `mat::commands::fabric`: `Rotated` のときだけ hint が呼ばれ body に
  `matd_reload` が乗ること（hint を関数注入してテスト）。`Pending` では乗らない。
- `rotate_ipk.rs`: note 文言のアサーション更新。

### 6.2 e2e（`scripts/e2e-device-m3.sh`）

matd の購読が `established` になった後、`mat listen` ステップの**前**に追加:

1. `matd reload --socket $MATD_SOCK` → `reloaded == true`、`ipk == "unchanged"`、
   `reload_count == 1`。`matd status` の `reloads.count == 1`、node 1 の購読
   `state` が `established` のまま（張り直されていない）。
2. `mat fabric rotate-ipk`（直経路、matv は KeySetWrite(0) を受理する — m4 と
   同じ呼び方）→ `status == "rotated"`、`matd_reload == "reloaded"`。
   `matd status` の `reloads.count == 2`。rotate の proof CASE が matv の唯一
   session を奪うので matd の購読は一度落ちる — 既存の「established を待つ」
   ポーリング（関数化して再利用）で再確立を待ってから次へ進む（この再確立が
   新 IPK での cold establish）。
3. matd 経由の `mat on`（`--matd $MATD_SOCK`）が通る。matv は同時 1 session なので
   rotate の proof CASE が matd の session を追い出しており、この `mat on` は
   matd が**新 IPK で**cold establish した証明になる（旧 IPK でも 1 世代は
   通るため厳密な否定証明ではないが、reload 後の確立が壊れていないことは
   pin できる）。
4. 既存の `mat listen` → `mat on` ステップへ続く。

### 6.3 実機スモーク（hogar-matd コンテナ、他セッションと同時に走らせない）

- 本番 fabric で rotate も provision も回さない。`matd reload` 単体:
  `docker exec hogar-matd matd --socket /run/matd/matd.sock reload` →
  `ipk: unchanged`、`reload_count: 1`。直後の `matd status` で購読 19 本が
  `established` のまま、`reloads.count == 1`。warm read（matd 経由）が通る。
- 実行前に他セッションへ連絡する（本セッション開始時、mat-af が deploy.sh で
  hogar を再起動予定と連絡あり）。

## 7. 完了条件

- `task check` 合格、e2e-device-m3 合格（reload ステップ込み）、実機スモーク合格。
- main へ rebase → no-ff マージ → push。
- メモリ更新: jarvis-matd-deploy の運用手順「rotate → matd restart → 検証」を
  「rotate（自動 reload、`matd_reload` を確認）→ 検証」へ。
