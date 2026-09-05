//! 個別コマンドの CommandFields エンコーダ（ColorControl / LevelControl /
//! GroupKeyManagement / Groups）。汎用 invoke は mat-native が
//! `mat_core::ids` の型表から TLV を組むので、ここにあるのは shortcut 系と
//! group provision 専用の固定形だけ。

use crate::tlv::{Tag, Writer};

/// CommandFields for colorcontrol MoveToHueAndSaturation (cluster spec
/// §3.2.11.7): `{0: hue, 1: saturation, 2: transition_time (0.1 s units),
/// 3: options_mask, 4: options_override}`. Options are fixed to 0 (execute
/// unconditionally), which is what chip-tool sends by default too.
pub fn encode_move_to_hue_and_saturation_fields(
    hue: u8,
    saturation: u8,
    transition_time_ds: u16,
) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(hue));
    w.put_uint(Tag::Context(1), u64::from(saturation));
    w.put_uint(Tag::Context(2), u64::from(transition_time_ds));
    w.put_uint(Tag::Context(3), 0);
    w.put_uint(Tag::Context(4), 0);
    w.end_container();
    w.finish()
}

/// CommandFields for colorcontrol MoveToColorTemperature (cluster spec
/// §3.2.11.10): `{0: ColorTemperatureMireds(u16), 1: TransitionTime(u16,
/// 0.1 s units), 2: OptionsMask(u8), 3: OptionsOverride(u8)}`. Options are
/// fixed to 0 (execute per the device's Options attribute), matching what
/// chip-tool sends by default.
pub fn encode_move_to_color_temperature_fields(mireds: u16, transition_time_ds: u16) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(mireds));
    w.put_uint(Tag::Context(1), u64::from(transition_time_ds));
    w.put_uint(Tag::Context(2), 0);
    w.put_uint(Tag::Context(3), 0);
    w.end_container();
    w.finish()
}

/// CommandFields for levelcontrol MoveToLevel (cluster spec §1.6.7.1):
/// `{0: Level(u8), 1: TransitionTime(u16, 0.1 s units), 2: OptionsMask(u8),
/// 3: OptionsOverride(u8)}`. Options are fixed to 0 (execute per the
/// device's Options attribute), matching what chip-tool sends by default.
pub fn encode_move_to_level_fields(level: u8, transition_time_ds: u16) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(level));
    w.put_uint(Tag::Context(1), u64::from(transition_time_ds));
    w.put_uint(Tag::Context(2), 0);
    w.put_uint(Tag::Context(3), 0);
    w.end_container();
    w.finish()
}

/// CommandFields for GroupKeyManagement KeySetWrite (cluster spec §11.2.8.4):
/// `{0: GroupKeySetStruct{0: GroupKeySetID(u16), 1: GroupKeySecurityPolicy(0
/// = TrustFirst), 2: EpochKey0(16B octstr), 3: EpochStartTime0(u64, epoch-us),
/// 4: EpochKey1(null), 5: EpochStartTime1(null), 6: EpochKey2(null), 7:
/// EpochStartTime2(null)}}`. Only a single active epoch (0) is provisioned —
/// matches the chip-tool `groupkeymanagement key-set-write` JSON this mirrors
/// (`commands/group.rs`'s `key_set` object). `epochStartTime0` is fixed to 1
/// (matching `mat_core::group::EPOCH_START_TIME`, which the controller-side
/// `groupsettings add-keysets` validityTime must also match).
pub fn encode_key_set_write_fields(keyset_id: u16, epoch_key: &[u8; 16]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.start_struct(Tag::Context(0)); // GroupKeySet
    w.put_uint(Tag::Context(0), u64::from(keyset_id));
    w.put_uint(Tag::Context(1), 0); // GroupKeySecurityPolicy: TrustFirst
    w.put_bytes(Tag::Context(2), epoch_key);
    w.put_uint(Tag::Context(3), 1); // EpochStartTime0
    w.put_null(Tag::Context(4));
    w.put_null(Tag::Context(5));
    w.put_null(Tag::Context(6));
    w.put_null(Tag::Context(7));
    w.end_container();
    w.end_container();
    w.finish()
}

/// `group-key-map` attribute Data TLV (list of `GroupKeyMapStruct{1: GroupId,
/// 2: GroupKeySetID}`, spec §11.2.7.6) — the write is a **full replace**, so
/// callers must pass the final merged list (existing entries + the one being
/// added/updated), not just the delta. `fabricIndex` (field 254) is omitted:
/// the server substitutes it from the write's accessing fabric.
pub fn encode_group_key_map_tlv(entries: &[(u16, u16)]) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_array(Tag::Anonymous);
    for (group_id, keyset_id) in entries {
        w.start_struct(Tag::Anonymous);
        w.put_uint(Tag::Context(1), u64::from(*group_id));
        w.put_uint(Tag::Context(2), u64::from(*keyset_id));
        w.end_container();
    }
    w.end_container();
    w.finish()
}

/// CommandFields for Groups AddGroup (cluster spec §1.3.6.1): `{0:
/// GroupID(u16), 1: GroupName(str, <= 16 chars per spec, unchecked here)}`.
pub fn encode_add_group_fields(group_id: u16, name: &str) -> Vec<u8> {
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.put_uint(Tag::Context(0), u64::from(group_id));
    w.put_str(Tag::Context(1), name);
    w.end_container();
    w.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::im::*;
    use crate::tlv::{Reader, Tag, Value};

    #[test]
    fn move_to_hue_and_saturation_fields_shape() {
        let fields = encode_move_to_hue_and_saturation_fields(200, 254, 10);
        let mut r = Reader::new(&fields);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let expect = [
            (0u8, 200u64), // hue
            (1, 254),      // saturation
            (2, 10),       // transition time (0.1s 単位)
            (3, 0),        // options mask
            (4, 0),        // options override
        ];
        for (tag, val) in expect {
            let el = r.next().unwrap().unwrap();
            assert_eq!((el.tag, el.value), (Tag::Context(tag), Value::Uint(val)));
        }
        assert_eq!(r.next().unwrap().unwrap().value, Value::ContainerEnd);
        assert!(r.next().unwrap().is_none());
    }

    #[test]
    fn move_to_color_temperature_fields_match_wire_shape() {
        // CommandFields (colorcontrol MoveToColorTemperature, cluster §3.2.11.10):
        // {0: ColorTemperatureMireds(u16), 1: TransitionTime(u16 0.1s),
        //  2: OptionsMask(u8)=0, 3: OptionsOverride(u8)=0}.
        // MoveToHueAndSaturation エンコーダと同じ手筋（anonymous struct + context tags）。
        let bytes = encode_move_to_color_temperature_fields(370, 30);
        // anonymous struct open (0x15) ... context-tagged uints ... close (0x18)
        assert_eq!(bytes.first(), Some(&0x15), "opens anonymous struct");
        assert_eq!(bytes.last(), Some(&0x18), "closes container");
        // mireds=370=0x0172 が context tag 0 の u16 として載る（0x25 = ctx-tag u16）
        assert!(
            bytes.windows(4).any(|w| w == [0x25, 0x00, 0x72, 0x01]),
            "mireds 370 as ctx-tag-0 u16 little-endian, got {bytes:02X?}"
        );
        // transition=30=0x1E が context tag 1 の u8 として載る（0x24 = ctx-tag u8）
        assert!(
            bytes.windows(3).any(|w| w == [0x24, 0x01, 0x1E]),
            "transition 30 as ctx-tag-1 u8, got {bytes:02X?}"
        );
    }

    #[test]
    fn move_to_level_fields_match_wire_shape() {
        // CommandFields (levelcontrol MoveToLevel, cluster spec §1.6.7.1):
        // {0: Level(u8), 1: TransitionTime(u16 0.1s),
        //  2: OptionsMask(u8)=0, 3: OptionsOverride(u8)=0}.
        // MoveToColorTemperature エンコーダと同じ手筋。
        let bytes = encode_move_to_level_fields(127, 30);
        assert_eq!(bytes.first(), Some(&0x15), "opens anonymous struct");
        assert_eq!(bytes.last(), Some(&0x18), "closes container");
        // level=127=0x7F が context tag 0 の u8 として載る（0x24 = ctx-tag u8）
        assert!(
            bytes.windows(3).any(|w| w == [0x24, 0x00, 0x7F]),
            "level 127 as ctx-tag-0 u8, got {bytes:02X?}"
        );
        // transition=30=0x1E が context tag 1 の u8 として載る
        assert!(
            bytes.windows(3).any(|w| w == [0x24, 0x01, 0x1E]),
            "transition 30 as ctx-tag-1 u8, got {bytes:02X?}"
        );
    }

    // M8a Task9: group provision デバイス側専用エンコーダ。

    #[test]
    fn key_set_write_fields_shape() {
        let f = encode_key_set_write_fields(60, &[0xAB; 16]);
        let mut r = Reader::new(&f);
        let first = r.next().unwrap().unwrap();
        let j = tlv_element_to_json(&mut r, first).unwrap();
        // field 0 = GroupKeySetStruct: {0: 60, 1: 0, 2: "abab..", 3: 1, 4..7: null}
        assert_eq!(j["0"]["0"], serde_json::json!(60));
        assert_eq!(j["0"]["1"], serde_json::json!(0));
        assert_eq!(j["0"]["2"], serde_json::json!("ab".repeat(16)));
        assert_eq!(j["0"]["3"], serde_json::json!(1));
        assert!(j["0"]["4"].is_null() && j["0"]["7"].is_null());
    }

    #[test]
    fn group_key_map_tlv_is_list_of_structs() {
        let t = encode_group_key_map_tlv(&[(10, 60), (11, 61)]);
        let mut r = Reader::new(&t);
        let first = r.next().unwrap().unwrap();
        let j = tlv_element_to_json(&mut r, first).unwrap();
        assert_eq!(
            j,
            serde_json::json!([{"1": 10, "2": 60}, {"1": 11, "2": 61}])
        );
    }

    #[test]
    fn add_group_fields_shape() {
        let f = encode_add_group_fields(10, "grp10");
        let mut r = Reader::new(&f);
        let first = r.next().unwrap().unwrap();
        let j = tlv_element_to_json(&mut r, first).unwrap();
        assert_eq!(j["0"], serde_json::json!(10));
        assert_eq!(j["1"], serde_json::json!("grp10"));
    }
}
