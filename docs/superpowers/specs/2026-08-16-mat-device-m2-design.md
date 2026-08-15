# mat-device / matv — M2: Echo 相互運用ゲート 設計

> 正本（経緯・全体像）: jarvis-brain vault `docs/superpowers/specs/2026-08-15-matv-alexa-design.md`。
> 本書は M2 の mat repo 側技術 spec。入力は `2026-08-15-mat-device-design.md` の「M1 完了時の申し送り」節（既知リスク 8 件）。

## ゴール

matv を**単一 OnOff 仮想デバイス**として Echo と相互運用させる（本プロジェクトの白黒ポイント）。Aggregator・マトリョーシカ化は M3、mando 接続は M4。

受け入れ条件:

- Alexa アプリから QR コードで commission が成功する
- Alexa アプリからの On/Off 操作が matv に届き、状態が変わる
- 「アレクサ、〈デバイス名〉をつけて」の音声操作が通る
- matv 側の状態変化が Subscribe 経由で Alexa アプリに反映される
- matv 再起動後も Echo から操作できる（fabric 永続化 + operational announce + CASE 再確立）
- `task check` が通り、既存回帰（`pase_self_handshake` / `case_self_handshake` / `task e2e:device:m1`）が生きている

## 二段ゲート構成

1. **ゲート 1: chip-tool（WSL2 内で完結）** — 公式コントローラ chip-tool を審査官にし、chip 系パーサの期待（resumptionID・wildcard read・Subscribe）を、両端のログが見える環境で先に潰す。commission → on/off invoke → wildcard read → subscribe → matv 再起動後の再接続、までを通す。
2. **ゲート 2: Echo 実機（jarvis 配備・本番 Echo / 本番 Alexa アカウント）** — matv を jarvis へクロスビルド配備（despliegue の既存フロー。WSL2 は NAT 配下で Echo から mDNS 発見不可のため）。Alexa アプリから commission し、受け入れ条件を手動チェックリストで消化する。

**Echo attestation 早期チェックポイント**: Phase A 完了（chip-tool commission 成功）時点で一度 jarvis に配備し、Alexa アプリから **commission だけ**試す。Echo が dev attestation チェーン（CSA 非認証デバイス）を受け入れるかの白黒を、Subscribe 実装前に付ける。HA Matter Hub が test 証明書で Alexa ペアリングできている実績から見込みは高いが、ここが落ちると M2 全体の前提が崩れるため、落ちた場合は実装を止めて情報収集に切り替える。

## 実装スコープ（フェーズ分け）

申し送りのうち未対応の 7 件（#1 piggyback ack は M1 終盤で修正済み）を M2 必須として全部拾う。順序は発火順 ≒ ゲート 1 の進行順。

**Phase A — chip-tool commission を通す**

1. **Sigma2 TBE の resumptionID**（TBEData2 の必須フィールド、chip のパーサが期待）。CASE 確立ごとに乱数生成して TBE2 に載せる
2. **Sigma1 resumption 要求への応答** — resumption フィールド付き Sigma1 を正しく parse し、**full Sigma2 への fallback を正**とする（仕様準拠の縮退経路。再起動後は resumption state が無いので fallback は必ず要る）。Sigma2Resume の実装は chip-tool / Echo が fallback を拒否した場合のみ（YAGNI）
3. **fail-safe 期限切れ時の導入済み fabric ロールバック** — CommissioningComplete 前にコミッショナーが死んでもゾンビ fabric が残らない。`next_fabric_index` の再利用問題も併せて修正
4. **wildcard read / 複数 path read** — path 展開と AttributeStatusIB を含む ReportData 生成（chip 系は commissioning 中から常用）
5. **mDNS unsolicited announce / goodbye** — コミッショニング窓オープン時・operational サービス出現時の announce、終了時の goodbye（現状は query 応答のみ）

**Phase B — chip-tool 操作・購読を通す**

6. **Subscribe サーバ側** — SubscribeRequest 受理、priming report、属性変化時のレポート発行、max-interval の keep-alive レポート。Echo は購読失敗をデバイスオフラインとみなすため必須

**Phase C — 堅牢化**

7. **コミッショニング窓ライフサイクル** — 窓 open/close の状態機械。窓外は PASE に応答しない（現状の常設パスコード窓を閉じる）。CommissioningComplete / 窓タイムアウトで close
8. **PASE salt の乱数化 + iterations 引き上げ**（現状固定 salt・iterations 1000）

逐次ループの head-of-line blocking は **Echo の並列 exchange で顕在化した場合のみ**対応（YAGNI）。

**Phase D — Echo ゲート**

- despliegue で jarvis へ配備 → Alexa アプリ commission → アプリ操作 → 音声操作 → matv 側変化の反映確認 → matv 再起動試験。手動チェックリストを plan に明記

## chip-tool の調達（Phase A 冒頭の小 spike）

候補順: Docker イメージ（host network で mDNS/UDP を通す）→ prebuilt バイナリ → 自前ビルド。dev PAA は M1 の `--paa-dir` 資産をそのまま `--paa-trust-store-path` に渡す。調達方法が確定したら `task e2e:device:m2-chip`（ローカルゲート、CI 外可）に固める。

## テスト戦略

- 各プロトコル修正は TDD。mat-controller に initiator 側（wildcard read・subscribe 発行）が実装済みのため、多くは自己閉ループの統合テストが書ける
- **例外: CASE resumption**。controller の initiator は resumption を送らない（`case.rs` に「No optional fields (resumption...) are sent」と明記、Sigma2 の resumptionID も無視）。resumptionID の存在は unit テスト（TBE2 encode 検証）で担保し、resumption 要求 Sigma1 への fallback はテスト資産として controller 側に resumption 発行を足して閉ループ化する（`test_support` と同格の test 資産扱い）
- chip-tool ゲート・Echo ゲートは統合テストの外側の実機ゲートとして扱う（M1 の `e2e:device:m1` と同じ位置づけ）

## スコープ外（M2）

- Aggregator + bridged endpoints（M3）/ mando 転送・設定駆動の複数仮想デバイス（M4）
- BLE / BTP サーバ役 / Thread
- 並行セッション処理（head-of-line blocking は顕在化時のみ）
- Sigma2Resume による resumption 受理（fallback 拒否が観測された場合のみ）
- 申し送りの「クリーンアップ（急がない）」群（MRP 送信ループ 3 重複の共有化等）。触るファイルで自然に拾える分だけ拾う
- Alexa 以外のコントローラ（Apple Home / Google Home）の検証
