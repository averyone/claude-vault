# claude-vault Windows installer
#
# One-shot setup for claude-vault with multi-device sync (Turso):
# toolchain (MSVC Build Tools, rustup, cargo), Turso CLI + database,
# environment variables, build/install, initial import, and Claude Code
# hook wiring.
#
# Platform notes (differences from install-mac.sh):
#   - MSVC Build Tools replace the Xcode Command Line Tools.
#   - The Turso CLI has no native Windows support; per Turso's docs it runs
#     under WSL. WSL is only needed for this one-time provisioning (login,
#     database creation, token minting) — claude-vault itself builds and
#     syncs natively on Windows.
#   - Environment variables are persisted per-user via the registry instead
#     of ~/.zshrc.
#   - settings.json is edited with PowerShell instead of python3.
#
# Safe to re-run: every step checks before it acts.
#
# Usage:  powershell -ExecutionPolicy Bypass -File .\scripts\install-windows.ps1
#         powershell -ExecutionPolicy Bypass -File .\scripts\install-windows.ps1 -NoWsl
#
# -NoWsl skips WSL entirely: the Turso CLI is built natively with Go (Turso
# ships no Windows binaries, but the CLI compiles fine) and used only for
# auth — a brief browser login and Platform API token minting — while
# database provisioning goes over the Platform REST API. Set
# $env:TURSO_API_TOKEN to skip the CLI steps entirely (e.g. a token minted
# on another machine or at https://app.turso.tech).

param([switch]$NoWsl)

$ErrorActionPreference = 'Stop'
# wsl.exe emits its own messages as UTF-16; this makes them UTF-8 so
# captured output is comparable as normal strings.
$env:WSL_UTF8 = '1'

$DbName = 'claude-vault'
$ClaudeDir = Join-Path $env:USERPROFILE '.claude'
$ClaudeSettings = Join-Path $ClaudeDir 'settings.json'
# Rust's dirs::data_dir() resolves to roaming AppData on Windows
$VaultDb = Join-Path $env:APPDATA 'claude-vault\vault.db'
# Repo root = parent of this script's directory
$RepoDir = Split-Path -Parent $PSScriptRoot

function Show-Banner([string]$Title) {
  Write-Host ''
  Write-Host ('=' * 68)
  Write-Host "<<$Title>>"
  Write-Host ('=' * 68)
}

function Assert-LastExitCode([string]$What) {
  if ($LASTEXITCODE -ne 0) {
    Write-Error "$What failed (exit code $LASTEXITCODE)."
  }
}

# turso CLI runner: the native Go-built binary in -NoWsl mode, otherwise
# inside WSL, where the CLI is officially supported.
function Invoke-Turso([string]$TursoArgs) {
  if ($NoWsl) {
    & turso ($TursoArgs -split ' ')
  } else {
    wsl -e sh -c ('PATH="$HOME/.turso:$PATH" turso ' + $TursoArgs)
  }
}

# The turso CLI exits 0 even when logged out, printing an error to stdout,
# so login state must be detected from the output, not the exit code.
function Test-TursoLoggedIn {
  $who = (Invoke-Turso 'auth whoami' 2>$null) -join ' '
  return ($who -and $who -notmatch 'not logged in')
}

# -NoWsl mode talks to the Turso Platform REST API instead of the CLI.
$TursoApiBase = 'https://api.turso.tech/v1'

function Invoke-TursoApi {
  param([string]$Method, [string]$Path, $Body)
  $Params = @{
    Method  = $Method
    Uri     = "$TursoApiBase$Path"
    Headers = @{ Authorization = "Bearer $TursoApiToken" }
  }
  if ($null -ne $Body) {
    $Params.Body = ConvertTo-Json -InputObject $Body
    $Params.ContentType = 'application/json'
  }
  Invoke-RestMethod @Params
}

if (-not (Get-Command winget -ErrorAction SilentlyContinue)) {
  Write-Error "winget is required (ships with Windows 10/11 as 'App Installer'). Install it from the Microsoft Store, then re-run this script."
}

Show-Banner 'STEP 1: INSTALL MSVC BUILD TOOLS'
# Equivalent of the Xcode Command Line Tools: the MSVC linker and Windows SDK
# that rust's default x86_64-pc-windows-msvc / aarch64-pc-windows-msvc
# toolchain needs. Detected via vswhere, which ships with any VS installer.
$VsWhere = "${env:ProgramFiles(x86)}\Microsoft Visual Studio\Installer\vswhere.exe"
$HaveVcTools = (Test-Path $VsWhere) -and
  ((& $VsWhere -products * -requires Microsoft.VisualStudio.Component.VC.Tools.x86.x64 -property installationPath) -join '')
if ($HaveVcTools) {
  Write-Host "MSVC Build Tools already installed at: $HaveVcTools"
} else {
  Write-Host 'MSVC Build Tools not found. Installing via winget (this can take a while)...'
  winget install --id Microsoft.VisualStudio.2022.BuildTools -e `
    --accept-package-agreements --accept-source-agreements `
    --override '--quiet --wait --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended'
  Assert-LastExitCode 'MSVC Build Tools installation'
  Write-Host ''
  Write-Host 'Build tools installed. Open a NEW terminal and RE-RUN this script.'
  exit 1
}

Show-Banner 'STEP 2: INSTALL RUSTUP'
if (Get-Command rustup -ErrorAction SilentlyContinue) {
  Write-Host "rustup already installed: $((rustup --version 2>$null | Select-Object -First 1))"
} else {
  Write-Host 'rustup not found. Installing via winget...'
  winget install --id Rustlang.Rustup -e --accept-package-agreements --accept-source-agreements
  Assert-LastExitCode 'rustup installation'
}
# Make cargo/rustup available to this session (new shells get it from the
# PATH entry the rustup installer registers).
$CargoBin = Join-Path $env:USERPROFILE '.cargo\bin'
if ($env:Path -notlike "*$CargoBin*") {
  $env:Path = "$CargoBin;$env:Path"
  Write-Host "Added $CargoBin to PATH for this session."
}

Show-Banner 'STEP 3: VERIFY CARGO'
if (Get-Command cargo -ErrorAction SilentlyContinue) {
  Write-Host "cargo already installed: $(cargo --version)"
} else {
  Write-Host 'cargo not found. Installing the stable toolchain via rustup...'
  rustup toolchain install stable
  rustup default stable
  Write-Host "cargo installed: $(cargo --version)"
}

if ($NoWsl) {
  Show-Banner 'STEP 4: TURSO CLI (NATIVE GO BUILD, NO WSL)'
  $TursoApiToken = $env:TURSO_API_TOKEN
  if ($TursoApiToken) {
    Write-Host 'Using the Turso Platform API token from $env:TURSO_API_TOKEN.'
  } else {
    if (Get-Command go -ErrorAction SilentlyContinue) {
      Write-Host "Go already installed: $(go version)"
    } else {
      Write-Host 'Go not found. Installing via winget...'
      winget install --id GoLang.Go -e --accept-package-agreements --accept-source-agreements
      Assert-LastExitCode 'Go installation'
      $env:Path = "$env:ProgramFiles\Go\bin;$env:Path"
    }
    $GoBin = Join-Path $env:USERPROFILE 'go\bin'
    if ($env:Path -notlike "*$GoBin*") {
      $env:Path = "$GoBin;$env:Path"
    }
    if (Get-Command turso -ErrorAction SilentlyContinue) {
      Write-Host "turso already installed: $((Get-Command turso).Source)"
    } else {
      Write-Host 'turso not found. Building it natively with Go (this can take a minute)...'
      go install github.com/tursodatabase/turso-cli/cmd/turso@latest
      Assert-LastExitCode 'turso CLI build'
    }
    if (Test-TursoLoggedIn) {
      Write-Host "Already logged in to Turso as: $(Invoke-Turso 'auth whoami')"
    } else {
      Write-Host 'Not logged in to Turso. Starting signup (a browser window will open;'
      Write-Host 'if you already have an account this simply logs you in)...'
      Invoke-Turso 'auth signup'
      if (-not (Test-TursoLoggedIn)) {
        Write-Error "Turso login did not complete. Run 'turso auth login' and re-run this script."
      }
    }
    # Mint a fresh Platform API token for the REST calls below. Tokens are
    # shown only once, so an existing one by this name is revoked first.
    $TokenName = 'claude-vault-installer'
    $TokenList = @(Invoke-Turso 'auth api-tokens list')
    if (@($TokenList | Where-Object { $_.Trim() -match "^$TokenName(\s|$)" }).Count -gt 0) {
      Invoke-Turso "auth api-tokens revoke $TokenName" | Out-Null
    }
    $TursoApiToken = (Invoke-Turso "auth api-tokens mint $TokenName") -join ''
    if (-not $TursoApiToken -or $TursoApiToken -match ' ') {
      Write-Error "'turso auth api-tokens mint' did not return a token, got: $TursoApiToken"
    }
    Write-Host "Minted Platform API token '$TokenName' ($($TursoApiToken.Length) chars)."
  }
  try {
    $Orgs = @(Invoke-TursoApi GET '/organizations')
  } catch {
    Write-Error "Could not list Turso organizations - is the API token valid? ($_)"
  }
  # Tolerate both documented response shapes: a bare array or {organizations:[...]}
  if ($Orgs.Count -eq 1 -and $Orgs[0].PSObject.Properties['organizations']) {
    $Orgs = @($Orgs[0].organizations)
  }
  if ($Orgs.Count -eq 0) {
    Write-Error 'The API token has access to no Turso organizations.'
  } elseif ($Orgs.Count -eq 1) {
    $OrgSlug = $Orgs[0].slug
  } else {
    Write-Host "Organizations available to this token: $(($Orgs | ForEach-Object { $_.slug }) -join ', ')"
    $OrgSlug = Read-Host -Prompt 'Organization slug to use'
    if ($OrgSlug -notin ($Orgs | ForEach-Object { $_.slug })) {
      Write-Error "'$OrgSlug' is not one of the available organizations."
    }
  }
  Write-Host "Using Turso organization: $OrgSlug"
} else {
  Show-Banner 'STEP 4: INSTALL TURSO CLI (VIA WSL)'
  # Turso's CLI does not support native Windows; the documented path is WSL.
  wsl -e sh -c 'exit 0' 2>$null
  if ($LASTEXITCODE -ne 0) {
    Write-Host 'WSL is not ready. Installing it (requires admin; a reboot may be needed)...'
    wsl --install
    Write-Host ''
    Write-Host 'After WSL setup (and a reboot if prompted), RE-RUN this script.'
    Write-Host '(Or re-run with -NoWsl to provision via the Turso Platform API instead.)'
    exit 1
  }
  $TursoPath = (wsl -e sh -c 'PATH="$HOME/.turso:$PATH" command -v turso' 2>$null) -join ''
  if ($TursoPath) {
    Write-Host "turso already installed in WSL: $TursoPath"
  } else {
    Write-Host 'turso not found in WSL. Installing via get.tur.so...'
    wsl -e sh -c 'curl -sSfL https://get.tur.so/install.sh | bash'
    Assert-LastExitCode 'Turso CLI installation'
  }
  if (Test-TursoLoggedIn) {
    Write-Host "Already logged in to Turso as: $(Invoke-Turso 'auth whoami')"
  } else {
    Write-Host 'Not logged in to Turso. Starting signup (follow the URL it prints;'
    Write-Host 'if you already have an account this simply logs you in)...'
    Invoke-Turso 'auth signup'
    if (-not (Test-TursoLoggedIn)) {
      Write-Error "Turso login did not complete. Run 'wsl turso auth login' and re-run this script."
    }
  }
}

Show-Banner "STEP 5: FIND OR CREATE THE '$DbName' DATABASE"
if ($NoWsl) {
  $Database = $null
  try {
    $Database = (Invoke-TursoApi GET "/organizations/$OrgSlug/databases/$DbName").database
  } catch {
    if ([int]$_.Exception.Response.StatusCode -ne 404) { throw }
  }
  if ($Database) {
    Write-Host "Database '$DbName' already exists."
  } else {
    # Databases must be created inside a group; reuse the first existing one
    # or create 'default' in the closest location (as the CLI would).
    $Groups = @((Invoke-TursoApi GET "/organizations/$OrgSlug/groups").groups)
    if ($Groups.Count -gt 0) {
      $GroupName = $Groups[0].name
    } else {
      $GroupName = 'default'
      $Location = (Invoke-RestMethod 'https://region.turso.io').server
      Write-Host "No group found. Creating group '$GroupName' in closest location '$Location'..."
      Invoke-TursoApi POST "/organizations/$OrgSlug/groups" @{ name = $GroupName; location = $Location } | Out-Null
    }
    Write-Host "Database '$DbName' not found. Creating it in group '$GroupName'..."
    $Database = (Invoke-TursoApi POST "/organizations/$OrgSlug/databases" @{ name = $DbName; group = $GroupName }).database
  }
} else {
  $ExistingUrl = (Invoke-Turso "db show $DbName --url" 2>$null) -join ''
  if ($ExistingUrl -like 'libsql://*') {
    Write-Host "Database '$DbName' already exists."
  } else {
    Write-Host "Database '$DbName' not found. Creating it..."
    Invoke-Turso "db create $DbName"
    Assert-LastExitCode 'Database creation'
  }
}

Show-Banner 'STEP 6: SET SYNC ENVIRONMENT VARIABLES (USER SCOPE)'
if ($NoWsl) {
  $SyncUrl = "libsql://$($Database.Hostname)"
  $AuthToken = (Invoke-TursoApi POST "/organizations/$OrgSlug/databases/$DbName/auth/tokens").jwt
} else {
  $SyncUrl = (Invoke-Turso "db show $DbName --url") -join ''
  $AuthToken = (Invoke-Turso "db tokens create $DbName") -join ''
}
if ($SyncUrl -notlike 'libsql://*') {
  Write-Error "Expected a libsql:// sync URL, got: $SyncUrl"
}
if (-not $AuthToken -or $AuthToken -match ' ') {
  Write-Error "Token minting did not return a token, got: $AuthToken"
}
Write-Host "Sync URL: $SyncUrl"
Write-Host "Minted a fresh auth token ($($AuthToken.Length) chars)."
# Persist for future shells (registry, user scope) and export to this session
[Environment]::SetEnvironmentVariable('CLAUDE_VAULT_SYNC_URL', $SyncUrl, 'User')
[Environment]::SetEnvironmentVariable('CLAUDE_VAULT_AUTH_TOKEN', $AuthToken, 'User')
$env:CLAUDE_VAULT_SYNC_URL = $SyncUrl
$env:CLAUDE_VAULT_AUTH_TOKEN = $AuthToken
Write-Host 'Wrote CLAUDE_VAULT_SYNC_URL and CLAUDE_VAULT_AUTH_TOKEN to the user environment.'

Show-Banner 'STEP 7: REMOVE ANY PREVIOUSLY INSTALLED CLAUDE-VAULT'
cargo uninstall claude-vault 2>$null | Out-Null
if ($LASTEXITCODE -eq 0) {
  Write-Host 'Uninstalled previous claude-vault binary.'
} else {
  Write-Host 'No previously installed claude-vault binary (nothing to uninstall).'
}

Show-Banner 'STEP 8: BUILD AND INSTALL CLAUDE-VAULT'
Write-Host "Running cargo install --path $RepoDir (this can take a few minutes)..."
cargo install --path $RepoDir
Assert-LastExitCode 'cargo install'
Write-Host "Installed: $((Get-Command claude-vault).Source) ($(claude-vault --version))"

Show-Banner 'STEP 9: IMPORT CONVERSATION HISTORY INTO THE SYNCED VAULT'
# A database created by an older (local-only) claude-vault is a plain SQLite
# file; sync mode must build its embedded-replica file fresh from the server.
# Move any such file aside — its contents are re-imported from ~/.claude below,
# and UUID dedup makes that safe.
if ((Test-Path $VaultDb) -and -not (Test-Path "$VaultDb-info")) {
  $Backup = "$VaultDb.pre-sync-$(Get-Date -Format yyyyMMddHHmmss).bak"
  Write-Host 'Existing local (non-replica) database found. Moving it aside:'
  Write-Host "  $VaultDb -> $Backup"
  Move-Item $VaultDb $Backup
  Remove-Item -ErrorAction SilentlyContinue "$VaultDb-wal", "$VaultDb-shm"
}
Write-Host 'Importing from ~/.claude/projects (first sync run may take a while)...'
claude-vault import
Assert-LastExitCode 'claude-vault import'
claude-vault stats

Show-Banner 'STEP 10 & 11: WIRE SYNC INTO CLAUDE CODE SETTINGS'
New-Item -ItemType Directory -Force -Path $ClaudeDir | Out-Null
$Settings = [pscustomobject]@{}
if (Test-Path $ClaudeSettings) {
  Copy-Item $ClaudeSettings "$ClaudeSettings.bak-$(Get-Date -Format yyyyMMddHHmmss)"
  Write-Host 'Backed up existing settings.json.'
  $Settings = Get-Content -Raw $ClaudeSettings | ConvertFrom-Json
}

# Step 10: env vars for Claude Code sessions (hooks inherit these too)
if (-not $Settings.PSObject.Properties['env']) {
  $Settings | Add-Member -NotePropertyName env -NotePropertyValue ([pscustomobject]@{})
}
$Settings.env | Add-Member -Force -NotePropertyName CLAUDE_VAULT_SYNC_URL -NotePropertyValue $SyncUrl
$Settings.env | Add-Member -Force -NotePropertyName CLAUDE_VAULT_AUTH_TOKEN -NotePropertyValue $AuthToken
Write-Host 'Set env.CLAUDE_VAULT_SYNC_URL and env.CLAUDE_VAULT_AUTH_TOKEN'

# Step 11: auto-archive hooks with sync flags inlined. Hook commands run
# through cmd.exe on Windows, hence >NUL and no trailing '&' (cmd has no
# background operator; SessionEnd runs synchronously here).
$Flags = "--sync-url `"$SyncUrl`" --auth-token `"$AuthToken`""
$Desired = [ordered]@{
  PreCompact = "claude-vault $Flags import >NUL 2>&1"
  SessionEnd = "claude-vault $Flags import >NUL 2>&1"
}
if (-not $Settings.PSObject.Properties['hooks']) {
  $Settings | Add-Member -NotePropertyName hooks -NotePropertyValue ([pscustomobject]@{})
}
foreach ($Event in $Desired.Keys) {
  $Command = $Desired[$Event]
  $Entries = @()
  if ($Settings.hooks.PSObject.Properties[$Event]) {
    $Entries = @($Settings.hooks.$Event)
  }
  $Updated = $false
  foreach ($Entry in $Entries) {
    foreach ($Hook in @($Entry.hooks)) {
      $Cmd = [string]$Hook.command
      if ($Hook.type -eq 'command' -and $Cmd.TrimStart().StartsWith('claude-vault') -and $Cmd -match 'import') {
        $Hook.command = $Command
        $Updated = $true
        break
      }
    }
    if ($Updated) { break }
  }
  if ($Updated) {
    Write-Host "Updated existing $Event hook to include sync flags"
  } else {
    $Entries += [pscustomobject]@{ hooks = @([pscustomobject]@{ type = 'command'; command = $Command }) }
    Write-Host "Added $Event auto-archive hook with sync flags"
  }
  $Settings.hooks | Add-Member -Force -NotePropertyName $Event -NotePropertyValue $Entries
}
ConvertTo-Json -InputObject $Settings -Depth 32 | Set-Content -Encoding UTF8 $ClaudeSettings
Write-Host "Wrote $ClaudeSettings"

Show-Banner 'DONE'
Write-Host 'claude-vault is installed with multi-device sync enabled.'
Write-Host ''
Write-Host "  Binary:    $((Get-Command claude-vault).Source)"
Write-Host "  Database:  $VaultDb (embedded replica)"
Write-Host "  Sync URL:  $SyncUrl"
Write-Host ''
Write-Host 'Open a new terminal to pick up the environment variables. Claude Code'
Write-Host 'sessions started from now on archive to the shared vault automatically.'
Write-Host 'Run this script (or install-mac.sh on a Mac) on any other machine to'
Write-Host 'connect it to the same vault.'
