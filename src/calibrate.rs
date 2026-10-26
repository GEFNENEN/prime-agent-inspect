//! Cross-checking the estimate against the provider's own numbers.
//!
//! The estimate is only trustworthy if it lands near reality on real files.
//! `calibrate` replays a session: after each compaction the assembled prompt
//! is estimated, and the first assistant turn after it reports what the
//! provider actually charged for. The difference is the estimate's error.
//!
//! Reconstruction runs once per compaction, so a session with 220 compactions
//! pays 220 branch walks. That is the cost of measuring a real quantity; the
//! web UI caches the result per file revision rather than recomputing it.

use crate::overhead::{overhead, Overhead};
use crate::session::Session;

#[derive(Debug, Clone)]
pub struct Reckoning {
    pub compaction_id: String,
    pub timestamp: String,
    pub tokens_before: u64,
    /// Provider-reported prompt on the first turn after the compaction.
    pub measured: u64,
    /// What the analyzer reconstructs for that same prompt.
    pub estimated: u64,
}

impl Reckoning {
    #[must_use]
    pub fn error(&self) -> f64 {
        if self.measured == 0 {
            return 0.0;
        }
        #[allow(clippy::cast_precision_loss)]
        let measured = self.measured as f64;
        #[allow(clippy::cast_precision_loss)]
        let estimated = self.estimated as f64;
        (estimated - measured) / measured
    }
}

/// One row per compaction that has a measured follow-up prompt.
///
/// Everything needed is already in the timeline: the assembly it recorded is
/// exactly the state the next turn sent. Rebuilding it per compaction would
/// make this quadratic on a session with hundreds of compactions.
#[must_use]
pub fn calibrate(session: &Session, leaf_id: &str) -> Vec<Reckoning> {
    let fixed = overhead(session, leaf_id);
    let timeline = crate::context::compaction_timeline(session, leaf_id);
    let mut out = Vec::new();
    for point in timeline {
        let Some(measured) = point.prompt_after else {
            continue;
        };
        if measured == 0 {
            continue;
        }
        // The summary and the retained tail plus the notice are what the
        // assembler emitted; the fixed overhead is not in the file.
        let estimated = point.summary_tokens + point.retained_tokens + point.notice_tokens + fixed.tokens;
        out.push(Reckoning {
            compaction_id: point.id,
            timestamp: point.timestamp,
            tokens_before: point.tokens_before,
            measured,
            estimated,
        });
    }
    out
}

/// Same rows, split by whether the fixed overhead was measured or assumed, so
/// a report can say how much to trust its own numbers.
#[must_use]
pub fn trust(session: &Session, leaf_id: &str) -> Overhead {
    overhead(session, leaf_id)
}

/// Median and worst-case relative error, for a one-line confidence statement.
#[must_use]
pub fn summarize(rows: &[Reckoning]) -> Option<(f64, f64, usize)> {
    if rows.is_empty() {
        return None;
    }
    let mut errors: Vec<f64> = rows.iter().map(|row| row.error().abs()).collect();
    errors.sort_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal));
    let median = errors[errors.len() / 2];
    let worst = errors.last().copied().unwrap_or_default();
    Some((median, worst, rows.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_is_relative_and_signed() {
        let over = Reckoning {
            compaction_id: "c".into(),
            timestamp: String::new(),
            tokens_before: 0,
            measured: 100,
            estimated: 110,
        };
        assert!((over.error() - 0.1).abs() < 1e-9);
        let under = Reckoning {
            measured: 100,
            estimated: 90,
            ..over.clone()
        };
        assert!((under.error() + 0.1).abs() < 1e-9);
    }

    #[test]
    fn a_zero_measurement_does_not_divide_by_zero() {
        let row = Reckoning {
            compaction_id: "c".into(),
            timestamp: String::new(),
            tokens_before: 0,
            measured: 0,
            estimated: 50,
        };
        assert!((row.error() - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn summary_reports_median_and_worst() {
        let rows: Vec<Reckoning> = [0.1_f64, -0.3, 0.2]
            .iter()
            .map(|error| Reckoning {
                compaction_id: "c".into(),
                timestamp: String::new(),
                tokens_before: 0,
                measured: 100,
                estimated: (100.0 * (1.0 + error)).round() as u64,
            })
            .collect();
        let (median, worst, count) = summarize(&rows).expect("summary");
        assert_eq!(count, 3);
        assert!(median <= worst);
        assert!((worst - 0.3).abs() < 0.01, "worst should be 30%: {worst}");
    }
}
