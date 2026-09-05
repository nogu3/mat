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

/// CommandFields for GroupKeyManagement KeySetWrite (cluster spec §11.2.8.1):
/// `{0: GroupKeySetStruct{0: GroupKeySetID(u16), 1: GroupKeySecurityPolicy(0
/// = TrustFirst), 2: EpochKey0(16B octstr), 3: EpochStartTime0(u64, epoch-us),
/// 4: EpochKey1(octstr or null), 5: EpochStartTime1(u64 or null), 6/7: key2/start2}}`.
/// `epochs` contains (epoch_key, start_time) tuples; caller must ensure 1..=3 tuples with
/// monotonic, non-zero start_time (EpochStartTime0 == 0 is INVALID_COMMAND on the device).
/// Missing epochs are filled with null. 4+ epochs is a caller bug (debug_assert; release
/// uses first 3).
pub fn encode_key_set_write_fields_multi(keyset_id: u16, epochs: &[([u8; 16], u64)]) -> Vec<u8> {
    debug_assert!(
        (1..=3).contains(&epochs.len()),
        "KeySetWrite carries 1..=3 epoch keys"
    );
    let mut w = Writer::new();
    w.start_struct(Tag::Anonymous);
    w.start_struct(Tag::Context(0)); // GroupKeySet
    w.put_uint(Tag::Context(0), u64::from(keyset_id));
    w.put_uint(Tag::Context(1), 0); // GroupKeySecurityPolicy: TrustFirst
    for i in 0..3u8 {
        let key_tag = Tag::Context(2 + 2 * i);
        let start_tag = Tag::Context(3 + 2 * i);
        match epochs.get(usize::from(i)) {
            Some((key, start)) => {
                w.put_bytes(key_tag, key);
                w.put_uint(start_tag, *start);
            }
            None => {
                w.put_null(key_tag);
                w.put_null(start_tag);
            }
        }
    }
    w.end_container();
    w.end_container();
    w.finish()
}

/// Single-epoch form of [`encode_key_set_write_fields_multi`] with EpochStartTime0 = 1.
/// Used by `group provision`. **Important:** the value 1 must remain in sync with
/// `mat_core::group::EPOCH_START_TIME`, which the controller-side
/// `groupsettings add-keysets` validity time must also match.
pub fn encode_key_set_write_fields(keyset_id: u16, epoch_key: &[u8; 16]) -> Vec<u8> {
    encode_key_set_write_fields_multi(keyset_id, &[(*epoch_key, 1)])
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

    #[test]
    fn key_set_write_single_epoch_is_the_multi_form_with_one_key() {
        assert_eq!(
            encode_key_set_write_fields(42, &[0xAB; 16]),
            encode_key_set_write_fields_multi(42, &[([0xAB; 16], 1)])
        );
    }

    #[test]
    fn key_set_write_multi_fills_epoch_slots_in_order_and_nulls_the_rest() {
        let tlv = encode_key_set_write_fields_multi(0, &[([0x0C; 16], 1), ([0x0E; 16], 2)]);
        // {0: GroupKeySet{0: id, 1: policy, 2: key0, 3: start0, 4: key1, 5: start1, 6: null, 7: null}}
        let mut r = Reader::new(&tlv);
        assert_eq!(r.next().unwrap().unwrap().value, Value::StructStart);
        let el = r.next().unwrap().unwrap();
        assert_eq!((el.tag, el.value), (Tag::Context(0), Value::StructStart));
        let mut seen = Vec::new();
        loop {
            let el = r.next().unwrap().unwrap();
            if el.value == Value::ContainerEnd {
                break;
            }
            seen.push((el.tag, el.value));
        }
        assert_eq!(
            seen,
            vec![
                (Tag::Context(0), Value::Uint(0)),
                (Tag::Context(1), Value::Uint(0)),
                (Tag::Context(2), Value::Bytes(&[0x0C; 16])),
                (Tag::Context(3), Value::Uint(1)),
                (Tag::Context(4), Value::Bytes(&[0x0E; 16])),
                (Tag::Context(5), Value::Uint(2)),
                (Tag::Context(6), Value::Null),
                (Tag::Context(7), Value::Null),
            ]
        );
    }
}
