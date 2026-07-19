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

## Hacking

`CLAUDE.md` has the build/dev workflow and the hard-won herdr/Windows
gotchas (pane spawning, heartbeats, PowerShell 5.1 quirks). The short
version: `cargo build --release`, `cargo test`,
`cargo clippy --all-targets -- -D warnings`, all green before shipping.
