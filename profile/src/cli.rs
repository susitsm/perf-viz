//! Command-line flags shared by the loader's tools.

use crate::load::Options;

/// The loader's tuning flags. Each is optional; unset ones keep [`Options::default`].
#[derive(clap::Args, Clone, Debug, Default)]
pub struct LoadArgs {
    /// Threads parsing and unwinding.
    #[arg(long)]
    pub threads: Option<usize>,
    /// Threads reading the file.
    #[arg(long)]
    pub readers: Option<usize>,
    /// Region size, in MiB.
    #[arg(long)]
    pub region_mb: Option<u64>,
    /// Regions open at once.
    #[arg(long)]
    pub in_flight: Option<usize>,
    /// Process a region after this many following ones are committed.
    #[arg(long)]
    pub lookahead: Option<usize>,
    /// Drop the file from the page cache first.
    #[arg(long)]
    pub cold: bool,
    /// Use the single-reader mode.
    #[arg(long)]
    pub sequential: bool,
    /// Shift one planned bound off a record boundary, to exercise the fallback.
    #[arg(long)]
    pub corrupt_bound: bool,
}

impl LoadArgs {
    /// The loader options these flags describe.
    pub fn options(&self) -> Options {
        let mut o = Options::default();
        if let Some(v) = self.threads {
            o.workers = v;
        }
        if let Some(v) = self.readers {
            o.readers = v;
        }
        if let Some(v) = self.region_mb {
            o.region_mb = v;
        }
        if let Some(v) = self.in_flight {
            o.in_flight = v;
        }
        if let Some(v) = self.lookahead {
            o.lookahead = v;
        }
        o.cold = self.cold;
        o.force_sequential = self.sequential;
        o.corrupt_bound = self.corrupt_bound;
        o
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct T {
        #[command(flatten)]
        load: LoadArgs,
    }

    #[test]
    fn unset_flags_keep_the_defaults_and_set_ones_override() {
        let d = Options::default();
        let o = T::try_parse_from(["t"]).unwrap().load.options();
        assert_eq!((o.workers, o.readers, o.region_mb, o.cold), (d.workers, d.readers, d.region_mb, false));
        let o = T::try_parse_from(["t", "--readers", "5", "--threads", "2", "--region-mb", "64", "--in-flight", "9", "--lookahead", "0", "--cold", "--sequential", "--corrupt-bound"]).unwrap().load.options();
        assert_eq!((o.readers, o.workers, o.region_mb, o.in_flight, o.lookahead), (5, 2, 64, 9, 0));
        assert!(o.cold && o.force_sequential && o.corrupt_bound);
    }

    #[test]
    fn bad_numbers_are_errors() {
        assert!(T::try_parse_from(["t", "--readers", "many"]).is_err());
    }
}
