//! Reconstructing the prompt a compaction left behind.
//!
//! The numbers that explain a context problem are not "how big is the file"
//! but "what does the model see on the next turn". `pa-core` builds that view
//! in `build_session_context`: take the newest compaction on the branch, emit
//! its summary, then the retained tail from `firstKeptEntryId` onward.
//!
//! Two rules there are easy to miss and both shape the answer:
//!
//! * only the newest `ipython_state` notice is kept (upstream #3753), so
//!   stacked copies in old files are repaired at assembly rather than counted;
//! * only the newest `harness_digest` is kept, and a compaction's own digest
//!   snapshot outranks it.
//!
//! This module mirrors those rules so the report matches what the model saw.

use crate::model::{Chars, Kind};
use crate::session::Session;
use crate::token_est::estimate;

pub const IPYTHON_STATE: &str = "ipython_state";
pub const HARNESS_DIGEST: &str = "harness_digest";

/// One component of the assembled prompt.
#[derive(Debug, Clone)]
pub struct Component {
    pub label: String,
    pub chars: Chars,
    pub tokens: u64,
    /// How many entries contributed; a notice repeated 3× shows up as one
    /// component with `entries: 3` when the assembler would keep all three.
    pub entries: u64,
    /// True when the token count is extrapolated from characters instead of
    /// measured from provider usage.
    pub estimated: bool,
    /// Indices into `Session::entries` that this bucket holds. The UI needs
    /// them to fetch the underlying text on demand: the parsed session keeps
    /// character counts, never content.
    pub members: Vec<usize>,
}

/// The prompt a compaction leaves behind, broken into labelled parts.
#[derive(Debug, Clone, Default)]
pub struct Assembly {
    pub components: Vec<Component>,
    /// Entries the assembler would replay, in order.
    pub kept_entries: u64,
    /// Entries on the branch that the assembler drops (pre-cut history,
    /// superseded notices and digests).
    pub dropped_entries: u64,
    /// Per-tool breakdown of the kept `toolResult` rows, biggest first. The
    /// semantic grouping above says "toolResult is 55%"; this says which tool
    /// produced it.
    pub tools: Vec<ToolShare>,
}

/// One tool's share of the retained `toolResult` rows.
#[derive(Debug, Clone)]
pub struct ToolShare {
    pub tool: String,
    pub tokens: u64,
    pub entries: u64,
    pub chars: u64,
}

impl Assembly {
    #[must_use]
    pub fn total_tokens(&self) -> u64 {
        self.components.iter().map(|item| item.tokens).sum()
    }

    #[must_use]
    pub fn total_chars(&self) -> Chars {
        let mut chars = Chars::default();
        for item in &self.components {
            chars.cjk += item.chars.cjk;
            chars.other += item.chars.other;
        }
        chars
    }
}

fn push(
    components: &mut Vec<Component>,
    label: &str,
    chars: Chars,
    members: Vec<usize>,
) {
    if chars.total() == 0 && members.is_empty() {
        return;
    }
    let (tokens, _) = estimate(chars);
    components.push(Component {
        label: label.to_string(),
        chars,
        tokens,
        entries: members.len() as u64,
        estimated: true,
        members,
    });
}

/// Assemble the branch's prompt the way `build_session_context` does.
#[must_use]
pub fn assemble(session: &Session, leaf_id: &str) -> Assembly {
    let path = session.branch(leaf_id);
    let Some(end) = path.len().checked_sub(1) else {
        return Assembly::default();
    };
    let marks = Marks::scan(session, &path, end);
    assemble_at(session, &path, end, &marks)
}

/// Newest digest/notice positions up to a given point, so assembling each
/// compaction in a 220-compaction session stays linear instead of rescanning
/// the whole branch every time.
pub struct Marks {
    /// For each path position, the newest `harness_digest` position at or
    /// before it, if any.
    digest_at: Vec<Option<usize>>,
    notice_at: Vec<Option<usize>>,
    /// Path position of each entry id, so a compaction's cut point is a lookup
    /// rather than a scan from the head of the branch.
    position_of: std::collections::HashMap<String, usize>,
}

impl Marks {
    #[must_use]
    pub fn scan(session: &Session, path: &[usize], end: usize) -> Self {
        let mut digest_at = vec![None; end + 1];
        let mut notice_at = vec![None; end + 1];
        let mut position_of = std::collections::HashMap::with_capacity(end + 1);
        let mut digest = None;
        let mut notice = None;
        for position in 0..=end {
            let index = path[position];
            let entry = &session.entries[index];
            position_of.insert(entry.id.clone(), position);
            match &entry.kind {
                Kind::CustomMessage { custom_type, .. } if custom_type == HARNESS_DIGEST => {
                    digest = Some(position);
                }
                Kind::CustomMessage { custom_type, .. } if custom_type == IPYTHON_STATE => {
                    notice = Some(position);
                }
                _ => {}
            }
            digest_at[position] = digest;
            notice_at[position] = notice;
        }
        Self {
            digest_at,
            notice_at,
            position_of,
        }
    }

    fn newest_digest(&self, end: usize) -> Option<usize> {
        self.digest_at.get(end).copied().flatten()
    }

    fn newest_notice(&self, end: usize) -> Option<usize> {
        self.notice_at.get(end).copied().flatten()
    }

    fn cut(&self, first_kept: &str) -> Option<usize> {
        self.position_of.get(first_kept).copied()
    }
}

/// Assemble as if `path[end]` were the leaf.
fn assemble_at(session: &Session, path: &[usize], end: usize, marks: &Marks) -> Assembly {
    let mut assembly = Assembly::default();
    let newest_digest = marks.newest_digest(end);
    let newest_notice = marks.newest_notice(end);

    let compaction_position = (0..=end)
        .rev()
        .find(|&position| matches!(session.entries[path[position]].kind, Kind::Compaction { .. }));

    let mut kept: Vec<usize> = Vec::new();
    match compaction_position {
        None => {
            for &index in path.iter().take(end + 1) {
                if keeps(session, index, newest_digest, newest_notice) {
                    kept.push(index);
                }
            }
        }
        Some(position) => {
            let compaction_index = path[position];
            let Kind::Compaction { first_kept, .. } = &session.entries[compaction_index].kind else {
                unreachable!("position points at a compaction")
            };
            // The cut may name an entry that is not on this branch (it can
            // belong to a sibling); the walk then retains nothing before the
            // compaction, which is what the real assembler does too.
            let start = marks
                .cut(first_kept)
                .filter(|&cut| cut < position)
                .unwrap_or(position);
            for &index in &path[start..position] {
                if keeps(session, index, newest_digest, newest_notice) {
                    kept.push(index);
                }
            }
            kept.push(compaction_index);
            for &index in &path[position + 1..=end] {
                if keeps(session, index, newest_digest, newest_notice) {
                    kept.push(index);
                }
            }
        }
    }

    assembly.kept_entries = kept.len() as u64;
    assembly.dropped_entries = path.len() as u64 - kept.len() as u64;

    // Group by origin so the report answers "what is eating the prompt".
    let mut summary_chars = Chars::default();
    let mut summary_members: Vec<usize> = Vec::new();
    let mut digest_chars = Chars::default();
    let mut digest_members: Vec<usize> = Vec::new();
    let mut notice_chars = Chars::default();
    let mut notice_members: Vec<usize> = Vec::new();
    let mut user_chars = Chars::default();
    let mut user_members: Vec<usize> = Vec::new();
    let mut assistant_chars = Chars::default();
    let mut assistant_members: Vec<usize> = Vec::new();
    let mut tool_chars = Chars::default();
    let mut tool_members: Vec<usize> = Vec::new();
    let mut other_chars = Chars::default();
    let mut other_members: Vec<usize> = Vec::new();
    // Per-tool totals for the retained toolResult rows.
    let mut per_tool: std::collections::HashMap<&str, (Chars, u64)> =
        std::collections::HashMap::new();

    for index in kept {
        let entry = &session.entries[index];
        let chars = entry.chars();
        match &entry.kind {
            Kind::Compaction { .. } | Kind::BranchSummary { .. } => {
                summary_chars.cjk += chars.cjk;
                summary_chars.other += chars.other;
                summary_members.push(index);
            }
            Kind::CustomMessage { custom_type, .. } if custom_type == HARNESS_DIGEST => {
                digest_chars.cjk += chars.cjk;
                digest_chars.other += chars.other;
                digest_members.push(index);
            }
            Kind::CustomMessage { custom_type, .. } if custom_type == IPYTHON_STATE => {
                notice_chars.cjk += chars.cjk;
                notice_chars.other += chars.other;
                notice_members.push(index);
            }
            Kind::User { .. } => {
                user_chars.cjk += chars.cjk;
                user_chars.other += chars.other;
                user_members.push(index);
            }
            Kind::Assistant { .. } => {
                assistant_chars.cjk += chars.cjk;
                assistant_chars.other += chars.other;
                assistant_members.push(index);
            }
            Kind::ToolResult { tool, .. } => {
                tool_chars.cjk += chars.cjk;
                tool_chars.other += chars.other;
                tool_members.push(index);
                let label = normalise_tool(tool);
                let slot = per_tool.entry(label).or_insert((Chars::default(), 0));
                slot.0.cjk += chars.cjk;
                slot.0.other += chars.other;
                slot.1 += 1;
            }
            _ => {
                other_chars.cjk += chars.cjk;
                other_chars.other += chars.other;
                other_members.push(index);
            }
        }
    }

    assembly.tools = per_tool
        .into_iter()
        .map(|(tool, (chars, entries))| {
            let (tokens, _) = estimate(chars);
            ToolShare {
                tool: tool.to_string(),
                tokens,
                entries,
                chars: chars.total(),
            }
        })
        .collect();
    assembly.tools.sort_by_key(|share| std::cmp::Reverse(share.tokens));

    // Estimating each tool separately and summing does not land on the
    // toolResult component's total (rounding differs), and the UI shows both
    // numbers side by side. Distribute the component's total by character
    // share instead, letting the largest bucket absorb the rounding, so the
    // split adds up to the number it claims to expand.
    let (tool_total_tokens, _) = estimate(tool_chars);
    let total_chars: u64 = assembly.tools.iter().map(|share| share.chars).sum();
    if total_chars > 0 && !assembly.tools.is_empty() {
        let mut assigned = 0_u64;
        for share in &mut assembly.tools[1..] {
            #[allow(clippy::cast_precision_loss)]
            let fraction = share.chars as f64 / total_chars as f64;
            #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
            let part = (tool_total_tokens as f64 * fraction).round() as u64;
            share.tokens = part;
            assigned += part;
        }
        if let Some(largest) = assembly.tools.first_mut() {
            largest.tokens = tool_total_tokens.saturating_sub(assigned);
        }
    }

    push(&mut assembly.components, "摘要 summary", summary_chars, summary_members);
    push(&mut assembly.components, "摘要 harness_digest", digest_chars, digest_members);
    push(&mut assembly.components, "kernel ipython_state", notice_chars, notice_members);
    push(&mut assembly.components, "保留 user", user_chars, user_members);
    push(&mut assembly.components, "保留 assistant", assistant_chars, assistant_members);
    push(&mut assembly.components, "保留 toolResult", tool_chars, tool_members);
    push(&mut assembly.components, "保留 其他", other_chars, other_members);

    assembly
        .components
        .sort_by_key(|item| std::cmp::Reverse(item.tokens));
    assembly
}

/// A tool name fit for grouping.
///
/// Older sessions carry a `toolName` that is not a name at all — a streamed
/// call with no function name left the parser's raw text in the field (upstream
/// fixed this in `5e9505f1c`). Such a value would otherwise create a one-off
/// bucket that looks like a real tool, so anything that is not a plausible
/// identifier collapses into one label.
fn normalise_tool(tool: &str) -> &str {
    let plausible = !tool.is_empty()
        && tool.len() <= 40
        && tool
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '/' | ':'));
    if plausible {
        tool
    } else {
        "(unparsed)"
    }
}

/// The assembler drops superseded digests and notices; everything else on the
/// branch inside the cut window is replayed.
fn keeps(
    session: &Session,
    index: usize,
    newest_digest: Option<usize>,
    newest_notice: Option<usize>,
) -> bool {
    let entry = &session.entries[index];
    match &entry.kind {
        Kind::CustomMessage { custom_type, .. } if custom_type == HARNESS_DIGEST => {
            newest_digest == Some(index)
        }
        Kind::CustomMessage { custom_type, .. } if custom_type == IPYTHON_STATE => {
            newest_notice == Some(index)
        }
        Kind::SessionInfo { .. } | Kind::ModelChange { .. } => false,
        _ => true,
    }
}

/// Everything the analysis knows about a compaction, in order.
#[derive(Debug, Clone)]
pub struct CompactionPoint {
    pub id: String,
    pub at: i64,
    pub timestamp: String,
    pub tokens_before: u64,
    pub summary_tokens: u64,
    /// Prompt the provider reported on the first assistant turn after this
    /// compaction, when the session recorded one.
    pub prompt_after: Option<u64>,
    /// Token cost of the retained tail as assembled right after this
    /// compaction, excluding the summary.
    pub retained_tokens: u64,
    pub retained_entries: u64,
    /// The notice copies the assembled context would hold at this point.
    pub notice_copies: u64,
    pub notice_tokens: u64,
    pub gap_turns: u64,
}

/// Walk every compaction on the branch in order and record what it left
/// behind. `prompt_after` comes from the next assistant usage, which is the
/// one number in the file that reflects the real assembled prompt.
#[must_use]
pub fn compaction_timeline(session: &Session, leaf_id: &str) -> Vec<CompactionPoint> {
    let path = session.branch(leaf_id);
    let Some(end) = path.len().checked_sub(1) else {
        return Vec::new();
    };
    let marks = Marks::scan(session, &path, end);
    let mut points: Vec<CompactionPoint> = Vec::new();
    let mut turns_since = 0_u64;

    for position in 0..=end {
        let index = path[position];
        let entry = &session.entries[index];
        match &entry.kind {
            Kind::Assistant { usage, .. } => {
                turns_since += 1;
                let Some(usage) = usage else { continue };
                let prompt = usage.prompt();
                if prompt == 0 {
                    continue;
                }
                // Attribute the first measured prompt after a compaction to it.
                if let Some(last) = points.last_mut() {
                    if last.prompt_after.is_none() {
                        last.prompt_after = Some(prompt);
                    }
                }
            }
            Kind::Compaction {
                tokens_before,
                summary_chars,
                ..
            } => {
                let assembly = assemble_at(session, &path, position, &marks);
                let notice = assembly
                    .components
                    .iter()
                    .find(|item| item.label.contains(IPYTHON_STATE));
                let (summary_tokens, _) = estimate(*summary_chars);
                let retained_tokens = assembly
                    .total_tokens()
                    .saturating_sub(summary_tokens)
                    .saturating_sub(notice.map_or(0, |item| item.tokens));
                points.push(CompactionPoint {
                    id: entry.id.clone(),
                    at: entry.at,
                    timestamp: entry.timestamp.clone(),
                    tokens_before: *tokens_before,
                    summary_tokens,
                    prompt_after: None,
                    retained_tokens,
                    retained_entries: assembly.kept_entries,
                    notice_copies: notice.map_or(0, |item| item.entries),
                    notice_tokens: notice.map_or(0, |item| item.tokens),
                    gap_turns: turns_since,
                });
                turns_since = 0;
            }
            _ => {}
        }
    }
    points
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::load;
    use std::io::Write;
    use std::path::PathBuf;

    struct TempDir(PathBuf);

    impl TempDir {
        fn new() -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("prime-agent-inspect-asm-{}-{unique}", std::process::id()));
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

    fn session_from(lines: &[&str]) -> (TempDir, crate::session::Session) {
        let dir = TempDir::new();
        let path = dir.path().join("s.jsonl");
        let mut file = std::fs::File::create(&path).expect("create");
        for line in lines {
            writeln!(file, "{line}").expect("write");
        }
        let session = load(&path).expect("loads");
        (dir, session)
    }

    /// A streamed call with no function name left garbage in `toolName`; it
    /// must not become its own bucket.
    #[test]
    fn an_unparsed_tool_name_collapses_to_one_bucket() {
        let (_dir, session) = session_from(&[
            r#"{"type":"session","id":"s","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"t1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"toolResult","toolName":"ipython","content":[{"type":"text","text":"ok"}]}}"#,
            r#"{"type":"message","id":"t2","parentId":"t1","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"toolResult","toolName":"'invoke>\n<parameter name='","content":[{"type":"text","text":"Tool not found"}]}}"#,
        ]);
        let assembly = assemble(&session, "t2");
        let names: Vec<&str> = assembly.tools.iter().map(|t| t.tool.as_str()).collect();
        assert!(names.contains(&"ipython"), "real tool kept: {names:?}");
        assert!(
            names.contains(&"(unparsed)"),
            "garbage name collapses: {names:?}"
        );
        assert!(
            !names.iter().any(|name| name.contains("invoke")),
            "the raw parser text must not survive: {names:?}"
        );
    }

    /// The per-tool split must sum to the toolResult component it expands.
    #[test]
    fn tool_shares_add_up_to_the_tool_result_component() {
        let (_dir, session) = session_from(&[
            r#"{"type":"session","id":"s","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"t1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"toolResult","toolName":"ipython","content":[{"type":"text","text":"aaaaaaaaaa"}]}}"#,
            r#"{"type":"message","id":"t2","parentId":"t1","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"toolResult","toolName":"bash","content":[{"type":"text","text":"bbbbbbbbbb"}]}}"#,
        ]);
        let assembly = assemble(&session, "t2");
        let tool_sum: u64 = assembly.tools.iter().map(|t| t.tokens).sum();
        let component = assembly
            .components
            .iter()
            .find(|c| c.label.contains("toolResult"))
            .expect("toolResult component");
        assert_eq!(tool_sum, component.tokens, "shares must not drift");
        assert_eq!(assembly.tools.len(), 2);
    }

    /// The assembler keeps only the newest notice; older copies on the branch
    /// must not inflate the reported prompt.
    #[test]
    fn stacked_notices_collapse_to_one() {
        let (_dir, session) = session_from(&[
            r#"{"type":"session","id":"s","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"go"}}"#,
            r#"{"type":"custom_message","id":"n1","parentId":"u1","timestamp":"2026-01-01T00:00:02.000Z","customType":"ipython_state","content":"aaaa"}"#,
            r#"{"type":"custom_message","id":"n2","parentId":"n1","timestamp":"2026-01-01T00:00:03.000Z","customType":"ipython_state","content":"bbbb"}"#,
            r#"{"type":"custom_message","id":"n3","parentId":"n2","timestamp":"2026-01-01T00:00:04.000Z","customType":"ipython_state","content":"cccc"}"#,
        ]);
        let assembly = assemble(&session, "n3");
        let notice = assembly
            .components
            .iter()
            .find(|item| item.label.contains(IPYTHON_STATE))
            .expect("notice component");
        assert_eq!(notice.entries, 1, "only the newest notice survives");
        assert_eq!(notice.chars.total(), 4);
        assert_eq!(assembly.dropped_entries, 2);
    }

    /// Before any compaction the whole branch is replayed.
    #[test]
    fn no_compaction_keeps_the_whole_branch() {
        let (_dir, session) = session_from(&[
            r#"{"type":"session","id":"s","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"hello"}}"#,
            r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hi"}],"usage":{"input":7,"output":1}}}"#,
        ]);
        let assembly = assemble(&session, "a1");
        assert_eq!(assembly.kept_entries, 2);
        assert_eq!(assembly.dropped_entries, 0);
    }

    /// A compaction replaces everything before `firstKeptEntryId` with its
    /// summary, so the pre-cut rows stop counting.
    #[test]
    fn compaction_drops_history_before_the_cut() {
        let (_dir, session) = session_from(&[
            r#"{"type":"session","id":"s","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"old question that will be summarized away"}}"#,
            r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"old answer"}],"usage":{"input":9,"output":2}}}"#,
            r#"{"type":"message","id":"u2","parentId":"a1","timestamp":"2026-01-01T00:00:03.000Z","message":{"role":"user","content":"new"}}"#,
            r#"{"type":"compaction","id":"c1","parentId":"u2","timestamp":"2026-01-01T00:00:04.000Z","summary":"the summary text","tokensBefore":5000,"firstKeptEntryId":"a1"}"#,
            r#"{"type":"message","id":"a2","parentId":"c1","timestamp":"2026-01-01T00:00:05.000Z","message":{"role":"assistant","content":[{"type":"text","text":"next"}],"usage":{"input":3,"output":1}}}"#,
        ]);
        let assembly = assemble(&session, "a2");
        let labels: Vec<&str> = assembly
            .components
            .iter()
            .map(|item| item.label.as_str())
            .collect();
        assert!(labels.iter().any(|label| label.contains("摘要")));
        // u1 is before the cut; a1 is the cut point and stays.
        assert_eq!(assembly.dropped_entries, 1);
    }

    /// The measured prompt after a compaction is the ground truth the report
    /// compares its estimate against.
    #[test]
    fn timeline_records_the_first_prompt_after_each_compaction() {
        let (_dir, session) = session_from(&[
            r#"{"type":"session","id":"s","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"x"}}"#,
            r#"{"type":"compaction","id":"c1","parentId":"u1","timestamp":"2026-01-01T00:00:02.000Z","summary":"s","tokensBefore":100,"firstKeptEntryId":"u1"}"#,
            r#"{"type":"message","id":"a1","parentId":"c1","timestamp":"2026-01-01T00:00:03.000Z","message":{"role":"assistant","content":[{"type":"text","text":"y"}],"usage":{"input":40,"cacheRead":60,"output":1}}}"#,
        ]);
        let timeline = compaction_timeline(&session, "a1");
        assert_eq!(timeline.len(), 1);
        assert_eq!(timeline[0].tokens_before, 100);
        assert_eq!(timeline[0].prompt_after, Some(100), "prompt includes cacheRead");
    }
}
