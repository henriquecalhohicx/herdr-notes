# Per-tab notes — design

Date: 2026-07-19

## Goal

Change the note's scope from **one note per herdr workspace** to **one note per
herdr tab**. All panes in a tab share that tab's note; each tab gets its own.

## Feasibility (verified against herdr 0.7.4-preview)

- `PaneInfo` in the socket API carries a `tab_id` field (already deserialized in
  `launch.rs`).
- herdr injects `HERDR_TAB_ID` into every managed pane, exactly like
  `HERDR_WORKSPACE_ID` (confirmed by dumping a live pane's env:
  `HERDR_TAB_ID=w1:t2`).
- A pane created by `pane split` lands in the focused tab and inherits that
  tab's `HERDR_TAB_ID` natively — the launcher does not pass it.
- Tab ids are of the form `<workspace>:<n>` (e.g. `w1:t2`). The `:` is
  filename-unsafe under the current `state::is_filename_safe`.
- Tab ids are monotonic and never reused: closing `w1:t3` and creating another
  tab yields `w1:t4`. The session's id counter persists across a server
  restart (`w1` survived; new work became `w2`). An orphaned note file can
  therefore never be reassigned to a future tab — no stale-content risk.

## Decisions

- **Migration: none.** Tabs start with a fresh, empty note. Existing
  per-workspace `*.json` files are left on disk for the user to delete. (User
  choice.)
- **Orphans: accepted.** A closed tab leaves a small, dead JSON file that never
  collides with a future tab (monotonic ids). Same model as today's
  per-workspace orphans, just more frequent. No auto-cleanup — reaping would
  require enumerating every tab across every workspace at each launch (extra
  herdr calls in the hot path) and could delete notes for tabs living in a
  session the launcher cannot see.
- **Out of scope (pre-existing):** the plugin state dir is shared across herdr
  sessions, so two sessions each having a `w1:t2` share one note file. Already
  true today for `w1` workspaces; not made worse here.

## Changes

### `src/state.rs`

- Key the note file on `HERDR_TAB_ID` instead of `HERDR_WORKSPACE_ID`. Rename
  `workspace_env()` → `tab_env()` reading the new var.
- `note_key` sanitizes the id: map `:` → `_`, so `w1:t2` → `w1_t2` (file
  `w1_t2.json`). Genuinely unsafe ids (empty, path-traversal shapes, spaces,
  other separators) still fall back to the legacy shared `notes.json`.
  Windows keeps its ASCII case-fold (NTFS is case-insensitive).
- `is_filename_safe` (or its replacement) admits `:` as the one extra allowed
  character; everything else stays rejected.
- No migration path added — first-load-migration logic that moved a legacy or
  per-workspace file into a slot is kept only for the legacy→key path already
  present; it does NOT bridge old per-workspace files into tab slots.

### `src/launch.rs`

- `same_note` compares `note_key(pane.tab_id)` instead of
  `note_key(pane.workspace_id)`.
- The separate same-tab preference in `launch_decision` collapses into the
  note-file match: keying by tab id already scopes to the tab, so the
  two-step `find(...tab_id == focused.tab_id).or_else(...any)` reduces to a
  single note-file match. A Notes pane in another tab is a different document
  and is ignored (each tab opens its own). The duplicate-instance guard still
  fires for two panes on one tab's note (last-writer-wins loss).

### Launcher scripts

- No key-related change. `HERDR_TAB_ID` is injected natively into the split
  pane; the `HERDR_PLUGIN_STATE_DIR` pass-through stays.

### Tests

- `state.rs`: ids like `w1:t2` now produce `w1_t2` (previously asserted
  unsafe → legacy). Update `state_path_keys_on_safe_*` and
  `note_key_mirrors_file_identity`. Add a case proving `:` is sanitized, not
  rejected, and that a real path-traversal id still hits legacy.
- `launch.rs`: matching flips from `workspace_id` to `tab_id`. Update the
  existing decision tests so panes in the same tab match and panes in other
  tabs of the same workspace no longer match (they are now separate notes).

### Docs

- Reword `CLAUDE.md`, `README.md`, and the module doc-comments in `state.rs`
  and `launch.rs` from workspace-scoped to tab-scoped where they describe the
  note-file key.

## Non-goals

- No auto-cleanup of orphaned note files.
- No migration of existing per-workspace notes.
- No cross-session note isolation (pre-existing behavior unchanged).
- No new dependencies; no architecture change.
