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

$backup = "$settingsPath.herdr-notes.bak"
Copy-Item $settingsPath $backup -Force
Write-Host "Backed up to $backup"

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
# rename, not a cross-volume copy), flushed via Set-Content, then moved over
# the real target. The user's live settings file is never observed
# half-written, and the temp file never lingers past a failed run.
$tmp = "$settingsPath.herdr-notes.tmp"
try {
    $settings | ConvertTo-Json -Depth 20 | Set-Content $tmp -Encoding UTF8
    Move-Item -Path $tmp -Destination $settingsPath -Force
}
finally {
    if (Test-Path $tmp) { Remove-Item $tmp -Force }
}

if ($Remove) { Write-Host "Removed the herdr-notes prompt hook." }
else { Write-Host "Installed. Restart any running Claude Code session to pick it up." }
