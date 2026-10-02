//! Getting the records of a `perf.data` file off the disk fast, and tuning how.
//!
//! - [`ordered`]: the multi-reader pipeline the viewer loads through (regions claimed in file order,
//!   committed in order, then processed in parallel), planned by [`anchor`] over the section found
//!   by [`split`].
//! - [`pipeline`] and [`stream`]: the earlier pipeline, still used to read only the small task and
//!   mmap records.
//! - [`tune`] and [`synth`]: measure the disk and pick reader settings, on the recording or on a
//!   generated one.
//! - [`records`] and [`ctx`]: walk records and parse kernel events.

pub mod anchor;
pub mod ctx;
pub mod ordered;
pub mod pipeline;
pub mod records;
pub mod split;
pub mod stream;
pub mod synth;
pub mod tune;

pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
