# Ticket Links Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Underline configured issue keys (`HM-54561`) in the notes preview, walk them with `n`/`N`, and open the cursored one in the default browser with `o`.

**Architecture:** One hand-rolled matcher in `markdown.rs` runs during render, so the styling and the navigable hit list come from a single scan (a second scan over raw source would see `HM-**54561**` where render sees `HM-54561` and the ordinals would slip). Render returns a `Vec<TicketHit>` that `App` caches on every preview draw; `n`/`N`/`o` read that cache. Prefix→URL templates live in a hand-edited `tickets.json` beside the note files, loaded once at construction.

**Tech Stack:** Rust 2024, ratatui 0.30, crossterm 0.29, serde_json, unicode-width. No new dependencies — the matcher is hand-rolled like the rest of the renderer.

**Spec:** `docs/superpowers/specs/2026-07-28-ticket-links-design.md`

## Global Constraints

- No new crate dependencies. No `regex`.
- `cargo build --release`, `cargo test`, and `cargo clippy --all-targets -- -D warnings` must all pass before any task is considered done. `cargo build --release` fails with os error 5 while a Notes TUI is running in a pane — quit the pane first (`Get-Process herdr-notes | Stop-Process` for stragglers).
- Every failure path is a SILENT no-op: no printing, no new UI, no panic. Missing config, malformed config, unmapped prefix, spawn refused.
- Detection covers the rendered note body in preview mode only. Not the header title, not the prompt block, not bare URLs.
- Only prefixes present in the config map are detected at all, so `n` can never land on a key `o` cannot open.
- Never block the event-loop thread: browser launch uses `spawn()`, never `output()`. A blocking wait freezes input, drawing and the 5s identity re-stamp; past 20s of no re-stamp the launcher treats the pane as a corpse and REPLACEs it, and `pane close` kills with no signal, losing the dirty debounce buffer.
- On Windows, any spawned process gets `creation_flags(0x0800_0000)` (`CREATE_NO_WINDOW`) or a console flashes over the TUI.
- Unit tests never touch the real store dir: `App::with_note(note, false)` and config injected by assigning the field directly.
- Esc must NEVER exit the TUI. Only `q` quits.

---

### Task 1: `tickets` module — config and URL building

**Files:**
- Create: `src/tickets.rs`
- Modify: `src/main.rs` (add `mod tickets;`)
- Test: inline `#[cfg(test)] mod tests` in `src/tickets.rs`

**Interfaces:**
- Consumes: `crate::state::store_dir() -> Option<PathBuf>` (already exists).
- Produces:
  - `pub struct Config` (opaque, `Clone + Debug + Default`)
  - `Config::from_json(&str) -> Config`
  - `Config::load_in(&Path) -> Config`
  - `Config::load() -> Config`
  - `Config::has_prefix(&self, &str) -> bool`
  - `Config::is_empty(&self) -> bool`
  - `pub fn ticket_url(&Config, key: &str) -> Option<String>`

- [ ] **Step 1: Write the failing tests**

Create `src/tickets.rs` with the test module only (no implementation yet — the file must fail to compile against the missing items, which is the failure we want):

```rust
//! Ticket links: the prefix→URL config and the browser launch.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_map_loads() {
        let cfg = Config::from_json(r#"{"HM":"https://example.test/browse/{key}"}"#);
        assert!(cfg.has_prefix("HM"));
        assert!(!cfg.is_empty());
        assert_eq!(
            ticket_url(&cfg, "HM-54561").as_deref(),
            Some("https://example.test/browse/HM-54561")
        );
    }

    #[test]
    fn a_template_without_the_placeholder_is_dropped() {
        // Opening a keyless URL is worse than doing nothing: the user would
        // land on a tracker home page and think the feature worked.
        let cfg = Config::from_json(r#"{"HM":"https://example.test/browse/"}"#);
        assert!(!cfg.has_prefix("HM"));
        assert!(cfg.is_empty());
    }

    #[test]
    fn junk_input_degrades_to_an_empty_map() {
        for src in ["", "not json", "[]", "null", r#"{"HM":42}"#, r#"{"":"x/{key}"}"#] {
            let cfg = Config::from_json(src);
            assert!(cfg.is_empty(), "{src:?} should yield an empty map");
        }
    }

    #[test]
    fn a_bom_prefixed_file_still_parses() {
        // herdr panes run Windows PowerShell 5.1, whose `Set-Content -Encoding
        // UTF8` writes a BOM. A hand-written config is exactly the file that
        // gets created that way.
        let cfg = Config::from_json("\u{feff}{\"HM\":\"https://example.test/{key}\"}");
        assert!(cfg.has_prefix("HM"));
    }

    #[test]
    fn an_unmapped_prefix_has_no_url() {
        let cfg = Config::from_json(r#"{"HM":"https://example.test/{key}"}"#);
        assert_eq!(ticket_url(&cfg, "CR-3171"), None);
        assert_eq!(ticket_url(&cfg, "nonsense"), None);
    }

    #[test]
    fn a_missing_file_is_an_empty_map() {
        let dir = std::env::temp_dir().join(format!("notes-tickets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(Config::load_in(&dir).is_empty());

        std::fs::write(dir.join(FILE), r#"{"TT":"https://example.test/{key}"}"#).unwrap();
        assert!(Config::load_in(&dir).has_prefix("TT"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
```

Add `mod tickets;` to `src/main.rs` beside the other `mod` declarations.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test tickets`
Expected: FAIL — `cannot find type Config in this scope` / `cannot find function ticket_url`.

- [ ] **Step 3: Write the implementation**

Insert above the test module in `src/tickets.rs`:

```rust
use std::collections::BTreeMap;
use std::path::Path;

/// Hand-edited by the user, beside the note files.
pub const FILE: &str = "tickets.json";

/// Issue-key prefix → URL template containing `{key}`. Empty map means the
/// whole feature is dormant: nothing is detected, styled or openable.
#[derive(Clone, Debug, Default)]
pub struct Config {
    map: BTreeMap<String, String>,
}

impl Config {
    /// Forgiving parse, matching the rest of the crate: anything unusable is
    /// dropped silently rather than failing the load. A template with no
    /// `{key}` is dropped too — a keyless URL would open the tracker's home
    /// page and read as success.
    pub fn from_json(s: &str) -> Self {
        let mut map = BTreeMap::new();
        // PS 5.1 writes a UTF-8 BOM; every stdin/file parser in this crate
        // strips it.
        if let Ok(serde_json::Value::Object(obj)) =
            serde_json::from_str::<serde_json::Value>(s.trim_start_matches('\u{feff}'))
        {
            for (prefix, template) in obj {
                if let Some(t) = template.as_str()
                    && !prefix.is_empty()
                    && t.contains("{key}")
                {
                    map.insert(prefix, t.to_string());
                }
            }
        }
        Self { map }
    }

    /// Reads `tickets.json` from `dir`. Injected base dir so tests never touch
    /// the real store, exactly as `state.rs` does it.
    pub fn load_in(dir: &Path) -> Self {
        std::fs::read_to_string(dir.join(FILE))
            .map(|s| Self::from_json(&s))
            .unwrap_or_default()
    }

    /// The real load: `tickets.json` in the note store dir, so it follows the
    /// same three-tier resolution the note files use.
    pub fn load() -> Self {
        crate::state::store_dir().map(|d| Self::load_in(&d)).unwrap_or_default()
    }

    pub fn has_prefix(&self, prefix: &str) -> bool {
        self.map.contains_key(prefix)
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

/// The URL for `key`, or `None` when its prefix is unmapped. Pure — this is the
/// tested seam; `open` itself stays thin and untested.
pub fn ticket_url(cfg: &Config, key: &str) -> Option<String> {
    let (prefix, _) = key.split_once('-')?;
    Some(cfg.map.get(prefix)?.replace("{key}", key))
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test tickets`
Expected: PASS (6 tests).
Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/tickets.rs src/main.rs
git commit -m "feat(tickets): prefix->URL config, loaded from the note store dir"
```

---

### Task 2: the matcher

**Files:**
- Modify: `src/markdown.rs`
- Test: inline test module in `src/markdown.rs`

**Interfaces:**
- Consumes: `crate::tickets::Config::has_prefix` (Task 1).
- Produces: `pub fn find_tickets(s: &str, cfg: &crate::tickets::Config) -> Vec<std::ops::Range<usize>>` — byte ranges of every configured issue key in `s`, left to right, non-overlapping.

- [ ] **Step 1: Write the failing tests**

Add to `markdown.rs`'s existing `#[cfg(test)] mod tests`:

```rust
    fn cfg() -> crate::tickets::Config {
        crate::tickets::Config::from_json(
            r#"{"HM":"https://example.test/{key}","CR":"https://example.test/c/{key}"}"#,
        )
    }

    fn keys(s: &str) -> Vec<String> {
        find_tickets(s, &cfg()).into_iter().map(|r| s[r].to_string()).collect()
    }

    #[test]
    fn the_matcher_finds_configured_keys_left_to_right() {
        assert_eq!(keys("To estimate CR-3171 HM-54561"), ["CR-3171", "HM-54561"]);
        assert_eq!(keys("HM-1"), ["HM-1"]);
    }

    #[test]
    fn the_matcher_rejects_near_misses() {
        for s in ["hm-1", "HM-", "H-1", "xHM-1", "HM-1x", "HM-1_", "-1", "HM1"] {
            assert!(keys(s).is_empty(), "{s:?} is not a ticket key");
        }
    }

    #[test]
    fn the_matcher_skips_unconfigured_prefixes() {
        assert!(keys("ABC-99").is_empty());
    }

    #[test]
    fn the_matcher_tolerates_multibyte_neighbours() {
        // `start` only walks back over ASCII uppercase, so the byte before it
        // can be a UTF-8 continuation byte — slicing there must not panic.
        assert_eq!(keys("café HM-7 —"), ["HM-7"]);
    }

    #[test]
    fn punctuation_is_a_boundary() {
        assert_eq!(keys("(HM-2), [HM-3]."), ["HM-2", "HM-3"]);
        assert_eq!(keys("HM-4-final"), ["HM-4"]);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib markdown 2>&1 | head -20` (or `cargo test the_matcher`)
Expected: FAIL — `cannot find function find_tickets in this scope`.

- [ ] **Step 3: Write the implementation**

Add to `src/markdown.rs`, below `toggle_checkbox`:

```rust
/// Byte ranges of every CONFIGURED issue key in `s`, left to right and
/// non-overlapping. A key is an uppercase ASCII run of 2+, a `-`, then 1+
/// ASCII digits, with a non-alphanumeric boundary on both sides — and its
/// prefix must be in `cfg`, so an unmapped tracker is never highlighted and
/// the ticket cursor can never land on something `o` cannot open.
///
/// The crate's ONE ticket scan. Anything that needs to know where the keys are
/// goes through here, for the same reason the checkbox parser is single-homed:
/// a second scan drifts.
pub fn find_tickets(s: &str, cfg: &crate::tickets::Config) -> Vec<std::ops::Range<usize>> {
    fn keyish(b: u8) -> bool {
        b.is_ascii_alphanumeric() || b == b'_'
    }
    let bytes = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'-' {
            i += 1;
            continue;
        }
        // The uppercase run before the dash, then the digit run after it. Both
        // walks stay inside ASCII, so `start`/`end` land on char boundaries.
        let mut start = i;
        while start > 0 && bytes[start - 1].is_ascii_uppercase() {
            start -= 1;
        }
        let mut end = i + 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        let long_enough = i - start >= 2 && end > i + 1;
        let bounded = (start == 0 || !keyish(bytes[start - 1]))
            && (end == bytes.len() || !keyish(bytes[end]));
        if long_enough && bounded && cfg.has_prefix(&s[start..i]) {
            out.push(start..end);
            i = end;
            continue;
        }
        i += 1;
    }
    out
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test find_tickets; cargo test the_matcher; cargo test punctuation_is_a_boundary`
Expected: PASS.
Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/markdown.rs
git commit -m "feat(markdown): single-homed issue-key matcher"
```

---

### Task 3: render tickets — styling plus the hit list

**Files:**
- Modify: `src/markdown.rs`
- Test: inline test module in `src/markdown.rs`

**Interfaces:**
- Consumes: `find_tickets` (Task 2), `crate::tickets::Config` (Task 1).
- Produces:
  - `pub struct TicketHit { pub key: String, pub row: usize }` (`Clone + Debug + PartialEq`)
  - `pub fn render_markdown_tickets(text: &str, width: usize, cfg: &crate::tickets::Config, cursor: Option<usize>) -> (Vec<Line<'static>>, Vec<Option<usize>>, Vec<TicketHit>)`
  - `render_markdown_mapped(text, width)` and `render_markdown(text, width)` keep their current signatures as wrappers (empty config, no cursor), so every existing caller and test is untouched.

`row` is an index into the returned `lines`, i.e. the rendered row the key's first character landed on. Hits are in document order, which IS the navigation order.

- [ ] **Step 1: Write the failing tests**

Add to `markdown.rs`'s test module (`cfg()` from Task 2 is reused):

```rust
    fn hit_style(lines: &[Line], key: &str) -> Style {
        lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content == key)
            .expect("key should be its own span")
            .style
    }

    #[test]
    fn ticket_keys_render_as_their_own_underlined_span() {
        let (lines, _, hits) =
            render_markdown_tickets("To estimate HM-54561 today", 40, &cfg(), None);
        assert_eq!(hits, vec![TicketHit { key: "HM-54561".into(), row: 0 }]);
        let st = hit_style(&lines, "HM-54561");
        assert!(st.add_modifier.contains(Modifier::UNDERLINED));
        assert!(!st.add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn the_cursored_hit_is_reversed_and_only_it() {
        let (lines, _, hits) =
            render_markdown_tickets("HM-1 and CR-2", 40, &cfg(), Some(1));
        assert_eq!(hits.len(), 2);
        assert!(!hit_style(&lines, "HM-1").add_modifier.contains(Modifier::REVERSED));
        assert!(hit_style(&lines, "CR-2").add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn hits_carry_the_row_they_landed_on() {
        let text = "# Head\n\n- first HM-1\n- second CR-2";
        let (_, _, hits) = render_markdown_tickets(text, 40, &cfg(), None);
        let rows: Vec<usize> = hits.iter().map(|h| h.row).collect();
        assert_eq!(hits.iter().map(|h| h.key.as_str()).collect::<Vec<_>>(), ["HM-1", "CR-2"]);
        assert!(rows[0] < rows[1], "rows ascend with document order: {rows:?}");
    }

    #[test]
    fn a_key_inside_bold_is_one_hit_and_stays_bold() {
        let (lines, _, hits) = render_markdown_tickets("**HM-9** done", 40, &cfg(), None);
        assert_eq!(hits.len(), 1, "markdown markers must not split the key");
        let st = hit_style(&lines, "HM-9");
        assert!(st.add_modifier.contains(Modifier::BOLD));
        assert!(st.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn fenced_code_is_not_linkified() {
        let text = "```\nHM-1\n```";
        let (_, _, hits) = render_markdown_tickets(text, 40, &cfg(), None);
        assert!(hits.is_empty(), "fenced code is code, like checkbox_lines treats it");
    }

    #[test]
    fn a_wrapped_key_keeps_its_style_on_every_row() {
        // Width 10 forces the key onto its own continuation row; the char-level
        // wrap must carry the underline across.
        let (lines, _, hits) =
            render_markdown_tickets("aaaa bbbb cccc HM-12345", 10, &cfg(), None);
        assert_eq!(hits.len(), 1);
        let underlined: usize = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter(|s| s.style.add_modifier.contains(Modifier::UNDERLINED))
            .map(|s| s.content.chars().count())
            .sum();
        assert_eq!(underlined, "HM-12345".chars().count());
        assert!(hits[0].row < lines.len());
    }

    #[test]
    fn a_key_inside_inline_code_is_still_one_hit() {
        // The pass runs over the assembled span list, code spans included, so
        // nav and styling agree on a backticked key.
        let (lines, _, hits) = render_markdown_tickets("see `HM-8` please", 40, &cfg(), None);
        assert_eq!(hits.len(), 1);
        assert!(hit_style(&lines, "HM-8").add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn an_empty_config_changes_nothing() {
        let empty = crate::tickets::Config::default();
        let (lines, map, hits) = render_markdown_tickets("HM-1 here", 40, &empty, None);
        assert!(hits.is_empty());
        let (base_lines, base_map) = render_markdown_mapped("HM-1 here", 40);
        assert_eq!(lines.len(), base_lines.len());
        assert_eq!(map, base_map);
        assert!(
            !lines
                .iter()
                .flat_map(|l| l.spans.iter())
                .any(|s| s.style.add_modifier.contains(Modifier::UNDERLINED))
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test render_markdown_tickets 2>&1 | head -20`
Expected: FAIL — `cannot find function render_markdown_tickets` / `cannot find struct TicketHit`.

- [ ] **Step 3: Write the implementation**

Three edits in `src/markdown.rs`.

(a) Replace the two public entry points and `render_line`'s signature. `render_markdown_mapped` keeps its signature and delegates:

```rust
/// One configured issue key found while rendering: the key text and the
/// rendered row its first character landed on. Document order == the order
/// `n`/`N` walk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TicketHit {
    pub key: String,
    pub row: usize,
}

/// Per-render ticket state: what to match, which hit is cursored, and the hits
/// found so far. `hits.len()` doubles as the ordinal of the next hit, which is
/// why detection and highlight can never disagree — one pass assigns both.
struct TicketCtx<'a> {
    cfg: &'a crate::tickets::Config,
    cursor: Option<usize>,
    hits: Vec<TicketHit>,
}

pub fn render_markdown(text: &str, width: usize) -> Vec<Line<'static>> {
    render_markdown_mapped(text, width).0
}

pub fn render_markdown_mapped(text: &str, width: usize) -> (Vec<Line<'static>>, Vec<Option<usize>>) {
    let cfg = crate::tickets::Config::default();
    let (lines, map, _) = render_markdown_tickets(text, width, &cfg, None);
    (lines, map)
}

/// `render_markdown_mapped` plus ticket links: keys configured in `cfg` are
/// underlined (the `cursor`-th one also REVERSED) and returned as hits.
pub fn render_markdown_tickets(
    text: &str,
    width: usize,
    cfg: &crate::tickets::Config,
    cursor: Option<usize>,
) -> (Vec<Line<'static>>, Vec<Option<usize>>, Vec<TicketHit>) {
    let width = width.max(8);
    let mut out = Vec::new();
    let mut map: Vec<Option<usize>> = Vec::new();
    let mut ctx = TicketCtx { cfg, cursor, hits: Vec::new() };
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
            // Fenced code is code: no linkification, exactly as
            // `checkbox_lines` skips it.
            wrap_into(&mut out, vec![(line.to_string(), Style::default().fg(CODE))], width, 0);
        } else {
            render_line(&mut out, &mut ctx, line, width);
        }
        map.resize(out.len(), Some(src));
    }
    if out.is_empty() {
        out.push(Line::raw(""));
        map.push(None);
    }
    (out, map, ctx.hits)
}
```

Then in `render_line`, change the signature to
`fn render_line(out: &mut Vec<Line<'static>>, ctx: &mut TicketCtx<'_>, line: &str, width: usize)`
and replace EVERY `wrap_into(out, spans, width, hang)` call inside it with
`emit(out, ctx, spans, width, hang)` (the heading, blockquote, checkbox, bullet, numbered-list and plain-paragraph branches — six call sites). The `is_hr` and empty-line branches push directly and are untouched.

(b) Add the ticket-aware emit path next to `wrap_into`:

```rust
/// `wrap_into` with the ticket pass in front of it: keys get their own styled
/// span, and each one's rendered row is recorded as a hit. Bypassed entirely
/// when no prefixes are configured, so the feature costs nothing when unused.
fn emit(
    out: &mut Vec<Line<'static>>,
    ctx: &mut TicketCtx<'_>,
    spans: Vec<(String, Style)>,
    width: usize,
    hang: usize,
) {
    if ctx.cfg.is_empty() {
        wrap_into(out, spans, width, hang);
        return;
    }
    let (spans, marks) = style_tickets(spans, ctx);
    let base = out.len();
    let offsets: Vec<usize> = marks.iter().map(|(off, _)| *off).collect();
    let rows = wrap_into_marked(out, spans, width, hang, &offsets);
    for ((_, key), row) in marks.into_iter().zip(rows) {
        ctx.hits.push(TicketHit { key, row: base + row });
    }
}

/// Splits every configured key out of `spans` into its own span — underlined,
/// plus REVERSED when its ordinal is the cursored one — keeping whatever style
/// the surrounding text already had (bold, code, dim quote). Returns the
/// rebuilt spans and, per key, its char offset into the flattened sequence and
/// its text. Char offsets rather than byte offsets because `wrap_into` works in
/// chars.
fn style_tickets(
    spans: Vec<(String, Style)>,
    ctx: &TicketCtx<'_>,
) -> (Vec<(String, Style)>, Vec<(usize, String)>) {
    let mut out: Vec<(String, Style)> = Vec::new();
    let mut marks: Vec<(usize, String)> = Vec::new();
    let mut chars = 0usize;
    for (text, style) in spans {
        let mut last = 0usize;
        for range in find_tickets(&text, ctx.cfg) {
            let head = &text[last..range.start];
            if !head.is_empty() {
                chars += head.chars().count();
                out.push((head.to_string(), style));
            }
            let key = text[range.clone()].to_string();
            let ordinal = ctx.hits.len() + marks.len();
            let mut st = style.add_modifier(Modifier::UNDERLINED);
            if ctx.cursor == Some(ordinal) {
                st = st.add_modifier(Modifier::REVERSED);
            }
            marks.push((chars, key.clone()));
            chars += key.chars().count();
            out.push((key, st));
            last = range.end;
        }
        let tail = &text[last..];
        if !tail.is_empty() {
            chars += tail.chars().count();
            out.push((tail.to_string(), style));
        }
    }
    (out, marks)
}
```

(c) Make `wrap_into` a wrapper over a mark-tracking form. Replace the existing `wrap_into` signature/body with:

```rust
fn wrap_into(out: &mut Vec<Line<'static>>, spans: Vec<(String, Style)>, width: usize, hang: usize) {
    wrap_into_marked(out, spans, width, hang, &[]);
}

/// `wrap_into`, plus for each char offset in `marks` the index — RELATIVE to
/// the first row this call pushes — of the row that char landed on. A ticket
/// key can be split by the wrap; the mark is its first char, so the hit points
/// at the row the key starts on.
fn wrap_into_marked(
    out: &mut Vec<Line<'static>>,
    spans: Vec<(String, Style)>,
    width: usize,
    hang: usize,
    marks: &[usize],
) -> Vec<usize> {
    let chars: Vec<(char, Style, usize)> = spans
        .iter()
        .flat_map(|(t, s)| t.chars().map(|c| (c, *s, c.width().unwrap_or(0))).collect::<Vec<_>>())
        .collect();
    // Char range per pushed row, so a mark can be mapped back afterwards.
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut start = 0;
    let mut first = true;
    loop {
        let budget = if first { width } else { width.saturating_sub(hang).max(4) };
        let mut end = start;
        let mut cols = 0;
        while end < chars.len() && cols + chars[end].2 <= budget {
            cols += chars[end].2;
            end += 1;
        }
        if end >= chars.len() {
            out.push(to_line(&chars[start..], if first { 0 } else { hang }));
            ranges.push((start, chars.len()));
            break;
        }
        if let Some(pos) = chars[start..end].iter().rposition(|(c, _, _)| *c == ' ').filter(|&p| p > 0)
        {
            end = start + pos;
        }
        // A single char wider than the whole budget still makes progress.
        if end == start {
            end = start + 1;
        }
        out.push(to_line(&chars[start..end], if first { 0 } else { hang }));
        ranges.push((start, end));
        start = end;
        while start < chars.len() && chars[start].0 == ' ' {
            start += 1;
        }
        first = false;
        if start >= chars.len() {
            break;
        }
    }
    marks
        .iter()
        .map(|m| ranges.iter().position(|(_, e)| m < e).unwrap_or(ranges.len().saturating_sub(1)))
        .collect()
}
```

Note the behavioural equivalence to the old loop: both `return` sites became `break`, and the trailing-space skip is unchanged.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib`
Expected: PASS, including every pre-existing `markdown` and `app` test (the wrappers keep old behaviour — an empty config takes the `wrap_into` fast path).
Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/markdown.rs
git commit -m "feat(markdown): underline configured ticket keys, return their rows"
```

---

### Task 4: app state and `n`/`N` navigation

**Files:**
- Modify: `src/app.rs` (struct fields ~394-435, `with_note` ~453-481, `App::new` ~438-451, `toggle_global` ~535-560, confirm-clear arm ~849-861, `on_key_preview` ~871-901, cursor helpers ~1106-1160)
- Test: inline test module in `src/app.rs`

**Interfaces:**
- Consumes: `markdown::TicketHit`, `markdown::render_markdown_tickets` (Task 3), `tickets::Config` (Task 1).
- Produces (private to `App`, used by Task 5 and Task 6): fields `tickets: tickets::Config`, `ticket_hits: Vec<markdown::TicketHit>`, `ticket_cursor: Option<usize>`, `follow_ticket: bool`; methods `fn move_ticket(&mut self, delta: isize)`, `fn clear_ticket_cursor(&mut self)`, `fn clear_cursors(&mut self)`, `fn clamp_ticket_cursor(&mut self)`.

- [ ] **Step 1: Write the failing tests**

Add to `app.rs`'s test module. `app(text)` and `rendered(app, w, h)` already exist there; `rendered` is what populates `ticket_hits`, mirroring the running loop, which always draws before reading a key.

```rust
    fn ticket_app(text: &str) -> App {
        let mut a = app(text);
        a.tickets = crate::tickets::Config::from_json(
            r#"{"HM":"https://example.test/browse/{key}"}"#,
        );
        a
    }

    #[test]
    fn n_and_N_walk_the_ticket_cursor_and_clamp() {
        let mut a = ticket_app("first HM-1\nsecond HM-2\n");
        rendered(&mut a, 40, 10);
        assert_eq!(a.ticket_cursor, None, "no cursor until you ask for one");
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.ticket_cursor, Some(0));
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.ticket_cursor, Some(1));
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.ticket_cursor, Some(1), "clamps at the last ticket");
        a.on_key(key(KeyCode::Char('N')));
        assert_eq!(a.ticket_cursor, Some(0));
        a.on_key(key(KeyCode::Char('N')));
        assert_eq!(a.ticket_cursor, Some(0), "clamps at the first ticket");
    }

    #[test]
    fn n_does_nothing_without_configured_tickets() {
        let mut a = app("HM-1 here"); // no config injected
        rendered(&mut a, 40, 10);
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.ticket_cursor, None);
        assert!(a.ticket_hits.is_empty());
    }

    #[test]
    fn the_two_cursors_are_mutually_exclusive() {
        let mut a = ticket_app("[ ] task HM-1\n[ ] other\n");
        rendered(&mut a, 40, 10);
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.box_cursor, Some(0));
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.ticket_cursor, Some(0));
        assert_eq!(a.box_cursor, None, "n drops the checkbox cursor");
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.box_cursor, Some(0));
        assert_eq!(a.ticket_cursor, None, "j drops the ticket cursor");
    }

    #[test]
    fn esc_drops_both_cursors() {
        let mut a = ticket_app("[ ] task HM-1\n");
        rendered(&mut a, 40, 10);
        a.on_key(key(KeyCode::Char('n')));
        a.on_key(key(KeyCode::Esc));
        assert_eq!(a.ticket_cursor, None);
        assert!(!a.follow_ticket, "and the pending scroll-follow with it");
        assert_eq!(a.box_cursor, None);
    }

    #[test]
    fn clearing_the_note_drops_the_ticket_cursor() {
        // A stale ordinal is harmless only while there is no text under it.
        let mut a = ticket_app("HM-1\n");
        rendered(&mut a, 40, 10);
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.ticket_cursor, Some(0));
        a.on_key(key(KeyCode::Char('x')));
        a.on_key(key(KeyCode::Char('y')));
        assert_eq!(a.ticket_cursor, None);
    }

    #[test]
    fn an_edit_that_removes_a_ticket_reclamps_the_cursor() {
        let mut a = ticket_app("HM-1 and HM-2\n");
        rendered(&mut a, 40, 10);
        a.on_key(key(KeyCode::Char('n')));
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(a.ticket_cursor, Some(1));
        a.note.text = "HM-1 only\n".to_string();
        rendered(&mut a, 40, 10);
        assert_eq!(a.ticket_cursor, Some(0), "clamped to the surviving ticket");
        a.note.text = "nothing here\n".to_string();
        rendered(&mut a, 40, 10);
        assert_eq!(a.ticket_cursor, None);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib app 2>&1 | head -20`
Expected: FAIL — `no field ticket_cursor on type App`.

- [ ] **Step 3: Write the implementation**

Add the fields to `struct App`, after `follow_box`:

```rust
    /// Prefix→URL templates for issue keys, loaded ONCE (see `App::new`).
    /// Default-empty in `with_note`, so unit tests never read the store dir.
    tickets: crate::tickets::Config,
    /// Ticket keys found by the last preview draw, in document order. The draw
    /// is the single scan that both styles the keys and lists them, so nav and
    /// highlight cannot disagree. The loop always draws before reading a key,
    /// so this is never consulted stale.
    ticket_hits: Vec<markdown::TicketHit>,
    /// Ordinal into `ticket_hits` — which key `o` would open. Mutually
    /// exclusive with `box_cursor`: one cursor at a time keeps `esc` and
    /// `space` unambiguous.
    ticket_cursor: Option<usize>,
    /// One-shot scroll-follow, same contract as `follow_box`.
    follow_ticket: bool,
```

Initialise them in `with_note`'s struct literal:

```rust
            tickets: crate::tickets::Config::default(),
            ticket_hits: Vec::new(),
            ticket_cursor: None,
            follow_ticket: false,
```

Load the real config in `App::new`, beside `refresh_prompts` (same reason: keep disk access out of `with_note` so tests stay hermetic):

```rust
        app.tickets = crate::tickets::Config::load();
```

Add the helpers next to `clear_box_cursor`:

```rust
    /// Re-clamps the ticket ordinal against the hits the last draw found, and
    /// drops it when there are none. Called from the draw, since the hit list
    /// is a draw product and an edit can delete a key.
    fn clamp_ticket_cursor(&mut self) {
        let n = self.ticket_hits.len();
        self.ticket_cursor = match self.ticket_cursor {
            Some(c) if n > 0 => Some(c.min(n - 1)),
            _ => None,
        };
    }

    fn clear_ticket_cursor(&mut self) {
        self.ticket_cursor = None;
        self.follow_ticket = false;
    }

    /// Drops BOTH preview cursors. Every path that swaps or wipes the buffer
    /// calls this rather than either single clear, so a cursor added later
    /// cannot be missed by a document swap — the recurring bug class in this
    /// crate (see the `toggle_global` / `global.json` gotchas).
    fn clear_cursors(&mut self) {
        self.clear_box_cursor();
        self.clear_ticket_cursor();
    }

    /// Steps the ticket cursor over the last draw's hits. From no cursor, `n`
    /// lands on the first key and `N` on the last. Clamps at both ends; does
    /// nothing when the note has no configured keys.
    fn move_ticket(&mut self, delta: isize) {
        self.clamp_ticket_cursor();
        let n = self.ticket_hits.len();
        if n == 0 {
            return; // clamp already dropped the cursor
        }
        self.clear_box_cursor(); // one cursor at a time
        self.ticket_cursor = Some(match self.ticket_cursor {
            None if delta > 0 => 0,
            None => n - 1,
            Some(c) => c.saturating_add_signed(delta).min(n - 1),
        });
        self.follow_ticket = true;
    }
```

Wire the keys in `on_key_preview`:

```rust
            KeyCode::Char('j') => {
                self.clear_ticket_cursor();
                self.move_box(1)
            }
            KeyCode::Char('k') => {
                self.clear_ticket_cursor();
                self.move_box(-1)
            }
            KeyCode::Char(' ') => {
                self.clear_ticket_cursor();
                self.toggle_box()
            }
            KeyCode::Char('n') => self.move_ticket(1),
            KeyCode::Char('N') => self.move_ticket(-1),
```

and change the `Esc` arm to `KeyCode::Esc => self.clear_cursors(),`.

Replace the `clear_box_cursor()` call in the confirm-clear arm (~line 856) and in `toggle_global` (~line 553) with `clear_cursors()`. In `toggle_global`, also clear the stale hit list so nothing can be opened out of the note you just left:

```rust
        self.preview_scroll = 0;
        self.clear_cursors();
        self.ticket_hits.clear();
```

Also update `toggle_global`'s "Everything per-DOCUMENT resets" comment to name the ticket cursor and hit list.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib`
Expected: PASS. Note `an_edit_that_removes_a_ticket_reclamps_the_cursor` and `clearing_the_note_drops_the_ticket_cursor` also need Task 5's draw wiring — if they fail on the clamp, complete Task 5 and re-run before committing. (`ticket_hits` is only refreshed by the draw.)

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "feat(notes): ticket cursor state and n/N navigation"
```

---

### Task 5: draw wiring and the footer hint

**Files:**
- Modify: `src/app.rs` (`draw_preview` ~1423-1512, footer consts ~1391-1405)
- Test: inline test module in `src/app.rs`

**Interfaces:**
- Consumes: Task 3's `render_markdown_tickets`/`TicketHit`, Task 4's fields and `clamp_ticket_cursor`.
- Produces: `self.ticket_hits` refreshed on every preview draw with rows already offset past the prompt block; `follow_ticket` honoured then cleared.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn the_prompt_block_offsets_ticket_rows() {
        // Hit rows index the FINAL line list, so they must be shifted past the
        // prompt block the same way `map` is.
        let mut a = ticket_app("HM-1 here\n");
        rendered(&mut a, 40, 20);
        let without = a.ticket_hits[0].row;
        a.prompts = vec![crate::prompts::PromptGroup {
            pane: "w1:p5".into(),
            prompts: vec![prompt(1, "look at HM-1")],
        }];
        a.prompt_labels = vec!["claude p5".into()];
        rendered(&mut a, 40, 20);
        assert_eq!(a.ticket_hits.len(), 1, "the block is not scanned for tickets");
        assert!(a.ticket_hits[0].row > without, "row shifted past the block");
    }

    #[test]
    fn an_empty_note_has_no_hits() {
        let mut a = ticket_app("");
        rendered(&mut a, 40, 10);
        assert!(a.ticket_hits.is_empty());
        assert_eq!(a.ticket_cursor, None);
    }

    #[test]
    fn the_ticket_cursor_scrolls_itself_into_view_once() {
        let mut a = ticket_app(&format!("{}HM-1 at the bottom\n", "filler\n".repeat(40)));
        rendered(&mut a, 40, 10);
        assert_eq!(a.preview_scroll, 0);
        a.on_key(key(KeyCode::Char('n')));
        rendered(&mut a, 40, 10);
        assert!(a.preview_scroll > 0, "scrolled to the only ticket");
        assert!(!a.follow_ticket, "one-shot: cleared after the draw");
        let settled = a.preview_scroll;
        a.on_key(key(KeyCode::Char('g')));
        rendered(&mut a, 40, 10);
        assert_eq!(a.preview_scroll, 0, "manual scrolling is not fought");
        assert!(settled > 0);
    }

    #[test]
    fn the_footer_advertises_the_ticket_keys_while_the_cursor_is_live() {
        let mut a = ticket_app("HM-1\n");
        rendered(&mut a, 90, 10);
        a.on_key(key(KeyCode::Char('n')));
        let screen = rendered(&mut a, 90, 10);
        assert!(screen.contains("o open"), "{screen}");
        assert!(screen.contains("esc drop"));
    }

    #[test]
    fn every_short_footer_form_keeps_the_quit_hint() {
        let mut a = ticket_app("HM-1\n");
        rendered(&mut a, 40, 10);
        a.on_key(key(KeyCode::Char('n')));
        let screen = rendered(&mut a, 40, 10);
        assert!(screen.contains("q quit"), "{screen}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib the_prompt_block_offsets_ticket_rows the_footer_advertises 2>&1 | head -30`
Expected: FAIL — hits empty / `o open` absent from the screen.

- [ ] **Step 3: Write the implementation**

In `draw_preview`, the empty-note branch clears the hit list and the real-note branch collects it. Replace the `(mut lines, map)` construction with:

```rust
        let (mut lines, map): (Vec<Line<'static>>, Vec<Option<usize>>) =
            if self.note.text.trim().is_empty() {
                // No text, so no keys — and no stale hits left behind for `o`.
                self.ticket_hits.clear();
                let mut lines = block;
                lines.extend(empty_help().lines().map(|l| {
                    Line::from(Span::styled(l.to_string(), Style::default().add_modifier(Modifier::DIM)))
                }));
                let map = vec![None; lines.len()];
                (lines, map)
            } else {
                let (mut lines, mut map, mut hits) = markdown::render_markdown_tickets(
                    &self.note.text,
                    text_w,
                    &self.tickets,
                    self.ticket_cursor,
                );
                // The block's rows map to NO source line, so the checkbox cursor can
                // never land on one and the highlight/scroll-follow keep pointing at
                // real note lines. Edit mode never reaches here.
                if !block.is_empty() {
                    let n = block.len();
                    let mut merged = block;
                    merged.append(&mut lines);
                    lines = merged;
                    let mut merged_map = vec![None; n];
                    merged_map.append(&mut map);
                    map = merged_map;
                    // Hit rows index the FINAL list, so they shift with it.
                    for hit in &mut hits {
                        hit.row += n;
                    }
                }
                self.ticket_hits = hits;
                (lines, map)
            };
        // The hit list is a draw product; an edit may have deleted the key the
        // ordinal pointed at.
        self.clamp_ticket_cursor();
```

Then, after the existing `if let Some(src) = self.cursor_line() { ... }` block and before the `clamp_scroll` line, add the ticket scroll-follow (the highlight itself already happened inside the render, via the `cursor` argument):

```rust
        // Same one-shot contract as `follow_box`: only right after `n`/`N`
        // moved the cursor, never merely because a cursor exists — otherwise
        // every other scroll key looks broken while a ticket is selected.
        if self.follow_ticket {
            if let Some(row) = self.ticket_cursor.and_then(|c| self.ticket_hits.get(c)).map(|h| h.row)
            {
                let h = usize::from(area.height).max(1);
                if row < self.preview_scroll {
                    self.preview_scroll = row;
                } else if row >= self.preview_scroll + h {
                    self.preview_scroll = row + 1 - h;
                }
            }
            self.follow_ticket = false;
        }
```

Footer: add a third pair and extend the selection. Replace the hint block with:

```rust
        const PREVIEW_HINTS: &str =
            " e edit  j/k spc tick  n ticket  r title  l list  Up/Dn scroll  x clear  q quit";
        const PREVIEW_HINTS_SHORT: &str = " e edit  j/k spc tick  l list  q quit";
        const PREVIEW_HINTS_CURSOR: &str =
            " e edit  j/k spc tick  esc drop  r title  l list  Up/Dn scroll  x clear  q quit";
        const PREVIEW_HINTS_CURSOR_SHORT: &str = " e edit  j/k spc tick  esc drop  q quit";
        // While a ticket is selected, `o` is the whole point and `esc` is the
        // only way out — both outrank `l list` and the scroll keys.
        const PREVIEW_HINTS_TICKET: &str =
            " e edit  n/N ticket  o open  esc drop  r title  l list  Up/Dn scroll  q quit";
        const PREVIEW_HINTS_TICKET_SHORT: &str = " n/N ticket  o open  esc drop  q quit";
        let hints = match self.note.mode {
            Mode::Preview => {
                let (full, short) = if self.ticket_cursor.is_some() {
                    (PREVIEW_HINTS_TICKET, PREVIEW_HINTS_TICKET_SHORT)
                } else if self.box_cursor.is_some() {
                    (PREVIEW_HINTS_CURSOR, PREVIEW_HINTS_CURSOR_SHORT)
                } else {
                    (PREVIEW_HINTS, PREVIEW_HINTS_SHORT)
                };
                if usize::from(hint_a.width) >= full.chars().count() { full } else { short }
            }
            Mode::Edit => " Esc preview (saves)   Ctrl+S save",
        };
```

Extend the comment above those consts to record the new form and that the ticket short form drops `e edit` to keep `o open` and `esc drop` (the two keys that matter while a ticket is selected) inside the 37-column floor.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib`
Expected: PASS, including Task 4's two clamp tests.
Run: `cargo clippy --all-targets -- -D warnings`
Expected: clean.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "feat(notes): wire ticket hits through the preview draw and footer"
```

---

### Task 6: `o` opens the browser, plus docs and live verification

**Files:**
- Modify: `src/tickets.rs` (add `open`), `src/app.rs` (`open_children` field, `o` key, heartbeat reaping), `README.md`, `CLAUDE.md`
- Test: inline test modules in `src/tickets.rs` and `src/app.rs`

**Interfaces:**
- Consumes: `tickets::ticket_url` (Task 1), `App.ticket_hits`/`ticket_cursor` (Task 4).
- Produces: `pub fn open(url: &str) -> Option<std::process::Child>`; `App.open_children: Vec<std::process::Child>`.

- [ ] **Step 1: Write the failing tests**

In `src/app.rs`'s test module:

```rust
    #[test]
    fn o_without_a_cursor_or_config_does_nothing() {
        // No panic, no child, no output — the whole failure contract.
        let mut a = ticket_app("HM-1\n");
        rendered(&mut a, 40, 10);
        a.on_key(key(KeyCode::Char('o')));
        assert!(a.open_children.is_empty(), "no cursor, nothing to open");

        let mut b = app("HM-1\n"); // no config
        rendered(&mut b, 40, 10);
        b.on_key(key(KeyCode::Char('n')));
        b.on_key(key(KeyCode::Char('o')));
        assert!(b.open_children.is_empty());
    }

    #[test]
    fn o_resolves_the_cursored_key_to_a_url() {
        // The URL, not the spawn, is the tested part: `pending_open` returns
        // what `o` would hand to the browser.
        let mut a = ticket_app("first HM-1\nsecond HM-2\n");
        rendered(&mut a, 40, 10);
        a.on_key(key(KeyCode::Char('n')));
        a.on_key(key(KeyCode::Char('n')));
        assert_eq!(
            a.pending_open().as_deref(),
            Some("https://example.test/browse/HM-2")
        );
    }
```

In `src/tickets.rs`'s test module:

```rust
    #[test]
    fn the_launch_command_matches_the_platform() {
        let cmd = launch_command("https://example.test/browse/HM-1");
        let program = cmd.get_program().to_string_lossy().to_string();
        let args: Vec<String> =
            cmd.get_args().map(|a| a.to_string_lossy().to_string()).collect();
        if cfg!(windows) {
            assert_eq!(program, "rundll32.exe");
            assert_eq!(args[0], "url.dll,FileProtocolHandler");
        } else if cfg!(target_os = "macos") {
            assert_eq!(program, "open");
        } else {
            assert_eq!(program, "xdg-open");
        }
        // The URL is a single argv entry: no shell, so nothing in it is
        // interpreted (an `&` in a query string, for one).
        assert_eq!(args.last().unwrap(), "https://example.test/browse/HM-1");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib 2>&1 | head -20`
Expected: FAIL — `cannot find function launch_command`, `no field open_children`, `no method pending_open`.

- [ ] **Step 3: Write the implementation**

In `src/tickets.rs`:

```rust
/// The platform's URL handler, as a `Command` so it can be asserted without
/// launching anything. `rundll32` rather than `cmd /c start` on Windows: `cmd`
/// flashes a console over the TUI and its quoting mangles URLs containing `&`.
fn launch_command(url: &str) -> std::process::Command {
    let mut cmd = if cfg!(windows) {
        let mut c = std::process::Command::new("rundll32.exe");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else {
        std::process::Command::new("xdg-open")
    };
    cmd.arg(url);
    cmd
}

/// Hands `url` to the platform browser. `spawn`, never `output`: a blocking
/// wait here sits on the event-loop thread and would freeze input, drawing AND
/// the 5s identity re-stamp — past 20s the launcher calls this live pane a
/// corpse and REPLACEs it, and `pane close` kills with no signal, taking the
/// dirty debounce buffer. Returns the child so the caller can reap it (unix
/// would otherwise leave a zombie per open); `None` on any failure, silently.
pub fn open(url: &str) -> Option<std::process::Child> {
    let mut cmd = launch_command(url);
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd.spawn().ok()
}
```

In `src/app.rs`, add the field after `follow_ticket`:

```rust
    /// Browser launches still running, reaped on the heartbeat so unix does not
    /// accumulate a zombie per `o`.
    open_children: Vec<std::process::Child>,
```

with `open_children: Vec::new(),` in `with_note`'s literal, and the two methods next to `move_ticket`:

```rust
    /// The URL `o` would open right now, or `None`. Separate from `open_ticket`
    /// so the resolution is testable without launching a browser.
    fn pending_open(&self) -> Option<String> {
        let key = self.ticket_cursor.and_then(|c| self.ticket_hits.get(c)).map(|h| h.key.clone())?;
        crate::tickets::ticket_url(&self.tickets, &key)
    }

    /// Opens the cursored ticket. Silent no-op when there is no cursor, no
    /// mapping, or the spawn fails — nothing may print from the TUI.
    fn open_ticket(&mut self) {
        let Some(url) = self.pending_open() else { return };
        if let Some(child) = crate::tickets::open(&url) {
            self.open_children.push(child);
        }
    }
```

Add the key to `on_key_preview`: `KeyCode::Char('o') => self.open_ticket(),`

And reap in `heartbeat`, right after `self.last_beat = Instant::now();`:

```rust
        // Non-blocking: a browser still running just stays in the list.
        self.open_children.retain_mut(|c| !matches!(c.try_wait(), Ok(Some(_)) | Err(_)));
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test`
Expected: PASS (whole suite).
Run: `cargo clippy --all-targets -- -D warnings` and `cargo build --release`
Expected: clean. (Close any open Notes pane first — `cargo build --release` fails with os error 5 while the TUI is running, and `Get-Process herdr-notes | Stop-Process` clears stragglers.)

- [ ] **Step 5: Document it**

In `README.md`, add a short section under the preview-keys documentation:

```markdown
### Ticket links

Issue keys in the note (`HM-54561`) are underlined once their prefix is
configured. `n`/`N` walk them, `o` opens the selected one in your browser,
`esc` drops the cursor.

Create `tickets.json` next to the note files — `%LOCALAPPDATA%\herdr\plugins\herdr-notes\`
on Windows, `~/.local/share/herdr/plugins/herdr-notes/` on unix:

```json
{
  "HM": "https://your-org.atlassian.net/browse/{key}",
  "CR": "https://your-tracker.example/issue/{key}"
}
```

Only listed prefixes are detected, so an unmapped key is never highlighted and
never pretends to be openable. The file is read once at startup: after editing
it, close and reopen the Notes pane. A missing or malformed file simply turns
the feature off.
```

In `CLAUDE.md`, add `src/tickets.rs` to the Layout list ("prefix→URL config
(`tickets.json` in the note store dir, loaded once at construction, forgiving
parse, `{key}`-less templates dropped), `ticket_url`, and the `spawn`-only
browser launch"), extend the `src/markdown.rs` entry to name `find_tickets` /
`render_markdown_tickets` / `TicketHit` as the crate's only ticket scan, extend
the `src/app.rs` entry with the ticket cursor and `clear_cursors`, and add these
Gotchas:

```markdown
- Ticket detection runs DURING render (`render_markdown_tickets`) and the hit
  list is a draw product cached in `App.ticket_hits`, not a source scan. A
  second scan over the raw note text would see `HM-**54561**` where the render
  sees `HM-54561`; the counts diverge and the cursor ordinal slips onto the
  wrong key. Same single-parser rule as `markdown::checkbox_lines`.
- Hit rows index the FINAL preview line list, so `draw_preview` shifts them by
  `block.len()` exactly as it shifts `map`. Forgetting that scrolls the
  ticket cursor to a row in the prompt block.
- A ratatui widget cannot emit OSC 8 hyperlinks and herdr owns pane mouse
  events, so a mouse-clickable link is not reachable from this plugin at all.
  `n`/`N`/`o` is not a stylistic choice, it is the only mechanism available.
- `tickets.json` is read ONCE at construction (`App::new`, beside
  `refresh_prompts`), never on the heartbeat: it changes ~never and the
  heartbeat already does socket work. Editing it needs a pane restart.
- `tickets::open` must `spawn`, never `output` — the `git_branch` freeze chain
  applies verbatim (blocked event loop, no identity re-stamp, launcher REPLACEs
  the pane, `pane close` takes the dirty buffer with it). The returned `Child`
  is reaped on the heartbeat or unix leaves a zombie per `o`.
```

- [ ] **Step 6: Verify live in a throwaway pane**

Drive the real binary rather than trusting unit tests, per the repo's
end-to-end recipe. A plain `pane split` pane has `HERDR_ENV=1` but no
`HERDR_PLUGIN_STATE_DIR`, so it resolves `%LOCALAPPDATA%\herdr\plugins\herdr-notes\`
— write the throwaway config there and use a `TT` prefix so a real tracker is
never contacted:

```powershell
$dir = "$env:LOCALAPPDATA\herdr\plugins\herdr-notes"
$cfg = Join-Path $dir 'tickets.json'
if (Test-Path $cfg) { Copy-Item $cfg "$cfg.bak" }   # restore this afterwards
$json = '{"TT":"https://example.com/{key}"}'
[System.IO.File]::WriteAllText($cfg, $json, (New-Object System.Text.UTF8Encoding($false)))
```

Then: `herdr pane split`, `herdr pane run <id> "<abs path>\target\release\herdr-notes.exe; exit"`,
`herdr pane send-keys <id> e`, type `check TT-42 and TT-43`, `Escape`, then
`send-keys <id> n` and `herdr pane read <id> --source visible` — the first key
should be highlighted. `send-keys <id> n` again, then `o`, and confirm the
browser opens `https://example.com/TT-43`. Finish with `send-keys <id> Escape q`,
`herdr pane close <id>`, delete the throwaway note file, and restore
`tickets.json.bak` (or delete the file if there was no backup).

- [ ] **Step 7: Commit**

```bash
git add src/tickets.rs src/app.rs README.md CLAUDE.md
git commit -m "feat(notes): o opens the cursored ticket in the browser"
```

---

## Notes for the implementer

- `PREVIEW_HINTS_TICKET_SHORT` drops `e edit` deliberately. Measure the forms
  (`.chars().count()`) if you change them — the selection compares the FULL
  form against the pane width and falls back to the short one, so the short
  form's own length is the floor below which `q quit` starts to clip.
- The plan touches `draw_preview`'s comment block. Keep the existing prose
  about why the empty-note branch shares the clamp/scroll tail; it documents a
  real bug that was fixed.
- Every silent-failure path is deliberate. Do not add an error message, a
  status line, or a `dbg!` — the TUI printing anything corrupts its own screen,
  and the same module is reachable from the `--capture-prompt` hook path where
  stdout output is injected into the user's prompt.
