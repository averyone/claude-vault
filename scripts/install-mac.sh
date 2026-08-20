#!/bin/zsh
# claude-vault macOS installer
#
# One-shot setup for claude-vault with multi-device sync (Turso):
# toolchain (Xcode CLT, rustup, cargo), Turso CLI + database, environment
# variables, build/install, initial import, and Claude Code hook wiring.
#
# Safe to re-run: every step checks before it acts.
#
# Usage:  ./scripts/install-mac.sh

set -e
set -o pipefail

DB_NAME="claude-vault"
ZSHRC="$HOME/.zshrc"
CLAUDE_DIR="$HOME/.claude"
CLAUDE_SETTINGS="$CLAUDE_DIR/settings.json"
VAULT_DB="$HOME/Library/Application Support/claude-vault/vault.db"
# Repo root = parent of this script's directory
REPO_DIR="${0:A:h:h}"

banner() {
  echo ""
  echo "════════════════════════════════════════════════════════════════════"
  echo "<<$1>>"
  echo "════════════════════════════════════════════════════════════════════"
}

banner "STEP 1: INSTALL XCODE COMMAND LINE TOOLS"
if xcode-select -p >/dev/null 2>&1; then
  echo "Xcode Command Line Tools already installed at: $(xcode-select -p)"
else
  echo "Xcode Command Line Tools not found. Launching installer..."
  xcode-select --install
  echo ""
  echo "A GUI installer has opened. Complete it, then RE-RUN this script."
  exit 1
fi

banner "STEP 2: INSTALL RUSTUP"
if command -v rustup >/dev/null 2>&1; then
  echo "rustup already installed: $(rustup --version 2>/dev/null | head -1)"
else
  echo "rustup not found. Installing via rustup.rs..."
  curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
fi
# Make cargo/rustup available to this script and future shells
if ! grep -qF 'source "$HOME/.cargo/env"' "$ZSHRC" 2>/dev/null; then
  echo 'source "$HOME/.cargo/env"' >> "$ZSHRC"
  echo "Added 'source \$HOME/.cargo/env' to $ZSHRC"
fi
if [ -f "$HOME/.cargo/env" ]; then
  source "$HOME/.cargo/env"
  echo "Sourced \$HOME/.cargo/env into this shell."
fi

banner "STEP 3: VERIFY CARGO"
if command -v cargo >/dev/null 2>&1; then
  echo "cargo already installed: $(cargo --version)"
else
  echo "cargo not found. Installing the stable toolchain via rustup..."
  rustup toolchain install stable
  rustup default stable
  echo "cargo installed: $(cargo --version)"
fi

banner "STEP 4: INSTALL TURSO CLI"
if command -v turso >/dev/null 2>&1; then
  echo "turso already installed: $(turso --version 2>/dev/null | head -1)"
else
  echo "turso not found. Installing via get.tur.so..."
  curl -sSfL https://get.tur.so/install.sh | bash
  export PATH="$HOME/.turso:$PATH"
fi
# The turso CLI exits 0 even when logged out, printing an error to stdout,
# so login state must be detected from the output, not the exit code.
turso_logged_in() {
  local who
  who="$(turso auth whoami 2>/dev/null)"
  [[ -n "$who" && "$who" != *"not logged in"* ]]
}
if turso_logged_in; then
  echo "Already logged in to Turso as: $(turso auth whoami)"
else
  echo "Not logged in to Turso. Starting signup (a browser window will open;"
  echo "if you already have an account this simply logs you in)..."
  turso auth signup
  if ! turso_logged_in; then
    echo "ERROR: Turso login did not complete. Run 'turso auth login' and re-run this script." >&2
    exit 1
  fi
fi

banner "STEP 5: FIND OR CREATE THE '$DB_NAME' DATABASE"
if turso db show "$DB_NAME" --url 2>/dev/null | grep -q '^libsql://'; then
  echo "Database '$DB_NAME' already exists."
else
  echo "Database '$DB_NAME' not found. Creating it..."
  turso db create "$DB_NAME"
fi

banner "STEP 6: SET SYNC ENVIRONMENT VARIABLES IN ~/.zshrc"
SYNC_URL="$(turso db show "$DB_NAME" --url)"
if [[ "$SYNC_URL" != libsql://* ]]; then
  echo "ERROR: expected a libsql:// URL from 'turso db show', got: $SYNC_URL" >&2
  exit 1
fi
AUTH_TOKEN="$(turso db tokens create "$DB_NAME")"
if [[ -z "$AUTH_TOKEN" || "$AUTH_TOKEN" == *" "* ]]; then
  echo "ERROR: 'turso db tokens create' did not return a token, got: $AUTH_TOKEN" >&2
  exit 1
fi
echo "Sync URL: $SYNC_URL"
echo "Minted a fresh auth token (${#AUTH_TOKEN} chars)."
# Replace any existing entries, then append the current values
sed -i '' '/^export CLAUDE_VAULT_SYNC_URL=/d' "$ZSHRC" 2>/dev/null || true
sed -i '' '/^export CLAUDE_VAULT_AUTH_TOKEN=/d' "$ZSHRC" 2>/dev/null || true
{
  echo "export CLAUDE_VAULT_SYNC_URL=\"$SYNC_URL\""
  echo "export CLAUDE_VAULT_AUTH_TOKEN=\"$AUTH_TOKEN\""
} >> "$ZSHRC"
echo "Wrote CLAUDE_VAULT_SYNC_URL and CLAUDE_VAULT_AUTH_TOKEN to $ZSHRC"
export CLAUDE_VAULT_SYNC_URL="$SYNC_URL"
export CLAUDE_VAULT_AUTH_TOKEN="$AUTH_TOKEN"
set +e
source "$ZSHRC"
set -e
echo "Sourced $ZSHRC back into this shell."

banner "STEP 7: REMOVE ANY PREVIOUSLY INSTALLED CLAUDE-VAULT"
if cargo uninstall claude-vault >/dev/null 2>&1; then
  echo "Uninstalled previous claude-vault binary."
else
  echo "No previously installed claude-vault binary (nothing to uninstall)."
fi

banner "STEP 8: BUILD AND INSTALL CLAUDE-VAULT"
echo "Running cargo install --path $REPO_DIR (this can take a few minutes)..."
cargo install --path "$REPO_DIR"
echo "Installed: $(command -v claude-vault) ($(claude-vault --version))"

banner "STEP 9: IMPORT CONVERSATION HISTORY INTO THE SYNCED VAULT"
# A database created by an older (local-only) claude-vault is a plain SQLite
# file; sync mode must build its embedded-replica file fresh from the server.
# Move any such file aside — its contents are re-imported from ~/.claude below,
# and UUID dedup makes that safe.
if [ -f "$VAULT_DB" ] && [ ! -f "$VAULT_DB-info" ]; then
  BACKUP="$VAULT_DB.pre-sync-$(date +%Y%m%d%H%M%S).bak"
  echo "Existing local (non-replica) database found. Moving it aside:"
  echo "  $VAULT_DB -> $BACKUP"
  mv "$VAULT_DB" "$BACKUP"
  rm -f "$VAULT_DB-wal" "$VAULT_DB-shm"
fi
echo "Importing from ~/.claude/projects (first sync run may take a while)..."
claude-vault import
claude-vault stats

banner "STEP 10 & 11: WIRE SYNC INTO CLAUDE CODE SETTINGS"
mkdir -p "$CLAUDE_DIR"
if [ -f "$CLAUDE_SETTINGS" ]; then
  cp "$CLAUDE_SETTINGS" "$CLAUDE_SETTINGS.bak-$(date +%Y%m%d%H%M%S)"
  echo "Backed up existing settings.json."
fi
python3 - "$CLAUDE_SETTINGS" <<'PYEOF'
import json, os, sys

path = sys.argv[1]
url = os.environ["CLAUDE_VAULT_SYNC_URL"]
token = os.environ["CLAUDE_VAULT_AUTH_TOKEN"]

settings = {}
if os.path.exists(path):
    with open(path) as f:
        settings = json.load(f)

# Step 10: env vars for Claude Code sessions (hooks inherit these too)
env = settings.setdefault("env", {})
env["CLAUDE_VAULT_SYNC_URL"] = url
env["CLAUDE_VAULT_AUTH_TOKEN"] = token
print("Set env.CLAUDE_VAULT_SYNC_URL and env.CLAUDE_VAULT_AUTH_TOKEN")

# Step 11: auto-archive hooks with sync flags inlined
flags = f'--sync-url "{url}" --auth-token "{token}"'
desired = {
    "PreCompact": f"claude-vault {flags} import >/dev/null 2>&1",
    "SessionEnd": f"claude-vault {flags} import >/dev/null 2>&1 &",
}
hooks = settings.setdefault("hooks", {})
for event, command in desired.items():
    entries = hooks.setdefault(event, [])
    updated = False
    for entry in entries:
        for h in entry.get("hooks", []):
            cmd = h.get("command", "")
            if h.get("type") == "command" and cmd.lstrip().startswith("claude-vault") and "import" in cmd:
                h["command"] = command
                updated = True
                break
        if updated:
            break
    if updated:
        print(f"Updated existing {event} hook to include sync flags")
    else:
        entries.append({"hooks": [{"type": "command", "command": command}]})
        print(f"Added {event} auto-archive hook with sync flags")

with open(path, "w") as f:
    json.dump(settings, f, indent=2)
    f.write("\n")
print(f"Wrote {path}")
PYEOF

banner "DONE"
echo "claude-vault is installed with multi-device sync enabled."
echo ""
echo "  Binary:    $(command -v claude-vault)"
echo "  Database:  $VAULT_DB (embedded replica)"
echo "  Sync URL:  $SYNC_URL"
echo ""
echo "Open a new terminal (or 'source ~/.zshrc') to pick up the environment"
echo "variables. Claude Code sessions started from now on archive to the"
echo "shared vault automatically. Run this same script on any other Mac to"
echo "connect it to the same vault."
