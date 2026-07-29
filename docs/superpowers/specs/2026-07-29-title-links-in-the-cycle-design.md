# Title links join the cursor, and bare `o` gets one rule

Builds on `2026-07-28-link-followups-design.md`. That wave underlined the
header title's links but left them cursorless, and gave bare `o` (no cursor
live) a special case: open the TITLE's first link. Two consequences the user
hit immediately — `o` means two different things depending on cursor state, and
a SECOND link in the title is unreachable by any key.

## The rule this replaces both with

Title links become the FIRST ordinals in the one hit list, and bare `o` opens
hit 0 — whatever region hit 0 happens to be in.

- `n`/`N` walk title → prompt block → body.
- `o` with a cursor opens the cursored hit, exactly as now.
- `o` with no cursor opens `link_hits.first()`. On a ticket-named note that IS
  the title's link, so the one-keystroke path the user relies on survives
  without a special case behind it.
- `o` with no links at all stays a silent no-op.

`pending_open` therefore loses its title branch, its second `find_links` call
over the title text, and its `showing_tab_note()` gate.

## Why the title needed a shortcut in the first place

The header is a 1-row, no-wrap `Paragraph` rendered outside the scrollable
body, so a title hit has no row for the cursor's scroll-follow to target. That
is the whole reason the previous wave took the shortcut instead of putting the
title in the list.

Fix: `LinkHit.row` becomes `Option<usize>` — `None` for a title hit. Selecting
a title link therefore never moves `preview_scroll`, because `follow_link`
skips a rowless hit.

## Ordering: the title is scanned before the body is rendered

`draw` renders the BODY first (the preview returns the scroll hint the title
line displays), but title hits must be the first ordinals. So the title's links
are scanned early — a bare `markdown::find_links(&self.note.title,
&self.tickets)`, no rendering involved — and their count becomes the offset
`draw_preview` applies:

- `body_cursor = cursor - title_hits.len() - block_hits.len()`, via
  `checked_sub`, which yields `None` whenever the cursor sits in an earlier
  region.
- `self.link_hits = title_hits ++ block_hits ++ body_hits`; body rows still
  shift by `block.len()`.
- The title's spans are styled later in `draw`, applying `REVERSED` when
  `link_cursor == Some(i)` for the i-th title hit — the same "each region
  highlights its own" rule the prompt block already follows.

Title hits exist only when a title is actually rendered, so never in Global
mode (the header shows `★ Global` and no title there). That keeps the previous
wave's fix — bare `o` must not reach the global note's title text — true by
construction rather than by a gate in `pending_open`.

## Footer

`o open` joins the base hint set, ranked just after `n/N link` so it survives
into a narrow dock but drops before `q quit`. Bare `o` is now meaningful
whenever any link exists, so the hint is no longer advertising a key that does
nothing while no cursor is live.

It will also show on a note with no links at all, where `o` is a silent no-op.
That is the accepted trade: the alternative is a footer that changes with link
PRESENCE as well as cursor state, which is more surprising than a hint for a
key that quietly does nothing.

## Error handling

Unchanged: every failure path is a silent no-op — no printing, no new UI, no
panic. No cursor and no links, an unmapped prefix, a spawn refused.

## Testing

- `n` from cold lands on the title's link: the header row is reversed and
  nothing in the block or body is.
- A title with TWO links: `n`, `n` reaches the second; both resolve correctly
  through `pending_open`.
- Links in all three regions: ordinals walk title → block → body, asserted
  through `pending_open` at every ordinal, with the reversed cells matching the
  expected target and the other regions asserted clean. This is the third
  change to the ordinal offset and every previous bug hid in the two-list
  agreement, so this test is the load-bearing one.
- Bare `o` resolves hit 0 in each shape: a title link present; no title link
  but a block link present; body only; and no links at all → no-op.
- Global mode: no title hits exist, and bare `o` cannot reach the global note's
  title text.
- Selecting a title hit leaves `preview_scroll` untouched — the regression net
  for `row: Option<usize>`.
- Footer: `o open` present in the base state; exact strings re-asserted at 79 /
  46 / 37 columns for all three states.
- `cargo build --release`, `cargo test`, `cargo clippy --all-targets -- -D warnings`.

## Migration hazard (do not silently patch)

Existing tests set titles that contain keys, and their ordinals shift under the
new ordering. `a_live_cursor_beats_the_title` is the clear case: title
`titled HM-1`, body `HM-2`, asserting that after one `n` the cursored open is
`HM-2`. Under the new rule one `n` lands on the TITLE's `HM-1`, so that
assertion legitimately changes — and the test's purpose ("a cursor beats the
title fallback") ceases to exist, because the fallback is gone. It should be
folded into the ordering test, not patched until green.

Every other test whose note carries a key in its title shifts the same way. The
implementation must audit them explicitly and state, per test, whether the
change is a rename, a re-expectation, or a deletion with its coverage moved.

## Out of scope

A cursor that can scroll the header (it is one row and never scrolls), mouse
clicking and OSC 8 hyperlinks (structurally unreachable from a ratatui pane),
linkifying the `l` dashboard's read-only preview, and any change to the block's
or body's own link detection.
