//! The fixed per-request overhead: system prompt plus tool definitions.
//!
//! It is not in the session file. A session's first assistant turn is the one
//! place it can be measured directly: before any compaction the whole branch
//! is the prompt, so
//!
//! ```text
//! fixed = first_measured_prompt - estimate(entries before that turn)
//! ```
//!
//! Every later estimate adds `fixed`. Without it the analyzer reports prompts
//! ~12k tokens too small on a default install, which is exactly the error that
//! makes a real context problem look like a non-problem.
//!
//! The fixed part is not constant: the skills inventory, the harness digest and
//! the MCP server list all grow, so the first turn's value under-reports a late
//! session. The median of the residuals at every compaction is more robust and
//! is preferred whenever at least two compactions carry a measured follow-up.

use crate::model::{Chars, Kind};
use crate::session::Session;
use crate::token_est::estimate;

/// Fallback when a session records no measurable turn.
pub const DEFAULT_FIXED_TOKENS: u64 = 12_000;

/// Fixed overhead for this session, derived from its own measured turns.
#[derive(Debug, Clone, Copy)]
pub struct Overhead {
    pub tokens: u64,
    /// True when derived from measured turns, false when it is the fallback.
    pub measured: bool,
    /// How many measured points the value came from; 0 for the fallback.
    pub samples: usize,
}

#[must_use]
pub fn overhead(session: &Session, leaf_id: &str) -> Overhead {
    let timeline = crate::context::compaction_timeline(session, leaf_id);
    from_timeline(&timeline).unwrap_or_else(|| first_turn_overhead(session, leaf_id))
}

/// Fixed overhead straight from a timeline: each point already records the
/// assembled content it left behind and the prompt the provider then charged
/// for, so the residual needs no second assembly.
#[must_use]
pub fn from_timeline(timeline: &[crate::context::CompactionPoint]) -> Option<Overhead> {
    let mut residuals: Vec<u64> = Vec::new();
    for point in timeline {
        let Some(measured) = point.prompt_after else {
            continue;
        };
        if measured == 0 {
            continue;
        }
        let content = point.summary_tokens + point.retained_tokens + point.notice_tokens;
        if measured > content {
            residuals.push(measured - content);
        }
    }
    if residuals.len() < 2 {
        return None;
    }
    residuals.sort_unstable();
    Some(Overhead {
        tokens: residuals[residuals.len() / 2],
        measured: true,
        samples: residuals.len(),
    })
}

fn first_turn_overhead(session: &Session, leaf_id: &str) -> Overhead {
    let path = session.branch(leaf_id);
    let mut chars = Chars::default();
    for &index in &path {
        let entry = &session.entries[index];
        // The first assistant turn's own output is not part of its prompt:
        // measure up to, not including, it.
        if let Kind::Assistant {
            usage: Some(usage), ..
        } = &entry.kind
        {
            let prompt = usage.prompt();
            if prompt == 0 {
                continue;
            }
            let (content_tokens, _) = estimate(chars);
            // A turn whose content already exceeds the prompt means the file
            // was edited or the estimate is wrong; do not derive garbage.
            if prompt > content_tokens {
                return Overhead {
                    tokens: prompt - content_tokens,
                    measured: true,
                    samples: 1,
                };
            }
            return Overhead {
                tokens: DEFAULT_FIXED_TOKENS,
                measured: false,
                samples: 0,
            };
        }
        let entry_chars = entry.chars();
        chars.cjk += entry_chars.cjk;
        chars.other += entry_chars.other;
    }
    Overhead {
        tokens: DEFAULT_FIXED_TOKENS,
        measured: false,
        samples: 0,
    }
}

/// Assembled prompt plus the fixed overhead: what the provider would see.
#[must_use]
pub fn full_prompt_tokens(session: &Session, leaf_id: &str, overhead: Overhead) -> u64 {
    crate::context::assemble(session, leaf_id).total_tokens() + overhead.tokens
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::path::PathBuf;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("prime-agent-inspect-ovh-{}-{unique}", std::process::id()));
            std::fs::create_dir_all(&path).expect("mkdir");
            Self(path)
        }
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn load_lines(lines: &[&str]) -> (TempDir, Session) {
        let dir = TempDir::new();
        let path = dir.path().join("s.jsonl");
        let mut file = std::fs::File::create(&path).expect("create");
        for line in lines {
            writeln!(file, "{line}").expect("write");
        }
        let session = crate::session::load(&path).expect("loads");
        (dir, session)
    }

    /// A first turn whose prompt exceeds its content reveals the fixed part.
    #[test]
    fn derives_overhead_from_the_first_measured_turn() {
        let (_dir, session) = load_lines(&[
            r#"{"type":"session","id":"s","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"hello"}}"#,
            r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hi"}],"usage":{"input":1100,"cacheRead":0}}}"#,
        ]);
        let overhead = overhead(&session, "a1");
        assert!(overhead.measured);
        // "hello" is 5 chars, ~2 tokens; the rest of the 1100 is fixed.
        assert!(
            overhead.tokens > 1000 && overhead.tokens < 1100,
            "unexpected overhead {}",
            overhead.tokens
        );
    }

    /// No measured turn means the documented fallback, flagged as such.
    #[test]
    fn falls_back_when_nothing_was_measured() {
        let (_dir, session) = load_lines(&[
            r#"{"type":"session","id":"s","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"hello"}}"#,
        ]);
        let overhead = overhead(&session, "u1");
        assert!(!overhead.measured);
        assert_eq!(overhead.tokens, DEFAULT_FIXED_TOKENS);
    }
}
