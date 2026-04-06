//! Session listing implementation.
//!
//! Scans `~/.claude/projects/<sanitized-cwd>/` for `.jsonl` session files and
//! extracts metadata from stat + head/tail reads without full JSONL parsing.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;

use regex::Regex;
use serde_json::Value;
use std::sync::OnceLock;
use uuid::Uuid;

use crate::types::sessions::{SdkSessionInfo, SessionMessage, SessionMessageType};

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

const LITE_READ_BUF_SIZE: usize = 65536;
const MAX_SANITIZED_LENGTH: usize = 200;

// ---------------------------------------------------------------------------
// UUID validation
// ---------------------------------------------------------------------------

fn validate_uuid(s: &str) -> Option<String> {
    Uuid::parse_str(s).ok().map(|u| u.hyphenated().to_string())
}

// ---------------------------------------------------------------------------
// Path sanitization
// ---------------------------------------------------------------------------

/// 32-bit hash to base36, matching the CLI's JavaScript `Bun.hash` fallback.
fn simple_hash(s: &str) -> String {
    let mut h: i32 = 0;
    for ch in s.chars() {
        h = h
            .wrapping_shl(5)
            .wrapping_sub(h)
            .wrapping_add(ch as i32);
    }
    let n = h.unsigned_abs() as u64;
    if n == 0 {
        return "0".to_string();
    }
    const DIGITS: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    let mut out = Vec::new();
    let mut n = n;
    while n > 0 {
        out.push(DIGITS[(n % 36) as usize]);
        n /= 36;
    }
    out.reverse();
    String::from_utf8(out).unwrap()
}

fn sanitize_path(name: &str) -> String {
    // Replace all non-alphanumeric characters with hyphens
    let sanitized: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c.is_alphabetic() { c } else { '-' })
        .collect();
    if sanitized.len() <= MAX_SANITIZED_LENGTH {
        sanitized
    } else {
        let h = simple_hash(name);
        format!("{}-{}", &sanitized[..MAX_SANITIZED_LENGTH], h)
    }
}

// ---------------------------------------------------------------------------
// Config directories
// ---------------------------------------------------------------------------

pub(crate) fn get_claude_config_home_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("CLAUDE_CONFIG_DIR") {
        PathBuf::from(dir)
    } else {
        let home = std::env::var("HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from("."));
        home.join(".claude")
    }
}

pub(crate) fn get_projects_dir() -> PathBuf {
    get_claude_config_home_dir().join("projects")
}

fn get_project_dir(project_path: &str) -> PathBuf {
    get_projects_dir().join(sanitize_path(project_path))
}

pub(crate) fn canonicalize_path(d: &str) -> String {
    std::fs::canonicalize(d)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| d.to_owned())
}

/// Finds the project directory for a given path, with prefix-fallback for
/// long paths where CLI (Bun.hash) and SDK (simpleHash) produce different suffixes.
pub(crate) fn find_project_dir(project_path: &str) -> Option<PathBuf> {
    let exact = get_project_dir(project_path);
    if exact.is_dir() {
        return Some(exact);
    }

    let sanitized = sanitize_path(project_path);
    if sanitized.len() <= MAX_SANITIZED_LENGTH {
        return None;
    }

    let prefix = &sanitized[..MAX_SANITIZED_LENGTH];
    let projects_dir = get_projects_dir();
    let entries = fs::read_dir(&projects_dir).ok()?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if entry.path().is_dir() && name_str.starts_with(&format!("{}-", prefix)) {
            return Some(entry.path());
        }
    }
    None
}

// ---------------------------------------------------------------------------
// JSON string field extraction — no full parse, works on truncated lines
// ---------------------------------------------------------------------------

fn unescape_json_string(raw: &str) -> String {
    if !raw.contains('\\') {
        return raw.to_owned();
    }
    // Re-use serde_json to unescape
    let quoted = format!("\"{}\"", raw);
    serde_json::from_str::<String>(&quoted).unwrap_or_else(|_| raw.to_owned())
}

fn extract_json_string_field<'a>(text: &'a str, key: &str) -> Option<String> {
    let patterns = [
        format!("\"{}\":\"", key),
        format!("\"{}\": \"", key),
    ];
    for pattern in &patterns {
        if let Some(idx) = text.find(pattern.as_str()) {
            let value_start = idx + pattern.len();
            let slice = &text[value_start..];
            let mut i = 0;
            let chars: Vec<char> = slice.chars().collect();
            while i < chars.len() {
                if chars[i] == '\\' {
                    i += 2;
                    continue;
                }
                if chars[i] == '"' {
                    let raw: String = chars[..i].iter().collect();
                    return Some(unescape_json_string(&raw));
                }
                i += 1;
            }
        }
    }
    None
}

pub(crate) fn extract_last_json_string_field_pub(text: &str, key: &str) -> Option<String> {
    extract_last_json_string_field(text, key)
}

pub(crate) fn extract_first_prompt_from_head_pub(head: &str) -> String {
    extract_first_prompt_from_head(head)
}

fn extract_last_json_string_field(text: &str, key: &str) -> Option<String> {
    let patterns = [
        format!("\"{}\":\"", key),
        format!("\"{}\": \"", key),
    ];
    let mut last_value: Option<String> = None;
    for pattern in &patterns {
        let mut search_from = 0;
        loop {
            match text[search_from..].find(pattern.as_str()) {
                None => break,
                Some(rel_idx) => {
                    let idx = search_from + rel_idx;
                    let value_start = idx + pattern.len();
                    let slice = &text[value_start..];
                    let chars: Vec<char> = slice.chars().collect();
                    let mut i = 0;
                    while i < chars.len() {
                        if chars[i] == '\\' {
                            i += 2;
                            continue;
                        }
                        if chars[i] == '"' {
                            let raw: String = chars[..i].iter().collect();
                            last_value = Some(unescape_json_string(&raw));
                            break;
                        }
                        i += 1;
                    }
                    // Advance by the byte length of the first char at value_start
                    // to avoid splitting a multibyte UTF-8 character.
                    let step = text[value_start..]
                        .chars()
                        .next()
                        .map_or(1, |c| c.len_utf8());
                    search_from = value_start + step;
                    if search_from >= text.len() {
                        break;
                    }
                }
            }
        }
    }
    last_value
}

// ---------------------------------------------------------------------------
// First prompt extraction from head chunk
// ---------------------------------------------------------------------------

static SKIP_PATTERN: OnceLock<Regex> = OnceLock::new();
static COMMAND_NAME_RE: OnceLock<Regex> = OnceLock::new();

fn skip_pattern() -> &'static Regex {
    SKIP_PATTERN.get_or_init(|| {
        Regex::new(
            r"(?x)^(?:
                <local-command-stdout>
                |<session-start-hook>
                |<tick>
                |<goal>
                |\[Request\ interrupted\ by\ user[^\]]*\]
                |\s*<ide_opened_file>[\s\S]*</ide_opened_file>\s*$
                |\s*<ide_selection>[\s\S]*</ide_selection>\s*$
            )"
        )
        .unwrap()
    })
}

fn command_name_re() -> &'static Regex {
    COMMAND_NAME_RE.get_or_init(|| {
        Regex::new(r"<command-name>(.*?)</command-name>").unwrap()
    })
}

fn extract_first_prompt_from_head(head: &str) -> String {
    let mut command_fallback = String::new();

    for line in head.lines() {
        if !line.contains("\"type\":\"user\"") && !line.contains("\"type\": \"user\"") {
            continue;
        }
        if line.contains("\"tool_result\"") {
            continue;
        }
        if line.contains("\"isMeta\":true") || line.contains("\"isMeta\": true") {
            continue;
        }
        if line.contains("\"isCompactSummary\":true") || line.contains("\"isCompactSummary\": true") {
            continue;
        }

        let entry: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };

        if entry.get("type").and_then(|t| t.as_str()) != Some("user") {
            continue;
        }

        let message = match entry.get("message") {
            Some(Value::Object(m)) => m,
            _ => continue,
        };

        let content = match message.get("content") {
            Some(c) => c,
            None => continue,
        };

        let mut texts = Vec::new();
        match content {
            Value::String(s) => texts.push(s.clone()),
            Value::Array(arr) => {
                for block in arr {
                    if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                        if let Some(Value::String(t)) = block.get("text") {
                            texts.push(t.clone());
                        }
                    }
                }
            }
            _ => {}
        }

        for raw in texts {
            let result = raw.replace('\n', " ");
            let result = result.trim().to_string();
            if result.is_empty() {
                continue;
            }

            if let Some(caps) = command_name_re().captures(&result) {
                if command_fallback.is_empty() {
                    command_fallback = caps.get(1).map(|m| m.as_str().to_owned()).unwrap_or_default();
                }
                continue;
            }

            if skip_pattern().is_match(&result) {
                continue;
            }

            let result = if result.len() > 200 {
                let truncated = &result[..result.char_indices().nth(200).map(|(i, _)| i).unwrap_or(result.len())];
                format!("{}\u{2026}", truncated.trim_end())
            } else {
                result
            };
            return result;
        }
    }

    command_fallback
}

// ---------------------------------------------------------------------------
// File I/O — read head and tail of a file
// ---------------------------------------------------------------------------

struct LiteSessionFile {
    mtime: i64,
    size: u64,
    head: String,
    tail: String,
}

fn read_session_lite(file_path: &Path) -> Option<LiteSessionFile> {
    let mut f = fs::File::open(file_path).ok()?;
    let metadata = f.metadata().ok()?;
    let size = metadata.len();
    if size == 0 {
        return None;
    }

    let mtime = metadata
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);

    let mut head_bytes = vec![0u8; LITE_READ_BUF_SIZE.min(size as usize)];
    f.read_exact(&mut head_bytes[..LITE_READ_BUF_SIZE.min(size as usize)]).ok()?;
    let head = String::from_utf8_lossy(&head_bytes).into_owned();

    let tail = if size <= LITE_READ_BUF_SIZE as u64 {
        head.clone()
    } else {
        let tail_offset = size - LITE_READ_BUF_SIZE as u64;
        f.seek(SeekFrom::Start(tail_offset)).ok()?;
        let mut tail_bytes = vec![0u8; LITE_READ_BUF_SIZE];
        let n = f.read(&mut tail_bytes).ok()?;
        String::from_utf8_lossy(&tail_bytes[..n]).into_owned()
    };

    Some(LiteSessionFile { mtime, size, head, tail })
}

// ---------------------------------------------------------------------------
// Git worktree detection
// ---------------------------------------------------------------------------

pub(crate) fn get_worktree_paths(cwd: &str) -> Vec<String> {
    let output = Command::new("git")
        .args(["worktree", "list", "--porcelain"])
        .current_dir(cwd)
        .output();

    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return vec![],
    };

    let stdout = match String::from_utf8(output.stdout) {
        Ok(s) => s,
        Err(_) => return vec![],
    };

    stdout
        .lines()
        .filter(|l| l.starts_with("worktree "))
        .map(|l| l["worktree ".len()..].to_owned())
        .collect()
}

// ---------------------------------------------------------------------------
// Field extraction — shared by list_sessions and get_session_info
// ---------------------------------------------------------------------------

fn parse_session_info_from_lite(
    session_id: &str,
    lite: &LiteSessionFile,
    project_path: Option<&str>,
) -> Option<SdkSessionInfo> {
    let (head, tail, mtime, size) = (&lite.head, &lite.tail, lite.mtime, lite.size);

    // Skip sidechain sessions
    let first_line = head.lines().next().unwrap_or("");
    if first_line.contains("\"isSidechain\":true") || first_line.contains("\"isSidechain\": true") {
        return None;
    }

    let custom_title = extract_last_json_string_field(tail, "customTitle")
        .or_else(|| extract_last_json_string_field(head, "customTitle"))
        .or_else(|| extract_last_json_string_field(tail, "aiTitle"))
        .or_else(|| extract_last_json_string_field(head, "aiTitle"));

    let first_prompt_str = extract_first_prompt_from_head(head);
    let first_prompt = if first_prompt_str.is_empty() { None } else { Some(first_prompt_str.clone()) };

    let summary = custom_title.clone()
        .or_else(|| extract_last_json_string_field(tail, "lastPrompt"))
        .or_else(|| extract_last_json_string_field(tail, "summary"))
        .or_else(|| first_prompt.clone());

    let summary = summary?;

    let git_branch = extract_last_json_string_field(tail, "gitBranch")
        .or_else(|| extract_json_string_field(head, "gitBranch"));

    let session_cwd = extract_json_string_field(head, "cwd")
        .or_else(|| project_path.map(str::to_owned));

    // Scope tag extraction to {"type":"tag"} lines
    let tag_line = tail
        .lines()
        .rev()
        .find(|l| l.starts_with("{\"type\":\"tag\""));
    let tag = tag_line.and_then(|l| extract_last_json_string_field(l, "tag"))
        .filter(|t| !t.is_empty());

    // Parse created_at from first entry's ISO timestamp
    let created_at = extract_json_string_field(first_line, "timestamp")
        .and_then(|ts| parse_iso8601_to_millis(&ts));

    Some(SdkSessionInfo {
        session_id: session_id.to_owned(),
        summary,
        last_modified: mtime,
        file_size: Some(size),
        custom_title,
        first_prompt,
        git_branch,
        cwd: session_cwd,
        tag,
        created_at,
    })
}

/// Parse a subset of ISO 8601 timestamps to milliseconds since epoch.
fn parse_iso8601_to_millis(ts: &str) -> Option<i64> {
    // Normalize trailing Z
    let ts = ts.trim_end_matches('Z');
    // Strip timezone offset if present (e.g. +00:00)
    let ts = if let Some(pos) = ts.rfind('+') {
        &ts[..pos]
    } else if ts.len() > 19 && ts.as_bytes()[19] == b'-' {
        &ts[..19]
    } else {
        ts
    };

    // Expect "YYYY-MM-DDTHH:MM:SS" or "YYYY-MM-DDTHH:MM:SS.mmm"
    let (date_part, time_part) = ts.split_once('T')?;
    let date_parts: Vec<&str> = date_part.split('-').collect();
    if date_parts.len() != 3 {
        return None;
    }
    let year: i64 = date_parts[0].parse().ok()?;
    let month: i64 = date_parts[1].parse().ok()?;
    let day: i64 = date_parts[2].parse().ok()?;

    let (time_main, millis) = if let Some((t, ms)) = time_part.split_once('.') {
        let ms_str = &format!("{:0<3}", ms)[..3];
        let ms_val: i64 = ms_str.parse().ok()?;
        (t, ms_val)
    } else {
        (time_part, 0)
    };

    let time_parts: Vec<&str> = time_main.split(':').collect();
    if time_parts.len() != 3 {
        return None;
    }
    let hour: i64 = time_parts[0].parse().ok()?;
    let minute: i64 = time_parts[1].parse().ok()?;
    let second: i64 = time_parts[2].parse().ok()?;

    // Simplified days-since-epoch computation (Gregorian calendar)
    let days = days_since_epoch(year, month, day)?;
    let secs = days * 86400 + hour * 3600 + minute * 60 + second;
    Some(secs * 1000 + millis)
}

fn days_since_epoch(year: i64, month: i64, day: i64) -> Option<i64> {
    // Using the algorithm from https://en.wikipedia.org/wiki/Julian_day_number
    let a = (14 - month) / 12;
    let y = year + 4800 - a;
    let m = month + 12 * a - 3;
    let jdn = day + (153 * m + 2) / 5 + 365 * y + y / 4 - y / 100 + y / 400 - 32045;
    // Unix epoch (1970-01-01) is JDN 2440588
    Some(jdn - 2440588)
}

// ---------------------------------------------------------------------------
// Core implementation
// ---------------------------------------------------------------------------

fn read_sessions_from_dir(project_dir: &Path, project_path: Option<&str>) -> Vec<SdkSessionInfo> {
    let entries = match fs::read_dir(project_dir) {
        Ok(e) => e,
        Err(_) => return vec![],
    };

    let mut results = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if !name_str.ends_with(".jsonl") {
            continue;
        }
        let session_id_str = &name_str[..name_str.len() - 6];
        let session_id = match validate_uuid(session_id_str) {
            Some(id) => id,
            None => continue,
        };
        let lite = match read_session_lite(&entry.path()) {
            Some(l) => l,
            None => continue,
        };
        if let Some(info) = parse_session_info_from_lite(&session_id, &lite, project_path) {
            results.push(info);
        }
    }
    results
}

fn deduplicate_by_session_id(sessions: Vec<SdkSessionInfo>) -> Vec<SdkSessionInfo> {
    let mut by_id: HashMap<String, SdkSessionInfo> = HashMap::new();
    for s in sessions {
        let existing = by_id.get(&s.session_id);
        if existing.is_none() || s.last_modified > existing.unwrap().last_modified {
            by_id.insert(s.session_id.clone(), s);
        }
    }
    by_id.into_values().collect()
}

fn apply_sort_limit_offset(
    mut sessions: Vec<SdkSessionInfo>,
    limit: Option<usize>,
    offset: usize,
) -> Vec<SdkSessionInfo> {
    sessions.sort_by(|a, b| b.last_modified.cmp(&a.last_modified));
    if offset > 0 {
        sessions = sessions.into_iter().skip(offset).collect();
    }
    if let Some(lim) = limit {
        if lim > 0 {
            sessions.truncate(lim);
        }
    }
    sessions
}

fn list_sessions_for_project(
    directory: &str,
    limit: Option<usize>,
    offset: usize,
    include_worktrees: bool,
) -> Vec<SdkSessionInfo> {
    let canonical_dir = canonicalize_path(directory);

    let worktree_paths = if include_worktrees {
        get_worktree_paths(&canonical_dir)
    } else {
        vec![]
    };

    if worktree_paths.len() <= 1 {
        let project_dir = match find_project_dir(&canonical_dir) {
            Some(d) => d,
            None => return vec![],
        };
        let sessions = read_sessions_from_dir(&project_dir, Some(&canonical_dir));
        return apply_sort_limit_offset(sessions, limit, offset);
    }

    let projects_dir = get_projects_dir();
    let case_insensitive = cfg!(windows);

    // Sort worktree paths: sanitized prefix length descending
    let mut indexed: Vec<(String, String)> = worktree_paths
        .into_iter()
        .map(|wt| {
            let sanitized = sanitize_path(&wt);
            let prefix = if case_insensitive { sanitized.to_lowercase() } else { sanitized };
            (wt, prefix)
        })
        .collect();
    indexed.sort_by(|a, b| b.1.len().cmp(&a.1.len()));

    let all_dirents: Vec<PathBuf> = match fs::read_dir(&projects_dir) {
        Ok(rd) => rd.flatten().filter(|e| e.path().is_dir()).map(|e| e.path()).collect(),
        Err(_) => {
            let project_dir = match find_project_dir(&canonical_dir) {
                Some(d) => d,
                None => return apply_sort_limit_offset(vec![], limit, offset),
            };
            return apply_sort_limit_offset(
                read_sessions_from_dir(&project_dir, Some(&canonical_dir)),
                limit,
                offset,
            );
        }
    };

    let mut all_sessions: Vec<SdkSessionInfo> = Vec::new();
    let mut seen_dirs: HashSet<String> = HashSet::new();

    // Always include the user's actual directory
    if let Some(canonical_project_dir) = find_project_dir(&canonical_dir) {
        let dir_base = canonical_project_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let key = if case_insensitive { dir_base.to_lowercase() } else { dir_base.clone() };
        seen_dirs.insert(key);
        all_sessions.extend(read_sessions_from_dir(&canonical_project_dir, Some(&canonical_dir)));
    }

    for path in &all_dirents {
        let dir_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let dir_key = if case_insensitive { dir_name.to_lowercase() } else { dir_name.clone() };

        if seen_dirs.contains(&dir_key) {
            continue;
        }

        for (wt_path, prefix) in &indexed {
            let is_match = dir_key == *prefix
                || (prefix.len() >= MAX_SANITIZED_LENGTH
                    && dir_key.starts_with(&format!("{}-", prefix)));
            if is_match {
                seen_dirs.insert(dir_key.clone());
                all_sessions.extend(read_sessions_from_dir(path, Some(wt_path)));
                break;
            }
        }
    }

    let deduped = deduplicate_by_session_id(all_sessions);
    apply_sort_limit_offset(deduped, limit, offset)
}

fn list_all_sessions(limit: Option<usize>, offset: usize) -> Vec<SdkSessionInfo> {
    let projects_dir = get_projects_dir();
    let project_dirs: Vec<PathBuf> = match fs::read_dir(&projects_dir) {
        Ok(rd) => rd.flatten().filter(|e| e.path().is_dir()).map(|e| e.path()).collect(),
        Err(_) => return vec![],
    };

    let mut all_sessions: Vec<SdkSessionInfo> = Vec::new();
    for project_dir in project_dirs {
        all_sessions.extend(read_sessions_from_dir(&project_dir, None));
    }

    let deduped = deduplicate_by_session_id(all_sessions);
    apply_sort_limit_offset(deduped, limit, offset)
}

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Lists sessions with metadata extracted from stat + head/tail reads.
///
/// When `directory` is provided, returns sessions for that project directory
/// and its git worktrees. When omitted, returns sessions across all projects.
///
/// # Arguments
/// - `directory` — Project directory to list sessions for.
/// - `limit` — Maximum number of sessions to return.
/// - `offset` — Number of sessions to skip (for pagination).
/// - `include_worktrees` — When `directory` is provided and inside a git repo,
///   include sessions from all git worktree paths. Defaults to `true`.
pub fn list_sessions(
    directory: Option<&str>,
    limit: Option<usize>,
    offset: usize,
    include_worktrees: bool,
) -> Vec<SdkSessionInfo> {
    match directory {
        Some(dir) => list_sessions_for_project(dir, limit, offset, include_worktrees),
        None => list_all_sessions(limit, offset),
    }
}

/// Reads metadata for a single session by ID.
///
/// Wraps a single file read — no O(n) directory scan when `directory` is given.
/// Falls back to worktree paths if not found in the primary directory.
pub fn get_session_info(session_id: &str, directory: Option<&str>) -> Option<SdkSessionInfo> {
    let uuid = validate_uuid(session_id)?;
    let file_name = format!("{}.jsonl", uuid);

    if let Some(dir) = directory {
        let canonical = canonicalize_path(dir);
        if let Some(project_dir) = find_project_dir(&canonical) {
            let path = project_dir.join(&file_name);
            if let Some(lite) = read_session_lite(&path) {
                return parse_session_info_from_lite(&uuid, &lite, Some(&canonical));
            }
        }

        // Worktree fallback
        for wt in get_worktree_paths(&canonical) {
            if wt == canonical {
                continue;
            }
            if let Some(wt_dir) = find_project_dir(&wt) {
                if let Some(lite) = read_session_lite(&wt_dir.join(&file_name)) {
                    return parse_session_info_from_lite(&uuid, &lite, Some(&wt));
                }
            }
        }
        return None;
    }

    // No directory — search all project directories
    let projects_dir = get_projects_dir();
    let entries = fs::read_dir(&projects_dir).ok()?;
    for entry in entries.flatten() {
        if entry.path().is_dir() {
            if let Some(lite) = read_session_lite(&entry.path().join(&file_name)) {
                return parse_session_info_from_lite(&uuid, &lite, None);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// get_session_messages — full transcript reconstruction
// ---------------------------------------------------------------------------

const TRANSCRIPT_ENTRY_TYPES: &[&str] = &["user", "assistant", "progress", "system", "attachment"];

fn try_read_session_file(project_dir: &Path, file_name: &str) -> Option<String> {
    fs::read_to_string(project_dir.join(file_name)).ok()
}

fn read_session_file(session_id: &str, directory: Option<&str>) -> Option<String> {
    let file_name = format!("{}.jsonl", session_id);

    if let Some(dir) = directory {
        let canonical_dir = canonicalize_path(dir);

        if let Some(project_dir) = find_project_dir(&canonical_dir) {
            if let Some(content) = try_read_session_file(&project_dir, &file_name) {
                return Some(content);
            }
        }

        for wt in get_worktree_paths(&canonical_dir) {
            if wt == canonical_dir {
                continue;
            }
            if let Some(wt_dir) = find_project_dir(&wt) {
                if let Some(content) = try_read_session_file(&wt_dir, &file_name) {
                    return Some(content);
                }
            }
        }
        return None;
    }

    let projects_dir = get_projects_dir();
    let entries = fs::read_dir(&projects_dir).ok()?;
    for entry in entries.flatten() {
        if let Some(content) = try_read_session_file(&entry.path(), &file_name) {
            return Some(content);
        }
    }
    None
}

fn parse_transcript_entries(content: &str) -> Vec<Value> {
    let mut entries = Vec::new();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let entry: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if !entry.is_object() {
            continue;
        }
        let entry_type = entry.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if TRANSCRIPT_ENTRY_TYPES.contains(&entry_type) && entry.get("uuid").and_then(|u| u.as_str()).is_some() {
            entries.push(entry);
        }
    }
    entries
}

fn build_conversation_chain(entries: &[Value]) -> Vec<&Value> {
    if entries.is_empty() {
        return vec![];
    }

    let by_uuid: HashMap<&str, &Value> = entries
        .iter()
        .filter_map(|e| e.get("uuid").and_then(|u| u.as_str()).map(|u| (u, e)))
        .collect();

    let entry_index: HashMap<&str, usize> = entries
        .iter()
        .enumerate()
        .filter_map(|(i, e)| e.get("uuid").and_then(|u| u.as_str()).map(|u| (u, i)))
        .collect();

    let parent_uuids: HashSet<&str> = entries
        .iter()
        .filter_map(|e| e.get("parentUuid").and_then(|p| p.as_str()))
        .collect();

    let terminals: Vec<&Value> = entries
        .iter()
        .filter(|e| {
            e.get("uuid")
                .and_then(|u| u.as_str())
                .map(|u| !parent_uuids.contains(u))
                .unwrap_or(false)
        })
        .collect();

    let mut leaves: Vec<&Value> = Vec::new();
    for terminal in terminals {
        let mut cur: Option<&Value> = Some(terminal);
        let mut seen: HashSet<&str> = HashSet::new();
        while let Some(node) = cur {
            let uid = match node.get("uuid").and_then(|u| u.as_str()) {
                Some(u) => u,
                None => break,
            };
            if !seen.insert(uid) {
                break;
            }
            let t = node.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if t == "user" || t == "assistant" {
                leaves.push(node);
                break;
            }
            let parent = node.get("parentUuid").and_then(|p| p.as_str());
            cur = parent.and_then(|p| by_uuid.get(p).copied());
        }
    }

    if leaves.is_empty() {
        return vec![];
    }

    let main_leaves: Vec<&Value> = leaves
        .iter()
        .copied()
        .filter(|leaf| {
            leaf.get("isSidechain").and_then(|v| v.as_bool()).unwrap_or(false) == false
                && leaf.get("teamName").is_none()
                && leaf.get("isMeta").and_then(|v| v.as_bool()).unwrap_or(false) == false
        })
        .collect();

    fn pick_best<'a>(candidates: &[&'a Value], index: &HashMap<&str, usize>) -> &'a Value {
        let mut best = candidates[0];
        let mut best_idx = index
            .get(best.get("uuid").and_then(|u| u.as_str()).unwrap_or(""))
            .copied()
            .unwrap_or(0);
        for &cur in &candidates[1..] {
            let cur_idx = index
                .get(cur.get("uuid").and_then(|u| u.as_str()).unwrap_or(""))
                .copied()
                .unwrap_or(0);
            if cur_idx > best_idx {
                best = cur;
                best_idx = cur_idx;
            }
        }
        best
    }

    let leaf = if !main_leaves.is_empty() {
        pick_best(&main_leaves, &entry_index)
    } else {
        pick_best(&leaves, &entry_index)
    };

    let mut chain: Vec<&Value> = Vec::new();
    let mut chain_seen: HashSet<&str> = HashSet::new();
    let mut chain_cur: Option<&Value> = Some(leaf);
    while let Some(node) = chain_cur {
        let uid = match node.get("uuid").and_then(|u| u.as_str()) {
            Some(u) => u,
            None => break,
        };
        if !chain_seen.insert(uid) {
            break;
        }
        chain.push(node);
        let parent = node.get("parentUuid").and_then(|p| p.as_str());
        chain_cur = parent.and_then(|p| by_uuid.get(p).copied());
    }

    chain.reverse();
    chain
}

fn is_visible_message(entry: &Value) -> bool {
    let entry_type = entry.get("type").and_then(|t| t.as_str()).unwrap_or("");
    if entry_type != "user" && entry_type != "assistant" {
        return false;
    }
    if entry.get("isMeta").and_then(|v| v.as_bool()).unwrap_or(false) {
        return false;
    }
    if entry.get("isSidechain").and_then(|v| v.as_bool()).unwrap_or(false) {
        return false;
    }
    entry.get("teamName").is_none()
}

fn to_session_message(entry: &Value) -> SessionMessage {
    let message_type = if entry.get("type").and_then(|t| t.as_str()) == Some("user") {
        SessionMessageType::User
    } else {
        SessionMessageType::Assistant
    };
    SessionMessage {
        message_type,
        uuid: entry.get("uuid").and_then(|u| u.as_str()).unwrap_or("").to_owned(),
        session_id: entry.get("sessionId").and_then(|s| s.as_str()).unwrap_or("").to_owned(),
        message: entry.get("message").cloned(),
        parent_tool_use_id: None,
    }
}

/// Reads a session's conversation messages from its JSONL transcript file.
///
/// Parses the full JSONL, builds the conversation chain via `parentUuid` links,
/// and returns user/assistant messages in chronological order.
pub fn get_session_messages(
    session_id: &str,
    directory: Option<&str>,
    limit: Option<usize>,
    offset: usize,
) -> Vec<SessionMessage> {
    if validate_uuid(session_id).is_none() {
        return vec![];
    }

    let content = match read_session_file(session_id, directory) {
        Some(c) if !c.is_empty() => c,
        _ => return vec![],
    };

    let entries = parse_transcript_entries(&content);
    let chain = build_conversation_chain(&entries);
    let mut messages: Vec<SessionMessage> = chain
        .into_iter()
        .filter(|e| is_visible_message(e))
        .map(|e| to_session_message(e))
        .collect();

    if offset > 0 {
        messages = messages.into_iter().skip(offset).collect();
    }
    if let Some(lim) = limit {
        if lim > 0 {
            messages.truncate(lim);
        }
    }
    messages
}
