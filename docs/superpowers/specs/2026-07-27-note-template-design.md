# Note template + preview checkbox toggle — design

Date: 2026-07-27

## Goal

Make a fresh note useful without typing structure by hand, and make ticking a
TODO a single keypress from preview mode. Serves the core use case: come back to
a tab after an hour away and read, in one glance, where the work stands and
what is left.

Builds on the shipped notes-v2 (per-tab notes, `title`/`tab_id`/`created`/
`updated`, `l` list overlay with context/filter/global row). All code lives in
`src/template.rs` (new), `src/app.rs`, `src/state.rs`, `src/markdown.rs`.

## Scope

This is **phase A** of three. The full idea the user brought was three
independent subsystems; they are specced and built separately:

- **A (this spec)** — template, preview checkbox toggle, header age, overlay
  TODO progress. Self-contained, no new processes, no new files on disk.
- **B (later)** — auto-capture of the last N prompts per tab, written by a
  Claude Code `UserPromptSubmit` hook into a *separate* `<tab>.prompts.json`,
  rendered read-only by the TUI. Not started.
- **C (later)** — auto-default title and multi-pane prompt grouping. Depends on
  B existing.

## User decisions (approved)

- **Lazy seed.** Template lands in the buffer on first `e`, not on note
  creation. A tab you merely toggle Notes into still writes no file.
- **Sections: `Status` / `Next` / `Notes`.** No `Done` section — finished items
  stay as checked boxes in `Next`.
- **No `Last Prompts` heading in the template.** Prompts (phase B) are
  structured data rendered above the buffer, never text inside it. Two writers
  on one text document would clobber each other: the TUI autosaves its whole
  in-memory buffer every 2s and would erase anything a hook appended to the
  file.
- **`j`/`k` + `space`** for the preview checkbox cursor. `Up`/`Dn` keep
  scrolling unchanged.
- **Built-in hardcoded template.** No user-supplied `template.md` in this phase.
- Extras included: age in the header, TODO progress in overlay rows.
- Extra dropped: age column in overlay rows — **already shipped**
  (`app.rs:981-982`, the right segment is already `context  2h`).

## Features

### 1. Template + lazy seed

New `src/template.rs`:

```rust
pub const DEFAULT: &str = "\
## Status
<one line: where this stands>

## Next
[ ] 

## Notes
";
```

Seed point: `App::enter_edit()`, when `note.text` is whitespace-empty. The edit
cursor is placed at the start of the `<one line: where this stands>` line so the
first keystroke replaces the placeholder region the user is looking at.

> **SUPERSEDED by the final branch review (human partner's ruling).** Edit mode
> has no line-kill, word-delete or selection, so nothing "replaces" the
> placeholder — removing it costs `End` plus 29 Backspaces on every new note.
> The placeholder line is now EMPTY and the seed path lands the cursor on it
> (line index 1). See `src/template.rs` and `CLAUDE.md`.

`App.note` is whichever buffer is active (`ActiveNote::Tab | Global`), so the
global note gets the same treatment with no extra code. This is deliberate.

**Blank rule.** Lazy seeding removes the toggle-a-pane orphan but not the
press-`e`-and-walk-away orphan: after seeding, the buffer is no longer empty, so
the existing delete-on-save rule stops firing and the file persists forever (tab
ids are never reused, so nothing reclaims it). `state::is_blank` therefore grows
one comparison:

```
is_blank(note) = title.trim().is_empty()
              && (text.trim().is_empty() || text == template::DEFAULT)
```

Exact-match against the pristine constant only. A note the user seeded and then
edited by even one character is a real note and is kept.

### 2. Preview checkbox cursor

Today preview mode has a scroll offset and no notion of a line. Ticking a box
means `e`, navigate, edit, `Esc` — the main friction in the workflow this
template exists to serve.

**Renderer provenance.** `markdown.rs` wraps by display width, so a rendered row
does not correspond 1:1 to a source line. The renderer must emit, per rendered
row, the index of the source line it came from. A checkbox wrapped across three
rows yields three rows all tagged with the same source line.

**State.** `App` gains `box_cursor: Option<usize>` — an index into the note's
checkbox source lines, in source order.

- `j` / `k` step the cursor. No checkboxes in the note → both are no-ops.
- The selected checkbox's rendered rows are highlighted — *all* of them, so a
  wrapped item does not look half-selected.
- `space` flips `[ ]` ↔ `[x]` on that source line in `note.text` and marks the
  buffer dirty; the existing 2s debounce persists it. No new save path.
- Preview scroll clamps to keep the cursor's rows visible, mirroring the
  overlay's existing `list_scroll` clamp.
- `Up`/`Dn`, `g`/`G`, and every other preview binding are unchanged.
- Re-entering preview from edit keeps the cursor index if it is still in range,
  clamps to the last checkbox otherwise, and clears to `None` when the note has
  no checkboxes left.

**Footer.** Gains `j/k box  space tick`. The footer is already tight in a right
dock; its behavior at narrow widths must be checked and truncated rather than
allowed to overflow.

### 3. Age in the header

Header becomes `[preview] — HM-54271 · 2h ago` using `note.updated` and the
existing `state::format_age`. `updated == 0` (a v1 file with no timestamp) →
omit the age entirely rather than print a bogus one.

Header width priority when columns run out: mode, then title, then scroll
position, then age. Age is dropped first.

### 4. TODO progress in overlay rows

`state::NoteSummary` gains `todo_done` and `todo_total`, counted in
`list_notes`. The overlay's right segment becomes `{context}  2/5  {age}`; the
count is omitted when `todo_total == 0`.

**The count must reuse the checkbox parser in `markdown.rs`**, not a second
ad-hoc scan. Two parsers drift, and then the dashboard count silently disagrees
with what the note renders.

`format_row` already truncates the name to protect the right segment, so there
is no width bug — but in a narrow dock the title is squeezed harder than today.
Verify live before shipping.

## Testing

TDD, per repo norms. Unit tests:

- seeds on whitespace-empty text; does not seed when text is present
- pristine template + empty title → deleted on save; template + one edited char
  → kept
- `space` flips the correct source line when the checkbox is wrapped across
  multiple rendered rows
- `j`/`k`/`space` are no-ops and do not panic on a note with zero checkboxes
- cursor index clamps correctly after an edit removes checkboxes
- progress count agrees with the markdown parser on plain, nested, and indented
  checkbox lines
- header omits age when `updated == 0`
- `format_row` still pads to exactly `inner_width` with context + progress + age

`cargo build --release`, `cargo test`, and
`cargo clippy --all-targets -- -D warnings` must all pass. End-to-end check in a
throwaway pane per the repo's verification recipe, since the cursor highlight
and footer truncation are not unit-testable.

## Out of scope

Prompt capture (phase B), auto-title and multi-pane grouping (phase C),
user-supplied `template.md`, a `Done` section.
