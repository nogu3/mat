# CASE Happy Eyeballs 化 + commissioning / x509 ユニットテスト補強（監査レーン D）

日付: 2026-09-03
対象: `mat-controller`（case / 新 race モジュール / commissioning・x509 のテスト）、
`mat-native/src/lib.rs`（2 つの候補ループを 1 呼び出しに置換するだけ）。
背景: 2026-08-31 コード監査「足りない機能 6（CASE 逐次試行で死アドレス 1 本 ~80 秒浪費）」
と「テストの穴（commissioning.rs / x509.rs のテスト密度 ~20%）」。

## 1. 現状と問題

### 1.1 候補アドレスの逐次試行

- `dnssd::ResolvedNode::socket_addrs(scope_id)` が候補 `Vec<SocketAddr>` を返す
  （非 link-local 優先、matd の常駐キャッシュはさらに最近見た順）。
- `mat_controller::case::establish(transport, peer, creds, node_id, mrp)` は
  **1 アドレス専用**。候補を回すループは mat-controller には無く、
  `crates/mat-native/src/lib.rs` の `CaseEstablisher::establish`（517-544 行）と
  そのコピーである `establish_subscription`（569-596 行）にある
  （`for peer in peers { case::establish(...) }`、成功で即 return）。
- mat 直経路（`OneShotRunner`）・matd warm session（`NativeBackend`）・matd 常駐
  Subscribe・`diag node`・mesh probe はすべてこの 1 実装を呼ぶ。

### 1.2 ~80 秒の内訳

`ResolvedNode::mrp_config()` は TXT `SII` を MRP の `initial_interval` にする。死んだ
アドレスへの Sigma1 `send_reliable` は受信が一切無いので `initial_interval` 起点で
再送 4 回（`max_retries: 4`、backoff 1.6、jitter 0.25）:

```
SII=5000ms: 5000 + 8000 + 12800 + 20480 + 32768 = 79,048ms（jitter 込み最大 ~99s）
```

既定 `--op-timeout-ms` 60,000ms より長い。候補 2 本で先頭が死んでいると、2 本目に
1 パケットも送らないまま op deadline で `timeout`（exit 3）になる。実運用でこの形に
なるのは OTBR 再起動などで OMR prefix が変わり、SRP に旧 prefix のアドレスが
lease 期間中残るケース。

### 1.3 テスト密度

- `commissioning.rs`: デコーダ 20 本のうち `decode_open_commissioning_window` は
  完全未テスト。他は happy path + 欠損 1 ケース程度で、`take_u8/u16/u32` の範囲検査、
  `skip_container`、`scan_struct_fields` の重複タグ last-wins、`fields_of` /
  `check_commissioning_response`、`decode_add_noc` の ICAC あり分岐、署名長・IPK 長
  エラー分岐、`CommissionError` Display が未カバー。
- `x509.rs`: `extract_hex_tag` / `parse_vid_pid` / `int_to_32` /
  `parse_ecdsa_signature` / `parse_spki` / `check_ecdsa_sha256_alg` / `parse_key_usage`
  / `parse_basic_constraints` / `parse_validity` / `DerReader::read` の長さ形式に
  負系テストがゼロ。`verify_signed_by` は「別 issuer で失敗」しか無い。
- どちらも純粋関数群で、`tlv::Writer` / `asn1` ビルダ / `x509::test_support`
  （DER 生成器、常時コンパイル）で実機なしに入力を作れる。

## 2. 決定事項（ブレインストーミング結果）

| 項目 | 決定 |
|---|---|
| 実装位置 | **案 A**: レース本体とソケット bind を mat-controller に置き、mat-native は 2 ループを 1 呼び出しに置換するだけ（`Establisher` トレイト不変） |
| stagger | **500ms 定数**。RFC 8305 の 250ms より長めにし、健全な先頭アドレスの Sigma2 が返る前に 2 本目の Sigma1 を撃って chip SDK デバイスの BUSY 応答を誘発しにくくする |
| 試行ごとの上限キャップ | **設けない**。MRP 予算は仕様どおり使い切らせ、全体は従来どおり呼び出し側の op deadline が縛る |
| resolve の後着 AAAA 取りこぼし（ARCHITECTURE.md 残余 (a)） | **対象外**。実害未観測で、直すと全 op の resolve 待ちが増える |
| test_support.rs の縮小 | **対象外**。縮められるのは `responder_task` の生データグラムループ ~130 行のみで mat-native のテストが依存、テスト密度向上とは無関係 |
| im.rs / mat-core / crates/mat / matd | **触らない**（並行レーン B・C との衝突回避） |

## 3. 設計: Happy Eyeballs

### 3.1 モジュール構成

```
crates/mat-controller/src/race.rs        新規: 汎用 staggered race（純粋・時間のみ依存）
crates/mat-controller/src/case.rs        追加: establish_any（候補列 → 専用ソケット × race）
crates/mat-controller/src/lib.rs         pub mod race;
crates/mat-native/src/lib.rs             CaseEstablisher の 2 ループを case::establish_any 呼び出しに置換
```

### 3.2 `race::race_staggered`

```rust
/// N 個の試行を `stagger` 間隔で順に起動し、最初に Ok を返した試行を採用する。
/// 残りの試行は drop でキャンセルされる。全試行が Err なら起動順の Err 一覧。
/// 空入力は Ok にも Err にもならないので呼び出し側が先に弾く（Err(vec![])）。
pub async fn race_staggered<I, T, E, Fut>(
    items: Vec<I>,
    stagger: Duration,
    mut attempt: impl FnMut(I) -> Fut,
) -> Result<(usize, T), Vec<E>>
where
    Fut: Future<Output = Result<T, E>>;
```

- 戻りの `usize` は勝った試行の index（呼び出し側が `peers[idx]` でアドレスを復元し
  ログに出す）。
- 実装は `tokio` のみ（`futures-util` は `ble` feature 限定の optional dep なので使わ
  ない）。`Vec<Pin<Box<dyn Future>>>` を `std::future::poll_fn` で自前ポーリングし、
  `tokio::time::Sleep` で次の起動を待つ。動いている試行が全部 Err になった時点で、
  まだ起動していない試行があれば stagger を待たず即起動する（死アドレスが速く
  失敗する loopback ケースで無駄待ちしない）。
- 敗者のキャンセル = future の drop。`case::establish` は tokio の cancel-safe な
  await しか持たないので、途中で捨てても状態は残らない。ソケットは試行の
  `Arc<Transport>` と共に閉じる（勝者は `SecureSession` が自分の `Arc<Transport>`
  を保持するので生き残る）。
- `#[cfg(test)]` で `tokio::time::pause()` を使い決定的にテストする（§5.1）。

### 3.3 `case::establish_any`

```rust
pub const RACE_STAGGER: Duration = Duration::from_millis(500);

pub struct Established {
    pub session: SecureSession,
    /// 勝った候補。
    pub peer: SocketAddr,
    /// 勝った試行の専用ソケットの local addr（ss / tcpdump 突合ログ用）。
    pub local: Option<SocketAddr>,
}

pub enum EstablishAnyError {
    /// 候補が空（resolve は成功したが AAAA が 1 本も無い）。
    NoAddresses,
    /// 全候補が失敗。起動順。
    AllFailed(Vec<(SocketAddr, CaseError)>),
    /// 試行用ソケットの bind 失敗（1 本でも失敗したらその場で全体エラー）。
    Bind(std::io::Error),
}

pub async fn establish_any(
    peers: &[SocketAddr],
    creds: &FabricCredentials,
    peer_node_id: u64,
    cfg: &MrpConfig,
    stagger: Duration,
) -> Result<Established, EstablishAnyError>;
```

- **試行ごとに `UdpTransport::bind()` で専用ソケット**を取る。1 ソケット共有では
  `UnsecuredExchange::screen` が自分の exchange 以外のデータグラムを捨てるため、
  並行試行が互いの応答を吸って落とす。bind はレース開始前にまとめて行う（N 個。
  候補数は実運用で 1〜3 本）。
- 並行 Sigma1 の安全性: local session id / exchange id / source node id は試行ごとに
  ランダムなので同一ノードへの同時ハンドシェイクは衝突しない。chip SDK デバイスは
  進行中の CASE があると 2 本目に BUSY StatusReport を返すことがあるが、それは敗者
  側の `CaseError::PeerStatus` になるだけで勝者に影響しない。
- Sigma3 まで進んだ敗者を drop した場合、デバイス側にはセッションが 1 本残り idle
  期限で消える。stagger 500ms で先頭が健全なら Sigma2 到着前に 2 本目を撃つことは
  稀で、実害は無い（ARCHITECTURE.md に記録）。
- `Display for EstablishAnyError`:
  - `NoAddresses` → `no addresses`
  - `AllFailed` → `CASE failed on all N address(es): <peer1>: <err1>; <peer2>: <err2>`
  - `Bind` → `bind udp: <io error>`

### 3.4 mat-native 側の置換

`CaseEstablisher::establish` / `establish_subscription` の「bind → resolve → for peer
→ ログ → 包む」を、「resolve → `case::establish_any(&peers, &self.creds, node_id,
&mrp, case::RACE_STAGGER)` → ログ → 包む」にする。変更は置換だけで、

- `Establisher` トレイトのシグネチャ、`SessionConn` / `SubscriptionSession`、
  `Resolver` 注入、`RESOLVE_TIMEOUT`、`map_resolve_err` は不変。
- エラー種別のマッピングは現状維持:
  `NoAddresses` → `ErrorKind::Unreachable`（detail
  `native: no addresses resolved for node {node_id}` — 現行文言そのまま）、
  `AllFailed` → `ErrorKind::SessionFailed`（detail `native: {EstablishAnyError}`、
  購読側は `native: subscription {EstablishAnyError}`）、
  `Bind` → `ErrorKind::Other`（detail `native: bind op udp: ...` /
  `native: bind subscription udp: ...` — 現行文言そのまま）。
- 成功ログ `op transport bound (dedicated socket + CASE)` /
  `subscription transport bound (dedicated socket + CASE)` は文言不変で、`local` は
  勝者ソケットの addr、`peer` は勝者の候補。運用手順（`ss -uanp` 突合）は変わらない。
- `dedicated_op_socket_tests::concurrent_establishes_use_dedicated_sockets` は候補 1 本
  なので不変で通る（回帰の釘）。

### 3.5 予算との整合（`--op-timeout-ms`、matd warm session）

- レース全体は従来どおり呼び出し側の deadline の下にある: 直経路は
  `native_direct::run_op_with_deadline` が op future 全体に timeout、matd は
  `NativeBackend::with_session_inner` の `bounded("establish")` /
  `bounded("resend-establish")`。deadline で `establish_any` の future ごと drop され、
  全試行のソケットが閉じる。
- 所要時間: 従来 = Σ(各候補の失敗待ち) + 勝者、最悪 N × ~80s。
  新 = max(試行の所要) + stagger × (起動した本数 − 1)、最悪 ~80s + 0.5s × (N−1)。
  **候補 1 本の完全到達不能ノードは所要不変**（~80s → 60s deadline で timeout）。
  短縮されるのは「死アドレス + 生アドレス」構成のみで、そのケースは
  ~80s+勝者 → 0.5s+勝者 になる。
- `RETRY_MIN_BUDGET`（10s）、`DEFAULT_OP_BUDGET`（60s）、
  `session::budget_components_are_pinned` の値、`MrpConfig` の既定、
  `ResolvedNode::mrp_config()` はいずれも変更しない。
- matd warm session への影響: `with_session_inner` は `establisher.establish(node_id)`
  を呼ぶだけで、セッション保持・再確立・deadline 判定のロジックは無変更。
  常駐 Subscribe（`establish_subscription`）も同じ置換で、購読の確立が速くなる以外
  の差はない。

### 3.6 エラー分類の互換

| 状況 | 旧 | 新 |
|---|---|---|
| resolve 失敗 | `map_resolve_err`（不変） | 同じ |
| 候補ゼロ | `unreachable` | `unreachable`（文言不変） |
| 全候補 CASE 失敗 | `session_failed`、detail は**最後の 1 本**のみ | `session_failed`、detail は**全候補**を `;` 区切りで列挙 |
| 一部成功 | 成功 | 成功（速い） |

docs/errors.md の kind / exit code に変更なし。

### 3.7 live テストの追従

`tests/live_remote.rs` / `tests/live_commission_real.rs` が手書きしている同型ループ
（`for peer in &peers { case::establish }`）は `case::establish_any` に置き換える
（`#[ignore]` の実機テスト、コンパイルは `cargo test` で通る）。

## 4. 設計: テスト補強

追加はすべて `#[cfg(test)] mod tests` 内のユニットテスト。プロダクションコードは
変更しない（テストで必要になる可視性変更 `pub(crate)` は許可、挙動変更は不可）。

### 4.1 commissioning.rs

1. `decode_open_commissioning_window`: `encode_open_commissioning_window` との
   round-trip + 5 フィールドそれぞれ欠損で `Malformed`。
2. テーブル駆動の不正入力（代表デコーダ 4〜5 本、たとえば `decode_noc_response` /
   `decode_attestation_response` / `decode_add_noc` / `decode_arm_fail_safe` /
   `decode_commissioning_status_response`）: 空スライス、先頭が struct でない、
   struct 途中で切れる、タグの型違い（Bytes 期待に Utf8 等）。
3. `take_u8` / `take_u16` / `take_u32` の範囲超過（`u8` に 256、`u16` に 65536、
   `u32` に 2^32）→ `Malformed`。
4. `skip_container`: 未知タグに入れ子の struct / array / list が入っていても後続
   フィールドを読める。閉じの無いコンテナ → `Malformed`（detail "truncated"）。
5. `scan_struct_fields` の重複タグ last-wins。
6. `decode_add_noc` の ICAC（tag 1）あり分岐、IPK 長違い（"ipk length"）。
7. `decode_attestation_response` / `decode_csr_response` の署名長違い
   （"signature length"）。
8. `fields_of` / `check_commissioning_response`: `im::InvokeResponseData` を直に
   組み、status≠0 → `CommandStatus`、`fields_tlv: None` → `Malformed`、
   errorCode≠0 → `CommandStatus`。
9. `random_discriminator()` が 12 bit に収まる（複数回サンプル）。
10. `CommissionError` Display: 全 variant を 1 回ずつ整形し、非空で variant 固有の
    語を含む（format 文字列の取り違え検出）。

### 4.2 x509.rs

`asn1` ビルダと `x509::test_support::make_test_cert_ext` で入力を作る。

1. `extract_hex_tag`: 正常 / prefix 無し / 4 桁未満 / 非 hex / 末尾 prefix /
   マルチバイト境界（`str::get` が `None`）。
2. `parse_vid_pid`: OID RDN 正常、CN `Mvid:`/`Mpid:` フォールバック、非 UTF-8 値、
   非 0x0C/0x13 タグの値は skip、OID RDN の hex 不正 → `None`。
3. `int_to_32`: 32 バイト / 先頭 0x00 付き 33 バイト（OK）/ 0x00 無し 33 バイト
   （Err）/ 空 / 単独 0x00。
4. `parse_ecdsa_signature`: 正常 / 内側タグ違い / r が 33 バイト非ゼロ先頭 /
   unused-bits ≠ 0。
5. `parse_spki`: 正常 / 曲線 OID 違い / 鍵 OID 違い（RSA）/ 圧縮点（33 バイト）
   → `BadPublicKey`。
6. `check_ecdsa_sha256_alg`: sha256WithRSAEncryption → `UnsupportedAlg`。
7. `parse_key_usage`: 正常 / 空 BIT STRING → `Der` / タグ違い / 3 バイト超。
8. `parse_basic_constraints`: 空 SEQUENCE → `false` / 先頭が BOOLEAN でない →
   `false` / BOOLEAN 空内容 → `false`（DEFAULT FALSE、実装の `unwrap_or(0)`）/
   値が SEQUENCE でない → Err。
9. `parse_validity` / `parse_time_value`: UTCTime / GeneralizedTime / 非 Time タグ →
   `None`。
10. `DerReader::read`: 短形式 / 0x81 / 0x82 / 0x83 以上 → Err / 不定長 0x80 → Err /
    長さがバッファを超える → Err。
11. `verify_signed_by`: 署名フィールド 1 バイト改変 → Err、TBS 差し替え → Err。

### 4.3 密度の目標

追加後の `#[test]` 数: commissioning.rs 36 → 50、x509.rs 10 → 23（2026-09-03 実績）。
件数は目安であり、受け入れ条件はここに列挙したケースが全部入っていること（テーブル
駆動テストは 1 本で複数ケースを覆う）。

## 5. テスト（Happy Eyeballs 側）

### 5.1 `race.rs` ユニット（`tokio::time::pause`、決定的）

- 先頭が即 Ok → index 0、2 本目は起動されない（起動カウンタで検証）。
- 先頭が遅い（Ok まで 10s）、2 本目が stagger 後すぐ Ok → index 1、経過時間 ≈ stagger。
- 先頭が即 Err、2 本目が Ok → index 1、経過 < stagger（Err 即時で次を前倒し起動）。
- 全部 Err → `Err(vec)` の長さ = 本数、順序 = 起動順。
- 空入力 → `Err(vec![])`。
- 起動タイミング: 3 本で t=0 / 0.5s / 1.0s に起動される（各試行の開始時刻を記録）。
- 敗者の drop: 勝者確定後に敗者の future が drop される（`Drop` を実装したガードで検証）。

### 5.2 `establish_any` 統合（loopback、実機不要、`tests/case_self_handshake.rs` に追加）

- 候補 = [死ポート（bind して即 drop した `[::1]` ポート）, 生 responder]、
  `fast_cfg()`（50ms × 3 再送 ≈ 200ms で Err）、stagger 50ms → 生 responder で確立し
  `peer` が 2 本目、その後 read が通る。所要が旧逐次方式の下限（200ms）を超えない
  ことは assert しない（CI ジッタ）— 勝者 index の検証で足りる。
- 候補 = [死ポート, 死ポート] → `AllFailed` に 2 本、それぞれの peer が入る。
- 候補 = [] → `NoAddresses`。
- 候補 = [生 responder, 生 responder（別ポート）] → 先頭が勝ち、responder 2 は
  Sigma1 を受け取っていない（`responder_task` は受信で完了するので、
  `tokio::time::timeout` で未完了を確認）。

### 5.3 mat-native

既存 `concurrent_establishes_use_dedicated_sockets` が不変で通ることのみ（新規テスト
は追加しない — レーン間の衝突回避）。

### 5.4 実機スモーク（hogar-matd コンテナ内、マージ前必須）

- 手順は `jarvis-matd-deploy` メモリの 2026-09-02 式: musl 静的 x86_64 の
  `mat` / `matd` を `*.new` として docker cp、隔離 matd（`--store <コピー>
  --socket /tmp/x.sock`）で本番 matd を止めずに検証、終了後に後始末。
  他セッションの実機スモークと同時に走らせない。
- 合格条件:
  1. 直経路 `read`（node 23 / 24）exit 0、`op transport bound` ログに peer / local。
  2. 隔離 matd 経由 `read` exit 0、warm session 2 回目が再確立なし。
  3. 隔離 matd の常駐 Subscribe が established になる。
  4. 到達不能ノードの所要時間計測: コンテナ内 `avahi-browse -rtp _matter._tcp`
     で AAAA が 2 本以上のノードを探し、あれば旧バイナリ（本番 1.30.0）と新
     バイナリで同じ `read` の所要を比較する。多アドレスのノードが無ければ、
     候補 1 本の到達不能ノード（存在すれば）で新旧とも op deadline（60s）で
     `timeout` になることを確認し、「短縮は死+生構成のみ、その経路は §5.2 の
     loopback テストで釘打ち」と記録する。
  5. WARN / ERROR が起動直後の既知バースト以外に出ない。

## 6. ドキュメント更新

- `ARCHITECTURE.md` の M8c-3 訂正ブロック（1013-1026 行付近）: 残余 (b) を
  「2026-09-03 Happy Eyeballs 化で解消（stagger 500ms、試行ごと専用ソケット、
  敗者 drop）」に更新。(a) は未実装のまま残す。
- `docs/commands.md` の「Op timeout budget」節: 複数アドレスの候補は 500ms stagger
  で並行試行し、最初に確立したものを採用する旨と、候補 1 本の到達不能ノードは
  従来どおり budget いっぱいで `timeout` になる旨を追記。
- `CLAUDE.md` の Backend 節に 1 行（「候補アドレスは `case::establish_any` の
  staggered race、逐次ループを書かない」）。

## 7. 非目標

- 試行ごとの MRP 上限キャップ、SII を短縮する仕掛け。
- resolve の後着 AAAA 待ち（残余 (a)）。
- GUA / ULA / link-local の優先順位変更（現行の非 link-local 優先を維持）。
- test_support.rs の縮小。
- im.rs / session.rs / dnssd.rs の分割（直列で後）。
- mat-core / crates/mat / matd の変更。

## 8. 受け入れ基準

1. `task check` 緑（fmt / clippy `-D warnings` / 全テスト）。
2. §5.1 / §5.2 のテストが全部入り、`cargo test -p mat-controller` で実機なしに通る。
3. §4 に列挙したケースが全部テストに入っている（件数は目安、§4.3）。
4. `concurrent_establishes_use_dedicated_sockets` 不変で通る。
5. §5.4 の実機スモーク合格。
6. §6 のドキュメント更新。
7. `task semver` を回し、追加 pub API（`race` モジュール、`case::establish_any`
   ほか）が minor 相当であることを確認（publish 方針は CLAUDE.md、通常 minor）。
