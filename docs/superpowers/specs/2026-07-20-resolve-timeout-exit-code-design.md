# mDNS resolve timeout の exit code 分類（0.23.x quality）

日付: 2026-07-20 / 対象: `mat-native::CaseEstablisher::establish`

## 問題

`CaseEstablisher::establish` は operational mDNS resolve の失敗を `map_err` で
一律 `ErrorKind::Unreachable`（exit 5）に丸めている。dnssd 層は
`DnssdError::Timeout` を区別して返しているのに variant を捨てているため、
「時間内に広告が取れなかっただけ（OTBR proxy は ~30 秒周期でしか広告しない
ので、リトライで通ることが多い）」と「socket I/O 等の構造的失敗」が呼び出し
側から区別できない。README の exit code 契約には 3=timeout が既にあり、
chip-tool 時代の直経路は discovery timeout を timeout(3) に分類していた。

## 決定（ユーザー承認 2026-07-20）

resolve 失敗を variant で振り分ける:

- `DnssdError::Timeout` → `ErrorKind::Timeout`（exit 3）
- それ以外（socket I/O 等）→ `ErrorKind::Unreachable`（exit 5、現状維持）

小関数 `dnssd_error_kind(&DnssdError) -> ErrorKind` に切り出して establish の
call site から使う。detail は従来の `native: mDNS resolve node N: ...` を維持。

## スコープ

- 対象は establish の operational resolve のみ。mat 直経路（一発 ~5s）と
  matd 経路（常駐キャッシュのミス時 35s）は同じ establish を通るため、両経路
  一貫して変わる（経路で分類が割れない性質は維持）。
- **触らない**: commission 経路（`CommissionError` の分類は Timeout→3 が既に
  正しく、resolve 失敗→BLE フォールバック境界は独自セマンティクス）/
  probe（設計上 exit 0 固定で verdict が値）/ group 送信。
- 新 kind は作らない（既存の安定 kind `timeout` を使う）。

## テスト

- `dnssd_error_kind` の単体テスト（Timeout→Timeout / それ以外→Unreachable）。
- 可能なら fake `Resolver` 注入（`build_with_resolver`）による establish
  レベルのテストで、resolve timeout が `ErrorKind::Timeout` の `MatError` に
  なることをピン。

## ドキュメント

README「Errors and exit codes」の native backend 段落に、resolve timeout→3 /
その他 resolve 失敗→5 の一行を追記。

## 呼び出し側への影響

これまで exit 5 だった resolve timeout が exit 3 になる（0.23.x パッチ）。
exit 3 は「リトライ・timeout 延長で回復し得る」既存セマンティクスの適用で
あり、新契約の追加はない。
