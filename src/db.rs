use anyhow::{anyhow, Context, Result};
use libsql::{params, Builder, Connection, Database};
use std::path::Path;

/// Convert project directory names like "-home-murou-ghq-github-com-user-repo" to "user/repo"
pub fn format_project_name(raw: &str) -> String {
    let parts: Vec<&str> = raw.trim_start_matches('-').split('-').collect();

    // Try to find github-com pattern and extract user/repo
    for (i, part) in parts.iter().enumerate() {
        if *part == "github" && parts.get(i + 1) == Some(&"com") && i + 3 < parts.len() {
            let user = parts[i + 2];
            let repo = parts[i + 3..].join("-");
            if let Some(pos) = repo.find("--worktrees") {
                return format!("{user}/{}", &repo[..pos]);
            }
            return format!("{user}/{repo}");
        }
    }

    // Fallback: take last meaningful segments
    let meaningful: Vec<&str> = parts.iter().copied().filter(|p| !p.is_empty()).collect();
    if meaningful.len() > 2 {
        meaningful[meaningful.len() - 2..].join("/")
    } else {
        raw.to_string()
    }
}

/// Configuration for multi-device sync via a libSQL sync server (e.g. Turso or self-hosted sqld).
pub struct SyncOptions {
    pub url: String,
    pub auth_token: Option<String>,
}

/// An open vault database. In local mode this is a plain SQLite file; in sync mode
/// it is a libSQL embedded replica: reads are served from the local file, writes are
/// forwarded to the sync server, and `sync()` pulls remote changes into the replica.
pub struct Vault {
    db: Database,
    pub conn: Connection,
    sync_enabled: bool,
}

impl Vault {
    pub fn sync_enabled(&self) -> bool {
        self.sync_enabled
    }

    /// Pull the latest state from the sync server into the local replica.
    /// Returns the number of frames applied.
    pub async fn sync(&self) -> Result<u64> {
        if !self.sync_enabled {
            anyhow::bail!(
                "Sync is not enabled. Pass --sync-url (or set CLAUDE_VAULT_SYNC_URL) to use a synced database."
            );
        }
        let replicated = self
            .db
            .sync()
            .await
            .context("Failed to sync with the remote database")?;
        Ok(replicated.frames_synced() as u64)
    }

    /// Best-effort sync after local writes so the replica reflects them immediately.
    /// Failures are downgraded to a warning: the writes already reached the primary,
    /// and the next successful sync will catch the replica up.
    pub async fn sync_after_write(&self) {
        if self.sync_enabled {
            if let Err(e) = self.db.sync().await {
                eprintln!("Warning: post-write sync failed (will catch up on next sync): {e:#}");
            }
        }
    }
}

pub async fn open_vault(path: &Path, sync: Option<SyncOptions>) -> Result<Vault> {
    let (db, sync_enabled) = match sync {
        Some(opts) => {
            let db = Builder::new_remote_replica(
                path,
                opts.url.clone(),
                opts.auth_token.unwrap_or_default(),
            )
            .build()
            .await
            .with_context(|| {
                format!(
                    "Failed to open synced database {} (sync URL: {})",
                    path.display(),
                    opts.url
                )
            })?;
            (db, true)
        }
        None => {
            let db = Builder::new_local(path)
                .build()
                .await
                .with_context(|| format!("Failed to open database: {}", path.display()))?;
            (db, false)
        }
    };

    let conn = db.connect()?;
    let vault = Vault {
        db,
        conn,
        sync_enabled,
    };

    if vault.sync_enabled {
        // Pull the latest remote state before doing anything. Tolerate failure so
        // read commands still work offline against the (possibly stale) replica.
        if let Err(e) = vault.db.sync().await {
            eprintln!("Warning: could not sync with remote (using local replica): {e:#}");
        }
    } else {
        vault
            .conn
            .execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA busy_timeout=5000;
                 PRAGMA foreign_keys=ON;",
            )
            .await?;
    }

    // Skip DDL when the schema is already current. This matters in sync mode, where
    // DDL statements are forwarded to the remote primary on every invocation.
    if !schema_is_current(&vault.conn).await? {
        init_schema(&vault.conn).await?;
    }

    Ok(vault)
}

/// Fetch the first row of a query, erroring if there is none.
async fn query_row(
    conn: &Connection,
    sql: &str,
    params: impl libsql::params::IntoParams,
) -> Result<libsql::Row> {
    let mut rows = conn.query(sql, params).await?;
    rows.next()
        .await?
        .ok_or_else(|| anyhow!("Query returned no rows"))
}

async fn schema_is_current(conn: &Connection) -> Result<bool> {
    let row = query_row(
        conn,
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='sessions'),
                (SELECT sql FROM sqlite_master WHERE type='table' AND name='messages_fts')",
        (),
    )
    .await?;
    let has_sessions: i64 = row.get(0)?;
    let fts_sql: Option<String> = row.get(1)?;
    Ok(has_sessions == 1 && fts_sql.is_some_and(|sql| sql.contains("porter")))
}

async fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS sessions (
            session_id TEXT PRIMARY KEY,
            project    TEXT NOT NULL,
            started_at TEXT,
            imported_at TEXT NOT NULL DEFAULT (datetime('now'))
        );

        CREATE TABLE IF NOT EXISTS messages (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id TEXT NOT NULL,
            uuid       TEXT,
            role       TEXT NOT NULL,
            content    TEXT NOT NULL,
            timestamp  TEXT,
            FOREIGN KEY (session_id) REFERENCES sessions(session_id)
        );

        CREATE UNIQUE INDEX IF NOT EXISTS idx_messages_uuid
            ON messages(uuid) WHERE uuid IS NOT NULL;
        ",
    )
    .await?;

    // Create FTS table with Porter stemming, or migrate from old schema
    migrate_fts(conn).await?;

    conn.execute_batch(
        "
        CREATE TRIGGER IF NOT EXISTS messages_ai AFTER INSERT ON messages BEGIN
            INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
        END;

        CREATE TRIGGER IF NOT EXISTS messages_ad AFTER DELETE ON messages BEGIN
            INSERT INTO messages_fts(messages_fts, rowid, content) VALUES('delete', old.id, old.content);
        END;

        CREATE TRIGGER IF NOT EXISTS messages_au AFTER UPDATE ON messages BEGIN
            INSERT INTO messages_fts(messages_fts, rowid, content) VALUES('delete', old.id, old.content);
            INSERT INTO messages_fts(rowid, content) VALUES (new.id, new.content);
        END;
        ",
    )
    .await?;
    Ok(())
}

/// Ensure the FTS table uses Porter stemming tokenizer.
/// Migrates from the old tokenizer if needed.
async fn migrate_fts(conn: &Connection) -> Result<()> {
    let fts_exists: bool = {
        let row = query_row(
            conn,
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='messages_fts')",
            (),
        )
        .await?;
        row.get::<i64>(0)? == 1
    };

    if fts_exists {
        // Check if the FTS table already uses porter tokenizer
        let row = query_row(
            conn,
            "SELECT sql FROM sqlite_master WHERE type='table' AND name='messages_fts'",
            (),
        )
        .await?;
        let create_sql: String = row.get(0)?;

        if create_sql.contains("porter") {
            return Ok(());
        }

        // Old schema — drop and recreate with porter tokenizer
        eprintln!("Migrating FTS index to Porter stemming tokenizer...");
        conn.execute_batch(
            "
            DROP TRIGGER IF EXISTS messages_ai;
            DROP TRIGGER IF EXISTS messages_ad;
            DROP TRIGGER IF EXISTS messages_au;
            DROP TABLE messages_fts;
            ",
        )
        .await?;
    }

    conn.execute_batch(
        "
        CREATE VIRTUAL TABLE messages_fts USING fts5(
            content,
            content_rowid='id',
            content='messages',
            tokenize='porter unicode61'
        );
        ",
    )
    .await?;

    // If migrating from old schema, rebuild the index from existing messages
    if fts_exists {
        conn.execute_batch("INSERT INTO messages_fts(messages_fts) VALUES('rebuild')")
            .await?;
        eprintln!("FTS index rebuilt with Porter stemming.");
    }

    Ok(())
}

pub async fn upsert_session(
    conn: &Connection,
    session_id: &str,
    project: &str,
    started_at: Option<&str>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO sessions (session_id, project, started_at)
         VALUES (?1, ?2, ?3)
         ON CONFLICT(session_id) DO UPDATE SET
            project = excluded.project,
            started_at = COALESCE(excluded.started_at, sessions.started_at)",
        params![session_id, project, started_at],
    )
    .await?;
    Ok(())
}

pub async fn insert_message(
    conn: &Connection,
    session_id: &str,
    uuid: Option<&str>,
    role: &str,
    content: &str,
    timestamp: Option<&str>,
) -> Result<bool> {
    // The unique index on uuid makes OR IGNORE skip duplicates in a single
    // statement — one round trip instead of a SELECT-then-INSERT pair, which
    // matters in sync mode where writes are forwarded to the remote primary.
    let inserted = conn
        .execute(
            "INSERT OR IGNORE INTO messages (session_id, uuid, role, content, timestamp)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![session_id, uuid, role, content, timestamp],
        )
        .await?;
    Ok(inserted > 0)
}

pub struct SearchResult {
    pub session_id: String,
    pub project: String,
    pub role: String,
    pub content: String,
    pub timestamp: Option<String>,
}

/// Escape a query string for safe use in FTS5 MATCH.
/// Wraps each token in double quotes to prevent FTS5 operator interpretation.
fn escape_fts_query(query: &str) -> String {
    // If the user already used explicit FTS5 syntax (AND, OR, NOT, quotes), pass through
    if query.contains('"')
        || query.contains(" AND ")
        || query.contains(" OR ")
        || query.contains(" NOT ")
    {
        return query.to_string();
    }
    // Otherwise, quote each whitespace-separated token
    query
        .split_whitespace()
        .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Normalize a project filter so that "user/repo" also matches raw dir names like
/// "-home-foo-ghq-github-com-user-repo". Replaces "/" with "-" for LIKE matching.
fn normalize_project_filter(filter: &str) -> String {
    let normalized = filter.replace('/', "-");
    format!("%{normalized}%")
}

fn limit_param(limit: usize) -> i64 {
    // SQLite treats a negative LIMIT as "no limit"
    i64::try_from(limit).unwrap_or(-1)
}

pub async fn search(
    conn: &Connection,
    query: &str,
    limit: usize,
    project_filter: Option<&str>,
    role_filter: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<SearchResult>> {
    let escaped = escape_fts_query(query);
    if escaped.is_empty() {
        return Ok(vec![]);
    }

    let mut sql = String::from(
        "SELECT m.session_id, s.project, m.role, m.content, m.timestamp
         FROM messages_fts f
         JOIN messages m ON m.id = f.rowid
         JOIN sessions s ON s.session_id = m.session_id
         WHERE messages_fts MATCH ?1",
    );
    let mut param_idx = 2;

    if project_filter.is_some() {
        sql.push_str(&format!(" AND s.project LIKE ?{param_idx}"));
        param_idx += 1;
    }
    if role_filter.is_some() {
        sql.push_str(&format!(" AND m.role = ?{param_idx}"));
        param_idx += 1;
    }
    if since.is_some() {
        sql.push_str(&format!(" AND m.timestamp >= ?{param_idx}"));
        param_idx += 1;
    }
    if until.is_some() {
        sql.push_str(&format!(" AND m.timestamp <= ?{param_idx}"));
        param_idx += 1;
    }
    sql.push_str(&format!(" ORDER BY rank LIMIT ?{param_idx}"));

    let mut params_vec: Vec<libsql::Value> = vec![escaped.into()];
    if let Some(proj) = project_filter {
        params_vec.push(normalize_project_filter(proj).into());
    }
    if let Some(role) = role_filter {
        params_vec.push(role.to_string().into());
    }
    if let Some(s) = since {
        params_vec.push(s.to_string().into());
    }
    if let Some(u) = until {
        params_vec.push(u.to_string().into());
    }
    params_vec.push(limit_param(limit).into());

    let mut rows = conn.query(&sql, params_vec).await?;
    let mut results = Vec::new();
    while let Some(row) = rows.next().await? {
        results.push(SearchResult {
            session_id: row.get(0)?,
            project: row.get(1)?,
            role: row.get(2)?,
            content: row.get(3)?,
            timestamp: row.get(4)?,
        });
    }

    Ok(results)
}

pub async fn stats(conn: &Connection) -> Result<(i64, i64)> {
    let session_count: i64 = query_row(conn, "SELECT COUNT(*) FROM sessions", ())
        .await?
        .get(0)?;
    let message_count: i64 = query_row(conn, "SELECT COUNT(*) FROM messages", ())
        .await?
        .get(0)?;
    Ok((session_count, message_count))
}

/// Resolve a session ID prefix to a full session ID.
/// Returns an error if the prefix matches zero or multiple sessions.
pub async fn resolve_session_id(conn: &Connection, prefix: &str) -> Result<String> {
    let mut rows = conn
        .query(
            "SELECT session_id FROM sessions WHERE session_id LIKE ?1 || '%'",
            params![prefix],
        )
        .await?;
    let mut matches: Vec<String> = Vec::new();
    while let Some(row) = rows.next().await? {
        matches.push(row.get(0)?);
    }

    match matches.len() {
        0 => anyhow::bail!("No session found matching: {prefix}"),
        1 => Ok(matches.into_iter().next().unwrap()),
        n => {
            let previews: Vec<String> = matches.iter().take(5).cloned().collect();
            anyhow::bail!(
                "Ambiguous prefix '{prefix}' matches {n} sessions:\n  {}",
                previews.join("\n  ")
            );
        }
    }
}

pub async fn get_session_messages(
    conn: &Connection,
    session_id: &str,
) -> Result<Vec<SearchResult>> {
    let mut rows = conn
        .query(
            "SELECT m.session_id, s.project, m.role, m.content, m.timestamp
             FROM messages m
             JOIN sessions s ON s.session_id = m.session_id
             WHERE m.session_id = ?1
             ORDER BY m.id ASC",
            params![session_id],
        )
        .await?;

    let mut results = Vec::new();
    while let Some(row) = rows.next().await? {
        results.push(SearchResult {
            session_id: row.get(0)?,
            project: row.get(1)?,
            role: row.get(2)?,
            content: row.get(3)?,
            timestamp: row.get(4)?,
        });
    }

    Ok(results)
}

/// Get the Nth most recent session ID (0-indexed).
pub async fn nth_recent_session_id(conn: &Connection, n: usize) -> Result<String> {
    let mut rows = conn
        .query(
            "SELECT session_id FROM sessions
             ORDER BY COALESCE(started_at, imported_at) DESC
             LIMIT 1 OFFSET ?1",
            params![n as i64],
        )
        .await?;
    match rows.next().await? {
        Some(row) => Ok(row.get(0)?),
        None => anyhow::bail!("No session found at position {}", n + 1),
    }
}

pub struct SessionSummary {
    pub session_id: String,
    pub project: String,
    pub started_at: Option<String>,
    pub message_count: i64,
    pub first_user_message: Option<String>,
}

pub async fn list_sessions(
    conn: &Connection,
    limit: usize,
    project_filter: Option<&str>,
    since: Option<&str>,
    until: Option<&str>,
) -> Result<Vec<SessionSummary>> {
    let mut sql = String::from(
        "SELECT s.session_id, s.project, s.started_at,
                (SELECT COUNT(*) FROM messages m WHERE m.session_id = s.session_id) as msg_count,
                NULL as first_msg
         FROM sessions s",
    );
    let mut conditions = Vec::new();
    let mut param_idx = 1;
    if project_filter.is_some() {
        conditions.push(format!("s.project LIKE ?{param_idx}"));
        param_idx += 1;
    }
    if since.is_some() {
        conditions.push(format!(
            "COALESCE(s.started_at, s.imported_at) >= ?{param_idx}"
        ));
        param_idx += 1;
    }
    if until.is_some() {
        conditions.push(format!(
            "COALESCE(s.started_at, s.imported_at) <= ?{param_idx}"
        ));
        param_idx += 1;
    }
    if !conditions.is_empty() {
        sql.push_str(" WHERE ");
        sql.push_str(&conditions.join(" AND "));
    }
    sql.push_str(&format!(
        " ORDER BY COALESCE(s.started_at, s.imported_at) DESC LIMIT ?{param_idx}"
    ));

    let mut params_vec: Vec<libsql::Value> = Vec::new();
    if let Some(proj) = project_filter {
        params_vec.push(normalize_project_filter(proj).into());
    }
    if let Some(s) = since {
        params_vec.push(s.to_string().into());
    }
    if let Some(u) = until {
        params_vec.push(u.to_string().into());
    }
    params_vec.push(limit_param(limit).into());

    let mut rows = conn.query(&sql, params_vec).await?;
    let mut sessions: Vec<SessionSummary> = Vec::new();
    while let Some(row) = rows.next().await? {
        sessions.push(SessionSummary {
            session_id: row.get(0)?,
            project: row.get(1)?,
            started_at: row.get(2)?,
            message_count: row.get(3)?,
            first_user_message: None,
        });
    }

    // Fetch first meaningful user message for each session
    for session in &mut sessions {
        let mut candidates: Vec<String> = Vec::new();
        let mut rows = conn
            .query(
                "SELECT substr(content, 1, 200) FROM messages
                 WHERE session_id = ?1 AND role = 'user'
                 ORDER BY id ASC LIMIT 10",
                params![session.session_id.as_str()],
            )
            .await?;
        while let Some(row) = rows.next().await? {
            candidates.push(row.get(0)?);
        }

        session.first_user_message = candidates.into_iter().find(|c| is_meaningful_preview(c));

        // If no meaningful user message found, try assistant messages
        if session.first_user_message.is_none() {
            let mut asst_candidates: Vec<String> = Vec::new();
            let mut rows = conn
                .query(
                    "SELECT substr(content, 1, 200) FROM messages
                     WHERE session_id = ?1 AND role = 'assistant'
                     ORDER BY id ASC LIMIT 3",
                    params![session.session_id.as_str()],
                )
                .await?;
            while let Some(row) = rows.next().await? {
                asst_candidates.push(row.get(0)?);
            }
            session.first_user_message = asst_candidates
                .into_iter()
                .find(|c| is_meaningful_preview(c));
        }
    }

    Ok(sessions)
}

/// Check if a message is suitable as a session preview in `recent`.
/// Skips tool_result artifacts and system meta-messages.
fn is_meaningful_preview(content: &str) -> bool {
    let trimmed = content.trim();
    if trimmed.len() < 5 {
        return false;
    }
    // Skip messages starting with JSON/XML/path characters (likely tool output)
    let first_char = trimmed.chars().next().unwrap_or(' ');
    if matches!(first_char, '{' | '[' | '<') {
        return false;
    }
    // System/meta messages that are never human input
    let noise_prefixes = [
        "Tool loaded",
        "This session is being continued",
        "Your task is to create a detailed summary",
    ];
    for prefix in &noise_prefixes {
        if trimmed.starts_with(prefix) {
            return false;
        }
    }
    true
}

pub async fn delete_session(conn: &Connection, session_id: &str) -> Result<u64> {
    let msg_deleted = conn
        .execute(
            "DELETE FROM messages WHERE session_id = ?1",
            params![session_id],
        )
        .await?;
    conn.execute(
        "DELETE FROM sessions WHERE session_id = ?1",
        params![session_id],
    )
    .await?;
    Ok(msg_deleted)
}

pub async fn verify(conn: &Connection) -> Result<()> {
    println!("=== 1. Messages by role ===");
    let mut rows = conn
        .query("SELECT role, COUNT(*) FROM messages GROUP BY role", ())
        .await?;
    while let Some(row) = rows.next().await? {
        let role: String = row.get(0)?;
        let count: i64 = row.get(1)?;
        println!("  {role}: {count}");
    }

    println!("\n=== 2. Sessions and projects ===");
    let session_count: i64 = query_row(conn, "SELECT COUNT(*) FROM sessions", ())
        .await?
        .get(0)?;
    let project_count: i64 = query_row(conn, "SELECT COUNT(DISTINCT project) FROM sessions", ())
        .await?
        .get(0)?;
    println!("  Sessions: {session_count}, Projects: {project_count}");

    println!("\n=== 3. Top 5 projects by message count ===");
    let mut rows = conn
        .query(
            "SELECT s.project, COUNT(*) as cnt FROM messages m
             JOIN sessions s ON s.session_id = m.session_id
             GROUP BY s.project ORDER BY cnt DESC LIMIT 5",
            (),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        let proj: String = row.get(0)?;
        let count: i64 = row.get(1)?;
        println!("  {}: {count}", format_project_name(&proj));
    }

    println!("\n=== 4. FTS index integrity ===");
    let msg_count: i64 = query_row(conn, "SELECT COUNT(*) FROM messages", ())
        .await?
        .get(0)?;
    let fts_count: i64 = query_row(conn, "SELECT COUNT(*) FROM messages_fts", ())
        .await?
        .get(0)?;
    let fts_ok = msg_count == fts_count;
    println!("  messages: {msg_count} rows");
    println!("  messages_fts: {fts_count} rows");
    println!("  Match: {}", if fts_ok { "OK" } else { "FAIL" });

    println!("\n=== 5. UUID deduplication ===");
    let dup_count: i64 = query_row(
        conn,
        "SELECT COUNT(*) FROM (SELECT uuid, COUNT(*) as cnt FROM messages WHERE uuid IS NOT NULL GROUP BY uuid HAVING cnt > 1)",
        (),
    )
    .await?
    .get(0)?;
    let dup_ok = dup_count == 0;
    println!(
        "  Duplicate UUIDs: {dup_count} {}",
        if dup_ok { "OK" } else { "FAIL" }
    );

    println!("\n=== 6. Recent message samples ===");
    let mut rows = conn
        .query(
            "SELECT m.role, substr(m.content, 1, 120), m.timestamp
             FROM messages m JOIN sessions s ON s.session_id = m.session_id
             ORDER BY m.timestamp DESC LIMIT 10",
            (),
        )
        .await?;
    while let Some(row) = rows.next().await? {
        let role: String = row.get(0)?;
        let content: String = row.get(1)?;
        let ts: Option<String> = row.get(2)?;
        let content = content.replace('\n', " ");
        println!("  [{role}] {} | {content}", ts.as_deref().unwrap_or("?"));
    }

    println!("\n=== 7. FTS5 search test ===");
    for query in ["import", "SQLite", "cargo"] {
        let count: i64 = query_row(
            conn,
            "SELECT COUNT(*) FROM messages_fts WHERE messages_fts MATCH ?1",
            params![query],
        )
        .await?
        .get(0)?;
        println!("  \"{query}\": {count} hits");
    }

    if !fts_ok || !dup_ok {
        anyhow::bail!("Verification failed");
    }
    println!("\nAll checks passed.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn setup_db() -> (Vault, TempDir) {
        let tmp = TempDir::new().unwrap();
        let vault = open_vault(&tmp.path().join("test.db"), None).await.unwrap();
        (vault, tmp)
    }

    #[tokio::test]
    async fn test_open_and_init() {
        let (_vault, _tmp) = setup_db().await;
    }

    #[tokio::test]
    async fn test_reopen_existing_db() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.db");
        {
            let vault = open_vault(&path, None).await.unwrap();
            upsert_session(&vault.conn, "s1", "proj", None)
                .await
                .unwrap();
        }
        // Second open must detect the existing schema and skip re-init
        let vault = open_vault(&path, None).await.unwrap();
        let (sessions, _) = stats(&vault.conn).await.unwrap();
        assert_eq!(sessions, 1);
    }

    #[tokio::test]
    async fn test_upsert_session() {
        let (vault, _tmp) = setup_db().await;
        let conn = &vault.conn;
        upsert_session(conn, "sess-1", "my-project", Some("2024-01-01T00:00:00Z"))
            .await
            .unwrap();
        upsert_session(conn, "sess-1", "my-project", None)
            .await
            .unwrap();

        let project: String = query_row(
            conn,
            "SELECT project FROM sessions WHERE session_id = 'sess-1'",
            (),
        )
        .await
        .unwrap()
        .get(0)
        .unwrap();
        assert_eq!(project, "my-project");
    }

    #[tokio::test]
    async fn test_insert_and_search() {
        let (vault, _tmp) = setup_db().await;
        let conn = &vault.conn;
        upsert_session(conn, "sess-1", "proj", None).await.unwrap();
        insert_message(
            conn,
            "sess-1",
            Some("u1"),
            "user",
            "hello world",
            Some("2024-01-01T00:00:00Z"),
        )
        .await
        .unwrap();
        insert_message(
            conn,
            "sess-1",
            Some("u2"),
            "assistant",
            "hi there",
            Some("2024-01-01T00:00:01Z"),
        )
        .await
        .unwrap();

        let results = search(conn, "hello", 10, None, None, None, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].role, "user");
        assert_eq!(results[0].content, "hello world");
    }

    #[tokio::test]
    async fn test_porter_stemming() {
        let (vault, _tmp) = setup_db().await;
        let conn = &vault.conn;
        upsert_session(conn, "s1", "proj", None).await.unwrap();
        insert_message(
            conn,
            "s1",
            Some("u1"),
            "user",
            "the server is running fine",
            None,
        )
        .await
        .unwrap();
        insert_message(
            conn,
            "s1",
            Some("u2"),
            "user",
            "configure the database settings",
            None,
        )
        .await
        .unwrap();

        // "run" should match "running" via stemming
        let results = search(conn, "run", 10, None, None, None, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("running"));

        // "configuration" should match "configure" via stemming
        let results = search(conn, "configuration", 10, None, None, None, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("configure"));
    }

    #[tokio::test]
    async fn test_duplicate_uuid_skipped() {
        let (vault, _tmp) = setup_db().await;
        let conn = &vault.conn;
        upsert_session(conn, "sess-1", "proj", None).await.unwrap();
        let inserted = insert_message(conn, "sess-1", Some("u1"), "user", "first", None)
            .await
            .unwrap();
        assert!(inserted);
        let inserted = insert_message(conn, "sess-1", Some("u1"), "user", "duplicate", None)
            .await
            .unwrap();
        assert!(!inserted);

        let (_, msg_count) = stats(conn).await.unwrap();
        assert_eq!(msg_count, 1);
    }

    #[tokio::test]
    async fn test_stats() {
        let (vault, _tmp) = setup_db().await;
        let conn = &vault.conn;
        upsert_session(conn, "s1", "p1", None).await.unwrap();
        upsert_session(conn, "s2", "p2", None).await.unwrap();
        insert_message(conn, "s1", Some("u1"), "user", "msg1", None)
            .await
            .unwrap();
        insert_message(conn, "s1", Some("u2"), "assistant", "msg2", None)
            .await
            .unwrap();
        insert_message(conn, "s2", Some("u3"), "user", "msg3", None)
            .await
            .unwrap();

        let (sessions, messages) = stats(conn).await.unwrap();
        assert_eq!(sessions, 2);
        assert_eq!(messages, 3);
    }

    #[tokio::test]
    async fn test_search_no_results() {
        let (vault, _tmp) = setup_db().await;
        let conn = &vault.conn;
        upsert_session(conn, "s1", "proj", None).await.unwrap();
        insert_message(conn, "s1", Some("u1"), "user", "hello", None)
            .await
            .unwrap();

        let results = search(conn, "nonexistent", 10, None, None, None, None)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_search_with_date_filter() {
        let (vault, _tmp) = setup_db().await;
        let conn = &vault.conn;
        upsert_session(conn, "s1", "proj", None).await.unwrap();
        insert_message(
            conn,
            "s1",
            Some("u1"),
            "user",
            "early message",
            Some("2024-01-01T00:00:00Z"),
        )
        .await
        .unwrap();
        insert_message(
            conn,
            "s1",
            Some("u2"),
            "user",
            "late message",
            Some("2024-06-01T00:00:00Z"),
        )
        .await
        .unwrap();

        let results = search(conn, "message", 10, None, None, Some("2024-03-01"), None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("late"));

        let results = search(conn, "message", 10, None, None, None, Some("2024-03-01"))
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("early"));
    }

    #[tokio::test]
    async fn test_search_empty_query() {
        let (vault, _tmp) = setup_db().await;
        let conn = &vault.conn;
        upsert_session(conn, "s1", "proj", None).await.unwrap();
        insert_message(conn, "s1", Some("u1"), "user", "hello", None)
            .await
            .unwrap();

        let results = search(conn, "", 10, None, None, None, None).await.unwrap();
        assert!(results.is_empty());

        let results = search(conn, "   ", 10, None, None, None, None)
            .await
            .unwrap();
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn test_project_filter_with_slash() {
        let (vault, _tmp) = setup_db().await;
        let conn = &vault.conn;
        upsert_session(conn, "s1", "-home-user-ghq-github-com-owner-repo", None)
            .await
            .unwrap();
        insert_message(conn, "s1", Some("u1"), "user", "test msg", None)
            .await
            .unwrap();

        // Filter with formatted name "owner/repo"
        let results = search(conn, "test", 10, Some("owner/repo"), None, None, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);

        // Filter with just "repo"
        let results = search(conn, "test", 10, Some("repo"), None, None, None)
            .await
            .unwrap();
        assert_eq!(results.len(), 1);
    }

    #[tokio::test]
    async fn test_delete_session() {
        let (vault, _tmp) = setup_db().await;
        let conn = &vault.conn;
        upsert_session(conn, "s1", "proj", None).await.unwrap();
        insert_message(conn, "s1", Some("u1"), "user", "msg1", None)
            .await
            .unwrap();
        insert_message(conn, "s1", Some("u2"), "assistant", "msg2", None)
            .await
            .unwrap();

        let deleted = delete_session(conn, "s1").await.unwrap();
        assert_eq!(deleted, 2);

        let (sessions, messages) = stats(conn).await.unwrap();
        assert_eq!(sessions, 0);
        assert_eq!(messages, 0);
    }

    #[tokio::test]
    async fn test_list_sessions_with_date_filter() {
        let (vault, _tmp) = setup_db().await;
        let conn = &vault.conn;
        upsert_session(conn, "s1", "proj", Some("2024-01-15T00:00:00Z"))
            .await
            .unwrap();
        upsert_session(conn, "s2", "proj", Some("2024-06-15T00:00:00Z"))
            .await
            .unwrap();
        insert_message(conn, "s1", Some("u1"), "user", "old session", None)
            .await
            .unwrap();
        insert_message(conn, "s2", Some("u2"), "user", "new session", None)
            .await
            .unwrap();

        let sessions = list_sessions(conn, 100, None, Some("2024-03-01"), None)
            .await
            .unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "s2");

        let sessions = list_sessions(conn, 100, None, None, Some("2024-03-01"))
            .await
            .unwrap();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "s1");
    }

    #[tokio::test]
    async fn test_insert_without_uuid() {
        let (vault, _tmp) = setup_db().await;
        let conn = &vault.conn;
        upsert_session(conn, "s1", "proj", None).await.unwrap();
        let inserted = insert_message(conn, "s1", None, "user", "msg1", None)
            .await
            .unwrap();
        assert!(inserted);
        // Without uuid, duplicate check is skipped, so second insert also succeeds
        let inserted = insert_message(conn, "s1", None, "user", "msg2", None)
            .await
            .unwrap();
        assert!(inserted);

        let (_, count) = stats(conn).await.unwrap();
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn test_sync_disabled_on_local_vault() {
        let (vault, _tmp) = setup_db().await;
        assert!(!vault.sync_enabled());
        assert!(vault.sync().await.is_err());
    }
}
