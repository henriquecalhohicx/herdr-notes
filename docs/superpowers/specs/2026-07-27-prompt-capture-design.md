# Prompt capture — design

Date: 2026-07-27

## Goal

When you come back to a herdr tab after an hour, the Notes pane should already
tell you what you last asked the agent — without you having written it down.
Capture the last 3 prompts submitted in each Claude pane of a tab and render
them above that tab's note.

This is **phase B** of three. Phase A (the Status/Next/Notes seed template, the
preview checkbox cursor, header age, overlay TODO progress) shipped in
`62a686e`/`91b35de`. Phase C (auto-default title, per-agent grouping of the
captured prompts) comes after and depends on this.

## Architecture

```
Claude pane                          Notes pane
─────────────                        ──────────
you submit a prompt
  └─ UserPromptSubmit hook
       └─ herdr-notes --capture-prompt   (reads JSON on stdin)
            └─ writes <tab>__<pane>.prompts.json   ← atomic, its own file
                                          │
                                     reader globs <tab>__*.prompts.json
                                     every 5s (heartbeat tick), merges
                                     newest-first, renders above the note
```

One writer per file, one reader. The clobber problem that shaped this whole
design — the TUI autosaving its entire in-memory buffer every 2s and erasing
anything an external process appended — cannot arise, because no file has two
writers and the captured prompts never enter the note's text buffer.

## Ground truth (verified, not assumed)

- herdr installs its own Claude Code hook at `~/.claude/hooks/herdr-agent-state.ps1`,
  which reads `$env:HERDR_ENV` and `$env:HERDR_PANE_ID`. So the herdr pane
  environment does reach a hook subprocess.
- The `UserPromptSubmit` payload carries `prompt` verbatim, alongside
  `session_id`, `cwd`, and `transcript_path` — confirmed against a working
  hook on this machine (`caveman-mode-tracker.js` reads `data.prompt`).
- Hook entries take a `timeout` (herdr's uses 10, caveman's 5).

> **AMENDED after Task 4 review (human partner's ruling).** This design assumed
> the hook could resolve the same store dir as the Notes pane simply by both
> reading `state::store_dir()`. A live check in a real herdr agent pane proved
> that assumption wrong: the pane's environment carries `HERDR_ENV=1`,
> `HERDR_TAB_ID`, `HERDR_PANE_ID`, etc., but `HERDR_PLUGIN_STATE_DIR` is
> plugin-scoped — herdr injects it only into the Notes pane itself (via
> `pane split --env`/the unix `[[panes]]` entry), never into a Claude Code
> agent pane. Left alone, the hook would fall through to the config-dir
> layout while the pane resolved the plugin-state layout, and gate 4 (no note
> file for this tab) would never fire for a real user because it would be
> checking the wrong directory. `state::store_base()` now adds a middle tier:
> `HERDR_PLUGIN_STATE_DIR` set and non-empty still wins outright (the pane),
> but `HERDR_ENV == "1"` alone (no explicit dir — the hook) now resolves the
> conventional plugin state dir (`%LOCALAPPDATA%\herdr\plugins\herdr-notes` /
> `$XDG_DATA_HOME/herdr/plugins/herdr-notes`) instead of degrading to the
> config layout. See `src/state.rs`.

## User decisions (approved)

- **Capture only when the tab already has a note file.** No note, no capture,
  no file. Nothing retroactive.
- **Rendered as a block at the top of preview, scrolling with the note.** Not
  pinned, not behind a key.
- **Last 3 per pane, first line only, truncated to 120 characters.** Stored
  exactly as displayed — nothing sits on disk that the pane does not show.
- **A global off switch, on by default:** `HERDR_NOTES_NO_CAPTURE` non-empty
  makes the hook exit immediately.
- **One file per pane, merged on read.** Not one shared file per tab.

## Features

### 1. The writer — `herdr-notes --capture-prompt`

A new stdin mode beside the existing `--launch-decision` / `--focused-pane` /
`--open-plan` in `src/main.rs`. Reads the `UserPromptSubmit` JSON payload on
stdin, stripping a leading `\u{feff}` as every other stdin path here does.

Gate chain. Every failure is a **silent exit 0**:

1. `HERDR_NOTES_NO_CAPTURE` set and non-empty → exit
2. `HERDR_ENV != "1"` → exit (not running inside herdr)
3. `HERDR_TAB_ID` missing, empty, or filename-unsafe → exit. There is
   deliberately no legacy single-file fallback: prompts are per-tab or they
   are nothing.
4. No note file for that tab → exit
5. stdin is not valid JSON, or carries no non-empty `prompt` → exit

**Two hard safety rules, both from how `UserPromptSubmit` works:**

- **Always exit 0.** A non-zero exit from this hook can block the user's
  prompt from being sent. A bug in this binary must never cost them a message.
  The capture path catches everything and returns 0 unconditionally.
- **Never write to stdout.** Whatever a `UserPromptSubmit` hook prints on
  stdout is injected into the prompt as context. The capture path prints
  nothing, ever — not a success line, not a debug line.

### 2. Storage

`<tab-key>__<pane-key>.prompts.json`, in the same store dir as the notes, both
keys sanitized `:` → `_` exactly as `state::note_key` already does for tab ids:

```
wA_t1.json                    the note
wA_t1__wA_p5.prompts.json     auth-refactor's prompts
wA_t1__wA_p6.prompts.json     checkout-tests' prompts
```

```json
{
  "version": 1,
  "prompts": [
    {"ts": 1785168522, "pane": "wA:p5", "agent": "claude",
     "text": "add a sliding-window rate limiter to the gateway"}
  ]
}
```

Ring of 3, oldest evicted on write. Atomic save through the existing
temp + `sync_all` + rename path. Forgiving parse, same as notes: any garbled
or missing field degrades to a default rather than wedging the pane.

`pane` and `agent` are recorded now even though phase B renders neither, so
phase C's per-agent grouping needs no migration. `agent` is `"claude"`
throughout phase B — see Known limits.

**The trap this creates:** `state::list_notes` enumerates every `*.json` in the
store dir, and `wA_t1__wA_p5.prompts.json` matches that filter. Without an
explicit skip, every prompt file becomes a junk row in the notes overlay.
`list_notes` must exclude any file ending `.prompts.json`.

### 3. Reader and rendering

The reader globs the store dir for the active tab's `<tab-key>__*.prompts.json`,
parses each, merges all entries newest-first by `ts`, and keeps the newest 3
overall.

Refreshed on the existing 5s heartbeat tick, not on every draw — a directory
scan every 500ms frame is waste. Up to 5s of lag before a brand-new prompt
appears, which is irrelevant to the returning-after-an-hour case this serves.

Rendered as dim lines **prepended** to the preview's rendered rows, with their
entries in the provenance map set to `None`. That single detail is what keeps
the phase A checkbox cursor correct: `j`/`k` can never land on a prompt line,
and both the highlight test (`map[i] == Some(src)`) and the scroll-follow
(`map.iter().position(...)`) keep pointing at real note lines.

Not rendered:

- in edit mode — the buffer stays purely the user's text
- in the overlay's read-only preview of another tab's note
- when the pane is showing the global note — global is not a tab and has no
  prompts

### 4. Hook installation

`scripts/install-prompt-hook.ps1` performs an idempotent merge of the
`UserPromptSubmit` entry into `~/.claude/settings.json`, taking a backup first,
and is safe to re-run. The README carries the raw snippet for installing it by
hand. Hook `timeout: 5`.

Stated plainly because it is the one part of this feature that reaches outside
the plugin: **that script writes to the user's global Claude Code settings**,
not to anything scoped to this repo.

## Known limits

- **Codex panes capture nothing.** Codex has no submit-time hook equivalent to
  `UserPromptSubmit`. On a mixed grid, only the Claude panes will have history.
  This is a gap in the ecosystem, not something this design can close.
- **The note must exist first.** Open Notes in a tab, write something, and only
  then do prompts start accumulating. "I opened Notes and see no prompts" is
  the expected behavior on a fresh tab, not a bug.
- **Pane files orphan** when their pane closes, exactly as note files orphan
  when their tab closes. Tab and pane ids are never reused, so an orphan is a
  dead file rather than a stale-content risk.

## Testing

- one test per gate in the chain, each asserting exit 0 and no file written
- the capture path never writes to stdout
- ring eviction at 3; first-line-only and 120-character truncation
- filename sanitization for tab and pane keys, including an unsafe id
- `list_notes` skips `*.prompts.json`
- merge ordering across two pane files with interleaved timestamps
- prepended prompt rows carry `None` in the provenance map, and the checkbox
  cursor still resolves to the right source line with a prompt block present
- no prompt block on the global note, in edit mode, or in the overlay preview
- a fixture-driven parse of a real `UserPromptSubmit` payload

`cargo build --release`, `cargo test`, and
`cargo clippy --all-targets -- -D warnings` must all pass. End-to-end check
against a real Claude pane, since the hook contract cannot be unit-tested.

## Out of scope

Per-agent grouping of the rendered prompts and auto-default titles (phase C),
Codex capture, expanding a truncated prompt to its full text, and any pruning
of orphaned pane files.
