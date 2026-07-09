# `mat group provision --rebind` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 既存 wire group へのノード追加を `mat group provision --rebind` 一発でできるようにする（issue #5、spec: `docs/superpowers/specs/2026-07-09-group-provision-rebind-design.md`）。

**Architecture:** controller 側 groupsettings は永続化されており、既存グループへの provision 再実行は bind-keyset が `Duplicate key id` で失敗する。`--rebind` は bind-keyset の**直前**に `groupsettings unbind-keyset <group_id> <keyset_id>` を **best-effort**（失敗を一切無視、debug ログのみ）で挿入する。unbind が本当に必要で失敗したケースは直後の bind-keyset が従来どおり落ちるので検知はそちらに委ねる — これで未 bind の新規グループでも冪等に成功する。直経路（mat 単体）と matd 経路の両方に同じ step を入れる。

**Tech Stack:** Rust (clap / serde / tokio)、fake-chip-tool.sh（mat 統合テスト）、fake ws サーバ（matd 統合テスト）。

## Global Constraints

- `--rebind` 無しの既存挙動は**完全不変**（bind 済みグループへの再実行は失敗のまま。誤って鍵を回す事故の防護）。
- stdout は純 JSON のみ。診断は stderr へ `tracing`（CLAUDE.md 出力規約）。
- unbind の失敗は exit code も classify_failure も見ずに無視する（エラー形状は実機未確定のため依存しない）。
- `Op::GroupProvision` の新フィールドは `#[serde(default)] rebind: bool`（旧 mat → 新 matd 後方互換）。
- コミット前に `task check`（fmt:check + clippy -D warnings + test）を通す。
- コミットは各タスクで自分が編集したファイルのみ `git add`（ユーザー CLAUDE.md 規約）。

---

### Task 1: 直経路の `--rebind`（CLI + group.rs + fake-chip-tool + mat 統合テスト）

**Files:**
- Modify: `crates/mat/src/cli.rs`（`GroupCommand::Provision` に `rebind` フラグ、283 行付近）
- Modify: `crates/mat/src/main.rs`（`GroupCommand::Provision` の分配、156 行付近）
- Modify: `crates/mat/src/commands/group.rs`（`provision()` に unbind step と出力 note、30 行付近）
- Modify: `crates/mat/src/matd_client.rs`（`GroupCommand::Provision` の分配に `rebind` 追加 — enum 変更でコンパイルが要求する。op JSON にも `"rebind"` を常時載せる。228 行付近と 477 行付近のユニットテスト）
- Modify: `crates/mat/tests/fixtures/fake-chip-tool.sh`（`groupsettings` ハンドラ、90 行付近）
- Test: `crates/mat/tests/integration.rs`（Phase 3: groupcast 節、`group_provision_broken_acl_read_is_parse_error_without_write` の後ろに追加）

**Interfaces:**
- Consumes: 既存の `run_step` / `ChipTool::run` / `output::emit`（`group.rs` 内）。
- Produces: `commands::group::provision(store_path, group_id, node_ids, keyset_id, name, endpoint, epoch_key, rebind: bool)` — 末尾に `rebind: bool` を追加した 8 引数。matd_client の op JSON に `"rebind": <bool>` フィールド（Task 2 の matd がこれを読む）。

- [ ] **Step 1: 失敗するテストを書く**

`crates/mat/tests/integration.rs` の `group_provision_broken_acl_read_is_parse_error_without_write` の直後に 3 テストを追加:

```rust
#[test]
fn group_provision_rebind_unbinds_before_bind() {
    // bind 済み controller（FAKE_GROUP_BOUND=1）でも --rebind なら
    // unbind-keyset → bind-keyset の順で成功する（issue #5: 既存グループへの
    // ノード追加）。直経路の rebind は matd 再起動が必要なので note が出る。
    let store = store_with_node5();
    let args_file = store.path().join("recorded-args.txt");
    mat(store.path())
        .env("FAKE_CHIP_ARGS_FILE", &args_file)
        .env("FAKE_GROUP_BOUND", "1")
        .args([
            "group", "provision", "--group", "1", "--nodes", "5", "--rebind",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"status\":\"provisioned\""))
        .stdout(predicate::str::contains("restart"));
    let recorded = std::fs::read_to_string(&args_file).unwrap();
    let unbind = recorded
        .find("groupsettings unbind-keyset 1 42")
        .expect("unbind-keyset call missing");
    let bind = recorded
        .find("groupsettings bind-keyset 1 42")
        .expect("bind-keyset call missing");
    assert!(unbind < bind, "unbind must run before bind: {recorded}");
}

#[test]
fn group_provision_rebind_on_unbound_group_succeeds() {
    // 未 bind（新規グループ）でも --rebind 付きで成功する: unbind の失敗は
    // best-effort で無視される（冪等）。
    let store = store_with_node5();
    mat(store.path())
        .args([
            "group", "provision", "--group", "1", "--nodes", "5", "--rebind",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("\"status\":\"provisioned\""));
}

#[test]
fn group_provision_without_rebind_still_fails_on_bound_group() {
    // --rebind 無しの既存挙動は不変: bind 済みなら bind-keyset の Duplicate で
    // 失敗する（誤って鍵を回す事故の防護）。
    let store = store_with_node5();
    mat(store.path())
        .env("FAKE_GROUP_BOUND", "1")
        .args(["group", "provision", "--group", "1", "--nodes", "5"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("bind-keyset"));
}
```

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p mat --test integration group_provision_rebind -- --nocapture 2>&1 | tail -20`
Expected: FAIL（`unexpected argument '--rebind'` で exit code 2）。`group_provision_without_rebind_still_fails_on_bound_group` も FAIL（fake が `FAKE_GROUP_BOUND` 未対応で成功してしまう）。

- [ ] **Step 3: fake-chip-tool.sh の groupsettings ハンドラを拡張**

`crates/mat/tests/fixtures/fake-chip-tool.sh` の `groupsettings)` ケース（90–95 行）を以下に置き換え:

```sh
  groupsettings)
    # コントローラ側 group state（ローカル操作）。add-group / add-keysets /
    # bind-keyset / unbind-keyset。ネットワーク不要なので timeout/reject 注入はしない。
    #   FAKE_GROUP_BOUND=1 → keyset が bind 済みの controller を模す:
    #     bind-keyset は Duplicate key id で失敗。ただし同一 FAKE_CHIP_ARGS_FILE 内に
    #     unbind-keyset の実行記録が既にあれば「bind し直し」として成功する。
    #     unbind-keyset は成功。
    #   未設定（未 bind）→ unbind-keyset は「未 bind」風エラーで exit 1（mat は
    #     best-effort で無視する）、その他は成功。
    gop="$2"
    if [ "$gop" = "unbind-keyset" ]; then
      if [ -n "$FAKE_GROUP_BOUND" ]; then
        echo "[1656][CHIP:TOO] unbind-keyset ok"
        exit 0
      fi
      echo "[1656][CHIP:DMG] CHIP Error 0x000000C9: keyset not bound"
      exit 1
    fi
    if [ "$gop" = "bind-keyset" ] && [ -n "$FAKE_GROUP_BOUND" ]; then
      if [ -n "$FAKE_CHIP_ARGS_FILE" ] && grep -q "unbind-keyset" "$FAKE_CHIP_ARGS_FILE" 2>/dev/null; then
        echo "[1656][CHIP:TOO] bind-keyset ok"
        exit 0
      fi
      echo "[1656][CHIP:DMG] src/credentials/GroupDataProviderImpl.cpp:1362: CHIP Error 0x0000001A: Duplicate key id"
      exit 1
    fi
    echo "[1656][CHIP:TOO] $2 ok"
    exit 0
    ;;
```

（引数記録はスクリプト先頭で行われるので、bind-keyset 実行時には直前の unbind-keyset の行が既にファイルにある。`grep "unbind-keyset"` は `groupsettings bind-keyset ...` 行にはマッチしない。）

- [ ] **Step 4: cli.rs に `--rebind` フラグを追加**

`crates/mat/src/cli.rs` の `Provision` variant、`epoch_key` フィールドの直後に追加:

```rust
        /// 既存グループの keyset binding を unbind してから bind し直す（既存グループ
        /// へのノード追加用）。--nodes には既存メンバー全員 + 新規を渡し、--keyset-id
        /// は既存と同じ値にすること（新規だけ渡すと epoch key が既存メンバーと食い違い
        /// 届かなくなる）。未 bind の新規グループに付けても安全（冪等）。
        #[arg(long)]
        rebind: bool,
```

- [ ] **Step 5: main.rs の分配に `rebind` を通す**

`crates/mat/src/main.rs` の `GroupCommand::Provision` アーム（156 行付近）を更新:

```rust
            GroupCommand::Provision {
                group_id,
                node_ids,
                keyset_id,
                name,
                endpoint,
                epoch_key,
                rebind,
            } => {
                // name 未指定なら group_id から決定的に補完（open-window の disc と同様）。
                let gid = group_id.id();
                let name = name.clone().unwrap_or_else(|| format!("grp{gid}"));
                let ids: Vec<u64> = node_ids.iter().map(NodeRef::id).collect();
                commands::group::provision(
                    &store_path,
                    gid,
                    &ids,
                    *keyset_id,
                    &name,
                    *endpoint,
                    epoch_key.as_deref(),
                    *rebind,
                )
            }
```

- [ ] **Step 6: group.rs の provision に unbind step と note を実装**

`crates/mat/src/commands/group.rs`:

シグネチャに `rebind: bool` を追加（`#[allow(clippy::too_many_arguments)]` は既にある）:

```rust
pub fn provision(
    store_path: &Path,
    group_id: u16,
    node_ids: &[u64],
    keyset_id: u16,
    name: &str,
    endpoint: u16,
    epoch_key: Option<&str>,
    rebind: bool,
) -> Result<(), MatError> {
```

`add-keysets` の `run_step` と `bind-keyset` の `run_step` の間に挿入:

```rust
    if rebind {
        // 既存グループの keyset binding を解除してから bind し直す（issue #5:
        // controller 側 groupsettings は永続化されており、bind 済みだと bind-keyset
        // が Duplicate key id で落ちる）。unbind は best-effort: 「未 bind なのに
        // unbind」を区別せず失敗を無視する（unbind が本当に必要で失敗したケースは
        // 直後の bind-keyset が従来どおり落ちるので、検知はそちらに委ねる）。
        let out = chip.run(vec![
            "groupsettings".into(),
            "unbind-keyset".into(),
            group_id.to_string(),
            keyset_id.to_string(),
        ])?;
        if !out.success() {
            tracing::debug!(
                group_id,
                keyset_id,
                code = ?out.code,
                "groupsettings unbind-keyset failed; ignored (best-effort rebind)"
            );
        }
    }
```

（`chip.run` の `Err`（chip-tool 起動不能）は `?` で伝播してよい — 次の step も同じ理由で落ちるため。）

末尾の `output::emit` を note 付きに変更:

```rust
    let mut body = json!({
        "group_id": group_id,
        "keyset_id": keyset_id,
        "name": name,
        "endpoint": endpoint,
        "nodes": node_ids,
        "status": "provisioned",
    });
    if rebind {
        // 直経路の rebind は matd の warm chip-tool が旧 group 状態をメモリに
        // 持ったままになるため、稼働中なら再起動が要る（storage は更新済み）。
        body["note"] =
            json!("rebound keyset binding; if matd is running, restart it to reload group state");
    }
    output::emit(body);
    Ok(())
```

`tracing` が group.rs で未 use なら `use tracing;` は不要（`tracing::debug!` はフルパスで呼べる。マクロなので Cargo.toml に `tracing` 依存があれば十分 — mat crate は既に stderr 構造化ログで使用済み）。

- [ ] **Step 7: matd_client.rs をコンパイルに追従させ、op JSON に rebind を載せる**

`crates/mat/src/matd_client.rs` の `GroupCommand::Provision` アーム（228 行付近）:

```rust
            GroupCommand::Provision {
                group_id,
                node_ids,
                keyset_id,
                name,
                endpoint,
                epoch_key,
                rebind,
            } => {
                // name 未指定なら group_id から決定的に補完（main の直接経路と同じ規則）。
                let gid = group_id.id();
                let name = name.clone().unwrap_or_else(|| format!("grp{gid}"));
                let ids: Vec<u64> = node_ids.iter().map(NodeRef::id).collect();
                json!({
                    "op": "group_provision", "group_id": gid, "node_ids": ids,
                    "keyset_id": keyset_id, "name": name, "endpoint": endpoint,
                    "epoch_key": epoch_key, "rebind": rebind,
                })
            }
```

同ファイルのユニットテスト `group_provision_fills_default_name_and_keeps_null_epoch`（477 行付近）の構築側に `rebind: false,` を追加し、期待 JSON に `"rebind":false` を追加:

```rust
        let cmd = Command::Group {
            action: GroupCommand::Provision {
                group_id: GroupRef::Id(7),
                node_ids: vec![NodeRef::Id(1), NodeRef::Id(2)],
                keyset_id: 42,
                name: None,
                endpoint: 1,
                epoch_key: None,
                rebind: false,
            },
        };
        // name 未指定は grp<group_id> に補完。epoch_key は null のまま（matd 側で生成）。
        assert_eq!(
            to_op(&cmd).unwrap(),
            json!({
                "op":"group_provision","group_id":7,"node_ids":[1,2],
                "keyset_id":42,"name":"grp7","endpoint":1,"epoch_key":null,
                "rebind":false
            })
        );
```

- [ ] **Step 8: テストが通ることを確認**

Run: `cargo test -p mat 2>&1 | tail -10`
Expected: PASS（新 3 テスト + 既存 group_provision 系 + matd_client ユニットテスト全部）。

- [ ] **Step 9: `task check` → コミット**

Run: `task check`
Expected: fmt:check / clippy / test すべて成功。

```bash
git add crates/mat/src/cli.rs crates/mat/src/main.rs crates/mat/src/commands/group.rs \
        crates/mat/src/matd_client.rs crates/mat/tests/fixtures/fake-chip-tool.sh \
        crates/mat/tests/integration.rs
git commit -m "feat(mat): group provision --rebind（unbind-keyset を best-effort 挿入、直経路）

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 2: matd 経路の rebind（protocol + server + matd 統合テスト）

**Files:**
- Modify: `crates/matd/src/protocol.rs`（`Op::GroupProvision` に `rebind` フィールド、91 行付近 + ユニットテスト 337 行付近）
- Modify: `crates/matd/src/server.rs`（`group_provision()` に unbind step、345–460 行付近）
- Test: `crates/matd/tests/integration.rs`（fake ws 追加 + テスト 3 件）

**Interfaces:**
- Consumes: Task 1 が op JSON に載せる `"rebind": <bool>`。既存の `group_step(backend, line)`（`server.rs:462`、`Result<(), MatError>`）。
- Produces: `Op::GroupProvision { ..., rebind: bool }`（`#[serde(default)]`）。matd の group_provision 成功応答は従来同形（note なし — warm chip-tool 自身が unbind/bind するため再起動不要）。

- [ ] **Step 1: protocol.rs の失敗するユニットテストを書く**

`crates/matd/src/protocol.rs` のテスト mod、`group_provision_parses_and_has_no_single_node_or_cmdline` の直後に追加:

```rust
    #[test]
    fn group_provision_rebind_defaults_false_and_parses_true() {
        // 旧 mat からの op（rebind フィールド無し）は false に落ちる（後方互換）。
        let r = parse(
            r#"{"op":"group_provision","group_id":1,"node_ids":[1],"keyset_id":42,"name":"g","endpoint":1}"#,
        );
        assert!(matches!(r.op, Op::GroupProvision { rebind: false, .. }));

        let r = parse(
            r#"{"op":"group_provision","group_id":1,"node_ids":[1],"keyset_id":42,"name":"g","endpoint":1,"rebind":true}"#,
        );
        assert!(matches!(r.op, Op::GroupProvision { rebind: true, .. }));
    }
```

- [ ] **Step 2: テストが落ちることを確認**

Run: `cargo test -p matd group_provision_rebind 2>&1 | tail -10`
Expected: コンパイルエラー（`Op::GroupProvision` に `rebind` フィールドが無い）。

- [ ] **Step 3: protocol.rs にフィールドを追加**

`Op::GroupProvision`（91 行付近）の `epoch_key` の直後:

```rust
    GroupProvision {
        group_id: u16,
        node_ids: Vec<u64>,
        keyset_id: u16,
        name: String,
        endpoint: u16,
        #[serde(default)]
        epoch_key: Option<String>,
        /// 既存グループの keyset binding を unbind してから bind し直す（issue #5）。
        /// 旧 mat からの op には無いフィールドなので default = false。
        #[serde(default)]
        rebind: bool,
    },
```

- [ ] **Step 4: server.rs の group_provision に unbind step を入れる**

`crates/matd/src/server.rs` の `group_provision()`:

分配（350 行付近）に `rebind` を追加:

```rust
    let Op::GroupProvision {
        group_id,
        node_ids,
        keyset_id,
        name,
        endpoint,
        epoch_key,
        rebind,
    } = op
```

`add-keysets` の `group_step` と `bind-keyset` の `group_step` の間（387 行付近）に挿入:

```rust
    if *rebind {
        // 既存グループの keyset binding を解除してから bind し直す（issue #5）。
        // best-effort: 「未 bind なのに unbind」を区別せず失敗を無視する（unbind が
        // 本当に必要で失敗したケースは直後の bind-keyset が従来どおり落ちるので、
        // 検知はそちらに委ねる）。
        if let Err(e) = group_step(
            backend,
            &format!("groupsettings unbind-keyset {group_id} {keyset_id}"),
        )
        .await
        {
            tracing::debug!(
                group_id,
                keyset_id,
                error = %e.detail,
                "groupsettings unbind-keyset failed; ignored (best-effort rebind)"
            );
        }
    }
```

（`MatError` の `detail` は public フィールド。`mat/src/commands/group.rs:422` と同じアクセス。）

- [ ] **Step 5: protocol ユニットテストが通ることを確認**

Run: `cargo test -p matd group_provision_rebind 2>&1 | tail -5`
Expected: PASS。

- [ ] **Step 6: matd 統合テスト用の fake ws とテスト 3 件を書く**

`crates/matd/tests/integration.rs`、`spawn_fake_ws_recording` の直後に追加:

```rust
/// group の bind 状態を模した fake ws（issue #5 rebind 用）。コマンド行を記録しつつ:
/// - `groupsettings unbind-keyset`: bound なら成功（以後 unbound）、unbound なら FAILURE
/// - `groupsettings bind-keyset`: bound かつ未 unbind なら FAILURE（Duplicate 相当）、
///   それ以外は成功
/// - `accesscontrol read acl`: admin エントリのみの実機数値キー形式
/// - それ以外: 成功（value: true）
async fn spawn_fake_ws_group(bound: bool) -> (u16, Arc<tokio::sync::Mutex<Vec<String>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let lines_log: Arc<tokio::sync::Mutex<Vec<String>>> =
        Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let log = Arc::clone(&lines_log);
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let log = Arc::clone(&log);
            tokio::spawn(async move {
                let mut ws = accept_async(stream).await.unwrap();
                let mut bound = bound;
                while let Some(Ok(msg)) = ws.next().await {
                    if let Message::Text(line) = msg {
                        log.lock().await.push(line.clone());
                        let resp = if line.contains("groupsettings unbind-keyset") {
                            if bound {
                                bound = false;
                                json!({ "results": [{ "value": true }], "logs": [] })
                            } else {
                                json!({ "results": [{ "error": "FAILURE" }], "logs": [] })
                            }
                        } else if line.contains("groupsettings bind-keyset") && bound {
                            // bind 済みのまま bind → Duplicate key id 相当の失敗。
                            json!({ "results": [{ "error": "FAILURE" }], "logs": [] })
                        } else if line.contains("accesscontrol read acl") {
                            json!({ "results": [{ "value": [{"1":5,"2":2,"3":[112233],"4":null,"254":1}] }], "logs": [] })
                        } else {
                            json!({ "results": [{ "value": true }], "logs": [] })
                        };
                        ws.send(Message::Text(resp.to_string())).await.unwrap();
                    }
                }
            });
        }
    });
    (port, lines_log)
}
```

テスト 3 件（既存の group_provision 系テストの後ろに追加）:

```rust
/// bind 済み controller でも rebind:true なら unbind → bind の順で成功する（issue #5）。
#[tokio::test]
async fn group_provision_rebind_unbinds_before_bind() {
    let (port, lines) = spawn_fake_ws_group(true).await;
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd(store_path, port).await;

    let resps = roundtrip(
        &socket,
        &[json!({
            "op":"group_provision","group_id":1,"node_ids":[1],"keyset_id":42,
            "name":"living","endpoint":1,"rebind":true,
        })],
    )
    .await;
    assert_eq!(resps[0]["status"], "provisioned", "{}", resps[0]);
    // matd 経路は warm chip-tool 自身が状態更新するので再起動 note は出ない。
    assert!(resps[0].get("note").is_none(), "{}", resps[0]);

    let recorded = lines.lock().await.clone();
    let unbind = recorded
        .iter()
        .position(|l| l.contains("groupsettings unbind-keyset 1 42"))
        .expect("unbind-keyset line missing");
    let bind = recorded
        .iter()
        .position(|l| l.contains("groupsettings bind-keyset 1 42"))
        .expect("bind-keyset line missing");
    assert!(unbind < bind, "unbind must run before bind: {recorded:?}");
    handle.abort();
}

/// 未 bind（新規グループ）でも rebind:true は成功する（unbind の失敗を無視 = 冪等）。
#[tokio::test]
async fn group_provision_rebind_on_unbound_group_succeeds() {
    let (port, _lines) = spawn_fake_ws_group(false).await;
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd(store_path, port).await;

    let resps = roundtrip(
        &socket,
        &[json!({
            "op":"group_provision","group_id":1,"node_ids":[1],"keyset_id":42,
            "name":"living","endpoint":1,"rebind":true,
        })],
    )
    .await;
    assert_eq!(resps[0]["status"], "provisioned", "{}", resps[0]);
    handle.abort();
}

/// rebind 無しの既存挙動は不変: bind 済みなら bind-keyset の失敗で止まる。
#[tokio::test]
async fn group_provision_without_rebind_fails_on_bound_group() {
    let (port, _lines) = spawn_fake_ws_group(true).await;
    let (_dir, store_path) = make_store();
    let (socket, handle) = start_matd(store_path, port).await;

    let resps = roundtrip(
        &socket,
        &[json!({
            "op":"group_provision","group_id":1,"node_ids":[1],"keyset_id":42,
            "name":"living","endpoint":1,
        })],
    )
    .await;
    assert!(
        resps[0]["error"]["kind"].is_string(),
        "must fail on bound group without rebind: {}",
        resps[0]
    );
    handle.abort();
}
```

（`start_matd` が `handle` を返す既存パターンに合わせる。既存テストが handle を捨てているなら `let (socket, _handle) = ...` に合わせ、`handle.abort()` 行は落とす — 既存の group_provision テストの形を正とする。）

- [ ] **Step 7: matd テスト全体が通ることを確認**

Run: `cargo test -p matd 2>&1 | tail -10`
Expected: PASS（新 4 テスト含む）。

- [ ] **Step 8: `task check` → コミット**

Run: `task check`
Expected: すべて成功。

```bash
git add crates/matd/src/protocol.rs crates/matd/src/server.rs crates/matd/tests/integration.rs
git commit -m "feat(matd): group_provision op に rebind（unbind-keyset を best-effort 挿入）

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```

---

### Task 3: README・バージョン 0.15.0

**Files:**
- Modify: `README.md`（「Groupcast」節 363–436 行付近、matd 節のバージョンスキュー注記 514–518 行付近）
- Modify: `Cargo.toml`（workspace.package version 0.14.0 → 0.15.0）
- Modify: `Cargo.lock`（`cargo check` で追従）

**Interfaces:**
- Consumes: Task 1–2 の CLI/挙動（`--rebind`、直経路 note、matd 後方互換）。
- Produces: なし（ドキュメントのみ）。

- [ ] **Step 1: README の Groupcast 節に「既存グループへのノード追加」を追記**

`README.md` の bash 例ブロック（381 行付近、`mat group provision --group 1 --nodes 5 6 7 --name living` の直後）に追加:

```bash
# Add a node to an existing group: pass --rebind with ALL existing members plus
# the new one, and the SAME --keyset-id the group already uses.
mat group provision --group 1 --nodes 5 6 7 8 --name living --rebind
```

出力例ブロック（398 行付近）の provision 行の下に rebind 版を追加:

```json
// provision --rebind via the direct path also notes the matd restart caveat
{ "timestamp": "...", "group_id": 1, "keyset_id": 42, "name": "living", "endpoint": 1, "nodes": [5, 6, 7, 8], "status": "provisioned", "note": "rebound keyset binding; if matd is running, restart it to reload group state" }
```

箇条書き（431 行付近の `mat group grant` の項の直前）に新項目を追加:

```markdown
- **Adding a node to an existing group: `--rebind`.** The controller-side
  `groupsettings` state persists across chip-tool runs, so re-running provision
  on an existing group fails at `bind-keyset` with `Duplicate key id` — worse,
  the earlier `add-keysets` step has already rotated the controller's epoch key,
  leaving it out of sync with the devices (groupcast silently breaks). Without
  `--rebind` this failure is intentional (it stops you from rotating keys by
  accident). With `--rebind`, provision unbinds the keyset binding first
  (best-effort; also safe on a brand-new group) and re-provisions cleanly. Three
  rules: pass **all existing members plus the new node** to `--nodes` (a fresh
  epoch key is generated, so nodes left out stop receiving groupcasts), keep the
  **same `--keyset-id`** (the device keyset table holds max 3 entries and the
  IPK uses one), and confirm membership per node with
  `mat read -e 0 -c groupkeymanagement -a group-key-map`. After a direct-path
  `--rebind`, restart `matd` if it is running (its warm chip-tool still holds
  the old group state in memory; storage is already updated).
```

`mat group grant` の項（431–436 行）の「The controller-side `groupsettings` state is not idempotent, so provision cannot simply be re-run」の直後に `— use `provision --rebind` to re-run it on an existing group;` を挿入して文を繋げる（grant = ACL のみ / rebind = フル再 provision の使い分けが読めるように）。

- [ ] **Step 2: matd 節にバージョンスキュー注記を追記**

`README.md` 514–518 行付近の既存スキュー注記（`matd ≥ 0.14; an older matd rejects the unknown op ...`）に続けて追加:

```markdown
`group provision --rebind` through matd needs matd ≥ 0.15: an older matd
silently ignores the unknown `rebind` field and fails at `bind-keyset`
(`Duplicate key id`) — same as running provision without the flag.
```

- [ ] **Step 3: バージョンを 0.15.0 に上げる**

`Cargo.toml` の `[workspace.package]` `version = "0.14.0"` → `"0.15.0"`。

Run: `cargo check 2>&1 | tail -3`
Expected: 成功（Cargo.lock が 0.15.0 に追従）。

- [ ] **Step 4: spec の受け入れ条件チェックボックスを埋める**

`docs/superpowers/specs/2026-07-09-group-provision-rebind-design.md` 末尾の受け入れ条件のうち、fake-chip-tool テスト / 既存挙動不変 / README・--help / 冪等の各項目を `- [x]` に更新（実機一発成功の項は E2E 未実施なら残す）。

- [ ] **Step 5: `task check` → コミット**

Run: `task check`
Expected: すべて成功。

```bash
git add README.md Cargo.toml Cargo.lock \
        docs/superpowers/specs/2026-07-09-group-provision-rebind-design.md
git commit -m "docs: group provision --rebind の手順とスキュー注記、0.15.0

Co-Authored-By: Claude Fable 5 <noreply@anthropic.com>"
```
