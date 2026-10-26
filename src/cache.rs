//! Caching layer for the UI.
//!
//! A cold read of a 184 MB session takes ~19 s: the file is parsed from
//! scratch on every request. A browser tab that clicks a session and then
//! refreshes would pay that twice. The cache keys on `(path, size,
//! modified-time)`, so a session that has not been appended to is served from
//! memory, and one that has changed is re-read exactly once.
//!
//! Only the derived numbers are cached, never file contents: the parsed
//! [`Session`] holds character counts, not text, so the whole corpus costs a
//! few MB rather than the 418 MB on disk.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use crate::session::Session;

/// Identity of one on-disk revision of a session file.
#[derive(PartialEq, Eq, Clone, Copy)]
struct Stamp {
    size: u64,
    modified: Option<SystemTime>,
}

/// A parsed session plus the numbers the UI asks for.
pub struct Cached {
    pub session: Session,
    /// Serialized list row, built once per revision.
    pub row: String,
    /// Serialized detail payload, built on first request. Interior mutability
    /// so a warm read does not repeat the assembly work.
    detail: Mutex<Option<String>>,
}

impl Cached {
    /// The cached detail payload, if one was already built.
    #[must_use]
    pub fn detail(&self) -> Option<String> {
        self.detail
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().cloned())
    }

    /// Record a freshly built detail payload.
    pub fn set_detail(&self, body: &str) {
        if let Ok(mut guard) = self.detail.lock() {
            *guard = Some(body.to_string());
        }
    }
}#[derive(Default)]
pub struct Cache {
    entries: Mutex<HashMap<PathBuf, (Stamp, std::sync::Arc<Cached>)>>,
}

impl Cache {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Fetch a session, reusing the parsed copy when the file is unchanged.
    ///
    /// # Errors
    ///
    /// Returns the load error message when the file cannot be read.
    pub fn get(&self, path: &Path) -> Result<std::sync::Arc<Cached>, String> {
        let stamp = stamp_of(path)?;

        if let Ok(guard) = self.entries.lock() {
            if let Some((cached_stamp, cached)) = guard.get(path) {
                if *cached_stamp == stamp {
                    return Ok(std::sync::Arc::clone(cached));
                }
            }
        }

        let session = crate::session::load(path).map_err(|error| {
            format!("{}: {}", error.path.display(), error.message)
        })?;
        let row = serde_json::to_string(&crate::web::list_row(&session))
            .unwrap_or_else(|_| "{}".to_string());
        let cached = std::sync::Arc::new(Cached {
            session,
            row,
            detail: Mutex::new(None),
        });
        if let Ok(mut guard) = self.entries.lock() {
            guard.insert(path.to_path_buf(), (stamp, std::sync::Arc::clone(&cached)));
        }
        Ok(cached)
    }

    /// Drop every cached revision. The UI's refresh button does not call this
    /// — it relies on the stamp — but a long-running server can reclaim the
    /// memory of sessions it will never show again.
    pub fn clear(&self) {
        if let Ok(mut guard) = self.entries.lock() {
            guard.clear();
        }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.lock().map(|guard| guard.len()).unwrap_or(0)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn stamp_of(path: &Path) -> Result<Stamp, String> {
    let meta = std::fs::metadata(path).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(Stamp {
        size: meta.len(),
        modified: meta.modified().ok(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new() -> Self {
            static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path =
                std::env::temp_dir().join(format!("prime-agent-inspect-cache-{}-{unique}", std::process::id()));
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

    fn write(path: &Path, lines: &[&str]) {
        let mut file = std::fs::File::create(path).expect("create");
        for line in lines {
            writeln!(file, "{line}").expect("write");
        }
    }

    #[test]
    fn an_unchanged_file_is_served_from_memory() {
        let dir = TempDir::new();
        let path = dir.path().join("s.jsonl");
        write(
            &path,
            &[
                r#"{"type":"session","id":"s","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
                r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"x"}}"#,
            ],
        );
        let cache = Cache::new();
        let first = cache.get(&path).expect("loads");
        let second = cache.get(&path).expect("loads");
        assert!(
            std::sync::Arc::ptr_eq(&first, &second),
            "unchanged file must reuse the parsed copy"
        );
    }

    #[test]
    fn an_appended_file_is_reread() {
        let dir = TempDir::new();
        let path = dir.path().join("s.jsonl");
        write(
            &path,
            &[
                r#"{"type":"session","id":"s","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
                r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"x"}}"#,
            ],
        );
        let cache = Cache::new();
        let first = cache.get(&path).expect("loads");
        assert_eq!(first.session.entries.len(), 1);

        write(
            &path,
            &[
                r#"{"type":"session","id":"s","timestamp":"2026-01-01T00:00:00.000Z","cwd":"/tmp"}"#,
                r#"{"type":"message","id":"u1","parentId":null,"timestamp":"2026-01-01T00:00:01.000Z","message":{"role":"user","content":"x"}}"#,
                r#"{"type":"message","id":"a1","parentId":"u1","timestamp":"2026-01-01T00:00:02.000Z","message":{"role":"assistant","content":[{"type":"text","text":"y"}],"usage":{"input":5}}}"#,
            ],
        );
        let second = cache.get(&path).expect("loads");
        assert_eq!(second.session.entries.len(), 2, "growth must invalidate");
    }
}
