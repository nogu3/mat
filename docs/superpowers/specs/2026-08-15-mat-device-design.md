# mat-device / matv — Matter デバイス側スタック 設計（M1）

> 正本（経緯・全体像）: jarvis-brain vault `docs/superpowers/specs/2026-08-15-matv-alexa-design.md`。
> 本書はその mat repo 側の技術 spec。M1（mat で自己コミッショニング）を対象とする。

## ゴール

家全体を 1 つの Matter 製品として Alexa（Echo）に見せる。そのために mat ワークスペースに **Matter デバイス側スタック**をフルスクラッチで実装する。

- **`mat-device`**（新 crate）: デバイス側プロトコル。PASE verifier / CASE responder / IM サーバ / コミッショニされる側の状態機械 / mDNS 広告
- **`matv`**（新バイナリ crate）: mat-device を使う最初のアプリケーション。設定ファイル駆動の仮想デバイスホスト（将来: Aggregator + bridged endpoints → mando 転送）

マイルストーン:

1. **M1（本 spec / 本計画の範囲）**: 最小の commissionable デバイスを `mat` 自身が commission できる。自作コントローラをテストハーネスに使う閉ループ開発
2. M2: 単一 OnOff デバイスとして Echo がコミッショニング→発見→操作できる（相互運用ゲート）
3. M3: Aggregator + bridged endpoints、設定ファイル駆動
4. M4: mando 接続（Thermostat=enl エアコン、シーンスイッチ）、jarvis 常駐

## 設計原則

- **フルスクラッチ**。rs-matter / matter.js は使わない。新規外部依存も追加しない（既存 workspace 依存 + RustCrypto 系のみ）
- **core/platform 分離**: `mat-device` の `core/` モジュール群（状態機械・コーデック・データモデル）は tokio・ソケット・ファイル I/O を持ち込まない。I/O は `net/`（feature `net`、default on）に隔離。将来の組み込み（ESP）流用の芽を残す。no_std 化自体はスコープ外
- **役割中立な部品は mat-controller に、デバイス側の状態機械は mat-device に**。mat-controller に足すのは「initiator 側と対になる欠けた半分」（コーデック・verifier 型・responder exchange）だけ
- **1 バイナリ 1 仕事**: `mat` CLI にはデバイス役を混ぜない。ARCHITECTURE.md の non-goal を「`mat`/`matd` に混ぜない。sibling crate + 別バイナリは可」に改訂する
- デバイスは**逐次処理**（同時 1 フロー）。コミッショニングも IM 応答も単一ループで捌く。並行化は必要になるまでやらない（YAGNI）

## 再利用マップ（調査済み・2026-08-15 時点）

**そのまま使う**: `tlv`（全部）/ `message`（Header encode+decode 両方向）/ `crypto`（`seal_message`・`open_message` は鍵を渡すだけの役割中立。デバイスは i2r/r2i を入れ替えて渡す）/ `counter`（TxCounter・RxWindow）/ `transport`（`UdpTransport::bind_addr` で :5540）/ `cert`（NOC/RCAC の parse・チェーン検証・`issue_noc`）/ `fabric`（`compressed_fabric_id`・`derive_ipk_operational`・`case_destination_id`）/ `setup_code`（QR 両方向）/ `spake2p::{derive_w0_w1, compute_verifier}` / `commissioning`・`im` のクラスタ/コマンド/opcode 定数 / `mat_core::ids`（全データモデル表）/ `exchange::MrpConfig`

**pub(crate) → pub 昇格が要る**: `spake2p` の点演算・transcript 系 / `pase` の PAKE opcode とコーデック / `x509::test_support::{make_test_cert, make_test_csr}`

**新規実装（存在しない）**: SPAKE2+ **verifier 型** / PASE responder 状態機械 + コーデックの欠けた 5 半分 / CASE **responder** / responder 側 exchange（`UnsecuredExchange` は peer-initiated を捨てる）/ デバイス役 SecureSession の応答メソッド / **IM サーバ**（Read/Invoke の decode、ReportData/InvokeResponse の encode）/ **mDNS 広告**（RR シリアライズごと新規）/ コミッショニングサーバ（fail-safe・CSR 生成・AddNOC 受け入れ・attestation 応答）/ fabric 永続化

**最重要資産**: `mat-controller/src/test_support.rs`（817 行、feature `test-responder`）に**動作実証済みの PASE verifier（:573–817）と CASE responder（:317–541）** が実 UDP で動く形で存在する。M1 はこれを production 品質の状態機械に昇格させる作業が主体。昇格後、test_support は新しい共有部品を使う形にリファクタし、既存テスト（`pase_self_handshake` / `case_self_handshake`）を回帰ガードにする。

## M1 の受け入れ条件

- `matv --config <toml>` で起動したデバイスに対し、実機 `mat commission --setup-code ...` が成功する（PASE → ArmFailSafe → SetRegulatoryConfig → attestation → CSR → AddTrustedRoot → AddNOC → CASE → CommissioningComplete の全ステップ）
- attestation は自己生成の dev チェーン（PAA→PAI→DAC）で通す。mat 側には `--paa-dir` に dev PAA を渡す。CD は検証が warn-only なのでダミーで可
- `task check`（fmt + clippy -D warnings + test）が通り、CI に `cargo check -p mat-device --no-default-features`（core の tokio 非依存の機械的検査）が加わる
- ファブリック（NOC・op key・ACL・IPK）は store ディレクトリに永続化され、再起動後も operational 広告と CASE 応答が生きている

## スコープ外（M1）

- Echo 実機との相互運用（M2）/ Aggregator・bridged endpoints（M3）/ mando 転送・matv の設定駆動仮想デバイス（M4）
- Subscribe のサーバ側（M2 で Echo が要求した時点で実装）
- BLE peripheral / BTP サーバ役 / Thread
- 並行セッション処理・no_std 化

## M1 完了時の申し送り（2026-08-15 最終レビューより）

M1 は受け入れ条件を満たして完了（実 `mat commission` が実 mDNS 発見込みで success、`task e2e:device:m1` で再現可能）。最終ブランチレビューが M2 計画の入力として残した既知リスク・繰延事項:

**M2（Echo 相互運用）で必ず拾う**（発火しやすい順）:

1. ~~piggyback ack 同載リクエストの取り扱い~~（M1 終盤で修正済み: screen が全 peer-initiated 非 ack を退避し runtime がドレイン）
2. **Sigma2 TBE の resumptionID 欠落** — 仕様上 TBEData2 の必須フィールドで chip のパーサが期待する。Echo との CASE が硬く落ちる可能性が高い。Sigma1 の resumption 要求への応答挙動（現状: full Sigma1 として扱う）も併せて確認
3. **fail-safe 期限切れの導入済み fabric ロールバック** — 現状、CommissioningComplete 前にコミッショナーが死ぬとゾンビ fabric が永続し、リトライごとに増える（`next_fabric_index` の再利用問題と併せて修正）
4. **wildcard read / 複数 path read の AttributeStatusIB** — chip 系コントローラは常用する
5. **Subscribe のサーバ側** — Echo は購読失敗をデバイスオフラインとみなす
6. **mDNS の unsolicited announcement / goodbye** — 現状は query 応答のみ。Echo は窓オープン時・operational 出現時の announce を期待する
7. **コミッショニング窓のライフサイクル** — 現状 PASE は常時応答（広告だけ止まる）。窓 open/close 実装時に常設パスコード窓も閉じる
8. **PASE salt の乱数化**（現状固定 salt・iterations 1000）と逐次ループの head-of-line blocking（Echo の並列 exchange 挙動で顕在化しうる）

**クリーンアップ（急がない）**: MRP 送信ループの 3 重複（send_reliable/respond_status/reply_reliable）の共有ヘルパ化 / HKDF48 セッション鍵導出の共有化 / UnsecuredExchange↔ResponderExchange の鏡像重複 / controller 役 send_reliable の cross-exchange piggyback ack 早期完了（reply_reliable にのみ実装済み）/ next_subscription_report が非 ReportData の退避メッセージで UnexpectedOpcode になり得る潜在点
