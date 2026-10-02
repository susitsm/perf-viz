use linux_perf_event_reader::{EventRecord, RawData};

pub const MAX_TYPES: usize = 128;

/// Order-independent summary of a file's records. Every field is a commutative
/// aggregate, so readers that visit records in different orders (or in parallel)
/// must produce identical values. The sums also keep the parse from being optimized out.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Stats {
    pub records: u64,
    pub bytes: u64,
    pub per_type: [u64; MAX_TYPES],
    pub samples: u64,
    pub ip_sum: u64,
    pub time_sum: u64,
    pub pid_tid_sum: u64,
    pub callchain_entries: u64,
    pub user_stack_bytes: u64,
    pub user_reg0_sum: u64,
    /// Wrapping sum of every u64 word in user stack dumps (only filled with `touch`).
    pub stack_sum: u64,
}

impl Default for Stats {
    fn default() -> Self {
        Self {
            records: 0,
            bytes: 0,
            per_type: [0; MAX_TYPES],
            samples: 0,
            ip_sum: 0,
            time_sum: 0,
            pid_tid_sum: 0,
            callchain_entries: 0,
            user_stack_bytes: 0,
            user_reg0_sum: 0,
            stack_sum: 0,
        }
    }
}

impl Stats {
    #[inline]
    pub fn count(&mut self, record_type: u32, size: usize) {
        self.records += 1;
        self.bytes += size as u64;
        self.per_type[(record_type as usize).min(MAX_TYPES - 1)] += 1;
    }

    #[inline]
    pub fn add_event(&mut self, ev: &EventRecord<'_>, touch: bool) {
        if let EventRecord::Sample(sample) = ev {
            self.samples += 1;
            self.ip_sum = self.ip_sum.wrapping_add(sample.ip.unwrap_or(0));
            self.time_sum = self.time_sum.wrapping_add(sample.timestamp.unwrap_or(0));
            let pid = sample.pid.unwrap_or(0) as u32 as u64;
            let tid = sample.tid.unwrap_or(0) as u32 as u64;
            self.pid_tid_sum = self.pid_tid_sum.wrapping_add(pid << 32 | tid);
            self.callchain_entries += sample
                .callchain
                .as_ref()
                .map_or(0, |chain| chain.len() as u64);
            if let Some((dump, _)) = &sample.user_stack {
                self.user_stack_bytes += dump.len() as u64;
                if touch {
                    self.stack_sum = self.stack_sum.wrapping_add(sum_words(dump));
                }
            }
            self.user_reg0_sum = self.user_reg0_sum.wrapping_add(
                sample
                    .user_regs
                    .as_ref()
                    .and_then(|registers| registers.get(0))
                    .unwrap_or(0),
            );
        }
    }

    pub fn merge(&mut self, other: &Stats) {
        self.records += other.records;
        self.bytes += other.bytes;
        for (count, other_count) in self.per_type.iter_mut().zip(&other.per_type) {
            *count += other_count;
        }
        self.samples += other.samples;
        self.ip_sum = self.ip_sum.wrapping_add(other.ip_sum);
        self.time_sum = self.time_sum.wrapping_add(other.time_sum);
        self.pid_tid_sum = self.pid_tid_sum.wrapping_add(other.pid_tid_sum);
        self.callchain_entries += other.callchain_entries;
        self.user_stack_bytes += other.user_stack_bytes;
        self.user_reg0_sum = self.user_reg0_sum.wrapping_add(other.user_reg0_sum);
        self.stack_sum = self.stack_sum.wrapping_add(other.stack_sum);
    }
}

/// Wrapping sum of little-endian-agnostic u64 words (byte order doesn't matter for
/// equality between readers on the same file). Written so LLVM vectorizes it.
fn sum_words(data: &RawData<'_>) -> u64 {
    let sum = |bytes: &[u8]| {
        let mut acc = [0u64; 4];
        let mut total = 0u64;
        for word in bytes.chunks_exact(32) {
            for (i, lane) in acc.iter_mut().enumerate() {
                *lane = lane.wrapping_add(u64::from_ne_bytes(
                    word[i * 8..i * 8 + 8].try_into().unwrap(),
                ));
            }
        }
        for lane in acc {
            total = total.wrapping_add(lane);
        }
        for word in bytes.chunks_exact(32).remainder().chunks_exact(8) {
            total = total.wrapping_add(u64::from_ne_bytes(word.try_into().unwrap()));
        }
        total
    };
    match data {
        RawData::Single(bytes) => sum(bytes),
        RawData::Split(first, second) => sum(first).wrapping_add(sum(second)),
    }
}
