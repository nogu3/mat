# `mat diag node` — 到達不能の根本原因診断

- 日付: 2026-06-29
- ステータス: 設計確定（実装前）

## 背景と動機

commissioned 済みノードが制御できないとき、現状の `mat` は `invoke`/`read` が
`timeout` / `session_failed` 等の **kind を1つ返すだけ**で、「なぜ届かないか」を
構造的に説明しない。Home Assistant も同様に「利用不可」としか出さず、原因の切り分けは
人手の調査に依存する。

実機調査（弱リンクの commissioned ノードの照明）で、手動の層別切り分け
（**ping6 → mDNS ブラウズで自 fabric の `_matter._tcp` 照合 → chip-tool で
operational 解決 → 分類**）により、「**弱い Thread リンクで SRP 登録が完了せず
mDNS 広告ゼロ → 到達不能（fabric は無傷）**」と正確に特定できた。広告ゼロを
「fabric 削除」と早合点して工場リセットしていれば、HA/mat 両方の登録を失っていた。

この手動手順を `mat` の機能として確立し、**根本原因を `verdict` として分類**して返す。

## スコープ

### やること
- 新サブコマンド `mat diag node -n <node> [-e <ep>] [--deep]`
- 層別チェックを実施し、単一の最尤 `verdict` ＋ 根拠 `checks` ＋ `recommendation` を
  純 JSON で stdout に返す
- 部分結果を許容（取れたチェックだけ埋める）。完全不達でもエラーにせず JSON を返す

### やらないこと（YAGNI / スコープ外）
- 自動復旧アクションの実行（リセット・再コミッションの自動化はしない。推奨を出すだけ）
- 複数ノードの一括診断 / メッシュ地図化（上位層の責務）
- 人間向け装飾出力（色・プログレス等。CLAUDE.md ②）
- 候補 verdict の確度つき複数返し（単一最尤のみ。将来拡張余地として残す）

## 設計方針（ブレストで確定）

- **方針 C（ハイブリッド）**: 既定は chip-tool 完結（移植性優先、CLAUDE.md ① の精神を
  既定で守る）。`--deep` 指定時のみ補助プローブ（`ping6`・`avahi-browse`）を実施。
  補助ツールが環境に無ければ該当チェックを `unavailable` 扱い。
- **コマンド面 A**: `diag` ファミリ配下に `node` を追加（既存 `mat diag thread` と並ぶ）。

## コマンド面

`cli.rs` の `DiagCommand` enum に `Node` バリアントを追加:

```
mat diag node -n <node> [-e <ep>] [--deep]
```

- `-n/--node`: 必須。commissioned 済み node_id。
- `-e/--endpoint`: 既定 1（thread 診断は ep0 を内部で使う点は thread サブコマンド準拠）。
- `--deep`: ping6 / mDNS ブラウズを有効化。

## モジュール構成 / 再利用

- `crates/mat/src/commands/diag.rs` に `node()` を追加（既存 `thread()` と同居）。
- 再利用:
  - `mat_core::normalize::classify_failure`（既存）
  - `diag thread` の属性読みロジック（neighbor-table / routing-role）
  - `Store::nodes()`（台帳のアドレス取得）
- 新規純関数（`mat-core` 側、ユニットテスト対象）:
  - `parse_avahi_matter(stdout: &str) -> Vec<MatterInstance>`
    （`avahi-browse -rt _matter._tcp` 出力を `{compressed_fabric, node_id, address}` に）
  - `parse_ping6(stdout: &str) -> Ping6Stats { loss_pct, rtt_ms }`
  - `derive_verdict(checks: &Checks) -> Verdict { verdict, summary, recommendation }`

## 各チェックの取得方法

| check | 取得手段 | 既定 | `--deep` |
|---|---|---|---|
| `ip` | `ping6 -c N <台帳アドレス>` → `parse_ping6` | unavailable | 実施 |
| `mdns` | `avahi-browse -rt _matter._tcp` → `parse_avahi_matter`。`<自CFID>-<nodeid>` で自 fabric 判定、`-<nodeid>` 一致で any-fabric 判定 | unavailable | 実施 |
| `operational` | chip-tool で軽い read を1回試行 → `classify_failure` で成否/kind | 実施 | 実施 |
| `thread` | 既存 `diag thread` の neighbor-table(LQI)/routing-role を読む（部分結果可） | 実施 | 実施 |

### 自 CFID（compressed fabric id）の入手

mDNS の自 fabric 判定には自分の compressed fabric id が要る。chip-tool は起動時ログに
`Compressed FabricId 0x...` を出す（例: `<COMPRESSED-FABRIC-ID>`、16桁 hex）。
診断実行中に1回キャプチャして使う。取得できなければ mdns の
`advertised_self_fabric` は `null`（不明）とし、`advertised_any_fabric`（node_id 接尾辞
一致）だけで判定にあたる。

### operational チェックの実体

純粋な「解決のみ」を chip-tool は単体コマンドで提供しないため、**軽量な read を1回
試行**して `classify_failure` で分類する（`unresolvable`=timeout/0x32、
`session_failed`=0x54、`device_rejected`=IM failure、成功=`ok`）。読む属性は
`descriptor` の軽い属性など副作用のないものを選ぶ（実装時に確定）。

## verdict 導出（決定木・純関数 `derive_verdict`）

```
if operational.resolved == true:
    ok
elif deep and ip.ok == false:
    ip_unreachable
elif mdns.advertised_self_fabric == false:
    if mdns.advertised_any_fabric == true:
        fabric_missing            # 他 fabric では広告有・自 fabric だけ無 → 削除/外された
    elif thread が弱リンク or ip.loss が高い:
        link_starved              # どの fabric でも広告ゼロ＋弱リンク → SRP 未登録（今回）
    else:
        not_advertised            # 広告ゼロだが弱リンク根拠不足 → 汎用フォールバック
elif operational.kind == session_failed:
    session_failed
elif operational.kind == timeout:
    unresolvable                  # mDNS には在るが解決 timeout
elif operational.kind == device_rejected:
    device_rejected               # CASE OK だがコマンド拒否（endpoint/cluster/ACL）
else:
    unknown
```

- 「弱リンク」の閾値（例: best LQI が一定未満、neighbor 孤立、ip.loss ≥ 一定%）は
  実装時に定数化し、`derive_verdict` のユニットテストで固定。
- 既定（`--deep` 無し）は `ip`/`mdns` が `unavailable` のため `link_starved` /
  `fabric_missing` を**確定できない** → `unresolvable` / `not_advertised` 止まり。
  正確な区別には `--deep` が要る旨を `summary` に明記する。

### verdict → recommendation 対応（例）

| verdict | recommendation |
|---|---|
| `ok` | （制御可能のはず） |
| `ip_unreachable` | 電源 / Thread BR / ネットワーク経路を確認 |
| `link_starved` | Thread リンクを改善（ルーター近くへ）／待つ。**工場リセットしない（fabric は無傷）** |
| `fabric_missing` | マルチアドミン共有で再コミッション |
| `not_advertised` | `--deep` で再診断。電源入れ直し後に待つ |
| `unresolvable` | リトライ（一時的な mDNS/解決失敗） |
| `session_failed` | リトライ／認証（CASE）状態を確認 |
| `device_rejected` | endpoint / cluster / ACL を確認 |
| `unknown` | `checks` を確認（分類不能） |

## 出力スキーマ（stdout 純 JSON）

```json
{
  "node_id": 1,
  "endpoint": 1,
  "verdict": "link_starved",
  "summary": "IP reachable but not advertising Matter on any fabric; weak Thread link (50% loss) — SRP registration likely incomplete.",
  "checks": {
    "ip":   { "ok": true,  "loss_pct": 50, "rtt_ms": 168, "method": "ping6" },
    "mdns": { "advertised_self_fabric": false, "advertised_any_fabric": false },
    "operational": { "resolved": false, "kind": "timeout" },
    "thread": { "neighbor_count": 1, "best_lqi": 12, "routing_role": 2 }
  },
  "unavailable": [
    { "check": "...", "kind": "..." }
  ],
  "recommendation": "Improve the Thread link (move the device near a router) or wait; do NOT factory reset — the fabric is intact.",
  "timestamp": "2026-06-29T12:40:39+09:00"
}
```

- 既定（chip-tool 完結）では `checks.ip` / `checks.mdns` を省き、`unavailable` に
  `{check:"ip"|"mdns", kind:"skipped_no_deep"}` 等で記録。
- `timestamp` は ISO 8601（CLAUDE.md）。

## エラー処理 / 部分結果（`diag thread` 準拠）

- 取れたチェックだけ `checks` を埋め、取れないものは `unavailable`（理由 kind 付き）。
- stdout は純 JSON、診断ログは stderr（CLAUDE.md ②③）。`chip-tool`/`avahi`/`ping6` の
  stderr は debug で残す（呑み込まない）。
- **完全不達でもプロセスはエラー終了しない**。診断コマンドの価値は「必ず JSON で
  原因見解を返すこと」。`verdict` を得られた範囲の最尤（`ip_unreachable` /
  `unresolvable` 等）で返し、exit 0。
- 補助ツール（`avahi-browse`/`ping6`）不在は `--deep` 指定時のみ問題化し、当該 check を
  `unavailable`（kind=`tool_missing`）にして続行。

## テスト

- `derive_verdict` の決定木を**全分岐**ユニットテスト（`link_starved` / `fabric_missing`
  / `ip_unreachable` / `unresolvable` / `session_failed` / `device_rejected` / `ok` /
  `not_advertised` / `unknown`）。今回の実機ケース（ip ok・mdns 自/any ともに false・
  operational timeout・thread 弱リンク → `link_starved`）を回帰テストとして固定。
- `parse_avahi_matter` / `parse_ping6` の正常 + 異常（コマンド無し出力 / 空 / 想定外形）。
- fake-chip-tool 結合: operational timeout → `unresolvable`、成功 → `ok` 等。

## 影響範囲 / 非互換

- 既存サブコマンドの挙動は不変（新規追加のみ）。
- 新規依存は `--deep` 時のみの外部コマンド（`ping6` / `avahi-browse`）。既定経路は
  追加依存なし。README の「Errors and exit codes」「diag」節に `diag node` を追記。

## 未確定（実装時に確定する詳細）

- operational チェックで読む具体的な属性（副作用のない軽量属性）。
- 「弱リンク」判定の具体閾値（LQI/loss）。
- ping6 の回数・タイムアウト、avahi-browse のタイムアウト。
