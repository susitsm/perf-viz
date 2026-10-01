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
        if let EventRecord::Sample(s) = ev {
            self.samples += 1;
            self.ip_sum = self.ip_sum.wrapping_add(s.ip.unwrap_or(0));
            self.time_sum = self.time_sum.wrapping_add(s.timestamp.unwrap_or(0));
            let pid = s.pid.unwrap_or(0) as u32 as u64;
            let tid = s.tid.unwrap_or(0) as u32 as u64;
            self.pid_tid_sum = self.pid_tid_sum.wrapping_add(pid << 32 | tid);
            self.callchain_entries += s.callchain.as_ref().map_or(0, |c| c.len() as u64);
            if let Some((d, _)) = &s.user_stack {
                self.user_stack_bytes += d.len() as u64;
                if touch {
                    self.stack_sum = self.stack_sum.wrapping_add(sum_words(d));
                }
            }
            self.user_reg0_sum = self
                .user_reg0_sum
                .wrapping_add(s.user_regs.as_ref().and_then(|r| r.get(0)).unwrap_or(0));
        }
    }

    pub fn merge(&mut self, o: &Stats) {
        self.records += o.records;
        self.bytes += o.bytes;
        for (a, b) in self.per_type.iter_mut().zip(&o.per_type) {
            *a += b;
        }
        self.samples += o.samples;
        self.ip_sum = self.ip_sum.wrapping_add(o.ip_sum);
        self.time_sum = self.time_sum.wrapping_add(o.time_sum);
        self.pid_tid_sum = self.pid_tid_sum.wrapping_add(o.pid_tid_sum);
        self.callchain_entries += o.callchain_entries;
        self.user_stack_bytes += o.user_stack_bytes;
        self.user_reg0_sum = self.user_reg0_sum.wrapping_add(o.user_reg0_sum);
        self.stack_sum = self.stack_sum.wrapping_add(o.stack_sum);
    }
}

/// Wrapping sum of little-endian-agnostic u64 words (byte order doesn't matter for
/// equality between readers on the same file). Written so LLVM vectorizes it.
fn sum_words(d: &RawData<'_>) -> u64 {
    let sum = |b: &[u8]| {
        let mut acc = [0u64; 4];
        let mut total = 0u64;
        for w in b.chunks_exact(32) {
            for (i, a) in acc.iter_mut().enumerate() {
                *a = a.wrapping_add(u64::from_ne_bytes(w[i * 8..i * 8 + 8].try_into().unwrap()));
            }
        }
        for a in acc {
            total = total.wrapping_add(a);
        }
        for w in b.chunks_exact(32).remainder().chunks_exact(8) {
            total = total.wrapping_add(u64::from_ne_bytes(w.try_into().unwrap()));
        }
        total
    };
    match d {
        RawData::Single(b) => sum(b),
        RawData::Split(a, b) => sum(a).wrapping_add(sum(b)),
    }
}
