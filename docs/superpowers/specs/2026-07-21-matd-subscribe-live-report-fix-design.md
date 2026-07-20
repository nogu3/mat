# matd 常駐 Subscribe — live レポート未達の修正（実機 E2E 不合格の後追い）

日付: 2026-07-21 / 対象: `mat-controller`(session) + `mat-native`(SubscribeConn) + `matd`(subscription) / 前提: 0.25.0（PR #11、branch `worktree-feat-matd-subscribe-listen`, HEAD 460b6c4）

## 背景

0.25.0（matd 常駐 Subscribe + `mat listen`）を jarvis にデプロイして実機 E2E を実施した結果、**核心機能（デバイスの状態変化イベントが `mat listen` に流れる）が動かなかった**。単体・統合テストは fake で全て緑だが、**実機でしか出ない「デバイス発 live レポートの受信経路」の不具合**。本 spec は次セッションで root-cause を確定して直すための調査駆動の設計。現状 PR #11 は未マージ、本番は 0.24.0 にロールバック済み。

前セッションの詳細記録: `.superpowers/sdd/progress.md` の「実機E2E」節、メモリ `[[matd-subscribe-listen]]`。

## 確定した事実（E2E 実測）

1. **購読は確立する。** SubscriptionManager は commissioned 全ノードへ購読を張り、到達可能なノードで SubscribeResponse を受信し `subscription established`（node_id / subscription_id / max_interval_s=3600）を info ログ。
2. **配信路の下流は全て正常。** `mat listen` の接続・ack 行・フィルタ（node/endpoint/cluster/attribute、名前/数値）・count/timeout・exit code（0/3/13）・lag 切断は期待どおり動作（統合テストと実機で確認）。
3. **live レポートが pump に届かない。** 購読済みノードの on-off を実際に変化させても（`mat read` で true↔false を実証）、`mat listen` は **イベントゼロで exit 3 timeout**。session 層に受信ログ（`recv_from` 直後）を仕込んで再デプロイした結果、**どの購読ソケットも device 発 datagram を1つも受信していない**（全ノード合計 0 件）。`events_from_report` / pump ループ本体（`next_subscription_report` の Ok 分岐）にも一切到達しない。
4. **デバイスは live レポートを送っている。** tcpdump で、node 6 は状態変化のたびにレポート（len 147/173）を **LAN 上の別 Matter コントローラ（別 fabric、別ホスト `fdcd:3f07:c294:6868:cb38:e02f:6ead:6e99`、port 54994）の購読へ確実に送っていた**（多admin 環境）。→ デバイス側の live 報告は機能している。matd の fabric 購読へは送られていない/受けられていない。
5. **matd の購読ポートへの live レポートは tcpdump に出ない。** node 6 のトグル時、eth0 capture には matd の **OP コマンド traffic のみ**（matd `ba27:...96f6` → node6:5540）で、device → matd 購読ポートの live レポートは観測されなかった。

→ 問題の局在: **「デバイス → matd の購読ポンプ」の受信のみ**。listen→broadcast→client 側は無罪。

## 環境の重要な前提（次セッションで必須の背景）

- **jarvis に OTBR を移行済み**（2026-07-20、ユーザー確認）。jarvis は `wpan0`（802.15.4 Thread ラジオ）を持つ Thread BR。HA 側 OTBR は無い。メモリ `[[thread-network-topology]]` は更新済み。
- **Thread プレフィックス 2 系統を観測**: `fd54:4b81:8cce`（eth0 の別 BR `fe80::1ac2:3cff:fe48:7e45` 経由でも到達）と `fdd8:2861:a64d`（wpan0 直、node 7-12）。node のルートは `ip -6 route get <addr>` で確認（例: node8=fdd8→dev wpan0、node6=fd54→dev eth0）。
- **matd の unit は `MAT_MATD_IFACE=eth0`**。しかし多くのデバイスは wpan0 経由。socket は `[::]` bind（全 iface 受信）だが、**送信元アドレス選択はルート依存**（wpan0 の `fd54:...f289` か eth0 の `ba27:...96f6`）。
- **多admin**: 同一デバイスに matd 以外の Matter コントローラ（別 fabric）が購読中。
- **matd の tracing は `RUST_LOG` を読む**（`MAT_LOG` は無効。mat 本体は `MAT_LOG`）。デバッグ時は `RUST_LOG="matd=debug,mat_native=debug,mat_controller=debug,info"`。
- **デバイスの購読/CASE スロットは有限**。前セッションで matd を多数回再起動した結果 **IM status 0x80 (RESOURCE_EXHAUSTED)** と CASE MRP timeout が多発し枯渇させた。**残留購読は最大 MaxInterval=3600s で自然解放**。次セッションは枯渇回復後（≥1h）に、**再起動を最小化**して行う。
- **ssh jarvis が複雑コマンド/backgrounding で exit 255 頻発**（1Password agent）。launcher script を scp して単純コマンドで叩く運用（前セッションで確立）。

## 根本原因の候補（ランク付け）と切り分け実験

`H*` は仮説、`E*` は切り分け実験。**まず E1〜E4 で局在を確定してから修正する**（未確定のまま直さない）。

### H1（最有力）: matd の購読がデバイス側で真に active になっていない

SubscribeResponse は受けるが、matd 側の最終処理の欠落でデバイスが購読を未完了/即破棄とみなし live レポートを出さない。

- **H1a**: matd が SubscribeResponse に MRP ack を返せていない → デバイスが再送の末に購読を捨てる。`screen()` は needs_ack を ack する実装だが、**実機ワイヤで SubscribeResponse の needs_ack と matd の ack 送出を確認していない**。
- **H1b**: priming 最終チャンクの StatusResponse、または SubscribeResponse 受信直後の exchange クローズが spec とズレていて、デバイスが購読を activate しない。
- **H1c**: `subscribe_wildcard` が SubscribeResponse を受けた exchange と、live レポートが来る device-initiated exchange の扱いに齟齬（`AnyPeerInitiated` フィルタや MRP の非対称）。

### H2: 購読は active だが、送信元アドレス不安定/経路非対称で matd が受け取れない

デバイスは CASE 確立時の matd 送信元（addr:port）へ live レポートを送るが、その宛先が非対称経路・アドレス失効・iface 不一致（IFACE=eth0 vs 実経路 wpan0）で matd の socket に届かない。E2E では device→matd 購読ポートの traffic が eth0 capture に出なかった（H2 なら wpan0 側に出る可能性）。

### H3: 多admin/リソース制約でデバイスが matd 購読を受理していない

デバイスの per-fabric/全体の購読上限、または別コントローラの購読との競合で matd の購読が accept-then-drop されている。IM 0x80 の頻発はこの兆候（ただし前セッションのは自作の枯渇の可能性大）。

### H4: matd ランタイム/socket 配線の不具合

pump タスクが実際には正しい socket で recv していない（Arc/session 配線、または tokio スケジューリング）。前セッションで **ある matd socket に Recv-Q=6720 の未読データ滞留**を観測（role 未特定）。購読 socket なら pump が読んでいない証左。

### 切り分け実験（クリーンなデバイス状態で、単一ノード・単一 matd・最小再起動）

- **E1（局在の主実験）**: 単一ノード、fresh matd、`RUST_LOG=...debug`。計装で (a) `subscribe_wildcard` 完了、(b) SubscribeResponse への matd の MRP ack 送出、(c) pump ループ entry、(d) 購読 socket の全 datagram受信 を可視化。トグル 1 回で観測。→ (c)まで来て(d)ゼロなら「デバイスが送っていない」（H1/H3）、(d)は来るが pump に渡らないなら screen/decrypt（H2 の受信側 or 別バグ）。
- **E2（送受信の物理確認）**: `ss` で matd の購読 socket の実 port を特定 → **eth0 と wpan0 両方**を、matd 購読ソース addr:port でフィルタして tcpdump。デバイスが matd の購読ポートへ live レポートを送っているか、送っているならどの iface に届くかを確認（H2 判定）。
- **E3（ワイヤ差分）**: 既知良好のコントローラ（`chip-tool subscribe-attribute`／別 fabric admin）と matd の Subscribe ハンドシェイクを両方 tcpdump して**バイト差分**を取る（H1 の spec ズレ検出）。matd の SubscribeRequest/StatusResponse/ack の並びを CHIP と突合。
- **E4（socket role 特定）**: Recv-Q が溜まる socket を `ss -p` の fd と `/proc/<pid>/fd` で role 特定。購読 pump socket なら H4。

## 仮説別の候補修正（E1〜E4 で確定してから採用）

- **H1a/H1b 確定時**: ハンドシェイク完了の是正 — SubscribeResponse の確実な MRP ack、priming 最終 StatusResponse と exchange クローズ順を CHIP に合わせる。`subscribe_wildcard` / `next_subscription_report` の exchange・MRP まわりを実機ワイヤ基準で修正し、`ReliableChannel` 単体テストに実機で判明した並びを追加で釘打ち。
- **H2 確定時**: 購読 socket の**送信元アドレスを安定化**（stable/EUI-64 アドレスへ bind、または `IPV6_*` で public/stable 優先、または iface 明示 bind）。matd IFACE の扱い（eth0 固定 vs wpan0 vs 自動）も再検討。CASE 寿命の間ソースが有効であり続けることを保証。
- **H3 確定時**: `KeepSubscriptions`/teardown の実挙動を検証し、**古い購読の確実な掃除**と多admin 競合の回避。establish の**同時実行を制限・スタガー**し、IM 0x80 に指数 backoff（+上限）で対処、常駐再購読の churn を抑える。
- **H4 確定時**: pump タスク/socket 配線の是正（dedicated socket の Arc/session、tokio ブロッキング混入の排除）。

## 仮説に依らず入れる改善（付随）

- **再起動 churn 対策**: matd の常駐購読が再起動・再購読でデバイスの購読/CASE スロットを枯渇させ得ることが実機で判明。establish の同時数上限・スタガー起動・re-subscribe の下限間隔・IM 0x80 の backoff を入れる（本番投入の前提）。
- **`MAT_LOG` 整合**: matd の tracing 初期化を `mat` 本体と同様に `MAT_LOG`（無ければ `RUST_LOG`）も読むよう揃える。運用のデバッグ動線を一本化。
- **専用 ephemeral socket 方式の再考**: 再起動で port が変わり orphan 購読が残る。stable port/stable source への寄せ、または再起動時の既存購読掃除の設計余地を検討（H2/H3 の結論次第）。

## 環境前提（次セッション開始時のチェックリスト）

- [ ] デバイスの購読/CASE スロット回復（前セッションの枯渇から ≥1h、または `mat` で数ノードの単発 read/CASE が安定成功することで確認）。
- [ ] matd を安定稼働させたまま検証（**再起動を最小化**、必要時のみ）。
- [ ] `RUST_LOG` で debug。`MAT_MATD_SOCKET=/run/user/1000/matd/matd.sock` を明示。
- [ ] wpan0/eth0 の両 iface を意識（`ip -6 route get` で対象ノードの経路確認）。
- [ ] ssh は launcher script + 単純コマンドで（複雑 backgrounding は exit 255）。
- [ ] 本番サービスへの影響を避けるなら、検証は別 socket/別 store の隔離 matd で（casad への影響回避）。

## 受け入れ基準（E2E ゲート）

- `mat listen --node <N> --cluster onoff --count 1 --timeout-ms 20000` を起動中に `mat on/off --node <N>` で状態を反転させると、**数秒以内に on-off 変化イベントが 1 行流れて exit 0**。`priming:false`、JSON は mat スキーマ（timestamp/node_id/endpoint/cluster/attribute/value/priming）。
- Nanoleaf（例: node 6 `desk_tape_light` / node 8 `dropped_ceiling_tape_light`）で再現的に成功。
- 常駐購読が数十分の連続稼働でデバイスを枯渇させない（IM 0x80 が定常発生しない）。
- 合格後: 本番 jarvis へ再デプロイ → スモーク → PR #11 マージ。

## スコープ外（本 spec でも据え置き）

EventReport / DataVersionFilter / LIT ICD / `subscriptions.toml` / 状態スナップショット・リプレイ（元 spec のスコープ外を継承）。

## 結果（2026-07-20 実施 — root-cause 確定と修正）

E1〜E3 を単一ノード隔離 matd（別 socket・単一ノード store・RUST_LOG debug/trace + 両 iface tcpdump）で実施した。**確定した root-cause は仮説リストのどれとも微妙に違い、2 段の複合**だった:

1. **「established 後に沈黙」ではなく priming チャンクハンドシェイク中に死んでいた。** 前セッションの「IM 0x80 = RESOURCE_EXHAUSTED（スロット枯渇）」は**誤読**で、0x80 は **INVALID_ACTION** — デバイスが chunk 応答（我々の StatusResponse）待ちタイムアウト（≈5s）で exchange を破棄した後、遅れて届いた再送 StatusResponse に返す応答。**「枯渇」は幻**（E2E 中の read/CASE は常に成功しており、待機も不要だった）。
   - **真因(a): MRP 再送間隔が SII 固定。** 実機 Thread デバイスは TXT で SII=5000ms を広告し、我々は active な exchange 中の再送にもこれを使っていた（ワイヤ実測: 喪失した Sigma3 の再送まで 4.99 秒）。loss のある経路では 1 喪失で 5 秒停止 → デバイス側タイムアウトに必ず負ける。spec 4.12.8 の「直近受信ありのピアには SESSION_ACTIVE_INTERVAL (SAI=300ms) で再送」を実装して解消（`MrpConfig.active_interval` + `SecureSession`/`UnsecuredExchange` の `last_rx`、`PEER_ACTIVE_WINDOW=4s`）。CASE Sigma3 の回復も同時に 5s→300ms。
   - ack 不正説（H1a）は棄却: 健全リンク（node8）では standalone ack が全て受理され priming 26 チャンクが dup ゼロで完走。平文 CASE 段の ack バイト列も spec 準拠を pcap で確認。
2. **真因(b): 確立後の盲目窓。** flaky リンクのデバイスはレポート配送失敗（MRP 全滅）時に**購読を黙って破棄**する（実測: node6 で配送 2 分後に破棄、以後トグルしてもレポートゼロ）。subscriber 側の死活検知は keepalive 無音 ×1.5 しかなく、ceiling 3600s では**盲目窓が最長 90 分**。`SUBSCRIBE_MAX_INTERVAL_CEILING_S` を 300s に短縮（盲目窓 ≤7.5 分で自動再購読）。
   - H2（送信元アドレス/経路非対称）は補助要因どまり: fd54 系ノードへは「eth0 の別 BR 経由」と「wpan0 直（jarvis 自身が同一 mesh の BR）」の 2 経路があるが、ping 実測で node6 はどちらの経路でも 20〜47% loss = **node6 自身の弱リンク**（node5 の前歴と同型）。H3/H4 は棄却（Recv-Q 滞留は再現せず）。
3. **付随改善**: matd の tracing が `MAT_LOG`（無ければ `RUST_LOG`）を読むよう mat と整合。購読経路の恒久観測性（購読 socket の bind/peer info ログ、pump の datagram/report debug、screen 棄却理由 trace、pump 終了理由 info）。
4. **実機結果**: node8/7/10/12 で購読確立・pump 稼働、`mat on/off` → `mat listen` へ約 400ms でイベント配信・exit 0（受け入れ基準達成）。node6 はセッション当時 RF が 20〜47% loss で priming 完走が運任せ（確立に一度成功し live レポート配信も実証。破棄→再購読ループは設計どおり回る）。デバイス側は max_interval=300 を受理。

コミット: 8f4139d（SAI 再送 + 観測性）、以降のコミット（ceiling 300s + docs）。
