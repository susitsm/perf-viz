//! Reference implementation: the stock `linux-perf-data` record iterator.

use std::{fs::File, io::BufReader, path::Path};

use linux_perf_data::{PerfFileReader, PerfFileRecord};

use crate::{Result, stats::Stats};

pub fn read_all(path: &Path, touch: bool) -> Result<Stats> {
    let file = BufReader::with_capacity(1 << 20, File::open(path)?);
    let PerfFileReader {
        mut perf_file,
        mut record_iter,
    } = PerfFileReader::parse_file(file)?;

    let mut stats = Stats::default();
    while let Some(record) = record_iter.next_record(&mut perf_file)? {
        match record {
            PerfFileRecord::EventRecord { record, .. } => {
                stats.count(record.record_type.0, record.data.len() + 8);
                stats.add_event(&record.parse()?, touch);
            }
            PerfFileRecord::UserRecord(record) => {
                stats.count(record.record_type.record_type().0, record.data.len() + 8);
                record.parse()?;
            }
        }
    }
    Ok(stats)
}
