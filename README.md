# claude-vault

[![CI](https://github.com/kuroko1t/claude-vault/actions/workflows/ci.yml/badge.svg)](https://github.com/kuroko1t/claude-vault/actions/workflows/ci.yml)
[![Crates.io](https://img.shields.io/crates/v/claude-vault.svg)](https://crates.io/crates/claude-vault)
[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](LICENSE)

Archive your Claude Code conversations into a searchable SQLite database. Single binary, zero dependencies.

## Why?

Claude Code stores session history as JSONL files under `~/.claude/projects/`, but these files have problems:

- **Files are deleted by default** — Old sessions are auto-deleted over time (`cleanupPeriodDays` setting). Even if you change this, the other issues remain.
- **Data loss on compact** — `/compact` compresses in-memory context, and the original conversation details are lost
- **Poor searchability** — JSONL files are scattered across directories with no cross-session search

claude-vault copies conversations into a durable SQLite database with full-text search — once archived, your history survives file deletion, compaction, and cleanup. Zero runtime dependencies, single binary.

## Demo

### List sessions

```
$ claude-vault list -n 5
ID         DATE                    MSGS  PROJECT                   PREVIEW
----------------------------------------------------------------------------------------------------
47cf1f2e   2026-03-13T14:21:48      555  user/my-project           How to auto-archive without manual steps?
a4d5aa81   2026-03-13T03:01:35      232  user/another-repo         Prepare README.md for OSS publishing
6b4b6b21   2026-03-13T00:40:44       81  user/experiment           Search for open-source tools and compare features
67e36ae8   2026-03-12T22:59:44      465  user/trading-bot          Implement daily strategy with backtesting
cd9851d6   2026-03-04T04:12:25      312  org/ml-pipeline           Fix Docker build for GPU training container

Export: claude-vault export <ID>
```

### Search conversations

```
$ claude-vault search "Docker"
[user] user/my-project | 84d8d116 | 2026-02-04T01:15:53Z
docker info | grep -A5 "Build"
  buildx: Docker Buildx (Docker Inc.)
    Version:  v0.28.0
---
[assistant] user/trading-bot | c86a3eec | 2026-02-09T23:40:43Z
The session is running inside Docker, so docker commands are not available.
Run the following on the host:
  docker compose -f docker-compose.safe.yml build --no-cache
```

### Revive a session

```
$ cd ~/src/my-project
$ claude-vault revive 47cf1f2e
Revived 47cf1f2e-9a3b-4c81-b0d2-5e7f1a2c8d40 (86 turns) into
  /home/user/.claude/projects/-home-user-src-my-project/9f3c1d20-4b7a-42e1-8c95-3d6f0b1e7a24.jsonl
Resume: claude --resume 9f3c1d20-4b7a-42e1-8c95-3d6f0b1e7a24
```

The session is now a normal Claude Code session — pick it up with `claude --resume`, or select it from `/resume`, and keep going.

### Database stats

```
$ claude-vault stats
Database: /home/user/.local/share/claude-vault/vault.db
Sessions: 178
Messages: 94562
```

## Features

- **FTS5 full-text search** with Porter stemming (e.g. "running" matches "run")
- **Multi-device sync** (optional) — share one vault across machines via libSQL embedded replicas (Turso or self-hosted sqld)
- **Automatic archiving** via Claude Code hooks (PreCompact + SessionEnd)
- **Noise filtering** — strips tool results, system tags, and meta messages
- **UUID deduplication** — safe to re-import; duplicates are skipped
- **Session export** — Markdown, JSON, or plain text
- **Session revive** — write an archived session back to disk as a resumable Claude Code transcript
- **Single binary** — no Python, Node.js, or other runtime required

## Install

### From GitHub Releases (recommended)

Download a prebuilt binary from [Releases](https://github.com/kuroko1t/claude-vault/releases):

```bash
# Linux x86_64
curl -fsSL https://github.com/kuroko1t/claude-vault/releases/latest/download/claude-vault-x86_64-unknown-linux-gnu.tar.gz | tar xz
sudo mv claude-vault /usr/local/bin/

# macOS Apple Silicon
curl -fsSL https://github.com/kuroko1t/claude-vault/releases/latest/download/claude-vault-aarch64-apple-darwin.tar.gz | tar xz
sudo mv claude-vault /usr/local/bin/
```

### From crates.io

```bash
cargo install claude-vault
```

### From source

```bash
cargo install --path .
```

### One-shot installers (with multi-device sync)

If you want the full setup — toolchain, Turso CLI and database, environment
variables, build, initial import, and Claude Code auto-archive hooks — run the
bundled installer for your platform instead of the manual steps:

```bash
# macOS
git clone https://github.com/kuroko1t/claude-vault.git
cd claude-vault
./scripts/install-mac.sh
```

```powershell
# Windows (the Turso CLI has no native Windows support, so the script
# provisions the database through WSL; claude-vault itself runs natively)
git clone https://github.com/kuroko1t/claude-vault.git
cd claude-vault
powershell -ExecutionPolicy Bypass -File .\scripts\install-windows.ps1

# Windows without WSL: builds the Turso CLI natively with Go, uses it just
# for login and API-token minting, and provisions over the Platform REST API
powershell -ExecutionPolicy Bypass -File .\scripts\install-windows.ps1 -NoWsl
```

The scripts are idempotent (every step checks before it acts), so they're safe
to re-run after a failure or to repair a partial setup. See
[Multi-device sync](#multi-device-sync) for what they configure.

## Quick Start

1. Import all existing conversations from `~/.claude/projects/`:

```bash
claude-vault import
# Imported 94562 messages (0 skipped, 12847 filtered, 0 errors) from 203 files
```

2. Search your history:

```bash
claude-vault search "error handling"
```

3. Browse sessions:

```bash
claude-vault list
```

4. Bring one back to life and keep working on it:

```bash
cd ~/src/my-project
claude-vault revive --last
# Resume: claude --resume 9f3c1d20-...
```

5. (Optional) Set up auto-archiving — see [Hooks Setup](#auto-archive-with-claude-code-hooks).

## Using from Claude Code

Search past conversations directly from a Claude Code session:

```bash
# Keyword search
claude-vault search "previous Docker configuration"

# Structured output for Claude to parse
claude-vault search "auth bug" --json
claude-vault list --json
```

Or pull a whole conversation back into Claude Code and carry on with it:

```bash
claude-vault revive 47cf1f2e     # file it under the current directory
claude --resume <printed-id>     # ...and continue where it left off
```

With [auto-archiving hooks](#auto-archive-with-claude-code-hooks) configured, Claude Code can always search your full history — even for sessions whose JSONL files have been deleted. `revive` is what closes the loop: a session the vault outlived can become a live session again.

<details>
<summary><h2>Usage</h2></summary>

### import

Import all JSONL files from `~/.claude/projects/` recursively (including subagent directories):

```bash
claude-vault import
```

### import-file

Import a single JSONL session file:

```bash
claude-vault import-file /path/to/session.jsonl --project my-project
```

### search

```bash
claude-vault search "Docker"
claude-vault search "deploy" --project my-app
claude-vault search "deploy" --since 2024-01-01 --until 2024-06-30
claude-vault search '"error handling" AND rust'   # FTS5 syntax
claude-vault search "auth bug" --json              # machine-readable output
claude-vault search "retry" --role assistant       # only assistant messages
claude-vault search "retry" --limit 50             # default is 10
claude-vault search "cargo build" --include-tools  # show tool_use lines too
```

### export

Export a session to Markdown, JSON, or plain text. Accepts session ID prefixes (like git short hashes):

```bash
claude-vault export 47cf1f2e
claude-vault export --last                          # most recent session
claude-vault export --last 2                        # second most recent
claude-vault export --last --format markdown > session.md
claude-vault export 47cf1f2e --format json          # structured output
claude-vault export 47cf1f2e --format text          # flat plain text
```

`export` gives you the conversation as a document. To get it back into Claude Code as a session you can continue, use [`revive`](#revive) instead.

### revive

Write an archived session back into `~/.claude/projects/` as a JSONL transcript, so you can pick the conversation up in Claude Code and keep going. This is the inverse of `import`: where `import` pulls sessions off disk into the vault, `revive` puts one back.

The session is filed under the directory you run the command from — that directory is what Claude Code uses to decide which project a session belongs to:

```bash
cd ~/src/my-repo
claude-vault revive 47cf1f2e          # revive a specific session here
claude-vault revive --last            # the most recent session
claude-vault revive --last 3          # the third most recent
claude-vault revive 47cf --include-tools   # keep tool calls as text
claude-vault revive 47cf --cwd ~/src/other-repo   # file it elsewhere
```

```
Revived 47cf1f2e-9a3b-4c81-b0d2-5e7f1a2c8d40 (86 turns) into
  /home/user/.claude/projects/-home-user-src-my-repo/9f3c1d20-4b7a-42e1-8c95-3d6f0b1e7a24.jsonl
Resume: claude --resume 9f3c1d20-4b7a-42e1-8c95-3d6f0b1e7a24
```

Then continue the conversation:

```bash
claude --resume 9f3c1d20-4b7a-42e1-8c95-3d6f0b1e7a24
```

It also shows up in `/resume` inside Claude Code, listed under the project directory you revived it into.

#### What the revived transcript contains

The vault stores cleaned message text, not the full raw session, so a revived transcript is a faithful record of the *conversation* rather than a byte-for-byte copy of the original file. Specifically:

| Aspect | Behavior |
|--------|----------|
| Session ID | Always **fresh**, so reviving can never overwrite a session Claude Code already has on disk. The vault copy is untouched, and you can revive the same session more than once. |
| Tool calls | Archived without their results, so they're dropped by default — a tool call with no result is not a valid thing to replay. `--include-tools` keeps them as plain text. |
| Turns | Consecutive same-role messages are joined into one turn (and any assistant turns before the first user turn are dropped), so the file is a well-formed alternating conversation. This means the turn count is usually lower than the archived message count. |
| Metadata | `cwd` comes from the target directory; `gitBranch` and Claude Code `version` are detected best-effort and omitted if unavailable. Assistant turns are marked with Claude Code's `<synthetic>` model marker, the same one it uses for messages it injects itself. |
| Timestamps | Preserved from the original messages. |

Because tool calls and their results are gone, a revived session has the conversation history but not the original tool output. Claude re-reads files and re-runs commands as needed when you continue.

### list

```bash
claude-vault list
claude-vault list --project my-app --since 2024-01-01
claude-vault list --json
```

### Other commands

```bash
claude-vault delete 47cf -y                        # delete a session
claude-vault stats                                  # show database statistics
claude-vault verify                                 # check database integrity
claude-vault sync                                   # pull latest from sync server (see Multi-device sync)
claude-vault completions zsh > ~/.zfunc/_claude-vault  # shell completions
claude-vault --db /path/to/vault.db search "query"  # custom database path
```

</details>

## Auto-archive with Claude Code Hooks

Add to `~/.claude/settings.json`:

```json
{
  "hooks": {
    "PreCompact": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "claude-vault import >/dev/null 2>&1"
          }
        ]
      }
    ],
    "SessionEnd": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "claude-vault import >/dev/null 2>&1 &"
          }
        ]
      }
    ]
  }
}
```

| Hook | Timing | Mode |
|------|--------|------|
| **PreCompact** | Before `/compact` runs | Synchronous — captures full data before compression |
| **SessionEnd** | When a session ends | Background — non-blocking |

Once configured, conversations are archived automatically with no manual steps.

## Multi-device sync

Share one vault across machines using [libSQL embedded replicas](https://docs.turso.tech/features/embedded-replicas/introduction). Each machine keeps a full local copy of the database — reads and searches stay local and fast — while writes are forwarded to a sync server and replicated to every device.

> **Why not just put `vault.db` in iCloud/Dropbox?** File-sync services don't participate in SQLite's locking and sync the WAL files independently, which corrupts the database. Embedded replicas exist to solve exactly this.

### Quick setup (macOS / Windows)

The bundled installers do everything in this section for you — install the
toolchain and Turso CLI, create the database, set the environment variables,
build claude-vault, import your history, and wire up the Claude Code
auto-archive hooks:

```bash
# macOS
./scripts/install-mac.sh
```

```powershell
# Windows (add -NoWsl to provision without WSL, via a Go-built Turso CLI
# and the Platform REST API)
powershell -ExecutionPolicy Bypass -File .\scripts\install-windows.ps1
```

Run it on each machine you want connected to the same vault. It's idempotent,
so re-running is always safe. The manual equivalent follows.

### Installing the Turso CLI

```bash
# macOS (either works)
brew install tursodatabase/tap/turso
curl -sSfL https://get.tur.so/install.sh | bash

# Linux
curl -sSfL https://get.tur.so/install.sh | bash

# Windows: no official native binaries — run the same script inside WSL,
wsl -e sh -c 'curl -sSfL https://get.tur.so/install.sh | bash'
# or build the CLI natively with Go
go install github.com/tursodatabase/turso-cli/cmd/turso@latest
```

Then create an account (or log in — same command either way; it opens a
browser):

```bash
turso auth signup
```

Turso's free tier is more than enough for conversation archives. Note that the
hosted service holds a copy of your conversation history; if that's a concern,
self-host sqld instead (see below).

### Setup with Turso

```bash
# One-time: create a database and get credentials
turso db create claude-vault
turso db show claude-vault --url        # -> libsql://claude-vault-<org>.turso.io
turso db tokens create claude-vault     # -> auth token

# On every machine (e.g. in ~/.zshrc):
export CLAUDE_VAULT_SYNC_URL="libsql://claude-vault-<org>.turso.io"
export CLAUDE_VAULT_AUTH_TOKEN="<token>"
```

Tokens don't expire by default; pass `--expiration 30d` if you want rotation,
and revoke with `turso db tokens invalidate claude-vault` if one leaks.

That's it — all commands now operate on the synced database:

```bash
claude-vault import          # archives to the shared vault
claude-vault search "..."    # searches history from ALL machines
claude-vault sync            # manually pull the latest remote state
```

The flags `--sync-url` and `--auth-token` work as one-off alternatives to the environment variables. A self-hosted [sqld](https://github.com/tursodatabase/libsql/tree/main/libsql-server) server works the same way (`--sync-url http://your-server:8080`; token optional depending on your server config).

### Sync behavior

| Operation | Behavior |
|-----------|----------|
| Reads (search, list, export, revive) | Always local — served from the on-disk replica |
| Writes (import, delete) | Forwarded to the sync server, then reflected locally |
| On startup | Pulls the latest remote state; falls back to the local replica with a warning if offline |
| Offline | Reads work (possibly stale); writes require connectivity |

If you use the [auto-archive hooks](#auto-archive-with-claude-code-hooks), export the two environment variables somewhere the hook commands can see them (or add the flags to the hook commands directly), and every machine archives into the same vault. UUID deduplication makes concurrent imports from multiple machines safe.

Migrating an existing local vault: a database created in local mode is a plain SQLite file, and sync mode refuses to open it (`db file exists but metadata file does not`) — the embedded replica must be built fresh from the server. Move the old `vault.db` aside (the installer script does this automatically), then run `claude-vault import` once with sync enabled and your `~/.claude/projects` history is re-imported into the shared database. Sessions whose JSONL files are already gone can be carried over by exporting/re-importing, or seed the shared vault from the old file directly with `turso db create claude-vault --from-file vault.db`.

<details>
<summary><h2>How It Works</h2></summary>

```
~/.claude/projects/<project>/<session>.jsonl
        │
        ▼
    ┌─────────┐     ┌──────────┐     ┌───────────┐
    │  Parse   │────▶│  Filter  │────▶│  SQLite   │
    │  JSONL   │     │  & Clean │     │  + FTS5   │
    └─────────┘     └──────────┘     └───────────┘
                                           │
                                  revive   │
    ~/.claude/projects/<cwd>/  ◀───────────┘
      <new-session>.jsonl
```

1. **Parse** — Reads each JSONL record, extracts `user` and `assistant` messages
2. **Filter** — Removes system-injected noise (see below)
3. **Store** — Inserts into SQLite with UUID-based dedup; FTS5 index is updated via triggers
4. **Revive** (optional) — Rebuilds a stored session as a Claude Code transcript under a chosen directory, so it can be resumed and continued

### Noise Filtering

| Category | What's removed |
|----------|---------------|
| Tool results | All `tool_result` content (Bash output, file creation messages, web fetch results, etc.) |
| System tags | `<system-reminder>`, `<local-command-caveat>`, `<local-command-stdout>`, `<command-name>`, `<command-message>`, `<command-args>`, `<task-notification>` |
| Read-only tools | Read, Glob, Grep, LSP, ToolSearch, browser snapshot/navigation, TaskGet/TaskOutput/TaskList |
| Meta messages | eval-loop iterations/commands, Stop hook feedback, empty/whitespace-only content |

User text input, assistant responses, and code-modifying tool calls (Edit, Write, Bash, etc.) are preserved.

</details>

<details>
<summary><h2>Schema</h2></summary>

```sql
CREATE TABLE sessions (
    session_id  TEXT PRIMARY KEY,
    project     TEXT NOT NULL,
    started_at  TEXT,
    imported_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE messages (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL REFERENCES sessions(session_id),
    uuid       TEXT,
    role       TEXT NOT NULL,
    content    TEXT NOT NULL,
    timestamp  TEXT
);

CREATE UNIQUE INDEX idx_messages_uuid ON messages(uuid) WHERE uuid IS NOT NULL;

CREATE VIRTUAL TABLE messages_fts USING fts5(
    content, content_rowid='id', content='messages',
    tokenize='porter unicode61'
);
```

Default database location (platform-specific, via the OS data directory):

| Platform | Path |
|----------|------|
| Linux | `$XDG_DATA_HOME/claude-vault/vault.db` (usually `~/.local/share/...`) |
| macOS | `~/Library/Application Support/claude-vault/vault.db` |
| Windows | `%APPDATA%\claude-vault\vault.db` |

Override it with `--db <path>` or the `CLAUDE_VAULT_DB` environment variable.

In local mode, SQLite is configured with WAL mode and a 5-second busy timeout for safe concurrent access. In sync mode, the file is a libSQL embedded replica managed by the sync protocol.

</details>

## License

MIT
