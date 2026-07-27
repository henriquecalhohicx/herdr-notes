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

# Drop any previous herdr-notes entry so re-running never stacks duplicates.
$existing = @()
if ($settings.hooks.UserPromptSubmit) {
    $existing = @($settings.hooks.UserPromptSubmit | Where-Object {
        -not ($_.hooks | Where-Object { $_.command -like '*herdr-notes*--capture-prompt*' })
    })
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
$settings | ConvertTo-Json -Depth 20 | Set-Content $settingsPath -Encoding UTF8

if ($Remove) { Write-Host "Removed the herdr-notes prompt hook." }
else { Write-Host "Installed. Restart any running Claude Code session to pick it up." }
