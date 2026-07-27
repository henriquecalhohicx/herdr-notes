# Note Template + Preview Checkbox Toggle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A fresh note opens with a Status/Next/Notes skeleton instead of a blank buffer, and a TODO can be ticked with one keypress from preview mode.

**Architecture:** Four layers, bottom up. `markdown.rs` gains a public checkbox API (find, count, toggle) and per-rendered-row source-line provenance so the TUI can map a screen row back to the line it came from. A new `template.rs` holds the skeleton as a single const. `app.rs` seeds that const into the buffer on first `e`, adds a `box_cursor` over the note's checkboxes driven by `j`/`k`/`space`, and shows note age in the header plus TODO progress in overlay rows. `state.rs` extends the delete-on-save blank rule to cover a seeded-but-untouched note.

**Tech Stack:** Rust 2024, ratatui + crossterm, `unicode-width`. No new dependencies.

## Global Constraints

- Phase A only. Prompt capture, auto-title, multi-pane grouping, a user-supplied `template.md`, and a `Done` section are all **out of scope** — do not build them.
- `cargo build --release`, `cargo test`, and `cargo clippy --all-targets -- -D warnings` must all pass before shipping.
- `cargo build --release` fails with os error 5 while the TUI runs in a pane. Close the pane first, and `Get-Process herdr-notes | Stop-Process` for stragglers.
- Esc must NEVER exit the TUI. Only `q` quits.
- Wrap and cursor math budget by display columns (`unicode-width`), never char count.
- The existing public behavior of `render_markdown(text, width) -> Vec<Line>` must not change — the overlay preview calls it and its tests must stay green untouched.

## Spec Corrections (found while planning — read before starting)

Two things in the spec do not survive contact with the code:

1. **The template's `[ ] ` lines are not checkboxes to the current parser.**
   `markdown::checkbox` at `src/markdown.rs:119` requires a `- ` or `* `
   bullet prefix. A bare `[ ] foo` falls through to the plain-paragraph
   branch. It *renders* identically (`- [ ] foo` renders as `[ ] foo`), which
   is why the mockup looks right, but it would carry no cursor and no count.
   Task 1 extends the parser to accept a bare `[ ]`, so the template can keep
   the bare form the user approved.

2. **Overlay progress is computed at draw time, not stored on `NoteSummary`.**
   The spec put `todo_done`/`todo_total` on the struct. `OverlayEntry` already
   carries the note's full `text` (`app.rs:39`), and only the visible rows are
   drawn, so counting in the draw loop is cheaper than two new fields and
   cannot go stale after an in-overlay rename. Task 6 does it that way.

One deliberate deviation: age is appended **after** the scroll hint in the
header (`[preview] — HM-54271  1/12  2h ago`), not before it as the mockup
showed. The spec fixes the drop-priority as "age first to go" when columns run
out, and the terminal clips from the right, so age must sit last.

---

## File Structure

- **Create `src/template.rs`** — one `pub const DEFAULT: &str`. Nothing else. Owning the skeleton in its own file keeps `state::is_blank` and `app::enter_edit` referencing one source of truth.
- **Modify `src/markdown.rs`** — public checkbox API (`checkbox_lines`, `checkbox_counts`, `toggle_checkbox`) and `render_markdown_mapped`. All checkbox knowledge lives here; nothing else in the crate may parse `[ ]`.
- **Modify `src/app.rs`** — `box_cursor` state, three preview keybindings, cursor highlight + scroll-follow in `draw_preview`, header age, footer, overlay progress, empty-note help.
- **Modify `src/state.rs`** — `is_blank` only.
- **Modify `src/main.rs`** — one `mod template;` line.
- **Modify `CLAUDE.md`** — living-doc updates.

---

### Task 1: Checkbox API in the markdown module

The renderer knows how to *draw* a checkbox but exposes nothing to find, count, or flip one. Add that, and widen the parser to accept a bullet-less `[ ]` so the template's lines count.

Fence awareness matters: a `[ ] foo` inside a ```` ``` ```` block is code, not a task, and must not be cursorable or counted.

**Files:**
- Modify: `src/markdown.rs:119-128` (the `checkbox` fn)
- Modify: `src/markdown.rs` (add public fns after `checkbox`)
- Test: `src/markdown.rs` (the existing `mod tests` at line 266)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub fn checkbox_lines(text: &str) -> Vec<(usize, bool)>` — `(source line index, is_done)` for every checkbox line, in source order, code fences skipped.
  - `pub fn checkbox_counts(text: &str) -> (usize, usize)` — `(done, total)`.
  - `pub fn toggle_checkbox(text: &str, line_idx: usize) -> Option<String>` — the whole text with that line's box flipped; `None` when `line_idx` is not a checkbox line.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/markdown.rs`:

```rust
    #[test]
    fn bare_checkboxes_parse_without_a_bullet() {
        // The template uses bullet-less boxes; they must render AND count.
        assert_eq!(texts("[ ] bare\n[x] done", 40), vec!["[ ] bare", "[x] done"]);
        assert_eq!(checkbox_counts("[ ] bare\n[x] done"), (1, 2));
        // Still works with the bullet forms.
        assert_eq!(checkbox_counts("- [ ] a\n* [x] b"), (1, 2));
        // A line that merely starts with a bracket is not a checkbox.
        assert_eq!(checkbox_counts("[link](url)"), (0, 0));
    }

    #[test]
    fn checkbox_lines_reports_source_indices_and_skips_fences() {
        let md = "## Next\n[ ] alpha\n\n```\n[ ] not a task\n```\n- [x] beta";
        assert_eq!(checkbox_lines(md), vec![(1, false), (6, true)]);
        assert_eq!(checkbox_counts(md), (1, 2));
    }

    #[test]
    fn checkbox_lines_finds_indented_boxes() {
        assert_eq!(checkbox_lines("  [ ] indented\n    - [x] deep"), vec![(0, false), (1, true)]);
    }

    #[test]
    fn toggle_checkbox_flips_only_the_target_line() {
        let md = "[ ] one\n[ ] two";
        assert_eq!(toggle_checkbox(md, 1).unwrap(), "[ ] one\n[x] two");
        assert_eq!(toggle_checkbox("- [x] done", 0).unwrap(), "- [ ] done");
        assert_eq!(toggle_checkbox("  [X] shouty", 0).unwrap(), "  [ ] shouty");
    }

    #[test]
    fn toggle_checkbox_rejects_non_checkbox_lines() {
        assert!(toggle_checkbox("plain text", 0).is_none());
        assert!(toggle_checkbox("[ ] one", 9).is_none(), "out of range");
        // A box inside a fence is code, not a task.
        assert!(toggle_checkbox("```\n[ ] fenced\n```", 1).is_none());
    }

    #[test]
    fn toggle_checkbox_preserves_a_trailing_blank_line() {
        // `.lines()` drops the trailing empty element, `split('\n')` keeps it —
        // the round-trip must not silently eat the note's final newline.
        assert_eq!(toggle_checkbox("[ ] a\n", 0).unwrap(), "[x] a\n");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib markdown`
Expected: FAIL — `cannot find function checkbox_counts in this scope` (and the same for `checkbox_lines` / `toggle_checkbox`).

- [ ] **Step 3: Widen the parser and add the public API**

Replace the body of `checkbox` at `src/markdown.rs:119-128` so a missing bullet is not fatal:

```rust
/// `[ ] rest` / `[x] rest`, optionally behind a `- ` or `* ` bullet (the
/// template writes them bare). A bare `[ ]` with no text counts too.
fn checkbox(t: &str) -> Option<(bool, &str)> {
    let rest = t.strip_prefix("- ").or_else(|| t.strip_prefix("* ")).unwrap_or(t);
    let (done, rest) = if let Some(r) = rest.strip_prefix("[ ]") {
        (false, r)
    } else {
        let r = rest.strip_prefix("[x]").or_else(|| rest.strip_prefix("[X]"))?;
        (true, r)
    };
    Some((done, rest.strip_prefix(' ').unwrap_or(rest)))
}
```

Add below it:

```rust
/// `(source line index, done)` for every checkbox line, in source order.
/// Lines inside a fenced code block are code and are skipped, matching what
/// `render_markdown` draws. Indices are `str::lines()` indices.
pub fn checkbox_lines(text: &str) -> Vec<(usize, bool)> {
    let mut out = Vec::new();
    let mut in_code = false;
    for (i, raw) in text.lines().enumerate() {
        let line = raw.trim_end();
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            continue;
        }
        if in_code {
            continue;
        }
        if let Some((done, _)) = checkbox(line.trim_start()) {
            out.push((i, done));
        }
    }
    out
}

/// `(done, total)` over every checkbox line — the overlay's progress column.
pub fn checkbox_counts(text: &str) -> (usize, usize) {
    let boxes = checkbox_lines(text);
    (boxes.iter().filter(|(_, done)| *done).count(), boxes.len())
}

/// `text` with the checkbox on `line_idx` flipped, or `None` when that line
/// is not a checkbox. Splits on `'\n'` rather than `lines()` so a trailing
/// newline survives the round-trip; the two index identically for every line
/// `lines()` yields, so a `checkbox_lines` index is safe here.
pub fn toggle_checkbox(text: &str, line_idx: usize) -> Option<String> {
    if !checkbox_lines(text).iter().any(|(i, _)| *i == line_idx) {
        return None;
    }
    let mut lines: Vec<String> = text.split('\n').map(String::from).collect();
    let line = lines.get(line_idx)?;
    let pos = line.find('[')?;
    let flipped = match line.get(pos..pos + 3)? {
        "[ ]" => "[x]",
        "[x]" | "[X]" => "[ ]",
        _ => return None,
    };
    lines[line_idx] = format!("{}{flipped}{}", &line[..pos], &line[pos + 3..]);
    Some(lines.join("\n"))
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib markdown`
Expected: PASS — the new tests plus the eight pre-existing markdown tests, all green. If `bullets_numbers_and_checkboxes` broke, the parser widening was too greedy; re-read Step 3.

- [ ] **Step 5: Commit**

```bash
git add src/markdown.rs
git commit -m "feat(markdown): public checkbox API, bullet-less boxes parse"
```

---

### Task 2: Rendered-row source-line provenance

`render_markdown` wraps by display width, so one source line can become three screen rows. To highlight "the row the cursor is on" and to scroll to it, the caller needs to know which source line produced each rendered row.

Additive only: `render_markdown` keeps its exact signature and becomes a one-line wrapper, so the overlay preview and all existing tests are untouched.

**Files:**
- Modify: `src/markdown.rs:14-38` (`render_markdown`)
- Test: `src/markdown.rs` (`mod tests`)

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces: `pub fn render_markdown_mapped(text: &str, width: usize) -> (Vec<Line<'static>>, Vec<Option<usize>>)` — the rendered rows plus a parallel vector, one entry per row, holding the `str::lines()` index that produced it (`None` only for the synthetic blank row emitted for empty input). The two vectors always have equal length.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/markdown.rs`:

```rust
    #[test]
    fn mapped_render_tags_every_row_with_its_source_line() {
        let (lines, map) = render_markdown_mapped("# One\n\n[ ] two", 40);
        assert_eq!(lines.len(), map.len(), "map is parallel to rows");
        assert_eq!(map, vec![Some(0), Some(1), Some(2)]);
    }

    #[test]
    fn wrapped_source_line_tags_all_of_its_rows() {
        // Narrow width forces the one checkbox onto several rows; every one of
        // them must point back at source line 0, or the cursor highlight would
        // light up half an item.
        let (lines, map) = render_markdown_mapped("[ ] alpha beta gamma delta", 12);
        assert!(lines.len() > 1, "should wrap: {} rows", lines.len());
        assert!(map.iter().all(|m| *m == Some(0)), "{map:?}");
    }

    #[test]
    fn mapped_render_matches_plain_render() {
        let md = "## Next\n[ ] a\n\n```\ncode\n```\n> quote";
        let plain = render_markdown(md, 30);
        let (mapped, map) = render_markdown_mapped(md, 30);
        assert_eq!(plain.len(), mapped.len());
        assert_eq!(map.len(), mapped.len());
    }

    #[test]
    fn mapped_render_of_empty_input_has_an_unmapped_row() {
        let (lines, map) = render_markdown_mapped("", 40);
        assert_eq!(lines.len(), 1);
        assert_eq!(map, vec![None]);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib markdown`
Expected: FAIL — `cannot find function render_markdown_mapped in this scope`.

- [ ] **Step 3: Split render_markdown into a wrapper plus the mapped form**

Replace `src/markdown.rs:14-38` with:

```rust
pub fn render_markdown(text: &str, width: usize) -> Vec<Line<'static>> {
    render_markdown_mapped(text, width).0
}

/// `render_markdown` plus, for each rendered row, the `str::lines()` index of
/// the source line that produced it. One source line can wrap to several rows,
/// which all carry the same index; the synthetic blank row emitted for empty
/// input carries `None`. Lets the preview map a screen row back to the note
/// text — needed by the checkbox cursor.
pub fn render_markdown_mapped(text: &str, width: usize) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
    let width = width.max(8);
    let mut out = Vec::new();
    let mut map: Vec<Option<usize>> = Vec::new();
    let mut in_code = false;
    for (src, raw) in text.lines().enumerate() {
        let line = raw.trim_end();
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            out.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(CODE).add_modifier(Modifier::DIM),
            )));
        } else if in_code {
            wrap_into(&mut out, vec![(line.to_string(), Style::default().fg(CODE))], width, 0);
        } else {
            render_line(&mut out, line, width);
        }
        // `out` only ever grows, so this fills exactly the rows this source
        // line just added.
        map.resize(out.len(), Some(src));
    }
    if out.is_empty() {
        out.push(Line::raw(""));
        map.push(None);
    }
    (out, map)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib markdown && cargo clippy --all-targets -- -D warnings`
Expected: PASS, no warnings.

- [ ] **Step 5: Commit**

```bash
git add src/markdown.rs
git commit -m "feat(markdown): render_markdown_mapped exposes row->source-line map"
```

---

### Task 3: Template module, lazy seed, and the blank rule

The template lands in the buffer on the first `e`, not at note creation — so a tab you merely toggle Notes into still writes no file. But seeding makes the buffer non-empty, which would defeat the existing delete-on-save rule and leave an orphan file for every note someone opened and walked away from. Tab ids are never reused, so those files are never reclaimed. `is_blank` therefore also treats the pristine template as blank.

The empty-note preview shows the skeleton dim, built from the same const so the two cannot drift.

**Files:**
- Create: `src/template.rs`
- Modify: `src/main.rs:8-12` (module list)
- Modify: `src/state.rs:341-344` (`is_blank`)
- Modify: `src/app.rs:29` (`EMPTY_HELP`), `src/app.rs:605-611` (`enter_edit`), `src/app.rs:810`
- Test: `src/state.rs` (`mod tests` at line 452), `src/app.rs` (`mod tests` at line 1103)

**Interfaces:**
- Consumes: nothing from Tasks 1-2.
- Produces: `template::DEFAULT: &str` — the skeleton, used by `state::is_blank`, `App::enter_edit`, and `app::empty_help`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/state.rs`:

```rust
    #[test]
    fn pristine_template_counts_as_blank() {
        let mut n = Note { text: crate::template::DEFAULT.to_string(), ..Note::default() };
        assert!(is_blank(&n), "seeded but untouched = nothing worth a file");
        n.text.push_str("shipped the thing");
        assert!(!is_blank(&n), "one edited char makes it a real note");
        n.text = crate::template::DEFAULT.to_string();
        n.title = "HM-54271".into();
        assert!(!is_blank(&n), "a title alone makes it a real note");
    }
```

Add to `mod tests` in `src/app.rs`:

```rust
    #[test]
    fn first_edit_seeds_the_template() {
        let mut a = app("");
        a.on_key(key(KeyCode::Char('e')));
        assert_eq!(a.note.text, crate::template::DEFAULT);
        assert_eq!(a.lines[a.row], "<one line: where this stands>", "cursor on the status line");
        assert_eq!(a.col, 0);
        assert!(a.dirty, "the seed must reach disk on the next flush");
    }

    #[test]
    fn edit_does_not_seed_over_existing_text() {
        let mut a = app("already written");
        a.on_key(key(KeyCode::Char('e')));
        assert_eq!(a.note.text, "already written");
    }

    #[test]
    fn empty_preview_shows_the_template_skeleton() {
        let mut a = app("");
        let screen = rendered(&mut a, 60, 24);
        assert!(screen.contains("## Status"), "{screen}");
        assert!(screen.contains("## Next"), "{screen}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test`
Expected: FAIL — `failed to resolve: could not find template in the crate root`.

- [ ] **Step 3: Add the module and wire it in**

Create `src/template.rs`:

```rust
//! The skeleton a fresh note is seeded with on its first edit. One const, so
//! `state::is_blank` (which treats the pristine template as nothing worth a
//! file) and the empty-note preview can never drift from what gets seeded.
//!
//! `Next` intentionally ships one empty bullet-less checkbox with a trailing
//! space — `markdown::checkbox` accepts the bare form, and the space puts the
//! edit cursor where the first task's text goes.

pub const DEFAULT: &str = "\
## Status
<one line: where this stands>

## Next
[ ] 

## Notes
";
```

In `src/main.rs`, add to the module list (keep it alphabetical):

```rust
mod state;
mod template;
```

In `src/state.rs`, replace `is_blank` at lines 341-344:

```rust
/// A note with no title and no text carries nothing worth a file — and
/// neither does one that is still the untouched seed template, or every tab
/// where someone pressed `e` once would leave an orphan file forever (tab ids
/// are never reused, so nothing reclaims them).
pub fn is_blank(note: &Note) -> bool {
    note.title.trim().is_empty()
        && (note.text.trim().is_empty() || note.text == crate::template::DEFAULT)
}
```

In `src/app.rs`, replace the `EMPTY_HELP` const at line 29 with a builder:

```rust
/// Shown in preview when the note is empty: the skeleton `e` would seed,
/// plus the quick-start help. Built from `template::DEFAULT` so the preview
/// cannot advertise a template different from the one that gets written.
fn empty_help() -> String {
    format!(
        "(empty note — press e to start with this template)\n\n{}\n\
         \n  e or Enter  start writing\
         \n  l           all notes\
         \n  q           quit\n\nEverything autosaves and survives restarts.",
        template::DEFAULT
    )
}
```

Add `use crate::template;` next to the other `use crate::` lines at `src/app.rs:20-21`.

At `src/app.rs:810`, swap the const for the call:

```rust
                Paragraph::new(empty_help()).style(Style::default().add_modifier(Modifier::DIM)),
```

Replace `enter_edit` at `src/app.rs:605-611`:

```rust
    fn enter_edit(&mut self) {
        // Lazy seed: a tab you merely toggled Notes into and never edited
        // still writes no file. `dirty` so the seed survives to the next
        // autosave; `is_blank` deletes it again if it stays untouched.
        if self.note.text.trim().is_empty() {
            self.note.text = template::DEFAULT.to_string();
            self.dirty = true;
            self.last_edit = Instant::now();
        }
        self.lines = self.note.text.split('\n').map(String::from).collect();
        // Land on the status placeholder — the first thing worth writing.
        self.row = self
            .lines
            .iter()
            .position(|l| l.starts_with('<'))
            .unwrap_or(0)
            .min(self.lines.len().saturating_sub(1));
        self.col = 0;
        self.edit_scroll = 0;
        self.note.mode = Mode::Edit;
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test`
Expected: PASS. If `q_quits_only_in_preview` or another empty-note test broke, it asserted on the old `(empty note)` string — update the assertion to the new copy, do not revert the help text.

- [ ] **Step 5: Commit**

```bash
git add src/template.rs src/main.rs src/state.rs src/app.rs
git commit -m "feat(notes): seed a Status/Next/Notes template on first edit"
```

---

### Task 4: Preview checkbox cursor

Ticking a box today means `e`, navigate, edit, `Esc` — the main friction in the workflow the template exists to serve. Add a cursor that hops between checkbox lines in preview mode.

`j`/`k`/`space` are all currently unbound in preview (`src/app.rs:397-411`), so nothing is being taken away. `Up`/`Dn`/`g`/`G` keep scrolling exactly as before.

**Files:**
- Modify: `src/app.rs:197-220` (`App` fields), `src/app.rs:230-253` (`with_note`)
- Modify: `src/app.rs:397-411` (preview keys), `src/app.rs:613-618` (`leave_edit`)
- Modify: `src/app.rs:806-826` (`draw_preview`), `src/app.rs:787-790` (footer)
- Test: `src/app.rs` (`mod tests`)

**Interfaces:**
- Consumes: `markdown::checkbox_lines`, `markdown::toggle_checkbox` (Task 1); `markdown::render_markdown_mapped` (Task 2).
- Produces: `App.box_cursor: Option<usize>` — an ordinal into `checkbox_lines`, not a source line index. Private; nothing after this task depends on it.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/app.rs`:

```rust
    #[test]
    fn j_k_walk_the_checkbox_cursor_and_clamp() {
        let mut a = app("## Next\n[ ] one\ntext\n[ ] two");
        assert_eq!(a.box_cursor, None, "no cursor until you ask for one");
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.box_cursor, Some(0));
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.box_cursor, Some(1));
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.box_cursor, Some(1), "clamps at the last box");
        a.on_key(key(KeyCode::Char('k')));
        assert_eq!(a.box_cursor, Some(0));
        a.on_key(key(KeyCode::Char('k')));
        assert_eq!(a.box_cursor, Some(0), "clamps at the first box");
    }

    #[test]
    fn k_from_no_cursor_starts_at_the_last_box() {
        let mut a = app("[ ] one\n[ ] two");
        a.on_key(key(KeyCode::Char('k')));
        assert_eq!(a.box_cursor, Some(1));
    }

    #[test]
    fn space_toggles_the_selected_box() {
        let mut a = app("[ ] one\n[ ] two");
        a.on_key(key(KeyCode::Char('j')));
        a.on_key(key(KeyCode::Char('j')));
        a.on_key(key(KeyCode::Char(' ')));
        assert_eq!(a.note.text, "[ ] one\n[x] two");
        assert!(a.dirty, "the toggle must reach disk on the next flush");
        a.on_key(key(KeyCode::Char(' ')));
        assert_eq!(a.note.text, "[ ] one\n[ ] two", "toggles back");
    }

    #[test]
    fn space_with_no_cursor_is_a_noop() {
        let mut a = app("[ ] one");
        a.on_key(key(KeyCode::Char(' ')));
        assert_eq!(a.note.text, "[ ] one");
        assert!(!a.dirty);
    }

    #[test]
    fn checkbox_keys_are_noops_without_checkboxes() {
        let mut a = app("just prose\nmore prose");
        for k in ['j', 'k', ' '] {
            a.on_key(key(KeyCode::Char(k)));
        }
        assert_eq!(a.box_cursor, None);
        assert_eq!(a.note.text, "just prose\nmore prose");
    }

    #[test]
    fn cursor_clamps_when_an_edit_removes_checkboxes() {
        let mut a = app("[ ] one\n[ ] two");
        a.on_key(key(KeyCode::Char('j')));
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.box_cursor, Some(1));
        a.note.text = "[ ] one".into(); // an edit deleted the second box
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.box_cursor, Some(0), "clamped to the surviving box");
        a.note.text = "no boxes left".into();
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.box_cursor, None);
    }

    #[test]
    fn leaving_edit_drops_a_cursor_with_nothing_to_point_at() {
        let mut a = app("[ ] one");
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.box_cursor, Some(0));
        a.on_key(key(KeyCode::Char('e')));
        a.lines = vec!["prose only".into()];
        a.on_key(key(KeyCode::Esc));
        assert_eq!(a.box_cursor, None);
    }

    #[test]
    fn cursor_scrolls_itself_into_view() {
        // 30 boxes in a 10-row body: the last one is far below the fold.
        let text: String = (0..30).map(|i| format!("[ ] item {i}\n")).collect();
        let mut a = app(&text);
        for _ in 0..30 {
            a.on_key(key(KeyCode::Char('j')));
        }
        let _ = rendered(&mut a, 40, 12); // 12 rows - header - hint = 10 body rows
        assert!(a.preview_scroll > 0, "draw must scroll the cursor into view");
    }

    #[test]
    fn preview_footer_falls_back_to_the_short_form_when_narrow() {
        let mut a = app("body");
        assert!(rendered(&mut a, 90, 8).contains("Up/Dn scroll"), "wide pane shows the full hints");
        let narrow = rendered(&mut a, 40, 8);
        assert!(narrow.contains("j/k spc tick"), "the new binding survives truncation: {narrow}");
        assert!(narrow.contains("q quit"), "quit must never be the thing that gets clipped: {narrow}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib app`
Expected: FAIL — `no field box_cursor on type App`.

- [ ] **Step 3: Implement the cursor**

Add the field to `App` after `preview_scroll` (`src/app.rs:204`):

```rust
    preview_scroll: usize,
    /// Ordinal into `markdown::checkbox_lines(&note.text)` — which checkbox
    /// the preview cursor sits on. NOT a source line index: the text can
    /// change under it, so it is re-resolved and re-clamped on every use.
    box_cursor: Option<usize>,
```

Initialize it in `with_note` beside `preview_scroll: 0` (`src/app.rs:237`):

```rust
            preview_scroll: 0,
            box_cursor: None,
```

Add the three helpers next to the other `App` methods (put them just above `enter_edit`):

```rust
    /// Source line of the selected checkbox, re-resolved against the current
    /// text so a stale ordinal can never point at the wrong line.
    fn cursor_line(&self) -> Option<usize> {
        let boxes = markdown::checkbox_lines(&self.note.text);
        boxes.get(self.box_cursor?).map(|(line, _)| *line)
    }

    /// Steps the checkbox cursor. From no cursor, `j` lands on the first box
    /// and `k` on the last. Clamps at both ends; clears when the note has no
    /// checkboxes left.
    fn move_box(&mut self, delta: isize) {
        let n = markdown::checkbox_lines(&self.note.text).len();
        if n == 0 {
            self.box_cursor = None;
            return;
        }
        self.box_cursor = Some(match self.box_cursor {
            None if delta > 0 => 0,
            None => n - 1,
            Some(c) => c.saturating_add_signed(delta).min(n - 1),
        });
    }

    /// Flips the selected checkbox straight in `note.text`. Preview mode never
    /// touches `lines`, so `commit` has nothing to overwrite this with — the
    /// existing debounce persists it.
    fn toggle_box(&mut self) {
        let Some(line) = self.cursor_line() else { return };
        let Some(text) = markdown::toggle_checkbox(&self.note.text, line) else { return };
        self.note.text = text;
        self.touch();
    }
```

Add the imports at `src/app.rs:20`:

```rust
use crate::markdown::{self, render_markdown, render_markdown_mapped};
```

(Delete the old `use crate::markdown::render_markdown;` line it replaces.)

Add the keys to the preview match, after the `KeyCode::Down` arm at `src/app.rs:403`:

```rust
            KeyCode::Char('j') => self.move_box(1),
            KeyCode::Char('k') => self.move_box(-1),
            KeyCode::Char(' ') => self.toggle_box(),
```

Re-clamp on the way out of edit — replace `leave_edit` at `src/app.rs:613-618`:

```rust
    fn leave_edit(&mut self) {
        self.commit();
        self.note.mode = Mode::Preview;
        self.dirty = false;
        // The edit may have deleted the box the cursor pointed at.
        let n = markdown::checkbox_lines(&self.note.text).len();
        self.box_cursor = match (self.box_cursor, n) {
            (_, 0) => None,
            (Some(c), _) => Some(c.min(n - 1)),
            (None, _) => None,
        };
        self.save();
    }
```

Replace the render half of `draw_preview` (`src/app.rs:817-825`) with the mapped form plus highlight and scroll-follow:

```rust
        // The rightmost column is reserved for the overflow scrollbar so text
        // never sits underneath it.
        let text_w = usize::from(area.width).saturating_sub(1).max(1);
        let (mut lines, map) = render_markdown_mapped(&self.note.text, text_w);
        let total = lines.len();
        let max = total.saturating_sub(usize::from(area.height));
        if let Some(src) = self.cursor_line() {
            // Highlight EVERY row of the selected item — a wrapped checkbox
            // spans several and would otherwise look half-selected.
            for (i, line) in lines.iter_mut().enumerate() {
                if map.get(i).copied().flatten() == Some(src) {
                    line.style = line.style.add_modifier(Modifier::REVERSED);
                }
            }
            // Scroll follows the cursor, mirroring the overlay's list clamp.
            if let Some(first) = map.iter().position(|m| *m == Some(src)) {
                let h = usize::from(area.height).max(1);
                if first < self.preview_scroll {
                    self.preview_scroll = first;
                } else if first >= self.preview_scroll + h {
                    self.preview_scroll = first + 1 - h;
                }
            }
        }
        self.preview_scroll = clamp_scroll(self.preview_scroll, total, usize::from(area.height));
        let scroll = u16::try_from(self.preview_scroll).unwrap_or(u16::MAX);
        frame.render_widget(Paragraph::new(lines).scroll((scroll, 0)), area);
        draw_scrollbar(frame, area, max, self.preview_scroll);
        (max > 0).then(|| format!("{}/{total}", self.preview_scroll + 1))
```

Replace the footer at `src/app.rs:787-790`:

```rust
        // The full hint line no longer fits a narrow right dock; clipping it
        // would eat `q quit`, so fall back to a short form instead.
        const PREVIEW_HINTS: &str =
            " e edit  j/k spc tick  r title  l list  Up/Dn scroll  x clear  q quit";
        const PREVIEW_HINTS_SHORT: &str = " e edit  j/k spc tick  l list  q quit";
        let hints = match self.note.mode {
            Mode::Preview => {
                if usize::from(hint_a.width) >= PREVIEW_HINTS.chars().count() {
                    PREVIEW_HINTS
                } else {
                    PREVIEW_HINTS_SHORT
                }
            }
            Mode::Edit => " Esc preview (saves)   Ctrl+S save",
        };
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS, no warnings. If `preview_scroll_keys_move_and_clamp_at_top` broke, the scroll-follow ran with no cursor set — it must be inside the `if let Some(src)` block.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "feat(notes): j/k + space tick checkboxes from preview mode"
```

---

### Task 5: Note age in the header

Coming back to a tab after an hour, the first question is how stale what you are reading is. `note.updated` and `state::format_age` both already exist.

Age goes last, after the scroll hint, so it is the first thing the terminal clips when columns run out.

**Files:**
- Modify: `src/app.rs:778-783` (the scroll-hint span in `draw`)
- Test: `src/app.rs` (`mod tests`)

**Interfaces:**
- Consumes: nothing from Tasks 1-4.
- Produces: nothing consumed later.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/app.rs`:

```rust
    #[test]
    fn header_shows_the_note_age() {
        let mut a = app("body");
        a.note.updated = state::unix_now().saturating_sub(2 * 60 * 60);
        let screen = rendered(&mut a, 60, 8);
        assert!(screen.contains("2h ago"), "{screen}");
    }

    #[test]
    fn header_omits_age_for_a_note_with_no_timestamp() {
        let mut a = app("body");
        a.note.updated = 0; // a v1 file, before `updated` existed
        let screen = rendered(&mut a, 60, 8);
        assert!(!screen.contains("ago"), "{screen}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib app::tests::header`
Expected: FAIL — `assertion failed: screen.contains("2h ago")`.

- [ ] **Step 3: Append the age span**

In `src/app.rs`, directly after the `if let Some(hint) = scroll_hint { ... }` block (line 783) and still inside the `else` branch:

```rust
            // Last, so it is the first thing clipped when the dock is narrow.
            if self.note.updated > 0 {
                let age = state::format_age(state::unix_now().saturating_sub(self.note.updated));
                title.push(Span::styled(
                    format!("  {age} ago"),
                    Style::default().add_modifier(Modifier::DIM),
                ));
            }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "feat(notes): show note age in the header"
```

---

### Task 6: TODO progress in overlay rows

The cross-session dashboard should answer "how far did that tab get" without opening each note. `OverlayEntry` already carries the note's full `text`, and only the visible rows are drawn, so the count is computed in the draw loop — no new fields, and no staleness after an in-overlay rename.

`format_row` already truncates the name to protect the right segment, so the extra columns cannot overflow the box.

**Files:**
- Modify: `src/app.rs:981-983` (the row's right segment)
- Test: `src/app.rs` (`mod tests`)

**Interfaces:**
- Consumes: `markdown::checkbox_counts` (Task 1).
- Produces: nothing consumed later.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/app.rs`:

```rust
    #[test]
    fn overlay_rows_show_todo_progress() {
        let mut a = app("body");
        let mut e = entry_with_tab("Busy Tab", state::TabStatus::Live, "w1:t1");
        e.text = "[ ] one\n[x] two\n[x] three".into();
        a.overlay = Some(Overlay::from_entries(vec![e]));
        let screen = rendered(&mut a, 70, 14);
        assert!(screen.contains("2/3"), "{screen}");
    }

    #[test]
    fn overlay_rows_omit_progress_when_the_note_has_no_boxes() {
        let mut a = app("body");
        let mut e = entry_with_tab("Prose Tab", state::TabStatus::Live, "w1:t1");
        e.text = "no tasks here".into();
        a.overlay = Some(Overlay::from_entries(vec![e]));
        let screen = rendered(&mut a, 70, 14);
        assert!(!screen.contains("0/0"), "{screen}");
    }

    #[test]
    fn overlay_row_with_progress_still_fits_the_box() {
        let row = format_row(">", "*", "A Very Long Note Title Indeed", "spec-droid · claude  2/3  2h", 40);
        assert_eq!(dwidth(&row), 40);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib app::tests::overlay_rows`
Expected: FAIL — `assertion failed: screen.contains("2/3")`.

- [ ] **Step 3: Add the progress segment**

Replace `src/app.rs:981-982`:

```rust
                let age = if e.updated == 0 { "—".to_string() } else { state::format_age(now.saturating_sub(e.updated)) };
                // Counted per draw off the row's own text (only visible rows
                // are drawn) rather than cached, so an in-overlay rename or
                // delete can never leave a stale count behind.
                let (done, total) = markdown::checkbox_counts(&e.text);
                let progress = if total > 0 { format!("  {done}/{total}") } else { String::new() };
                let right = format!("{}{progress}  {age}", e.context);
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS, no warnings.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "feat(overlay): show TODO progress per row"
```

---

### Task 7: Full verification and living-doc update

Unit tests cannot see the cursor highlight, the footer at a real dock width, or whether the seeded template actually reaches disk. Drive the real binary once, then record what the code now assumes.

**Files:**
- Modify: `CLAUDE.md` (the `src/` layout bullets and Gotchas)
- Test: manual, in a throwaway pane

**Interfaces:**
- Consumes: everything from Tasks 1-6.
- Produces: nothing.

- [ ] **Step 1: Run the full gate**

Close any open Notes pane first (`cargo build --release` fails with os error 5 while the TUI is running), then:

```bash
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
```

Expected: all three clean. `Get-Process herdr-notes | Stop-Process` if the build hits os error 5.

- [ ] **Step 2: Drive the real binary end to end**

In a throwaway pane, per the repo's verification recipe: `herdr pane split`, `pane run` the release binary, then `pane send-keys` and `pane read --source visible`. Check, in order:

1. Fresh tab, preview shows `(empty note — press e to start with this template)` and the dim skeleton.
2. `e` seeds the template with the cursor on the status line; `Esc`; `q`. No file appears at `%APPDATA%\herdr\notes\<tab-key>.json` (the pane's `HERDR_TAB_ID` with `:` sanitized to `_`) — the pristine-template blank rule fired.
3. Reopen, `e`, type a status line and two `[ ]` tasks, `Esc`. The file now exists.
4. `j` highlights the first task, `j` again the second, `space` ticks it green, `k` `space` unticks. The highlight covers the whole item when it wraps — narrow the pane to force a wrap and re-check.
5. `l` — the row shows `1/2`.
6. Header shows the age; the footer at the real dock width still ends in `q quit`.

- [ ] **Step 3: Update CLAUDE.md**

In the `src/app.rs` bullet, after the in-note title editing clause, add:

```
  Preview also carries a checkbox cursor (`j`/`k` hop between checkbox
  lines, `space` flips the box straight in `note.text`); a fresh note is
  seeded with `template::DEFAULT` on the first `e` (lazy — a tab you only
  toggled Notes into writes no file), and the header shows the note's age.
```

Add a `src/template.rs` bullet after the `src/state.rs` one:

```
- `src/template.rs` — the Status/Next/Notes skeleton, one const. `is_blank`
  treats the pristine template as blank, so seeding cannot leak orphan files.
```

In the `src/markdown.rs` bullet, note the new API:

```
  Also the crate's ONLY checkbox parser: `checkbox_lines`/`checkbox_counts`/
  `toggle_checkbox` (fence-aware) and `render_markdown_mapped`, which returns
  a per-rendered-row source-line map.
```

In the Gotchas "Learned building this plugin" list, add:

```
- The markdown checkbox parser accepts a BARE `[ ]` as well as `- [ ]` /
  `* [ ]`. It did not originally, which made the seed template's own tasks
  invisible to the cursor and the progress count while rendering identically
  (`- [ ] x` renders as `[ ] x`, so the bug was invisible on screen). Any
  second `[ ]` scan added anywhere else in the crate will drift from this one
  — count and toggle through `markdown::` only.
- Seeding a template into a fresh note re-opens the orphan-file hole the
  blank-note delete rule closed: the buffer is no longer empty, so the file
  persists for every tab where someone pressed `e` once and walked away, and
  tab ids are never reused to reclaim it. `is_blank` therefore also matches
  the pristine template exactly.
- One source line can render to several rows (width wrapping), so anything
  mapping a screen row back to the note text needs `render_markdown_mapped`,
  not row arithmetic. Highlight ALL rows of a wrapped item or it looks
  half-selected.
```

- [ ] **Step 4: Verify the docs match the code**

Re-read each CLAUDE.md claim against the diff — file paths, function names, and key bindings must all exist as written.

- [ ] **Step 5: Commit**

```bash
git add CLAUDE.md
git commit -m "docs: record the template, checkbox cursor, and their gotchas"
```

---

## Self-Review

**Spec coverage:**

| Spec section | Task |
|---|---|
| 1. Template + lazy seed | 3 |
| 1. Blank rule (pristine template) | 3 |
| 2. Renderer provenance | 2 |
| 2. `box_cursor`, `j`/`k`, `space`, highlight, scroll-follow, re-entry clamp | 4 |
| 2. Footer | 4 |
| 3. Age in the header (omit when `updated == 0`, drops first) | 5 |
| 4. Overlay TODO progress, single checkbox parser | 1 + 6 |
| Testing: all nine listed cases | 1, 3, 4, 5, 6 |
| Testing: build/test/clippy + end-to-end | 7 |

No gaps. Two spec deviations are documented in "Spec Corrections" above, plus the bare-`[ ]` parser widening the spec did not anticipate.

**Placeholder scan:** none — every step carries the code it needs.

**Type consistency:** `checkbox_lines -> Vec<(usize, bool)>` is consumed as `(line, _)` in `cursor_line`, as `.len()` in `move_box`/`leave_edit`, and via `checkbox_counts` in the overlay. `render_markdown_mapped -> (Vec<Line>, Vec<Option<usize>>)` is destructured as `(mut lines, map)` and indexed with `map.get(i).copied().flatten()`. `template::DEFAULT` is `&str`, `.to_string()`d at both assignment sites and compared with `==` against `String` in `is_blank`. `box_cursor` is an ordinal everywhere; only `cursor_line` converts it to a source line.
