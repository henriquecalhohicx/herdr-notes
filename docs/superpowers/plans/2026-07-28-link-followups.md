# Link Follow-ups Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the notes pane's links complete: bare http(s) URLs join the ticket cursor, keys in the prompt block and the header title become reachable, the footer advertises the keys at real dock widths, and editing `tickets.json` no longer needs a pane restart.

**Architecture:** The crate's one link scan grows from `find_tickets` to `find_links`, returning both configured issue keys and bare URLs in document order with a kind tag. Everything ticket-named is renamed link-named in one mechanical pass. The prompt block gets its own linkifier (it truncates rather than wraps, so its rows are 1:1 with lines) and its hits are ordered ahead of the body's; the title is styled but cursorless, reachable instead by bare `o`. The footer's six fixed strings collapse into ranked token slices plus one fitter.

**Tech Stack:** Rust 2024, ratatui 0.30, crossterm 0.29, serde_json, unicode-width. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-07-28-link-followups-design.md`
**Base feature spec:** `docs/superpowers/specs/2026-07-28-ticket-links-design.md`

## Global Constraints

- No new crate dependencies. No `regex`.
- `cargo build --release`, `cargo test`, `cargo clippy --all-targets -- -D warnings` must all pass before a task is done. `cargo test --lib` does NOT work — this crate has only a `[[bin]]` target; use `cargo test` or `cargo test <name>`. `cargo build --release` fails with os error 5 while a Notes TUI is running in a pane (quit it first; `Get-Process herdr-notes | Stop-Process` for stragglers).
- Every failure path is a SILENT no-op: no printing, no new UI, no panic. Missing/malformed config, unmapped prefix, truncated key, spawn refused, stat failure.
- Only `http://` and `https://` may ever match as URLs — no `file://`, no `javascript:`. The URL is passed as a single argv entry to a known executable with no shell.
- `find_links` stays the crate's ONLY link scan. A second scan drifts (same rule as `markdown::checkbox_lines`).
- Never block the event-loop thread. The heartbeat may `stat` a local file; it may not spawn or wait.
- The overlay's read-only note preview stays link-free (`render_markdown` / `render_markdown_mapped`), because no cursor can be plumbed there.
- Widths are measured in display columns (`dwidth`), never chars.
- Unit tests never touch the real store dir. Config is injected by assigning the field, or by pointing `HERDR_PLUGIN_STATE_DIR` at a temp dir under `ENV_LOCK` as existing tests do.
- Esc must NEVER exit the TUI; only `q` quits.

---

### Task 1: URLs join the link scan

**Files:**
- Modify: `src/markdown.rs` (add beside `find_tickets`, ~line 234)
- Test: inline test module in `src/markdown.rs`

**Interfaces:**
- Consumes: `crate::tickets::Config::has_prefix` (existing).
- Produces:
  - `pub enum LinkKind { Ticket, Url }` (`Clone + Copy + Debug + PartialEq + Eq`)
  - `pub fn find_links(s: &str, cfg: &crate::tickets::Config) -> Vec<(std::ops::Range<usize>, LinkKind)>`
  - The existing `pub fn find_tickets` is renamed to the private `fn find_ticket_ranges` with its body unchanged; `find_links` is the only public entry.

- [ ] **Step 1: Write the failing tests**

Add to `markdown.rs`'s test module (the `cfg()` helper already exists there and maps `HM` and `CR`):

```rust
    fn links(s: &str) -> Vec<(String, LinkKind)> {
        find_links(s, &cfg())
            .into_iter()
            .map(|(r, k)| (s[r].to_string(), k))
            .collect()
    }

    #[test]
    fn a_bare_url_is_a_link() {
        assert_eq!(
            links("see https://example.test/a/b?q=1 now"),
            [("https://example.test/a/b?q=1".to_string(), LinkKind::Url)]
        );
    }

    #[test]
    fn the_scheme_is_matched_case_insensitively() {
        assert_eq!(links("HTTPS://EXAMPLE.TEST/x").len(), 1);
        assert_eq!(links("Http://example.test/x").len(), 1);
    }

    #[test]
    fn a_scheme_with_no_host_is_not_a_link() {
        assert!(links("https:// nothing").is_empty());
        assert!(links("https://").is_empty());
        assert!(links("ftp://example.test/x").is_empty());
        assert!(links("file:///c:/secrets.txt").is_empty());
    }

    #[test]
    fn trailing_sentence_punctuation_is_not_part_of_the_url() {
        assert_eq!(links("go to https://example.test/x.")[0].0, "https://example.test/x");
        assert_eq!(links("go to https://example.test/x, then")[0].0, "https://example.test/x");
        assert_eq!(links("\"https://example.test/x\"")[0].0, "https://example.test/x");
    }

    #[test]
    fn a_closing_bracket_goes_back_to_the_prose_only_when_unbalanced() {
        assert_eq!(links("(see https://example.test/x)")[0].0, "https://example.test/x");
        assert_eq!(links("https://example.test/a_(b)")[0].0, "https://example.test/a_(b)");
    }

    #[test]
    fn a_ticket_key_inside_a_url_is_part_of_the_url() {
        let got = links("https://jira.test/browse/HM-1 and HM-2");
        assert_eq!(
            got,
            [
                ("https://jira.test/browse/HM-1".to_string(), LinkKind::Url),
                ("HM-2".to_string(), LinkKind::Ticket),
            ]
        );
    }

    #[test]
    fn tickets_and_urls_come_back_in_document_order() {
        let got = links("HM-1 then https://example.test/x then CR-2");
        assert_eq!(
            got.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>(),
            ["HM-1", "https://example.test/x", "CR-2"]
        );
    }

    #[test]
    fn urls_are_found_with_no_configured_prefixes() {
        let empty = crate::tickets::Config::default();
        let s = "HM-1 https://example.test/x";
        let got = find_links(s, &empty);
        assert_eq!(got.len(), 1, "no prefixes configured, so only the URL");
        assert_eq!(got[0].1, LinkKind::Url);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test find_links 2>&1 | head -20`
Expected: FAIL — `cannot find function find_links` / `cannot find type LinkKind`.

- [ ] **Step 3: Write the implementation**

Rename the existing `pub fn find_tickets` to `fn find_ticket_ranges` (body and doc comment unchanged apart from the name), then add below it:

```rust
/// What an openable target IS: an issue key that resolves through the config,
/// or a URL that is already the target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LinkKind {
    Ticket,
    Url,
}

/// Byte ranges of every openable target in `s` — configured issue keys and
/// bare http(s) URLs — left to right and non-overlapping.
///
/// The crate's ONE link scan. Anything that needs to know where the links are
/// goes through here, for the same reason the checkbox parser is single-homed:
/// a second scan drifts.
pub fn find_links(
    s: &str,
    cfg: &crate::tickets::Config,
) -> Vec<(std::ops::Range<usize>, LinkKind)> {
    let mut merged: Vec<(std::ops::Range<usize>, LinkKind)> = find_url_ranges(s)
        .into_iter()
        .map(|r| (r, LinkKind::Url))
        .chain(find_ticket_ranges(s, cfg).into_iter().map(|r| (r, LinkKind::Ticket)))
        .collect();
    // URLs sort ahead of a key starting at the same offset, so the overlap
    // filter below keeps the URL: a key inside a URL path is part of that URL,
    // not a second target.
    merged.sort_by_key(|(r, kind)| (r.start, matches!(kind, LinkKind::Ticket)));
    let mut out: Vec<(std::ops::Range<usize>, LinkKind)> = Vec::new();
    for (range, kind) in merged {
        if out.last().is_some_and(|(prev, _)| range.start < prev.end) {
            continue; // swallowed by the target already taken
        }
        out.push((range, kind));
    }
    out
}

/// Byte ranges of bare `http://` / `https://` URLs. The scheme is matched
/// case-insensitively, the URL runs to the next whitespace, and at least one
/// non-whitespace character must follow `//` — a bare scheme is not a link.
fn find_url_ranges(s: &str) -> Vec<std::ops::Range<usize>> {
    const SCHEMES: [&str; 2] = ["https://", "http://"];
    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    let mut resume = 0usize;
    for (i, _) in s.char_indices() {
        if i < resume {
            continue;
        }
        let rest = &s[i..];
        let Some(scheme) = SCHEMES
            .iter()
            .find(|sc| rest.len() >= sc.len() && rest[..sc.len()].eq_ignore_ascii_case(sc))
        else {
            continue;
        };
        let body = i + scheme.len();
        let end = s[body..].find(char::is_whitespace).map_or(s.len(), |off| body + off);
        let end = trim_url_end(s, body, end);
        if end == body {
            continue; // nothing after `//`
        }
        out.push(i..end);
        resume = end;
    }
    out
}

/// Trims trailing sentence punctuation from a URL, and a trailing `)`/`]`/`}`
/// only when the URL holds no matching opener — so `(see https://x/y)` gives
/// the bracket back to the prose while `https://x/a_(b)` keeps it.
fn trim_url_end(s: &str, start: usize, mut end: usize) -> usize {
    let bytes = s.as_bytes();
    while end > start {
        let last = bytes[end - 1];
        let opener = match last {
            b'.' | b',' | b';' | b':' | b'!' | b'?' | b'\'' | b'"' => {
                end -= 1;
                continue;
            }
            b')' => b'(',
            b']' => b'[',
            b'}' => b'{',
            _ => break,
        };
        let inner = &s[start..end - 1];
        let opens = inner.bytes().filter(|b| *b == opener).count();
        let closes = inner.bytes().filter(|b| *b == last).count();
        if opens > closes {
            break; // balanced: the bracket belongs to the URL
        }
        end -= 1;
    }
    end
}
```

Also update the one existing caller inside `style_tickets` (`src/markdown.rs:373`) from `find_tickets(&text, ctx.cfg)` to `find_ticket_ranges(&text, ctx.cfg)` for now — Task 2 replaces it with `find_links`.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test`
Expected: PASS, including every pre-existing test (`find_ticket_ranges` behaviour is unchanged; the matcher tests that called `find_tickets` through the `keys` helper need that helper repointed at `find_links` — do that as part of Step 3 and keep their assertions identical).
Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/markdown.rs
git commit -m "feat(markdown): bare http(s) URLs join the single link scan"
```

---

### Task 2: rename ticket → link, and carry the kind through the render

**Files:**
- Modify: `src/markdown.rs` (`TicketHit` ~14-30, `render_markdown_mapped`/`render_markdown_tickets` ~41-84, `render_line` ~86, `emit` ~337-355, `style_tickets` ~357-397)
- Modify: `src/app.rs` (fields ~399-416, `with_note`, `toggle_global`, the `x` confirm arm, the overlay `ConfirmDelete` arm, `on_key_preview`, the cursor helpers ~1160-1235, `draw_preview` ~1566-1640)
- Test: inline test modules in both files

**Interfaces:**
- Consumes: Task 1's `find_links` and `LinkKind`.
- Produces:
  - `pub struct LinkHit { pub text: String, pub kind: LinkKind, pub row: usize }` (`Clone + Debug + PartialEq + Eq`), replacing `TicketHit { key, row }`
  - `pub fn render_markdown_links(text: &str, width: usize, cfg: &crate::tickets::Config, cursor: Option<usize>) -> (Vec<Line<'static>>, Vec<Option<usize>>, Vec<LinkHit>)`, replacing `render_markdown_tickets`
  - `render_markdown` and `render_markdown_mapped` keep their exact signatures AND stay link-free (see Step 3 — this is what keeps the overlay preview plain)
  - `App` fields `link_hits: Vec<markdown::LinkHit>`, `link_cursor: Option<usize>`, `follow_link: bool`; methods `move_link`, `clear_link_cursor`, `clamp_link_cursor`; `clear_cursors`, `pending_open`, `open_ticket` keep their names

- [ ] **Step 1: Write the failing tests**

In `src/markdown.rs`'s test module, REPLACE the existing `an_empty_config_changes_nothing` test with the pair below (its old intent — "an empty config renders exactly as the plain path" — now belongs to the link-free `render_markdown_mapped`, while `render_markdown_links` with an empty config still finds URLs):

```rust
    #[test]
    fn the_mapped_entry_point_stays_link_free() {
        // The overlay's read-only preview renders through here; no cursor can
        // be plumbed there, so it must never underline anything.
        let (lines, map) = render_markdown_mapped("HM-1 https://example.test/x", 40);
        assert_eq!(map.len(), lines.len());
        assert!(
            !lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .any(|s| s.style.add_modifier.contains(Modifier::UNDERLINED))
        );
    }

    #[test]
    fn an_empty_config_still_finds_urls() {
        let empty = crate::tickets::Config::default();
        let (lines, _, hits) =
            render_markdown_links("HM-1 https://example.test/x", 40, &empty, None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, LinkKind::Url);
        assert_eq!(hits[0].text, "https://example.test/x");
        assert!(hit_style(&lines, "https://example.test/x").add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn a_url_split_by_wrap_keeps_its_style_on_every_row() {
        let (lines, _, hits) =
            render_markdown_links("aaaa bbbb https://example.test/xyz", 14, &cfg(), None);
        assert_eq!(hits.len(), 1);
        let underlined: usize = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter(|s| s.style.add_modifier.contains(Modifier::UNDERLINED))
            .map(|s| s.content.chars().count())
            .sum();
        assert_eq!(underlined, "https://example.test/xyz".chars().count());
    }
```

Every other `markdown` test that names `TicketHit`, `render_markdown_tickets` or `hits[i].key` is mechanically repointed at `LinkHit`, `render_markdown_links` and `.text`, with a `kind: LinkKind::Ticket` field added to the `LinkHit` literals. Keep their assertions identical.

In `src/app.rs`'s test module, rename every `ticket_cursor` / `ticket_hits` / `follow_ticket` reference to `link_cursor` / `link_hits` / `follow_link`, and add:

```rust
    #[test]
    fn a_url_in_the_note_is_navigable_and_opens_itself() {
        let mut a = ticket_app("read https://example.test/doc later\n");
        rendered(&mut a, 40, 10);
        assert_eq!(a.link_hits.len(), 1);
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.pending_open().as_deref(), Some("https://example.test/doc"));
    }

    #[test]
    fn one_cursor_walks_tickets_and_urls_in_document_order() {
        let mut a = ticket_app("HM-1 then https://example.test/x\n");
        rendered(&mut a, 60, 10);
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.pending_open().as_deref(), Some("https://example.test/browse/HM-1"));
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.pending_open().as_deref(), Some("https://example.test/x"));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test 2>&1 | head -30`
Expected: FAIL — `cannot find type LinkHit` / `no method named pending_open` resolving a URL / `no field link_hits`.

- [ ] **Step 3: Write the implementation**

In `src/markdown.rs`:

```rust
/// One openable target found while rendering: its text, what kind of target it
/// is, and the rendered row its first character landed on. Document order ==
/// the order `n`/`N` walk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinkHit {
    pub text: String,
    pub kind: LinkKind,
    pub row: usize,
}

/// Per-render link state: what to match, which hit is cursored, and the hits
/// found so far. `hits.len()` doubles as the ordinal of the next hit, which is
/// why detection and highlight can never disagree — one pass assigns both.
/// `enabled` is false for the link-free entry points.
struct LinkCtx<'a> {
    cfg: &'a crate::tickets::Config,
    cursor: Option<usize>,
    hits: Vec<LinkHit>,
    enabled: bool,
}
```

`render_markdown_mapped` keeps its signature and builds a DISABLED context (this is what keeps the overlay preview plain now that URLs need no config):

```rust
pub fn render_markdown_mapped(text: &str, width: usize) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
    let cfg = crate::tickets::Config::default();
    let (lines, map, _) = render_inner(text, width, &cfg, None, false);
    (lines, map)
}

/// `render_markdown_mapped` plus links: configured issue keys and bare http(s)
/// URLs are underlined (the `cursor`-th one also REVERSED) and returned as hits.
pub fn render_markdown_links(
    text: &str,
    width: usize,
    cfg: &crate::tickets::Config,
    cursor: Option<usize>,
) -> (Vec<Line<'static>>, Vec<Option<usize>>, Vec<LinkHit>) {
    render_inner(text, width, cfg, cursor, true)
}
```

`render_inner` is the old `render_markdown_tickets` body with `LinkCtx { cfg, cursor, hits: Vec::new(), enabled }` and an added `enabled: bool` parameter. `render_line`, `emit` and `style_tickets` take `&mut LinkCtx<'_>` / `&LinkCtx<'_>`; rename `style_tickets` to `style_links`.

`emit`'s fast path changes — an empty config no longer means "nothing to find", because URLs need no config:

```rust
    if !ctx.enabled {
        wrap_into(out, spans, width, hang);
        return;
    }
```

`style_links` scans with `find_links` and carries the kind:

```rust
        for (range, kind) in find_links(&text, ctx.cfg) {
            ...
            marks.push((chars, text[range.clone()].to_string(), kind));
            ...
        }
```

with `emit` pushing `LinkHit { text, kind, row: base + row }`. Keep the `#[allow(clippy::type_complexity)]` or introduce `type Marks = Vec<(usize, String, LinkKind)>` — the alias is preferred now that the tuple grew.

In `src/app.rs`: rename the three fields, the three helpers and every call site; repoint `draw_preview` at `render_markdown_links`; and resolve by kind:

```rust
    /// The URL `o` would open right now, or `None`. Separate from `open_ticket`
    /// so the resolution is testable without launching a browser.
    fn pending_open(&self) -> Option<String> {
        let hit = self.link_cursor.and_then(|c| self.link_hits.get(c))?;
        match hit.kind {
            markdown::LinkKind::Ticket => crate::tickets::ticket_url(&self.tickets, &hit.text),
            markdown::LinkKind::Url => Some(hit.text.clone()),
        }
    }
```

The footer's `n/N ticket` tokens stay as they are in this task (Task 3 rewrites that whole block).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test`
Expected: PASS. Every pre-existing assertion is unchanged apart from renames.
Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/markdown.rs src/app.rs
git commit -m "refactor(notes): links, not just tickets — one cursor over keys and URLs"
```

---

### Task 3: footer hints degrade in tokens

**Files:**
- Modify: `src/app.rs` (the hint const block and its selection, ~1490-1520; add `fit_hints` beside `fit_right` ~1956)
- Test: inline test module in `src/app.rs`

**Interfaces:**
- Consumes: `dwidth` (existing), `App.link_cursor` / `App.box_cursor` (Task 2).
- Produces: `type Hints = &'static [(&'static str, u8)]`, `const HINTS_PREVIEW`/`HINTS_BOX`/`HINTS_LINK`, `fn fit_hints(tokens: Hints, width: usize) -> String`.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn the_full_footer_shows_every_token() {
        assert_eq!(
            fit_hints(HINTS_PREVIEW, 79),
            " e edit  j/k spc tick  n/N link  r title  l list  Up/Dn scroll  x clear  q quit"
        );
    }

    #[test]
    fn a_narrow_dock_drops_tokens_by_rank() {
        // 46 columns is a typical right dock. Ranks drop x clear, Up/Dn scroll,
        // r title and l list in that order; what is left fits in 39.
        assert_eq!(fit_hints(HINTS_PREVIEW, 46), " e edit  j/k spc tick  n/N link  q quit");
        // At the 37-column floor the checkbox hint goes too. Greedy by rank,
        // not optimal packing: something shorter could still have fitted, and
        // that is the accepted trade for one simple rule.
        assert_eq!(fit_hints(HINTS_PREVIEW, 37), " e edit  n/N link  q quit");
    }

    #[test]
    fn the_link_state_keeps_o_open_and_esc_drop_to_the_floor() {
        for w in [37, 46, 60] {
            let line = fit_hints(HINTS_LINK, w);
            assert!(line.contains("o open"), "{w}: {line}");
            assert!(line.contains("esc drop"), "{w}: {line}");
            assert!(dwidth(&line) <= w, "{w}: {line}");
        }
    }

    #[test]
    fn every_state_keeps_q_quit_at_every_width() {
        for tokens in [HINTS_PREVIEW, HINTS_BOX, HINTS_LINK] {
            for w in 10..=90 {
                assert!(fit_hints(tokens, w).contains("q quit"), "width {w}");
            }
        }
    }

    #[test]
    fn a_wider_pane_never_shows_fewer_tokens() {
        for tokens in [HINTS_PREVIEW, HINTS_BOX, HINTS_LINK] {
            let mut prev = 0;
            for w in 10..=100 {
                let n = fit_hints(tokens, w).split("  ").count();
                assert!(n >= prev, "width {w} regressed from {prev} to {n}");
                prev = n;
            }
        }
    }

    #[test]
    fn the_footer_advertises_the_link_key_in_a_narrow_dock() {
        // The whole point of this task: at 46 columns the old short form had no
        // room for it, so the feature was invisible.
        let mut a = ticket_app("HM-1\n");
        let screen = rendered(&mut a, 46, 10);
        assert!(screen.contains("n/N link"), "{screen}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test fit_hints 2>&1 | head -20`
Expected: FAIL — `cannot find function fit_hints` / `cannot find value HINTS_PREVIEW`.

- [ ] **Step 3: Write the implementation**

Replace the six `PREVIEW_HINTS*` consts and the `match` that picked between them with:

```rust
        let hints = match self.note.mode {
            Mode::Preview => {
                let tokens = if self.link_cursor.is_some() {
                    HINTS_LINK
                } else if self.box_cursor.is_some() {
                    HINTS_BOX
                } else {
                    HINTS_PREVIEW
                };
                fit_hints(tokens, usize::from(hint_a.width))
            }
            Mode::Edit => " Esc preview (saves)   Ctrl+S save".to_string(),
        };
```

and add, near `fit_right`:

```rust
/// Footer hint tokens for one preview state, in DISPLAY order, each with a drop
/// rank: when the line does not fit, the highest rank goes first and ties break
/// on the later slice position. Rank 0 never drops, so `q quit` survives to the
/// floor and only below that does the terminal clip — which is what the six
/// fixed hint strings used to guarantee, at the cost of a step change between
/// two widths and nothing in between.
type Hints = &'static [(&'static str, u8)];

const HINTS_PREVIEW: Hints = &[
    ("e edit", 3),
    ("j/k spc tick", 4),
    ("n/N link", 2),
    ("r title", 6),
    ("l list", 5),
    ("Up/Dn scroll", 7),
    ("x clear", 8),
    ("q quit", 0),
];

/// While a checkbox cursor is live, `esc drop` is the only way out of it, so it
/// outranks everything but `q quit`.
const HINTS_BOX: Hints = &[
    ("e edit", 3),
    ("j/k spc tick", 4),
    ("esc drop", 1),
    ("r title", 6),
    ("l list", 5),
    ("Up/Dn scroll", 7),
    ("x clear", 8),
    ("q quit", 0),
];

/// While a link cursor is live, opening is the point (`o open`) and `esc drop`
/// is the way out; `x clear` is not offered at all — wiping the note under a
/// live link cursor is not a thing anyone reaches for.
const HINTS_LINK: Hints = &[
    ("e edit", 4),
    ("n/N link", 3),
    ("o open", 1),
    ("esc drop", 2),
    ("r title", 6),
    ("l list", 5),
    ("Up/Dn scroll", 7),
    ("q quit", 0),
];

/// Renders `tokens` into a footer line of at most `width` display COLUMNS,
/// dropping by rank until it fits. Greedy by rank rather than optimal packing:
/// a lower-ranked token that would still have fitted is not re-added, which
/// keeps the rule one sentence long and the output predictable.
fn fit_hints(tokens: Hints, width: usize) -> String {
    let mut keep: Vec<(&str, u8)> = tokens.to_vec();
    loop {
        let line = render_hints(&keep);
        if dwidth(&line) <= width {
            return line;
        }
        let Some(pos) = keep
            .iter()
            .enumerate()
            .filter(|(_, (_, rank))| *rank > 0)
            .max_by_key(|(i, (_, rank))| (*rank, *i))
            .map(|(i, _)| i)
        else {
            return line; // only rank 0 left: let the terminal clip, as before
        };
        keep.remove(pos);
    }
}

/// One leading space, two spaces between tokens — the shape the fixed hint
/// strings had.
fn render_hints(keep: &[(&str, u8)]) -> String {
    let joined: Vec<&str> = keep.iter().map(|(t, _)| *t).collect();
    format!(" {}", joined.join("  "))
}
```

Keep the existing explanatory comment above the block, updated: it documents why `esc drop`/`o open` are state-scoped and why `q quit` is the one token that never leaves.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test`
Expected: PASS. Any pre-existing test asserting an exact old footer string is expected to need its expectation updated to the new output — verify by reading the new value, and only accept a change that still contains `q quit`.
Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "feat(notes): footer hints degrade token by token instead of in two forms"
```

---

### Task 4: prompt-block keys and URLs become links

**Files:**
- Modify: `src/app.rs` (`prompt_block` ~1974-1996, `draw_preview` ~1547-1610)
- Test: inline test module in `src/app.rs`

**Interfaces:**
- Consumes: Task 1's `find_links`/`LinkKind`, Task 2's `LinkHit` and renamed fields, `truncate_w`/`dwidth` (existing).
- Produces: `fn prompt_block(groups: &[(String, Vec<crate::prompts::Prompt>)], width: usize, cfg: &crate::tickets::Config, cursor: Option<usize>) -> (Vec<Line<'static>>, Vec<markdown::LinkHit>)`; block hits ordered ahead of body hits in `App.link_hits`.

- [ ] **Step 1: Write the failing tests**

```rust
    fn block_of(text: &str) -> (Vec<Line<'static>>, Vec<markdown::LinkHit>) {
        let groups = vec![("claude p5".to_string(), vec![prompt(1, text)])];
        prompt_block(&groups, 60, &ticket_cfg(), None)
    }

    /// The config `ticket_app` injects, for the free-function tests.
    fn ticket_cfg() -> crate::tickets::Config {
        crate::tickets::Config::from_json(r#"{"HM":"https://example.test/browse/{key}"}"#)
    }

    #[test]
    fn a_key_in_the_prompt_block_is_a_hit() {
        let (_, hits) = block_of("look at HM-54283 today");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "HM-54283");
        assert_eq!(hits[0].kind, markdown::LinkKind::Ticket);
    }

    #[test]
    fn a_key_cut_by_truncation_is_not_a_hit() {
        // `truncate_w` appends NO ellipsis, so a cut key looks perfectly valid
        // and `o` would open the wrong ticket.
        let groups = vec![("claude p5".to_string(), vec![prompt(1, "padding HM-54283")])];
        let (lines, hits) = prompt_block(&groups, 14, &ticket_cfg(), None);
        assert!(hits.is_empty(), "{lines:?}");
    }

    #[test]
    fn block_hits_come_before_body_hits() {
        let mut a = ticket_app("body HM-2 here\n");
        a.prompts = vec![crate::prompts::PromptGroup {
            pane: "w1:p5".into(),
            prompts: vec![prompt(1, "prompt HM-1 here")],
        }];
        a.prompt_labels = vec!["claude p5".into()];
        rendered(&mut a, 60, 20);
        assert_eq!(
            a.link_hits.iter().map(|h| h.text.as_str()).collect::<Vec<_>>(),
            ["HM-1", "HM-2"]
        );
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.pending_open().as_deref(), Some("https://example.test/browse/HM-1"));
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.pending_open().as_deref(), Some("https://example.test/browse/HM-2"));
    }

    #[test]
    fn the_highlight_and_the_open_target_agree_across_both_regions() {
        // The one place two hit lists must agree on an order.
        let mut a = ticket_app("body HM-2 here\n");
        a.prompts = vec![crate::prompts::PromptGroup {
            pane: "w1:p5".into(),
            prompts: vec![prompt(1, "prompt HM-1 here")],
        }];
        a.prompt_labels = vec!["claude p5".into()];
        rendered(&mut a, 60, 20);
        for (ordinal, expected) in [(0usize, "HM-1"), (1, "HM-2")] {
            a.link_cursor = Some(ordinal);
            let mut term =
                ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 20)).unwrap();
            term.draw(|f| a.draw(f)).unwrap();
            let buf = term.backend().buffer();
            let reversed: String = (0..20)
                .flat_map(|y| (0..60).map(move |x| (x, y)))
                .filter_map(|(x, y)| buf.cell((x, y)))
                .filter(|c| c.modifier.contains(Modifier::REVERSED))
                .map(|c| c.symbol().to_string())
                .collect();
            assert!(reversed.contains(expected), "ordinal {ordinal}: reversed={reversed:?}");
            assert!(a.pending_open().unwrap().ends_with(expected));
        }
    }

    #[test]
    fn a_titled_body_less_note_still_carries_its_block_hits() {
        let mut a = ticket_app("");
        a.note.title = "named".into();
        a.prompts = vec![crate::prompts::PromptGroup {
            pane: "w1:p5".into(),
            prompts: vec![prompt(1, "prompt HM-1 here")],
        }];
        a.prompt_labels = vec!["claude p5".into()];
        rendered(&mut a, 60, 20);
        assert_eq!(a.link_hits.len(), 1, "the empty-note path must not drop block hits");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test prompt_block 2>&1 | head -30`
Expected: FAIL — `prompt_block` takes 2 arguments, not 4; returns `Vec<Line>`, not a tuple.

- [ ] **Step 3: Write the implementation**

Replace `prompt_block` with the linkifying form plus one line helper:

```rust
/// One prompt-block row: `text` truncated to `budget` display columns, with
/// every link inside the RETAINED prefix split into its own underlined span
/// (REVERSED when its ordinal is `cursor`). Hits are appended to `hits` with
/// `row`.
///
/// Truncation is the hazard here: `truncate_w` appends NO ellipsis, so a cut
/// `HM-54283` would read as a perfectly valid `HM-542` and `o` would open the
/// wrong ticket. The scan therefore runs on the FULL text and keeps only hits
/// that end inside the retained prefix — which is a byte prefix, so the
/// offsets line up.
fn block_line(
    number: &str,
    text: &str,
    budget: usize,
    style: Style,
    cfg: &crate::tickets::Config,
    cursor: Option<usize>,
    row: usize,
    hits: &mut Vec<markdown::LinkHit>,
) -> Line<'static> {
    let kept = truncate_w(text, budget);
    let mut spans: Vec<Span<'static>> = Vec::new();
    if !number.is_empty() {
        spans.push(Span::styled(number.to_string(), style));
    }
    let mut last = 0usize;
    for (range, kind) in markdown::find_links(text, cfg) {
        if range.end > kept.len() {
            break;
        }
        if range.start > last {
            spans.push(Span::styled(kept[last..range.start].to_string(), style));
        }
        let mut st = style.add_modifier(Modifier::UNDERLINED);
        if cursor == Some(hits.len()) {
            st = st.add_modifier(Modifier::REVERSED);
        }
        spans.push(Span::styled(kept[range.clone()].to_string(), st));
        hits.push(markdown::LinkHit { text: kept[range.clone()].to_string(), kind, row });
        last = range.end;
    }
    if last < kept.len() {
        spans.push(Span::styled(kept[last..].to_string(), style));
    }
    Line::from(spans)
}

/// The prompt block: one heading per group above the note, each group's prompts
/// numbered from 1, a blank line between groups and a rule underneath. Returns
/// the rows and every link found in them — rows here are 1:1 with lines
/// (truncated, never wrapped), so a hit's row is its line index. `cursor` is an
/// ordinal into THIS block's hits, which are the first ones in the pane's list.
fn prompt_block(
    groups: &[(String, Vec<crate::prompts::Prompt>)],
    width: usize,
    cfg: &crate::tickets::Config,
    cursor: Option<usize>,
) -> (Vec<Line<'static>>, Vec<markdown::LinkHit>) {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let head = Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM);
    let mut out: Vec<Line<'static>> = Vec::new();
    let mut hits: Vec<markdown::LinkHit> = Vec::new();
    for (label, prompts) in groups.iter().filter(|(_, p)| !p.is_empty()) {
        if !out.is_empty() {
            out.push(Line::raw(""));
        }
        let row = out.len();
        out.push(block_line("", label, width, head, cfg, cursor, row, &mut hits));
        for (i, p) in prompts.iter().enumerate() {
            // The number and its separator cost 3 columns.
            let row = out.len();
            out.push(block_line(
                &format!("{}. ", i + 1),
                &p.text,
                width.saturating_sub(3),
                dim,
                cfg,
                cursor,
                row,
                &mut hits,
            ));
        }
    }
    if !out.is_empty() {
        out.push(Line::from(Span::styled("─".repeat(width), dim)));
    }
    (out, hits)
}
```

Note the numbering change: the old code wrote `format!("{}. {body}", i + 1)` into one span; `block_line` takes the number as its own span so the body's link offsets stay aligned with the source text.

In `draw_preview`, thread the block hits through and split the cursor:

```rust
        let (block, block_hits) = if self.showing_tab_note() {
            prompt_block(&self.labelled_prompts(), text_w, &self.tickets, self.link_cursor)
        } else {
            (Vec::new(), Vec::new())
        };
        // The block sits above the note, so its hits are the FIRST ordinals and
        // a body cursor is offset by however many the block holds. `None` here
        // means "the cursor is in the block" — the block already applied its own
        // REVERSED, so the body render must not claim the ordinal too.
        let body_cursor = self.link_cursor.and_then(|c| c.checked_sub(block_hits.len()));
```

The empty-note branch keeps the block hits instead of clearing them (a titled, body-less note still accumulates prompts):

```rust
                self.link_hits = block_hits;
```

and the real-note branch concatenates, shifting only the BODY rows:

```rust
                let (mut lines, mut map, mut hits) = markdown::render_markdown_links(
                    &self.note.text,
                    text_w,
                    &self.tickets,
                    body_cursor,
                );
                if !block.is_empty() {
                    let n = block.len();
                    let mut merged = block;
                    merged.append(&mut lines);
                    lines = merged;
                    let mut merged_map = vec![None; n];
                    merged_map.append(&mut map);
                    map = merged_map;
                    for hit in &mut hits {
                        hit.row += n;
                    }
                }
                let mut all = block_hits;
                all.extend(hits);
                self.link_hits = all;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test`
Expected: PASS. The three pre-existing `prompt_block_*` tests need their call sites updated to the new signature and to destructure the tuple; their assertions stay as they are.
Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "feat(notes): prompt-block keys and URLs join the link cursor"
```

---

### Task 5: title links, and bare `o`

**Files:**
- Modify: `src/app.rs` (the title assembly in `draw` ~1440-1460, `pending_open` ~1220)
- Test: inline test module in `src/app.rs`

**Interfaces:**
- Consumes: Task 1's `find_links`, Task 2's `pending_open`.
- Produces: `pending_open` falling back to the title's first link when no cursor is live.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn the_header_title_underlines_its_key() {
        let mut a = ticket_app("body\n");
        a.note.title = "Design new ticket HM-54599".into();
        let mut term = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 10)).unwrap();
        term.draw(|f| a.draw(f)).unwrap();
        let buf = term.backend().buffer();
        let underlined: String = (0..60)
            .filter_map(|x| buf.cell((x, 0)))
            .filter(|c| c.modifier.contains(Modifier::UNDERLINED))
            .map(|c| c.symbol().to_string())
            .collect();
        assert_eq!(underlined, "HM-54599");
    }

    #[test]
    fn bare_o_opens_the_titles_key() {
        // The note is usually named after its ticket, and no cursor can live in
        // the header, so `o` with no cursor is the one-keystroke path.
        let mut a = ticket_app("body with no keys\n");
        a.note.title = "Design new ticket HM-54599".into();
        rendered(&mut a, 60, 10);
        assert_eq!(
            a.pending_open().as_deref(),
            Some("https://example.test/browse/HM-54599")
        );
    }

    #[test]
    fn a_live_cursor_beats_the_title() {
        let mut a = ticket_app("body HM-2 here\n");
        a.note.title = "titled HM-1".into();
        rendered(&mut a, 60, 10);
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.pending_open().as_deref(), Some("https://example.test/browse/HM-2"));
    }

    #[test]
    fn bare_o_with_no_key_in_the_title_is_a_no_op() {
        let mut a = ticket_app("body with no keys\n");
        a.note.title = "just a name".into();
        rendered(&mut a, 60, 10);
        assert_eq!(a.pending_open(), None);
        a.on_key(key(KeyCode::Char('o')));
        assert!(a.open_children.is_empty());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test the_titles_key 2>&1 | head -20`
Expected: FAIL — nothing underlined on row 0; `pending_open` returns `None` with no cursor.

- [ ] **Step 3: Write the implementation**

In `draw`, replace the single bold title span with a split one:

```rust
            } else if !self.note.title.trim().is_empty() {
                title.push(Span::raw(" —"));
                let bold = Style::default().add_modifier(Modifier::BOLD);
                // Underlined for consistency with the body, but cursorless: the
                // header is a 1-row no-wrap Paragraph outside the scrollable
                // body, so `n`/`N` can never reach it. Bare `o` is the
                // affordance instead (see `pending_open`).
                let text = format!(" {}", self.note.title);
                let mut last = 0usize;
                for (range, _) in markdown::find_links(&text, &self.tickets) {
                    if range.start > last {
                        title.push(Span::styled(text[last..range.start].to_string(), bold));
                    }
                    title.push(Span::styled(
                        text[range.clone()].to_string(),
                        bold.add_modifier(Modifier::UNDERLINED),
                    ));
                    last = range.end;
                }
                if last < text.len() {
                    title.push(Span::styled(text[last..].to_string(), bold));
                }
            }
```

and extend `pending_open`:

```rust
    /// The URL `o` would open right now, or `None`. With a cursor live it is the
    /// cursored hit; with no cursor it is the header TITLE's first link, which
    /// is where the ticket usually is and which no cursor can reach. Separate
    /// from `open_ticket` so the resolution is testable without a browser.
    fn pending_open(&self) -> Option<String> {
        let (text, kind) = match self.link_cursor.and_then(|c| self.link_hits.get(c)) {
            Some(hit) => (hit.text.clone(), hit.kind),
            None => {
                let (range, kind) =
                    markdown::find_links(&self.note.title, &self.tickets).into_iter().next()?;
                (self.note.title[range].to_string(), kind)
            }
        };
        match kind {
            markdown::LinkKind::Ticket => crate::tickets::ticket_url(&self.tickets, &text),
            markdown::LinkKind::Url => Some(text),
        }
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test`
Expected: PASS.
Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "feat(notes): underline the title's key and let bare o open it"
```

---

### Task 6: config hot-reload, docs, and the live check

**Files:**
- Modify: `src/tickets.rs` (add `config_path`), `src/app.rs` (field + heartbeat), `README.md`, `CLAUDE.md`
- Test: inline test modules in both files

**Interfaces:**
- Consumes: `crate::state::store_dir` (existing), `Config::load` (existing).
- Produces: `pub fn config_path() -> Option<std::path::PathBuf>` in `tickets`; `App.tickets_mtime: Option<std::time::SystemTime>`.

- [ ] **Step 1: Write the failing tests**

In `src/tickets.rs`:

```rust
    #[test]
    fn the_config_path_sits_beside_the_notes() {
        // Same store dir as the note files, so it follows the same three tiers.
        let _guard = crate::state::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("notes-cfgpath-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: serialized by ENV_LOCK; restored below.
        unsafe { std::env::set_var("HERDR_PLUGIN_STATE_DIR", &dir) };
        assert_eq!(config_path(), Some(dir.join(FILE)));
        unsafe { std::env::remove_var("HERDR_PLUGIN_STATE_DIR") };
        let _ = std::fs::remove_dir_all(&dir);
    }
```

Match the exact env-swap convention the existing `state.rs`/`app.rs` tests use (`swap_env` helper and `ENV_LOCK`) rather than inventing a second one — read one of them first and follow it.

In `src/app.rs`:

```rust
    #[test]
    fn the_heartbeat_picks_up_an_edited_config() {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let dir = std::env::temp_dir().join(format!("notes-hotreload-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let restore = swap_env(&dir);

        let mut a = App::new();
        assert!(a.tickets.is_empty(), "no config file yet");

        let path = dir.join(crate::tickets::FILE);
        std::fs::write(&path, r#"{"TT":"https://example.test/{key}"}"#).unwrap();
        a.last_beat = Instant::now() - HEARTBEAT_EVERY;
        a.heartbeat();
        assert!(a.tickets.has_prefix("TT"), "an edited config is picked up");

        // An UNCHANGED mtime must not reload: clobber the in-memory config and
        // check the beat leaves it alone.
        a.tickets = crate::tickets::Config::default();
        a.last_beat = Instant::now() - HEARTBEAT_EVERY;
        a.heartbeat();
        assert!(a.tickets.is_empty(), "unchanged mtime: no reload");
        a.tickets = crate::tickets::Config::load(); // put it back for the next step

        std::fs::remove_file(&path).unwrap();
        a.last_beat = Instant::now() - HEARTBEAT_EVERY;
        a.heartbeat();
        assert!(a.tickets.is_empty(), "a deleted config turns the feature off");

        restore();
        let _ = std::fs::remove_dir_all(&dir);
    }
```

`swap_env` is the existing helper (see the `App::new` tests around `src/app.rs:2713`); reuse it exactly, including how it restores.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test hot_reload; cargo test the_config_path 2>&1 | head -20`
Expected: FAIL — `cannot find function config_path`; the config is not re-read on the heartbeat.

- [ ] **Step 3: Write the implementation**

In `src/tickets.rs`:

```rust
/// Where the config lives: beside the note files, so it follows the same
/// three-tier store resolution. Pure path logic, no I/O.
pub fn config_path() -> Option<std::path::PathBuf> {
    crate::state::store_dir().map(|dir| dir.join(FILE))
}
```

and have `Config::load` use it, so the path is spelled once:

```rust
    pub fn load() -> Self {
        config_path()
            .and_then(|p| std::fs::read_to_string(p).ok())
            .map(|s| Self::from_json(&s))
            .unwrap_or_default()
    }
```

In `src/app.rs`, add the field beside `tickets`:

```rust
    /// Modification time of `tickets.json` as of the last read, so the
    /// heartbeat can reload only when it actually changed.
    tickets_mtime: Option<std::time::SystemTime>,
```

`tickets_mtime: None` in `with_note`'s literal; in `App::new`, stamp it beside the existing load:

```rust
        app.tickets = crate::tickets::Config::load();
        app.tickets_mtime = crate::tickets::config_path()
            .and_then(|p| std::fs::metadata(p).ok())
            .and_then(|m| m.modified().ok());
```

and in `heartbeat`, right after the browser-child reap:

```rust
        // Config hot-reload: ONE local `metadata` stat per beat, and a real
        // read only when the mtime moved. Deliberately not the `git_branch`
        // class of gotcha — that one is about SPAWNING a process on the
        // event-loop thread; a stat of a local file is not in that class. Keep
        // it that way: anything heavier here freezes input, drawing and the
        // identity re-stamp. A missing file gives `None`, which differs from a
        // stamped `Some` and so reloads to an empty config — the feature going
        // dormant, symmetric with never having had a config at all.
        if self.persist {
            let mtime = crate::tickets::config_path()
                .and_then(|p| std::fs::metadata(p).ok())
                .and_then(|m| m.modified().ok());
            if mtime != self.tickets_mtime {
                self.tickets_mtime = mtime;
                self.tickets = crate::tickets::Config::load();
            }
        }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test`
Expected: PASS.
Run: `cargo clippy --all-targets -- -D warnings` and `cargo build --release`
Expected: clean. (Close any open Notes pane first — os error 5 otherwise.)

- [ ] **Step 5: Document it**

`README.md` — rewrite the ticket-links section as a links section:

```markdown
### Links

Configured issue keys (`HM-54599`) and bare `http(s)://` URLs in the note are
underlined. `n`/`N` walk them — the captured-prompt block above the note counts
too, and its links come first — `o` opens the selected one in your browser,
`esc` drops the cursor. With NO cursor live, `o` opens the first key in the
note's title, which is where the ticket usually is.

Create `tickets.json` beside the note files (`%LOCALAPPDATA%\herdr\plugins\herdr-notes\`
inside herdr, `%APPDATA%\herdr\notes\` outside it; unix
`~/.local/share/herdr/plugins/herdr-notes/` and `~/.config/herdr/notes/`):

```json
{
  "HM": "https://your-org.atlassian.net/browse/{key}",
  "CR": "https://your-tracker.example/issue/{key}"
}
```

A prefix must be two or more UPPERCASE letters, and only listed prefixes are
detected — an unmapped key is never underlined and never pretends to be
openable. Edits are picked up within about five seconds, no restart needed. A
missing or malformed file simply turns issue keys off; URLs need no config.
```

`CLAUDE.md` — update in place:
- the `src/markdown.rs` Layout entry: `find_links` is now the single scan over BOTH kinds, `find_ticket_ranges`/`find_url_ranges` are its internals, `render_markdown_links` replaces `render_markdown_tickets`, and `render_markdown`/`render_markdown_mapped` are the LINK-FREE entry points the overlay preview uses
- the `src/app.rs` entry: `link_hits`/`link_cursor`/`follow_link`, block-then-body hit ordering, the title's cursorless underline plus bare `o`, and the footer's ranked token slices with `fit_hints`
- the `src/tickets.rs` entry: `config_path` and mtime hot-reload
- the footer sentence: it no longer picks between fixed forms; it drops ranked tokens, `q quit` never drops, and the exact outputs at 79/46/37 columns are pinned by tests

and add these Gotchas:

```markdown
- `prompt_block` TRUNCATES with `truncate_w` and appends NO ellipsis, so a cut
  `HM-54283` reads as a perfectly valid `HM-542`. Link detection there must scan
  the FULL prompt text and keep only hits ending inside the retained prefix, or
  `o` opens a ticket that was never in the note. Same reason `block_line` takes
  the `N. ` number as its own span: putting it in the same string would shift
  every link offset by three.
- Block links occupy the FIRST ordinals and body links are offset by
  `block_hits.len()`. `render_markdown_links` highlights the nth BODY hit, so
  `draw_preview` passes `cursor - block_hits.len()` and `None` when the cursor
  is in the block (the block applied its own REVERSED). Two hit lists agreeing
  on one order is the same failure shape as the cross-line ordinal bug.
- The empty-note preview path must KEEP the block's hits, not clear them: a
  titled, body-less note still has a note file and so still accumulates
  prompts, and those prompts are the only links it has.
- The header title can be styled but can NEVER host a cursor — it is a 1-row
  no-wrap `Paragraph` outside the scrollable body. Bare `o` (no cursor live)
  opening the title's first link is the affordance that replaces one, and it is
  why `pending_open` has a title fallback at all.
- URLs need no config, so `emit`'s old "empty `Config` means nothing to find"
  fast path is GONE. What keeps the overlay's read-only preview plain is the
  `enabled` flag on `LinkCtx`: `render_markdown`/`render_markdown_mapped` build
  a disabled context. Anything new that renders a note nobody can put a cursor
  in belongs on that path too.
- Config hot-reload is ONE `metadata` stat per 5s beat, and a read only when the
  mtime moved. That is deliberately not the `git_branch` gotcha's class (that
  one is about SPAWNING on the event-loop thread) — but the ceiling is the same:
  nothing heavier than a local stat belongs on the heartbeat.
- Footer hints drop by RANK, greedily, and are not optimally packed: at 37
  columns a shorter token that would still have fitted is not re-added. Accepted
  for one-sentence predictability; the exact outputs at 79/46/37 are asserted so
  a change is visible.
```

- [ ] **Step 6: Verify live in a throwaway pane (controller-owned)**

Do NOT run this step as an implementer — it touches the user's live herdr
session and opens a browser. The controller runs it: a pane with a note holding
a prompt-block key, a body key and a pasted URL; `n` walking block → body,
`o` opening each; the footer showing `n/N link` at ~46 columns; and an edit to
`tickets.json` taking effect within ~5s without restarting the pane.

- [ ] **Step 7: Commit**

```bash
git add src/tickets.rs src/app.rs README.md CLAUDE.md
git commit -m "feat(notes): hot-reload the link config, document the link keys"
```

---

## Notes for the implementer

- The `HINTS_*` ranks are the whole UX of the footer. If you change a rank,
  update the three exact-string tests deliberately, not to make them pass.
- `block_line` and `style_links` both split spans on `find_links`. They are NOT
  duplicates to be merged: one works on already-truncated single-row text with
  byte offsets, the other on pre-wrap span lists with char offsets. Merging them
  would mean one of the two loses the property that makes it correct.
- Every silent-failure path is deliberate. Do not add an error message, a status
  line, or a `dbg!` — the TUI printing anything corrupts its own screen, and the
  same binary has a `--capture-prompt` hook mode where stdout is injected into
  the user's prompt.
