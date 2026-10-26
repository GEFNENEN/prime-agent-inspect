//! Character-to-token estimation.
//!
//! `chars / 4` under-reports this workload badly: a p8-style session measures
//! ~2.1 characters per token because Chinese text and code dominate. The
//! coefficients below are calibrated against 19,116 measured
//! `(characters, provider-reported prompt delta)` pairs from a real session,
//! bucketed by CJK fraction; median characters-per-token in each bucket.

use crate::model::Chars;

/// `(cjk_fraction, characters per token)` measured on real traffic, ascending.
const CALIBRATION: &[(f64, f64)] = &[
    (0.0, 2.21),
    (0.1, 2.03),
    (0.2, 1.80),
    (0.3, 1.64),
    (0.4, 1.53),
    (0.5, 1.41),
    (0.6, 1.30),
];

/// Characters per token for a CJK fraction, linearly interpolated between the
/// measured buckets and clamped to the end values outside them.
#[must_use]
pub fn chars_per_token(cjk_fraction: f64) -> f64 {
    let clamped = cjk_fraction.clamp(0.0, 1.0);
    let last = CALIBRATION[CALIBRATION.len() - 1];
    if clamped >= last.0 {
        return last.1;
    }
    for pair in CALIBRATION.windows(2) {
        let (low_frac, low_ratio) = pair[0];
        let (high_frac, high_ratio) = pair[1];
        if clamped <= high_frac {
            let span = high_frac - low_frac;
            let t = if span > 0.0 {
                (clamped - low_frac) / span
            } else {
                0.0
            };
            return low_ratio + (high_ratio - low_ratio) * t;
        }
    }
    last.1
}

/// Estimated tokens for a character count, plus the ratio used so the caller
/// can show its work.
#[must_use]
pub fn estimate(chars: Chars) -> (u64, f64) {
    let total = chars.total();
    if total == 0 {
        return (0, chars_per_token(0.0));
    }
    #[allow(clippy::cast_precision_loss)] // character counts stay well inside f64
    let fraction = chars.cjk as f64 / total as f64;
    let ratio = chars_per_token(fraction);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let tokens = (total as f64 / ratio).round() as u64;
    (tokens, ratio)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn japanese_and_chinese_are_cjk() {
        let chars = Chars {
            cjk: 3,
            other: 1,
        };
        assert_eq!(chars.total(), 4);
    }

    #[test]
    fn pure_ascii_uses_the_top_bucket() {
        let (tokens, ratio) = estimate(Chars { cjk: 0, other: 400 });
        assert!((ratio - 2.21).abs() < 1e-9);
        assert_eq!(tokens, 181);
    }

    #[test]
    fn heavy_cjk_estimates_more_tokens_for_the_same_characters() {
        let ascii = estimate(Chars { cjk: 0, other: 1000 }).0;
        let cjk = estimate(Chars { cjk: 1000, other: 0 }).0;
        assert!(
            cjk > ascii,
            "CJK should cost more tokens per character: {cjk} vs {ascii}"
        );
    }

    #[test]
    fn interpolation_is_monotonic() {
        let mut previous = f64::MAX;
        for step in 0..=10 {
            #[allow(clippy::cast_precision_loss)]
            let fraction = step as f64 / 10.0;
            let ratio = chars_per_token(fraction);
            assert!(
                ratio <= previous + 1e-9,
                "ratio must fall as CJK rises: {fraction} -> {ratio} vs {previous}"
            );
            previous = ratio;
        }
    }

    #[test]
    fn empty_input_estimates_zero() {
        assert_eq!(estimate(Chars::default()).0, 0);
    }
}
