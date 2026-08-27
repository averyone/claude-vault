use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::db::SearchResult;
use crate::strip_tool_lines;

/// Everything the revived transcript needs that the vault does not store.
pub struct ReviveOptions<'a> {
    /// Working directory the revived session belongs to (where `revive` was run).
    pub cwd: &'a Path,
    /// Claude config directory (usually ~/.claude).
    pub claude_dir: &'a Path,
    /// Keep `[tool_use: ...]` lines in the revived transcript.
    pub include_tools: bool,
    /// Git branch recorded on each entry, if `cwd` is a repository.
    pub git_branch: Option<String>,
    /// Claude Code version recorded on each entry, if it could be detected.
    pub version: Option<String>,
}

#[derive(Debug)]
pub struct ReviveResult {
    pub session_id: String,
    pub path: PathBuf,
    pub message_count: usize,
}

/// Encode a working directory the way Claude Code names its project directories:
/// `/Users/me/src/repo` -> `-Users-me-src-repo`. Dots are dashes too, so
/// `.claude-worktrees` round-trips the same way Claude Code writes it.
pub fn encode_project_dir(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c == '/' || c == '.' { '-' } else { c })
        .collect()
}

struct Turn {
    role: String,
    content: String,
    timestamp: Option<String>,
}

/// Collapse archived messages into an alternating user/assistant transcript.
///
/// The vault drops tool results during import, so a session can end up with
/// runs of same-role messages. Joining each run into one turn — and dropping
/// any assistant turns before the first user turn — keeps the revived file a
/// well-formed conversation that Claude Code can pick up and continue.
fn build_turns(messages: &[SearchResult], include_tools: bool) -> Vec<Turn> {
    let mut turns: Vec<Turn> = Vec::new();

    for m in messages {
        let content = if include_tools {
            m.content.clone()
        } else {
            strip_tool_lines(&m.content)
        };
        let content = content.trim();
        if content.is_empty() {
            continue;
        }
        if turns.is_empty() && m.role != "user" {
            continue;
        }

        match turns.last_mut() {
            Some(last) if last.role == m.role => {
                last.content.push_str("\n\n");
                last.content.push_str(content);
            }
            _ => turns.push(Turn {
                role: m.role.clone(),
                content: content.to_string(),
                timestamp: m.timestamp.clone(),
            }),
        }
    }

    turns
}

fn user_entry(text: &str) -> Value {
    json!({
        "type": "user",
        "message": { "role": "user", "content": [{ "type": "text", "text": text }] },
    })
}

/// Assistant entries carry the metadata Claude Code writes for messages it
/// synthesized rather than received from the API — same shape it uses for its
/// own injected turns, so the revived transcript reads back as a normal one.
fn assistant_entry(text: &str) -> Value {
    json!({
        "type": "assistant",
        "message": {
            "id": uuid::Uuid::new_v4().to_string(),
            "type": "message",
            "role": "assistant",
            "model": "<synthetic>",
            "content": [{ "type": "text", "text": text }],
            "stop_reason": "stop_sequence",
            "stop_sequence": "",
            "usage": {
                "input_tokens": 0,
                "output_tokens": 0,
                "cache_creation_input_tokens": 0,
                "cache_read_input_tokens": 0,
            },
        },
    })
}

/// Build the JSONL lines for a revived session.
pub fn build_transcript(
    messages: &[SearchResult],
    session_id: &str,
    opts: &ReviveOptions,
) -> Vec<String> {
    let turns = build_turns(messages, opts.include_tools);
    let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let cwd = opts.cwd.to_string_lossy().to_string();

    let mut lines = Vec::with_capacity(turns.len());
    let mut parent: Option<String> = None;
    let mut last_timestamp = now.clone();

    for turn in &turns {
        let mut entry = if turn.role == "user" {
            user_entry(&turn.content)
        } else {
            assistant_entry(&turn.content)
        };

        if let Some(ts) = &turn.timestamp {
            last_timestamp = ts.clone();
        }
        let uuid = uuid::Uuid::new_v4().to_string();

        let obj = entry.as_object_mut().expect("entry is a JSON object");
        obj.insert("parentUuid".into(), json!(parent));
        obj.insert("isSidechain".into(), json!(false));
        obj.insert("userType".into(), json!("external"));
        obj.insert("cwd".into(), json!(cwd));
        obj.insert("sessionId".into(), json!(session_id));
        obj.insert("uuid".into(), json!(uuid));
        obj.insert("timestamp".into(), json!(last_timestamp));
        if let Some(branch) = &opts.git_branch {
            obj.insert("gitBranch".into(), json!(branch));
        }
        if let Some(version) = &opts.version {
            obj.insert("version".into(), json!(version));
        }

        parent = Some(uuid);
        lines.push(entry.to_string());
    }

    lines
}

/// Write an archived session back into `~/.claude/projects/` as a resumable
/// JSONL transcript. The revived session always gets a fresh ID so it can
/// never overwrite a session Claude Code already has on disk.
pub fn revive_session(messages: &[SearchResult], opts: &ReviveOptions) -> Result<ReviveResult> {
    let session_id = uuid::Uuid::new_v4().to_string();
    let lines = build_transcript(messages, &session_id, opts);
    if lines.is_empty() {
        anyhow::bail!("Session has no revivable messages");
    }

    let project_dir = opts
        .claude_dir
        .join("projects")
        .join(encode_project_dir(opts.cwd));
    std::fs::create_dir_all(&project_dir)
        .with_context(|| format!("Failed to create {}", project_dir.display()))?;

    let path = project_dir.join(format!("{session_id}.jsonl"));
    let mut file = std::fs::File::create(&path)
        .with_context(|| format!("Failed to create {}", path.display()))?;
    for line in &lines {
        writeln!(file, "{line}")?;
    }

    Ok(ReviveResult {
        session_id,
        path,
        message_count: lines.len(),
    })
}

/// Best-effort branch name for `cwd`; `None` when it is not a git repository.
pub fn detect_git_branch(cwd: &Path) -> Option<String> {
    // `branch --show-current` (not `rev-parse`) so a repo with no commits yet
    // still reports its branch, and a detached HEAD reports nothing.
    let output = Command::new("git")
        .args(["branch", "--show-current"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let branch = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!branch.is_empty()).then_some(branch)
}

/// Best-effort Claude Code version; `None` when the CLI is not on PATH.
pub fn detect_claude_version() -> Option<String> {
    let output = Command::new("claude").arg("--version").output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?;
    // e.g. "2.1.246 (Claude Code)" -> "2.1.246"
    text.split_whitespace().next().map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn msg(role: &str, content: &str, timestamp: Option<&str>) -> SearchResult {
        SearchResult {
            session_id: "s1".into(),
            project: "proj".into(),
            role: role.into(),
            content: content.into(),
            timestamp: timestamp.map(String::from),
        }
    }

    fn opts<'a>(cwd: &'a Path, claude_dir: &'a Path) -> ReviveOptions<'a> {
        ReviveOptions {
            cwd,
            claude_dir,
            include_tools: false,
            git_branch: None,
            version: None,
        }
    }

    #[test]
    fn test_encode_project_dir() {
        assert_eq!(
            encode_project_dir(Path::new("/Users/me/src/repo")),
            "-Users-me-src-repo"
        );
    }

    #[test]
    fn test_encode_project_dir_dots_become_dashes() {
        assert_eq!(
            encode_project_dir(Path::new("/home/me/repo/.claude-worktrees/wt")),
            "-home-me-repo--claude-worktrees-wt"
        );
    }

    #[test]
    fn test_build_turns_merges_consecutive_same_role() {
        let messages = vec![
            msg("user", "hello", None),
            msg("assistant", "first", None),
            msg("assistant", "second", None),
            msg("user", "bye", None),
        ];
        let turns = build_turns(&messages, false);
        assert_eq!(turns.len(), 3);
        assert_eq!(turns[1].role, "assistant");
        assert_eq!(turns[1].content, "first\n\nsecond");
    }

    #[test]
    fn test_build_turns_drops_leading_assistant() {
        let messages = vec![msg("assistant", "orphan", None), msg("user", "hello", None)];
        let turns = build_turns(&messages, false);
        assert_eq!(turns.len(), 1);
        assert_eq!(turns[0].role, "user");
    }

    #[test]
    fn test_build_turns_strips_tool_lines_by_default() {
        let messages = vec![
            msg("user", "hi", None),
            msg(
                "assistant",
                "plan\n[tool_use: Edit] {\"f\":\"x\"}\ndone",
                None,
            ),
        ];
        let turns = build_turns(&messages, false);
        assert_eq!(turns[1].content, "plan\ndone");

        let turns = build_turns(&messages, true);
        assert!(turns[1].content.contains("[tool_use: Edit]"));
    }

    #[test]
    fn test_build_turns_skips_tool_only_message() {
        let messages = vec![
            msg("user", "hi", None),
            msg("assistant", "[tool_use: Bash] {\"command\":\"ls\"}", None),
            msg("assistant", "done", None),
        ];
        let turns = build_turns(&messages, false);
        assert_eq!(turns.len(), 2);
        assert_eq!(turns[1].content, "done");
    }

    #[test]
    fn test_build_transcript_chains_parent_uuids() {
        let tmp = TempDir::new().unwrap();
        let messages = vec![
            msg("user", "hello", Some("2024-01-01T00:00:00Z")),
            msg("assistant", "hi there", Some("2024-01-01T00:00:01Z")),
        ];
        let lines = build_transcript(&messages, "sess-1", &opts(tmp.path(), tmp.path()));
        assert_eq!(lines.len(), 2);

        let first: Value = serde_json::from_str(&lines[0]).unwrap();
        let second: Value = serde_json::from_str(&lines[1]).unwrap();

        assert!(first["parentUuid"].is_null());
        assert_eq!(second["parentUuid"], first["uuid"]);
        assert_ne!(first["uuid"], second["uuid"]);

        assert_eq!(first["type"], "user");
        assert_eq!(first["sessionId"], "sess-1");
        assert_eq!(first["timestamp"], "2024-01-01T00:00:00Z");
        assert_eq!(first["message"]["content"][0]["text"], "hello");

        assert_eq!(second["type"], "assistant");
        assert_eq!(second["message"]["model"], "<synthetic>");
        assert_eq!(second["message"]["content"][0]["text"], "hi there");
    }

    #[test]
    fn test_build_transcript_carries_optional_metadata() {
        let tmp = TempDir::new().unwrap();
        let messages = vec![msg("user", "hello", None)];

        let lines = build_transcript(&messages, "sess-1", &opts(tmp.path(), tmp.path()));
        let entry: Value = serde_json::from_str(&lines[0]).unwrap();
        assert!(entry.get("gitBranch").is_none());
        assert!(entry.get("version").is_none());

        let mut with_meta = opts(tmp.path(), tmp.path());
        with_meta.git_branch = Some("main".into());
        with_meta.version = Some("2.1.246".into());
        let lines = build_transcript(&messages, "sess-1", &with_meta);
        let entry: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(entry["gitBranch"], "main");
        assert_eq!(entry["version"], "2.1.246");
    }

    #[test]
    fn test_revive_session_writes_file() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("src").join("myrepo");
        let claude_dir = tmp.path().join("claude");
        let messages = vec![
            msg("user", "hello", Some("2024-01-01T00:00:00Z")),
            msg("assistant", "hi", Some("2024-01-01T00:00:01Z")),
        ];

        let result = revive_session(&messages, &opts(&cwd, &claude_dir)).unwrap();
        assert_eq!(result.message_count, 2);
        assert!(result.path.exists());
        assert_eq!(
            result.path.file_name().unwrap().to_string_lossy(),
            format!("{}.jsonl", result.session_id)
        );
        assert_eq!(
            result.path.parent().unwrap(),
            claude_dir.join("projects").join(encode_project_dir(&cwd))
        );

        let contents = std::fs::read_to_string(&result.path).unwrap();
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            let entry: Value = serde_json::from_str(line).unwrap();
            assert_eq!(entry["sessionId"], result.session_id.as_str());
            assert_eq!(entry["cwd"], cwd.to_string_lossy().as_ref());
            assert_eq!(entry["isSidechain"], false);
        }
    }

    #[test]
    fn test_revive_session_fresh_id_never_overwrites() {
        let tmp = TempDir::new().unwrap();
        let cwd = tmp.path().join("repo");
        let claude_dir = tmp.path().join("claude");
        let messages = vec![msg("user", "hello", None)];

        let first = revive_session(&messages, &opts(&cwd, &claude_dir)).unwrap();
        let second = revive_session(&messages, &opts(&cwd, &claude_dir)).unwrap();
        assert_ne!(first.session_id, second.session_id);
        assert!(first.path.exists() && second.path.exists());
    }

    #[test]
    fn test_revive_session_errors_when_nothing_revivable() {
        let tmp = TempDir::new().unwrap();
        let messages = vec![msg("assistant", "[tool_use: Bash] {}", None)];
        let err = revive_session(&messages, &opts(tmp.path(), tmp.path())).unwrap_err();
        assert!(err.to_string().contains("no revivable messages"));
    }
}
