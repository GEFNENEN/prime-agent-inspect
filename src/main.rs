//! CLI surface.
//!
//! Deliberately narrow: everything the analyzer can do is reachable from
//! `main`, and each subcommand prints either a human table or JSON. A UI, if
//! added later, is another consumer of the same `lib` functions rather than a
//! second implementation.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;

use prime_agent_inspect::calibrate::{calibrate, summarize, trust};
use prime_agent_inspect::context::{assemble, compaction_timeline};
use prime_agent_inspect::session::{load, Session};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("prime-agent-inspect: {error}");
            ExitCode::FAILURE
        }
    }
}

fn usage() -> String {
    "\
prime-agent-inspect — read-only analyzer for prime-agent sessions

USAGE:
  prime-agent-inspect serve  <sessions-dir> [--port N] [--open]
                                           browser UI: list, click, refresh
  prime-agent-inspect list   <sessions-dir>            every session with size, turns, compactions
  prime-agent-inspect show   <session.jsonl>           compaction timeline and the prompt each left
  prime-agent-inspect check  <session.jsonl>           how far the token estimate drifts from measured usage
  prime-agent-inspect help

Token counts come from provider usage where the file records it; anything
else is estimated from a CJK-aware characters-per-token calibration and
labelled EST.

serve binds 127.0.0.1 only: the analyzer reads whole session files, so it
must not be reachable from the network."
        .to_string()
}

fn run(args: &[String]) -> Result<(), String> {
    match args.first().map(String::as_str) {
        Some("serve") => {
            let dir = args.get(1).ok_or("serve requires a sessions directory")?;
            let port = flag_value(args, "--port")
                .and_then(|value| value.parse::<u16>().ok())
                .unwrap_or(7700);
            let open = args.iter().any(|arg| arg == "--open");
            serve(Path::new(dir), port, open)
        }
        Some("list") => {
            let dir = args.get(1).ok_or("list requires a sessions directory")?;
            list(Path::new(dir))
        }
        Some("show") => {
            let file = args.get(1).ok_or("show requires a session file")?;
            show(Path::new(file))
        }
        Some("check") => {
            let file = args.get(1).ok_or("check requires a session file")?;
            check(Path::new(file))
        }
        Some("help" | "--help" | "-h") | None => {
            println!("{}", usage());
            Ok(())
        }
        Some(other) => Err(format!("unknown command {other:?}\n\n{}", usage())),
    }
}

fn flag_value(args: &[String], flag: &str) -> Option<String> {
    args.iter()
        .position(|arg| arg == flag)
        .and_then(|index| args.get(index + 1))
        .cloned()
}

/// Serve the browser UI. Sessions are re-read per request, so the page's
/// refresh button is the snapshot boundary: what you see is what the files
/// held when you pressed it.
fn serve(dir: &Path, port: u16, open: bool) -> Result<(), String> {
    let root = dir.to_path_buf();
    let address = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    let shown = root.clone();
    let cache = Arc::new(prime_agent_inspect::cache::Cache::new());

    let handler = move |target: &str| -> prime_agent_inspect::http::Response {
        let (path, query) = match target.split_once('?') {
            Some((path, query)) => (path, query),
            None => (target, ""),
        };
        match path {
            "/" | "/index.html" => prime_agent_inspect::http::Response::html(include_str!("ui.html")),
            "/api/sessions" => match list_payload(&root, &cache) {
                Ok(body) => prime_agent_inspect::http::Response::json(body),
                Err(error) => prime_agent_inspect::http::Response::text(500, &error),
            },
            "/api/session" => match query_value(query, "id") {
                Some(id) => match session_payload(&root, &id, &cache) {
                    Ok(body) => prime_agent_inspect::http::Response::json(body),
                    Err(error) => error,
                },
                None => prime_agent_inspect::http::Response::text(400, "missing id"),
            },
            "/api/drop-cache" => {
                cache.clear();
                prime_agent_inspect::http::Response::json("{\"ok\":true}".to_string())
            }
            "/favicon.ico" => prime_agent_inspect::http::Response {
                status: 204,
                content_type: "image/x-icon",
                body: Vec::new(),
            },
            _ => prime_agent_inspect::http::Response::text(404, "not found"),
        }
    };

    let url = format!("http://127.0.0.1:{port}/");
    eprintln!("prime-agent-inspect serving {url}");
    eprintln!("  sessions: {}", shown.display());
    eprintln!("  press Ctrl+C to stop");
    if open {
        open_browser(&url);
    }
    prime_agent_inspect::http::serve(address, Arc::new(handler)).map_err(|error| {
        format!("cannot bind 127.0.0.1:{port}: {error}\n(is another prime-agent-inspect already running?)")
    })
}

fn open_browser(url: &str) {
    #[cfg(windows)]
    let _ = std::process::Command::new("cmd")
        .args(["/C", "start", "", url])
        .spawn();
    #[cfg(target_os = "macos")]
    let _ = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let _ = std::process::Command::new("xdg-open").arg(url).spawn();
}

fn query_value(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (name, value) = pair.split_once('=')?;
        if name == key {
            Some(percent_decode(value))
        } else {
            None
        }
    })
}

fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[index + 1..index + 3]).unwrap_or("");
            if let Ok(byte) = u8::from_str_radix(hex, 16) {
                out.push(byte);
                index += 3;
                continue;
            }
        }
        if bytes[index] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[index]);
        }
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn list_payload(root: &Path, cache: &Arc<prime_agent_inspect::cache::Cache>) -> Result<String, String> {
    let files = session_files(root)?;
    let mut rows = Vec::new();
    let mut total_bytes = 0_u64;
    for path in files {
        let Ok(cached) = cache.get(&path) else {
            continue;
        };
        total_bytes += cached.session.bytes;
        rows.push(cached.row.clone());
    }
    // Newest activity first: that is what a user looks for.
    rows.sort_by_key(|row| std::cmp::Reverse(newest_of(row)));
    let payload = format!(
        "{{\"root\":{},\"totalBytes\":{},\"sessions\":[{}]}}",
        serde_json::to_string(&root.to_string_lossy()).unwrap_or_default(),
        total_bytes,
        rows.join(",")
    );
    Ok(payload)
}

/// `lastAt` out of an already-serialized list row, without re-parsing it.
fn newest_of(row: &str) -> i64 {
    row.split("\"lastAt\":")
        .nth(1)
        .and_then(|rest| rest.split([',', '}']).next())
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(0)
}

fn session_payload(
    root: &Path,
    id: &str,
    cache: &Arc<prime_agent_inspect::cache::Cache>,
) -> Result<String, prime_agent_inspect::http::Response> {
    // `id` arrives from the URL and may be either a file stem or the header id
    // inside the file; imported or duplicated session files have both, and they
    // do not always agree. Resolve against the directory listing instead of
    // trusting the value as a path, so `../` cannot escape either way.
    let mut candidates: Vec<PathBuf> = Vec::new();
    if !id.is_empty()
        && id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        candidates.push(root.join(format!("{id}.jsonl")));
    }
    if candidates.iter().all(|path| !path.is_file()) {
        // Fall back to a scan: the header id wins when a file was copied.
        if let Ok(files) = session_files(root) {
            for path in files {
                let stem_matches = path
                    .file_stem()
                    .is_some_and(|stem| stem.to_string_lossy() == id);
                if stem_matches {
                    candidates.push(path);
                    break;
                }
            }
            if candidates.iter().all(|path| !path.is_file()) {
                for path in session_files(root).unwrap_or_default() {
                    if header_id(&path).as_deref() == Some(id) {
                        candidates.push(path);
                        break;
                    }
                }
            }
        }
    }
    let path = candidates
        .into_iter()
        .find(|path| path.is_file())
        .ok_or_else(|| prime_agent_inspect::http::Response::text(404, "no such session"))?;

    let cached = cache
        .get(&path)
        .map_err(|error| prime_agent_inspect::http::Response::text(500, &error))?;
    if let Some(body) = cached.detail() {
        return Ok(body);
    }
    match serde_json::to_string(&prime_agent_inspect::web::detail(&cached.session, None)) {
        Ok(body) => {
            cached.set_detail(&body);
            Ok(body)
        }
        Err(error) => Err(prime_agent_inspect::http::Response::text(
            500,
            &format!("cannot serialize: {error}"),
        )),
    }
}

/// The `id` inside a session file's header line, read without parsing the rest.
fn header_id(path: &Path) -> Option<String> {
    use std::io::{BufRead, BufReader};
    let file = std::fs::File::open(path).ok()?;
    let mut first = String::new();
    BufReader::new(file).read_line(&mut first).ok()?;
    let value: serde_json::Value = serde_json::from_str(first.trim()).ok()?;
    value
        .get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_string)
}

fn session_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|error| format!("{}: {error}", dir.display()))?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "jsonl") {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn list(dir: &Path) -> Result<(), String> {
    let files = session_files(dir)?;
    println!(
        "{:<38} {:>8} {:>7} {:>6} {:>9} {:>9}  name",
        "session", "size", "entries", "comp", "peak", "newest"
    );
    let mut total_bytes = 0_u64;
    for path in files {
        let Ok(session) = load(&path) else {
            println!("{:<38} (unreadable)", short(&path));
            continue;
        };
        total_bytes += session.bytes;
        let leaf = session.default_leaf().unwrap_or_default();
        let (peak, newest) = if leaf.is_empty() {
            (0, 0)
        } else {
            session.prompt_extremes(leaf)
        };
        println!(
            "{:<38} {:>8} {:>7} {:>6} {:>9} {:>9}  {}",
            short(&path),
            human_bytes(session.bytes),
            session.entries.len(),
            session.compaction_count(),
            peak,
            newest,
            session.name().unwrap_or("")
        );
    }
    println!("\ntotal {}", human_bytes(total_bytes));
    Ok(())
}

fn show(file: &Path) -> Result<(), String> {
    let session = load(file).map_err(|error| format!("{}: {}", error.path.display(), error.message))?;
    let leaf = session
        .default_leaf()
        .ok_or("session has no entries")?
        .to_string();

    print_header(&session, &leaf);
    print_assembly(&session, &leaf);
    print_timeline(&session, &leaf);
    print_notices(&session);
    Ok(())
}

fn print_header(session: &Session, leaf: &str) {
    let branch = session.branch(leaf);
    println!("file      {}", session.path.display());
    println!("id        {}", session.header.id);
    println!("cwd       {}", session.header.cwd);
    println!(
        "entries   {} on disk, {} on the active branch",
        session.entries.len(),
        branch.len()
    );
    println!("size      {}", human_bytes(session.bytes));
    if session.skipped_lines > 0 {
        println!("skipped   {} unparseable lines", session.skipped_lines);
    }
    if let Some(name) = session.name() {
        println!("name      {name}");
    }
    println!();
}

fn print_assembly(session: &Session, leaf: &str) {
    let assembly = assemble(session, leaf);
    println!("current prompt composition (what the next turn would send)");
    println!(
        "  {:<26} {:>10} {:>10} {:>7}",
        "component", "chars", "tokens", "share"
    );
    let total = assembly.total_tokens();
    for component in &assembly.components {
        let share = if total == 0 {
            0.0
        } else {
            #[allow(clippy::cast_precision_loss)]
            let tokens = component.tokens as f64;
            #[allow(clippy::cast_precision_loss)]
            let total = total as f64;
            tokens / total * 100.0
        };
        println!(
            "  {:<26} {:>10} {:>10} {:>6.1}%{}",
            component.label,
            component.chars.total(),
            component.tokens,
            share,
            if component.estimated { " EST" } else { "" }
        );
    }
    println!(
        "  {:<26} {:>10} {:>10}\n  ({} entries kept, {} dropped)\n",
        "TOTAL",
        assembly.total_chars().total(),
        total,
        assembly.kept_entries,
        assembly.dropped_entries
    );
}

fn print_timeline(session: &Session, leaf: &str) {
    let timeline = compaction_timeline(session, leaf);
    if timeline.is_empty() {
        println!("no compactions on this branch\n");
        return;
    }
    println!("compaction timeline ({} total)", timeline.len());
    println!(
        "  {:<22} {:>9} {:>9} {:>9} {:>7} {:>6} {:>5}",
        "when", "before", "after", "retained", "notice", "copies", "turns"
    );
    for point in timeline.iter().rev().take(20).rev() {
        println!(
            "  {:<22} {:>9} {:>9} {:>9} {:>7} {:>6} {:>5}",
            short_time(&point.timestamp),
            point.tokens_before,
            point
                .prompt_after
                .map_or_else(|| "-".to_string(), |value| value.to_string()),
            point.retained_tokens,
            point.notice_tokens,
            point.notice_copies,
            point.gap_turns
        );
    }
    if timeline.len() > 20 {
        println!("  (showing the last 20 of {})", timeline.len());
    }
    println!();
}

fn print_notices(session: &Session) {
    if session.duplicate_notices.is_empty() {
        return;
    }
    println!("repeated custom messages (candidates for context bloat)");
    for (name, count, chars) in &session.duplicate_notices {
        println!("  {name:<24} {count:>5} copies {chars:>10} chars");
    }
    println!();
}

fn check(file: &Path) -> Result<(), String> {
    let session = load(file).map_err(|error| format!("{}: {}", error.path.display(), error.message))?;
    let leaf = session
        .default_leaf()
        .ok_or("session has no entries")?
        .to_string();
    let rows = calibrate(&session, &leaf);
    if rows.is_empty() {
        println!("no compaction has a measured follow-up prompt; nothing to check");
        return Ok(());
    }
    println!(
        "  {:<22} {:>10} {:>10} {:>9}",
        "when", "measured", "estimated", "error"
    );
    for row in rows.iter().rev().take(15).rev() {
        println!(
            "  {:<22} {:>10} {:>10} {:>8.1}%",
            short_time(&row.timestamp),
            row.measured,
            row.estimated,
            row.error() * 100.0
        );
    }
    if let Some((median, worst, count)) = summarize(&rows) {
        let fixed = trust(&session, &leaf);
        println!(
            "\n{} compactions checked: median |error| {:.1}%, worst {:.1}%",
            count,
            median * 100.0,
            worst * 100.0
        );
        if fixed.measured {
            println!(
                "fixed overhead {} tokens (measured from {} point(s), not in the session file)",
                fixed.tokens, fixed.samples
            );
        } else {
            println!(
                "fixed overhead {} tokens (assumed; this session records no measurable turn)",
                fixed.tokens
            );
        }
    }
    Ok(())
}

fn short(path: &Path) -> String {
    path.file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default()
}

fn short_time(timestamp: &str) -> String {
    timestamp
        .get(5..)
        .unwrap_or(timestamp)
        .trim_end_matches('Z')
        .replace('T', " ")
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "K", "M", "G"];
    #[allow(clippy::cast_precision_loss)]
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}{}", UNITS[0])
    } else {
        format!("{value:.1}{}", UNITS[unit])
    }
}
