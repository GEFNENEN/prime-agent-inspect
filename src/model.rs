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
    pub cost: f64,
}

impl Usage {
    /// The prompt the provider actually saw. `input` alone excludes the cached
    /// prefix, which is the majority of a long session's prompt.
    #[must_use]
    pub fn prompt(self) -> u64 {
        self.input + self.cache_read + self.cache_write
    }
}

#[derive(Debug, Clone)]
pub enum Kind {
    User { chars: Chars },
    Assistant { chars: Chars, usage: Option<Usage> },
    ToolResult { chars: Chars, tool: String },
    Compaction {
        summary_chars: Chars,
        tokens_before: u64,
        first_kept: String,
        usage: Option<Usage>,
    },
    BranchSummary { summary_chars: Chars },
    CustomMessage { custom_type: String, chars: Chars },
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
        kind,
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
                },
                "toolResult" => Kind::ToolResult {
                    chars,
                    tool: message
                        .get("toolName")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
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
    let mut chars = Chars::default();
    match value {
        Value::String(text) => chars.add(text),
        Value::Array(items) => {
            for item in items {
                chars.add_chars(value_chars(item));
            }
        }
        Value::Object(map) => {
            for (key, item) in map {
                if is_counted_field(key) {
                    chars.add_chars(value_chars(item));
                }
            }
        }
        _ => {}
    }
    chars
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
        assert_eq!(usage.prompt(), 105, "prompt includes the cached prefix");
        assert!((usage.cost - 0.5).abs() < f64::EPSILON);
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
