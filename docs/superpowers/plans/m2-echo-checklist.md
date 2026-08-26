# M2 Echo attestation チェックリスト

Task 10（早期チェックポイント）と Task 16（最終受け入れ）で使い回す。Alexa アプリでの
commission 試行前後にこのファイルへ結果を追記していく。

## 前提

- matv は jarvis 上で手動起動（systemd 化は Phase D、ここでは対象外）。
- Echo 側で「認定されていないデバイス」警告が出ても続行できれば成功扱い。attestation
  ステップで硬く失敗した場合は spec の判断どおり **実装を止めて** ユーザーに報告し、
  Echo のファーム世代・別経路の調査へ切り替える。

## セットアップ記録（Task 10 実施分）

- **デプロイした binary の commit SHA**: `5ab8014497ff258f349f6bac4d5e8a894dff8627`
  （`dist:arm64` タスクへの matv 追加コミット自体は Taskfile/checklist のみでバイナリ内容に影響しないため、
  ビルド元ソースはこの SHA 時点のもの）
- **jarvis 上のパス**:
  - binary: `~/.local/bin/matv`（新規配置、上書き対象なし）
  - config/作業ディレクトリ: `~/matv-m2/`
  - store: `~/matv-m2/store/`
  - stdout log: `~/matv-m2/matv.stdout.log`
  - stderr log: `~/matv-m2/matv.stderr.log`
- **NIC**: `eth0`（`ip -6 addr show scope link` で確認した唯一の物理有線 NIC。他に
  `wpan0`・`tailscale0` があるが対象外）
- **matv.toml**:

  ```toml
  passcode = 74593218
  discriminator = 3210
  vendor_id = 0xFFF1
  product_id = 0x8000
  port = 5540
  store = "/home/jarvis/matv-m2/store"
  iface = "eth0"
  ```

  （passcode は spec §5.1.3.1 の well-known/trivial 値— 20202021 含む `INVALID_PASSCODES`
  ——を避けて選定）

- **QR payload**: `MT:Y.K90Q1212XCO93YL10`
- **manual code**: `31325045526`
- **QR 画像化コマンド**（ローカルに `qrencode` があれば）:

  ```bash
  qrencode -t ansiutf8 'MT:Y.K90Q1212XCO93YL10'
  ```

  作業マシン（このセッションの WSL 環境）には `qrencode` が **入っていない**
  （`command -v qrencode` が失敗）。**manual code の手入力（`31325045526`）が確実な代替手段。**
  Alexa アプリの「デバイスの追加」→「Matter デバイス」フローでは QR 読み取りの代わりに
  コード手入力を選べるはず — QR 画像化がすぐに用意できない場合はそちらを使う。

- **起動確認**:

  ```
  $ ssh jarvis 'pgrep -fa matv'
  771514 /home/jarvis/.local/bin/matv --config matv.toml
  ```

  stdout ログ 1 行目（JSON）:

  ```json
  {"manual_code":"31325045526","port":5540,"qr_payload":"MT:Y.K90Q1212XCO93YL10","store":"/home/jarvis/matv-m2/store"}
  ```

  stderr ログは起動時点で空（エラーなし）。

## Step 3: Alexa アプリでの commission 試行（人間チェックポイント・未実施）

- [ ] Alexa アプリ →「デバイスの追加」→「Matter デバイス」
- [ ] QR（`qrencode` 画像 or 上記コマンド出力）または manual code `31325045526` を入力
- [ ] 観測 (a): 「認定されていないデバイス」警告が出て続行できるか
- [ ] 観測 (b): attestation ステップで硬く失敗するか
- [ ] matv 側 stderr ログ（`ssh jarvis 'cat ~/matv-m2/matv.stderr.log'`）を試行前後で確認し、
      ここに貼り付ける

## Step 4: 結果と分岐（未実施）

- 結果: TBD
- 分岐: TBD（成功 → Phase B へ / attestation 拒否 → 実装を止めて情報収集に切り替え）

## Task 16（最終受け入れ）用メモ

- Task 16 では **この Task 10 の store をそのまま使わない**。Echo 側に古い登録が
  残っていればユーザーにアプリからの削除を依頼してから新 store で開始する。

## Task 10 結果（2026-08-16）: Echo attestation 早期チェック — 不通過【Amazon 側要因で確定】

- attempt 1-2（自己生成チェーン + 自作 CD）: 警告→同意→ AttestationResponse 受領後にクラウド側で停滞、fail-safe 満了で Echo が中断（GS014）
- attempt 3（観測強化後）: 同一パターンをログで確定。AttestationResponse は MRP ack 済み、以降 80 秒間 Echo からの着信ゼロ（pcap で裏取り）→ ArmFailSafe(0) の後始末のみ
- attempt 4（正典 chip テスト証明書チェーン PAA-FFF1→PAI→DAC）: 変化なし（GS014）
- attempt 5（CD の certificate_id="CSA00000SWC00000-00" / device_type_id=22 を matter.js に一致）: 変化なし（GS014）
- attempt 6【対照実験】: matter.js 公式サンプル（@matter/examples 0.15.6、Alexa ペアリング実績のある参照実装）を同一 jarvis/LAN/Echo で commission → **同一パターンで失敗**（attestationRequest 応答後 80 秒沈黙 → armFailSafe 後始末 →「Alexaをデバイスに接続できません」）

**結論**: 自作スタックの問題ではない。「認定されていないデバイス」への同意はローカル続行のみを解錠し、Amazon クラウド側の attestation 検証が未認証（テスト証明書）デバイスを通していない（このアカウント/リージョン/ファーム世代で）。参照実装も同様に落ちるため、デバイス側で回避できる差分は存在しない。

**申し送り**: M2 の Echo ゲート（Task 16）は Amazon 側の道（開発者コンソール登録・サポート問い合わせ・別ファーム世代の Echo での再試行等）が見つかるまで保留。残タスク（Subscribe/窓/salt）は chip-tool ゲートで完成させる方針をユーザーが承認済み。相互運用検証の代替として Apple Home / Google Home（自作証明書に寛容）が選択肢。

## Amazon 側の道 調査結果（2026-08-16 夜・Web 調査）

Task 10 の結論「Amazon クラウドが未認証を一律拒否」を Web 調査 2 系統（Amazon 公式経路 / コミュニティ事例）で検証した。**結論は修正が必要: 一律拒否ではなく Echo 個体/世代/ファーム依存の可能性が高い。**

### 公式経路（存在しないことの確認）

- Amazon に Google Home Developer Console 相当の「テスト VID を自アカウントに登録して attestation を通す」機構は**存在しない**。公式経路は CSA 認証 → DCL 登録（→ 任意で WWA）のみ。
  - Matter support: <https://developer.amazon.com/en-US/docs/alexa/smarthome/matter-support.html>
  - WWA 要件（CSA 認証 + DCL 登録が前提）: <https://developer.amazon.com/en-US/docs/alexa/smarthome/wwa-connection-requirements.html>
- 一方 Amazon 公式の ACK ドキュメントは「プロトタイプは VID 0xFFF1 + テスト証明書チェーンで Echo テストせよ」という立場で、**建前上テスト証明書での commissioning はサポートされている**: <https://developer.amazon.com/en-US/docs/alexa/ack/matter-provision-device.html>
- 未認証ブロックを認めた Amazon 公式声明・changelog 記載は皆無。

### コミュニティ証拠（一律拒否説への反証）

- **同一シグネチャの失敗報告**: RiDDiX/home-assistant-matter-hub **#449**（2026-08-13 起票・オープン）— matter.js 製未認証ブリッジが certificateChainRequest 後に Echo 沈黙 → fail-safe 失効 → GS014。同一ブリッジは Apple Home で完走。環境は **Echo Dot 2nd gen**。<https://github.com/RiDDiX/home-assistant-matter-hub/issues/449>
- **成功報告（反証）**: 同じ HAMH（未認証・テスト証明書）が **2026-07-12 に Echo Dot 4th/5th gen で commissioning 完走**（#414。addNOC → commissioningComplete 確認）。2026-01 にも成功報告あり。<https://github.com/RiDDiX/home-assistant-matter-hub/issues/414>
- → **失敗＝2nd gen、成功＝4th/5th gen** という世代相関。GS014 は認証済みデバイス（Nuki Gen4）でも出る汎用失敗コードで、attestation 拒否専用ではない。
- matter.js の Alexa 固有要件（ECOSYSTEMS.md）: **ポート 5540 固定**（それ以外は発見されない / AddNOC 後 ~20 秒でロールバック）・**Endpoint 1 必須**・仕様外クラスタ禁止。matv は 5540 + Endpoint 1 OnOff で充足済み。<https://github.com/matter-js/matter.js/blob/main/docs/ECOSYSTEMS.md>
- マルチアドミン共有（他エコシステムから Alexa へ共有）は attestation を再実行するため理論上回避にならず、成功報告も無し。
- ブリッジ経由が実績最多: 未認証 matter.js ブリッジ（HAMH）が通った環境では配下エンドポイントは個別 attestation を受けない — **M3 Aggregator 構成そのものが Echo 通過後の正攻法**。

### 次の一手（優先順）

1. **別世代の Echo（Dot 4th/5th gen 等）で再試行**。複数 Echo がある場合はどれがコミッショナーに選ばれるか制御できないため、他の Echo を一時電源オフして対象個体を固定する（コミュニティで唯一挙がっている切り分け策）。
2. RiDDiX #449 に pcap 所見（AttestationResponse MRP-ack 後 80 秒沈黙 → ArmFailSafe(0)）を追記して事例を束ねる。
3. project-chip/connectedhomeip に再現手順つき issue（Amazon の chrisdecenzo がトリアージ実績あり）。<https://github.com/project-chip/connectedhomeip/issues/34174>
4. developer.amazon.com/support へ照会。

### 追加観測（2026-08-16 17:38 / 18:13 JST の再試行）

Task 10 記録後の再試行（matv.stderr.log 08:38Z / 09:13Z）は**より手前の PASE 段階で失敗**: Echo が PBKDFParamRequest 送信 → matv 応答直後に StatusReport (0x40) で中断。同一バイナリで 16:35 JST は PASE 成立していたため、Echo 側の状態変化（失敗デバイスの記憶・早期中断）が疑われる。該当時間帯の pcap は無く StatusReport の中身は未取得。副産物 2 件:

- matv の PASE 状態機械は StatusReport を「unexpected opcode」として扱う。エラー StatusReport のコード（GeneralCode/ProtocolCode）をログに出すと今後の切り分けが速くなる（小改善候補）。
- jarvis 稼働中の matv は Task 10 時点のビルド（5ab8014、窓 close の PASE 無応答 drop・salt 乱数化を含まない）。**次回 Echo 検証前に最新 main のビルドを再配布すること**。

## 追加調査（2026-08-17）: 世代相関説を棄却 → クラウド側ロールアウト説へ更新

ユーザーの Echo は **Echo Show 11（最新モデル・2026 年版ファーム）**と判明。「古い世代で失敗・新しい世代で成功」という前日の世代相関説は棄却。時期相関を再調査した結果:

### タイムライン（未認証 matter.js 系ブリッジ × Alexa）

- **〜2026-07-13: attestation は通っていた**。RiDDiX/HAMH #401（07-04, 失敗するが addNoc 後 = attestation 通過）、#414（07-13, Echo Dot 4th/5th gen で commissioningComplete 完走）、Luligu/matterbridge #575（07-10 頃, 稼働中）
- **2026-08-13〜: 「attestation 交換直後に沈黙 → fail-safe 失効 → GS014」の新シグネチャが独立 3 系統で出現**:
  - RiDDiX/HAMH **#449**（08-13, Echo Dot 2nd gen fw 13121734531）<https://github.com/RiDDiX/home-assistant-matter-hub/issues/449>
  - Luligu/matterbridge **#605**（08-16, Echo Dot with Clock fw 13121734532 + Echo Show 5 3rd gen fw 5601010007320, **regulatory JP**。attestationRequest 応答直後に停止 → 80 秒で fail-safe 失効 → GS014。Google Home は成功）<https://github.com/Luligu/matterbridge/issues/605>
  - 本プロジェクト実測（08-16, Echo Show 11 2026FW, matv + matter.js 公式サンプル）
- 8 月以降の「未認証ブリッジ × Alexa 成功」報告はゼロ（消極的証拠）

### 判定

**「2026-07-13〜08-13 の間に Amazon がクラウド側で attestation 検証を厳格化（test 証明書チェーンを拒否）」説が最有力**。スタック非依存（HAMH / matterbridge / matter.js / matv）・Echo 世代非依存（2nd gen〜Show 11）で同一シグネチャ、クラウド側変更なら FW 世代を問わず一斉に効く点と整合。ただし Amazon の公式告知・changelog は皆無で、意図的強制かリグレッションかは不明（inconclusive, leaning supported）。

### 再検証（2026-08-18 早朝・Echo Show 11 / 最新 main ビルド）: 同一シグネチャで失敗を追認

「念のため」の実機再検証。条件を全て更新して実施:

- matv: 最新 main（282d635）のビルド（M2 完了版: 窓 close・salt 乱数化入り）を再配布
- 新 identity（discriminator 1478 / passcode 51869473、`~/matv-r2/`）+ まっさらな store
- Echo Show 11（最新 2026FW）、pcap 取得あり（`~/matv-r2/echo-r2.pcap`）

結果（matv ログ + pcap 両方で確認、時刻 JST）:

- 04:39:17 PASE 成立 → ArmFailSafe(80) → SetRegulatoryConfig → CertificateChainRequest×2 → AttestationRequest
- 04:39:17.947 matv が AttestationResponse 送信 → **04:39:17.960 Echo が MRP ack** → **以降ちょうど 80 秒間、Echo からの着信ゼロ（StatusReport もセッション close も無し）**
- 04:40:37 Echo が ArmFailSafe(0) の後始末だけ送って終了 → アプリは GS014

**追認事項**: (1) 最新ビルド・新 identity・新 store でも変化なし = クラウド側 attestation 拒否の結論は堅い。(2) 新 identity にしたことで前回の「PASE 早期中断（StatusReport 0x40）」は発生せず正常に attestation まで進んだ — あれは「Echo が失敗デバイス（同一 discriminator）を記憶して早期中断する」挙動で、本質と無関係と確定。(3) デバイス側で打てる手が無いことを実機で再確認。凍結判断は維持。

### 帰結（前節「次の一手」の修正）

1. ~~別世代 Echo で再試行~~ → **棄却**（最新 Show 11 で失敗済み。クラウド側なら世代交換は無意味）
2. デバイス側でできることは無い。**Echo ゲートは Amazon 側の解消（修正 or 公式声明）待ちで凍結**が妥当
3. ウォッチ先: **#605**（help wanted・JP 環境で本件と同一）と **#449**。pcap 所見（AttestationResponse MRP-ack 後 80 秒沈黙）を追記して事例を束ねる価値は引き続き有り
4. 相互運用の実機検証は **Apple Home / Google Home へ切替**（8 月時点でも成功報告が継続。M3 Aggregator の interview 型コントローラ対応とも方向が一致）

## 2026-08-18 追記: matter.js（HA Matter Server）実機ゲート通過

代替検証先の第一弾として HA の Matter Server（matter-server 1.1.7 =
matter.js 0.17.4 ベース）で実機検証を実施。**3 つの interop バグを特定・修正し、
commission 完走 + interview（32 属性）+ OnOff トグル往復まで green**
（検証リグ: jarvis 上に同一版 matter-server を立て WS API から commission）。

修正 3 件（コミット 8d67b35, 4f58be6）:
1. AddNOC: matter.js の空 ICACValue（省略でなく空バイト列）を None に正規化
2. ArmFailSafe: armed 中の非ゼロ再アームで未確定 fabric を巻き戻していた
   （spec §11.10.6.2 違反。matter.js は AddNOC 後に再アーム→CASE→Complete）
3. parse_sigma1: ネストした initiatorSessionParams の中の
   SESSION_ACTIVE_INTERVAL(300) を initiatorSessionId と誤読（sigma3/TBE の
   フラットループにも同修正）

**残タスク**: Timed Interaction（opcode 0x0A）未実装。スマホの HA アプリ経由の
追加は **Android の Google Play Services スタックが commissioner になり**、
TimedRequest → INVALID_ACTION で中断する（8/18 実測で送信元 MAC がスマホと確定）。
本番 HA へのペアリングは **PC ブラウザの HA Web UI から**行えば matter-server
経由になるため現状の修正だけで通る見込み。要: アドオン設定で
Test Net DCL 有効化。

## 2026-08-26 再試行（Apple Home ゲート通過後・フル適合ビルド）: 同一シグネチャで失敗

Write/ACL/root 適合性実装済みビルド（94c15e4 相当、Apple Home は同日タイル操作まで通過）で
Echo Show 11 から再試行（`~/matv-alexa/`, discriminator 1859, port 5540, self チェーン）:

- 10:30:28 PASE → ArmFailSafe(80) → SetRegulatoryConfig → **SetTCAcknowledgements(0x06)**
  （新観測。matv は UNSUPPORTED_COMMAND 応答 — HAMH #449 の実験で受理しても結果不変と確認済みの項目）
  → CertificateChainRequest×2 → AttestationRequest 応答
- 以降 80 秒沈黙 → 10:31:48 fail-safe 失効 → ArmFailSafe(0) 後始末 → アプリは GS014

**判定**: クラウド側 attestation 拒否のシグネチャ完全一致。デバイス側適合性の改善は無関係
（そもそも interview 到達前）。

**新情報（2026-08-25/26 のコミュニティ動向）**:
- matterbridge #605: コラボレータ tammeryousef1006 が **Echo Show 8 3rd gen (fw 3607267748)
  で未認証 matterbridge のペアリング成功**を報告（08-13 以降初の成功報告、非 JP 環境）
- 同日、JP 環境の報告者 k9i は依然失敗（エラーコードが GS014 → RN002 に変化）
- HAMH #449 にも 08-24 に同症状の新規報告（Apple/Google は成功、Alexa のみ失敗）

→ **「リージョン/アカウント段階のロールアウト（JP 未解除）」説が最有力に更新**。
JP + Echo Show 11 + 独立 Rust 実装で同日失敗という本試行はこの説の裏付けデータ点。
引き続きウォッチ + JP データ点の #605 への投稿を検討。
