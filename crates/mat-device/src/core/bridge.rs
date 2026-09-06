//! `DeviceKind` — 設定ファイルの device kind と、そこから bridged endpoint
//! （M3 spec の Aggregator 配下、matv の各デバイスに対応する 1 endpoint）
//! のクラスタ一式を組み立てるファクトリ。
//!
//! 種別追加は「[`DeviceKind`] に 1 値 + [`build_bridged_endpoint`] に 1
//! 分岐」で完結する — 設定ファイルのパーサ／net 層はこのモジュールだけを
//! 見れば新しい device kind を扱える。
use std::sync::atomic::{AtomicBool, AtomicU8};
use std::sync::Arc;

use mat_controller::im;

use crate::core::boolean_state::BooleanStateHandler;
use crate::core::bridged_device_basic_information::BridgedDeviceBasicInformationHandler;
use crate::core::datamodel::{ClusterHandler, DescriptorHandler};
use crate::core::generic_switch::GenericSwitchHandler;
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
    #[serde(rename = "switch")]
    Switch,
    #[serde(rename = "contact-sensor")]
    ContactSensor,
}

/// 1 つの bridged endpoint の観測ハンドル — kind ごとに違う本体クラスタの
/// 状態を、外側（runtime/ログ）へ種別を保ったまま渡す。
#[derive(Debug, Clone)]
pub enum BridgedState {
    OnOff(Arc<AtomicBool>),
    Switch(Arc<AtomicU8>),
    Contact(Arc<AtomicBool>),
}

/// 1 つの bridged endpoint に載せるクラスタ一式と、外側（runtime/ログ）
/// へ渡す状態ハンドル。
pub struct BridgedEndpoint {
    pub clusters: Vec<Box<dyn ClusterHandler>>,
    pub state: BridgedState,
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
                state: BridgedState::OnOff(onoff_state),
            }
        }
        DeviceKind::Switch => {
            let (identify, identify_state) = IdentifyHandler::new();
            let (switch, switch_state) = GenericSwitchHandler::new();
            BridgedEndpoint {
                clusters: vec![
                    Box::new(DescriptorHandler::for_device_types(&[
                        im::DEVICE_TYPE_GENERIC_SWITCH,
                        im::DEVICE_TYPE_BRIDGED_NODE,
                    ])),
                    Box::new(BridgedDeviceBasicInformationHandler::new(name, unique_id)),
                    Box::new(identify),
                    Box::new(GroupsHandler::new(
                        identify_state,
                        membership.clone(),
                        endpoint,
                    )),
                    Box::new(switch),
                ],
                state: BridgedState::Switch(switch_state),
            }
        }
        DeviceKind::ContactSensor => {
            let (identify, identify_state) = IdentifyHandler::new();
            let (contact, contact_state) = BooleanStateHandler::new();
            BridgedEndpoint {
                clusters: vec![
                    Box::new(DescriptorHandler::for_device_types(&[
                        im::DEVICE_TYPE_CONTACT_SENSOR,
                        im::DEVICE_TYPE_BRIDGED_NODE,
                    ])),
                    Box::new(BridgedDeviceBasicInformationHandler::new(name, unique_id)),
                    Box::new(identify),
                    Box::new(GroupsHandler::new(
                        identify_state,
                        membership.clone(),
                        endpoint,
                    )),
                    Box::new(contact),
                ],
                state: BridgedState::Contact(contact_state),
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
        let BridgedState::OnOff(state) = &endpoint.state else {
            panic!("expected BridgedState::OnOff")
        };
        assert!(!state.load(std::sync::atomic::Ordering::SeqCst));

        let onoff = endpoint
            .clusters
            .iter_mut()
            .find(|c| c.cluster_id() == im::CLUSTER_ON_OFF)
            .expect("OnOff handler registered");
        onoff.invoke(im::CMD_ON_OFF_ON, &[], &mut InvokeCtx::default());

        let BridgedState::OnOff(state) = &endpoint.state else {
            panic!("expected BridgedState::OnOff")
        };
        assert!(state.load(std::sync::atomic::Ordering::SeqCst));
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

    #[test]
    fn switch_and_contact_sensor_kinds_yield_their_cluster_sets() {
        let sw = build_bridged_endpoint(
            DeviceKind::Switch,
            "Btn",
            "uid-2",
            3,
            &GroupMembershipStore::new(),
        );
        let ids: Vec<u32> = sw.clusters.iter().map(|c| c.cluster_id()).collect();
        assert_eq!(
            ids,
            vec![
                im::CLUSTER_DESCRIPTOR,
                im::CLUSTER_BRIDGED_DEVICE_BASIC_INFORMATION,
                im::CLUSTER_IDENTIFY,
                im::CLUSTER_GROUPS,
                im::CLUSTER_SWITCH
            ]
        );
        assert!(matches!(sw.state, BridgedState::Switch(_)));
        let cs = build_bridged_endpoint(
            DeviceKind::ContactSensor,
            "Door",
            "uid-3",
            4,
            &GroupMembershipStore::new(),
        );
        let ids: Vec<u32> = cs.clusters.iter().map(|c| c.cluster_id()).collect();
        assert_eq!(
            ids,
            vec![
                im::CLUSTER_DESCRIPTOR,
                im::CLUSTER_BRIDGED_DEVICE_BASIC_INFORMATION,
                im::CLUSTER_IDENTIFY,
                im::CLUSTER_GROUPS,
                im::CLUSTER_BOOLEAN_STATE
            ]
        );
        assert!(matches!(cs.state, BridgedState::Contact(_)));
    }

    #[test]
    fn deserializes_new_kinds_from_their_config_spelling() {
        assert_eq!(
            serde_json::from_str::<DeviceKind>("\"switch\"").unwrap(),
            DeviceKind::Switch
        );
        assert_eq!(
            serde_json::from_str::<DeviceKind>("\"contact-sensor\"").unwrap(),
            DeviceKind::ContactSensor
        );
    }

    #[test]
    fn switch_descriptor_lists_generic_switch_and_bridged_node() {
        let sw = build_bridged_endpoint(
            DeviceKind::Switch,
            "Btn",
            "uid-2",
            3,
            &GroupMembershipStore::new(),
        );
        let desc = sw
            .clusters
            .iter()
            .find(|c| c.cluster_id() == im::CLUSTER_DESCRIPTOR)
            .unwrap();
        let v = mat_controller::im::tlv_to_json(
            &desc
                .read(
                    im::ATTR_DEVICE_TYPE_LIST,
                    &crate::core::datamodel::ReadCtx::default(),
                )
                .unwrap(),
        )
        .unwrap();
        // DeviceTypeStruct {0: DeviceType, 1: Revision} の配列。
        let types: Vec<u64> = v
            .as_array()
            .unwrap()
            .iter()
            .map(|d| d["0"].as_u64().unwrap())
            .collect();
        assert_eq!(
            types,
            vec![
                u64::from(im::DEVICE_TYPE_GENERIC_SWITCH),
                u64::from(im::DEVICE_TYPE_BRIDGED_NODE)
            ]
        );
    }
}
