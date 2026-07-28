# Ticket-link follow-ups: footer, prompt block, title, URLs, hot-reload

Builds on `2026-07-28-ticket-links-design.md`, which shipped underlined issue
keys in the note body with an `n`/`N` cursor and `o` to open. Five follow-ups,
all driven by using it: the `n` hint is invisible in a narrow dock, keys in the
title and the prompt block look identical but do nothing, editing the config
needs a pane restart, and pasted URLs are not reachable at all.

## Two facts that shape the design

- `prompt_block` truncates each line with `truncate_w` and appends NO ellipsis.
  A cut `HM-54283` therefore ends up as a valid-looking `HM-542`, which would
  open the wrong ticket. The matcher must run on the untruncated text and keep
  only hits that lie fully inside the retained prefix.
- The header title is a 1-row, no-wrap `Paragraph` outside the scrollable body.
  It can be styled, but it can never host a cursor.

## 1. Footer hints degrade in tokens, not in fixed forms

The six fixed consts already cover three states × two widths; a third width per
state would make nine. Replace them with per-state token slices carrying a drop
rank, plus one fitter.

```rust
/// (token, drop rank) — display order is slice order; the highest rank drops
/// first when the line does not fit. Rank 0 never drops.
const HINTS_PREVIEW: &[(&str, u8)] = &[
    ("e edit", 3), ("j/k spc tick", 4), ("n/N link", 2), ("r title", 6),
    ("l list", 5), ("Up/Dn scroll", 7), ("x clear", 8), ("q quit", 0),
];
```

Two further slices cover the live-cursor states. Both give `esc drop` rank 1 —
it is the only exit from a cursor — and the link-cursor slice gives `o open`
rank 1 as well, since opening is the point of having that cursor.

`fit_hints(tokens, width) -> String` joins the survivors with two spaces behind
a single leading space, dropping the highest-rank token repeatedly until the
line fits in `width` display columns. When only rank 0 remains and even that
overflows, it returns `q quit` and lets the terminal clip, exactly as today.

Effects: a 79-column pane renders the same line as today's full form; a
46-column dock renders `e edit  n/N link  l list  q quit` instead of losing the
link hint entirely; the 37-column floor still keeps `q quit`.

Measured in display columns (`dwidth`), not chars, because a title can hold CJK
and the same rule already governs the header's age token.

## 2. Prompt-block keys become links

`prompt_block` gains the config and returns `(Vec<Line>, Vec<LinkHit>)`. Rows
there are 1:1 with lines — it truncates and never wraps — so a hit's row is its
line index.

Truncation guard: run `find_links` on the ORIGINAL prompt text, then keep only
hits whose byte range ends at or before the retained prefix's length. The
retained text is a byte prefix of the original, so the offsets align and a
partially-cut key is dropped rather than opened.

## 3. Ordering: block hits precede body hits

The block sits above the note, so the cursor must walk it first.

- The combined list is `block_hits` then `body_hits`.
- Body hit rows already shift by `block.len()`; that stays.
- Body hit ORDINALS now shift by `block_hits.len()`.
- `markdown::render_markdown_links` (renamed from `render_markdown_tickets`,
  see §5) highlights the nth BODY hit, so the draw
  passes `cursor - block_hits.len()` when the cursor is in the body and `None`
  when it is in the block — in which case the draw applies `REVERSED` to that
  block row's hit span itself.

This is the one place where two hit lists must agree on an order, the same class
as the cross-line ordinal bug the last review caught. It is pinned by a test
with hits in BOTH regions asserting that the highlighted span and
`pending_open` name the same target.

## 4. Title keys are underlined, and bare `o` opens the title's key

The header's title span is split on `find_links` and its keys underlined, purely
so the title reads consistently with the body.

No cursor can live there, so the affordance is `o` with NO cursor live: it opens
the TITLE's first key. That is deterministic and matches what the underline
advertises, and it makes the common case — the note is named after the ticket —
a single keystroke.

- `o` with a cursor live opens the cursored hit, never the title.
- `o` with no cursor and no key in the title stays a silent no-op.

## 5. Bare `http(s)://` URLs join the same cursor

The crate's single scan becomes one function over both target kinds:

```rust
pub enum LinkKind { Ticket, Url }
pub struct LinkHit { pub text: String, pub kind: LinkKind, pub row: usize }
pub fn find_links(s: &str, cfg: &Config) -> Vec<(Range<usize>, LinkKind)>
```

`find_links` merges both matchers left to right, non-overlapping. A URL starting
before a ticket key consumes the range, so a key inside a URL path is not
double-matched.

URL matching: case-insensitive `http://` or `https://`, running to whitespace,
requiring at least one non-whitespace character after `//`. Trailing `.,;:!?'"`
are trimmed, and a trailing `)`, `]` or `}` is trimmed only when unbalanced
within the match — so `(see https://x/y)` and `https://x/y.` both open the right
thing. Only those two schemes ever match: no `file://`, no `javascript:`.

Styling is the same underline tickets get.

### The rename that comes with it

The state stops being ticket-specific: `ticket_hits`/`ticket_cursor`/
`follow_ticket` → `link_hits`/`link_cursor`/`follow_link`,
`clear_ticket_cursor` → `clear_link_cursor`, `move_ticket` → `move_link`,
`markdown::TicketHit` → `markdown::LinkHit`, `render_markdown_tickets` → `render_markdown_links`, `find_tickets` → `find_links`
(with the ticket half as an internal). `pending_open` and `open_ticket` keep
their names; `pending_open` resolves a `Ticket` hit through
`tickets::ticket_url` and a `Url` hit to its own text.

Mechanical, one commit, with `CLAUDE.md` and `README.md` updated in the same
change so the docs never describe a name that no longer exists.

## 6. Config hot-reload on mtime

`App` caches `tickets.json`'s modification time. On the 5s heartbeat, one
`fs::metadata(...).modified()` stat; when it differs from the cached value,
`Config::load()` replaces `self.tickets` and the mtime is re-cached. Gated on
`persist`, so unit tests never stat.

A deleted file reloads to an empty config, which turns the feature off —
symmetric with the "missing file means dormant" rule already in place. Every
failure is silent.

This is a local stat and is deliberately NOT in the class of the `git_branch`
spawn gotcha. `CLAUDE.md` records that explicitly, because the tempting
"improvement" of doing real work on the heartbeat is exactly what that gotcha
forbids.

## Error handling

Unchanged from the base feature: every failure path is a silent no-op — no
printing, no new UI, no panic. Missing or malformed config, unmapped prefix,
truncated key, spawn refused, stat failure.

## Security

URLs now come from note text rather than only from a config template. `open`
still passes a single argv entry to a known executable with no shell, and only
`http`/`https` can match, so a note cannot cause a local file or a script
handler to be invoked. The note file remains trusted input in the same way it
already is.

## Testing

- **Footer:** exact strings at 79 / 46 / 37 columns for all three states;
  monotonicity (a wider pane never shows fewer tokens); `q quit` present at
  every width in every state; `o open` surviving to the floor while a link
  cursor is live.
- **Prompt block:** a block hit navigates and opens; a key cut by truncation
  yields NO hit; block hits precede body hits; a block-region cursor highlights
  the block row while the body render receives `None`.
- **Ordering:** one test with hits in both regions asserting the highlighted
  span and `pending_open` agree.
- **Title:** underlined span present; bare `o` resolves the title's key; bare
  `o` with a live cursor opens the cursored hit instead; no key in the title is
  a no-op.
- **URLs:** trailing punctuation; balanced and unbalanced parens; bare
  `https://` rejected; uppercase scheme accepted; a ticket key inside a URL not
  double-matched; a URL split by wrap keeping its style on both rows.
- **Hot-reload:** an edited file is picked up on the next heartbeat; a deleted
  file goes dormant; an unchanged mtime does not reload.
- `cargo build --release`, `cargo test`, `cargo clippy --all-targets -- -D warnings`.
- Live check in a throwaway pane, as the base feature had: a block key and a
  pasted URL both openable, and the footer showing the link hint at ~46
  columns.

## Out of scope

A `"*"` catch-all URL template (it would linkify `ISO-8601`, `RFC-2119`,
`COVID-19` — the prefix allowlist is what keeps that quiet), a clipboard key
(`o` already gets you there), linkifying the `l` dashboard's read-only note
preview (no cursor to plumb there, so it could only be a style-only lie), mouse
clicking and OSC 8 hyperlinks (structurally unreachable from a ratatui pane).
