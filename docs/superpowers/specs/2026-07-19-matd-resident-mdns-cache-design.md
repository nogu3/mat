# matd 常駐 mDNS キャッシュ設計（native resolve 回帰の恒久修正・層2）

2026-07-19 起草。親: `2026-07-17-phase5-m8c3-native-default-design.md`（M8c-3=
native 既定化、0.22.0）。本 spec は 0.22.0 本番デプロイをロールバックさせた
**native 一発 mDNS resolve 回帰**の恒久修正の第二層。

## 背景（実機 tcpdump で確定した真因、2026-07-19 jarvis）

0.22.0 デプロイ時、matd 再起動で warm セッションが失われ、弱リンク Thread
ノード（node5/8/14）を native が resolve できず制御不能になった（avahi /
chip-tool は解決可）。真因は 2 層:

**層1（受信経路・ソケット — 本 spec の前提。実装済み）**: `mat-controller::
dnssd` の resolver は `[::]:0`（ephemeral ポート）で bind し `ff02::fb` に
join していなかった。node5/8/14 を広告する OTBR mDNS advertising proxy
（`192.168.1.112` / `fe80::56ef:44ff:fe81:ce58`）は SRV/AAAA を **ff02::fb へ
マルチキャストで応答**する（QU ビットを立てても無視、RFC 6762 §5.4 が許容）。
ephemeral ソケットはこれを受信できず必ず timeout。→ **修正済み**: resolver の
ソケットを `[::]:5353`（`SO_REUSEADDR` のみ。`SO_REUSEPORT` はマルチキャストを
負荷分散で取りこぼすため不可）＋ `ff02::fb` join ＋ 送信 hop 255（従来 hlim 1 は
RFC 6762 §11 非準拠）＋ QU ビットに変更（`dnssd::bind_mdns_socket` /
`QU_CLASS_IN`）。これで node5/14 が「一度も解決不能」→「広告を捉えれば解決」に
前進。

**層2（応答者特性・根本 — 本 spec の対象）**: node5/8/14 の operational
レコードは OTBR proxy が **約30秒周期**でしかマルチキャストせず（実測
675.7→709.2→740.2→770.9 秒、gap 30〜33s）、on-demand クエリにほぼ応答しない
（20秒間 QU 連送でも割り込み応答なし）。→ 一発 resolve の窓（8s/20s）＜
周期（~30s）で**構造的に取りこぼす**（20s 窓で ~80%）。avahi / chip-tool /
matd-warm が確実なのは **75分TTLの常駐キャッシュ**で周期アナウンスを保持する
から。node6 等の mains 機は自身で mDNS 応答するので一発でも常に成功。

**結論**: 一発 resolve では原理的に確実化できない。`matd`（設計ルール4の例外＝
state を持つ常駐バイナリ。既に warm CASE セッションを保持）に **avahi 相当の
常駐 mDNS キャッシュ**を持たせ、周期アナウンスを保持して establish/再確立を
確実化する。`mat`（一発）は設計ルール4を守り無変更。

## ゴール

1. matd が operational レコードの常駐 mDNS キャッシュを持ち、warm session の
   establish / 再確立が周期アナウンス依存のノードでも**確実に**成功する。
2. matd 再起動直後（キャッシュ空）でも、対象ノードの次の周期アナウンス
   （最大~35s）を待って必ず解決する。以降はキャッシュヒットで即座。
3. `mat`（一発直経路）は**無変更**（設計ルール4維持、キャッシュ無し）。
4. 出力スキーマ・socket プロトコル・エラー kind は**不変**。

## ユーザー決定（2026-07-19）

- **統合方式は案A**（Resolver 抽象 + mat-controller 常駐キャッシュ、mat-native
  の Engine 経由で注入）。検討した代替: (B) matd 内完結（establisher 境界を
  崩し resolve 配線が重複）— 不採用。(C) フル mDNS レスポンダ埋め込み — 過剰。
- **キャッシュミス時の establish は、リスナの次アナウンスを最大~35s await**
  （確実側。代替の「一発 resolve フォールバック（高速・~50-80%）」は不採用）。

## 設計

### コンポーネントと境界

```
mat-controller::dnssd  (protocol=設計ルール1)
  ├─ bind_mdns_socket(scope_id)         [層1で実装済み]
  ├─ OperationalCache                    [新規] 共有ハンドル(Arc)
  │    { map: instance -> (ResolvedNode, expiry),
  │      notify: tokio::sync::Notify,
  │      query_tx: mpsc::Sender<String> }
  │    - get(instance) -> Option<ResolvedNode>   (期限切れ/不在は None)
  │    - request(instance)                        (listener に provoke クエリ依頼)
  │    - (内部) insert(instance, node, ttl), notify_waiters()
  └─ run_operational_cache(scope_id, cache, shutdown) -> !   [新規] 常駐タスク
       単一 mDNS ソケット(bind_mdns_socket)。select!:
         - socket recv -> parse_message -> operational SRV/TXT/AAAA を
           per-instance に畳み込み -> 完成(SRV + >=1 AAAA)で insert + notify
         - query_tx 受信 -> その instance の SRV+TXT provoke クエリを送信
       map は上限 MAX_CACHE(例 256) で flood 制限。

mat-native
  ├─ trait Resolver { async fn resolve(scope_id, cfid, node_id, timeout)
  │                     -> Result<ResolvedNode, DnssdError> }
  ├─ OneShotResolver         [mat 用] dnssd::resolve_operational を直呼び
  ├─ CachingResolver{cache}  [matd 用]
  ├─ CaseEstablisher{ ..., resolver: Arc<dyn Resolver> }  establish から resolver 呼び
  └─ Engine::build(cfg)                 -> OneShotResolver 既定(既存挙動)
     Engine::build_with_resolver(cfg, Arc<dyn Resolver>)  -> 注入版(matd 用)

matd
  ├─ 起動: scope_id = dnssd::iface_index(iface)
  │        cache = OperationalCache::new(); tokio::spawn(run_operational_cache(...))
  │        NativeBackend::build_with_resolver(cfg, CachingResolver::new(cache))
  │        リスナ socket bind 失敗時 -> warn + OneShotResolver に degrade(従来動作で継続)
  └─ shutdown: リスナタスクを cancel(既存 shutdown 経路に相乗り)

mat (一発) : 無変更(OneShotResolver 既定)
```

listener は cfid を知らなくてよい（operational instance を完全名で cache し、
resolver が establish 引数の (cfid,node_id) から完全名を作って exact lookup）。
必要なのは scope_id のみ。matd は `iface_index` で取得する。

### データフロー（matd で node N を制御）

1. op 到着 → `NativeBackend::with_session(N)` → slot 空 →
   `establisher.establish(N)` → `resolver.resolve(scope, cfid, N, RESOLVE_TIMEOUT)`。
2. `CachingResolver`: instance = `operational_instance(cfid,N) + "._matter._tcp.local"`。
   `cache.get(instance)` **ヒット**（リスナが温めた）→ `ResolvedNode` → CASE →
   warm session 保持。
3. **ミス**（cold / 再起動直後）: `cache.request(instance)`（リスナが provoke
   クエリを 1 発送出）→ `notify` + timeout で最大 `RESOLVE_TIMEOUT` キャッシュ
   充填を await。リスナが周期アナウンス（or クエリ応答）を受信 → 畳み込み →
   insert → notify → resolver 起床 → `ResolvedNode` → CASE。
4. 以降の op → warm session（resolve 不要）。

`CachingResolver::resolve` の await ループ（miss 窓は `establish` から渡される
`timeout` ではなく CachingResolver 内部の `CACHE_MISS_TIMEOUT` を使う — 下記）:
```
if let Some(n) = cache.get(&instance) { return Ok(n); }
cache.request(instance.clone());                    // provoke
let deadline = Instant::now() + CACHE_MISS_TIMEOUT;  // 35s(下記)
loop {
    let notified = cache.notified();                // Notify future を先に取得(取りこぼし防止)
    if let Some(n) = cache.get(&instance) { return Ok(n); }
    if now >= deadline { return Err(Timeout); }
    tokio::select! { _ = notified => {}, _ = sleep_until(deadline) => {} }
}
```

### resolve 窓の分離（mat 一発を無変更に保つ）

- `mat-native::RESOLVE_TIMEOUT`（establisher が `resolve()` に渡す値）は **8s の
  まま据え置く**。`OneShotResolver`（`mat` 一発）はこれを honor し、**挙動不変**
  （周期ノードの cold で最大 35s ハングして失敗が遅くなる、といった `mat` 側の
  回帰を作らない）。
- `CachingResolver` は cache miss 時、渡された 8s ではなく**内部定数
  `CACHE_MISS_TIMEOUT = 35s`** を使ってリスナの次アナウンス（周期~30s）を確実に
  跨いで待つ。この 35s が効くのは「matd が cold かつ周期ノード」の初回のみで、
  cache ヒット時は即返し。matd establish は非対話・稀（再起動時/新規ノード）
  なので許容。
- ヒット時は両 resolver とも即返し（待ち窓は無関係）。

### TTL / expiry

- insert 時、畳み込んだ SRV/AAAA/TXT の**最小 TTL**（対象環境では ~4500s=75分）
  を expiry に採用。`get` は expiry 超過で None。
- リスナは ~30s ごとの再アナウンスで同 instance を再 insert → 常に鮮度更新
  （avahi と同じ挙動）。
- 安全上限は設けず広告 TTL を尊重（実測 4500s で過大でない）。

### エラー処理 / 耐性

- **リスナ socket bind 失敗（matd 起動時）**: warn ログ + `OneShotResolver` に
  degrade して matd は継続（従来 0.22.0 と同じ一発 resolve 挙動）。cache 前提で
  無限待ちにはしない。
- **resolve timeout**（deadline 内に充填されない）: 従来どおり `unreachable`
  （kind 不変）。
- **リスナ実行中の socket / parse エラー**: 他者の壊れたデータグラムや一時的
  socket エラーで常駐タスクを落とさない（continue、必要なら再 bind）。既存
  `resolve_operational` の「壊れた応答は握りつぶして継続」と同方針。
- listener タスクが万一終了しても、既存 cache エントリは TTL まで有効
  （ヒットは継続、ミスは timeout）。

### 設計ルール整合

- **ルール1**（protocol は backend crate に閉じる）: mDNS 受信・畳み込み・
  ソケットは `mat-controller`。matd/server は cache ハンドルを配線するだけ。
- **ルール4**（`mat` は KVS 以外の state を持たない・常駐/キャッシュ禁止）:
  cache は **matd 専用**。`mat` 一発は OneShotResolver で無変更。matd は元より
  warm session という state を持つ常駐バイナリ（ルール4の対象外）で、mDNS
  キャッシュはその責務と整合。
- **ルール2/3**（stdout は純 JSON / 診断は stderr）: 影響なし。

## テスト

- **mat-controller ユニット**:
  - `OperationalCache`: insert→get ヒット、expiry 超過で None、MAX_CACHE 上限。
  - fold ロジック: 合成 operational メッセージ（既存 `synth_response` 流用）を
    畳み込み → cache 充填 + notify 発火 を検証。socket I/O は張らず、
    `parse_message` の出力を fold 関数へ直接食わせる形でテスト。
- **mat-native ユニット**:
  - `CachingResolver`（fake cache ハンドル注入）: (a) ヒット即返し、(b) ミス→
    別タスクで insert→await が起床して返す、(c) ミス→無充填で timeout→err。
  - `OneShotResolver` は既存 `resolve_operational` 経路で挙動不変（回帰確認）。
  - 既存 matd `NativeBackend` テストは `FakeEstablisher`（resolver をバイパス）
    なので無影響。
- **matd 結線テスト**: `build_with_resolver` で CachingResolver が
  CaseEstablisher に載ることの型・構築テスト（実 socket は張らない）。
- **実機 E2E（jarvis、受け入れゲート）**:
  1. 修正版 matd をデプロイ（`task dist:arm64`）→ 再起動。
  2. 再起動直後に node5/8/14 を制御 → 初回は最大~35s、以降**即座かつ確実**に
     成功することを複数ラウンドで確認（回帰前は間欠失敗）。
  3. node6 等 mains 機の即応・healthy 経路に回帰が無いこと。
  4. `tcpdump` で cache が周期アナウンスを取り込んでいること（任意）。

## 受け入れ基準

- 上記ユニット/結線テストが green、`task check`（fmt/clippy -D warnings/test）通過。
- 実機 E2E: matd 経由の node5/8/14 制御が複数ラウンドで 100% 成功
  （初回 cold のみ最大~35s、以降即座）。healthy ノードに回帰なし。
- `mat` 一発直経路は無変更（OneShotResolver、コード diff は mat 側ロジックに
  及ばない）。

## スコープ外（YAGNI）

- 起動時の全ノード**eager** 再確立（cache が lazy establish を確実化すれば足りる。
  必要なら別途）。
- `mat` 一発直経路へのキャッシュ導入（設計ルール4に反する。対象外）。
- commissionable browse / discover 経路のキャッシュ（層1のソケット改善で
  受信は改善済み。別課題）。
- キャッシュの永続化（プロセス寿命内のみ。設計ルール4は matd にも「不要な
  永続 state を増やさない」精神で効く）。
