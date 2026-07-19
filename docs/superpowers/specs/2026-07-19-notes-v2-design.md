# Notes manager v2 — design

Date: 2026-07-19

## Goal

Turn the notes list overlay into a cross-session dashboard for someone running
many Claude Code sessions across herdr tabs/workspaces. Add: session context in
rows, filter/search, a shared global note, go-to-tab, color + live-first sort,
and a row-margin fix.

Builds on the shipped notes-manager (per-tab notes with `title`/`tab_id`/
`created`/`updated`, the `l` list overlay with preview/rename/delete, `r` title,
`live`/`closed` status). All code lives in `src/state.rs` + `src/app.rs`.

## User decisions (approved)

- Include: session context, filter/search, global note, color + live-first sort,
  go-to-tab `g`, margin fix.
- **NO bulk cleanup.** Delete stays one row at a time via `d` (existing).
- Global note interaction: a pinned `★ Global` row in the overlay whose `enter`
  switches THIS pane to the global note (edit it fully); the same row switches
  back to the tab note. (This is the one model change — see below.)

## Features

### 1. Session context in rows

Each overlay row shows which session/tab its note belongs to, not just
`live`/`closed`. Data (all read-only, fetched once when the overlay opens):

- `tab.list` — GLOBAL (all tabs across all workspaces): `tab_id` → `{label,
  workspace_id}`. Its key set is the LIVE-tab set (a tab present here exists).
- `workspace.list` — `workspace_id` → `label` (e.g. `spec-droid`).
- `pane.list` — `tab_id` → `agent` (first pane whose `agent` is non-null and
  not `"usage"`, e.g. `claude`/`codex`).

Row context string for a note's `tab_id`:
- Live: `"{workspace_label}{ · agent}"`, e.g. `spec-droid · claude` (agent
  omitted when none). Optionally the tab label/number if it is not just the
  default index — keep minimal; workspace + agent is the signal.
- Not in the tab.list set: `closed` (shown by color + a dim `closed` word).
- Socket unreachable (standalone): `?` / unknown.

Replaces the current live/closed *word column* with color (see #4); the context
string takes that horizontal space.

Implementation: a glue fn in `app.rs`, e.g.
`fn tab_contexts() -> Option<TabIndex>` where
`TabIndex { live: HashSet<String>, ctx: HashMap<String, RowContext> }` and
`RowContext { workspace: String, agent: Option<String> }`. `None` when the
socket is unreachable → all rows Unknown. Pure classification/formatting helpers
(`format_context`) live in `state.rs` and are unit-tested off injected maps.

### 2. Filter / search (`/`)

- In the overlay List mode, `/` enters a filter input (a one-line query shown in
  the box's bottom border or a header line).
- Typing filters rows to those whose title OR context contains the query
  (case-insensitive substring). Backspace edits; the list live-updates.
- `Enter` keeps the filter and returns to navigating the filtered rows; `Esc`
  clears the filter and shows all rows again.
- Selection clamps to the filtered set.
- Pure: `fn filter_rows(rows: &[Row], query: &str) -> Vec<usize>` (indices),
  unit-tested.

### 3. Global note

A single note shared by every tab — the cross-session master note.

- Stored at a fixed path: `state::global_path()` → `<store_dir>/global.json`
  (PluginState layout) or `<config>/herdr/notes/global.json` (config layout).
  Not tab-keyed.
- The App tracks which note the pane is currently showing:
  `enum ActiveNote { Tab, Global }`, default `Tab`. `App.active`.
- `App` resolves save/load path from `active`:
  `Tab` → existing `state::state_path()`; `Global` → `state::global_path()`.
  `save()` persists the current buffer to that path via `persist_at` (blank-note
  deletion still applies — an empty global note deletes `global.json`).
- Switching: the overlay's pinned `★ Global` row, on `enter`:
  - commit + save the current note,
  - if `active == Tab` → load `global.json`, set `active = Global`, close overlay;
  - if `active == Global` → load the tab note, set `active = Tab`, close overlay.
  The pinned row's label reflects state: `★ Global note` when on the tab note,
  `◂ Back to this tab's note` when already on global.
- Header shows `★ Global — [preview]` (or `[edit]`) when `active == Global`, so
  it is always obvious which note you are editing.
- Heartbeat/pane identity is unchanged (keyed by pane/tab env); only the
  save/load target changes. The global note is deliberately last-writer-wins if
  two tabs edit it at once (single-user assumption, same as the pre-existing
  cross-session note-file sharing).
- In the list, the global note appears as the pinned top row (always present,
  even when empty). Its status/context reads `global`. `g`/`d`/`r` do not apply
  to it (it has no tab; deleting the shared note from the list is out of scope —
  clear it via edit if wanted). `enter` = the switch behavior above (NOT
  preview), which is the one intentional inconsistency, made obvious by the row
  label.

### 4. Color + live-first sort (NO bulk clean)

- Sort order in the list: pinned `★ Global` row first, then LIVE notes (newest
  `updated` first), then CLOSED notes (newest first).
- Color: live rows green, closed rows dim/gray, the global row a distinct accent
  (e.g. cyan/yellow), the selected row reversed (existing). The `live`/`closed`
  word may stay as a short trailing tag or be dropped in favor of color — keep a
  short dim `closed` tag for the colorblind/greyscale case; live needs no word.
- Delete stays single-row `d` + `y/N`. No bulk key.

### 5. Go-to-tab (`g`)

- In List mode, `g` on a LIVE row: `ipc::call_text("tab.focus", {"tab_id": …})`,
  then close the overlay — you land in that tab. (After focusing another tab the
  Notes pane is no longer focused, so closing the overlay is the right end
  state.)
- `g` on a closed/unknown/global row: no-op (nowhere to focus).
- The classify already yields Live/Closed per row, so `g` is enabled iff Live.

### 6. Margin fix (row layout)

Current rows are left-flush (marker at the border) with a large right gap
because the name is a fixed 28-wide column while the box stretches to 60.

Fix: fill the inner width with balanced margins.
- 1-space left margin (before the marker) and 1-space right margin.
- Left segment: `{marker}{self_mark}{name}` (name truncated to a display-width
  budget using unicode-width, not char count).
- Right segment: the context + age, pinned to the right edge.
- Middle gap padded so left and right segments span the inner width evenly.
- Add display-width helpers in `app.rs` (`fn dwidth(&str) -> usize`, `fn
  truncate_w(&str, max) -> String`) using the already-imported
  `unicode_width::UnicodeWidthChar`.
- Apply the same 1-space margins to the box's top/bottom border titles.

## Overlay key map (List mode, after v2)

| Key | Action |
|-----|--------|
| ↑/↓ or k/j | move selection |
| enter | preview (regular rows) / switch to-or-from global (the `★` row) |
| g | go to the tab (live rows only) |
| r | rename highlighted note |
| d | delete highlighted note (y/N) |
| / | filter; type to narrow; Enter keep, Esc clear |
| esc / l | close overlay |

## Testing (TDD)

- `state.rs`: `global_path` selection (both layouts); `format_context`
  (workspace + agent, agent omitted, closed, unknown); `filter_rows`
  (case-insensitive title+context match, empty query = all); live-first-then-
  newest sort comparator; blank global note deletes `global.json` via
  `persist_at`.
- `app.rs`: `active` toggles Tab↔Global and save/load targets the right path
  (with `persist=false`, assert the in-memory switch + that the resolved path
  helper returns the global path); overlay `/` enters/edits/clears filter and
  selection clamps; `g` on a live row issues the focus (guard the ipc behind
  `persist` or a test seam so tests stay offline) and closes the overlay; `g` on
  closed/global is a no-op; row formatting fills width with balanced margins
  (assert the formatted string's display width == inner width for a sample).
- Esc still NEVER quits; overlay file writes still gated on `App.persist`.

## Non-goals

- No bulk cleanup of closed notes (explicit user decision).
- No deleting the global note from the list (clear via edit).
- No opening arbitrary orphan notes into the pane (only the fixed global note is
  switchable — bounded, avoids rebinding the pane to arbitrary tab files).
- No new dependencies.

## Global constraints (carry into the plan)

- `cargo test`, `cargo clippy --all-targets -- -D warnings`, `cargo build
  --release` all pass. Do NOT run `cargo build --release` while a Notes TUI is
  open (os error 5) — close the pane first.
- No new dependencies.
- Esc NEVER quits the TUI (only `q` in preview with no overlay/input). Verify
  across the new filter input and global switch.
- Pane border stays "Notes"; header shows one title (note's own, or `★ Global`)
  + mode + scroll — no duplicate "Notes".
- Forgiving parse (never panics); atomic save (temp + fsync + rename); path
  logic takes injected dirs so tests never touch real APPDATA or write into the
  repo; overlay file writes gated on `App.persist`.
- Column/margin math uses unicode-width (display columns), not char counts.
- Socket context calls are best-effort: any failure → Unknown, never a panic,
  the overlay still works offline.
