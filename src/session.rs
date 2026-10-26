//! Session loading: a JSONL file becomes a header plus an ordered entry list,
//! and a leaf id becomes the active branch.
//!
//! Session files are append-only histories where branching is expressed by
//! `parentId`, so "the conversation" is the path from the header to a leaf.
//! Both views matter: the tree explains how a session got here, the branch is
//! what a resume would replay.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::model::{parse_entry_at, Entry, Kind};

/// A session file that failed to load, and why.
#[derive(Debug)]
pub struct LoadError {
    pub path: PathBuf,
    pub message: String,
}

#[derive(Debug, Clone, Default)]
pub struct Header {
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    pub version: Option<u64>,
    pub parent_session: Option<String>,
}

#[derive(Debug)]
pub struct Session {
    pub path: PathBuf,
    pub header: Header,
    pub entries: Vec<Entry>,
    /// Leaf ids in file order; the last one is the session's current position.
    pub leaves: Vec<String>,
    /// Lines that did not parse as JSON or lacked an `id`.
    pub skipped_lines: u64,
    pub bytes: u64,
    /// Set when the same custom message type repeats; the analysis uses it to
    /// explain context that is restated rather than accumulated.
    pub duplicate_notices: Vec<(String, u64, u64)>,
}

impl Session {
    /// The newest entry is the default leaf: files are append-only, so the
    /// last row is where the session stopped.
    #[must_use]
    pub fn default_leaf(&self) -> Option<&str> {
        self.leaves.last().map(String::as_str)
    }

    /// Walk `parentId` from a leaf back to the root. Cycles and dangling
    /// parents stop the walk instead of looping.
    #[must_use]
    pub fn branch(&self, leaf_id: &str) -> Vec<usize> {
        let by_id: HashMap<&str, usize> = self
            .entries
            .iter()
            .enumerate()
            .map(|(index, entry)| (entry.id.as_str(), index))
            .collect();
        let mut path = Vec::new();
        let mut seen = std::collections::HashSet::new();
        let mut current = by_id.get(leaf_id).copied();
        while let Some(index) = current {
            if !seen.insert(index) {
                break;
            }
            path.push(index);
            current = self.entries[index]
                .parent
                .as_deref()
                .and_then(|parent| by_id.get(parent).copied());
        }
        path.reverse();
        path
    }

    /// Sessions carry a display name set by `/name`; the newest one wins.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        self.entries
            .iter()
            .rev()
            .find_map(|entry| match &entry.kind {
                Kind::SessionInfo { name } if !name.is_empty() => Some(name.as_str()),
                _ => None,
            })
    }

    #[must_use]
    pub fn first_at(&self) -> i64 {
        self.entries.iter().map(|entry| entry.at).min().unwrap_or(0)
    }

    #[must_use]
    pub fn last_at(&self) -> i64 {
        self.entries.iter().map(|entry| entry.at).max().unwrap_or(0)
    }

    #[must_use]
    pub fn compaction_count(&self) -> u64 {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.kind, Kind::Compaction { .. }))
            .count() as u64
    }

    /// Peak prompt the provider reported on the default branch, and the newest
    /// one. Peak answers "how close did this session get to the ceiling".
    #[must_use]
    pub fn prompt_extremes(&self, leaf_id: &str) -> (u64, u64) {
        let (peak, _, newest) = self.prompt_peak(leaf_id);
        (peak, newest)
    }

    /// Peak prompt, when it happened, and the newest prompt. The timestamp is
    /// what turns "369k" from a number into a fact about the session: a peak at
    /// the very start is a different story from one at the end.
    #[must_use]
    pub fn prompt_peak(&self, leaf_id: &str) -> (u64, i64, u64) {
        let mut peak = 0;
        let mut peak_at = 0;
        let mut newest = 0;
        for index in self.branch(leaf_id) {
            let entry = &self.entries[index];
            if let Kind::Assistant {
                usage: Some(usage), ..
            } = &entry.kind
            {
                let prompt = usage.prompt();
                if prompt > peak {
                    peak = prompt;
                    peak_at = entry.at;
                }
                newest = prompt;
            }
        }
        (peak, peak_at, newest)
    }
}

/// The raw JSONL line for one entry, read by seeking to its recorded offset.
///
/// The parsed session deliberately keeps counts rather than text; a preview
/// that wants to show a bucket's contents would otherwise have to re-parse the
/// whole file (187 MB on the largest session). One `seek` to a known offset is
/// O(line length).
///
/// # Errors
///
/// Returns a message when the file shrank or the offset no longer holds JSON.
pub fn raw_line_at(path: &Path, offset: u64) -> Result<Value, String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    file.seek(SeekFrom::Start(offset))
        .map_err(|error| format!("seek {offset}: {error}"))?;
    // Read in chunks until the newline: a line can be 400 KB, and the maximum
    // is not worth guessing.
    let mut line = Vec::new();
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut chunk)
            .map_err(|error| format!("read {offset}: {error}"))?;
        if read == 0 {
            break;
        }
        if let Some(newline) = chunk[..read].iter().position(|&byte| byte == b'\n') {
            line.extend_from_slice(&chunk[..newline]);
            break;
        }
        line.extend_from_slice(&chunk[..read]);
    }
    let text = String::from_utf8_lossy(&line);
    serde_json::from_str(text.trim()).map_err(|error| format!("line at {offset}: {error}"))
}

/// Read a session file. A line that fails to parse is counted and skipped
/// rather than failing the load: an analyzer is more useful when it can still
/// show a partially damaged session.
///
/// # Errors
///
/// Returns a [`LoadError`] when the file cannot be opened or read.
pub fn load(path: &Path) -> Result<Session, LoadError> {
    let file = File::open(path).map_err(|error| LoadError {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    let bytes = file.metadata().map(|meta| meta.len()).unwrap_or_default();
    let reader = BufReader::with_capacity(1 << 20, file);

    let mut header = Header::default();
    let mut entries: Vec<Entry> = Vec::new();
    let mut children: HashMap<String, u64> = HashMap::new();
    let mut skipped_lines = 0_u64;
    // Byte offset of the next line, so each entry can point back at its own
    // line. `read_until` (not `lines`) because the offset must count the
    // newline bytes.
    let mut offset = 0_u64;
    let mut line = String::new();
    let mut reader = reader;

    loop {
        line.clear();
        let start = offset;
        let Ok(read) = reader.read_line(&mut line) else {
            skipped_lines += 1;
            break;
        };
        if read == 0 {
            break;
        }
        offset += read as u64;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(trimmed) else {
            skipped_lines += 1;
            continue;
        };
        if value.get("type").and_then(Value::as_str) == Some("session") {
            header = parse_header(&value);
            continue;
        }
        let Some(entry) = parse_entry_at(&value, start) else {
            skipped_lines += 1;
            continue;
        };
        if let Some(parent) = &entry.parent {
            *children.entry(parent.clone()).or_insert(0) += 1;
        }
        entries.push(entry);
    }

    let leaves: Vec<String> = entries
        .iter()
        .filter(|entry| !children.contains_key(&entry.id))
        .map(|entry| entry.id.clone())
        .collect();

    let mut session = Session {
        path: path.to_path_buf(),
        header,
        entries,
        leaves,
        skipped_lines,
        bytes,
        duplicate_notices: Vec::new(),
    };
    session.duplicate_notices = duplicate_custom_types(&session);
    Ok(session)
}

fn parse_header(value: &Value) -> Header {
    Header {
        id: value
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        timestamp: value
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        cwd: value
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        version: value.get("version").and_then(Value::as_u64),
        parent_session: value
            .get("parentSession")
            .and_then(Value::as_str)
            .map(str::to_string),
    }
}

/// Custom message types that appear more than once, with their count and the
/// characters they occupy. `ipython_state` restating itself is the known
/// context-bloat pattern, so the report calls it out by name.
fn duplicate_custom_types(session: &Session) -> Vec<(String, u64, u64)> {
    let mut counts: HashMap<&str, (u64, u64)> = HashMap::new();
    for entry in &session.entries {
        if let Kind::CustomMessage { custom_type, chars } = &entry.kind {
            let slot = counts.entry(custom_type.as_str()).or_insert((0, 0));
            slot.0 += 1;
            slot.1 += chars.total();
        }
    }
    let mut repeated: Vec<(String, u64, u64)> = counts
        .into_iter()
        .filter(|(_, (count, _))| *count > 1)
        .map(|(name, (count, chars))| (name.to_string(), count, chars))
        .collect();
    repeated.sort_by_key(|left| std::cmp::Reverse(left.2));
    repeated
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_session(lines: &[&str]) -> (tempfile_dir::TempDir, PathBuf) {
        let dir = tempfile_dir::TempDir::new().expect("temp dir");
        let path = dir.path().join("s.jsonl");
        let mut file = File::create(&path).expect("create");
        for line in lines {
            writeln!(file, "{line}").expect("write");
        }
        (dir, path)
    }

    /// Minimal stand-in for the `tempfile` crate: the analyzer keeps its
    /// dependency list to serde only.
    mod tempfile_dir {
        use std::path::{Path, PathBuf};

        pub struct TempDir(PathBuf);

        impl TempDir {
            pub fn new() -> std::io::Result<Self> {
                static COUNTER: std::sync::atomic::AtomicU64 =
                    std::sync::atomic::AtomicU64::new(0);
                let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let path = std::env::temp_dir().join(format!(
                    "prime-agent-inspect-test-{}-{unique}",
                    std::process::id()
                ));
                std::fs::create_dir_all(&path)?;
                Ok(Self(path))
            }

            pub fn path(&self) -> &Path {
                &self.0
            }
        }

        impl Drop for TempDir {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn loads_header_entries_and_leaf() {
        let (_dir, path) = write_session(&[
            r#"{"type":"session","id":"s1","version":3,"timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp/work"}"#,
            r#"{"type":"message","id":"e1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"hello"}}"#,
            r#"{"type":"message","id":"e2","parentId":"e1","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"hi"}],"usage":{"input":5,"output":1}}}"#,
        ]);
        let session = load(&path).expect("loads");
        assert_eq!(session.header.id, "s1");
        assert_eq!(session.header.cwd, "/tmp/work");
        assert_eq!(session.entries.len(), 2);
        assert_eq!(session.default_leaf(), Some("e2"));
        assert_eq!(session.branch("e2"), vec![0, 1]);
    }

    #[test]
    fn malformed_lines_are_counted_not_fatal() {
        let (_dir, path) = write_session(&[
            r#"{"type":"session","id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
            "{not json",
            r#"{"type":"message","id":"e1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"x"}}"#,
        ]);
        let session = load(&path).expect("loads");
        assert_eq!(session.entries.len(), 1);
        assert_eq!(session.skipped_lines, 1);
    }

    #[test]
    fn a_cycle_stops_the_branch_walk() {
        let (_dir, path) = write_session(&[
            r#"{"type":"session","id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"a","parentId":"b","timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"x"}}"#,
            r#"{"type":"message","id":"b","parentId":"a","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"user","content":"y"}}"#,
        ]);
        let session = load(&path).expect("loads");
        let branch = session.branch("b");
        assert!(branch.len() <= 2, "cycle must not loop: {branch:?}");
    }

    #[test]
    fn repeated_notices_are_reported_by_size() {
        let (_dir, path) = write_session(&[
            r#"{"type":"session","id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
            r#"{"type":"custom_message","id":"c1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","customType":"ipython_state","content":"aaaa"}"#,
            r#"{"type":"custom_message","id":"c2","parentId":"c1","timestamp":"2026-01-01T00:00:02.000Z","customType":"ipython_state","content":"bbbb"}"#,
        ]);
        let session = load(&path).expect("loads");
        assert_eq!(session.duplicate_notices.len(), 1);
        let (name, count, chars) = &session.duplicate_notices[0];
        assert_eq!(name, "ipython_state");
        assert_eq!(*count, 2);
        assert_eq!(*chars, 8);
    }

    /// The parser keeps counts, so a preview needs a way back to the text.
    /// Every entry must point at the exact byte where its line starts.
    #[test]
    fn entries_remember_the_offset_of_their_own_line() {
        let (_dir, path) = write_session(&[
            r#"{"type":"session","id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
            r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"first"}}"#,
            r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"second"}]}}"#,
        ]);
        let session = load(&path).expect("loads");
        assert_eq!(session.entries.len(), 2);
        for entry in &session.entries {
            let value = raw_line_at(&path, entry.offset).expect("line readable");
            assert_eq!(
                value.get("id").and_then(Value::as_str),
                Some(entry.id.as_str()),
                "offset {} does not point at entry {}",
                entry.offset,
                entry.id
            );
        }
        // The header line is not an entry, so the first entry starts past it.
        assert!(session.entries[0].offset > 0);
    }

    /// Reading past the end must be an error, not an empty entry.
    #[test]
    fn reading_past_the_end_reports_an_error() {
        let (_dir, path) = write_session(&[
            r#"{"type":"session","id":"s1","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
        ]);
        let bytes = std::fs::metadata(&path).expect("stat").len();
        assert!(raw_line_at(&path, bytes).is_err());
    }
}
