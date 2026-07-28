# Ticket links in the notes preview

## Problem

Notes carry issue keys (`HM-54561`, `CR-3171`). Reading one out of the pane and
pasting it into a browser is manual. The keys should read as links and be
openable from the pane.

## Constraint that shapes the whole design

The pane draws through a ratatui cell grid: there is no way to emit OSC 8
hyperlink escapes inside a widget, the app enables no mouse capture, and herdr
owns pane mouse events. A true mouse-clickable link is therefore not reachable
from this plugin. The feature is: detect keys, style them, navigate them with
the keyboard, open the cursored one in the default browser.

## Scope

- Rendered note body in preview mode only. Not the header title, not the
  captured-prompt block, not bare `http(s)` URLs.
- Only prefixes present in the local config map are detected at all.

## Configuration

`tickets.json` in `state::store_dir()` — the same directory as note files, so it
inherits the same three-tier store resolution:

```json
{
  "HM": "https://hicxsolutions.atlassian.net/browse/{key}",
  "CR": "https://example-tracker.invalid/issue/{key}"
}
```

- Loaded ONCE at construction. Editing it needs a pane restart; documented in
  the README rather than reloaded on the heartbeat (5s file I/O for a value that
  changes ~never).
- Missing file, malformed JSON, or a non-string value → empty or partial map,
  silently, no panic.
- A template with no `{key}` placeholder drops that prefix rather than opening a
  keyless URL.
- Empty map ⇒ the feature is fully dormant: no styling, no cursor, `n`/`N`/`o`
  are no-ops.
- Prefix matching is case-sensitive uppercase.

## Components

### `markdown::find_tickets(&str, &Config) -> Vec<Range<usize>>`

The crate's ONE ticket matcher, hand-rolled like the rest of the renderer (no
`regex` dependency). Walks to a `-`, takes the uppercase run before it and the
digit run after it, requires a non-alphanumeric boundary on both sides, and
requires the prefix to be in the config map.

### Render-authoritative detection

`render_markdown_mapped` gains the ticket config and the cursor ordinal, and
returns a third value: `Vec<TicketHit { key: String, row: usize }>` in document
order. `render_markdown` and the current `_mapped` signature stay as wrappers
passing an empty config and `None`.

The matcher runs as a post-pass over each line's assembled
`Vec<(String, Style)>` BEFORE `wrap_into`, splitting spans at match boundaries.
`wrap_into` carries per-char styles through wrapping, so a key split across two
rows keeps its style on both rows for free. `row` is the first rendered row
containing the match.

Fenced code lines never reach that post-pass (they bypass `parse_inline`
already), so they are excluded exactly as `checkbox_lines` excludes them.

Detection and highlight come from the same single scan, which is why nav reads a
render byproduct instead of scanning the source separately: a second scan over
the raw source would see `HM-**54561**` where the render sees `HM-54561`, the
counts would diverge, and the cursor ordinal would slip. Same failure the crate
already forbids for the checkbox parser.

### `tickets.rs`

`Config` (prefix → template), its loader, the pure
`ticket_url(&Config, key) -> Option<String>`, and `open(url)`.

### App state

| Field | Meaning |
| --- | --- |
| `tickets: tickets::Config` | loaded once at construction |
| `ticket_hits: Vec<TicketHit>` | rewritten by every preview draw |
| `ticket_cursor: Option<usize>` | ordinal into `ticket_hits` |
| `follow_ticket: bool` | one-shot scroll-follow, same contract as `follow_box` |

Cache freshness: the event loop draws before polling input and redraws after
every key, so `ticket_hits` is never consulted stale. Edit mode draws no
preview; the first draw after leaving it refreshes the cache before any key is
read.

## Keys

- `n` next ticket, `N` previous, clamping at both ends. `n` with no cursor lands
  on hit 0. No hits ⇒ no cursor.
- `o` opens the cursored ticket. No cursor ⇒ no-op.
- `Esc` clears BOTH cursors, staying the one unambiguous drop key.
- `n`/`N` clear the box cursor; `j`/`k`/`space` clear the ticket cursor. One
  cursor is live at a time.
- `clamp_ticket_cursor` runs after each draw refresh, since editing can delete a
  ticket out from under the ordinal.

Every existing `clear_box_cursor()` call site — `toggle_global`, the `x`
confirm, the overlay self-delete, the mode-change path — becomes
`clear_cursors()`, so a document swap cannot miss the new per-document field.
That is the recurring bug class in this crate's gotchas: a field added later has
to be walked through every pre-existing path.

## Styling

Ticket keys get `Modifier::UNDERLINED` on top of whatever style they already
carry — no new color, so it survives light and dark themes and does not collide
with the cyan headings. The cursored hit adds `REVERSED`.

## Footer

A third hint pair for ticket-cursor-live (`n/N ticket`, `o open`, `esc drop`),
full and short forms, chosen by pane width like the existing pairs. The base
FULL hint gains `n ticket` for discoverability; the base short hint is unchanged
because its width floor is already tight at 37 columns.

## Opening

`tickets::open`:

- Windows: `rundll32.exe url.dll,FileProtocolHandler <url>` with
  `CREATE_NO_WINDOW`. Not `cmd /c start`: that flashes a console over the TUI
  and its argument quoting mangles URLs containing `&`.
- macOS: `open <url>`. Other unix: `xdg-open <url>`.
- `spawn()`, never `output()`. A blocking wait on the event-loop thread freezes
  input, drawing and the 5s identity re-stamp; past 20s of no re-stamp the
  launcher calls the pane a corpse, the next toggle REPLACEs it, and
  `pane close` kills with no signal — taking the dirty debounce buffer.
- stdio all null. Spawned `Child` handles are kept in a small `Vec` and
  `try_wait`-ed on each heartbeat so unix does not accumulate a zombie per open.

## Error handling

Every failure path is a silent no-op with no new UI: no config file, prefix
unmapped, template unusable, spawn refused. Because only mapped prefixes are
detected, `n` can never land on a key that `o` cannot open, so the common
"nothing happened" case does not arise.

## Security

The URL is built from a local config template the user owns plus a key matching
`[A-Z]{2,}-[0-9]+`, then passed as argv to a known executable. No shell is
involved, so there is no injection path. `tickets.json` is trusted input in the
same way the note files are.

## Testing

- `find_tickets`: rejects `hm-1`, `HM-`, `xHM-1`, `HM-1x`; accepts inside
  `**bold**` and inline code; skips fenced-code lines; skips unmapped prefixes;
  multiple hits per line in left-to-right order.
- Config: missing file, malformed JSON, non-string value, template without
  `{key}`.
- Render: hit gets an underlined span; the cursored ordinal also `REVERSED`; a
  wrap-split key keeps the style on both rows; returned `row` indices correct.
- App: `n`/`N` clamp; mutual exclusion in both directions; `Esc` clears both
  cursors; `toggle_global` clears both; `o` no-ops with no cursor and with no
  config.
- `ticket_url` is the pure seam carrying the URL-building tests. `open`'s spawn
  stays thin and untested.
- `cargo build --release`, `cargo test`, `cargo clippy --all-targets -- -D warnings`.
- End-to-end in a throwaway pane with a `TT` prefix mapped to
  `https://example.com/{key}`, driven by `pane send-keys n` then `o`, leaving
  the real config file untouched.

## Out of scope

Mouse clicking, OSC 8 escapes, linkifying the header title or the prompt block,
bare URL detection, config hot-reload, per-note overrides.
