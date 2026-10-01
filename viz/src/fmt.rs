//! Number formatting for the views.

/// `x` with three significant digits, in plain (non-scientific) notation: 52.6, 4.02, 0.153,
/// 0.0123, 100. Small values keep their precision instead of collapsing to `0.0`. Exactly zero
/// prints as `0`.
pub fn sig3(x: f64) -> String {
    if x == 0.0 {
        return "0".into();
    }
    if !x.is_finite() {
        return x.to_string();
    }
    // Scientific notation with two decimals already rounds to three significant digits, carries
    // included (99.96 -> 1.00e2), so the digits and exponent can be trusted.
    let s = format!("{:.2e}", x.abs());
    let (mantissa, exp) = s.split_once('e').expect("scientific notation has an exponent");
    let e: i32 = exp.parse().expect("exponent is an integer");
    let digits: String = mantissa.chars().filter(|&c| c != '.').collect();
    let body = if e >= 2 {
        format!("{digits}{}", "0".repeat((e - 2) as usize))
    } else if e >= 0 {
        let k = (e + 1) as usize;
        format!("{}.{}", &digits[..k], &digits[k..])
    } else {
        format!("0.{}{digits}", "0".repeat((-e - 1) as usize))
    };
    if x < 0.0 { format!("-{body}") } else { body }
}

/// What the views add up: the number of samples, or the sum of the samples' periods (cycles, for a
/// cycles event), which weighs each sample by the work it stands for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum Cost {
    Samples,
    #[default]
    Cycles,
}

impl Cost {
    /// A cost value for display: exact for samples, three significant digits with a unit for cycles.
    pub fn format(self, v: u64) -> String {
        match self {
            Cost::Samples => v.to_string(),
            Cost::Cycles => si(v as f64),
        }
    }
}

/// `x` with three significant digits and a K/M/G/T suffix: `999`, `1.23 K`, `53.8 G`.
pub fn si(x: f64) -> String {
    if x < 1000.0 {
        return format!("{}", x.round() as u64);
    }
    // Round first, so 999_600 (1000 K after rounding) is shown as 1.00 M.
    let r: f64 = sig3(x).parse().unwrap_or(x);
    let (scale, suffix) = if r >= 1e12 { (1e12, "T") } else if r >= 1e9 { (1e9, "G") } else if r >= 1e6 { (1e6, "M") } else { (1e3, "K") };
    format!("{} {suffix}", sig3(r / scale))
}

/// A percentage: `fraction` of `whole`, as `sig3` digits (no `%` sign). Zero if `whole` is zero.
pub fn pct_of(part: f64, whole: f64) -> String {
    if whole == 0.0 { "0".into() } else { sig3(100.0 * part / whole) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn three_significant_digits_at_every_magnitude() {
        assert_eq!(sig3(52.6), "52.6");
        assert_eq!(sig3(52.64), "52.6");
        assert_eq!(sig3(4.0245), "4.02");
        assert_eq!(sig3(7.0), "7.00");
        assert_eq!(sig3(0.15349), "0.153");
        assert_eq!(sig3(0.012345), "0.0123");
        assert_eq!(sig3(0.0000123), "0.0000123");
        assert_eq!(sig3(100.0), "100");
    }

    #[test]
    fn rounding_carries_into_the_next_magnitude() {
        assert_eq!(sig3(99.96), "100");
        assert_eq!(sig3(9.996), "10.0");
        assert_eq!(sig3(0.09996), "0.100");
    }

    #[test]
    fn si_units_keep_three_significant_digits() {
        assert_eq!(si(0.0), "0");
        assert_eq!(si(999.0), "999");
        assert_eq!(si(1234.0), "1.23 K");
        assert_eq!(si(1_322_465.0), "1.32 M");
        assert_eq!(si(53_818_498_413.0), "53.8 G");
        assert_eq!(si(999_600.0), "1.00 M", "rounding up crosses into the next unit");
        assert_eq!(si(12_345_678_901_234.0), "12.3 T");
        assert_eq!(Cost::Samples.format(41390), "41390");
        assert_eq!(Cost::Cycles.format(41390), "41.4 K");
    }

    #[test]
    fn zero_and_edge_cases() {
        assert_eq!(sig3(0.0), "0");
        assert_eq!(sig3(-1.234), "-1.23");
        assert_eq!(pct_of(1.0, 0.0), "0");
        assert_eq!(pct_of(1.0, 8.0), "12.5");
        assert_eq!(pct_of(1.0, 1000.0), "0.100");
    }

    #[test]
    fn always_three_digits_that_are_not_leading_zeros() {
        for x in [0.00417, 0.0417, 0.417, 4.17, 41.7, 99.0, 12.0, 0.5] {
            let digits: Vec<char> = sig3(x).chars().filter(|c| c.is_ascii_digit()).skip_while(|&c| c == '0').collect();
            assert!(digits.len() >= 3, "{x} -> {} has fewer than three significant digits", sig3(x));
        }
    }
}
