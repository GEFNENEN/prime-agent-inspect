//! Tolerant reading of prime-agent session JSONL.
//!
//! The analyzer deliberately does not link `pa-types`: session files carry a
//! `version` field and the format grows, so a read-only tool that parses the
//! fields it understands and ignores the rest keeps working across versions
//! instead of failing to compile against them.

use serde_json::Value;

/// CJK vs. other character split. Tokenizers spend roughly one token per CJK
/// character and several characters per token otherwise, so the two classes
/// need separate counts to estimate anything usable.
#[derive(Debug, Clone, Copy, Default)]
pub struct Chars {
    pub cjk: u64,
    pub other: u64,
}

impl Chars {
    #[must_use]
    pub fn total(self) -> u64 {
        self.cjk + self.other
    }

    fn add(&mut self, text: &str) {
        for ch in text.chars() {
            if is_cjk(ch) {
                self.cjk += 1;
            } else {
                self.other += 1;
            }
        }
    }
}

fn is_cjk(ch: char) -> bool {
    let code = ch as u32;
    (0x3040..=0x30FF).contains(&code)
        || (0x3400..=0x4DBF).contains(&code)
        || (0x4E00..=0x9FFF).contains(&code)
        || (0xF900..=0xFAFF).contains(&code)
        || (0xFF00..=0xFFEF).contains(&code)
        || (0xAC00..=0xD7AF).contains(&code)
}

/// Provider-reported accounting for one model call.
#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    /// The provider's own total for the call: prompt plus generated tokens.
    /// Kept because the parts are not always trustworthy — see [`Usage::prompt`].
    pub total: u64,
    pub cost: f64,
}

impl Usage {
    /// The prompt the provider actually saw.
    ///
    /// `input` alone excludes the cached prefix, which is the majority of a
    /// long session's prompt, so the parts have to be summed. But on this
    /// machine some calls (61 of 21,732 in the p8 session, all on streamed
    /// turns) come back with `input`/`cacheRead`/`output` inflated by orders of
    /// magnitude while `totalTokens` stays sane — one call claims a 62M-token
    /// prompt. Summing the parts then reports a peak ~250× the real ceiling,
    /// and every consumer (peak, calibration, the fixed-overhead residual)
    /// inherits it.
    ///
    /// `totalTokens` equals prompt + output on all 21,632 sane rows, so it is
    /// the field to trust: subtract the output. On the corrupt rows `output` is
    /// inflated as well, so it cannot be subtracted — but the total still sits
    /// on the curve (33,545 between neighbours of 26,795 and 33,975), so the
    /// total alone is the least wrong answer there. The parts are used only
    /// when the call reports no total at all.
    #[must_use]
    pub fn prompt(self) -> u64 {
        if self.total > self.output {
            return self.total - self.output;
        }
        if self.total > 0 {
            return self.total;
        }
        self.input + self.cache_read + self.cache_write
    }

    /// Whether the reported parts add up to the reported total.
    ///
    /// The 61 corrupt rows here do not just have a bad `totalTokens`: their
    /// `cacheRead` is inflated too, and together they account for 27% of every
    /// cached token in the session. Any sum over the parts — cache hit rate,
    /// per-model prompt totals — has to skip them or it reports a number that
    /// is wrong by a quarter while looking precise.
    #[must_use]
    pub fn parts_agree(self) -> bool {
        if self.total == 0 {
            return true; // nothing to check against; the parts are all we have
        }
        let parts = self.input + self.cache_read + self.cache_write + self.output;
        let tolerance = (self.total / 500).max(2);
        parts.abs_diff(self.total) <= tolerance
    }
}

#[derive(Debug, Clone)]
pub enum Kind {
    User { chars: Chars },
    Assistant {
        chars: Chars,
        usage: Option<Usage>,
        /// `provider/model` as reported by the call, so the detail view can
        /// show which models this session actually ran on. A session that
        /// switched providers mid-run looks like one number otherwise.
        model: Option<String>,
    },
    ToolResult {
        chars: Chars,
        tool: String,
        /// Failed tool calls are a different story from successful ones: they
        /// are usually long error text that the model then works around.
        is_error: bool,
    },
    Compaction {
        summary_chars: Chars,
        tokens_before: u64,
        first_kept: String,
        usage: Option<Usage>,
    },
    BranchSummary { summary_chars: Chars },
    CustomMessage { custom_type: String, chars: Chars },
    /// `type: "custom"` — harness bookkeeping rather than transcript. The goal
    /// state snapshots and refinement records live here; they are not sent to
    /// the model, but they are what the session *did*.
    Custom {
        custom_type: String,
        /// A short human handle: the goal objective, the refinement summary.
        label: String,
        chars: Chars,
    },
    GitState { commit: String, branch: String },
    /// Tokens a subagent spent, attributed to the turn that spawned it. The
    /// session's own usage excludes them, so a parent with heavy delegation
    /// looks cheap until this is added in.
    ChildUsage {
        target: String,
        origin: String,
        /// The child's running total *prompt* (input + cacheRead + cacheWrite),
        /// taken from `aggregateUsage`'s parts. The `totalTokens` field beside
        /// them is frozen at the child's first call and must not be used.
        prompt: u64,
        output: u64,
    },
    SessionInfo { name: String },
    ModelChange { provider: String, model: String },
    Other { kind: String, chars: Chars },
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub id: String,
    pub parent: Option<String>,
    pub timestamp: String,
    /// Milliseconds since the epoch, 0 when unparseable.
    pub at: i64,
    /// Byte offset of this entry's line in the session file. The parsed
    /// session keeps counts, not text; this is how a request that does want
    /// the text can seek straight to one line instead of rescanning 187 MB.
    pub offset: u64,
    pub kind: Kind,
}

impl Entry {
    /// Every entry contributes text to the transcript; this is the single
    /// place that answers "how big is this row".
    #[must_use]
    pub fn chars(&self) -> Chars {
        match &self.kind {
            Kind::User { chars }
            | Kind::Assistant { chars, .. }
            | Kind::ToolResult { chars, .. }
            | Kind::CustomMessage { chars, .. }
            | Kind::Other { chars, .. } => *chars,
            // Custom is harness bookkeeping: it never reaches the model, so it
            // must not inflate any context number.
            Kind::Custom { .. } | Kind::GitState { .. } | Kind::ChildUsage { .. } => {
                Chars::default()
            }
            Kind::Compaction { summary_chars, .. } | Kind::BranchSummary { summary_chars } => {
                *summary_chars
            }
            Kind::SessionInfo { .. } | Kind::ModelChange { .. } => Chars::default(),
        }
    }

    #[must_use]
    pub fn kind_name(&self) -> &str {
        match &self.kind {
            Kind::User { .. } => "user",
            Kind::Assistant { .. } => "assistant",
            Kind::ToolResult { .. } => "toolResult",
            Kind::Compaction { .. } => "compaction",
            Kind::BranchSummary { .. } => "branch_summary",
            Kind::CustomMessage { .. } => "custom_message",
            Kind::Custom { .. } => "custom",
            Kind::GitState { .. } => "git_state",
            Kind::ChildUsage { .. } => "child_usage",
            Kind::SessionInfo { .. } => "session_info",
            Kind::ModelChange { .. } => "model_change",
            Kind::Other { kind, .. } => kind,
        }
    }
}

/// Parse one JSONL line. Unknown `type` values still produce an entry so the
/// transcript stays complete; a malformed line returns `None`.
#[must_use]
pub fn parse_entry(value: &Value) -> Option<Entry> {
    parse_entry_at(value, 0)
}

/// Same as [`parse_entry`], recording where the line came from.
#[must_use]
pub fn parse_entry_at(value: &Value, offset: u64) -> Option<Entry> {
    let id = value.get("id")?.as_str()?.to_string();
    let parent = value
        .get("parentId")
        .and_then(Value::as_str)
        .map(str::to_string);
    let timestamp = value
        .get("timestamp")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let at = parse_iso_millis(&timestamp);
    let kind = classify(value);
    Some(Entry {
        id,
        parent,
        timestamp,
        at,
        offset,
        kind,
    })
}

/// `provider/model`, or just the model when the provider is absent. Sessions
/// can change provider mid-run; the pair is what the user configured.
fn model_label(message: &Value) -> Option<String> {
    let model = message.get("model").and_then(Value::as_str)?;
    if model.is_empty() {
        return None;
    }
    Some(match message.get("provider").and_then(Value::as_str) {
        Some(provider) if !provider.is_empty() => format!("{provider}/{model}"),
        _ => model.to_string(),
    })
}

fn classify(value: &Value) -> Kind {
    let entry_type = value.get("type").and_then(Value::as_str).unwrap_or("");
    match entry_type {
        "message" => {
            let Some(message) = value.get("message") else {
                return Kind::Other {
                    kind: "message".to_string(),
                    chars: Chars::default(),
                };
            };
            let role = message.get("role").and_then(Value::as_str).unwrap_or("");
            let chars = message_chars(message);
            match role {
                "user" => Kind::User { chars },
                "assistant" => Kind::Assistant {
                    chars,
                    usage: message.get("usage").and_then(parse_usage),
                    model: model_label(message),
                },
                "toolResult" => Kind::ToolResult {
                    chars,
                    // A malformed call can put the rest of its own markup in
                    // here: one row's `toolName` is literally
                    // "invoke>\n<parameter name=". Only the first line means
                    // anything, and it is still the name the model asked for.
                    tool: message
                        .get("toolName")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .lines()
                        .next()
                        .unwrap_or("")
                        .trim()
                        .chars()
                        .take(32)
                        .collect(),
                    is_error: message
                        .get("isError")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                },
                other => Kind::Other {
                    kind: format!("message:{other}"),
                    chars,
                },
            }
        }
        "compaction" => Kind::Compaction {
            summary_chars: str_chars(value.get("summary")),
            tokens_before: value
                .get("tokensBefore")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
            first_kept: value
                .get("firstKeptEntryId")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            usage: value.get("usage").and_then(parse_usage),
        },
        "branch_summary" => Kind::BranchSummary {
            summary_chars: str_chars(value.get("summary")),
        },
        "custom_message" => Kind::CustomMessage {
            custom_type: value
                .get("customType")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            chars: str_chars(value.get("content")),
        },
        "custom" => Kind::Custom {
            custom_type: value
                .get("customType")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            // The goal objective and the refinement summary are the two fields
            // that say what happened; everything else in `data` is a counter
            // that changes every turn.
            label: ["objective", "summary", "rationale"]
                .iter()
                .find_map(|key| {
                    value
                        .get("data")
                        .and_then(|data| data.get(*key))
                        .and_then(Value::as_str)
                })
                .map(|text| text.chars().take(160).collect())
                .unwrap_or_default(),
            chars: Chars::default(),
        },
        "git_state" => Kind::GitState {
            commit: value
                .get("git")
                .and_then(|git| git.get("commit"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            branch: value
                .get("git")
                .and_then(|git| git.get("branch"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        },
        "child_usage_attributed" => {
            let aggregate = value.get("aggregateUsage");
            let part = |key: &str| {
                aggregate
                    .and_then(|usage| usage.get(key))
                    .and_then(Value::as_u64)
                    .unwrap_or_default()
            };
            Kind::ChildUsage {
                target: value
                    .get("targetId")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                origin: value
                    .get("origin")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                prompt: part("input") + part("cacheRead") + part("cacheWrite"),
                output: part("output"),
            }
        }
        "session_info" => Kind::SessionInfo {
            name: value
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        },
        "model_change" => Kind::ModelChange {
            provider: value
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            model: value
                .get("modelId")
                .or_else(|| value.get("model"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        },
        other => Kind::Other {
            kind: other.to_string(),
            chars: summary_chars_of(value),
        },
    }
}

/// One block of readable text inside an entry, kept in the order the model
/// sees it so a preview can label thinking separately from output.
#[derive(Debug, Clone)]
pub struct TextBlock {
    /// `text`, `thinking`, `toolCall name`, `stdout`, `summary`, ...
    pub role: String,
    pub text: String,
}

/// Pull the model-facing text out of a raw JSONL line.
///
/// This is the read side of [`value_chars`]: that function counts characters,
/// this one returns them. Only the fields the counter counts are emitted, so a
/// preview can never show more text than the token estimate was based on.
#[must_use]
pub fn entry_blocks(value: &Value) -> Vec<TextBlock> {
    let mut blocks = Vec::new();
    match value.get("type").and_then(Value::as_str).unwrap_or("") {
        "message" => {
            let Some(message) = value.get("message") else {
                return blocks;
            };
            if let Some(content) = message.get("content") {
                blocks_of(content, &mut blocks);
            }
            // toolResult carries its output in `details`, not `content`, on
            // some providers; take the big streams when present.
            if let Some(details) = message.get("details") {
                for key in ["stdout", "stderr"] {
                    if let Some(text) = details.get(key).and_then(Value::as_str) {
                        if !text.is_empty() {
                            blocks.push(TextBlock { role: key.to_string(), text: text.to_string() });
                        }
                    }
                }
            }
        }
        "compaction" | "branch_summary" => {
            if let Some(text) = value.get("summary").and_then(Value::as_str) {
                blocks.push(TextBlock { role: "summary".to_string(), text: text.to_string() });
            }
        }
        "custom_message" => {
            if let Some(content) = value.get("content") {
                blocks_of(content, &mut blocks);
            }
        }
        other => {
            for key in ["summary", "content", "text", "name"] {
                if let Some(field) = value.get(key) {
                    blocks_of(field, &mut blocks);
                }
            }
            if blocks.is_empty() {
                blocks.push(TextBlock { role: other.to_string(), text: String::new() });
            }
        }
    }
    blocks
}

fn blocks_of(value: &Value, out: &mut Vec<TextBlock>) {
    match value {
        Value::String(text) => {
            if !text.is_empty() {
                out.push(TextBlock { role: "text".to_string(), text: text.clone() });
            }
        }
        Value::Array(items) => {
            for item in items {
                blocks_of(item, out);
            }
        }
        Value::Object(map) => {
            // A tool call's arguments are part of the prompt. Emit each
            // argument value under `toolCall <name>.<param>` rather than the
            // whole JSON blob: the blob's own characters are not what the token
            // count measured, and `{"code":"\ndef ...` is unreadable where
            // `code: def ...` is not.
            if let Some(name) = map.get("name").and_then(Value::as_str) {
                if let Some(args) = ["arguments", "input", "args"]
                    .iter()
                    .find_map(|key| map.get(*key))
                {
                    let before = out.len();
                    match args {
                        Value::Object(fields) => {
                            for (param, value) in fields {
                                let text = match value {
                                    Value::String(text) => text.clone(),
                                    other => other.to_string(),
                                };
                                if !text.is_empty() {
                                    out.push(TextBlock {
                                        role: format!("toolCall {name} · {param}"),
                                        text,
                                    });
                                }
                            }
                        }
                        other => {
                            let text = other.to_string();
                            if !text.is_empty() {
                                out.push(TextBlock { role: format!("toolCall {name}"), text });
                            }
                        }
                    }
                    if out.len() > before {
                        return;
                    }
                }
            }
            for (key, item) in map {
                if !is_counted_field(key) {
                    continue;
                }
                if let Some(text) = item.as_str() {
                    if !text.is_empty() {
                        out.push(TextBlock { role: key.clone(), text: text.to_string() });
                    }
                } else {
                    blocks_of(item, out);
                }
            }
        }
        _ => {}
    }
}

/// Fallback sizing for entry types this build does not model: count whatever
/// string fields look like payload instead of reporting zero.
fn summary_chars_of(value: &Value) -> Chars {
    let mut chars = Chars::default();
    for key in ["summary", "content", "text", "name"] {
        if let Some(field) = value.get(key) {
            chars.add_chars(value_chars(field));
        }
    }
    chars
}

impl Chars {
    fn add_chars(&mut self, other: Chars) {
        self.cjk += other.cjk;
        self.other += other.other;
    }
}

fn str_chars(value: Option<&Value>) -> Chars {
    value.map_or_else(Chars::default, value_chars)
}

fn value_chars(value: &Value) -> Chars {
    value_chars_filtered(value, false)
}

/// `everything` turns the key whitelist off for a subtree. A tool call's
/// `arguments` is a JSON blob whose keys are the tool's own parameter names
/// (`code`, `path`, `pattern`, ...), so applying the whitelist inside it counts
/// nothing: the model still receives all of it.
fn value_chars_filtered(value: &Value, everything: bool) -> Chars {
    let mut chars = Chars::default();
    match value {
        Value::String(text) => chars.add(text),
        Value::Array(items) => {
            for item in items {
                chars.add_chars(value_chars_filtered(item, everything));
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                if everything || is_counted_field(key) {
                    let payload = everything || is_payload_field(key);
                    chars.add_chars(value_chars_filtered(item, payload));
                }
            }
        }
        _ => {}
    }
    chars
}

/// Keys whose subtree is opaque tool input rather than a wrapper object.
fn is_payload_field(key: &str) -> bool {
    matches!(key, "arguments" | "args" | "input")
}

fn message_chars(message: &Value) -> Chars {
    let mut chars = Chars::default();
    if let Some(content) = message.get("content") {
        chars.add_chars(value_chars(content));
    }
    chars
}

/// Object keys whose values are model-facing text. Arguments and results are
/// counted; ids, timestamps and flags are not.
fn is_counted_field(key: &str) -> bool {
    matches!(
        key,
        "text" | "thinking" | "content" | "args" | "input" | "arguments" | "name" | "summary"
    )
}

fn parse_usage(value: &Value) -> Option<Usage> {
    let number = |key: &str| value.get(key).and_then(Value::as_u64).unwrap_or_default();
    let cost = value
        .get("cost")
        .and_then(|cost| cost.get("total"))
        .and_then(Value::as_f64)
        .unwrap_or_default();
    Some(Usage {
        input: number("input"),
        output: number("output"),
        cache_read: number("cacheRead"),
        cache_write: number("cacheWrite"),
        total: number("totalTokens"),
        cost,
    })
}

/// `2026-10-09T09:23:55.654Z` → epoch milliseconds. Returns 0 when the string
/// is not an ISO-8601 instant, which sorts such rows to the front rather than
/// dropping them.
#[must_use]
pub fn parse_iso_millis(text: &str) -> i64 {
    let bytes = text.as_bytes();
    if bytes.len() < 19 {
        return 0;
    }
    let num = |from: usize, to: usize| -> Option<i64> { text.get(from..to)?.parse().ok() };
    let (Some(year), Some(month), Some(day), Some(hour), Some(minute), Some(second)) = (
        num(0, 4),
        num(5, 7),
        num(8, 10),
        num(11, 13),
        num(14, 16),
        num(17, 19),
    ) else {
        return 0;
    };
    let millis: i64 = text
        .get(20..23)
        .and_then(|fraction| fraction.parse().ok())
        .unwrap_or(0);
    let days = days_from_civil(year, month, day);
    days * 86_400_000 + hour * 3_600_000 + minute * 60_000 + second * 1000 + millis
}

/// Howard Hinnant's days-from-civil, the standard branch-free conversion.
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let day_of_year = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_an_assistant_turn_with_usage() {
        let value = json!({
            "type": "message",
            "id": "e2",
            "parentId": "e1",
            "timestamp": "2026-01-01T00:00:02.500Z",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "hi 你好"}],
                "usage": {"input": 10, "output": 2, "cacheRead": 90, "cacheWrite": 5, "cost": {"total": 0.5}}
            }
        });
        let entry = parse_entry(&value).expect("entry parses");
        assert_eq!(entry.id, "e2");
        assert_eq!(entry.parent.as_deref(), Some("e1"));
        assert!(matches!(entry.kind, Kind::Assistant { .. }));
        assert_eq!(entry.chars().cjk, 2);
        let Kind::Assistant { usage, .. } = entry.kind else {
            panic!("assistant")
        };
        let usage = usage.expect("usage");
        assert_eq!(
            usage.prompt(),
            105,
            "without totalTokens the parts are summed, cached prefix included"
        );
        assert!((usage.cost - 0.5).abs() < f64::EPSILON);
    }

    /// Streamed turns on this machine sometimes report parts inflated by
    /// orders of magnitude while `totalTokens` stays sane. Trusting the sum
    /// produced a 92M-token "peak prompt" on a session whose real ceiling is
    /// ~370k, and every derived number inherited it.
    #[test]
    fn inflated_parts_do_not_become_an_inflated_prompt() {
        let value = json!({
            "type": "message",
            "id": "e3",
            "parentId": "e2",
            "timestamp": "2026-01-01T00:00:03.000Z",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "hi"}],
                "usage": {
                    "input": 1541032, "cacheRead": 61140224, "cacheWrite": 0,
                    "output": 344594, "totalTokens": 33545,
                }
            }
        });
        let entry = parse_entry(&value).expect("entry parses");
        let Kind::Assistant { usage, .. } = entry.kind else {
            panic!("assistant")
        };
        let usage = usage.expect("usage");
        assert_eq!(
            usage.prompt(),
            33_545,
            "output is inflated too, so the total is the only usable field"
        );
    }

    /// A healthy row: totalTokens is prompt + output, so the prompt is the
    /// total minus the generated part.
    #[test]
    fn total_tokens_is_preferred_over_the_parts() {
        let value = json!({
            "type": "message",
            "id": "e4",
            "parentId": "e3",
            "timestamp": "2026-01-01T00:00:04.000Z",
            "message": {
                "role": "assistant",
                "content": [{"type": "text", "text": "hi"}],
                "usage": {
                    "input": 106764, "cacheRead": 809216, "cacheWrite": 0,
                    "output": 8439, "totalTokens": 157819,
                }
            }
        });
        let entry = parse_entry(&value).expect("entry parses");
        let Kind::Assistant { usage, .. } = entry.kind else {
            panic!("assistant")
        };
        let usage = usage.expect("usage");
        assert_eq!(usage.prompt(), 149_380, "157819 - 8439");
    }

    #[test]
    fn unknown_types_keep_their_payload_size() {
        let value = json!({
            "type": "tool_started",
            "id": "x",
            "parentId": null,
            "timestamp": "2026-01-01T00:00:00.000Z",
            "name": "bash"
        });
        let entry = parse_entry(&value).expect("entry parses");
        assert_eq!(entry.kind_name(), "tool_started");
        assert_eq!(entry.chars().other, 4, "the tool name is counted");
    }

    #[test]
    fn iso_timestamps_order_correctly() {
        let early = parse_iso_millis("2026-10-09T09:23:55.654Z");
        let late = parse_iso_millis("2026-10-24T22:24:29.000Z");
        assert!(early > 0);
        assert!(late > early);
        assert_eq!(parse_iso_millis(""), 0);
    }

    /// The compaction row's own text is the summary, not the payload beside it.
    #[test]
    fn compaction_counts_its_summary() {
        let value = json!({
            "type": "compaction",
            "id": "c1",
            "parentId": "e1",
            "timestamp": "2026-01-01T00:00:00.000Z",
            "summary": "abc",
            "tokensBefore": 1234,
            "firstKeptEntryId": "e9"
        });
        let entry = parse_entry(&value).expect("entry parses");
        let Kind::Compaction { tokens_before, ref first_kept, .. } = entry.kind else {
            panic!("compaction")
        };
        assert_eq!(tokens_before, 1234);
        assert_eq!(first_kept, "e9");
        assert_eq!(entry.chars().total(), 3);
    }
}
