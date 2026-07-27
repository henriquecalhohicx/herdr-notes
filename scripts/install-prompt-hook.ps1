# Registers herdr-notes' prompt-capture hook in the user's GLOBAL Claude Code
# settings (~/.claude/settings.json). Idempotent: re-running replaces this
# plugin's entry and leaves every other hook alone. Backs up first.
#
# Uninstall: pass -Remove.

param([switch]$Remove)

$ErrorActionPreference = 'Stop'

$exe = Join-Path (Split-Path -Parent $PSScriptRoot) 'target\release\herdr-notes.exe'
if (-not $Remove -and -not (Test-Path $exe)) {
    Write-Error "herdr-notes.exe not found at $exe - run 'cargo build --release' first."
}

$settingsPath = Join-Path $env:USERPROFILE '.claude\settings.json'
if (-not (Test-Path $settingsPath)) {
    Write-Error "No Claude Code settings at $settingsPath."
}

# Keep the FIRST backup, never overwrite it. The README invites re-running
# this script, and -Force here would make the second run's "backup" a copy of
# the already-modified file - throwing away the only pristine pre-install copy.
$backup = "$settingsPath.herdr-notes.bak"
if (Test-Path $backup) {
    Write-Host "Keeping the existing pre-install backup at $backup"
}
else {
    Copy-Item $settingsPath $backup
    Write-Host "Backed up to $backup"
}

$settings = Get-Content $settingsPath -Raw -Encoding UTF8 | ConvertFrom-Json
if (-not $settings.hooks) {
    $settings | Add-Member -NotePropertyName hooks -NotePropertyValue ([pscustomobject]@{}) -Force
}

# Drop only herdr-notes' own hook OBJECT from each existing entry, keeping
# every sibling hook in that same entry alone. An entry is dropped entirely
# only once removing herdr-notes leaves its hooks array empty - never because
# it merely contains herdr-notes alongside something else.
$existing = @()
if ($settings.hooks.UserPromptSubmit) {
    foreach ($entry in @($settings.hooks.UserPromptSubmit)) {
        $keptHooks = @($entry.hooks | Where-Object { $_.command -notlike '*herdr-notes*--capture-prompt*' })
        if ($keptHooks.Count -gt 0) {
            $entry.hooks = @($keptHooks)
            $existing += $entry
        }
    }
}

if (-not $Remove) {
    $entry = [pscustomobject]@{
        hooks = @([pscustomobject]@{
            type    = 'command'
            command = "`"$exe`" --capture-prompt"
            timeout = 5
        })
    }
    $existing = @($existing) + $entry
}

$settings.hooks | Add-Member -NotePropertyName UserPromptSubmit -NotePropertyValue @($existing) -Force

# Atomic write: a temp sibling in the SAME directory (so the rename is a
# rename, not a cross-volume copy), then moved over the real target. The
# user's live settings file is never observed half-written, and the temp file
# never lingers past a failed run.
#
# UTF-8 with NO BOM, written explicitly: `Set-Content -Encoding UTF8` means
# utf8-no-BOM in pwsh 7 but BOM-prefixed UTF-8 in Windows PowerShell 5.1
# (verified on this machine: 5.1 emits EF BB BF, pwsh 7 emits none). herdr
# panes run 5.1, so `powershell scripts\install-prompt-hook.ps1` is a natural
# way to run this - and a BOM on ~/.claude/settings.json risks every global
# setting the user has silently failing to load. Do not go back to relying on
# the parameter's edition-dependent meaning.
$tmp = "$settingsPath.herdr-notes.tmp"
try {
    $json = $settings | ConvertTo-Json -Depth 20
    [System.IO.File]::WriteAllText($tmp, $json, (New-Object System.Text.UTF8Encoding($false)))
    Move-Item -Path $tmp -Destination $settingsPath -Force
}
finally {
    if (Test-Path $tmp) { Remove-Item $tmp -Force }
}

if ($Remove) { Write-Host "Removed the herdr-notes prompt hook." }
else { Write-Host "Installed. Restart any running Claude Code session to pick it up." }
