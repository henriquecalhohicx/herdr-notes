# Notes manager — design

Date: 2026-07-19

## Goal

Add titles, timestamps, an in-note list/manage overlay, and empty-note cleanup
to the per-tab notes plugin. Turns the bare `{text, mode}` note into something
you can name, browse across tabs, and prune.

Builds on the per-tab keying (`HERDR_TAB_ID`, see the prior spec).

## User decisions

- **Title editing:** the same key, `r`, renames from BOTH the note (preview
  mode) and the list overlay. Title is optional (blank ⇒ "(untitled)").
- **List surface:** an in-note overlay opened with `l`. Browse all notes,
  `enter` previews (read-only), `r` renames, `d` deletes (y/N). `esc`/`l`
  closes. `q` remains the only key that quits the TUI; Esc never exits.
- **Status column:** each row shows whether the note's owning tab is `live`,
  `closed`, or `unknown` (socket unreachable). `closed` = the old "orphan".
- **Cleanup:** manual only (`d` on the highlighted note). No bulk, no auto —
  BUT empty notes are no longer persisted at all (see below), which removes the
  main orphan source.

## Storage format v2 (`state.rs`)

`{text, mode}` grows to:

```json
{ "text": "...", "mode": "preview|edit", "title": "...",
  "tab_id": "w9:t1", "created": 1721400000, "updated": 1721400300 }
```

- `title` — optional, default `""`.
- `tab_id` — the raw herdr tab id, stored INSIDE the file so the overlay maps a
  file to its tab reliably (no filename un-sanitizing) and can flag `closed`.
- `created` / `updated` — unix seconds via the existing `unix_now()`. `created`
  is set once (preserved across saves); `updated` re-stamped every save.
- Parsing stays forgiving: every field defaults when missing, so pre-v2 files
  (`{text, mode}`) load unchanged — `title=""`, `tab_id` from the process env
  if available else `""`, `created=updated=0`.

## Empty-note lifecycle (`state.rs::save`)

- A note is EMPTY when `text` and `title` are both blank (after trim).
- `save()` on an empty note DELETES the on-disk file if present, and writes
  nothing. Non-empty notes write atomically as today (temp + fsync + rename),
  stamping `updated` (and `created` when the file is new).
- Effect: toggling notes into a tab and closing it without typing leaves no
  file. Only notes with real content (or a title) persist.

## Enumeration (`state.rs::list_notes`)

- `list_notes(dir)` scans the store dir for `*.json` (skipping `*.json.tmp`),
  parses each into a summary `{ file, tab_id, title, updated, nonempty,
  preview }`, and returns them sorted by `updated` descending.
- Pure over an injected dir so it is unit-testable without touching real
  APPDATA (same pattern as the existing path logic).

## Live-tab lookup (`ipc.rs` + overlay)

- One `pane.list` call via the existing `call_text`; collect the distinct
  `tab_id`s of the returned panes = the set of LIVE tabs (every tab has at
  least its root pane).
- A note's status is `live` if its `tab_id` is in that set, `closed` if not,
  `unknown` if the socket call failed (running the binary by hand).
- Caveat (documented, not fixed): `pane.list` covers only THIS herdr session's
  server. The note store is shared across sessions, so a note owned by another
  session's tab reads as `closed` here. Deletion is always manual, so this
  mislabel never causes automatic loss.

## Overlay UI (`app.rs`)

A new modal state layered over the note. Opened with `l` from preview mode.

```
+-- All notes --------------------+
| > Deploy checklist   2h   live   |
|   Meeting notes      5d   closed |
|   (untitled)         3d   closed |
+---------------------------------+
 up/down move  enter preview  r rename  d delete  esc
```

- Navigation: Up/Down (and `j`/`k`). The current tab's own note is marked.
- `enter` — read-only, scrollable preview of the highlighted note, drawn with
  the existing markdown renderer; `esc` returns to the list.
- `r` — inline one-line title input for the highlighted note; Enter writes the
  title (re-stamps `updated`), Esc cancels.
- `d` — delete the highlighted note's file behind a y/N confirm.
- `esc` or `l` — close the overlay back to the note.

In-note (preview mode), `r` opens the SAME one-line title input for THIS note.
The main note header shows the title when set:
`Deploy checklist — [preview]  12%`. The pane border label stays "Notes" — this
is the note's own name, not a second "Notes" title, so the no-duplicate-title
rule holds.

## Age format

`just now`, `5m`, `2h`, `3d`, `2w` computed from `now - updated`. A small pure
helper, unit-tested at boundaries.

## Testing (TDD throughout)

- `state.rs`: v2 round-trip; pre-v2 file still parses; empty note deletes its
  file; non-empty stamps created/updated; `list_notes` ordering + summaries.
- `app.rs`: `r` enters/commits/cancels the title input in preview; `l` opens
  and `esc`/`l` close the overlay; navigation clamps; `d` confirm flow; header
  renders the title.
- Age helper: boundary cases.
- `ipc`/live-tab: unit-test the pure "tab_id ∈ live-set ⇒ live/closed/unknown"
  classifier off an injected set; the socket call itself stays a thin wrapper.

## Non-goals

- No in-note-only vs overlay-only rename split — `r` does both.
- No bulk / automatic orphan deletion.
- No switching the pane to a different tab's note (preview is read-only).
- No cross-session orphan accuracy.
- No new dependencies.

## Sequencing (for the plan)

1. Storage v2 (format + timestamps + backward-compat parse).
2. Empty-note deletion in `save`.
3. `list_notes` enumeration + age helper.
4. Live-tab classifier + `pane.list` wrapper.
5. In-note `r` title input + header title.
6. Overlay: open/close, navigation, preview, rename, delete.
