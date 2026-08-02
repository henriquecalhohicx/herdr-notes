# Title Links In The Cycle Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Put the header title's links into the `n`/`N` cursor as the first ordinals and make bare `o` mean one thing — open hit 0 — replacing the title-only special case.

**Architecture:** `LinkHit.row` becomes `Option<usize>` so a header hit, which has no scrollable row, can live in the same list as block and body hits. `draw` scans the title's links before rendering the body (the body renders first because it returns the scroll hint the title line shows) and hands the count to `draw_preview` as the offset its own ordinals sit behind. `pending_open` then collapses to "cursored hit, else `link_hits.first()`".

**Tech Stack:** Rust 2024, ratatui 0.30, crossterm 0.29, serde_json, unicode-width. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-07-29-title-links-in-the-cycle-design.md`

## Global Constraints

- No new crate dependencies. Rust edition 2024.
- `cargo build --release`, `cargo test`, `cargo clippy --all-targets -- -D warnings` must all pass before a task is done. `cargo test --lib` does NOT work — bin-only crate; use `cargo test` or `cargo test <name>`. `cargo build --release` fails with os error 5 while a Notes TUI is running in a pane — say so rather than killing the user's process.
- Ordinal order is title → prompt block → body, and it is the order `n`/`N` walk.
- Each region applies its own `REVERSED`: the title in `draw`, the block in `prompt_block`, the body inside `render_markdown_links`. Exactly one region may claim a given ordinal.
- Title hits exist ONLY when a title is actually rendered: not in Global mode (the header shows `★ Global`), not while the title editor is open (`title_input.is_some()`), not when the title is blank. This is what keeps bare `o` from reaching text the user was never shown — by construction, not by a gate in `pending_open`.
- A rowless hit never moves `preview_scroll`.
- Widths are display COLUMNS (`dwidth`), never chars. The header's ALL-OR-NOTHING age token measures `dwidth` summed over every span already assembled, so splitting the title into more spans is fine only if the total width is unchanged.
- Every failure path stays a silent no-op: no printing from the TUI, no panics, no `unwrap` on user-derived data. Esc must NEVER exit the TUI.
- `markdown::find_links` stays the crate's ONLY link scan.

---

### Task 1: a hit's row becomes optional

**Files:**
- Modify: `src/markdown.rs` (`LinkHit` ~14-21, `emit`'s hit push)
- Modify: `src/app.rs` (`block_line`'s hit push, `draw_preview`'s body row shift ~1690-1693, the `follow_link` block ~1737-1747)
- Test: inline test modules in both files

**Interfaces:**
- Produces: `pub struct LinkHit { pub text: String, pub kind: LinkKind, pub row: Option<usize> }` — `None` means the hit lives in a region with no scrollable row. Every hit produced in THIS task still carries `Some(row)`; Task 2 is the first to produce `None`.

- [ ] **Step 1: Write the failing test**

Add to `src/app.rs`'s test module:

```rust
    #[test]
    fn block_and_body_hits_always_carry_a_row() {
        // Only the header title is rowless (Task 2). Anything rendered into the
        // scrollable body must keep a row, or the cursor's scroll-follow has
        // nothing to aim at.
        let mut a = ticket_app("body HM-2 here\n");
        a.prompts = vec![crate::prompts::PromptGroup {
            pane: "w1:p5".into(),
            prompts: vec![prompt(1, "prompt HM-1 here")],
        }];
        a.prompt_labels = vec!["claude p5".into()];
        rendered(&mut a, 60, 20);
        assert_eq!(a.link_hits.len(), 2);
        assert!(a.link_hits.iter().all(|h| h.row.is_some()), "{:?}", a.link_hits);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test block_and_body_hits_always_carry_a_row 2>&1 | head -20`
Expected: FAIL to compile — `no method named is_some` on `usize`.

- [ ] **Step 3: Write the implementation**

In `src/markdown.rs`, change the field and its doc:

```rust
/// One openable target found while rendering: its text, what kind of target it
/// is, and the rendered row its first character landed on — `None` for a hit in
/// a region that has no scrollable row (the header title, whose 1-row
/// `Paragraph` sits outside the body). Document order == the order `n`/`N` walk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkHit {
    pub text: String,
    pub kind: LinkKind,
    pub row: Option<usize>,
}
```

In `emit`, wrap the row: `ctx.hits.push(LinkHit { text, kind, row: Some(base + row) });`

In `src/app.rs`'s `block_line`, wrap it the same way: `hits.push(markdown::LinkHit { text: ..., kind, row: Some(row) });` (keep the surrounding truncation-guard code untouched).

In `draw_preview`'s body row shift:

```rust
                    for hit in &mut hits {
                        hit.row = hit.row.map(|r| r + n);
                    }
```

In the `follow_link` block, skip a rowless hit:

```rust
        if self.follow_link {
            // A rowless hit (the header title) has nothing to scroll to.
            if let Some(row) = self
                .link_cursor
                .and_then(|c| self.link_hits.get(c))
                .and_then(|h| h.row)
            {
```

Then repoint every test that reads or builds a `.row`. There are assertions comparing `.row` to a number (`the_prompt_block_offsets_ticket_rows`, `hits_carry_the_row_they_landed_on`, `a_titled_body_less_note_still_carries_its_block_hits`) and `LinkHit` literals in `src/markdown.rs`'s tests. Mechanical: compare against `Some(n)`, or unwrap with `.expect("body hit has a row")` in test code only. Keep every assertion's meaning identical.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test`
Expected: PASS — the whole suite, with no assertion's meaning changed.
Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/markdown.rs src/app.rs
git commit -m "refactor(notes): a link hit's row is optional, for regions with no row"
```

---

### Task 2: title links join the cursor

**Files:**
- Modify: `src/app.rs` (`draw` ~1480-1545, `draw_preview` signature and its hit assembly ~1615-1700)
- Test: inline test module in `src/app.rs`

**Interfaces:**
- Consumes: Task 1's `LinkHit { text, kind, row: Option<usize> }`.
- Produces: `App.link_hits` ordered title → block → body; `draw_preview(&mut self, frame: &mut Frame, area: Rect, title_hits: &[markdown::LinkHit]) -> Option<String>`. Task 3 relies on hit 0 being the title's link when the title has one.

- [ ] **Step 1: Write the failing tests**

```rust
    /// Reversed cells on one row of a rendered frame, as text.
    fn reversed_on_row(a: &mut App, w: u16, h: u16, row: u16) -> String {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| a.draw(f)).unwrap();
        let buf = term.backend().buffer();
        (0..w)
            .filter_map(|x| buf.cell((x, row)))
            .filter(|c| c.modifier.contains(Modifier::REVERSED))
            .map(|c| c.symbol().to_string())
            .collect()
    }

    /// Every reversed cell in a rendered frame, as text.
    fn reversed_all(a: &mut App, w: u16, h: u16) -> String {
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(w, h)).unwrap();
        term.draw(|f| a.draw(f)).unwrap();
        let buf = term.backend().buffer();
        (0..h)
            .flat_map(|y| (0..w).map(move |x| (x, y)))
            .filter_map(|(x, y)| buf.cell((x, y)))
            .filter(|c| c.modifier.contains(Modifier::REVERSED))
            .map(|c| c.symbol().to_string())
            .collect()
    }

    #[test]
    fn n_from_cold_lands_on_the_titles_link() {
        let mut a = ticket_app("body HM-2 here\n");
        a.note.title = "titled HM-1".into();
        rendered(&mut a, 60, 10);
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.link_cursor, Some(0));
        assert_eq!(a.link_hits[0].text, "HM-1");
        assert_eq!(a.link_hits[0].row, None, "the header has no scrollable row");
        // Row 0 is the header.
        assert_eq!(reversed_on_row(&mut a, 60, 10, 0), "HM-1");
    }

    #[test]
    fn both_links_in_a_title_are_reachable() {
        let mut a = ticket_app("body only\n");
        a.note.title = "HM-1 and HM-2".into();
        rendered(&mut a, 60, 10);
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.pending_open().as_deref(), Some("https://example.test/browse/HM-1"));
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.pending_open().as_deref(), Some("https://example.test/browse/HM-2"));
        assert_eq!(reversed_on_row(&mut a, 60, 10, 0), "HM-2");
    }

    #[test]
    fn ordinals_walk_title_then_block_then_body() {
        // The load-bearing test: three regions, one list, and exactly one
        // region highlighted per ordinal. This is the third change to the
        // ordinal offset and every previous bug hid in the two-list agreement.
        let mut a = ticket_app("body HM-3 here\n");
        a.note.title = "titled HM-1".into();
        a.prompts = vec![crate::prompts::PromptGroup {
            pane: "w1:p5".into(),
            prompts: vec![prompt(1, "prompt HM-2 here")],
        }];
        a.prompt_labels = vec!["claude p5".into()];
        rendered(&mut a, 60, 20);
        assert_eq!(
            a.link_hits.iter().map(|h| h.text.as_str()).collect::<Vec<_>>(),
            ["HM-1", "HM-2", "HM-3"]
        );
        for (ordinal, expected, others) in
            [(0usize, "HM-1", ["HM-2", "HM-3"]), (1, "HM-2", ["HM-1", "HM-3"]), (2, "HM-3", ["HM-1", "HM-2"])]
        {
            a.link_cursor = Some(ordinal);
            let reversed = reversed_all(&mut a, 60, 20);
            assert!(reversed.contains(expected), "ordinal {ordinal}: {reversed:?}");
            for other in others {
                assert!(!reversed.contains(other), "ordinal {ordinal} also reversed {other}");
            }
            assert!(a.pending_open().unwrap().ends_with(expected));
        }
    }

    #[test]
    fn selecting_a_title_link_does_not_scroll_the_body() {
        let mut a = ticket_app(&format!("{}HM-2 at the bottom\n", "filler\n".repeat(40)));
        a.note.title = "titled HM-1".into();
        rendered(&mut a, 60, 10);
        a.on_key(key(KeyCode::Char('n')));
        rendered(&mut a, 60, 10);
        assert_eq!(a.link_cursor, Some(0));
        assert_eq!(a.preview_scroll, 0, "a rowless hit has nothing to scroll to");
    }

    #[test]
    fn the_global_note_contributes_no_title_hits() {
        let mut a = ticket_app("global body HM-2\n");
        a.note.title = "titled HM-1".into();
        a.active = ActiveNote::Global;
        rendered(&mut a, 60, 10);
        assert_eq!(
            a.link_hits.iter().map(|h| h.text.as_str()).collect::<Vec<_>>(),
            ["HM-2"],
            "the header shows ★ Global and never a title, so its links are not on screen"
        );
    }

    #[test]
    fn the_open_title_editor_contributes_no_title_hits() {
        let mut a = ticket_app("body HM-2 here\n");
        a.note.title = "titled HM-1".into();
        a.title_input = Some("titled HM-1".into());
        rendered(&mut a, 60, 10);
        assert_eq!(
            a.link_hits.iter().map(|h| h.text.as_str()).collect::<Vec<_>>(),
            ["HM-2"],
            "the header shows the editor, not the title"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test n_from_cold_lands_on_the_titles_link 2>&1 | head -20`
Expected: FAIL — `link_hits[0].text` is `HM-2` (the body's), and nothing is reversed on row 0.

- [ ] **Step 3: Write the implementation**

In `draw`, before the body render, scan the title and build its hits:

```rust
        // Title links are the FIRST ordinals — but the BODY renders first,
        // because the preview returns the scroll hint this line displays. So
        // scan the title NOW (a pure `find_links`, no rendering) and hand the
        // count to the preview as the offset its own ordinals sit behind.
        //
        // Only when a title is actually on screen: never in Global mode (the
        // header shows `★ Global`), never while the title editor is open, never
        // for a blank title. That is what stops bare `o` opening text the user
        // was never shown — by construction, not by a gate in `pending_open`.
        let title_links: Vec<(std::ops::Range<usize>, markdown::LinkKind)> = if self
            .title_input
            .is_none()
            && self.active != ActiveNote::Global
            && !self.note.title.trim().is_empty()
        {
            markdown::find_links(&self.note.title, &self.tickets)
        } else {
            Vec::new()
        };
        let title_hits: Vec<markdown::LinkHit> = title_links
            .iter()
            .map(|(range, kind)| markdown::LinkHit {
                text: self.note.title[range.clone()].to_string(),
                kind: *kind,
                row: None, // the header is a 1-row Paragraph outside the body
            })
            .collect();

        // Body first: the preview reports a scroll hint for the title line.
        let (mode, scroll_hint) = match self.note.mode {
            Mode::Preview => ("preview", self.draw_preview(frame, body_a, &title_hits)),
            Mode::Edit => {
                self.draw_edit(frame, body_a);
                ("edit", None)
            }
        };
```

Replace the title-span assembly in the `else if !self.note.title.trim().is_empty()` arm with one that slices the RAW title (so `title_links`' offsets apply directly) and emits the leading space as its own span, keeping the assembled width identical to the old `format!(" {}", title)`:

```rust
            } else if !self.note.title.trim().is_empty() {
                title.push(Span::raw(" —"));
                let bold = Style::default().add_modifier(Modifier::BOLD);
                // The leading space is its own span so `title_links`' offsets
                // index the raw title with no shift. Same total width as the
                // old single ` {title}` span, which the age token's
                // ALL-OR-NOTHING measurement below depends on.
                title.push(Span::styled(" ", bold));
                let t = &self.note.title;
                let mut last = 0usize;
                for (i, (range, _)) in title_links.iter().enumerate() {
                    if range.start > last {
                        title.push(Span::styled(t[last..range.start].to_string(), bold));
                    }
                    // Each region highlights its own ordinal; the title's are
                    // the first ones, so the index IS the ordinal.
                    let mut st = bold.add_modifier(Modifier::UNDERLINED);
                    if self.link_cursor == Some(i) {
                        st = st.add_modifier(Modifier::REVERSED);
                    }
                    title.push(Span::styled(t[range.clone()].to_string(), st));
                    last = range.end;
                }
                if last < t.len() {
                    title.push(Span::styled(t[last..].to_string(), bold));
                }
            }
```

In `draw_preview`, take the title hits and shift both inner cursors by their count:

```rust
    fn draw_preview(
        &mut self,
        frame: &mut Frame,
        area: Rect,
        title_hits: &[markdown::LinkHit],
    ) -> Option<String> {
```

```rust
        // Ordinals run title → block → body. Each region gets the cursor
        // rebased into its own numbering, and `None` means "the cursor is in an
        // earlier region" — that region already applied its own REVERSED, so a
        // later one must not claim the ordinal too.
        let offset = title_hits.len();
        let block_cursor = self.link_cursor.and_then(|c| c.checked_sub(offset));
        let (block, block_hits) = if self.showing_tab_note() {
            prompt_block(&self.labelled_prompts(), text_w, &self.tickets, block_cursor)
        } else {
            (Vec::new(), Vec::new())
        };
        let body_cursor = block_cursor.and_then(|c| c.checked_sub(block_hits.len()));
```

Both hit-assembly sites prepend the title's hits. The empty-note branch:

```rust
                let mut all = title_hits.to_vec();
                all.extend(block_hits);
                self.link_hits = all;
```

and the real-note branch, after the existing body row shift:

```rust
                let mut all = title_hits.to_vec();
                all.extend(block_hits);
                all.extend(hits);
                self.link_hits = all;
```

- [ ] **Step 4: Audit every test whose note title holds a link**

`grep -n 'note.title = "' src/app.rs` and check each for a `[A-Z]{2,}-[0-9]+` key. The known ones are around lines 5475, 5492, 5503, 5512, 5527 and 5539; titles like `auth-refactor`, `20260728-team-solutions` or `Sprint Notes` hold no key and are unaffected.

For each affected test, state in your report which of these it was and why:
- **re-expectation** — the ordinals shifted, the intent stands (e.g. a body-link test whose note happens to be titled).
- **fold into the ordering test** — `a_live_cursor_beats_the_title` (title `titled HM-1`, body `HM-2`, asserting one `n` opens `HM-2`) is this case: under the new ordering one `n` lands on the TITLE's `HM-1`, and the test's purpose — "a cursor beats the title fallback" — ceases to exist because there is no fallback branch any more. Its coverage lives in `ordinals_walk_title_then_block_then_body`. Delete it rather than patching it until green.
- **rename only** — `bare_o_ignores_the_tab_notes_title_while_showing_global` and `bare_o_opens_the_first_of_two_links_in_the_title` keep their intent; Task 3 revisits their wording when `pending_open` changes.

Do NOT patch a test until it passes. If an assertion's meaning has to change, say so explicitly in the report.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test`
Expected: PASS, including the six new tests.
Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add src/app.rs
git commit -m "feat(notes): title links take the first ordinals in the link cursor"
```

---

### Task 3: bare `o` opens hit 0, footer advertises it, docs

**Files:**
- Modify: `src/app.rs` (`pending_open` ~1270-1295, `HINTS_PREVIEW` ~2074-2085), `README.md`, `CLAUDE.md`
- Test: inline test module in `src/app.rs`

**Interfaces:**
- Consumes: Task 2's title-first ordering.
- Produces: `pending_open` with no title branch; `HINTS_PREVIEW` carrying `o open`.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn bare_o_opens_the_first_link_on_screen() {
        // One rule, three shapes. Title first when it has a link…
        let mut a = ticket_app("body HM-3 here\n");
        a.note.title = "titled HM-1".into();
        rendered(&mut a, 60, 10);
        assert_eq!(a.pending_open().as_deref(), Some("https://example.test/browse/HM-1"));

        // …the block when the title has none…
        let mut b = ticket_app("body HM-3 here\n");
        b.note.title = "no key here".into();
        b.prompts = vec![crate::prompts::PromptGroup {
            pane: "w1:p5".into(),
            prompts: vec![prompt(1, "prompt HM-2 here")],
        }];
        b.prompt_labels = vec!["claude p5".into()];
        rendered(&mut b, 60, 20);
        assert_eq!(b.pending_open().as_deref(), Some("https://example.test/browse/HM-2"));

        // …and the body when neither has one.
        let mut c = ticket_app("body HM-3 here\n");
        c.note.title = "no key here".into();
        rendered(&mut c, 60, 10);
        assert_eq!(c.pending_open().as_deref(), Some("https://example.test/browse/HM-3"));
    }

    #[test]
    fn bare_o_with_no_links_anywhere_is_a_no_op() {
        let mut a = ticket_app("nothing to open here\n");
        a.note.title = "no key here".into();
        rendered(&mut a, 60, 10);
        assert_eq!(a.pending_open(), None);
        a.on_key(key(KeyCode::Char('o')));
        assert!(a.open_children.is_empty());
    }

    #[test]
    fn a_cursor_still_beats_hit_zero() {
        let mut a = ticket_app("body HM-3 here\n");
        a.note.title = "titled HM-1".into();
        rendered(&mut a, 60, 10);
        a.on_key(key(KeyCode::Char('N'))); // cold N lands on the LAST hit
        assert_eq!(a.pending_open().as_deref(), Some("https://example.test/browse/HM-3"));
    }

    #[test]
    fn the_global_notes_title_is_still_unreachable_by_bare_o() {
        // Not by a gate any more: in Global mode the header renders no title,
        // so the title contributes no hits at all.
        let mut a = ticket_app("");
        a.note.title = "titled HM-1".into();
        a.active = ActiveNote::Global;
        rendered(&mut a, 60, 10);
        assert_eq!(a.pending_open(), None);
    }

    #[test]
    fn the_base_footer_advertises_o_open() {
        assert_eq!(
            fit_hints(HINTS_PREVIEW, 79),
            " e edit  j/k spc tick  n/N link  o open  r title  l list  Up/Dn scroll  q quit"
        );
        // A 46-column dock keeps the two link keys and loses the checkbox hint.
        assert_eq!(fit_hints(HINTS_PREVIEW, 46), " e edit  n/N link  o open  q quit");
        assert_eq!(fit_hints(HINTS_PREVIEW, 37), " e edit  n/N link  o open  q quit");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test bare_o_opens_the_first_link_on_screen the_base_footer_advertises_o_open 2>&1 | head -30`
Expected: FAIL — `pending_open` returns the title's link only via its own scan (so the block/body shapes return `None`), and `o open` is absent from `HINTS_PREVIEW`.

- [ ] **Step 3: Write the implementation**

Replace `pending_open` entirely — the title branch, its second `find_links` call and its `showing_tab_note()` gate all go:

```rust
    /// The URL `o` would open right now, or `None`. With a cursor live it is the
    /// cursored hit; with NO cursor it is hit 0 — which, on a note named after
    /// its ticket, IS the title's link, so the one-keystroke path costs no
    /// special case. `link_hits` is a draw product and the loop always draws
    /// before reading a key, so "first" means the first link on screen.
    ///
    /// The global note's title cannot leak here: in Global mode the header
    /// renders `— ★ Global` and no title, so `draw` contributes no title hits.
    /// Separate from `open_ticket` so the resolution is testable without a
    /// browser.
    fn pending_open(&self) -> Option<String> {
        let hit = match self.link_cursor {
            Some(c) => self.link_hits.get(c)?,
            None => self.link_hits.first()?,
        };
        match hit.kind {
            markdown::LinkKind::Ticket => crate::tickets::ticket_url(&self.tickets, &hit.text),
            markdown::LinkKind::Url => Some(hit.text.clone()),
        }
    }
```

Replace `HINTS_PREVIEW` with the table below — `o open` sits after `n/N link` in display order and one rank behind it, so the two link keys are the last things to go:

```rust
const HINTS_PREVIEW: Hints = &[
    ("e edit", 4),
    ("j/k spc tick", 5),
    ("n/N link", 2),
    ("o open", 3),
    ("r title", 7),
    ("l list", 6),
    ("Up/Dn scroll", 8),
    ("x clear", 9),
    ("q quit", 0),
];
```

Extend the comment above the tables: bare `o` now works whenever any link is on screen, so `o open` belongs in the base state; it also shows on a note with no links, where `o` is a silent no-op, which is the accepted trade against a footer that changes with link PRESENCE as well as cursor state.

`HINTS_BOX` and `HINTS_LINK` are unchanged.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test`
Expected: PASS. `bare_o_ignores_the_tab_notes_title_while_showing_global` and `bare_o_opens_the_first_of_two_links_in_the_title` (Task 2 left them alone) still pass — the first because Global contributes no title hits, the second because the title's links are ordinals 0 and 1. If either now reads oddly against the new rule, rename it rather than change what it asserts, and say so in your report.
Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Document it**

`README.md` — replace the two sentences about `o` in the Links section with:

```markdown
`n`/`N` walk every link on screen, in the order you see them: the title first,
then the captured-prompt block, then the note body. `o` opens the selected one;
with no selection it opens the FIRST link on screen, which on a ticket-named
note is the title's. `esc` drops the cursor.
```

`CLAUDE.md` — update in place:
- the `src/app.rs` Layout entry: ordinals run title → block → body; the title is scanned in `draw` BEFORE `draw_preview` runs (the body renders first because it returns the scroll hint the title shows) and its count is the offset the block and body cursors sit behind; `LinkHit.row` is `Option<usize>` and a rowless header hit never moves `preview_scroll`; `pending_open` is "cursored hit, else `link_hits.first()`".
- the footer sentence: `HINTS_PREVIEW` now carries `o open`, and at 46 columns the checkbox hint is what makes room for it.

and add these Gotchas:

```markdown
- Title links are the FIRST ordinals, but the title is rendered LAST — `draw`
  renders the body first because `draw_preview` returns the scroll hint the
  title line displays. So the title is SCANNED early (a pure `find_links`, no
  rendering) and its count is handed to `draw_preview` as the offset the block
  and body cursors sit behind. Moving that scan after the body render silently
  renumbers every ordinal.
- Title hits exist only when a title is actually on screen: not in Global mode,
  not while `title_input` is open, not for a blank title. That is the whole
  mechanism stopping bare `o` from opening text the user was never shown —
  there is no gate in `pending_open` any more, so a future header state that
  hides the title has to be added to that condition too.
- `LinkHit.row` is `Option<usize>` because the header is a 1-row no-wrap
  `Paragraph` outside the scrollable body. `follow_link` skips a `None` row; a
  sentinel row number instead would scroll the body to an arbitrary line the
  moment a title link is selected.
- The title's spans slice the RAW `note.title` and emit the leading space as its
  own span, so `find_links`' offsets need no shift. The assembled width is
  unchanged, which the header's ALL-OR-NOTHING age token depends on — it sums
  `dwidth` over every span already pushed.
```

- [ ] **Step 6: Commit**

```bash
git add src/app.rs README.md CLAUDE.md
git commit -m "feat(notes): bare o opens the first link on screen"
```

---

## Notes for the implementer

- The one behaviour change a user will notice beyond the title: at 46 columns the base footer now shows `e edit  n/N link  o open  q quit` — `j/k spc tick` is what pays for `o open`. That is deliberate and asserted; if you think the ranks should differ, say so in your report rather than editing the expectations.
- Do not add a cursor that can scroll the header. It is one row and never scrolls; that is why a rowless hit exists.
- Every silent-failure path is deliberate. Do not add an error message, a status line, or a `dbg!` — the TUI printing anything corrupts its own screen, and the same binary has a `--capture-prompt` hook mode where stdout is injected into the user's prompt.
