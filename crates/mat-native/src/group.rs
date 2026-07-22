//! groupcast 送信コンテキストと送出処理。「未 provision・KVS 不備・counter
//! 初期化不能」は `GroupOutcome::Unavailable` に写像し、呼び出し側（mat / matd）
//! が `store_parse` の per-op ハードエラーへ変換する合図として使う
//! （M8c-3: chip-tool フォールバック撤去）。

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::Mutex;

use mat_controller::group::{GroupSender, PersistedGroupCounter};
use mat_controller::kvs;
use mat_controller::transport::UdpTransport;
use mat_core::error::{ErrorKind, MatError};

/// group 送信に必要な材料一式。`sender`（counter を内包）は初 group op で
/// lazy 構築する。鍵は send のたびに KVS から読む（`provision --rebind`
/// 直後でも stale にならない）。
pub struct GroupCtx {
    pub main_ini: PathBuf,
    pub counter_path: PathBuf,
    pub fabric_index: u8,
    pub fabric_id: u64,
    pub node_id: u64,
    pub scope_id: u32,
    pub dest_port: u16,
    pub transport: Arc<UdpTransport>,
    pub sender: Mutex<Option<GroupSender>>,
}

/// group 送信の結果。`Unavailable` は「native では送れない（未 provision・
/// KVS 不備等）」で、消費側（mat / matd）が `store_parse` の per-op
/// ハードエラーへ変換する合図（M8c-3: chip-tool フォールバック撤去）。
pub enum GroupOutcome {
    Sent,
    Unavailable(String),
}

/// group へ groupcast を 1 発送る。native で送れない事情（未 provision・
/// KVS 不備・counter 初期化不能）は `Unavailable` で返し、送出自体の失敗
/// （socket）だけを Err にする。
pub async fn send(
    ctx: &GroupCtx,
    group_id: u16,
    cluster: u32,
    command: u32,
    fields: Option<Vec<u8>>,
) -> Result<GroupOutcome, MatError> {
    let creds = match kvs::read_group_credentials(&ctx.main_ini, ctx.fabric_index, group_id) {
        Ok(c) => c,
        Err(e) => {
            return Ok(GroupOutcome::Unavailable(format!(
                "group {group_id} credentials: {e} (not provisioned? run `mat group provision`)"
            )))
        }
    };
    let mut slot = ctx.sender.lock().await;
    if slot.is_none() {
        let gdc = match kvs::read_group_data_counter(&ctx.main_ini) {
            Ok(Some(v)) => v,
            Ok(None) => {
                return Ok(GroupOutcome::Unavailable(
                    "chip-tool g/gdc missing; refusing to start the group counter low".into(),
                ))
            }
            Err(e) => return Ok(GroupOutcome::Unavailable(format!("read g/gdc: {e}"))),
        };
        let counter = match PersistedGroupCounter::load(&ctx.counter_path, gdc) {
            Ok(c) => c,
            Err(e) => {
                return Ok(GroupOutcome::Unavailable(format!(
                    "group counter store: {e}"
                )))
            }
        };
        match GroupSender::new(
            Arc::clone(&ctx.transport),
            ctx.scope_id,
            ctx.dest_port,
            ctx.fabric_id,
            ctx.node_id,
            counter,
        ) {
            Ok(s) => *slot = Some(s),
            Err(e) => {
                return Ok(GroupOutcome::Unavailable(format!(
                    "multicast socket setup: {e}"
                )))
            }
        }
    }
    match slot
        .as_mut()
        .expect("built above")
        .send_invoke(&creds, group_id, cluster, command, fields.as_deref())
        .await
    {
        Ok(counter) => {
            tracing::info!(group_id, counter, "groupcast sent (native)");
            Ok(GroupOutcome::Sent)
        }
        Err(e) => Err(group_send_error(group_id, e)),
    }
}

/// `GroupSendError` → mat の `ErrorKind`。`Io`（socket 送出失敗 = ワイヤに乗らな
/// かった）は `Unreachable`。`Crypto`（AES-CCM 暗号化失敗 — 実用上 caller bug か
/// payload サイズ超過のみで、ネットワーク不達ではない）は `Other` へ分離
/// （v1 品質修正 2 — 旧実装は両者を Unreachable に一括写像していた）。
fn group_send_error(group_id: u16, e: mat_controller::group::GroupSendError) -> MatError {
    use mat_controller::group::GroupSendError;
    let kind = match &e {
        GroupSendError::Io(_) => ErrorKind::Unreachable,
        GroupSendError::Crypto(_) => ErrorKind::Other,
    };
    MatError::new(kind, format!("groupcast send to group {group_id}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::write_group_fixture_ini;
    use mat_controller::im;

    /// v1 品質修正 2: `Io`(socket 送出失敗) は `Unreachable` のままだが、
    /// `Crypto`(AES-CCM 暗号化失敗 = ネットワーク不達ではない) は `Other` へ。
    #[test]
    fn group_send_error_maps_io_to_unreachable_and_crypto_to_other() {
        use mat_controller::group::GroupSendError;
        let io = group_send_error(10, GroupSendError::Io(std::io::Error::other("send failed")));
        assert_eq!(io.kind, mat_core::error::ErrorKind::Unreachable);
        let crypto = group_send_error(
            10,
            GroupSendError::Crypto(mat_controller::crypto::CryptoError::PayloadTooLarge),
        );
        assert_eq!(crypto.kind, mat_core::error::ErrorKind::Other);
        assert!(
            crypto.detail.contains("group 10"),
            "detail: {}",
            crypto.detail
        );
    }

    #[tokio::test]
    async fn group_invoke_sends_multicast_and_reports_sent() {
        // フィクスチャ ini（gk + keyset + g/gdc）と loopback 受信で end-to-end。
        // `lo` は multicast join を受け付けないので、実際に配達できる iface を
        // 実行時に探す（`scope_id` はループの試行ごとに決まる）。
        let dir = std::env::temp_dir().join(format!("mat-native-group-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ini = dir.join("chip_tool_config.ini");
        write_group_fixture_ini(&ini);

        let mut tried = Vec::new();
        for cand in crate::test_support::multicast_capable_interfaces() {
            // 候補ごとに新しい受信ソケット: 1 ソケットは同じ multicast group に
            // 1回しか join できず、失敗した候補が次を汚染してはいけない。
            let recv = tokio::net::UdpSocket::bind("[::]:0").await.unwrap();
            let port = recv.local_addr().unwrap().port();
            if recv
                .join_multicast_v6(
                    &mat_controller::group::group_multicast_addr(1, 10),
                    cand.index,
                )
                .is_err()
            {
                tried.push(format!("{}(idx={}): join failed", cand.name, cand.index));
                continue;
            }

            let counter_path = dir.join(format!("native_group_counter-{}", cand.index));
            let _ = std::fs::remove_file(&counter_path);
            let transport = Arc::new(UdpTransport::bind().await.unwrap());
            let ctx = GroupCtx {
                main_ini: ini.clone(),
                counter_path,
                fabric_index: 2,
                fabric_id: 1,
                node_id: 0x0001_0001,
                scope_id: cand.index,
                dest_port: port,
                transport,
                sender: Mutex::new(None),
            };
            let r = send(&ctx, 10, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON, None)
                .await
                .unwrap();
            assert!(matches!(r, GroupOutcome::Sent));

            let mut buf = [0u8; 1280];
            let result = tokio::time::timeout(
                std::time::Duration::from_millis(500),
                recv.recv_from(&mut buf),
            )
            .await;
            match result {
                Ok(Ok(_)) => {
                    // 未 provision group は Unavailable（消費側で store_parse ハードエラーになる）。
                    let r = send(&ctx, 99, im::CLUSTER_ON_OFF, im::CMD_ON_OFF_ON, None)
                        .await
                        .unwrap();
                    assert!(matches!(r, GroupOutcome::Unavailable(_)));
                    let _ = std::fs::remove_dir_all(&dir);
                    return; // 配達できる iface が見つかった時点で PASS。
                }
                _ => tried.push(format!("{}(idx={}): no delivery", cand.name, cand.index)),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
        panic!(
            "no multicast-capable interface delivered a loopback groupcast \
             datagram (lo excluded — it lacks IFF_MULTICAST on Linux); \
             tried: {tried:?}"
        );
    }
}
