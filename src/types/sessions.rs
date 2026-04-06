//! Session listing and mutation types

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Session metadata returned by [`list_sessions`](crate::sessions::list_sessions).
///
/// Contains only data extractable from stat + head/tail reads — no full
/// JSONL parsing required.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SdkSessionInfo {
    /// Unique session identifier (UUID).
    pub session_id: String,
    /// Display title — custom title, AI-generated title, or first prompt.
    pub summary: String,
    /// Last modified time in milliseconds since epoch.
    pub last_modified: i64,
    /// Session file size in bytes.
    pub file_size: Option<u64>,
    /// User-set custom title or AI-generated title.
    pub custom_title: Option<String>,
    /// First meaningful user prompt in the session.
    pub first_prompt: Option<String>,
    /// Git branch at the end of the session.
    pub git_branch: Option<String>,
    /// Working directory for the session.
    pub cwd: Option<String>,
    /// User-set session tag.
    pub tag: Option<String>,
    /// Creation time in milliseconds since epoch.
    pub created_at: Option<i64>,
}

/// Message type for a session transcript entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SessionMessageType {
    User,
    Assistant,
}

/// A user or assistant message from a session transcript.
///
/// Returned by [`get_session_messages`](crate::sessions::get_session_messages)
/// for reading historical session data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMessage {
    /// Message type — user or assistant.
    #[serde(rename = "type")]
    pub message_type: SessionMessageType,
    /// Unique message identifier (UUID).
    pub uuid: String,
    /// ID of the session this message belongs to.
    pub session_id: String,
    /// Raw message payload (role, content, etc.).
    pub message: Option<Value>,
    /// Always `None` for top-level conversation messages.
    pub parent_tool_use_id: Option<String>,
}

/// Result of a [`fork_session`](crate::session_mutations::fork_session) operation.
#[derive(Debug, Clone)]
pub struct ForkSessionResult {
    /// UUID of the new forked session.
    pub session_id: String,
}
