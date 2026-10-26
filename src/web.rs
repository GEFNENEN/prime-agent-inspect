//! JSON views for the web UI.
//!
//! The CLI prints tables; the browser needs the same numbers with structure.
//! Both go through this module so a fix to the accounting shows up in both
//! surfaces instead of drifting.

use serde_json::{json, Value};

use crate::calibrate::{calibrate, summarize};
use crate::context::{assemble, compaction_timeline};
use crate::model::Kind;
use crate::overhead::overhead;
use crate::session::Session;

/// One row of the session list.
#[must_use]
pub fn list_row(session: &Session) -> Value {
    let leaf = session.default_leaf().unwrap_or_default();
    let (peak, newest) = if leaf.is_empty() {
        (0, 0)
    } else {
        session.prompt_extremes(leaf)
    };
    let fixed = if leaf.is_empty() {
        None
    } else {
        Some(overhead(session, leaf))
    };
    json!({
        // The list keys on the file stem: it is the stable handle a click can
        // resolve, while the header id can disagree in a copied session file.
        "id": stem(session),
        "headerId": session.header.id,
        "path": session.path.to_string_lossy(),
        "name": session.name(),
        "cwd": session.header.cwd,
        "bytes": session.bytes,
        "entries": session.entries.len(),
        "compactions": session.compaction_count(),
        "peak": peak,
        "newest": newest,
        "fixed": fixed.map(|value| value.tokens),
        "fixedMeasured": fixed.map(|value| value.measured),
        "firstAt": session.first_at(),
        "lastAt": session.last_at(),
        "skippedLines": session.skipped_lines,
    })
}

fn stem(session: &Session) -> String {
    session
        .path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Everything the detail view shows for one session.
#[must_use]
pub fn detail(session: &Session, leaf_id: Option<&str>) -> Value {
    let leaf = leaf_id
        .filter(|id| !id.is_empty())
        .map(str::to_string)
        .or_else(|| session.default_leaf().map(str::to_string));

    let Some(leaf) = leaf else {
        return json!({
            "id": session.header.id,
            "path": session.path.to_string_lossy(),
            "error": "session has no entries",
        });
    };

    let assembly = assemble(session, &leaf);
    let fixed = overhead(session, &leaf);
    let total_tokens = assembly.total_tokens();

    let components: Vec<Value> = assembly
        .components
        .iter()
        .map(|component| {
            let share = if total_tokens == 0 {
                0.0
            } else {
                #[allow(clippy::cast_precision_loss)]
                let tokens = component.tokens as f64;
                #[allow(clippy::cast_precision_loss)]
                let total = total_tokens as f64;
                tokens / total
            };
            json!({
                "label": component.label,
                "chars": component.chars.total(),
                "cjk": component.chars.cjk,
                "tokens": component.tokens,
                "share": share,
                "entries": component.entries,
                "estimated": component.estimated,
            })
        })
        .collect();

    let timeline: Vec<Value> = compaction_timeline(session, &leaf)
        .iter()
        .map(|point| {
            json!({
                "id": point.id,
                "at": point.at,
                "timestamp": point.timestamp,
                "before": point.tokens_before,
                "after": point.prompt_after,
                "retained": point.retained_tokens,
                "notice": point.notice_tokens,
                "copies": point.notice_copies,
                "turns": point.gap_turns,
            })
        })
        .collect();

    let rows = calibrate(session, &leaf);
    let checks: Vec<Value> = rows
        .iter()
        .map(|row| {
            json!({
                "id": row.compaction_id,
                "timestamp": row.timestamp,
                "before": row.tokens_before,
                "measured": row.measured,
                "estimated": row.estimated,
                "error": row.error(),
            })
        })
        .collect();

    let accuracy = summarize(&rows).map(|(median, worst, count)| {
        json!({ "median": median, "worst": worst, "count": count })
    });

    let notices: Vec<Value> = session
        .duplicate_notices
        .iter()
        .map(|(name, count, chars)| json!({ "type": name, "copies": count, "chars": chars }))
        .collect();

    // Retained toolResult rows split by tool: the composition says
    // "toolResult is N%", this says which tool produced it.
    let tool_total: u64 = assembly.tools.iter().map(|share| share.tokens).sum();
    let tools: Vec<Value> = assembly
        .tools
        .iter()
        .map(|share| {
            let share_of = if tool_total == 0 {
                0.0
            } else {
                #[allow(clippy::cast_precision_loss)]
                let tokens = share.tokens as f64;
                #[allow(clippy::cast_precision_loss)]
                let total = tool_total as f64;
                tokens / total
            };
            json!({
                "tool": share.tool,
                "tokens": share.tokens,
                "entries": share.entries,
                "chars": share.chars,
                "share": share_of,
            })
        })
        .collect();

    json!({
        "id": stem(session),
        "headerId": session.header.id,
        "path": session.path.to_string_lossy(),
        "name": session.name(),
        "cwd": session.header.cwd,
        "leaf": leaf,
        "branches": session.leaves.len(),
        "bytes": session.bytes,
        "entries": session.entries.len(),
        "branchEntries": session.branch(&leaf).len(),
        "skippedLines": session.skipped_lines,
        "firstAt": session.first_at(),
        "lastAt": session.last_at(),
        "fixed": {
            "tokens": fixed.tokens,
            "measured": fixed.measured,
            "samples": fixed.samples,
        },
        "prompt": {
            "components": components,
            "tools": tools,
            "contentTokens": total_tokens,
            "fullTokens": total_tokens + fixed.tokens,
            "keptEntries": assembly.kept_entries,
            "droppedEntries": assembly.dropped_entries,
        },
        "timeline": timeline,
        "checks": checks,
        "accuracy": accuracy,
        "notices": notices,
        "entryKinds": entry_kinds(session, &leaf),
    })
}

/// How many entries of each kind sit on the branch, so the UI can show what
/// the file is mostly made of.
fn entry_kinds(session: &Session, leaf: &str) -> Vec<Value> {
    let mut counts: std::collections::HashMap<&str, u64> = std::collections::HashMap::new();
    for index in session.branch(leaf) {
        *counts
            .entry(session.entries[index].kind_name())
            .or_insert(0) += 1;
    }
    let mut rows: Vec<(&str, u64)> = counts.into_iter().collect();
    rows.sort_by_key(|(_, count)| std::cmp::Reverse(*count));
    rows.into_iter()
        .map(|(kind, count)| json!({ "kind": kind, "count": count }))
        .collect()
}

/// A page of transcript entries, newest last, for reading the conversation.
#[must_use]
pub fn transcript(session: &Session, leaf: &str, offset: usize, limit: usize) -> Value {
    let path = session.branch(leaf);
    let total = path.len();
    let start = offset.min(total);
    let end = start.saturating_add(limit).min(total);
    let rows: Vec<Value> = path[start..end]
        .iter()
        .map(|&index| {
            let entry = &session.entries[index];
            let kind = entry.kind_name();
            let (preview, tokens) = match &entry.kind {
                Kind::User { chars } | Kind::Assistant { chars, .. } => {
                    let (tokens, _) = crate::token_est::estimate(*chars);
                    (None, tokens)
                }
                Kind::ToolResult { chars, tool } => {
                    let (tokens, _) = crate::token_est::estimate(*chars);
                    (Some(tool.clone()), tokens)
                }
                Kind::Compaction { summary_chars, tokens_before, .. } => {
                    let (tokens, _) = crate::token_est::estimate(*summary_chars);
                    (Some(format!("{} before", tokens_before)), tokens)
                }
                Kind::CustomMessage { custom_type, chars } => {
                    let (tokens, _) = crate::token_est::estimate(*chars);
                    (Some(custom_type.clone()), tokens)
                }
                _ => (None, 0),
            };
            json!({
                "id": entry.id,
                "parent": entry.parent,
                "at": entry.at,
                "timestamp": entry.timestamp,
                "kind": kind,
                "chars": entry.chars().total(),
                "tokens": tokens,
                "detail": preview,
            })
        })
        .collect();
    json!({ "total": total, "offset": start, "limit": limit, "rows": rows })
}
