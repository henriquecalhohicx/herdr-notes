<div align="center">

# Herdr Notes

### The scratch note that lives beside your agents.

One markdown note per tab in a dockable [herdr](https://github.com/ogulcancelik/herdr)
pane — rendered preview, plain-text editing, and it never forgets: everything
autosaves and survives computer restarts.

<img alt="Rust" src="https://img.shields.io/badge/Rust-self--contained_crate-orange?logo=rust&logoColor=white">
<img alt="herdr" src="https://img.shields.io/badge/herdr-%E2%89%A5%200.7-5865a3">
<img alt="Platforms" src="https://img.shields.io/badge/Windows%20%C2%B7%20macOS%20%C2%B7%20Linux-supported-2ea44f">
<img alt="CI" src="https://github.com/alexarthurs/herdr-notes/actions/workflows/ci.yml/badge.svg">
<img alt="License" src="https://img.shields.io/badge/license-MIT-blue">

<br><br>

<img src="docs/media/hero.png" alt="The Notes pane docked beside a running test suite: rendered markdown with headings, checkboxes, code and quotes" width="920">

</div>

That's the note docked on the right, keeping its shape while the test suite runs
next door. Terminals are where the work happens, and the work generates thoughts —
half-finished todo lists, commands you keep retyping, things to ask about later.
This gives them a permanent home one keypress away: no editor window, no stray
`notes.txt`, no saving.

```
herdr plugin install alexarthurs/herdr-notes
```

---

## Why you'll keep it open

- **Rendered markdown** — headings, checkboxes, lists, quotes, code blocks
  and inline styles, drawn natively in the terminal with a scrollbar.
- **Zero-friction editing** — `e` to type, `Esc` to go back. That's it.
- **A note per tab** — every herdr tab keeps its own note, keyed to the tab
  itself. Open Notes in as many tabs as you like; each is its own document.
- **Titled, and always findable** — press `r` in preview to set or rename a
  note's title (it shows in the header); `l` opens an overlay listing every
  note across tabs with its age and live/closed status, for a quick preview,
  rename, or delete without hunting down the right tab.
- **Actually persistent** — atomic autosaves to a per-tab JSON file in
  herdr's config directory. Close the pane, kill the terminal, reboot: it
  comes back.
- **A polite pane** — one toggle action opens, focuses, or closes it;
  a heartbeat token means a dead pane gets replaced, never duplicated.

## Install

From a checkout of this repo:

```
cargo build --release
herdr plugin link .
```

Or straight from GitHub with the command under the hero image.

## Open

One toggle action, scoped to the current tab — it opens the pane docked on
the right edge, focuses it if it's already open, and closes it if it's focused:

```
herdr plugin action invoke herdr-notes.open-notes-windows   # windows
herdr plugin action invoke herdr-notes.open-notes           # linux / macos
```

First run greets you with the keymap:

<div align="center">
<img src="docs/media/welcome.png" alt="Empty note showing the built-in key reference" width="920">
</div>

## Keys

Preview (default):

| Key | Action |
| --- | --- |
| `e` / `Enter` | edit the note |
| `Up` `Down` `PgUp` `PgDn` | scroll |
| `g` / `G` | jump to top / bottom |
| `x` | clear the note (y/N confirm) |
| `q` | quit |

Edit:

| Key | Action |
| --- | --- |
| `Esc` | back to preview (saves) |
| `Ctrl+S` | save now (autosave runs anyway, ~2s after the last keystroke) |

`Esc` never exits the app.

<div align="center">
<img src="docs/media/edit.png" alt="Edit mode: the same note as plain markdown with a block cursor" width="920">
</div>

### Ticket links

Issue keys in the note (`HM-54561`) are underlined once their prefix is
configured. `n`/`N` walk them, `o` opens the selected one in your browser,
`esc` drops the cursor.

Create `tickets.json` next to the note files — `%LOCALAPPDATA%\herdr\plugins\herdr-notes\`
on Windows, `~/.local/share/herdr/plugins/herdr-notes/` on unix:

```json
{
  "HM": "https://your-org.atlassian.net/browse/{key}",
  "CR": "https://your-tracker.example/issue/{key}"
}
```

Only listed prefixes are detected, so an unmapped key is never highlighted and
never pretends to be openable. The file is read once at startup: after editing
it, close and reopen the Notes pane. A missing or malformed file simply turns
the feature off.

## Persistence

Each herdr tab gets its own note, stored as `<tab-key>.json` in herdr's
per-plugin state directory (`HERDR_PLUGIN_STATE_DIR` — e.g.
`%LOCALAPPDATA%\herdr\plugins\herdr-notes\` on Windows), keyed by the
`HERDR_TAB_ID` herdr injects into every pane (its `:` separator sanitized to
`_`, so tab `w1:t2` → `w1_t2.json`). Closing a tab just orphans its file;
delete `<tab-key>.json` by hand if you want it gone. Run outside herdr, the
pane falls back to `herdr/notes/` under the platform config dir (single
shared `notes.json` when there's no tab id), and any note found in the
fallback layout is moved into the state dir on first load — an existing note
is inherited, never lost.

The format is `{ "text": "...", "mode": "preview"|"edit", "title": "...",
"tab_id": "...", "created": ..., "updated": ... }`. Older `{ text, mode }`
files still load fine — the newer fields fill in with defaults. `created`
is stamped once; `updated` bumps on every save. Saves are atomic (temp file
+ fsync + rename) and happen on leaving edit mode, clear, quit, and
debounced while typing. A missing or corrupt file falls back to an empty
note — it never wedges the pane.

A note with no text and no title is deleted on save instead of written, so
opening Notes in a tab and closing it again without typing anything leaves
nothing behind. Press `l` in preview for an overlay listing every note
across tabs — title (or "(untitled)"), age, and whether its tab is still
open (`live`/`closed`/`?`) — with read-only preview, rename, and delete
built in.

## Prompt capture

Notes can show the last few prompts you typed into each Claude Code pane
sharing the tab, so you don't lose track of what you asked for while you're
jotting things down beside it. A `UserPromptSubmit` hook writes each prompt
(first line, truncated) to a small per-pane file as soon as a Notes pane is
open in the tab — capture no longer waits for the note to have any content
first — and the pane groups them by agent pane and renders one heading per
pane above the note in preview, each with its own last 3 prompts numbered
underneath — a four-agent tab keeps twelve prompts on screen, not three
shared between all of them.

An untitled note also picks up an automatic title as soon as one is
available: the agent pane's herdr label when one has been set, else its
terminal title when it says something meaningful, else its git branch, else
the oldest prompt still on file. It keeps re-checking on every heartbeat, so
renaming a pane updates an already-derived title too, not just an empty one.
Press `r` to set a title by hand at any point — that freezes it, so
auto-titling never overwrites it again — and clearing it back to empty with
`r` hands the note back to auto-titling.

Install the hook into your global Claude Code settings:

```
pwsh scripts/install-prompt-hook.ps1
```

It's idempotent — re-run it any time (after a rebuild, say) and it replaces
just this plugin's entry, leaving every other hook untouched. The first run
backs `~/.claude/settings.json` up to `~/.claude/settings.json.herdr-notes.bak`;
later runs KEEP that original backup rather than overwriting it, so it always
holds your pre-install settings. Pass `-Remove` to uninstall.

Windows PowerShell 5.1 works too (`powershell scripts\install-prompt-hook.ps1`)
— the script writes UTF-8 without a BOM on both editions.

The installer only ever touches its own hook entry: if you've merged the
herdr-notes command into an existing `UserPromptSubmit` entry alongside some
other tool, re-running it (or `-Remove`) drops just the herdr-notes hook
object and leaves every sibling hook in that entry alone.

Would rather not run a script against your global settings? Add this to the
`hooks.UserPromptSubmit` array in `~/.claude/settings.json` yourself:

```json
{
  "hooks": [
    {
      "type": "command",
      "command": "\"C:\\path\\to\\herdr-notes\\target\\release\\herdr-notes.exe\" --capture-prompt",
      "timeout": 5
    }
  ]
}
```

Set `HERDR_NOTES_NO_CAPTURE=1` in the environment to turn capture off without
touching the hook registration.

Known limits, from the design doc:

- **Codex panes capture nothing.** Codex has no submit-time hook equivalent to
  `UserPromptSubmit`. On a mixed grid, only the Claude panes will have
  history. This is a gap in the ecosystem, not something this design can
  close.
- **Opening Notes is enough — the note no longer needs content first.** A
  live Notes pane in the tab passes the capture gate on its own; the note
  file only has to exist when Notes isn't currently open in that tab. One
  side effect: a tab where Notes was opened but nothing was ever typed can
  leave an orphaned `<tab>__<pane>.prompts.json` with no note file beside
  it.
- **Pane files orphan** when their pane closes, exactly as note files orphan
  when their tab closes. Tab and pane ids are never reused, so an orphan is a
  dead file rather than a stale-content risk.

## Hacking

`CLAUDE.md` has the build/dev workflow and the hard-won herdr/Windows
gotchas (pane spawning, heartbeats, PowerShell 5.1 quirks). The short
version: `cargo build --release`, `cargo test`,
`cargo clippy --all-targets -- -D warnings`, all green before shipping.
