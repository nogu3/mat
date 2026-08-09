# Tier 2: PASE / open-window / setup_code の入力検証 — 設計

2026-08-10。commission/証明書監査（2026-08-06）Tier 2 の 7 件を 1 ブランチで実装する。
対象はすべて `crates/mat-controller`（+ CLI 層のエラー変換のみ）。バージョンは 1.25.0。

## 背景

Tier 2 は「入力検証の穴」のまとめ。攻撃面は 2 系統:

1. **ネットワーク入力**（PASE は unsecured 交換なので on-path 偽造で成立）—
   PBKDFParamResponse の iterations/salt が無検証で PBKDF2 に直結し、
   `iterations=u32::MAX` で CLI が数時間ハング（current_thread ランタイムなので
   timeout も効かない）。逆に 0 なら弱 KDF がサイレント成立。
2. **CLI 引数**（自傷）— `open-window --iteration 4000000000` が invoke 送信前の
   `compute_verifier` で CPU 100% ハング。`--discriminator 0x1000` 超はデバイスが
   受理した場合 **window 開放後に** `setup_code.rs` の assert で panic し、
   ランダム生成 passcode が表示されないまま失われ、開いた window が放置される。

## 変更 1: PBKDFParamResponse 検証（pase.rs）

`establish` 内、`decode_pbkdf_param_response` 成功後・`derive_w0_w1`（PBKDF2）
実行前に検証する:

- iterations `1000..=100_000`（spec §3.9 CRYPTO_PBKDF_ITERATIONS_MIN/MAX）
- salt 長 `16..=32` バイト（同 §3.9）

違反時は cB 不一致経路（既存 pase.rs:363-380）と同じパターンで
StatusReport(FAILURE, SC_PROTOCOL_CODE_INVALID_PARAMETER) を `send_once` で
一度だけ送ってから `PaseError::Malformed(...)` を返す。黙って諦めると
responder が Pake1 待ちのまま PASE セッション確立スロットを保持し続け、
直後の再試行が固まる（cB 経路の実機 E2E で確認済みの挙動）ため。

検証は `decode_pbkdf_param_response` 自体ではなく `establish` 側に置く
（decode は形の検証、値域は利用側の責務という既存の分担を保つ）——ただし
単体テストの都合で `validate_pbkdf_params(iterations, salt_len) -> Result<(), &'static str>`
的な自由関数に切り出してよい（実装計画で確定）。

## 変更 2: PASE 小物 2 件（pase.rs）

- **responderSessionId == 0 の拒否** — `decode_pbkdf_param_response` で 0 を
  `PaseError::Malformed("responder session id must be non-zero")` にする。
  CASE 側（case.rs:232、Sigma2）の既存実装と対称化。
- **Pake3 応答の ack カウンタ検証** — PBKDFParamResponse（292 行）/ Pake2
  （338 行）と同じ
  `!transport.is_reliable() && m.proto.acked_counter != ex.last_sent_counter()`
  → `PaseError::NotAcked` を Pake3 応答受信（387 行の `Some(m)` 分岐）にも
  追加する。3 応答のうちここだけ欠落していた。

## 変更 3: open-window 引数検証（commissioning.rs、backend のみ）

`validate_window_params(discriminator: u16, iterations: u32) -> Result<(), CommissionError>`
を自由関数として切り出し、`open_commissioning_window` の冒頭
（`random_valid_passcode` より前）で呼ぶ:

- iterations `1000..=100_000`
- discriminator `<= 0x0FFF`

違反は新 variant **`CommissionError::InvalidArgument { what: &'static str }`**。
CLI 層（`mat` の CommissionError → MatError 変換）で `parse_error`（exit 1）に
マップする。invoke 送信前に弾くので「window 開放後に panic」という順序の
致命性が構造的に消える。

検証を backend に置く理由: CLI・live E2E ハーネスなど全呼び出し元を単一
チョークポイントで守る（ユーザー決定 2026-08-10）。CLI 側の clap 制約は
追加しない（検証ロジックを二重にしない）。

## 変更 4: setup_code 検証（setup_code.rs）

- **`parse_manual_code`**:
  - passcode `> 99_999_998` を拒否（現状 digits7_10 由来で 27bit 超過値が
    黙って通る。既存の `ZeroPasscode` 拒否と合わせ spec §5.1.3 の
    `1..=99_999_998` を完成させる）。
  - digit1 の vid_pid_present ビット（bit 2）が桁数と不整合なら拒否
    （11 桁なのに 1 / 21 桁なのに 0）。現状 `digit1 & 0x3` が bit 2 を黙殺。
  - `SetupCodeError` に必要な variant を追加（`PasscodeOutOfRange` 等、
    命名は実装時に既存 variant の流儀に合わせる）。
- **`base38_decode`**: チャンク値が表現可能範囲を超えたら非正準として拒否
  （5 文字 > 0xFF_FFFF / 4 文字 > 0xFFFF / 2 文字 > 0xFF。現状は下位ビットの
  黙殺 = 同一 payload に複数表現が存在してしまう）。

**やらないこと**: trivial passcode（12345678 等 `INVALID_PASSCODES`）の parse 時
拒否。spec の禁止は生成側（デバイス）への制約で、parse で弾くと行儀の悪い
実機の救済経路を潰す interop リスクだけが残る。生成側は既に
`random_valid_passcode` が回避済み。

## テスト（TDD）

- pase: iterations 999 / 100_001・salt 15B / 33B の拒否と境界値（1000 /
  100_000・16B / 32B）の受理。responderSessionId=0 の拒否。
  Pake3 ack 検証はオフライン応答器が無い（Tier 5 の既知ギャップ、
  PASE 成功経路テスト自体が未整備）ため既存パターンの移植のみで新規
  単体テストなし。
- commissioning: `validate_window_params` の境界値（純関数なので直接叩く）。
- setup_code: 27bit 超過 manual code（check digit は `verhoeff_check_digit` で
  正当に算出して作る）、vid_pid ビット不整合、base38 非正準チャンクの拒否。
  既知の正当コード（既存テストの QR / manual）が引き続き通ることの回帰。

## 進め方

- ブランチ 1 本。7 項目 ≥ 4 タスクなので subagent-driven-development で実行。
- `task check` 全緑 → マージ前に jarvis 実機 E2E（`*.new` で本番未置換のまま
  検証、既存規律どおり）。open-window とオンネットワーク commission の
  実機経路が退行していないことを確認する。

## 監査バックログとの対応

| 監査項目 | 本設計 |
|---|---|
| pase.rs:326 iterations/salt 無検証 | 変更 1 |
| commissioning.rs:1204 open-window iterations | 変更 3 |
| commissioning.rs:1232 discriminator > 0x0FFF | 変更 3 |
| pase.rs:157 responderSessionId=0 受理 | 変更 2 |
| pase.rs:387 Pake3 ack 検証欠落 | 変更 2 |
| setup_code.rs:318 manual code 桁制約 | 変更 4 |
| setup_code.rs:113 base38 非正準チャンク | 変更 4 |
