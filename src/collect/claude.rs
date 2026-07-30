//! Claude Code session resolution: which session a running claude process
//! belongs to, and its working directory.
//!
//! Claude stores conversations as `<session-id>.jsonl` under
//! `~/.claude/projects/<encoded-cwd>/`. The encoded cwd is the process's real
//! cwd with `/` replaced by `-`. A running claude is actively writing to its
//! session file, so the most recently modified `.jsonl` in the project dir is
//! the live session.
//!
//! There is no pid-to-session mapping in the file format — multiple claude
//! processes sharing a cwd are ambiguous. In practice that is rare (one
//! terminal per project), and even then the most-recent-file heuristic is
//! correct for the session the reader is looking at.

use std::path::PathBuf;

/// A Claude Code session associated with a running process.
#[derive(Clone, Debug, PartialEq)]
pub struct ClaudeSession {
    pub session_id: String,
    /// The cwd the session was started in, read from the jsonl — more reliable
    /// than the process's cwd when a wrapper like ccproxy is involved.
    pub cwd: String,
}

/// Resolve the Claude session for a process, given its cwd.
///
/// Returns `None` when the process is not a claude instance, or when no session
/// file can be found. The caller has already identified the process as claude;
/// this function does the file-system lookup.
pub fn session_for(cwd: &str) -> Option<ClaudeSession> {
    let uid = std::env::var("UID")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(502);
    // Claude encodes the cwd by replacing both `/` and `.` with `-`, so
    // `/Users/gavin.jeong` becomes `-Users-gavin-jeong`.
    let encoded = cwd.replace(['/', '.'], "-");

    // `/tmp/claude-<uid>/<encoded-cwd>/` contains one directory per session,
    // named by session UUID. `/tmp` is not TCC-protected (unlike `~/.claude`),
    // so Rust's `std::fs` can read it directly.
    let session_dir = PathBuf::from(format!("/tmp/claude-{uid}")).join(&encoded);

    // The most recently modified subdirectory is the active session.
    let newest = std::fs::read_dir(&session_dir).ok()?.filter_map(|entry| {
        let entry = entry.ok()?;
        let path = entry.path();
        if path.is_dir() {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, path))
        } else {
            None
        }
    });

    let (_, dir) = newest.max_by_key(|(modified, _)| *modified)?;
    let session_id = dir.file_name()?.to_str()?.to_string();

    // The cwd is read from the session's jsonl, but that lives under
    // `~/.claude` which TCC may block. Fall back to the process's cwd (the
    // caller already knows it) when the file is unreachable.
    let home = std::env::var("HOME").unwrap_or_default();
    let jsonl = PathBuf::from(&home)
        .join(".claude/projects")
        .join(&encoded)
        .join(format!("{session_id}.jsonl"));
    let cwd = std::fs::read_to_string(&jsonl)
        .ok()
        .and_then(|content| {
            content
                .lines()
                .rev()
                .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
                .find_map(|value| value.get("cwd").and_then(|v| v.as_str()).map(String::from))
        })
        .unwrap_or_else(|| cwd.to_string());

    Some(ClaudeSession { session_id, cwd })
}

/// Whether a process name looks like a Claude Code instance.
pub fn is_claude(command: &str) -> bool {
    let name = command.split_whitespace().next().unwrap_or("");
    name.ends_with("claude") || name.ends_with("claude.exe") || name == "claude"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_detection_covers_both_binary_names() {
        assert!(is_claude("claude --dangerously-skip-permissions"));
        assert!(is_claude("/opt/claude/bin/claude --model default"));
        assert!(is_claude("claude.exe --settings {\"a\":1}"));
        assert!(!is_claude("ccproxy claude --intercept=mitm"));
        assert!(!is_claude("node qmd mcp"));
    }

    #[test]
    fn session_for_a_nonexistent_cwd_returns_none() {
        assert!(session_for("/this/path/does/not/exist/anywhere").is_none());
    }
}
