use mat_controller::im;
use mat_core::ids::resolve_attribute;
use mat_core::ids::resolve_cluster;

#[test]
fn descriptor_cluster_and_attrs_match_mat_core_ids() {
    assert_eq!(resolve_cluster("descriptor"), Some(im::CLUSTER_DESCRIPTOR));
    let attr = |name: &str| resolve_attribute(im::CLUSTER_DESCRIPTOR, name).unwrap().id;
    assert_eq!(attr("device-type-list"), im::ATTR_DEVICE_TYPE_LIST);
    assert_eq!(attr("server-list"), im::ATTR_SERVER_LIST);
    assert_eq!(attr("parts-list"), im::ATTR_PARTS_LIST);
}

#[test]
fn basic_information_cluster_and_attrs_match_mat_core_ids() {
    assert_eq!(
        resolve_cluster("basicinformation"),
        Some(im::CLUSTER_BASIC_INFORMATION)
    );
    let attr = |name: &str| {
        resolve_attribute(im::CLUSTER_BASIC_INFORMATION, name)
            .unwrap()
            .id
    };
    assert_eq!(attr("data-model-revision"), im::ATTR_DATA_MODEL_REVISION);
    assert_eq!(attr("vendor-name"), im::ATTR_VENDOR_NAME);
    assert_eq!(attr("vendor-id"), im::ATTR_VENDOR_ID);
    assert_eq!(attr("product-name"), im::ATTR_PRODUCT_NAME);
    assert_eq!(attr("product-id"), im::ATTR_PRODUCT_ID);
    // Task 5 additions.
    assert_eq!(attr("node-label"), im::ATTR_BI_NODE_LABEL);
    assert_eq!(attr("location"), im::ATTR_BI_LOCATION);
    assert_eq!(attr("hardware-version"), im::ATTR_BI_HARDWARE_VERSION);
    assert_eq!(
        attr("hardware-version-string"),
        im::ATTR_BI_HARDWARE_VERSION_STRING
    );
    assert_eq!(attr("software-version"), im::ATTR_BI_SOFTWARE_VERSION);
    assert_eq!(
        attr("software-version-string"),
        im::ATTR_BI_SOFTWARE_VERSION_STRING
    );
    assert_eq!(attr("unique-id"), im::ATTR_BI_UNIQUE_ID);
    assert_eq!(attr("capability-minima"), im::ATTR_BI_CAPABILITY_MINIMA);
    assert_eq!(
        attr("specification-version"),
        im::ATTR_BI_SPECIFICATION_VERSION
    );
    assert_eq!(
        attr("max-paths-per-invoke"),
        im::ATTR_BI_MAX_PATHS_PER_INVOKE
    );
}

#[test]
fn switch_and_boolean_state_ids_match_mat_core_ids() {
    assert_eq!(resolve_cluster("switch"), Some(im::CLUSTER_SWITCH));
    let attr = |name: &str| resolve_attribute(im::CLUSTER_SWITCH, name).unwrap().id;
    assert_eq!(
        attr("number-of-positions"),
        im::ATTR_SWITCH_NUMBER_OF_POSITIONS
    );
    assert_eq!(attr("current-position"), im::ATTR_SWITCH_CURRENT_POSITION);
    assert_eq!(attr("multi-press-max"), im::ATTR_SWITCH_MULTI_PRESS_MAX);
    assert_eq!(
        resolve_cluster("booleanstate"),
        Some(im::CLUSTER_BOOLEAN_STATE)
    );
    assert_eq!(
        resolve_attribute(im::CLUSTER_BOOLEAN_STATE, "state-value")
            .unwrap()
            .id,
        im::ATTR_BS_STATE_VALUE
    );
}

// `im::DEVICE_TYPE_ROOT_NODE` (RootNode device type, spec §9.2.2) is
// intentionally not pinned here: `mat_core::ids`'s generated table
// covers clusters/attributes/commands only, not device types — there is
// no `mat_core` lookup to check it against. See the doc comment on the
// constant itself.
