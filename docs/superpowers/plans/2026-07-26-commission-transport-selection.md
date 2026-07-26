# commission の経路選択（フォールバックと `--transport`）実装計画

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** mDNS で見つかった on-network 経路が PASE の MRP 予算を使い切って死んだとき、
BLE 経路へ自動で切り替える。あわせて `--transport` で経路を明示指定できるようにする。

**Architecture:** `mat-native/src/commission.rs` の `commission()` を「資材構築 →
発見（I/O）→ 経路計画（純関数）→ 経路実行ループ」に割る。経路計画 `plan_routes` は
mDNS の結果を引数で受け取る純関数なのでネットワーク無しで全分岐をテストできる。
「次の経路に進んでよい失敗」の判定は `CommissionError` の段で行う 1 つの述語
`is_dead_end` に集約する（`ErrorKind` に写した後では CASE timeout と PASE timeout が
区別できなくなるため）。

**Tech Stack:** Rust 2021 / tokio / clap 4（`ValueEnum` + `env`）/ `assert_cmd` +
`predicates`（CLI 統合テスト）

## Global Constraints

- 設計ルール（`CLAUDE.md`）: プロトコルコードは backend crate のみ / stdout は構造化
  JSON のみ / 診断は stderr の `tracing` / 状態は credential KVS 以外持たない。
- **stdout のスキーマを変更しない。** commission の成功出力は
  `{"timestamp","node_id","status"}` のまま。経路情報を JSON に足さない。
- `mat-native` は clap に依存しない。`Transport` は素の enum とし、clap の
  `ValueEnum` は `crates/mat/src/cli.rs` 側に置いて写す。
- `commission` は matd 経路を持たない direct-only op。socket プロトコルは変更しない。
- 候補経路が 1 本のときの `kind` / `detail` は現状と 1 文字も変えない（既存の
  エラー文言・exit code の互換性）。
- exit 2 を返せる `ErrorKind` は存在しない（`mat-core/src/error.rs:58-73`）。引数の
  矛盾は `main.rs` が `ExitCode::from(2)` を返す既存の形（`main.rs:36-45`）に合わせる。
- 各タスクの最後に `task check`（fmt:check + clippy -D warnings + test）が通ること。
- コミットは各タスク末尾で 1 つ。ブランチは `feat/commission-transport-selection`
  （作成済み、spec コミット `16a6180` が入っている）。

---

## File Structure

| ファイル | 役割 | 変更 |
|---|---|---|
| `crates/mat-controller/src/commissioning.rs` | `CommissionTarget` に derive 追加のみ | 修正 |
| `crates/mat-native/src/commission.rs` | 経路の型・計画・実行ループ・dead end 判定 | 修正（中心） |
| `crates/mat/src/cli.rs` | `--transport` の受け口と `validate_transport` | 修正 |
| `crates/mat/src/commands/commission.rs` | `transport` を `CommissionRequest` へ渡す | 修正 |
| `crates/mat/src/main.rs` | `Commission` の分解に `transport` を追加、引数矛盾で exit 2 | 修正 |
| `crates/mat/tests/integration.rs` | `--transport` の CLI テスト | 修正 |
| `README.md` | 経路選択の表・`--transport`・`MAT_TRANSPORT`・exit code の但し書き | 修正 |
| `Cargo.toml` | workspace version 1.3.0 → 1.4.0 | 修正 |

---

### Task 1: 経路の型と `plan_routes`（純関数）

**Files:**
- Modify: `crates/mat-controller/src/commissioning.rs:520-526`
- Modify: `crates/mat-native/src/commission.rs`（`Code` 定義の直後、35-39 行付近に追加）
- Test: `crates/mat-native/src/commission.rs` の `mod tests`（419 行以降）

**Interfaces:**
- Produces:
  - `pub enum Transport { Auto, OnNetwork, Ble }`（`Default` は `Auto`）
  - `enum Route { OnNetwork { target: CommissionTarget, label: String }, Ble }`
  - `fn Route::label(&self) -> String`
  - `enum Discovered { NotConsulted, Hit { target: CommissionTarget, label: String }, Miss }`
  - `fn plan_routes(code: &Code, transport: Transport, discovered: &Discovered) -> Result<Vec<Route>, MatError>`

- [ ] **Step 1: `CommissionTarget` に derive を足す**

`crates/mat-controller/src/commissioning.rs:520` の直前に derive を追加する。テストで
`Route` を比較するために必要（`SocketAddr` も `u16` も `Copy + Eq + Debug` なので通る）。

```rust
/// commissioning 対象デバイスの指定方法。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommissionTarget {
```

- [ ] **Step 2: 失敗するテストを書く**

`crates/mat-native/src/commission.rs` の `mod tests`（419 行の `#[cfg(test)] mod tests`）に
追加する。表の 9 分岐を全部固定する。

```rust
    fn qr() -> Code {
        Code::Qr {
            passcode: 20202021,
            long: 1082,
        }
    }

    fn manual() -> Code {
        Code::Manual {
            passcode: 20202021,
            short: 4,
        }
    }

    fn hit() -> Discovered {
        Discovered::Hit {
            target: CommissionTarget::Discriminator(1082),
            label: "on-network(2001:db8::1)".to_string(),
        }
    }

    #[test]
    fn plan_routes_auto_qr_hit_falls_back_to_ble() {
        let routes = plan_routes(&qr(), Transport::Auto, &hit()).unwrap();
        assert_eq!(routes.len(), 2);
        assert!(matches!(routes[0], Route::OnNetwork { .. }));
        assert!(matches!(routes[1], Route::Ble));
    }

    #[test]
    fn plan_routes_auto_qr_miss_is_ble_only() {
        let routes = plan_routes(&qr(), Transport::Auto, &Discovered::Miss).unwrap();
        assert_eq!(routes.len(), 1);
        assert!(matches!(routes[0], Route::Ble));
    }

    #[test]
    fn plan_routes_auto_manual_hit_is_on_network_only() {
        // manual code は long discriminator を持たず BLE scan に使えない。
        let routes = plan_routes(&manual(), Transport::Auto, &hit()).unwrap();
        assert_eq!(routes.len(), 1);
        assert!(matches!(routes[0], Route::OnNetwork { .. }));
    }

    #[test]
    fn plan_routes_auto_manual_miss_is_unreachable_with_qr_hint() {
        let e = plan_routes(&manual(), Transport::Auto, &Discovered::Miss).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Unreachable);
        assert!(e.detail.contains("manual code cannot use BLE"));
    }

    #[test]
    fn plan_routes_on_network_qr_hit_never_includes_ble() {
        let routes = plan_routes(&qr(), Transport::OnNetwork, &hit()).unwrap();
        assert_eq!(routes.len(), 1);
        assert!(matches!(routes[0], Route::OnNetwork { .. }));
    }

    #[test]
    fn plan_routes_on_network_qr_miss_is_unreachable() {
        let e = plan_routes(&qr(), Transport::OnNetwork, &Discovered::Miss).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Unreachable);
        assert!(e.detail.contains("on-network"));
    }

    #[test]
    fn plan_routes_on_network_manual_miss_is_unreachable_with_qr_hint() {
        let e = plan_routes(&manual(), Transport::OnNetwork, &Discovered::Miss).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Unreachable);
        assert!(e.detail.contains("manual code cannot use BLE"));
    }

    #[test]
    fn plan_routes_ble_qr_skips_mdns_entirely() {
        let routes = plan_routes(&qr(), Transport::Ble, &Discovered::NotConsulted).unwrap();
        assert_eq!(routes.len(), 1);
        assert!(matches!(routes[0], Route::Ble));
    }

    #[test]
    fn plan_routes_ble_manual_is_internal_error() {
        // CLI 層が exit 2 で弾く組み合わせ。native まで来たら内部エラー。
        let e = plan_routes(&manual(), Transport::Ble, &Discovered::NotConsulted).unwrap_err();
        assert_eq!(e.kind, ErrorKind::Other);
    }
```

- [ ] **Step 3: テストが落ちることを確認**

Run: `cargo test -p mat-native plan_routes`
Expected: FAIL（`cannot find function plan_routes` / `cannot find type Transport` 等のコンパイルエラー）

- [ ] **Step 4: 型と `plan_routes` を実装**

`crates/mat-native/src/commission.rs` の `Code` 定義（35-39 行）の直後に足す。

```rust
/// commission の経路指定。`mat` の `--transport` / `MAT_TRANSPORT` に対応する。
/// clap には依存しない（`ValueEnum` は `crates/mat/src/cli.rs` 側）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Transport {
    /// mDNS で見つかればまず on-network、PASE が MRP を使い切ったら BLE。
    #[default]
    Auto,
    /// mDNS のみ。BLE には落ちない。
    OnNetwork,
    /// mDNS を一切引かず BLE 直行。
    Ble,
}

/// mDNS を引いた結果。I/O は呼び出し側（`discover`）が済ませてから渡す
/// ——`plan_routes` を純関数に保つため。
enum Discovered {
    /// `--transport ble`: mDNS を引いていない。
    NotConsulted,
    Hit {
        target: CommissionTarget,
        /// ログ・エラー要約用のラベル（解決したアドレス等）。
        label: String,
    },
    Miss,
}

/// 試す経路の候補。
#[derive(Debug)]
enum Route {
    OnNetwork { target: CommissionTarget, label: String },
    Ble,
}

impl Route {
    fn label(&self) -> String {
        match self {
            Route::OnNetwork { label, .. } => label.clone(),
            Route::Ble => "ble".to_string(),
        }
    }
}

/// 試す順番を決める純関数。mDNS の I/O は含まない。
///
/// manual code は 4bit short discriminator しか持たず、BLE scan（12bit 完全
/// 一致）に使えないため BLE を候補に入れない。
fn plan_routes(
    code: &Code,
    transport: Transport,
    discovered: &Discovered,
) -> Result<Vec<Route>, MatError> {
    let is_qr = matches!(code, Code::Qr { .. });

    if transport == Transport::Ble {
        if !is_qr {
            return Err(MatError::new(
                ErrorKind::Other,
                "native commissioning: --transport ble requires the QR payload \
                 (manual code has no long discriminator) — should have been rejected by the CLI"
                    .to_string(),
            ));
        }
        return Ok(vec![Route::Ble]);
    }

    match discovered {
        Discovered::Hit { target, label } => {
            let mut routes = vec![Route::OnNetwork {
                target: *target,
                label: label.clone(),
            }];
            // auto かつ QR のときだけ BLE を後詰めする。これが本修正の中心:
            // mDNS のレコードは「存在」しか保証しないので、届かなかったときの
            // 逃げ道を計画に含めておく。
            if transport == Transport::Auto && is_qr {
                routes.push(Route::Ble);
            }
            Ok(routes)
        }
        Discovered::Miss if is_qr && transport == Transport::Auto => Ok(vec![Route::Ble]),
        Discovered::Miss if is_qr => Err(MatError::new(
            ErrorKind::Unreachable,
            "native commissioning: not found via mDNS (--transport on-network does not fall back to BLE)"
                .to_string(),
        )),
        Discovered::Miss => Err(MatError::new(
            ErrorKind::Unreachable,
            "native commissioning: not found via mDNS (manual code cannot use BLE; use the QR payload)"
                .to_string(),
        )),
        Discovered::NotConsulted => Err(MatError::new(
            ErrorKind::Other,
            "native commissioning: mdns was not consulted outside --transport ble (internal)"
                .to_string(),
        )),
    }
}
```

- [ ] **Step 5: テストが通ることを確認**

Run: `cargo test -p mat-native plan_routes`
Expected: PASS（9 件）

未使用警告が出る場合（`plan_routes` はまだ `commission()` から呼ばれない）は
`#[allow(dead_code)]` を付けず、Task 3 で配線するまでの一時的な状態として
`cargo clippy` の結果を確認する。警告が `-D warnings` で落ちるなら、Task 1 の
コミットに限り `plan_routes` / `Route` / `Discovered` に `#[allow(dead_code)]` を
付け、**Task 3 の Step で必ず外す**。

- [ ] **Step 6: コミット**

```bash
git add crates/mat-controller/src/commissioning.rs crates/mat-native/src/commission.rs
git commit -m "feat(commission): 経路計画 plan_routes を純関数として追加"
```

---

### Task 2: `is_dead_end` 述語

**Files:**
- Modify: `crates/mat-native/src/commission.rs`（`kind_of` の直後、94 行付近）
- Test: `crates/mat-native/src/commission.rs` の `mod tests`

**Interfaces:**
- Consumes: なし
- Produces: `fn is_dead_end(e: &CommissionError) -> bool`

- [ ] **Step 1: 失敗するテストを書く**

`mod tests` に追加する。**CASE timeout が `false` になることの固定が主目的**。

```rust
    #[test]
    fn dead_end_is_limited_to_pase_and_predelivery() {
        use mat_controller::commissioning::CommissionError as E;
        use mat_controller::exchange::ExchangeError;
        use mat_controller::pase::PaseError;

        // PASE の MRP 使い切り = 宛先が無言。デバイス側に状態は無い → 次の経路へ。
        assert!(is_dead_end(&E::Timeout("pase")));
        assert!(is_dead_end(&E::Pase(PaseError::Exchange(
            ExchangeError::Timeout
        ))));
    }

    #[test]
    fn case_timeout_is_not_a_dead_end() {
        use mat_controller::case::CaseError;
        use mat_controller::commissioning::CommissionError as E;
        use mat_controller::exchange::ExchangeError;

        // CASE まで来ているならデバイスは failsafe 中に我々の部分状態を持つ。
        // kind_of では Timeout に写るが、フォールバックしてはいけない。
        let e = E::Case(CaseError::Exchange(ExchangeError::Timeout));
        assert_eq!(kind_of(&e), ErrorKind::Timeout);
        assert!(!is_dead_end(&e));
    }

    #[test]
    fn device_rejection_is_not_a_dead_end() {
        use mat_controller::commissioning::CommissionError as E;
        use mat_controller::pase::PaseError;

        // 拒否は「届いている」証拠。別経路でも同じ理由で拒否される。
        assert!(!is_dead_end(&E::Pase(PaseError::ConfirmMismatch)));
        assert!(!is_dead_end(&E::CommandStatus {
            step: "add-noc",
            code: 1
        }));
    }
```

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat-native dead_end`
Expected: FAIL（`cannot find function is_dead_end`）

- [ ] **Step 3: 実装**

`kind_of` の直後に足す。

```rust
/// 「この失敗なら次の経路を試してよい」の判定。
///
/// 対象は **PASE の MRP 使い切り**と **PASE 直前の消失**だけ。PASE は最初の
/// 交換なので、ここで無言だったということはデバイス側に一切の状態が作られて
/// いない＝別経路でやり直しても中途状態と衝突しない。
///
/// attestation / NOC / CASE / 明示拒否は対象外。デバイスは failsafe 中に我々の
/// 部分状態を持っている可能性があり、自動で二度目を打ってはならない。判定を
/// `ErrorKind` ではなく `CommissionError` の段で行うのは、`kind_of` が
/// `Case(Exchange(Timeout))` も `Timeout` に写してしまい PASE と区別できなく
/// なるため。
fn is_dead_end(e: &CommissionError) -> bool {
    use mat_controller::exchange::ExchangeError;
    use mat_controller::pase::PaseError;
    matches!(
        e,
        CommissionError::Timeout("pase")
            | CommissionError::Pase(PaseError::Exchange(ExchangeError::Timeout))
            | CommissionError::Discovery(_)
    )
}
```

- [ ] **Step 4: テストが通ることを確認**

Run: `cargo test -p mat-native dead_end`
Expected: PASS（3 件）

- [ ] **Step 5: コミット**

```bash
git add crates/mat-native/src/commission.rs
git commit -m "feat(commission): 次の経路へ進んでよい失敗の判定を is_dead_end に集約"
```

---

### Task 3: `commission()` を経路実行ループへ書き換え

**Files:**
- Modify: `crates/mat-native/src/commission.rs:26-33`（`CommissionRequest`）、
  `187-340`（`commission`）、`354-401`（`ble_path`）
- Test: `crates/mat-native/src/commission.rs` の `mod tests`

**Interfaces:**
- Consumes: Task 1 の `Transport` / `Route` / `Discovered` / `plan_routes`、Task 2 の `is_dead_end`
- Produces:
  - `CommissionRequest` に `pub transport: Transport` フィールドが増える（Task 4 が設定する）
  - `fn short_detail(d: &str) -> &str`
  - `fn compose_failure(failures: &[String], last: MatError) -> MatError`

- [ ] **Step 1: 失敗するテストを書く（エラー合成）**

`mod tests` に追加する。

```rust
    #[test]
    fn single_route_failure_is_reported_verbatim() {
        // 候補が 1 本なら現状と 1 文字も変わらない（互換性）。
        let last = MatError::new(ErrorKind::Timeout, "native commissioning failed: x".to_string());
        let out = compose_failure(&["ble: native commissioning failed: x".to_string()], last);
        assert_eq!(out.kind, ErrorKind::Timeout);
        assert_eq!(out.detail, "native commissioning failed: x");
    }

    #[test]
    fn multi_route_failure_lists_every_route() {
        let last = MatError::new(
            ErrorKind::Unreachable,
            "native commissioning: no dataset".to_string(),
        );
        let out = compose_failure(
            &[
                "on-network(2001:db8::1): pase timed out".to_string(),
                "ble: no dataset".to_string(),
            ],
            last,
        );
        // kind は最後に試した経路のものを採る。
        assert_eq!(out.kind, ErrorKind::Unreachable);
        assert!(out.detail.starts_with("native commissioning: all routes failed — "));
        assert!(out.detail.contains("on-network(2001:db8::1): pase timed out"));
        assert!(out.detail.contains("ble: no dataset"));
    }

    #[test]
    fn short_detail_strips_known_prefixes() {
        assert_eq!(short_detail("native commissioning failed: boom"), "boom");
        assert_eq!(short_detail("native commissioning: boom"), "boom");
        assert_eq!(short_detail("boom"), "boom");
    }
```

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat-native compose_failure short_detail`
Expected: FAIL（`cannot find function compose_failure`）

- [ ] **Step 3: `CommissionRequest` に `transport` を足す**

`crates/mat-native/src/commission.rs:26-33` を置き換える。

```rust
/// [`commission`] への入力一式。
pub struct CommissionRequest {
    pub setup_code: String,
    pub device_node_id: u64,
    /// hex デコード済みの Thread active operational dataset（BLE 経路用）。
    pub thread_dataset: Option<Vec<u8>>,
    pub paa_dir: Option<std::path::PathBuf>,
    pub cd_signer_dir: Option<std::path::PathBuf>,
    /// 経路指定（既定 `Auto`）。
    pub transport: Transport,
}
```

- [ ] **Step 4: 補助関数を実装**

`compose_failure` / `short_detail` を `commission_error` の直後（96 行付近）に足す。

```rust
/// 経路要約に載せるための短縮。`MatError::detail` は経路ごとに
/// `native commissioning...` の接頭辞を持つので、並べるときは畳む。
fn short_detail(d: &str) -> &str {
    d.strip_prefix("native commissioning failed: ")
        .or_else(|| d.strip_prefix("native commissioning: "))
        .unwrap_or(d)
}

/// 全経路が尽きたときのエラー。`kind` は最後に試した経路のものを採り、
/// `detail` に経路ごとの結果を並べる。候補が 1 本だったときは現状の
/// `MatError` をそのまま返す（表現の互換性）。
fn compose_failure(failures: &[String], last: MatError) -> MatError {
    if failures.len() <= 1 {
        return last;
    }
    MatError::new(
        last.kind,
        format!(
            "native commissioning: all routes failed — {}",
            failures.join("; ")
        ),
    )
}
```

- [ ] **Step 5: テストが通ることを確認**

Run: `cargo test -p mat-native compose_failure short_detail`
Expected: PASS（3 件）

- [ ] **Step 6: 発見（I/O）を `discover` に切り出す**

現状 `commission()` 内の 233-295 行（`match code { ... }`）を、以下の関数に移す。
`commission()` からはこの範囲を削除する。

```rust
/// mDNS 発見（I/O）。結果を `Discovered` に畳んで `plan_routes` に渡す。
/// resolve/browse の失敗（timeout 以外）は現状どおりその場で `unreachable`。
async fn discover(code: &Code, scope_id: u32) -> Result<Discovered, MatError> {
    match code {
        Code::Qr { long, .. } => {
            match dnssd::resolve_commissionable(scope_id, *long, std::time::Duration::from_secs(5))
                .await
            {
                Ok(rn) => {
                    let label = match rn.addresses.first() {
                        Some(a) => format!("on-network({a})"),
                        None => format!("on-network(disc {long})"),
                    };
                    Ok(Discovered::Hit {
                        // 現状どおり Discriminator を渡す（commission_on_network が
                        // 内部で再 resolve する）。label は表示専用。
                        target: CommissionTarget::Discriminator(*long),
                        label,
                    })
                }
                Err(dnssd::DnssdError::Timeout { .. }) => Ok(Discovered::Miss),
                Err(e) => Err(MatError::new(
                    ErrorKind::Unreachable,
                    format!("native commissioning: mdns resolve: {e}"),
                )),
            }
        }
        Code::Manual { short, .. } => {
            let list = match dnssd::browse_commissionable(scope_id, dnssd::BROWSE_WINDOW).await {
                Ok(l) => l,
                Err(e) => {
                    return Err(MatError::new(
                        ErrorKind::Unreachable,
                        format!("native commissioning: mdns browse: {e}"),
                    ))
                }
            };
            match pick_by_short_strict(&list, *short)? {
                Some(c) => {
                    let Some(addr) = c.addresses.first() else {
                        return Err(MatError::new(
                            ErrorKind::Unreachable,
                            "native commissioning: commissionable found but no address resolved"
                                .to_string(),
                        ));
                    };
                    let port = c.port.unwrap_or(5540);
                    let scope = if (addr.segments()[0] & 0xffc0) == 0xfe80 {
                        scope_id
                    } else {
                        0
                    };
                    Ok(Discovered::Hit {
                        target: CommissionTarget::Addr(std::net::SocketAddr::V6(
                            std::net::SocketAddrV6::new(*addr, port, 0, scope),
                        )),
                        label: format!("on-network({addr})"),
                    })
                }
                None => Ok(Discovered::Miss),
            }
        }
    }
}
```

- [ ] **Step 7: 経路実行を `run_route` に切り出す**

`commission()` 内の 297-333 行（UDP bind + `commission_on_network`）を移し、BLE 側と
同じ戻り値型に揃える。`ble_path`（354-401 行）は `run_route` から呼ばれる形に直す
（シグネチャは維持し、戻り値だけ `RouteFail` を返すように包む）。

```rust
/// 1 経路の実行結果。`dead_end` が true なら次の経路へ進んでよい。
struct RouteFail {
    err: MatError,
    dead_end: bool,
}

async fn run_route(
    route: &Route,
    fabric: &CommissioningFabric,
    req: &CommissionRequest,
    scope_id: u32,
    passcode: u32,
    long: Option<u16>,
) -> Result<(), RouteFail> {
    match route {
        Route::OnNetwork { target, .. } => {
            let transport = match UdpTransport::bind().await {
                Ok(t) => std::sync::Arc::new(t),
                Err(e) => {
                    return Err(RouteFail {
                        err: MatError::new(
                            ErrorKind::Other,
                            format!("native commissioning: udp bind: {e}"),
                        ),
                        dead_end: false,
                    })
                }
            };
            match commissioning::commission_on_network(
                transport,
                fabric,
                CommissionParams {
                    passcode,
                    target: *target,
                    device_node_id: req.device_node_id,
                    paa_dir: req.paa_dir.as_deref(),
                    cd_signer_dir: req.cd_signer_dir.as_deref(),
                    scope_id,
                },
            )
            .await
            {
                Ok(dev) => {
                    tracing::info!(
                        node_id = dev.node_id,
                        fabric_index = ?dev.fabric_index,
                        "commission executed (native on-network)"
                    );
                    Ok(())
                }
                Err(CommissionError::Discovery(e)) => Err(RouteFail {
                    err: MatError::new(
                        ErrorKind::Unreachable,
                        format!("native commissioning: commissionable disappeared before PASE: {e}"),
                    ),
                    dead_end: true,
                }),
                Err(other) => {
                    let dead_end = is_dead_end(&other);
                    Err(RouteFail {
                        err: commission_error(other),
                        dead_end,
                    })
                }
            }
        }
        Route::Ble => {
            // BLE は QR コードでしか計画に載らない（plan_routes）。
            let long = long.expect("ble route is planned only for QR codes");
            ble_path(fabric, req, passcode, long, scope_id)
                .await
                .map_err(|err| RouteFail {
                    err,
                    dead_end: false,
                })
        }
    }
}
```

`ble_path` 本体（354-401 行）は戻り値 `Result<(), MatError>` のまま変更しない。

- [ ] **Step 8: `commission()` 本体をループに書き換える**

187 行からの本体のうち、資材構築（188-231 行）はそのまま残し、233 行以降を置き換える。

```rust
    // 発見（I/O）。--transport ble は mDNS を一切引かない。
    let discovered = if req.transport == Transport::Ble {
        Discovered::NotConsulted
    } else {
        discover(&code, scope_id).await?
    };

    let routes = plan_routes(&code, req.transport, &discovered)?;
    let (passcode, long) = match code {
        Code::Qr { passcode, long } => (passcode, Some(long)),
        Code::Manual { passcode, .. } => (passcode, None),
    };

    let plan: Vec<String> = routes.iter().map(Route::label).collect();
    tracing::info!(
        transport = ?req.transport,
        routes = ?plan,
        "commission route plan"
    );

    let mut failures: Vec<String> = Vec::new();
    for (i, route) in routes.iter().enumerate() {
        let label = route.label();
        match run_route(
            route,
            &commissioning_fabric,
            req,
            scope_id,
            passcode,
            long,
        )
        .await
        {
            Ok(()) => return Ok(()),
            Err(RouteFail { err, dead_end }) => {
                failures.push(format!("{label}: {}", short_detail(&err.detail)));
                let next = routes.get(i + 1);
                match (dead_end, next) {
                    (true, Some(n)) => {
                        tracing::warn!(
                            from = %label,
                            to = %n.label(),
                            reason = %err.detail,
                            "route dead end — falling back"
                        );
                        continue;
                    }
                    _ => return Err(compose_failure(&failures, err)),
                }
            }
        }
    }
    // plan_routes は空の Vec を返さない（返すなら Err）。
    Err(MatError::new(
        ErrorKind::Other,
        "native commissioning: empty route plan (internal)".to_string(),
    ))
```

- [ ] **Step 9: Task 1 で付けた `#[allow(dead_code)]` を外す**

Task 1 Step 5 で暫定的に付けた場合のみ。付けていなければ何もしない。

- [ ] **Step 10: ビルドとテスト**

Run: `cargo test -p mat-native`
Expected: PASS（`crates/mat/src/commands/commission.rs` が `CommissionRequest` を
`transport` 無しで構築しているためワークスペース全体はまだ壊れている。ここでは
`-p mat-native` のみ通ればよい）

- [ ] **Step 11: コミット**

```bash
git add crates/mat-native/src/commission.rs
git commit -m "feat(commission): 経路を順に試すループへ書き換え（PASE timeout で BLE へ）"
```

---

### Task 4: CLI の `--transport` と引数検証

**Files:**
- Modify: `crates/mat/src/cli.rs:80-101`（`Command::Commission`）
- Modify: `crates/mat/src/commands/commission.rs:20-43`（`run` のシグネチャと `CommissionRequest` 構築）
- Modify: `crates/mat/src/main.rs:29-31`（検証）、`152-166`（dispatch）
- Test: `crates/mat/tests/integration.rs`

**Interfaces:**
- Consumes: Task 3 の `CommissionRequest { transport }`、Task 1 の `mat_native::commission::Transport`
- Produces:
  - `pub enum TransportArg { Auto, OnNetwork, Ble }`（clap `ValueEnum`）と
    `fn TransportArg::to_native(self) -> mat_native::commission::Transport`
  - `pub fn validate_transport(t: TransportArg, setup_code: &str) -> Result<(), MatError>`
  - `commands::commission::run` に `transport: TransportArg` 引数が増える

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat/tests/integration.rs` の CLI 引数エラー節（50 行付近以降）に追加する。

```rust
#[test]
fn transport_ble_rejects_manual_code() {
    // BLE scan は 12bit 完全一致が要る。manual code は 4bit しか持たない。
    let store = store_with_node5();
    mat(store.path())
        .args([
            "commission",
            "--target",
            "192.0.2.10",
            "--setup-code",
            "34970112332",
            "--transport",
            "ble",
        ])
        .assert()
        .code(2)
        .stderr(predicate::str::contains("QR"));
}

#[test]
fn transport_rejects_unknown_value() {
    let store = store_with_node5();
    mat(store.path())
        .args([
            "commission",
            "--target",
            "192.0.2.10",
            "--setup-code",
            "MT:Y.K9042C00KA0648G00",
            "--transport",
            "carrier-pigeon",
        ])
        .assert()
        .code(2);
}
```

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat --test integration transport_`
Expected: FAIL（`--transport` は未知の引数なので clap が exit 2 を返し、1 件目の
`stderr` に "QR" が含まれず落ちる。2 件目はたまたま通る可能性があるが、それでよい）

- [ ] **Step 3: `cli.rs` に `--transport` を足す**

`crates/mat/src/cli.rs` の `Command::Commission`（80-101 行）の
`thread_dataset` フィールドの直後に追加する。

```rust
        /// 経路指定。auto（既定）は mDNS で見つかればまず on-network を試し、
        /// PASE が MRP 予算を使い切ったら BLE に切り替える。on-network は BLE に
        /// 落ちない。ble は mDNS を引かず BLE 直行（QR ペイロード必須）。
        #[arg(
            long = "transport",
            env = "MAT_TRANSPORT",
            value_enum,
            default_value_t = TransportArg::Auto,
            value_name = "MODE"
        )]
        transport: TransportArg,
```

ファイル末尾（`Command` enum の外）に足す。

```rust
/// `--transport` の受け口。`mat-native` は clap に依存しないため、CLI 側に
/// `ValueEnum` を置いて `mat_native::commission::Transport` へ写す。
#[derive(clap::ValueEnum, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TransportArg {
    #[default]
    Auto,
    #[value(name = "on-network")]
    OnNetwork,
    Ble,
}

impl TransportArg {
    pub fn to_native(self) -> mat_native::commission::Transport {
        use mat_native::commission::Transport;
        match self {
            TransportArg::Auto => Transport::Auto,
            TransportArg::OnNetwork => Transport::OnNetwork,
            TransportArg::Ble => Transport::Ble,
        }
    }
}

/// `--transport ble` は QR ペイロード（`MT:`）でしか成立しない。矛盾は CLI 引数
/// エラー（exit 2）として `main` が扱う。
pub fn validate_transport(t: TransportArg, setup_code: &str) -> Result<(), MatError> {
    if t == TransportArg::Ble && !setup_code.starts_with("MT:") {
        return Err(MatError::new(
            ErrorKind::Other,
            "--transport ble requires the QR payload (MT:...) — a manual code has no long \
             discriminator for the BLE scan"
                .to_string(),
        ));
    }
    Ok(())
}
```

`cli.rs` の先頭に `use mat_core::error::{ErrorKind, MatError};` が無ければ追加する。

- [ ] **Step 4: `main.rs` を配線**

`Cli::parse()`（29 行）の直後、`Store::locate`（31 行）の前に検証を挿入する。

```rust
    // `--transport ble` × manual code は成立しない組み合わせ。clap では表現できない
    // ので、ここで引数エラー（exit 2）として弾く。
    if let Command::Commission {
        transport,
        setup_code,
        ..
    } = &args.command
    {
        if let Err(e) = cli::validate_transport(*transport, setup_code) {
            e.emit();
            return ExitCode::from(2);
        }
    }
```

dispatch（152-166 行）に `transport` を追加する。

```rust
        Command::Commission {
            target,
            setup_code,
            node_id,
            alias,
            thread_dataset,
            transport,
        } => commands::commission::run(
            &store_path,
            target,
            setup_code,
            *node_id,
            alias.as_deref(),
            native_cfg.as_ref(),
            thread_dataset.as_deref(),
            *transport,
        ),
```

- [ ] **Step 5: `commands/commission.rs` を配線**

`run`（20-43 行）と `native_commission`（45-79 行）に `transport` を通す。

```rust
pub fn run(
    store_path: &Path,
    target: &str,
    setup_code: &str,
    node_id: Option<u64>,
    alias: Option<&str>,
    native: Option<&crate::native_direct::Config<'_>>,
    thread_dataset: Option<&str>,
    transport: crate::cli::TransportArg,
) -> Result<(), MatError> {
```

`native_commission` 呼び出しに `transport` を渡し、`CommissionRequest` の構築
（64-70 行付近）に足す。

```rust
        transport: transport.to_native(),
```

- [ ] **Step 6: テストが通ることを確認**

Run: `cargo test -p mat --test integration transport_`
Expected: PASS（2 件）

Run: `cargo test`
Expected: PASS（ワークスペース全体）

- [ ] **Step 7: コミット**

```bash
git add crates/mat/src/cli.rs crates/mat/src/main.rs crates/mat/src/commands/commission.rs crates/mat/tests/integration.rs
git commit -m "feat(commission): --transport / MAT_TRANSPORT を追加"
```

---

### Task 5: README とバージョン

**Files:**
- Modify: `README.md`（「Discover and commissioning」57 行付近、環境変数表 1232-1245 行、
  「Errors and exit codes」1100 行付近）
- Modify: `Cargo.toml:6`

**Interfaces:**
- Consumes: Task 4 の `--transport` / `MAT_TRANSPORT`
- Produces: なし

- [ ] **Step 1: 経路選択の説明を追加**

`README.md` の commission の例（68 行付近）の直後に節を足す。

````markdown
#### Route selection (`--transport`)

`commission` picks how to reach the device. mDNS finding a record only proves the
record exists — a Thread device's SRP registration outlives the device's
reachability — so `auto` keeps BLE as a fallback:

| `--transport` | QR payload (`MT:`) | manual code |
|---|---|---|
| `auto` (default) | mDNS hit → on-network, then BLE if PASE times out; miss → BLE | mDNS hit → on-network; miss → `unreachable` |
| `on-network` | mDNS only; never falls back to BLE | same |
| `ble` | skips mDNS entirely | rejected (exit `2`) |

The fallback fires **only** when PASE exhausts its MRP retry budget (or the device
disappears before PASE) — i.e. when nothing was ever established on the device. A
failure after PASE (attestation, NOC, CASE) stops immediately: the device holds
partial state under its failsafe and must not be re-driven automatically.

A manual code carries only a 4-bit short discriminator, which cannot drive the
12-bit BLE scan; use the QR payload for BLE.

```bash
# force BLE (skip mDNS entirely)
mat commission --target thread --setup-code "MT:Y.K9042C00KA0648G00" --transport ble
```
````

- [ ] **Step 2: 環境変数表に追記**

`README.md:1242` の `MAT_THREAD_DATASET` 行の直後に足す。

```markdown
| `MAT_TRANSPORT` | commission route: `auto` (default) / `on-network` / `ble` |
```

- [ ] **Step 3: exit code の但し書きを追加**

`README.md` の「Errors and exit codes」（1100 行）の表の直後に 1 段落足す。

```markdown
When `commission` tries more than one route, the reported `kind` and exit code are
those of the **last** route attempted, and `detail` lists every route's result.
```

- [ ] **Step 4: バージョンを上げる**

`Cargo.toml:6` を `version = "1.4.0"` にする。

- [ ] **Step 5: 検証**

Run: `task check`
Expected: PASS（fmt:check + clippy -D warnings + test）

- [ ] **Step 6: コミット**

```bash
git add README.md Cargo.toml
git commit -m "docs(commission): 経路選択と --transport を README に記載（1.4.0）"
```

---

### Task 6: 実機 E2E（jarvis、マージ前に必須）

**Files:** なし（実機での確認のみ）

**Interfaces:**
- Consumes: Task 5 までの全成果

実機 E2E はこのリポジトリの規律（マージ前に必ず jarvis 実機で確認、本番バイナリは
`*.new` で置かず検証）に従う。

- [ ] **Step 1: デプロイ用ビルド**

Run: `task dist:arm64`
Expected: `dist/arm64/{mat,matd}` が生成される（gnu + BLE feature 付き）

- [ ] **Step 2: jarvis へ転送（本番を置き換えない）**

```bash
scp dist/arm64/mat jarvis:~/mat.new
ssh jarvis 'chmod +x ~/mat.new && ~/mat.new --version'
```
Expected: `mat 1.4.0`

- [ ] **Step 3: 回帰の要点 — 既定 `auto` で新品が通ること**

工場出荷状態（または factory reset 直後）の機体を 1 台用意し、SRP レコードが無い
状態で既定のまま commission する。**この経路が壊れていないことが最重要の回帰確認。**

```bash
ssh jarvis 'DS=$(sudo ot-ctl dataset active -x | tr -d "\r" | grep -Eo "^[0-9a-fA-F]{16,}$" | head -1); \
  MAT_LOG=info MAT_FABRIC_INDEX=2 MAT_PAA_TRUST_STORE=/home/jarvis/paa-certs \
  MAT_THREAD_DATASET="$DS" ~/mat.new commission --target thread \
    --setup-code "MT:..." --alias <name>'
```
Expected: `commission route plan` の `routes` が `["ble"]`、成功して
`{"node_id":N,"status":"success"}`

- [ ] **Step 4: `--transport ble` が効くこと**

Step 3 と同じコマンドに `--transport ble` を足して、mDNS を引かずに BLE 直行する
ことを確認する（`MAT_IFACE=wpan0` のハック無しで通ること）。
Expected: `commission route plan` の `routes` が `["ble"]`、成功

- [ ] **Step 5: `--transport on-network` が BLE に落ちないこと**

commission 済みでない機体に対し `--transport on-network` を指定し、mDNS に居ない
場合に BLE を試さず即エラーになることを確認する。
Expected: `unreachable`（exit 5）、detail に `--transport on-network does not fall back to BLE`

- [ ] **Step 6: フォールバックの実地確認（可能なら）**

古い SRP レコードが残っている機体があれば、既定 `auto` で
`on-network → dead end → ble` と切り替わり成功することを確認する。
Expected: stderr に `route dead end — falling back from=on-network(...) to=ble`、
その後 `commission executed (native ble-thread)` と成功 JSON

この状況は意図的には作りにくい（一度 BLE で Thread 参加まで進んで CASE で失敗した
機体が必要）。**再現できない場合は Step 6 を skip し、その旨を報告すること**
——Task 1・3 のユニットテストが分岐自体は固定している。作り話で「確認した」と
書かないこと。

- [ ] **Step 7: 結果を報告**

Step 3-6 の実際の出力を添えて報告する。skip した Step があればそれも明記する。
本番バイナリの置き換え（`~/mat.new` → `~/.cargo/bin/mat`）は**マージ後**に行う。

---

## Self-Review

**Spec coverage:**

| spec の節 | 対応タスク |
|---|---|
| 設計 1（経路計画と実行の分離） | Task 1（計画）、Task 3（実行） |
| 設計 1 の表（6 マス） | Task 1 Step 2（9 分岐に展開してテスト） |
| `--transport ble` × manual = exit 2 | Task 4 Step 1/3/4 |
| 設計 2（`is_dead_end`、CASE timeout は対象外） | Task 2 |
| 設計 3（全経路失敗時のエラー合成） | Task 3 Step 1/4 |
| 設計 4（診断出力、stdout 不変） | Task 3 Step 8（`route plan` / `falling back`） |
| テスト節 | Task 1・2・3・4 の各テスト、Task 6 の実機 |
| README 節 | Task 5 |
| 互換性（1 本のときの文言不変） | Task 3 Step 1 の `single_route_failure_is_reported_verbatim` |
| バージョン 1.4.0 | Task 5 Step 4 |

**確認済みの前提（実装者は再調査不要）:**

- `crates/mat/Cargo.toml:19` に `mat-native.workspace = true` があり、`clap` も
  依存済み（20 行）。`crates/mat/src/cli.rs` から
  `mat_native::commission::Transport` を直接参照できる。
- `crates/mat/src/cli.rs` は `mat_core::alias` しか import していない。Task 4 Step 3 の
  `validate_transport` を書くには **`use mat_core::error::{ErrorKind, MatError};` の
  追加が必要**。
- `MatError` に `emit()` があり、`main.rs:36-45` が「`StoreParse` 以外は exit 2」の
  前例になっている。Task 4 Step 4 はこの形を踏襲する。
- exit 2 を返す `ErrorKind` は存在しないため、`validate_transport` の `ErrorKind` は
  何でもよい（`main.rs` が握り潰して `ExitCode::from(2)` を返す）。`Other` を使う。

**実装者が現物を見て決めてよい点:**

- `Code` に derive が必要になるか（`plan_routes` が `&Code` を取るだけなら不要）。
  必要なら足す。
