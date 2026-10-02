//! Quality of the unwinding on a real recording. The loader marks the part of a stack from its
//! first return address that does not follow a call instruction as suspect, so the share of
//! suspect stacks measures how often the unwinder walked off the real stack.

use super::recording;
use crate::load::{self, Options};

#[test]
fn nearly_all_stacks_unwind_to_valid_return_addresses() {
    let Some(path) = recording() else { return };
    let profile = load::load(&path, Options::default(), &mut |_| {}).unwrap();
    let stats = &profile.stats;

    let share = |part: u64, whole: u64| part as f64 / whole.max(1) as f64;
    let suspect = share(stats.suspect_samples, stats.samples);
    assert!(
        suspect < 0.01,
        "{} of {} stacks have a suspect tail",
        stats.suspect_samples,
        stats.samples
    );
    let suspect_frames = share(stats.suspect_frames, stats.physical_frames);
    assert!(
        suspect_frames < 0.01,
        "{} of {} frames are suspect",
        stats.suspect_frames,
        stats.physical_frames
    );
    assert_eq!(stats.modules_raw, 0, "modules that could not be opened");
    let unresolved = share(
        stats.unresolved_addresses as u64,
        stats.unique_addresses as u64,
    );
    assert!(
        unresolved < 0.01,
        "{} of {} addresses have no symbol",
        stats.unresolved_addresses,
        stats.unique_addresses
    );
}
