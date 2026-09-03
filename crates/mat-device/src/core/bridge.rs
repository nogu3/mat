//! `DeviceKind` — 設定ファイルの device kind と、そこから bridged endpoint
//! （M3 spec の Aggregator 配下、matv の各デバイスに対応する 1 endpoint）
//! のクラスタ一式を組み立てるファクトリ。
//!
//! 種別追加は「[`DeviceKind`] に 1 値 + [`build_bridged_endpoint`] に 1
//! 分岐」で完結する — 設定ファイルのパーサ／net 層はこのモジュールだけを
//! 見れば新しい device kind を扱える。
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use mat_controller::im;

use crate::core::bridged_device_basic_information::BridgedDeviceBasicInformationHandler;
use crate::core::datamodel::{ClusterHandler, DescriptorHandler};
use crate::core::group_membership::GroupMembershipStore;
use crate::core::groups::GroupsHandler;
use crate::core::identify::IdentifyHandler;
use crate::core::onoff::OnOffHandler;

/// 設定ファイルの kind enum。種別追加は「ここに 1 値 +
/// build_bridged_endpoint に 1 分岐」で完結する（M3 spec の拡張可能性
/// 要件）。serde 綴りは設定ファイルの正本表記。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize)]
pub enum DeviceKind {
    #[serde(rename = "onoff-light")]
    OnOffLight,
}

/// 1 つの bridged endpoint に載せるクラスタ一式と、外側（runtime/ログ）
/// へ渡す状態ハンドル。
pub struct BridgedEndpoint {
    pub clusters: Vec<Box<dyn ClusterHandler>>,
    pub onoff_state: Arc<AtomicBool>,
}

/// `kind`/`name`/`unique_id` から 1 つの bridged endpoint 分のクラスタ一式
/// を組み立てる。`name` は Bridged Device Basic Information の NodeLabel
/// （設定ファイルの device name が正本 — 同ハンドラのモジュールコメント
/// 参照）、`unique_id` はその UniqueID。
pub fn build_bridged_endpoint(
    kind: DeviceKind,
    name: &str,
    unique_id: &str,
    endpoint: u16,
    membership: &GroupMembershipStore,
) -> BridgedEndpoint {
    match kind {
        DeviceKind::OnOffLight => {
            let (identify, identify_state) = IdentifyHandler::new();
            let (onoff, onoff_state) = OnOffHandler::new();
            BridgedEndpoint {
                clusters: vec![
                    Box::new(DescriptorHandler::for_device_types(&[
                        im::DEVICE_TYPE_ON_OFF_LIGHT,
                        im::DEVICE_TYPE_BRIDGED_NODE,
                    ])),
                    Box::new(BridgedDeviceBasicInformationHandler::new(name, unique_id)),
                    Box::new(identify),
                    Box::new(GroupsHandler::new(
                        identify_state,
                        membership.clone(),
                        endpoint,
                    )),
                    Box::new(onoff),
                ],
                onoff_state,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn onoff_light_yields_the_m2_ep1_cluster_set_plus_bdbi() {
        let endpoint = build_bridged_endpoint(
            DeviceKind::OnOffLight,
            "Living",
            "uid-1",
            2,
            &GroupMembershipStore::new(),
        );
        let ids: std::collections::BTreeSet<u32> =
            endpoint.clusters.iter().map(|c| c.cluster_id()).collect();
        assert_eq!(
            ids,
            std::collections::BTreeSet::from([
                mat_controller::im::CLUSTER_DESCRIPTOR,
                mat_controller::im::CLUSTER_BRIDGED_DEVICE_BASIC_INFORMATION,
                mat_controller::im::CLUSTER_IDENTIFY,
                mat_controller::im::CLUSTER_GROUPS,
                mat_controller::im::CLUSTER_ON_OFF,
            ])
        );
    }

    #[test]
    fn onoff_light_registers_clusters_in_spec_order() {
        let endpoint = build_bridged_endpoint(
            DeviceKind::OnOffLight,
            "Living",
            "uid-1",
            2,
            &GroupMembershipStore::new(),
        );
        let ids: Vec<u32> = endpoint.clusters.iter().map(|c| c.cluster_id()).collect();
        assert_eq!(
            ids,
            vec![
                mat_controller::im::CLUSTER_DESCRIPTOR,
                mat_controller::im::CLUSTER_BRIDGED_DEVICE_BASIC_INFORMATION,
                mat_controller::im::CLUSTER_IDENTIFY,
                mat_controller::im::CLUSTER_GROUPS,
                mat_controller::im::CLUSTER_ON_OFF,
            ]
        );
    }

    #[test]
    fn onoff_state_handle_reflects_the_registered_onoff_handler() {
        use mat_controller::im;

        use crate::core::datamodel::InvokeCtx;

        let mut endpoint = build_bridged_endpoint(
            DeviceKind::OnOffLight,
            "Living",
            "uid-1",
            2,
            &GroupMembershipStore::new(),
        );
        assert!(!endpoint
            .onoff_state
            .load(std::sync::atomic::Ordering::SeqCst));

        let onoff = endpoint
            .clusters
            .iter_mut()
            .find(|c| c.cluster_id() == im::CLUSTER_ON_OFF)
            .expect("OnOff handler registered");
        onoff.invoke(im::CMD_ON_OFF_ON, &[], &mut InvokeCtx::default());

        assert!(endpoint
            .onoff_state
            .load(std::sync::atomic::Ordering::SeqCst));
    }

    /// 設定ファイル正本表記のデシリアライズ。TOML はこのクレートの
    /// dev-dependency にないため（`toml` は matv 側のみ）、`serde_json`
    /// （既存のワークスペース依存）で serde の rename 属性そのものを検証
    /// する — under test なのは文字列表記と enum の対応であって TOML の
    /// パース経路ではない。
    #[test]
    fn deserializes_onoff_light_from_its_config_spelling() {
        let kind: DeviceKind = serde_json::from_str("\"onoff-light\"").unwrap();
        assert_eq!(kind, DeviceKind::OnOffLight);
    }

    #[test]
    fn unknown_kind_spelling_is_a_deserialize_error() {
        let result: Result<DeviceKind, _> = serde_json::from_str("\"not-a-real-kind\"");
        assert!(result.is_err());
    }
}
