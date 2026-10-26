//! JSON views for the web UI.
//!
//! The CLI prints tables; the browser needs the same numbers with structure.
//! Both go through this module so a fix to the accounting shows up in both
//! surfaces instead of drifting.

use serde_json::{json, Value};

use crate::calibrate::{calibrate, summarize};
use crate::context::{assemble, compaction_timeline};
use crate::model::{entry_blocks, Kind};
use crate::overhead::overhead;
use crate::session::Session;

/// One row of the session list.
#[must_use]
pub fn list_row(session: &Session) -> Value {
    let leaf = session.default_leaf().unwrap_or_default();
    let (peak, peak_at, newest) = if leaf.is_empty() {
        (0, 0, 0)
    } else {
        session.prompt_peak(leaf)
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
        "peakAt": peak_at,
        "newest": newest,
        "fixed": fixed.map(|value| value.tokens),
        "fixedMeasured": fixed.map(|value| value.measured),
        "firstAt": session.first_at(),
        "lastAt": session.last_at(),
        "turns": if leaf.is_empty() {
            0
        } else {
            turn_count(session, leaf)
        },
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
    let (peak, peak_at, newest) = session.prompt_peak(&leaf);
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
                // Entry ids let the browser ask for the underlying text of
                // exactly this bucket. They are not in this payload: a 225-
                // compaction session would carry megabytes of duplicates.
                "members": component
                    .members
                    .iter()
                    .map(|&index| session.entries[index].id.as_str())
                    .collect::<Vec<_>>(),
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
                "retainedEntries": point.retained_entries,
                "summary": point.summary_tokens,
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
        // Provider-reported prompts on this branch. `peak` answers "how close
        // did this session get to the ceiling"; `newest` is the last one the
        // provider actually billed, which the composition above reconstructs.
        "peak": peak,
        "peakAt": peak_at,
        "newest": newest,
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
        "models": model_usage(session, &leaf),
        "cache": cache_stats(session, &leaf),
        "tools": tool_stats(session, &leaf),
        "harness": harness_stats(session, &leaf),
        "refinements": refinement_log(session, &leaf),
        "commits": commit_log(session, &leaf),
        "turns": turn_count(session, &leaf),
        "callsPerTurn": calls_per_turn(session, &leaf),
        "daily": daily_activity(session, &leaf),
        "accuracy": accuracy,
        "notices": notices,
        "entryKinds": entry_kinds(session, &leaf),
        "time": time_split(session, &leaf),
        "toolTime": tool_time(session, &leaf),
        "biggest": biggest(session, &leaf, 40),
        "output": output_split(session, &leaf),
        "children": child_roster(session, &leaf),
        "failures": failure_log(session, &leaf),
    })
}

/// One row per provider/model the session actually ran on, so a session that
/// switched providers mid-run does not look like a single number.
fn model_usage(session: &Session, leaf: &str) -> Vec<Value> {
    struct Row {
        calls: u64,
        peak: u64,
        prompt: u64,
        output: u64,
        first: i64,
        last: i64,
        skipped: u64,
    }
    let mut rows: std::collections::HashMap<String, Row> = std::collections::HashMap::new();
    for index in session.branch(leaf) {
        let entry = &session.entries[index];
        let Kind::Assistant {
            usage: Some(usage),
            model: Some(model),
            ..
        } = &entry.kind
        else {
            continue;
        };
        let row = rows.entry(model.clone()).or_insert(Row {
            calls: 0,
            peak: 0,
            prompt: 0,
            output: 0,
            first: entry.at,
            last: entry.at,
            skipped: 0,
        });
        row.calls += 1;
        row.peak = row.peak.max(usage.prompt());
        // Same rule as the cache stats: a row whose parts disagree with its
        // total carries an inflated `cacheRead`, so it would add tokens that
        // were never sent.
        if usage.parts_agree() {
            row.prompt += usage.prompt();
            row.output += usage.output;
        } else {
            row.skipped += 1;
        }
        row.first = row.first.min(entry.at);
        row.last = row.last.max(entry.at);
    }
    let mut out: Vec<Value> = rows
        .into_iter()
        .map(|(model, row)| {
            json!({
                "model": model,
                "calls": row.calls,
                "peak": row.peak,
                "prompt": row.prompt,
                "output": row.output,
                "firstAt": row.first,
                "lastAt": row.last,
                "skipped": row.skipped,
            })
        })
        .collect();
    out.sort_by_key(|row| std::cmp::Reverse(row["calls"].as_u64().unwrap_or_default()));
    out
}

/// Cache behaviour, measured rather than assumed: `cacheRead` is the part of
/// the prompt the provider served from its prefix cache, so the ratio says how
/// much of everything sent was a hit.
///
/// Only rows whose parts agree with their total are counted. The 61 corrupt
/// rows in the p8 session carry 27% of all `cacheRead` on their own — leaving
/// them in would move the hit rate by several points and no reader could tell.
fn cache_stats(session: &Session, leaf: &str) -> Value {
    let mut read = 0_u64;
    let mut write = 0_u64;
    let mut fresh = 0_u64;
    let mut output = 0_u64;
    let mut calls = 0_u64;
    let mut no_cache_calls = 0_u64;
    let mut skipped = 0_u64;
    for index in session.branch(leaf) {
        let Kind::Assistant {
            usage: Some(usage), ..
        } = &session.entries[index].kind
        else {
            continue;
        };
        if usage.prompt() == 0 {
            continue;
        }
        if !usage.parts_agree() {
            skipped += 1;
            continue;
        }
        calls += 1;
        read += usage.cache_read;
        write += usage.cache_write;
        fresh += usage.input;
        output += usage.output;
        if usage.cache_read == 0 && usage.cache_write == 0 {
            no_cache_calls += 1;
        }
    }
    let sent = fresh + read + write;
    let hit = if sent == 0 {
        0.0
    } else {
        #[allow(clippy::cast_precision_loss)]
        let read = read as f64;
        #[allow(clippy::cast_precision_loss)]
        let sent = sent as f64;
        read / sent
    };
    json!({
        "calls": calls,
        "cacheRead": read,
        "cacheWrite": write,
        "fresh": fresh,
        "output": output,
        "sent": sent,
        "hit": hit,
        "noCacheCalls": no_cache_calls,
        "skipped": skipped,
    })
}

/// Every tool the branch used, with calls, failures and characters. The
/// composition card says "toolResult is N%"; this says which tool and how often
/// it failed, which is where the retries come from.
fn tool_stats(session: &Session, leaf: &str) -> Vec<Value> {
    struct Row {
        calls: u64,
        errors: u64,
        chars: u64,
        tokens: u64,
    }
    let mut rows: std::collections::HashMap<String, Row> = std::collections::HashMap::new();
    for index in session.branch(leaf) {
        let Kind::ToolResult {
            chars, tool, is_error, ..
        } = &session.entries[index].kind
        else {
            continue;
        };
        let (tokens, _) = crate::token_est::estimate(*chars);
        let row = rows.entry(tool.clone()).or_insert(Row {
            calls: 0,
            errors: 0,
            chars: 0,
            tokens: 0,
        });
        row.calls += 1;
        row.tokens += tokens;
        row.chars += chars.total();
        if *is_error {
            row.errors += 1;
        }
    }
    let total_chars: u64 = rows.values().map(|row| row.chars).sum();
    let mut out: Vec<Value> = rows
        .into_iter()
        .map(|(tool, row)| {
            let share = if total_chars == 0 {
                0.0
            } else {
                #[allow(clippy::cast_precision_loss)]
                let chars = row.chars as f64;
                #[allow(clippy::cast_precision_loss)]
                let total = total_chars as f64;
                chars / total
            };
            json!({
                "tool": tool,
                "calls": row.calls,
                "errors": row.errors,
                "chars": row.chars,
                "tokens": row.tokens,
                "share": share,
            })
        })
        .collect();
    out.sort_by_key(|row| std::cmp::Reverse(row["chars"].as_u64().unwrap_or_default()));
    out
}

/// One row per subagent session: what it cost and how long it lived.
///
/// `child_usage_attributed` names its target by runtime id, which is not the
/// child's session id and cannot be resolved to a name from this file. What it
/// does carry is trustworthy: the *parts* of `aggregateUsage` are a running
/// total (the `totalTokens` beside them is frozen at the child's first call),
/// so the last row per target is that child's final spend.
fn child_roster(session: &Session, leaf: &str) -> Vec<Value> {
    struct Row {
        prompt: u64,
        output: u64,
        origin: String,
        first: i64,
        last: i64,
        rows: u64,
    }
    let mut rows: std::collections::HashMap<String, Row> = std::collections::HashMap::new();
    for index in session.branch(leaf) {
        let Kind::ChildUsage {
            target,
            origin,
            prompt,
            output,
        } = &session.entries[index].kind
        else {
            continue;
        };
        let at = session.entries[index].at;
        let row = rows.entry(target.clone()).or_insert_with(|| Row {
            prompt: 0,
            output: 0,
            origin: origin.clone(),
            first: at,
            last: at,
            rows: 0,
        });
        // The running total only grows, so the latest row wins; taking the max
        // also survives a session file that was resumed with a reset counter.
        row.prompt = row.prompt.max(*prompt);
        row.output = row.output.max(*output);
        row.first = row.first.min(at);
        row.last = row.last.max(at);
        row.rows += 1;
    }
    let mut out: Vec<Value> = rows
        .into_iter()
        .map(|(target, row)| {
            json!({
                "target": target,
                "origin": row.origin,
                "prompt": row.prompt,
                "output": row.output,
                "firstAt": row.first,
                "lastAt": row.last,
                "rows": row.rows,
            })
        })
        .collect();
    out.sort_by(|a, b| {
        b["prompt"]
            .as_u64()
            .unwrap_or_default()
            .cmp(&a["prompt"].as_u64().unwrap_or_default())
    });
    out
}

/// Everything that went wrong, in one place.
///
/// These records are scattered across six custom types and none of them is
/// visible in any other view; a session with 39 empty model replies and 27
/// failed retries otherwise looks perfectly healthy.
fn failure_log(session: &Session, leaf: &str) -> Value {
    let mut retries: Vec<Value> = Vec::new();
    let mut retry_failed = 0_u64;
    let mut attempts_max = 0_u64;
    let mut empty_replies = 0_u64;
    let mut compaction_failed = 0_u64;
    let mut child_failures: Vec<Value> = Vec::new();
    let mut tool_errors: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    let mut tool_errors_total = 0_u64;
    for index in session.branch(leaf) {
        let entry = &session.entries[index];
        if let Kind::ToolResult {
            tool, is_error, chars, ..
        } = &entry.kind
        {
            if *is_error {
                tool_errors_total += 1;
                *tool_errors.entry(tool.clone()).or_insert(0) += 1;
                let _ = chars;
            }
            continue;
        }
        let Kind::CustomMessage {
            custom_type,
            detail,
            ..
        } = &entry.kind
        else {
            continue;
        };
        let detail = detail.as_deref();
        let field_u64 = |key: &str| {
            detail
                .and_then(|value| value.get(key))
                .and_then(Value::as_u64)
                .unwrap_or_default()
        };
        match custom_type.as_str() {
            "provider_retry_outcome" => {
                let attempts = field_u64("attempts");
                let success = detail
                    .and_then(|value| value.get("success"))
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                attempts_max = attempts_max.max(attempts);
                if !success {
                    retry_failed += 1;
                }
                let error = detail
                    .and_then(|value| value.get("finalError"))
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(120)
                    .collect::<String>();
                retries.push(json!({
                    "at": entry.at,
                    "attempts": attempts,
                    "success": success,
                    "error": error,
                }));
            }
            "model_prompt_error" => empty_replies += 1,
            "compaction_outcome" => {
                let failed = detail
                    .and_then(|value| value.get("outcome"))
                    .and_then(Value::as_str)
                    .map(|text| text != "ok" && text != "success")
                    .unwrap_or(false);
                if failed {
                    compaction_failed += 1;
                }
            }
            "rlm_child_failure" => {
                child_failures.push(json!({
                    "at": entry.at,
                    "error": detail
                        .and_then(|value| value.get("error"))
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .chars()
                        .take(160)
                        .collect::<String>(),
                    "name": detail
                        .and_then(|value| value.get("sessionName"))
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                }));
            }
            _ => {}
        }
    }
    let mut tools: Vec<Value> = tool_errors
        .into_iter()
        .map(|(tool, count)| json!({ "tool": tool, "errors": count }))
        .collect();
    tools.sort_by(|a, b| {
        b["errors"]
            .as_u64()
            .unwrap_or_default()
            .cmp(&a["errors"].as_u64().unwrap_or_default())
    });
    retries.sort_by_key(|row| std::cmp::Reverse(row["at"].as_i64().unwrap_or_default()));
    child_failures.sort_by_key(|row| std::cmp::Reverse(row["at"].as_i64().unwrap_or_default()));
    json!({
        "providerRetries": retries.len(),
        "providerRetryFailed": retry_failed,
        "attemptsMax": attempts_max,
        "retries": retries.into_iter().take(20).collect::<Vec<_>>(),
        "emptyReplies": empty_replies,
        "compactionFailed": compaction_failed,
        "childFailures": child_failures.len(),
        "childFailureRows": child_failures.into_iter().take(20).collect::<Vec<_>>(),
        "toolErrors": tool_errors_total,
        "toolErrorsByTool": tools,
    })
}

/// Where the wall clock went.
///
/// The session file timestamps every record, so the difference between two
/// consecutive records is time the session spent between them. Two rules make
/// that usable:
///
/// * only transcript records count (user / assistant / tool results); the
///   ledger records -- goal snapshots, child-usage batches, refinement
///   notices -- are written in bursts and would otherwise own 44% of the
///   clock, which is an artefact of logging, not of work;
/// * a gap over five minutes is idle, not waiting. The session spans 17 days
///   and the median gap is 3.4 s; capping keeps a single overnight pause from
///   defining the total.
///
/// The result separates the two things that dominate: waiting on the provider
/// (tool result -> next assistant call) and running tools (assistant call ->
/// tool result). Everything else is the human reading the answer.
fn time_split(session: &Session, leaf: &str) -> Value {
    const IDLE_CAP_SECS: f64 = 300.0;
    let path = session.branch(leaf);
    let mut model = 0.0_f64;
    let mut tool = 0.0_f64;
    let mut model_gaps: Vec<f64> = Vec::new();
    let mut tool_gaps: Vec<f64> = Vec::new();
    // Tool results carry their own measured duration; wall clock is not it.
    let mut measured_ms: u64 = 0;
    let mut measured_n: u64 = 0;
    for &index in &path {
        if let Kind::ToolResult {
            duration_ms: Some(ms),
            ..
        } = &session.entries[index].kind
        {
            measured_ms += ms;
            measured_n += 1;
        }
    }
    // Transcript records only, in order.
    let mut points: Vec<(i64, bool)> = Vec::new();
    for &index in &path {
        let entry = &session.entries[index];
        match &entry.kind {
            Kind::User { .. } => points.push((entry.at, false)),
            Kind::Assistant { .. } => points.push((entry.at, true)),
            Kind::ToolResult { .. } => points.push((entry.at, false)),
            _ => {}
        }
    }
    let mut active = 0.0_f64;
    let mut idle_total = 0.0_f64;
    for pair in points.windows(2) {
        let (from, to) = (pair[0], pair[1]);
        if from.0 <= 0 || to.0 <= 0 || to.0 < from.0 {
            continue;
        }
        #[allow(clippy::cast_precision_loss)]
        let secs = (to.0 - from.0) as f64 / 1000.0;
        if secs > IDLE_CAP_SECS {
            idle_total += secs;
            continue;
        }
        active += secs;
        if to.1 {
            model += secs;
            model_gaps.push(secs);
        } else {
            tool += secs;
            tool_gaps.push(secs);
        }
    }
    let quantile = |values: &mut Vec<f64>, q: f64| -> f64 {
        if values.is_empty() {
            return 0.0;
        }
        values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        #[allow(clippy::cast_precision_loss, clippy::cast_sign_loss, clippy::cast_possible_truncation)]
        let at = ((values.len() - 1) as f64 * q).round() as usize;
        values[at]
    };
    let model_median = quantile(&mut model_gaps, 0.5);
    let tool_median = quantile(&mut tool_gaps, 0.5);
    let model_p90 = quantile(&mut model_gaps, 0.9);
    let tool_p90 = quantile(&mut tool_gaps, 0.9);
    #[allow(clippy::cast_precision_loss)]
    let measured_hours = measured_ms as f64 / 3_600_000.0;
    json!({
        "activeSecs": active,
        "modelSecs": model,
        "toolSecs": tool,
        // Transcript records are only three kinds, so every counted gap is
        // either waiting on the model or running a tool. Time between user
        // messages and the answer is model time; there is no third bucket,
        // and inventing one reported a hard zero on every session.
        "idleSecs": (idle_total).max(0.0),
        "modelCalls": model_gaps.len(),
        "toolCalls": tool_gaps.len(),
        "modelMedian": model_median,
        "modelP90": model_p90,
        "toolMedian": tool_median,
        "toolP90": tool_p90,
        "measuredToolSecs": measured_hours * 3600.0,
        "measuredToolCalls": measured_n,
        "idleCapSecs": IDLE_CAP_SECS,
    })
}

/// Per-tool wall time, from each tool result's own `durationMs`.
///
/// `tool_stats` counts what tools *emitted*; this counts what they *took*.
/// A tool that is cheap in output and expensive in time is invisible in every
/// other view: it inflates nothing, it just makes the session slow.
fn tool_time(session: &Session, leaf: &str) -> Vec<Value> {
    struct Row {
        calls: u64,
        total_ms: u64,
        max_ms: u64,
        durations: Vec<u64>,
        chars: u64,
    }
    let mut rows: std::collections::HashMap<String, Row> = std::collections::HashMap::new();
    for index in session.branch(leaf) {
        let Kind::ToolResult {
            tool,
            chars,
            duration_ms,
            ..
        } = &session.entries[index].kind
        else {
            continue;
        };
        let Some(ms) = duration_ms else { continue };
        let row = rows.entry(tool.clone()).or_insert_with(|| Row {
            calls: 0,
            total_ms: 0,
            max_ms: 0,
            durations: Vec::new(),
            chars: 0,
        });
        row.calls += 1;
        row.total_ms += ms;
        row.max_ms = row.max_ms.max(*ms);
        row.durations.push(*ms);
        row.chars += chars.total();
    }
    let mut out: Vec<Value> = rows
        .into_iter()
        .map(|(tool, mut row)| {
            row.durations.sort_unstable();
            let n = row.durations.len();
            let median = row.durations[n / 2];
            #[allow(clippy::cast_precision_loss)]
            let seconds = row.total_ms as f64 / 1000.0;
            json!({
                "tool": tool,
                "calls": row.calls,
                "seconds": seconds,
                "medianMs": median,
                "maxMs": row.max_ms,
                "chars": row.chars,
                "charsPerSecond": if seconds > 0.0 {
                    #[allow(clippy::cast_precision_loss)]
                    { row.chars as f64 / seconds }
                } else { 0.0 },
            })
        })
        .collect();
    out.sort_by(|a, b| {
        b["seconds"]
            .as_f64()
            .unwrap_or_default()
            .partial_cmp(&a["seconds"].as_f64().unwrap_or_default())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out
}

/// The biggest single rows on the branch, with their cost in the currency that
/// matters: characters, and how many model calls they stayed in context for.
///
/// A 400 KB tool result that is compacted away immediately costs one call; the
/// same result kept for 300 calls costs 300x. Characters alone cannot tell
/// those apart, which is why the raw "largest entries" list misleads.
fn biggest(session: &Session, leaf: &str, limit: usize) -> Vec<Value> {
    let path = session.branch(leaf);
    let position: std::collections::HashMap<&str, usize> = path
        .iter()
        .enumerate()
        .map(|(at, &index)| (session.entries[index].id.as_str(), at))
        .collect();
    // For each path position, the position at which it leaves the context
    // (the next compaction's cut, or the leaf).
    let mut evicted_at = vec![path.len(); path.len()];
    let mut tail_start = 0;
    for (at, &index) in path.iter().enumerate() {
        if let Kind::Compaction { first_kept, .. } = &session.entries[index].kind {
            if let Some(&cut) = position.get(first_kept.as_str()) {
                for slot in evicted_at.iter_mut().take(cut).skip(tail_start) {
                    *slot = at;
                }
                tail_start = cut;
            }
        }
    }
    // Calls remaining from a position onward, so the live span is a subtraction.
    let mut calls_after = vec![0_u64; path.len() + 1];
    for at in (0..path.len()).rev() {
        let calls = matches!(session.entries[path[at]].kind, Kind::Assistant { usage: Some(_), .. });
        calls_after[at] = calls_after[at + 1] + u64::from(calls);
    }
    let mut scored: Vec<(u64, usize, u64, &'static str)> = Vec::new();
    for (at, &index) in path.iter().enumerate() {
        let entry = &session.entries[index];
        let chars = entry.chars().total();
        if chars < 1000 {
            continue;
        }
        let end = evicted_at[at];
        let live = calls_after[at + 1].saturating_sub(calls_after[end]);
        let label = match &entry.kind {
            Kind::User { .. } => "user",
            Kind::Assistant { .. } => "assistant",
            Kind::ToolResult { .. } => "toolResult",
            Kind::Compaction { .. } => "compaction",
            _ => continue,
        };
        scored.push((chars.saturating_mul(live.max(1)), at, chars, label));
    }
    scored.sort_by_key(|row| std::cmp::Reverse(row.0));
    scored
        .into_iter()
        .take(limit)
        .map(|(cost, at, chars, label)| {
            let index = path[at];
            let entry = &session.entries[index];
            let end = evicted_at[at];
            json!({
                "entry": index,
                "at": entry.at,
                "kind": label,
                "chars": chars,
                "calls": calls_after[at + 1].saturating_sub(calls_after[end]),
                "cost": cost,
                "tool": match &entry.kind {
                    Kind::ToolResult { tool, .. } => tool.clone(),
                    _ => String::new(),
                },
            })
        })
        .collect()
}

/// What the output tokens were spent on.
///
/// `cache.output` is one number, but an assistant call emits three different
/// things and they are not interchangeable: reasoning, prose, and tool call
/// arguments. In p8 reasoning is 61% of everything the model wrote -- a fact
/// no usage table shows, because usage only reports a total.
fn output_split(session: &Session, leaf: &str) -> Value {
    let mut thinking = 0_u64;
    let mut text = 0_u64;
    let mut tool_args = 0_u64;
    let mut calls = 0_u64;
    let mut calls_with_thinking = 0_u64;
    for index in session.branch(leaf) {
        let Kind::Assistant { output, .. } = &session.entries[index].kind else {
            continue;
        };
        thinking += output.thinking;
        text += output.text;
        tool_args += output.tool_args;
        calls += 1;
        if output.thinking > 0 {
            calls_with_thinking += 1;
        }
    }
    let total = thinking + text + tool_args;
    let share = |part: u64| {
        if total == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            {
                part as f64 / total as f64
            }
        }
    };
    json!({
        "thinking": thinking,
        "text": text,
        "toolArgs": tool_args,
        "total": total,
        "thinkingShare": share(thinking),
        "textShare": share(text),
        "toolArgsShare": share(tool_args),
        "calls": calls,
        "callsWithThinking": calls_with_thinking,
    })
}

/// Harness bookkeeping entries: goal snapshots, refinements, git commits, and
/// what subagents spent. None of it is transcript — it never reaches the model
/// — but it is the record of what the session did.
///
/// `child_usage_attributed` carries two numbers per row and only one of them
/// is trustworthy. `childUsage` is that one call's usage; `aggregateUsage` is
/// the child's running total, but its `totalTokens` field is frozen at the
/// child's first call (a child that later spends 93M keeps reporting 120,524).
/// The running *parts* are fine, and they match Σ`childUsage` to within 0.1%,
/// so a child's total is read from the parts of its last row -- deduplicated by
/// `targetId`, which is also what makes the per-origin totals correct if a
/// child ever appears under two origins.
fn harness_stats(session: &Session, leaf: &str) -> Value {
    let mut custom: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    // target id -> running (prompt, output). Rows arrive in file order, so the
    // last write for a target is its final total.
    let mut children: std::collections::HashMap<&str, (u64, u64)> = std::collections::HashMap::new();
    let mut child_origin: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    for index in session.branch(leaf) {
        let entry = &session.entries[index];
        match &entry.kind {
            // Two record families hold the ledger: `type: "custom"` and
            // `type: "custom_message"`. Counting only the first reported one
            // type for a session that has nineteen -- the page exists to show
            // the other eighteen.
            Kind::Custom { custom_type, .. } => {
                *custom.entry(format!("custom/{custom_type}")).or_insert(0) += 1;
            }
            Kind::CustomMessage { custom_type, .. } => {
                *custom.entry(custom_type.clone()).or_insert(0) += 1;
            }
            Kind::Refinement { .. } => {
                *custom.entry("prime-agent.refinement".to_string()).or_insert(0) += 1;
            }
            Kind::GitState { .. } => {
                *custom.entry("git_state".to_string()).or_insert(0) += 1;
            }
            Kind::ChildUsage {
                target,
                origin,
                prompt,
                output,
            } => {
                children.insert(target.as_str(), (*prompt, *output));
                child_origin.insert(target.as_str(), origin.as_str());
            }
            _ => {}
        }
    }
    let child_prompt: u64 = children.values().map(|(prompt, _)| prompt).sum();
    let child_output: u64 = children.values().map(|(_, output)| output).sum();
    // Per origin, over children rather than rows.
    let mut origins: std::collections::HashMap<&str, (u64, u64, u64)> =
        std::collections::HashMap::new();
    for (target, (prompt, output)) in &children {
        if let Some(origin) = child_origin.get(target) {
            let slot = origins.entry(origin).or_insert((0, 0, 0));
            slot.0 += 1;
            slot.1 += prompt;
            slot.2 += output;
        }
    }
    let mut origin_rows: Vec<Value> = origins
        .into_iter()
        .map(|(origin, (count, tokens, output))| {
            json!({
                "origin": origin,
                "children": count,
                "tokens": tokens,
                "output": output,
            })
        })
        .collect();
    origin_rows.sort_by_key(|row| std::cmp::Reverse(row["tokens"].as_u64().unwrap_or_default()));

    let mut kinds: Vec<Value> = custom
        .into_iter()
        .map(|(custom_type, count)| json!({ "type": custom_type, "count": count }))
        .collect();
    kinds.sort_by_key(|row| std::cmp::Reverse(row["count"].as_u64().unwrap_or_default()));
    json!({
        "custom": kinds,
        "children": children.len(),
        "childPrompt": child_prompt,
        "childOutput": child_output,
        "origins": origin_rows,
    })
}

/// Every refinement the session wrote, newest last. Titles only: the bodies
/// live in `/api/refinement`, because 28 records carry 117 KB of edit text and
/// this payload is fetched on every click.
fn refinement_log(session: &Session, leaf: &str) -> Vec<Value> {
    let mut rows = Vec::new();
    for (index, &entry_index) in session.branch(leaf).iter().enumerate() {
        let Kind::Refinement {
            summary,
            scope,
            edits,
            ..
        } = &session.entries[entry_index].kind
        else {
            continue;
        };
        rows.push(json!({
            "at": session.entries[entry_index].at,
            // Position on the branch, which is what the detail endpoint needs
            // to find this record again without shipping its body here.
            "entry": index,
            "summary": summary,
            "scope": scope,
            "edits": edits
                .iter()
                .map(|edit| json!({
                    "action": edit.action,
                    "kind": edit.kind,
                    "title": edit.title,
                }))
                .collect::<Vec<_>>(),
        }));
    }
    rows
}

/// One refinement record in full, for the page that lists them.
#[must_use]
pub fn refinement_text(session: &Session, entry: usize) -> Value {
    let leaf = session.default_leaf().unwrap_or_default();
    let path = session.branch(leaf);
    let Some(&index) = path.get(entry) else {
        return json!({ "error": format!("no entry {entry} on this branch") });
    };
    let Some(raw) = crate::session::raw_line_at(&session.path, session.entries[index].offset).ok()
    else {
        return json!({ "error": "cannot read the record from disk" });
    };
    let data = raw.get("data").cloned().unwrap_or(Value::Null);
    let text = |key: &str| data.get(key).and_then(Value::as_str).unwrap_or("").to_string();
    let edits = data
        .get("appliedEdits")
        .and_then(Value::as_array)
        .map(|rows| {
            rows.iter()
                .map(|row| {
                    json!({
                        "action": text_of(row, "action"),
                        "kind": text_of(row, "kind"),
                        "id": text_of(row, "id"),
                        "title": text_of(row, "title"),
                        "path": text_of(row, "path"),
                        "reason": text_of(row, "reason"),
                        "content": text_of(row, "content"),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    json!({
        "at": session.entries[index].at,
        "summary": text("summary"),
        "rationale": text("rationale"),
        "expectedOutcome": text("expectedOutcome"),
        "scope": text("scope"),
        "edits": edits,
    })
}

/// Git snapshots the session recorded: one row per turn that saw a commit.
/// They are the only record of where the working tree stood during the run.
fn commit_log(session: &Session, leaf: &str) -> Vec<Value> {
    session
        .branch(leaf)
        .iter()
        .filter_map(|&index| {
            let Kind::GitState { commit, branch } = &session.entries[index].kind else {
                return None;
            };
            if commit.is_empty() {
                return None;
            }
            Some(json!({
                "commit": commit,
                "branch": branch,
                "at": session.entries[index].at,
            }))
        })
        .collect()
}

/// Turns on the branch, counted as user inputs. A turn then fans out into many
/// model calls (35 on average in p8), so "calls" and "turns" differ by an order
/// of magnitude and mixing them up makes a session look far busier than it is.
/// Calls per turn: how many model requests one user message fans out into.
///
/// The mean is not the shape. In p8, 444 of 627 turns are under 25 calls and
/// one is 1,419, so "35 calls per turn" describes almost no turn at all. The
/// median and the maximum are what say whether the session is a series of
/// quick turns or a few enormous ones.
fn calls_per_turn(session: &Session, leaf: &str) -> Value {
    let mut counts: Vec<u64> = Vec::new();
    let mut current: Option<u64> = None;
    for &index in &session.branch(leaf) {
        match &session.entries[index].kind {
            Kind::User { .. } => {
                if let Some(done) = current.replace(0) {
                    counts.push(done);
                }
            }
            Kind::Assistant { .. } => {
                if let Some(slot) = current.as_mut() {
                    *slot += 1;
                }
            }
            _ => {}
        }
    }
    if let Some(done) = current {
        counts.push(done);
    }
    // A user message can be followed immediately by another one -- a queued
    // follow-up that arrived while the previous turn was still running. Those
    // turns never reached the model, so they are counted but not averaged in.
    let all = counts.len();
    counts.retain(|count| *count > 0);
    let silent = (all - counts.len()) as u64;
    if counts.is_empty() {
        return json!({ "turns": all, "silent": silent, "n": 0,
                       "median": 0, "mean": 0.0, "max": 0 });
    }
    let mut sorted = counts.clone();
    sorted.sort_unstable();
    let n = sorted.len();
    let median = if n.is_multiple_of(2) {
        (sorted[n / 2 - 1] + sorted[n / 2]) / 2
    } else {
        sorted[n / 2]
    };
    let total: u64 = sorted.iter().sum();
    #[allow(clippy::cast_precision_loss)]
    let mean = total as f64 / n as f64;
    json!({
        "turns": all,
        "silent": silent,
        "n": n,
        "median": median,
        "mean": mean,
        "max": sorted[n - 1],
    })
}

fn turn_count(session: &Session, leaf: &str) -> u64 {
    session
        .branch(leaf)
        .iter()
        .filter(|&&index| matches!(session.entries[index].kind, Kind::User { .. }))
        .count() as u64
}

fn text_of(row: &Value, key: &str) -> String {
    row.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// Activity per calendar day: entries and tokens. A session that spans 17 days
/// is mostly idle, and the shape of the work is in where the spikes are.
fn daily_activity(session: &Session, leaf: &str) -> Vec<Value> {
    let mut days: std::collections::BTreeMap<&str, (u64, u64, u64)> =
        std::collections::BTreeMap::new();
    for index in session.branch(leaf) {
        let entry = &session.entries[index];
        let Some(day) = entry.timestamp.get(0..10) else {
            continue;
        };
        let slot = days.entry(day).or_insert((0, 0, 0));
        slot.0 += 1;
        if let Kind::Assistant {
            usage: Some(usage), ..
        } = &entry.kind
        {
            if usage.parts_agree() {
                slot.1 += usage.prompt();
                slot.2 += usage.output;
            }
        }
    }
    days.into_iter()
        .map(|(day, (entries, prompt, output))| {
            json!({ "day": day, "entries": entries, "prompt": prompt, "output": output })
        })
        .collect()
}

/// How many entries of each kind sit on the branch, so the UI can show what
/// the file is mostly made of.
fn entry_kinds(session: &Session, leaf: &str) -> Vec<Value> {
    struct Row {
        count: u64,
        chars: u64,
        cjk: u64,
        tokens: u64,
    }
    let mut rows: std::collections::HashMap<&str, Row> = std::collections::HashMap::new();
    for index in session.branch(leaf) {
        let chars = session.entries[index].chars();
        let (tokens, _) = crate::token_est::estimate(chars);
        let row = rows
            .entry(session.entries[index].kind_name())
            .or_insert(Row {
                count: 0,
                chars: 0,
                cjk: 0,
                tokens: 0,
            });
        row.count += 1;
        row.chars += chars.total();
        row.cjk += chars.cjk;
        row.tokens += tokens;
    }
    let mut out: Vec<Value> = rows
        .into_iter()
        .map(|(kind, row)| {
            json!({
                "kind": kind,
                "count": row.count,
                "chars": row.chars,
                "cjk": row.cjk,
                "tokens": row.tokens,
            })
        })
        .collect();
    out.sort_by_key(|row| std::cmp::Reverse(row["count"].as_u64().unwrap_or_default()));
    out
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
                Kind::ToolResult { chars, tool, .. } => {
                    let (tokens, _) = crate::token_est::estimate(*chars);
                    (Some(tool.clone()), tokens)
                }
                Kind::Compaction { summary_chars, tokens_before, .. } => {
                    let (tokens, _) = crate::token_est::estimate(*summary_chars);
                    (Some(format!("{} before", tokens_before)), tokens)
                }
                Kind::CustomMessage { custom_type, chars, .. } => {
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

/// The text behind one component bucket, one page at a time.
///
/// The detail payload names the entries in each slice but never carries their
/// content: a 225-compaction session would ship megabytes of duplicates just to
/// let a tooltip say "python_repl, 54k tokens". This endpoint is the other half
/// — the browser asks for one bucket, one page, on demand.
///
/// Each row is truncated to `preview_chars`. The point is to see what the
/// tokens are, not to read the session in the browser.
#[must_use]
pub fn slice_text(
    session: &Session,
    label: &str,
    offset: usize,
    limit: usize,
    preview_chars: usize,
) -> Value {
    let leaf = session.default_leaf().unwrap_or_default();
    let assembly = assemble(session, leaf);
    let Some(component) = assembly
        .components
        .iter()
        .find(|component| component.label == label)
    else {
        return json!({ "error": format!("no component named {label:?}"), "rows": [] });
    };

    let total = component.members.len();
    let start = offset.min(total);
    let end = start.saturating_add(limit).min(total);
    let mut rows = Vec::with_capacity(end - start);
    let mut truncated = 0_u64;

    for &index in &component.members[start..end] {
        let entry = &session.entries[index];
        let raw = crate::session::raw_line_at(&session.path, entry.offset);
        let (blocks, note) = match raw {
            Ok(value) => (entry_blocks(&value), None),
            Err(error) => (Vec::new(), Some(error)),
        };
        // Each block gets its own budget. A shared budget let one long
        // `thinking` block swallow it and drop the tool call after it, which
        // reads as "this entry has no arguments" — the opposite of the point.
        let mut kept = Vec::new();
        for block in &blocks {
            let chars: Vec<char> = block.text.chars().collect();
            let take = chars.len().min(preview_chars);
            let text: String = chars[..take].iter().collect();
            if take < chars.len() {
                truncated += 1;
            }
            kept.push(json!({
                "role": block.role,
                "text": text,
                "chars": chars.len(),
                "shown": take,
            }));
        }
        let (tokens, _) = crate::token_est::estimate(entry.chars());
        // `chars` is what the analyzer counted for this entry (the number the
        // donut used); `textChars` is what the blocks below actually contain.
        // They differ for tool calls because the counted text is the argument
        // *values* while the preview shows the JSON that carries them. Report
        // both rather than letting a reader find the mismatch and guess.
        let text_chars: u64 = kept
            .iter()
            .filter_map(|block| block.get("chars").and_then(Value::as_u64))
            .sum();
        rows.push(json!({
            "id": entry.id,
            "at": entry.at,
            "timestamp": entry.timestamp,
            "kind": entry.kind_name(),
            "chars": entry.chars().total(),
            "textChars": text_chars,
            "tokens": tokens,
            "blocks": kept,
            "error": note,
        }));
    }

    json!({
        "label": component.label,
        "total": total,
        "offset": start,
        "limit": limit,
        "truncated": truncated,
        "previewChars": preview_chars,
        "rows": rows,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Usage;
    use std::io::Write;
    use std::path::{Path, PathBuf};

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "prime-agent-inspect-web-{}-{unique}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).expect("mkdir");
            Self(path)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn write_session(lines: &[&str]) -> (TempDir, PathBuf) {
        let dir = TempDir::new();
        let path = dir.path().join("s.jsonl");
        let mut file = std::fs::File::create(&path).expect("create");
        for line in lines {
            writeln!(file, "{line}").expect("write");
        }
        (dir, path)
    }

    /// A component must name the entries it holds, and the names must resolve
    /// to exactly the entries the assembler kept.
    #[test]
    fn components_list_the_entries_they_hold() {
        let (_dir, path) = write_session(&[
            r#"{"type":"session","id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"hello there"}}"#,
            r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"general kenobi"}]}}"#,
        ]);
        let session = crate::session::load(&path).expect("loads");
        let assembly = assemble(&session, "a1");
        let user = assembly
            .components
            .iter()
            .find(|component| component.label == "保留 user")
            .expect("user component");
        assert_eq!(user.members.len(), 1);
        assert_eq!(session.entries[user.members[0]].id, "u1");
        assert_eq!(user.entries, 1, "the count must match the member list");
    }

    /// The preview endpoint is the read side of the counter: it must return the
    /// text the count was based on, truncated per block.
    #[test]
    fn slice_text_returns_the_underlying_text() {
        let (_dir, path) = write_session(&[
            r#"{"type":"session","id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"hello there"}}"#,
        ]);
        let session = crate::session::load(&path).expect("loads");
        let payload = slice_text(&session, "保留 user", 0, 10, 600);
        assert_eq!(payload["total"], 1);
        let row = &payload["rows"][0];
        assert_eq!(row["id"], "u1");
        assert_eq!(row["blocks"][0]["text"], "hello there");
        assert_eq!(row["blocks"][0]["chars"], 11);
    }

    /// A block longer than the budget is cut, and the row says so.
    #[test]
    fn long_blocks_are_truncated_per_block() {
        let long = "x".repeat(50);
        let assistant = format!(
            r#"{{"type":"message","id":"a1","parentId":null,"timestamp":"2026-01-01T00:00:02.000Z","message":{{"role":"assistant","content":[{{"type":"thinking","thinking":"{long}"}},{{"type":"text","text":"{long}"}}]}}}}"#
        );
        let (_dir, path) = write_session(&[
            r#"{"type":"session","id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
            &assistant,
        ]);
        let session = crate::session::load(&path).expect("loads");
        let payload = slice_text(&session, "保留 assistant", 0, 10, 10);
        let blocks = payload["rows"][0]["blocks"].as_array().expect("blocks");
        assert_eq!(blocks.len(), 2, "no block may be dropped by the budget");
        for block in blocks {
            assert_eq!(block["chars"], 50);
            assert_eq!(block["shown"], 10);
            assert_eq!(block["text"].as_str().unwrap().chars().count(), 10);
        }
        assert_eq!(payload["truncated"], 2);
    }

    /// An unknown label is a normal answer, not a panic.
    #[test]
    fn unknown_labels_report_an_error_field() {
        let (_dir, path) = write_session(&[
            r#"{"type":"session","id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"hi"}}"#,
        ]);
        let session = crate::session::load(&path).expect("loads");
        let payload = slice_text(&session, "no such bucket", 0, 10, 600);
        assert!(payload["error"].as_str().is_some());
        assert_eq!(payload["rows"].as_array().map(Vec::len), Some(0));
    }
    /// `Usage::prompt` trusts `totalTokens` because the parts are inflated on
    /// some rows; `parts_agree` is how a caller knows whether it may sum them.
    #[test]
    fn parts_agree_rejects_rows_whose_parts_contradict_the_total() {
        let good = Usage {
            input: 1_000,
            output: 200,
            cache_read: 9_000,
            cache_write: 0,
            total: 10_200,
            cost: 0.0,
        };
        assert!(good.parts_agree());
        // The real corrupt shape: total stays sane, cacheRead is 6,000× too big.
        let bad = Usage {
            input: 1_541_032,
            output: 344_594,
            cache_read: 61_140_224,
            cache_write: 0,
            total: 33_545,
            cost: 0.0,
        };
        assert!(!bad.parts_agree());
        // No total at all: nothing to contradict, the parts are all there is.
        let none = Usage {
            total: 0,
            ..good
        };
        assert!(none.parts_agree());
    }

    /// A subagent's `aggregateUsage.totalTokens` is frozen at its first call,
    /// so the running total has to come from the parts beside it.
    #[test]
    fn subagent_spend_comes_from_the_running_parts_not_the_frozen_total() {
        let (_dir, path) = write_session(&[
            r#"{"type":"session","id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"a1","parentId":"s1","timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"go"}}"#,
            r#"{"type":"child_usage_attributed","id":"c1","parentId":"a1","timestamp":"2026-01-01T00:00:02.000Z","targetId":"k1","origin":"spawn_task","childUsage":{"input":100,"output":10,"cacheRead":900,"cacheWrite":0,"totalTokens":1010},"aggregateUsage":{"input":100,"output":10,"cacheRead":900,"cacheWrite":0,"totalTokens":1010}}"#,
            // Same child, much later: the parts grew, `totalTokens` did not.
            r#"{"type":"child_usage_attributed","id":"c2","parentId":"a1","timestamp":"2026-01-01T00:00:03.000Z","targetId":"k1","origin":"spawn_task","childUsage":{"input":200,"output":20,"cacheRead":1800,"cacheWrite":0,"totalTokens":2020},"aggregateUsage":{"input":300,"output":30,"cacheRead":2700,"cacheWrite":0,"totalTokens":1010}}"#,
        ]);
        let session = crate::session::load(&path).expect("load");
        let payload = detail(&session, None);
        let harness = &payload["harness"];
        assert_eq!(harness["children"], 1);
        assert_eq!(
            harness["childPrompt"], 3_000,
            "1,000 then 3,000 cumulative; the frozen total would say 1,010"
        );
        assert_eq!(harness["origins"][0]["children"], 1);
        assert_eq!(harness["origins"][0]["tokens"], 3_000);
    }

    /// A turn is one user message. Some of them are followed by another user
    /// message before the model answers (a queued follow-up), so counting
    /// turns and counting calls-per-turn must agree on the total.
    #[test]
    fn turns_split_into_answered_and_queued() {
        let (_dir, path) = write_session(&[
            r#"{"type":"session","id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"u1","parentId":"s1","timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"a"}}"#,
            r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"assistant","content":"x","usage":{"input":10,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":11}}}"#,
            r#"{"type":"message","id":"a2","parentId":"a1","timestamp":"2026-01-01T00:00:03.000Z","message":{"role":"assistant","content":"y","usage":{"input":10,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":11}}}"#,
            // Queued: the model never answered u2 before u3 arrived.
            r#"{"type":"message","id":"u2","parentId":"a2","timestamp":"2026-01-01T00:00:04.000Z","message":{"role":"user","content":"b"}}"#,
            r#"{"type":"message","id":"u3","parentId":"u2","timestamp":"2026-01-01T00:00:05.000Z","message":{"role":"user","content":"c"}}"#,
            r#"{"type":"message","id":"a3","parentId":"u3","timestamp":"2026-01-01T00:00:06.000Z","message":{"role":"assistant","content":"z","usage":{"input":10,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":11}}}"#,
        ]);
        let session = crate::session::load(&path).expect("load");
        let payload = detail(&session, None);
        assert_eq!(payload["turns"], 3, "three user messages");
        let cpt = &payload["callsPerTurn"];
        assert_eq!(cpt["turns"], 3);
        assert_eq!(cpt["silent"], 1, "u2 was superseded before any call");
        assert_eq!(cpt["n"], 2);
        assert_eq!(cpt["max"], 2, "the first turn made two calls");
    }

    /// The time split must separate waiting on the provider from running tools,
    /// and must not let a long idle gap or a ledger record into either bucket.
    #[test]
    fn time_split_separates_model_from_tool_and_drops_idle() {
        let (_dir, path) = write_session(&[
            r#"{"type":"session","id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"u1","parentId":"s1","timestamp":"2026-01-01T00:00:00.000Z","message":{"role":"user","content":"a"}}"#,
            // 10 s to the first call: model time.
            r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-01-01T00:00:10.000Z","message":{"role":"assistant","content":[{"type":"text","text":"x"}],"usage":{"input":10,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":11}}}"#,
            // 2 s to the tool result: tool time, and the tool reports 500 ms.
            r#"{"type":"message","id":"t1","parentId":"a1","timestamp":"2026-01-01T00:00:12.000Z","message":{"role":"toolResult","toolName":"bash","content":[{"type":"text","text":"ok"}],"details":{"durationMs":500}}}"#,
            // 8 s to the next call: model time.
            r#"{"type":"message","id":"a2","parentId":"t1","timestamp":"2026-01-01T00:00:20.000Z","message":{"role":"assistant","content":[{"type":"text","text":"y"}],"usage":{"input":10,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":11}}}"#,
            // An hour later: idle, and must not be counted anywhere.
            r#"{"type":"message","id":"a3","parentId":"a2","timestamp":"2026-01-01T01:00:20.000Z","message":{"role":"assistant","content":[{"type":"text","text":"z"}],"usage":{"input":10,"output":1,"cacheRead":0,"cacheWrite":0,"totalTokens":11}}}"#,
        ]);
        let session = crate::session::load(&path).expect("load");
        let payload = detail(&session, None);
        let time = &payload["time"];
        assert_eq!(time["modelSecs"], 18.0, "10s + 8s of provider waiting");
        assert_eq!(time["toolSecs"], 2.0, "call to tool result");
        assert_eq!(time["activeSecs"], 20.0);
        assert_eq!(time["idleSecs"], 3600.0, "the hour-long pause");
        assert_eq!(time["measuredToolSecs"], 0.5, "the tool's own durationMs");
        // Tool time is wall time, measured time is the tool's own claim; the
        // two are different numbers and must stay different.
        assert_eq!(payload["toolTime"][0]["seconds"], 0.5);
    }

}
