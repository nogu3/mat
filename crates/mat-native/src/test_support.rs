//! テストヘルパ置き場。mat / matd 双方のテストから
//! `mat_native::test_support::*`（feature `test-support`、または自 crate
//! テスト時）で使う。

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use async_trait::async_trait;
use base64ct::{Base64, Encoding};
use serde_json::json;

use mat_controller::tlv::{Tag, Writer};
use mat_core::error::{ErrorKind, MatError};

use crate::{Establisher, NodeConn};

/// 送信 1 回目に `fail_kind` で失敗するよう設定できる fake セッション。
/// `sent` は `&mut self` 下でのみ触るので素の `usize` で足りる（atomic 不要）。
///
/// `reads`/`clusters` は `scripted()` + `with_read`/`with_cluster` で流し込む
/// プリセット応答（ops.rs のテスト等が使う）。未登録の `(endpoint, cluster,
/// attribute)` / `(endpoint, cluster)` は従来どおりの固定値へフォールバック
/// する — 既存テスト（`fail_first_send` 等）の挙動は不変。
pub struct FakeConn {
    pub fail_first_send: bool,
    pub fail_kind: ErrorKind,
    pub sent: usize,
    reads: HashMap<(u16, u32, u32), serde_json::Value>,
    clusters: HashMap<(u16, u32), Vec<(u32, serde_json::Value)>>,
}

impl Default for FakeConn {
    fn default() -> Self {
        Self {
            fail_first_send: false,
            fail_kind: ErrorKind::Timeout,
            sent: 0,
            reads: HashMap::new(),
            clusters: HashMap::new(),
        }
    }
}

impl FakeConn {
    /// 呼ばれた `(endpoint, cluster, attribute)` / `(endpoint, cluster)` に
    /// 応じたプリセット応答を返す fake を作る（`with_read`/`with_cluster` で
    /// 登録）。未登録の呼び出しは既存の固定値応答にフォールバックする。
    pub fn scripted() -> Self {
        Self::default()
    }

    /// `read_json(endpoint, cluster, attribute)` の応答を1件登録する。
    pub fn with_read(
        mut self,
        endpoint: u16,
        cluster: u32,
        attribute: u32,
        value: serde_json::Value,
    ) -> Self {
        self.reads.insert((endpoint, cluster, attribute), value);
        self
    }

    /// `read_cluster(endpoint, cluster)` の応答（wildcard read の結果）を
    /// 1件登録する。
    pub fn with_cluster(
        mut self,
        endpoint: u16,
        cluster: u32,
        rows: Vec<(u32, serde_json::Value)>,
    ) -> Self {
        self.clusters.insert((endpoint, cluster), rows);
        self
    }
}

#[async_trait]
impl NodeConn for FakeConn {
    async fn read_onoff(&mut self, _endpoint: u16) -> Result<bool, MatError> {
        let n = self.sent;
        self.sent += 1;
        if self.fail_first_send && n == 0 {
            return Err(MatError::new(self.fail_kind, "fake send failure"));
        }
        Ok(true)
    }
    async fn invoke(
        &mut self,
        _endpoint: u16,
        _cluster: u32,
        _command: u32,
        _fields: Option<Vec<u8>>,
        _timed: bool,
    ) -> Result<(), MatError> {
        Ok(())
    }

    async fn read_json(
        &mut self,
        endpoint: u16,
        cluster: u32,
        attribute: u32,
    ) -> Result<serde_json::Value, MatError> {
        if let Some(v) = self.reads.get(&(endpoint, cluster, attribute)) {
            return Ok(v.clone());
        }
        Ok(json!(1))
    }

    async fn read_cluster(
        &mut self,
        endpoint: u16,
        cluster: u32,
    ) -> Result<Vec<(u32, serde_json::Value)>, MatError> {
        if let Some(rows) = self.clusters.get(&(endpoint, cluster)) {
            return Ok(rows.clone());
        }
        Ok(vec![(0u32, json!(true))])
    }

    async fn write_tlv(
        &mut self,
        _endpoint: u16,
        _cluster: u32,
        _attribute: u32,
        _data_tlv: Vec<u8>,
        _timed: bool,
    ) -> Result<(), MatError> {
        let n = self.sent;
        self.sent += 1;
        if self.fail_first_send && n == 0 {
            return Err(MatError::new(self.fail_kind, "fake send failure"));
        }
        Ok(())
    }

    async fn open_window(
        &mut self,
        _timeout_s: u16,
        discriminator: u16,
        _iterations: u32,
    ) -> Result<(String, String), MatError> {
        // 固定文字列: manual_code は11桁数字風、qr_payload は "MT:" 始まり
        // （brief 記載どおり — 実 setup code とのバイト整合は問わないテスト用）。
        Ok((
            "34970112332".to_string(),
            format!("MT:FAKE0{discriminator:04X}QR0"),
        ))
    }
}

/// establish 呼び出し回数を外部の `Arc<AtomicUsize>` で数える fake。
/// `fail_first_send`/`fail_kind` を確立する Conn に伝える
/// （2 回目の確立=再確立では成功させる）。デフォルトは常に成功する
/// establish（group_invoke テストのように失敗パスを使わない場合向け）。
pub struct FakeEstablisher {
    pub calls: std::sync::Arc<AtomicUsize>,
    pub fail_first_send: bool,
    pub fail_kind: ErrorKind,
}

impl Default for FakeEstablisher {
    fn default() -> Self {
        Self {
            calls: std::sync::Arc::new(AtomicUsize::new(0)),
            fail_first_send: false,
            fail_kind: ErrorKind::Timeout,
        }
    }
}

#[async_trait]
impl Establisher for FakeEstablisher {
    async fn establish(&self, _node_id: u64) -> Result<Box<dyn NodeConn>, MatError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FakeConn {
            fail_first_send: self.fail_first_send && n == 0,
            fail_kind: self.fail_kind,
            ..Default::default()
        }))
    }
}

/// KeyMapData blob（`f/<idx>/gk/<n>`）: struct{ctx1:group_id,
/// ctx2:keyset_id, ctx3:next}。`crate::kvs` のテストフィクスチャと同構造。
fn keymap_blob(group_id: u16, keyset_id: u16, next: u8) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(1), u64::from(group_id));
    w.put_uint(Tag::Context(2), u64::from(keyset_id));
    w.put_uint(Tag::Context(3), u64::from(next));
    w.end_container();
    w.finish()
}

/// keyset blob（`f/<idx>/k/<n>`）: struct{ctx1:policy, ctx2:keys_count,
/// ctx3:array[struct{ctx4:start_time, ctx5:hash, ctx6:key(16B)}],
/// ctx7:next}。`crate::kvs` のテストフィクスチャと同構造（配列は1エントリ
/// のみで十分 — parser は最初のエントリだけ見る）。
fn keyset_blob(hash: u16, key: &[u8; 16]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(1), 0); // policy
    w.put_uint(Tag::Context(2), 1); // keys_count
    w.start_array(Tag::Context(3));
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(4), 0); // start_time
    w.put_uint(Tag::Context(5), u64::from(hash));
    w.put_bytes(Tag::Context(6), key);
    w.end_container();
    w.end_container();
    w.put_uint(Tag::Context(7), 0xFFFF); // next keyset id（無視される）
    w.end_container();
    w.finish()
}

/// fabric index 2 に group 10 → keyset 0x3c（hash 0x855f, key
/// `[0xDD;16]`）と `g/gdc = 1000` を持つ chip-tool KVS ini フィクスチャを
/// 書く。
pub fn write_group_fixture_ini(path: &std::path::Path) {
    let gk = keymap_blob(10, 0x3c, 0);
    let ks = keyset_blob(0x855f, &[0xDD; 16]);
    let gdc = 1000u32.to_le_bytes();
    let mut body = String::from("[Default]\n");
    body.push_str(&format!("f/2/gk/1 = {}\n", Base64::encode_string(&gk)));
    body.push_str(&format!("f/2/k/3c = {}\n", Base64::encode_string(&ks)));
    body.push_str(&format!("g/gdc = {}\n", Base64::encode_string(&gdc)));
    std::fs::write(path, body).unwrap();
}

/// A network interface eligible to try as the multicast join/egress
/// interface for multicast-loopback tests. Same discovery logic as
/// `mat_controller::group`'s `group_sender_multicast_loops_back_locally`
/// test (private there, so duplicated here): `lo` lacks `IFF_MULTICAST`
/// on Linux and never delivers. Shared here (rather than duplicated a
/// third time) for both mat-native's own group tests and mat / matd's
/// group-routing tests.
pub struct McastCandidate {
    pub name: String,
    pub index: u32,
}

pub fn multicast_capable_interfaces() -> Vec<McastCandidate> {
    const IFF_UP: u32 = 0x1;
    const IFF_MULTICAST: u32 = 0x1000;
    let mut up_first = Vec::new();
    let mut rest = Vec::new();
    let Ok(entries) = std::fs::read_dir("/sys/class/net") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "lo" {
            continue;
        }
        let base = entry.path();
        let flags = std::fs::read_to_string(base.join("flags"))
            .ok()
            .and_then(|s| u32::from_str_radix(s.trim().trim_start_matches("0x"), 16).ok())
            .unwrap_or(0);
        if flags & IFF_UP == 0 || flags & IFF_MULTICAST == 0 {
            continue;
        }
        let Some(index) = std::fs::read_to_string(base.join("ifindex"))
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok())
        else {
            continue;
        };
        let operstate = std::fs::read_to_string(base.join("operstate")).unwrap_or_default();
        let candidate = McastCandidate { name, index };
        if operstate.trim() == "up" {
            up_first.push(candidate);
        } else {
            rest.push(candidate);
        }
    }
    up_first.sort_by_key(|c| c.index);
    rest.sort_by_key(|c| c.index);
    up_first.extend(rest);
    up_first
}
