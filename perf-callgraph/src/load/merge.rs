//! Combining what the workers built into one physical trie and one time-ordered list of samples.

use super::consumer::{Counters, Local, Row};
use crate::unwinding::phys::PhysTrie;

/// What the readers extracted, merged into one trie and one time-ordered list of samples.
pub(super) struct Merged {
    pub(super) phys: PhysTrie,
    pub(super) rows: Vec<Row>,
    pub(super) counters: Counters,
}

pub(super) fn merge_states(states: Vec<Local>) -> Merged {
    let mut merged = Merged {
        phys: PhysTrie::default(),
        rows: Vec::new(),
        counters: Counters::default(),
    };
    for state in states {
        let map = merged.phys.merge(&state.trie);
        merged.rows.extend(state.rows.iter().map(|row| Row {
            node: map[row.node as usize],
            ..*row
        }));
        let counters = &mut merged.counters;
        counters.samples += state.counters.samples;
        counters.without_stack += state.counters.without_stack;
        counters.frames += state.counters.frames;
        counters.suspect_samples += state.counters.suspect_samples;
        counters.suspect_frames += state.counters.suspect_frames;
        counters.lost_records += state.counters.lost_records;
        counters.lost_events += state.counters.lost_events;
        counters.with_period += state.counters.with_period;
    }
    merged.rows.sort_by_key(|row| (row.time, row.tid, row.node));
    merged
}
