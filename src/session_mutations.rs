//! Session mutation functions.
//!
//! Rename/tag append typed metadata entries to the session's JSONL (matching
//! the CLI pattern); delete removes the JSONL file; fork creates a new session
//! with UUID remapping.
//!
//! Directory resolution matches `list_sessions` / `get_session_messages`:
//! `directory` is the project path (not the storage dir); when omitted, all
//! project directories are searched for the session file.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::Value;
use uuid::Uuid;

use crate::errors::ClaudeError;
use crate::sessions::{
    canonicalize_path, find_project_dir, get_projects_dir, get_worktree_paths,
};
use crate::types::sessions::ForkSessionResult;

// Re-export from sessions for use here
const LITE_READ_BUF_SIZE: usize = 65536;

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Rename a session by appending a `custom-title` entry.
///
/// `list_sessions` reads the LAST custom-title from the file tail, so
/// repeated calls are safe — the most recent wins.
pub fn rename_session(
    session_id: &str,
    title: &str,
    directory: Option<&str>,
) -> Result<(), ClaudeError> {
    validate_uuid_arg(session_id)?;
    let stripped = title.trim();
    if stripped.is_empty() {
        return Err(ClaudeError::InvalidConfig("title must be non-empty".into()));
    }

    let data = format!(
        "{}\n",
        serde_json::json!({
            "type": "custom-title",
            "customTitle": stripped,
            "sessionId": session_id,
        })
        .to_string()
    );

    append_to_session(session_id, &data, directory)
}

/// Tag a session. Pass `None` to clear the tag.
///
/// Tags are Unicode-sanitized before storing. `list_sessions` reads the LAST
/// tag entry, so the most recent call wins.
pub fn tag_session(
    session_id: &str,
    tag: Option<&str>,
    directory: Option<&str>,
) -> Result<(), ClaudeError> {
    validate_uuid_arg(session_id)?;
    let tag_value = if let Some(t) = tag {
        let sanitized = sanitize_unicode(t);
        let stripped = sanitized.trim().to_owned();
        if stripped.is_empty() {
            return Err(ClaudeError::InvalidConfig(
                "tag must be non-empty (use None to clear)".into(),
            ));
        }
        stripped
    } else {
        String::new()
    };

    let data = format!(
        "{}\n",
        serde_json::json!({
            "type": "tag",
            "tag": tag_value,
            "sessionId": session_id,
        })
        .to_string()
    );

    append_to_session(session_id, &data, directory)
}

/// Delete a session by removing its JSONL file.
///
/// This is a hard delete — the file is removed permanently. For soft-delete
/// semantics, use `tag_session(id, Some("__hidden"), ...)` and filter on listing.
pub fn delete_session(session_id: &str, directory: Option<&str>) -> Result<(), ClaudeError> {
    validate_uuid_arg(session_id)?;

    let path = find_session_file(session_id, directory).ok_or_else(|| {
        ClaudeError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "Session {} not found{}",
                session_id,
                directory
                    .map(|d| format!(" in project directory for {}", d))
                    .unwrap_or_default()
            ),
        ))
    })?;

    fs::remove_file(&path).map_err(ClaudeError::Io)
}

/// Fork a session into a new branch with fresh UUIDs.
///
/// Copies transcript messages from the source session into a new session file,
/// remapping every message UUID and preserving the `parentUuid` chain. Supports
/// `up_to_message_id` for branching from a specific point in the conversation.
pub fn fork_session(
    session_id: &str,
    directory: Option<&str>,
    up_to_message_id: Option<&str>,
    title: Option<&str>,
) -> Result<ForkSessionResult, ClaudeError> {
    validate_uuid_arg(session_id)?;
    if let Some(uid) = up_to_message_id {
        validate_uuid_arg(uid)?;
    }

    let (file_path, project_dir) =
        find_session_file_with_dir(session_id, directory).ok_or_else(|| {
            ClaudeError::Io(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "Session {} not found{}",
                    session_id,
                    directory
                        .map(|d| format!(" in project directory for {}", d))
                        .unwrap_or_default()
                ),
            ))
        })?;

    let content = fs::read(&file_path)?;
    if content.is_empty() {
        return Err(ClaudeError::InvalidConfig(format!(
            "Session {} has no messages to fork",
            session_id
        )));
    }

    let (mut transcript, content_replacements) = parse_fork_transcript(&content, session_id);

    // Filter out sidechains
    transcript.retain(|e| {
        !e.get("isSidechain")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
    });

    if transcript.is_empty() {
        return Err(ClaudeError::InvalidConfig(format!(
            "Session {} has no messages to fork",
            session_id
        )));
    }

    if let Some(cutoff_id) = up_to_message_id {
        let pos = transcript
            .iter()
            .position(|e| e.get("uuid").and_then(|u| u.as_str()) == Some(cutoff_id));
        match pos {
            Some(i) => transcript.truncate(i + 1),
            None => {
                return Err(ClaudeError::InvalidConfig(format!(
                    "Message {} not found in session {}",
                    cutoff_id, session_id
                )));
            }
        }
    }

    // Build UUID mapping (include progress entries for parentUuid chain)
    let mut uuid_mapping: HashMap<String, String> = HashMap::new();
    for entry in &transcript {
        if let Some(uid) = entry.get("uuid").and_then(|u| u.as_str()) {
            uuid_mapping.insert(uid.to_owned(), Uuid::new_v4().hyphenated().to_string());
        }
    }

    // Filter progress from written output
    let writable: Vec<&Value> = transcript
        .iter()
        .filter(|e| e.get("type").and_then(|t| t.as_str()) != Some("progress"))
        .collect();

    if writable.is_empty() {
        return Err(ClaudeError::InvalidConfig(format!(
            "Session {} has no messages to fork",
            session_id
        )));
    }

    let by_uuid: HashMap<&str, &Value> = transcript
        .iter()
        .filter_map(|e| e.get("uuid").and_then(|u| u.as_str()).map(|u| (u, e)))
        .collect();

    let forked_session_id = Uuid::new_v4().hyphenated().to_string();
    let now = chrono_now_iso();
    let writable_len = writable.len();
    let mut lines: Vec<String> = Vec::new();

    for (i, original) in writable.iter().enumerate() {
        let orig_uuid = original.get("uuid").and_then(|u| u.as_str()).unwrap_or("");
        let new_uuid = uuid_mapping.get(orig_uuid).cloned().unwrap_or_default();

        // Resolve parentUuid, skipping progress ancestors
        let mut new_parent_uuid: Option<String> = None;
        let mut parent_id = original.get("parentUuid").and_then(|p| p.as_str());
        while let Some(pid) = parent_id {
            match by_uuid.get(pid) {
                None => break,
                Some(parent) => {
                    if parent.get("type").and_then(|t| t.as_str()) != Some("progress") {
                        new_parent_uuid = uuid_mapping.get(pid).cloned();
                        break;
                    }
                    parent_id = parent.get("parentUuid").and_then(|p| p.as_str());
                }
            }
        }

        let timestamp = if i == writable_len - 1 {
            now.clone()
        } else {
            original
                .get("timestamp")
                .and_then(|t| t.as_str())
                .unwrap_or(&now)
                .to_owned()
        };

        let logical_parent = original
            .get("logicalParentUuid")
            .and_then(|p| p.as_str())
            .and_then(|p| uuid_mapping.get(p).cloned());

        let mut forked = original.as_object().cloned().unwrap_or_default();
        forked.insert("uuid".to_owned(), Value::String(new_uuid));
        forked.insert(
            "parentUuid".to_owned(),
            new_parent_uuid
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        forked.insert(
            "logicalParentUuid".to_owned(),
            logical_parent
                .map(Value::String)
                .unwrap_or(Value::Null),
        );
        forked.insert("sessionId".to_owned(), Value::String(forked_session_id.clone()));
        forked.insert("timestamp".to_owned(), Value::String(timestamp));
        forked.insert("isSidechain".to_owned(), Value::Bool(false));
        forked.insert(
            "forkedFrom".to_owned(),
            serde_json::json!({
                "sessionId": session_id,
                "messageUuid": orig_uuid,
            }),
        );

        for key in &["teamName", "agentName", "slug", "sourceToolAssistantUUID"] {
            forked.remove(*key);
        }

        lines.push(serde_json::to_string(&forked).unwrap_or_default());
    }

    if !content_replacements.is_empty() {
        lines.push(
            serde_json::to_string(&serde_json::json!({
                "type": "content-replacement",
                "sessionId": forked_session_id,
                "replacements": content_replacements,
            }))
            .unwrap_or_default(),
        );
    }

    // Derive title
    let fork_title = if let Some(t) = title {
        let stripped = t.trim().to_owned();
        if !stripped.is_empty() {
            stripped
        } else {
            derive_fork_title(&content)
        }
    } else {
        derive_fork_title(&content)
    };

    lines.push(
        serde_json::to_string(&serde_json::json!({
            "type": "custom-title",
            "sessionId": forked_session_id,
            "customTitle": fork_title,
        }))
        .unwrap_or_default(),
    );

    let fork_path = project_dir.join(format!("{}.jsonl", forked_session_id));
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&fork_path)?;
    file.write_all((lines.join("\n") + "\n").as_bytes())?;

    Ok(ForkSessionResult {
        session_id: forked_session_id,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn validate_uuid_arg(s: &str) -> Result<(), ClaudeError> {
    Uuid::parse_str(s).map_err(|_| {
        ClaudeError::InvalidConfig(format!("Invalid session_id: {}", s))
    })?;
    Ok(())
}

pub(crate) fn find_session_file(session_id: &str, directory: Option<&str>) -> Option<PathBuf> {
    find_session_file_with_dir(session_id, directory).map(|(p, _)| p)
}

pub(crate) fn find_session_file_with_dir(
    session_id: &str,
    directory: Option<&str>,
) -> Option<(PathBuf, PathBuf)> {
    let file_name = format!("{}.jsonl", session_id);

    let try_dir = |project_dir: &Path| -> Option<(PathBuf, PathBuf)> {
        let path = project_dir.join(&file_name);
        let meta = path.metadata().ok()?;
        if meta.len() > 0 {
            Some((path, project_dir.to_owned()))
        } else {
            None
        }
    };

    if let Some(dir) = directory {
        let canonical = canonicalize_path(dir);
        if let Some(project_dir) = find_project_dir(&canonical) {
            if let Some(result) = try_dir(&project_dir) {
                return Some(result);
            }
        }

        for wt in get_worktree_paths(&canonical) {
            if wt == canonical {
                continue;
            }
            if let Some(wt_dir) = find_project_dir(&wt) {
                if let Some(result) = try_dir(&wt_dir) {
                    return Some(result);
                }
            }
        }
        return None;
    }

    let projects_dir = get_projects_dir();
    let entries = fs::read_dir(&projects_dir).ok()?;
    for entry in entries.flatten() {
        if let Some(result) = try_dir(&entry.path()) {
            return Some(result);
        }
    }
    None
}

const TRANSCRIPT_TYPES: &[&str] = &["user", "assistant", "attachment", "system", "progress"];

fn parse_fork_transcript(content: &[u8], session_id: &str) -> (Vec<Value>, Vec<Value>) {
    let text = String::from_utf8_lossy(content);
    let mut transcript = Vec::new();
    let mut content_replacements = Vec::new();

    for line in text.lines() {
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
        if TRANSCRIPT_TYPES.contains(&entry_type)
            && entry.get("uuid").and_then(|u| u.as_str()).is_some()
        {
            transcript.push(entry);
        } else if entry_type == "content-replacement"
            && entry.get("sessionId").and_then(|s| s.as_str()) == Some(session_id)
        {
            if let Some(Value::Array(reps)) = entry.get("replacements") {
                content_replacements.extend(reps.clone());
            }
        }
    }

    (transcript, content_replacements)
}

fn append_to_session(
    session_id: &str,
    data: &str,
    directory: Option<&str>,
) -> Result<(), ClaudeError> {
    let file_name = format!("{}.jsonl", session_id);

    if let Some(dir) = directory {
        let canonical = canonicalize_path(dir);

        if let Some(project_dir) = find_project_dir(&canonical) {
            if try_append(&project_dir.join(&file_name), data)? {
                return Ok(());
            }
        }

        for wt in get_worktree_paths(&canonical) {
            if wt == canonical {
                continue;
            }
            if let Some(wt_dir) = find_project_dir(&wt) {
                if try_append(&wt_dir.join(&file_name), data)? {
                    return Ok(());
                }
            }
        }

        return Err(ClaudeError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!(
                "Session {} not found in project directory for {}",
                session_id, dir
            ),
        )));
    }

    let projects_dir = get_projects_dir();
    let entries = fs::read_dir(&projects_dir).map_err(|e| {
        ClaudeError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("Session {} not found (no projects directory): {}", session_id, e),
        ))
    })?;

    for entry in entries.flatten() {
        if try_append(&entry.path().join(&file_name), data)? {
            return Ok(());
        }
    }

    Err(ClaudeError::Io(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("Session {} not found in any project directory", session_id),
    )))
}

/// Append `data` to `path` only if the file exists and is non-empty.
///
/// Returns `true` on success, `false` if the file doesn't exist or is empty.
/// Re-raises all other IO errors.
fn try_append(path: &Path, data: &str) -> Result<bool, ClaudeError> {
    match fs::metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(ClaudeError::Io(e)),
        Ok(m) if m.len() == 0 => return Ok(false),
        Ok(_) => {}
    }

    let mut file = fs::OpenOptions::new().append(true).open(path)?;
    file.write_all(data.as_bytes())?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// Unicode sanitization
// ---------------------------------------------------------------------------

fn sanitize_unicode(value: &str) -> String {
    // Iteratively remove dangerous Unicode categories.
    // Matches Python SDK's _sanitize_unicode logic.
    let mut current = value.to_owned();
    for _ in 0..10 {
        let previous = current.clone();
        current = current
            .chars()
            .filter(|&c| {
                // Remove zero-width/directional/private-use/format characters
                let cp = c as u32;
                !(0x200B..=0x200F).contains(&cp)  // Zero-width spaces, LTR/RTL
                    && !(0x202A..=0x202E).contains(&cp) // Directional formatting
                    && !(0x2066..=0x2069).contains(&cp) // Directional isolates
                    && cp != 0xFEFF                     // BOM
                    && !(0xE000..=0xF8FF).contains(&cp) // Private use area
            })
            .collect();
        if current == previous {
            break;
        }
    }
    current
}

// ---------------------------------------------------------------------------
// Fork helpers
// ---------------------------------------------------------------------------

fn chrono_now_iso() -> String {
    // Minimal ISO 8601 UTC timestamp without chrono dependency
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let (year, month, day, hour, minute, second) = secs_to_datetime(secs);
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.000Z",
        year, month, day, hour, minute, second
    )
}

fn secs_to_datetime(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let second = secs % 60;
    let minutes = secs / 60;
    let minute = minutes % 60;
    let hours = minutes / 60;
    let hour = hours % 24;
    let days = hours / 24;

    // Convert days since epoch to year/month/day (Gregorian)
    // Using the algorithm from https://www.researchgate.net/publication/316558298
    let z = days + 719468;
    let era = z / 146097;
    let doe = z % 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    (y, m, d, hour, minute, second)
}

fn derive_fork_title(content: &[u8]) -> String {
    let buf_len = content.len();
    let head = String::from_utf8_lossy(&content[..buf_len.min(LITE_READ_BUF_SIZE)]).into_owned();
    let tail = String::from_utf8_lossy(
        &content[buf_len.saturating_sub(LITE_READ_BUF_SIZE)..],
    )
    .into_owned();

    // Reuse sessions module's field extraction logic
    let base = crate::sessions::extract_last_json_string_field_pub(&tail, "customTitle")
        .or_else(|| crate::sessions::extract_last_json_string_field_pub(&head, "customTitle"))
        .or_else(|| crate::sessions::extract_last_json_string_field_pub(&tail, "aiTitle"))
        .or_else(|| crate::sessions::extract_last_json_string_field_pub(&head, "aiTitle"))
        .or_else(|| {
            let first = crate::sessions::extract_first_prompt_from_head_pub(&head);
            if first.is_empty() { None } else { Some(first) }
        })
        .unwrap_or_else(|| "Forked session".to_owned());

    format!("{} (fork)", base)
}
