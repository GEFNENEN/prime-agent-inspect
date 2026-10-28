//! The global workspace, next to the session corpus.
//!
//! Sessions answer "where did this conversation's tokens go". This module
//! answers the other half: what is installed, how it is configured, what the
//! agent carries into every new session (harness memory, skills, models, MCP),
//! and what the logs and telemetry say about health.
//!
//! Everything here is read-only except [`save_file`], which accepts two config
//! files, validates them, and leaves a timestamped backup before writing.

use serde_json::{json, Map, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Instant, SystemTime};

/// Where the global state lives.
pub struct Roots {
    /// `~/.prime/agent`
    pub agent: PathBuf,
    /// `~/.agents/skills` — user-level skills, outside the agent directory.
    pub user_skills: PathBuf,
}

impl Roots {
    /// Derive the layout from the sessions directory the CLI was given.
    ///
    /// The sessions directory is `<agent>/sessions`, so its parent is the agent
    /// root. A caller that points at some other directory still gets a usable
    /// (if empty) workspace instead of an error.
    #[must_use]
    pub fn discover(sessions: &Path) -> Self {
        let agent = sessions
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .map_or_else(|| sessions.to_path_buf(), Path::to_path_buf);
        let user_skills = std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map_or_else(
                || agent.join("user-skills"),
                |home| PathBuf::from(home).join(".agents").join("skills"),
            );
        Self { agent, user_skills }
    }
}

/// Files the UI may read and write. `auth.json` is deliberately absent: it
/// holds credentials and nothing in this viewer needs them.
pub const EDITABLE: [&str; 2] = ["settings.json", "models.json"];

// ---------------------------------------------------------------- helpers

fn read_json(path: &Path) -> Option<Value> {
    let text = fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn modified_secs(path: &Path) -> u64 {
    path.metadata()
        .and_then(|meta| meta.modified())
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map_or(0, |delta| delta.as_secs())
}

/// Total bytes and file count below a directory. Directory entries that cannot
/// be read are skipped rather than failing the whole scan: a permission error
/// on one subtree should not hide the size of the rest.
fn dir_stats(dir: &Path) -> (u64, u64) {
    let mut bytes = 0_u64;
    let mut files = 0_u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(path) = stack.pop() {
        let Ok(entries) = fs::read_dir(&path) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                stack.push(entry.path());
            } else if kind.is_file() {
                if let Ok(meta) = entry.metadata() {
                    bytes += meta.len();
                    files += 1;
                }
            }
        }
    }
    (bytes, files)
}

fn head_of(text: &str, limit: usize) -> String {
    let mut out = String::with_capacity(limit.min(text.len()));
    let mut last_space = false;
    for ch in text.chars() {
        if out.chars().count() >= limit {
            out.push('…');
            break;
        }
        let space = ch.is_whitespace();
        if space && last_space {
            continue;
        }
        out.push(if space { ' ' } else { ch });
        last_space = space;
    }
    out.trim().to_string()
}

/// Read `name:` and `description:` from a SKILL.md frontmatter block.
///
/// Only the single-line form is handled, which is what every installed skill
/// uses. A multi-line YAML value falls back to the first prose paragraph, so
/// the page still shows something truthful instead of a truncated key.
fn frontmatter(text: &str) -> (String, String) {
    let mut name = String::new();
    let mut description = String::new();
    let lines: Vec<&str> = text.lines().take(80).collect();
    if lines.first().is_some_and(|line| line.trim() == "---") {
        for line in lines.iter().skip(1) {
            let trimmed = line.trim();
            if trimmed == "---" {
                break;
            }
            if let Some(value) = trimmed.strip_prefix("name:") {
                name = value.trim().trim_matches('"').to_string();
            } else if let Some(value) = trimmed.strip_prefix("description:") {
                description = value.trim().trim_matches('"').to_string();
            }
        }
    }
    if description.is_empty() {
        description = text
            .lines()
            .skip_while(|line| line.trim() != "---")
            .skip(1)
            .skip_while(|line| line.trim() != "---")
            .skip(1)
            .find(|line| {
                let line = line.trim();
                !line.is_empty() && !line.starts_with('#')
            })
            .map(|line| head_of(line, 240))
            .unwrap_or_default();
    }
    (name, description)
}

// ------------------------------------------------------------- redaction

fn secret_key(key: &str) -> bool {
    let key = key.to_ascii_lowercase();
    key.contains("key") || key.contains("token") || key.contains("secret") || key.contains("password")
}

fn looks_secret(value: &str) -> bool {
    const PREFIXES: [&str; 5] = ["sk-", "tvly-", "ghp_", "xoxb-", "AIza"];
    PREFIXES.iter().any(|prefix| value.starts_with(prefix))
}

/// Keep four leading and four trailing characters. Enough to recognise which
/// key it is, not enough to use it.
fn mask(value: &str) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= 10 {
        return "••••••".to_string();
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}…{tail}")
}

fn redact_url(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let pairs: Vec<String> = query
        .split('&')
        .map(|pair| match pair.split_once('=') {
            Some((key, value)) if !value.is_empty() && secret_key(key) => {
                format!("{key}={}", mask(value))
            }
            _ => pair.to_string(),
        })
        .collect();
    format!("{base}?{}", pairs.join("&"))
}

/// Walk a JSON value and mask anything credential-shaped.
///
/// Environment-variable *names* are kept: `apiKey: "DEEPSEEK_API_KEY"` is a
/// pointer to the secret, not the secret. A literal value is masked.
fn redact(value: &Value, key: &str) -> Value {
    match value {
        Value::String(text) => {
            if key.eq_ignore_ascii_case("apiKey") || key.eq_ignore_ascii_case("api_key") {
                let env_name = !text.is_empty()
                    && text.chars().all(|ch| ch.is_ascii_uppercase() || ch.is_ascii_digit() || ch == '_' || ch == '-');
                // Placeholders such as "noop" mean "no key", not "a key we are
                // hiding"; masking them would suggest a secret that is not there.
                let placeholder = matches!(text.as_str(), "noop" | "none" | "null" | "false" | "-" | "");
                return Value::String(if env_name || placeholder { text.clone() } else { mask(text) });
            }
            let mut text = redact_url(text);
            if looks_secret(&text) {
                text = mask(&text);
            }
            Value::String(text)
        }
        Value::Array(items) => Value::Array(items.iter().map(|item| redact(item, key)).collect()),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(child, item)| (child.clone(), redact(item, child)))
                .collect(),
        ),
        other => other.clone(),
    }
}

// ------------------------------------------------------------------ sizes

/// Directory sizes are the slowest thing on this page (tens of thousands of
/// stats). Cache them for a few seconds so switching tabs is instant, while a
/// later refresh still sees a grown directory.
pub struct SizeCache {
    last: std::sync::Mutex<Option<(Instant, Vec<Value>)>>,
}

impl SizeCache {
    #[must_use]
    pub fn new() -> Self {
        Self {
            last: std::sync::Mutex::new(None),
        }
    }

    fn get(&self, roots: &Roots, fresh: bool) -> Vec<Value> {
        if !fresh {
            if let Ok(guard) = self.last.lock() {
                if let Some((at, rows)) = guard.as_ref() {
                    if at.elapsed().as_secs() < 20 {
                        return rows.clone();
                    }
                }
            }
        }
        let rows = storage(roots);
        if let Ok(mut guard) = self.last.lock() {
            *guard = Some((Instant::now(), rows.clone()));
        }
        rows
    }
}

impl Default for SizeCache {
    fn default() -> Self {
        Self::new()
    }
}

/// Directories worth measuring, with a one-line note on what they are.
const STORAGE_DIRS: [(&str, &str); 17] = [
    ("sessions", "当前会话库"),
    ("session-artifacts", "会话产物：图片、脚本、临时文件"),
    ("sessions-backup", "会话备份"),
    ("sessions-archive", "归档会话"),
    ("kernel-venv", "IPython kernel 的 Python 环境"),
    ("npm", "npm 包缓存"),
    ("stop-hook", "stop hook 逐会话记录"),
    ("prime-context", "上下文缓存"),
    ("git", "克隆的仓库（技能来源）"),
    ("gsd-core", "gsd 扩展本体"),
    ("logs", "运行日志"),
    ("daemon-workers", "后台 worker 日志"),
    ("models", "模型目录缓存"),
    ("harness", "全局记忆"),
    ("skills", "内置技能"),
    ("themes", "主题"),
    ("mcp", "MCP 服务目录"),
];

fn storage(roots: &Roots) -> Vec<Value> {
    let mut rows = Vec::new();
    for (name, note) in STORAGE_DIRS {
        let path = roots.agent.join(name);
        if !path.is_dir() {
            continue;
        }
        let (bytes, files) = dir_stats(&path);
        rows.push(json!({
            "name": name, "kind": "dir", "bytes": bytes, "files": files, "note": note,
        }));
    }
    // Loose files at the agent root: config, telemetry, and the occasional
    // session file that never made it into sessions/.
    if let Ok(entries) = fs::read_dir(&roots.agent) {
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else { continue };
            if !kind.is_file() {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if meta.len() < 1024 {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            // Order matters: telemetry.jsonl also ends in .jsonl, and calling
            // it a stray session file would be wrong.
            let note = if name.starts_with("telemetry") {
                "遥测事件"
            } else if name.ends_with(".jsonl") {
                "散落在根目录的会话文件（不在 sessions/ 里）"
            } else if name.contains(".bak") {
                "配置备份"
            } else {
                "配置文件"
            };
            rows.push(json!({
                "name": name, "kind": "file", "bytes": meta.len(), "files": 1, "note": note,
            }));
        }
    }
    rows.sort_by(|a, b| b["bytes"].as_u64().cmp(&a["bytes"].as_u64()));
    rows
}

// ------------------------------------------------------------ settings

fn settings_of(roots: &Roots) -> Value {
    read_json(&roots.agent.join("settings.json")).unwrap_or(Value::Null)
}

/// Defaults the rest of the payload leans on, lifted out of settings.
fn defaults_of(settings: &Value) -> Value {
    let pick = |key: &str| settings.get(key).cloned().unwrap_or(Value::Null);
    json!({
        "provider": pick("defaultProvider"),
        "model": pick("defaultModel"),
        "subagentModel": pick("subagentDefaultModel"),
        "auxiliaryModel": pick("auxiliaryModel"),
        "imageModel": pick("imageModel"),
        "backupModel": pick("providerBackupModel"),
        "thinking": pick("defaultThinkingLevel"),
        "serviceTier": pick("defaultServiceTier"),
        "theme": pick("theme"),
        "rlmMaxDepth": pick("rlmMaxDepth"),
        "compaction": pick("compaction"),
        "autoRefine": pick("autoRefine"),
        "telemetry": pick("telemetry"),
        "retry": pick("retry"),
        "sessionDir": pick("sessionDir"),
        "recentModels": pick("recentModels"),
        "enabledModels": pick("enabledModels"),
        "allowedModels": pick("allowedModels"),
    })
}

/// Which settings keys are set explicitly versus left at their default. The
/// file contains every key with `null` for "unset", so the UI can show both.
fn settings_keys(settings: &Value) -> Value {
    let mut rows = Vec::new();
    if let Some(map) = settings.as_object() {
        for (key, value) in map {
            rows.push(json!({
                "key": key,
                "value": redact(value, key),
                "type": type_name(value),
                "isNull": value.is_null(),
            }));
        }
    }
    Value::Array(rows)
}

fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "未设置",
        Value::Bool(_) => "开关",
        Value::Number(_) => "数字",
        Value::String(_) => "文本",
        Value::Array(_) => "列表",
        Value::Object(_) => "对象",
    }
}

// ------------------------------------------------------------- providers

fn providers_of(roots: &Roots) -> (Value, Value) {
    let Some(models) = read_json(&roots.agent.join("models.json")) else {
        return (Value::Array(Vec::new()), json!({"count": 0, "modelCount": 0}));
    };
    let empty = Map::new();
    let map = models.get("providers").and_then(Value::as_object).unwrap_or(&empty);
    let mut rows = Vec::new();
    let mut model_count = 0_u64;
    for (name, provider) in map {
        let models: Vec<Value> = provider
            .get("models")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|model| {
                        json!({
                            "id": model.get("id").cloned().unwrap_or(Value::Null),
                            "name": model.get("name").cloned().unwrap_or(Value::Null),
                            "contextWindow": model.get("contextWindow").cloned().unwrap_or(Value::Null),
                            "reasoning": model.get("reasoning").cloned().unwrap_or(Value::Bool(false)),
                            "input": model.get("input").cloned().unwrap_or(Value::Null),
                            "thinking": model.get("thinkingLevelMap").and_then(Value::as_object).map(|m| m.keys().cloned().collect::<Vec<_>>()).unwrap_or_default(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        model_count += models.len() as u64;
        rows.push(json!({
            "name": name,
            "baseUrl": redact(provider.get("baseUrl").unwrap_or(&Value::Null), "baseUrl"),
            "api": provider.get("api").cloned().unwrap_or(Value::Null),
            "apiKey": redact(provider.get("apiKey").unwrap_or(&Value::Null), "apiKey"),
            "authHeader": provider.get("authHeader").cloned().unwrap_or(Value::Null),
            "models": models,
        }));
    }
    (Value::Array(rows), json!({"count": map.len(), "modelCount": model_count}))
}

// ----------------------------------------------------------------- mcp

fn mcp_of(roots: &Roots, settings: &Value) -> Value {
    let mut rows = Vec::new();
    if let Some(map) = settings.get("mcpServers").and_then(Value::as_object) {
        for (name, server) in map {
            let env_keys: Vec<String> = server
                .get("env")
                .and_then(Value::as_object)
                .map(|env| env.keys().cloned().collect())
                .unwrap_or_default();
            rows.push(json!({
                "name": name,
                "type": server.get("type").and_then(Value::as_str).unwrap_or("stdio"),
                "url": server.get("url").map(|url| redact(url, "url")).unwrap_or(Value::Null),
                "command": server.get("command").cloned().unwrap_or(Value::Null),
                "args": server.get("args").cloned().unwrap_or(Value::Null),
                "envKeys": env_keys,
            }));
        }
    }
    let catalog = read_json(&roots.agent.join("mcp").join("mcp-service-catalog.v2.json")).unwrap_or(Value::Null);
    json!({
        "servers": Value::Array(rows),
        "catalog": {
            "fetchedAt": catalog.get("fetchedAt").cloned().unwrap_or(Value::Null),
            "counts": catalog
                .get("payload")
                .and_then(|payload| payload.get("counts"))
                .cloned()
                .unwrap_or(Value::Null),
        },
    })
}

// ------------------------------------------------------------- harness

fn harness_state(roots: &Roots) -> Option<Value> {
    read_json(&roots.agent.join("harness").join("harness_state.json"))
}

fn entry_rows(kind: &str, items: &Value) -> Vec<Value> {
    let Some(map) = items.as_object() else {
        return Vec::new();
    };
    let mut rows: Vec<(String, Value)> = map
        .iter()
        .map(|(id, entry)| {
            let content = entry.get("content").and_then(Value::as_str).unwrap_or("");
            let title = entry.get("title").and_then(Value::as_str).unwrap_or("");
            let updated = entry.get("updated_at").and_then(Value::as_str).unwrap_or("");
            let row = json!({
                "kind": kind,
                "id": entry.get("id").and_then(Value::as_str).unwrap_or(id),
                "title": title,
                "path": entry.get("path").and_then(Value::as_str).unwrap_or(""),
                "scope": entry.get("scope").and_then(Value::as_str).unwrap_or(""),
                "source": entry.get("source").and_then(Value::as_str).unwrap_or(""),
                "version": entry.get("version").and_then(Value::as_u64).unwrap_or(0),
                "chars": content.chars().count(),
                "updated": updated,
                "head": head_of(content, 180),
            });
            (format!("{updated}\u{0}{title}"), row)
        })
        .collect();
    rows.sort_by(|a, b| b.0.cmp(&a.0));
    rows.into_iter().map(|(_, row)| row).collect()
}

/// Harness entries by kind, without their bodies.
pub fn entries(roots: &Roots) -> Value {
    let state = harness_state(roots).unwrap_or(Value::Null);
    let mut kinds = Map::new();
    for kind in ["prompt", "memory", "skill", "subagent", "factory"] {
        let items = state.get("entries").and_then(|e| e.get(kind)).cloned().unwrap_or(Value::Null);
        let rows = entry_rows(kind, &items);
        kinds.insert(kind.to_string(), Value::Array(rows));
    }
    let refinements: Vec<Value> = state
        .get("refinements")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .rev()
                .take(200)
                .map(|event| {
                    json!({
                        "id": event.get("id").cloned().unwrap_or(Value::Null),
                        "trigger": event.get("trigger").cloned().unwrap_or(Value::Null),
                        "changes": event.get("changes").cloned().unwrap_or(Value::Null),
                        "evidence": head_of(event.get("evidence").and_then(Value::as_str).unwrap_or(""), 300),
                        "outcome": head_of(event.get("outcome").and_then(Value::as_str).unwrap_or(""), 300),
                        "created": event.get("created_at").cloned().unwrap_or(Value::Null),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    json!({
        "path": roots.agent.join("harness").join("harness_state.json").display().to_string(),
        "schema": state.get("schema").cloned().unwrap_or(Value::Null),
        "kinds": Value::Object(kinds),
        "refinements": Value::Array(refinements),
    })
}

/// Full body of one harness entry, or one refinement event.
pub fn entry_text(roots: &Roots, kind: &str, id: &str) -> Value {
    let state = harness_state(roots).unwrap_or(Value::Null);
    if kind == "refinement" {
        let event = state
            .get("refinements")
            .and_then(Value::as_array)
            .and_then(|items| {
                items
                    .iter()
                    .find(|event| event.get("id").and_then(Value::as_str) == Some(id))
            });
        return json!({
            "kind": kind,
            "id": id,
            "title": "refinement",
            "text": event.map_or_else(|| "找不到这条 refine 事件".to_string(), |event| {
                let changes = event.get("changes").and_then(Value::as_array).map(|items| {
                    items.iter().map(|item| format!("• {}", item.as_str().unwrap_or(""))).collect::<Vec<_>>().join("\n")
                }).unwrap_or_default();
                format!(
                    "{}\n\n## 触发\n{}\n\n## 改动\n{}\n\n## 证据\n{}\n\n## 结果\n{}\n\n{}",
                    event.get("created_at").and_then(Value::as_str).unwrap_or(""),
                    event.get("trigger").and_then(Value::as_str).unwrap_or(""),
                    if changes.is_empty() { "（无）".to_string() } else { changes },
                    event.get("evidence").and_then(Value::as_str).unwrap_or("（无）"),
                    event.get("outcome").and_then(Value::as_str).unwrap_or("（无）"),
                    event.get("id").and_then(Value::as_str).unwrap_or(""),
                )
            }),
        });
    }
    let entry = state
        .get("entries")
        .and_then(|entries| entries.get(kind))
        .and_then(|items| items.as_object())
        .and_then(|map| map.get(id).or_else(|| map.values().find(|entry| entry.get("id").and_then(Value::as_str) == Some(id))));
    let Some(entry) = entry else {
        return json!({"kind": kind, "id": id, "title": "", "text": "找不到这条记录"});
    };
    let meta = json!({
        "path": entry.get("path").cloned().unwrap_or(Value::Null),
        "scope": entry.get("scope").cloned().unwrap_or(Value::Null),
        "source": entry.get("source").cloned().unwrap_or(Value::Null),
        "version": entry.get("version").cloned().unwrap_or(Value::Null),
        "created": entry.get("created_at").cloned().unwrap_or(Value::Null),
        "updated": entry.get("updated_at").cloned().unwrap_or(Value::Null),
        "reference": entry.get("reference").cloned().unwrap_or(Value::Null),
        "arguments": entry.get("arguments").cloned().unwrap_or(Value::Null),
    });
    json!({
        "kind": kind,
        "id": entry.get("id").cloned().unwrap_or(Value::Null),
        "title": entry.get("title").cloned().unwrap_or(Value::Null),
        "text": entry.get("content").and_then(Value::as_str).unwrap_or(""),
        "meta": meta,
    })
}

// -------------------------------------------------------------- skills

fn skill_rows(dir: &Path, source: &str) -> Vec<Value> {
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut rows = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let manifest = path.join("SKILL.md");
        let Ok(text) = fs::read_to_string(&manifest) else {
            continue;
        };
        let (name, description) = frontmatter(&text);
        let dir_name = path.file_name().map_or_else(String::new, |n| n.to_string_lossy().to_string());
        let (bytes, files) = dir_stats(&path);
        let has_python = fs::read_dir(&path)
            .map(|items| items.flatten().any(|item| item.path().extension().is_some_and(|ext| ext == "py")))
            .unwrap_or(false);
        rows.push(json!({
            "source": source,
            "dir": dir_name,
            "name": if name.is_empty() { dir_name.clone() } else { name },
            "description": description,
            "bytes": bytes,
            "files": files,
            "hasPython": has_python,
            "updated": modified_secs(&manifest),
        }));
    }
    rows.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    rows
}

pub fn skills(roots: &Roots) -> Value {
    json!({
        "prime": Value::Array(skill_rows(&roots.agent.join("skills"), "prime")),
        "user": Value::Array(skill_rows(&roots.user_skills, "user")),
        "primePath": roots.agent.join("skills").display().to_string(),
        "userPath": roots.user_skills.display().to_string(),
    })
}

/// One skill's SKILL.md plus its file listing.
///
/// `name` is matched against directory names, never used to build a path
/// blindly, so a crafted name cannot escape the skills directory.
pub fn skill_text(roots: &Roots, source: &str, name: &str) -> Result<Value, String> {
    let base = match source {
        "prime" => roots.agent.join("skills"),
        "user" => roots.user_skills.clone(),
        _ => return Err("unknown skill source".to_string()),
    };
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err("bad skill name".to_string());
    }
    let dir = base.join(name);
    if !dir.is_dir() {
        return Err(format!("no skill named {name}"));
    }
    let text = fs::read_to_string(dir.join("SKILL.md")).map_err(|error| format!("SKILL.md: {error}"))?;
    let mut files = Vec::new();
    let mut stack = vec![(dir.clone(), String::new())];
    while let Some((current, prefix)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&current) else { continue };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            if name == "__pycache__" || name == ".git" {
                continue;
            }
            let rel = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                stack.push((path, rel));
            } else if kind.is_file() {
                let bytes = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
                files.push(json!({"name": rel, "bytes": bytes}));
            }
        }
    }
    files.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    Ok(json!({
        "source": source,
        "name": name,
        "path": dir.display().to_string(),
        "text": text,
        "files": files,
    }))
}

// ------------------------------------------------------------- config io

fn config_path(roots: &Roots, name: &str) -> Result<PathBuf, String> {
    if !EDITABLE.contains(&name) {
        return Err(format!("{name} is not editable here"));
    }
    Ok(roots.agent.join(name))
}

pub fn file_text(roots: &Roots, name: &str) -> Result<Value, String> {
    let path = config_path(roots, name)?;
    let text = fs::read_to_string(&path).map_err(|error| format!("{name}: {error}"))?;
    let valid = serde_json::from_str::<Value>(&text).is_ok();
    Ok(json!({
        "name": name,
        "path": path.display().to_string(),
        "text": text,
        "bytes": text.len(),
        "modified": modified_secs(&path),
        "valid": valid,
        "hasSecrets": text.contains("tvly-") || text.contains("sk-") || text.contains("ghp_"),
    }))
}

/// Back up, validate, and replace a config file.
///
/// Validation is the only guardrail that matters: a syntactically broken
/// settings.json makes the agent refuse to start, so a bad save is worse than
/// no save. The backup is written first and never removed, so the previous
/// contents are always one rename away.
pub fn save_file(roots: &Roots, name: &str, text: &str) -> Result<Value, String> {
    let path = config_path(roots, name)?;
    let parsed: Value = serde_json::from_str(text)
        .map_err(|error| format!("JSON 解析失败：第 {} 行第 {} 列：{error}", error.line(), error.column()))?;
    if !parsed.is_object() {
        return Err("顶层必须是一个 JSON 对象".to_string());
    }
    if name == "models.json" && !parsed.get("providers").is_some_and(Value::is_object) {
        return Err("models.json 必须包含 providers 对象".to_string());
    }
    let backup = roots.agent.join(format!("{name}.bak-{}-ui", stamp()));
    if path.exists() {
        fs::copy(&path, &backup).map_err(|error| format!("写备份失败：{error}"))?;
    }
    let temp = roots.agent.join(format!("{name}.tmp-ui"));
    fs::write(&temp, text).map_err(|error| format!("写临时文件失败：{error}"))?;
    fs::rename(&temp, &path).map_err(|error| {
        let _ = fs::remove_file(&temp);
        format!("替换失败：{error}")
    })?;
    Ok(json!({
        "ok": true,
        "name": name,
        "bytes": text.len(),
        "backup": backup.display().to_string(),
        "note": "已写入磁盘；正在运行的会话不会重读，新会话或重启后生效。",
    }))
}

/// `YYYYMMDD-HHMMSS` in UTC, without pulling in a date crate.
fn stamp() -> String {
    let secs = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |delta| delta.as_secs());
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}{month:02}{day:02}-{:02}{:02}{:02}",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Howard Hinnant's days-to-civil conversion (proleptic Gregorian).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

// ------------------------------------------------------------ logs/health

fn log_rows(roots: &Roots) -> Vec<Value> {
    let dir = roots.agent.join("logs");
    let Ok(entries) = fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut rows: Vec<Value> = entries
        .flatten()
        .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_file()))
        .map(|entry| {
            let path = entry.path();
            let bytes = entry.metadata().map(|meta| meta.len()).unwrap_or(0);
            json!({
                "name": entry.file_name().to_string_lossy().to_string(),
                "bytes": bytes,
                "modified": modified_secs(&path),
            })
        })
        .collect();
    rows.sort_by(|a, b| b["bytes"].as_u64().cmp(&a["bytes"].as_u64()));
    rows.truncate(60);
    rows
}

pub fn log_tail(roots: &Roots, name: &str, lines: usize) -> Result<Value, String> {
    if name.contains('/') || name.contains('\\') || name.contains("..") {
        return Err("bad log name".to_string());
    }
    let path = roots.agent.join("logs").join(name);
    let text = fs::read_to_string(&path).map_err(|error| format!("{name}: {error}"))?;
    let all: Vec<&str> = text.lines().collect();
    let take = lines.clamp(20, 2000);
    let start = all.len().saturating_sub(take);
    Ok(json!({
        "name": name,
        "lines": all.len(),
        "from": start,
        "text": all[start..].join("\n"),
    }))
}

/// Summarise telemetry: what ran, how it ended, how long it took.
fn telemetry(roots: &Roots) -> Value {
    let mut events = Map::new();
    let mut runs = Vec::new();
    for (path, keep_runs) in [
        (roots.agent.join("telemetry.jsonl"), true),
        (roots.agent.join("telemetry.jsonl.1"), false),
    ] {
        let Ok(text) = fs::read_to_string(&path) else { continue };
        for line in text.lines() {
            let Ok(event) = serde_json::from_str::<Value>(line) else { continue };
            let Some(name) = event.get("name").and_then(Value::as_str) else { continue };
            let next = events.get(name).and_then(Value::as_u64).unwrap_or(0) + 1;
            events.insert(name.to_string(), json!(next));
            if keep_runs && name == "agent run completed" {
                runs.push(event.get("properties").cloned().unwrap_or(Value::Null));
            }
        }
    }

    let total = runs.len();
    let errors = runs.iter().filter(|run| run.get("outcome").and_then(Value::as_str) == Some("error")).count();
    let mut durations: Vec<u64> = runs
        .iter()
        .filter_map(|run| run.get("duration_ms").and_then(Value::as_u64))
        .collect();
    durations.sort_unstable();
    let p50 = durations.get(durations.len() / 2).copied().unwrap_or(0);
    let max = durations.last().copied().unwrap_or(0);
    let calls: u64 = runs.iter().map(|run| run.get("model_call_count").and_then(Value::as_u64).unwrap_or(0)).sum();
    let retries: u64 = runs.iter().map(|run| run.get("retry_count").and_then(Value::as_u64).unwrap_or(0)).sum();
    json!({
        "events": Value::Object(events),
        "runs": {
            "total": total,
            "errors": errors,
            "p50ms": p50,
            "maxms": max,
            "modelCalls": calls,
            "retries": retries,
        },
    })
}

/// Recent client errors, newest last, kept short.
fn client_errors(roots: &Roots) -> Vec<String> {
    let path = roots.agent.join("logs").join("client-errors.log");
    let Ok(text) = fs::read_to_string(&path) else {
        return Vec::new();
    };
    let all: Vec<&str> = text.lines().collect();
    all.iter()
        .rev()
        .take(12)
        .rev()
        .map(|line| head_of(line, 300))
        .collect()
}

/// Installed extras: packages, extensions, themes, cloned repos.
fn installs(roots: &Roots, settings: &Value) -> Value {
    let themes = fs::read_dir(roots.agent.join("themes"))
        .map(|items| {
            items
                .flatten()
                .filter_map(|entry| {
                    let name = entry.file_name().to_string_lossy().to_string();
                    name.strip_suffix(".json").map(ToString::to_string)
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut repos = Vec::new();
    let mut stack = vec![(roots.agent.join("git"), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            let name = entry.file_name().to_string_lossy().to_string();
            if name == ".git" {
                continue;
            }
            let rel = if prefix.is_empty() { name.clone() } else { format!("{prefix}/{name}") };
            if entry.path().join(".git").exists() {
                repos.push(rel);
            } else {
                stack.push((entry.path(), rel));
            }
        }
    }
    repos.sort();
    json!({
        "packages": settings.get("packages").cloned().unwrap_or(Value::Null),
        "extensions": settings.get("extensions").cloned().unwrap_or(Value::Null),
        "themes": themes,
        "repos": repos,
    })
}

// ------------------------------------------------------------- assembly

/// Everything the global page needs except file bodies and entry bodies.
///
/// `fresh` bypasses the directory-size cache for an explicit refresh.
#[must_use]
pub fn overview(roots: &Roots, sizes: &SizeCache, fresh: bool) -> Value {
    let settings = settings_of(roots);
    let (providers, provider_counts) = providers_of(roots);
    let harness = entries(roots);
    let skills = skills(roots);
    let storage_rows = sizes.get(roots, fresh);
    let skill_count = ["prime", "user"]
        .iter()
        .map(|key| skills.get(*key).and_then(Value::as_array).map_or(0, Vec::len))
        .sum::<usize>();
    let counts = json!({
        "sessions": fs::read_dir(roots.agent.join("sessions")).map(|items| items.flatten().filter(|entry| entry.path().extension().is_some_and(|ext| ext == "jsonl")).count()).unwrap_or(0),
        "skills": skill_count,
        "prompts": harness.get("kinds").and_then(|k| k.get("prompt")).and_then(Value::as_array).map_or(0, Vec::len),
        "memories": harness.get("kinds").and_then(|k| k.get("memory")).and_then(Value::as_array).map_or(0, Vec::len),
        "refinements": harness.get("refinements").and_then(Value::as_array).map_or(0, Vec::len),
        "providers": provider_counts.get("count").cloned().unwrap_or(Value::Null),
        "models": provider_counts.get("modelCount").cloned().unwrap_or(Value::Null),
        "mcp": settings.get("mcpServers").and_then(Value::as_object).map_or(0, Map::len),
    });

    let mcp = mcp_of(roots, &settings);

    json!({
        "scannedAt": SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map_or(0, |delta| delta.as_millis() as u64),
        "roots": {
            "agent": roots.agent.display().to_string(),
            "userSkills": roots.user_skills.display().to_string(),
            "sessions": roots.agent.join("sessions").display().to_string(),
        },
        "settings": redact(&settings, ""),
        "defaults": defaults_of(&settings),
        "settingKeys": settings_keys(&settings),
        "files": [
            {"name": "settings.json", "path": roots.agent.join("settings.json").display().to_string(), "bytes": roots.agent.join("settings.json").metadata().map(|m| m.len()).unwrap_or(0), "modified": modified_secs(&roots.agent.join("settings.json"))},
            {"name": "models.json", "path": roots.agent.join("models.json").display().to_string(), "bytes": roots.agent.join("models.json").metadata().map(|m| m.len()).unwrap_or(0), "modified": modified_secs(&roots.agent.join("models.json"))},
        ],
        "providers": providers,
        "mcp": mcp,
        "harness": harness,
        "skills": skills,
        "storage": storage_rows,
        "logs": log_rows(roots),
        "telemetry": telemetry(roots),
        "clientErrors": client_errors(roots),
        "installs": installs(roots, &settings),
        "counts": counts,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn masks_secrets_but_keeps_env_names() {
        assert_eq!(redact(&json!("DEEPSEEK_API_KEY"), "apiKey"), json!("DEEPSEEK_API_KEY"));
        assert_eq!(redact(&json!("sk-abcdefghijklmnop"), "apiKey"), json!("sk-a…mnop"));
        assert_eq!(redact(&json!("noop"), "apiKey"), json!("noop"));
    }

    #[test]
    fn masks_secret_query_parameters() {
        let url = redact(&json!("https://mcp.example.com/mcp/?apiKey=abcdefghijkl"), "url");
        assert_eq!(url, json!("https://mcp.example.com/mcp/?apiKey=abcd…ijkl"));
        let plain = redact(&json!("https://mcp.example.com/mcp"), "url");
        assert_eq!(plain, json!("https://mcp.example.com/mcp"));
    }

    #[test]
    fn civil_dates_round_trip_known_days() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_000), (2022, 1, 8));
    }

    #[test]
    fn frontmatter_reads_name_and_description() {
        let (name, description) = frontmatter("---\nname: tdd\ndescription: Test first.\n---\n\n# T\n");
        assert_eq!(name, "tdd");
        assert_eq!(description, "Test first.");
    }

    #[test]
    fn head_of_collapses_whitespace_and_marks_truncation() {
        assert_eq!(head_of("a  b\n c", 20), "a b c");
        assert!(head_of("abcdefghij", 4).ends_with('…'));
    }
}
