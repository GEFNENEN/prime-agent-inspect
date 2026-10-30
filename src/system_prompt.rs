//! What prime-agent sends besides the conversation.
//!
//! The system prompt is the largest piece of a request that is not in the
//! session file, and it is assembled at runtime from three kinds of input:
//! layers compiled into the binary, files on disk (`SYSTEM.md`,
//! `APPEND_SYSTEM.md`, `AGENTS.md`, `CLAUDE.md`), and the installed skill list.
//! Re-deriving that assembly here would be a second implementation that drifts
//! on the first upstream change, so the layers come from the binary itself:
//! `prime-agent prompt --json` assembles a fresh root session's prompt and
//! returns the per-layer split with the final text.
//!
//! Two things are deliberately *not* guessed:
//!
//! * `APPEND_SYSTEM.md` — the `prompt` command does not resolve it (verified:
//!   the file was placed at four candidate paths and never appeared), but real
//!   sessions do contain it. It is read here and marked as added by the viewer.
//! * Tool definitions — they are sent as provider-level tools, not as prompt
//!   text, and their schemas are produced per model at request time. The module
//!   reports the gap instead of inventing a number.

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::model::Chars;

/// What a caller asked to assemble.
pub struct Request<'a> {
    pub binary: &'a Path,
    pub cwd: &'a Path,
    pub model: Option<&'a str>,
    /// Agent root (`~/.prime/agent`); `APPEND_SYSTEM.md` lives here.
    pub agent_root: &'a Path,
}

/// Assemble the prompt and, when the file exists, add the append layer.
#[must_use]
pub fn assemble(request: &Request<'_>) -> Value {
    let mut body = match run_prompt(request) {
        Ok(value) => value,
        Err(message) => return json!({ "error": message }),
    };
    let appended = append_layer(request.agent_root);
    let mut layers = body
        .get("layers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if let Some(layer) = appended {
        layers.push(layer);
    }

    // The final text is the layers in order. Keeping it in one string is what
    // makes "final assembled version" a real view rather than a summary.
    let prompt = body.get("prompt").and_then(Value::as_str).unwrap_or("").to_string();
    let prompt_chars = prompt.chars().count();
    let mut final_text = prompt;
    let mut notes = vec![json!({
        "kind": "gap",
        "text": "工具定义（JSON schema）由运行时按模型生成，不在提示词文本里；本页不估算它。",
    })];
    if let Some(appended) = layers.last().filter(|layer| layer["name"] == "appended-prompt") {
        let text = appended.get("text").and_then(Value::as_str).unwrap_or("");
        if !text.is_empty() {
            let separator = if final_text.is_empty() { "" } else { "\n\n" };
            final_text.push_str(separator);
            final_text.push_str(text);
            notes.push(json!({
                "kind": "added",
                "text": "`APPEND_SYSTEM.md` 由本页补在末尾：`prime-agent prompt` 不解析该文件，但真实会话里有。",
            }));
        }
    }
    if let Some(layer) = layers.iter().find(|layer| layer["name"] == "custom") {
        notes.push(json!({
            "kind": "replace",
            "text": format!("检测到 SYSTEM.md（{}），它替换了 core / usage / opinionated 三层内置提示词。",
                layer.get("source").and_then(Value::as_str).unwrap_or("?")),
        }));
    }
    notes.push(json!({
        "kind": "runtime",
        "text": "每轮还会追加即时内容（harness digest、压缩摘要等），它们不在本页，也不在系统提示词里。",
    }));

    let estimated = crate::token_est::estimate(Chars::from_text(&final_text)).0;
    let cached = body.get("cachedPrefixChars").and_then(Value::as_u64).unwrap_or(0);
    body["layers"] = Value::Array(layers);
    body["prompt"] = Value::String(final_text.clone());
    body["finalChars"] = json!(final_text.chars().count());
    body["finalTokens"] = json!(estimated);
    body["basePromptChars"] = json!(prompt_chars);
    body["cachedPrefixChars"] = json!(cached);
    body["notes"] = Value::Array(notes);
    body["binary"] = json!(request.binary.display().to_string());
    body["cwd"] = json!(request.cwd.display().to_string());
    body
}

/// Run the CLI and split its output into layers with per-`##` sections.
fn run_prompt(request: &Request<'_>) -> Result<Value, String> {
    if request.cwd.as_os_str().is_empty() {
        return Err("没有可用的工作目录：请指定 cwd，或换一个能唯一匹配的会话 id".to_string());
    }
    let mut command = Command::new(request.binary);
    command
        .arg("prompt")
        .arg("--json")
        .arg("--cwd")
        .arg(request.cwd);
    if let Some(model) = request.model.filter(|value| !value.is_empty()) {
        command.arg("--model").arg(model);
    }
    let output = command
        .stdin(Stdio::null())
        .output()
        .map_err(|error| format!("不能运行 {}：{error}", request.binary.display()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!(
            "prime-agent prompt 退出码 {}：{}",
            output.status.code().unwrap_or(-1),
            first_line(&stderr, 300)
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let parsed: Value = serde_json::from_str(&text)
        .map_err(|error| format!("prompt --json 返回的不是 JSON：{error}"))?;
    let prompt = parsed.get("prompt").and_then(Value::as_str).unwrap_or("").to_string();
    let chars: Vec<char> = prompt.chars().collect();
    let cached = parsed
        .get("cachedPrefixLen")
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    let segments = parsed
        .get("segments")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    // Walk the text using the lengths the command reported. Blank lines between
    // layers belong to no layer, so they are skipped rather than folded into the
    // previous one; anything left at the end is reported, not hidden.
    let mut offset = 0_usize;
    let mut layers = Vec::with_capacity(segments.len());
    for segment in &segments {
        let declared = segment.get("chars").and_then(Value::as_u64).unwrap_or(0) as usize;
        let start = offset.min(chars.len());
        let end = start.saturating_add(declared).min(chars.len());
        let piece: String = chars[start..end].iter().collect();
        offset = end;
        while offset < chars.len() && chars[offset] == '\n' {
            offset += 1;
        }
        let complete = piece.chars().count() == declared;
        layers.push(json!({
            "name": segment.get("name").cloned().unwrap_or(Value::Null),
            "kind": segment.get("kind").cloned().unwrap_or(Value::Null),
            "source": segment.get("source").cloned().unwrap_or(Value::Null),
            "chars": piece.chars().count(),
            "declaredChars": declared,
            "tokens": crate::token_est::estimate(Chars::from_text(&piece)).0,
            "cached": start < cached,
            "complete": complete,
            "text": piece,
            "sections": sections(&piece),
        }));
    }
    Ok(json!({
        "prompt": prompt,
        "cachedPrefixChars": cached,
        "layers": layers,
        "unaccountedChars": chars.len().saturating_sub(offset),
    }))
}

/// Split a layer at its `##` headings. A layer is 15 KB of prose; without this
/// the reading view is one wall of text and nothing can be linked to.
fn sections(text: &str) -> Value {
    let mut rows = Vec::new();
    let mut title = String::from("（开头）");
    let mut body = String::new();
    for line in text.split_inclusive('\n') {
        if line.starts_with("## ") || line.starts_with("# ") {
            if !body.trim().is_empty() {
                rows.push(section_row(&title, &body));
            }
            title = line.trim_start_matches('#').trim().to_string();
            body = String::new();
            continue;
        }
        body.push_str(line);
    }
    if !body.trim().is_empty() {
        rows.push(section_row(&title, &body));
    }
    Value::Array(rows)
}

fn section_row(title: &str, body: &str) -> Value {
    json!({
        "title": title,
        "chars": body.chars().count(),
        "tokens": crate::token_est::estimate(Chars::from_text(body)).0,
        "text": body,
    })
}

/// `APPEND_SYSTEM.md` from the agent root, as a layer shaped like the others.
fn append_layer(agent: &Path) -> Option<Value> {
    let path = agent.join("APPEND_SYSTEM.md");
    let text = fs::read_to_string(&path).ok()?;
    if text.trim().is_empty() {
        return None;
    }
    Some(json!({
        "name": "appended-prompt",
        "kind": "file",
        "source": path.display().to_string(),
        "chars": text.chars().count(),
        "declaredChars": text.chars().count(),
        "tokens": crate::token_est::estimate(Chars::from_text(&text)).0,
        "cached": false,
        "complete": true,
        "text": text,
        "sections": sections(&text),
    }))
}

/// Every path that can contribute, with existence and size, so a missing
/// `SYSTEM.md` is visibly absent instead of silently assumed.
#[must_use]
pub fn inputs(agent: &Path, cwd: &Path) -> Value {
    let mut rows = Vec::new();
    for (label, path) in [
        ("SYSTEM.md（项目）", cwd.join(".prime").join("agent").join("SYSTEM.md")),
        ("APPEND_SYSTEM.md（项目）", cwd.join(".prime").join("agent").join("APPEND_SYSTEM.md")),
        ("SYSTEM.md（全局）", agent.join("SYSTEM.md")),
        ("APPEND_SYSTEM.md（全局）", agent.join("APPEND_SYSTEM.md")),
        ("AGENTS.md", cwd.join("AGENTS.md")),
        ("CLAUDE.md", cwd.join("CLAUDE.md")),
    ] {
        let chars = fs::read_to_string(&path).map(|text| text.chars().count()).unwrap_or(0);
        rows.push(json!({
            "label": label,
            "path": path.display().to_string(),
            "exists": path.exists(),
            "chars": chars,
        }));
    }
    // The project config directory is discovered by the binary; report the
    // candidate the docs and the binary's own error strings name.
    rows.push(json!({
        "label": "说明",
        "path": cwd.join(".prime").join("agent").display().to_string(),
        "exists": cwd.join(".prime").join("agent").is_dir(),
        "chars": 0,
        "note": "项目级 SYSTEM.md / APPEND_SYSTEM.md 放在这里；实测 `prime-agent prompt` 只认 SYSTEM.md。",
    }));
    Value::Array(rows)
}

/// Find an executable `prime-agent`.
///
/// `where prime-agent` on this machine answers with the POSIX launcher in
/// `~/.local/bin`, which is a shell script — starting it as a process fails with
/// "not a valid Win32 application". The launcher's real target is the executable
/// two directories over, so the search starts from the launcher and follows it.
#[must_use]
pub fn resolve_binary(explicit: Option<&str>) -> PathBuf {
    if let Some(value) = explicit.filter(|value| !value.trim().is_empty()) {
        let path = PathBuf::from(value);
        if path.is_file() {
            return path;
        }
    }
    if let Some(value) = std::env::var_os("PRIME_AGENT_BINARY") {
        let path = PathBuf::from(value);
        if path.is_file() {
            return path;
        }
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(home) = home.as_ref() {
        let home = PathBuf::from(home);
        candidates.push(home.join(".local").join("share").join("prime-agent").join("prime-agent.exe"));
        candidates.push(home.join(".local").join("bin").join("prime-agent.exe"));
        candidates.push(home.join(".local").join("bin").join("prime-agent"));
    }
    if let Some(found) = where_prime_agent() {
        candidates.insert(0, found.clone());
        if let Some(dir) = found.parent() {
            candidates.insert(1, dir.join("prime-agent.exe"));
            candidates.insert(2, dir.join("..").join("share").join("prime-agent").join("prime-agent.exe"));
        }
    }
    for candidate in candidates {
        if candidate.is_file() && candidate.extension().is_some_and(|ext| ext.eq_ignore_ascii_case("exe")) {
            return candidate;
        }
    }
    PathBuf::from("prime-agent.exe")
}

fn where_prime_agent() -> Option<PathBuf> {
    let output = Command::new("where").arg("prime-agent").stdin(Stdio::null()).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout);
    text.lines().next().map(|line| PathBuf::from(line.trim()))
}

/// Compare the assembled prompt with what a session's provider actually charged.
///
/// The measured number (see `overhead`) is *everything before the transcript*:
/// system prompt plus tool definitions. The two numbers are therefore different
/// quantities, and subtracting them would present a guess as a measurement.
///
/// What is genuinely checkable is the implied characters per measured token. A
/// value below ~3 means the prompt text alone plausibly fills most of the
/// charge; a value far above ~8 means the session's `usage` did not count the
/// whole prompt (an under-reporting provider, or a cached prefix excluded).
/// Both are observations about the data, stated as such.
#[must_use]
pub fn compare(
    session: &crate::session::Session,
    leaf: &str,
    estimated_tokens: u64,
    prompt_chars: u64,
) -> Value {
    let measured = crate::overhead::overhead(session, leaf);
    let measured_tokens = measured.tokens;
    #[allow(clippy::cast_precision_loss)]
    let implied = if measured_tokens == 0 {
        0.0
    } else {
        prompt_chars as f64 / measured_tokens as f64
    };
    let (verdict, tone) = if measured_tokens == 0 {
        ("没有可用的实测值。", "unknown")
    } else if !measured.measured {
        ("该会话没有可测回合，这个数是估算兜底值，不能当对照。", "unknown")
    } else if implied > 8.0 {
        ("实测明显没算全：提示词长度是实测 token 数的 8 倍以上，该会话的 usage 没把整个 prompt 计进去。", "warn")
    } else if implied > 4.0 {
        ("提示词本身只占实测的一部分，其余是工具定义；工具定义的具体大小无法从这里拆出来。", "mid")
    } else {
        ("提示词文字就可能占掉实测的大部分，工具定义的空间很窄。", "ok")
    };
    json!({
        "session": session.header.id,
        "cwd": session.header.cwd,
        "measuredTokens": measured_tokens,
        "measuredFrom": if measured.measured { "本会话实测" } else { "估算兜底（无可测回合）" },
        "samples": measured.samples,
        "estimatedTokens": estimated_tokens,
        "promptChars": prompt_chars,
        "impliedCharsPerToken": implied,
        "tone": tone,
        "verdict": verdict,
        "explain": "实测 = 系统提示词 + 工具定义（JSON schema，不在提示词文本里）。两个量不同，所以不给差额。",
        "estimateNote": "token 估算用的是按会话语料校准的表；英文散文比代码更省 token，所以这个数偏保守（偏高）。",
    })
}

fn first_line(text: &str, limit: usize) -> String {
    let line = text.lines().next().unwrap_or("").trim().to_string();
    if line.chars().count() > limit {
        line.chars().take(limit).collect()
    } else {
        line
    }
}

/// Distinct working directories of the newest sessions, so the page can offer
/// the ones that actually assemble a different prompt.
#[must_use]
pub fn recent_cwds(sessions: &Path, limit: usize) -> Vec<Value> {
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = fs::read_dir(sessions)
        .map(|items| {
            items
                .flatten()
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl"))
                .filter_map(|entry| {
                    let meta = entry.metadata().ok()?;
                    Some((meta.modified().ok()?, entry.path()))
                })
                .collect()
        })
        .unwrap_or_default();
    files.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    let mut out: Vec<Value> = Vec::new();
    for (_, path) in files.into_iter().take(limit.saturating_mul(6)) {
        let Ok(text) = fs::read_to_string(&path) else { continue };
        let Some(first) = text.lines().next() else { continue };
        let Ok(header) = serde_json::from_str::<Value>(first) else { continue };
        let Some(cwd) = header.get("cwd").and_then(Value::as_str) else { continue };
        if cwd.is_empty() || out.iter().any(|row| row["cwd"] == cwd) {
            continue;
        }
        out.push(json!({ "cwd": cwd, "session": path.file_stem().map(|stem| stem.to_string_lossy().to_string()) }));
        if out.len() >= limit {
            break;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sections_split_at_headings_without_repeating_the_heading() {
        let text = "intro line\n## Alpha\nbody a\n## Beta\nbody b\n";
        let rows = sections(text);
        let rows = rows.as_array().expect("array");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["title"], "（开头）");
        assert_eq!(rows[1]["title"], "Alpha");
        assert_eq!(rows[2]["title"], "Beta");
        // The heading is rendered as the section label, so keeping it in the
        // body too would print every `## Title` twice on screen.
        for row in rows {
            assert!(!row["text"].as_str().expect("text").contains("## "));
        }
        // Section bodies plus heading lines must add back up to the layer, or
        // the reader is shown a summary that silently loses text.
        let body_chars: u64 = rows.iter().filter_map(|row| row["chars"].as_u64()).sum();
        let heading_chars = "## Alpha\n## Beta\n".chars().count() as u64;
        assert_eq!(body_chars + heading_chars, text.chars().count() as u64);
    }

    #[test]
    fn a_preamble_before_the_first_heading_is_its_own_section() {
        let rows = sections("# only\nbody\n");
        let rows = rows.as_array().expect("array");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["title"], "only");
        assert_eq!(rows[0]["text"], "body\n");
    }

    #[test]
    fn empty_text_has_no_sections() {
        assert!(sections("").as_array().expect("array").is_empty());
        assert!(sections("\n\n").as_array().expect("array").is_empty());
    }

    #[test]
    fn resolve_binary_prefers_an_explicit_existing_path() {
        let exe = std::env::current_exe().expect("current exe");
        let resolved = resolve_binary(Some(&exe.display().to_string()));
        assert_eq!(resolved, exe);
    }

    #[test]
    fn resolve_binary_ignores_a_missing_explicit_path() {
        // A path that does not exist must not be returned as if it worked: the
        // caller would run it and report a spawn error instead of a bad option.
        let resolved = resolve_binary(Some("C:/definitely/not/here/prime-agent.exe"));
        assert_ne!(resolved, PathBuf::from("C:/definitely/not/here/prime-agent.exe"));
    }

    #[test]
    fn recent_cwds_are_distinct_and_capped() {
        let dir = std::env::temp_dir().join("pai-recent-cwds-test");
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("mkdir");
        for (index, cwd) in ["C:/a", "C:/a", "C:/b", "C:/c"].iter().enumerate() {
            fs::write(
                dir.join(format!("s{index}.jsonl")),
                format!("{{\"cwd\":\"{cwd}\"}}\n"),
            )
            .expect("write");
        }
        let rows = recent_cwds(&dir, 2);
        assert_eq!(rows.len(), 2);
        let listed: Vec<&str> = rows.iter().filter_map(|row| row["cwd"].as_str()).collect();
        assert_eq!(listed.len(), 2);
        assert_ne!(listed[0], listed[1]);
        let _ = fs::remove_dir_all(&dir);
    }
}
