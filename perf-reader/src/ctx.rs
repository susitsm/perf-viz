//! What is needed to parse a kernel event record: the per-attribute parse info of the recording.

use byteorder::ByteOrder;
use linux_perf_data::AttributeDescription;
use linux_perf_event_reader::{
    Endianness, RawData, RawEventRecord, RecordIdParseInfo, RecordParseInfo, RecordType,
    get_record_id,
};
use rustc_hash::FxHashMap;

use crate::Result;

#[derive(Clone)]
pub struct Ctx {
    parse_infos: Vec<RecordParseInfo>,
    id_info: RecordIdParseInfo,
    id_to_attr: FxHashMap<u64, usize>,
}

impl Ctx {
    pub fn new(attrs: &[AttributeDescription], endian: Endianness) -> Result<Self> {
        if attrs.is_empty() {
            return Err("no event attributes".into());
        }
        let parse_infos = attrs
            .iter()
            .map(|attribute| RecordParseInfo::new(&attribute.attr, endian))
            .collect();
        let mut id_to_attr = FxHashMap::default();
        for (i, attribute) in attrs.iter().enumerate() {
            for &id in &attribute.event_ids {
                id_to_attr.insert(id, i);
            }
        }
        Ok(Self {
            id_info: RecordIdParseInfo::new(&attrs[0].attr),
            parse_infos,
            id_to_attr,
        })
    }
}

impl Ctx {
    /// Wrap the body of a built-in (kernel event) record for parsing, picking the right attr.
    pub fn raw_event<'a, T: ByteOrder>(
        &self,
        ty: u32,
        misc: u16,
        body: &'a [u8],
    ) -> RawEventRecord<'a> {
        let record_type = RecordType(ty);
        let data = RawData::Single(body);
        let attr_index = if self.parse_infos.len() == 1 {
            0
        } else {
            get_record_id::<T>(record_type, data, &self.id_info)
                .and_then(|id| self.id_to_attr.get(&id).copied())
                .unwrap_or(0)
        };
        RawEventRecord::new(record_type, misc, data, self.parse_infos[attr_index])
    }
}
