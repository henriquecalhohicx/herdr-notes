//! Tiny hand-rolled markdown renderer for the preview: headings, bullets,
//! numbered lists, checkboxes, blockquotes, inline and fenced code, bold and
//! italic markers, horizontal rules. Unknown constructs render as plain text.
//! Long lines wrap to the pane width with a hanging indent for list items.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

const ACCENT: Color = Color::Cyan;
const CODE: Color = Color::Yellow;
const CHECK: Color = Color::Green;

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

pub fn render_markdown(text: &str, width: usize) -> Vec<Line<'static>> {
    render_markdown_mapped(text, width).0
}

/// `render_markdown` plus, for each rendered row, the `str::lines()` index of
/// the source line that produced it. One source line can wrap to several rows,
/// which all carry the same index; the synthetic blank row emitted for empty
/// input carries `None`. Lets the preview map a screen row back to the note
/// text — needed by the checkbox cursor.
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

fn render_inner(
    text: &str,
    width: usize,
    cfg: &crate::tickets::Config,
    cursor: Option<usize>,
    enabled: bool,
) -> (Vec<Line<'static>>, Vec<Option<usize>>, Vec<LinkHit>) {
    let width = width.max(8);
    let mut out = Vec::new();
    let mut map: Vec<Option<usize>> = Vec::new();
    let mut ctx = LinkCtx { cfg, cursor, hits: Vec::new(), enabled };
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
        // `out` only ever grows, so this fills exactly the rows this source
        // line just added.
        map.resize(out.len(), Some(src));
    }
    if out.is_empty() {
        out.push(Line::raw(""));
        map.push(None);
    }
    (out, map, ctx.hits)
}

fn render_line(out: &mut Vec<Line<'static>>, ctx: &mut LinkCtx<'_>, line: &str, width: usize) {
    let trimmed = line.trim_start();
    if trimmed.is_empty() {
        out.push(Line::raw(""));
        return;
    }
    let indent = line.chars().count() - trimmed.chars().count();
    let pad = " ".repeat(indent);

    // Headings: level distinguishable by weight/underline.
    let hashes = trimmed.chars().take_while(|c| *c == '#').count();
    if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
        let style = match hashes {
            1 => Style::default().fg(ACCENT).add_modifier(Modifier::BOLD | Modifier::UNDERLINED),
            2 => Style::default().fg(ACCENT).add_modifier(Modifier::BOLD),
            _ => Style::default().fg(ACCENT),
        };
        emit(out, ctx, parse_inline(trimmed[hashes..].trim_start(), style), width, 0);
        return;
    }

    if is_hr(trimmed) {
        out.push(Line::from(Span::styled(
            "─".repeat(width),
            Style::default().add_modifier(Modifier::DIM),
        )));
        return;
    }

    if let Some(rest) = trimmed.strip_prefix('>') {
        let dim = Style::default().add_modifier(Modifier::DIM);
        let mut spans = vec![("▎ ".to_string(), dim)];
        spans.extend(parse_inline(rest.trim_start(), dim));
        emit(out, ctx, spans, width, 2);
        return;
    }

    if let Some((done, rest)) = checkbox(trimmed) {
        let (glyph, style) = if done {
            ("[x] ", Style::default().fg(CHECK))
        } else {
            ("[ ] ", Style::default())
        };
        let mut spans = vec![(format!("{pad}{glyph}"), style)];
        spans.extend(parse_inline(rest, Style::default()));
        emit(out, ctx, spans, width, indent + 4);
        return;
    }

    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(bullet) {
            let mut spans = vec![(format!("{pad}• "), Style::default().fg(ACCENT))];
            spans.extend(parse_inline(rest, Style::default()));
            emit(out, ctx, spans, width, indent + 2);
            return;
        }
    }

    let digits = trimmed.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits > 0 {
        let rest = &trimmed[digits..];
        if let Some(body) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            let num = &trimmed[..digits];
            let mut spans = vec![(format!("{pad}{num}. "), Style::default().fg(ACCENT))];
            spans.extend(parse_inline(body, Style::default()));
            emit(out, ctx, spans, width, indent + digits + 2);
            return;
        }
    }

    let mut spans = Vec::new();
    if indent > 0 {
        spans.push((pad, Style::default()));
    }
    spans.extend(parse_inline(trimmed, Style::default()));
    emit(out, ctx, spans, width, 0);
}

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

/// Byte ranges of every CONFIGURED issue key in `s`, left to right and
/// non-overlapping. A key is an uppercase ASCII run of 2+, a `-`, then 1+
/// ASCII digits, with a non-alphanumeric boundary on both sides — and its
/// prefix must be in `cfg`, so an unmapped tracker is never highlighted and
/// the ticket cursor can never land on something `o` cannot open.
///
/// The crate's ONE ticket scan. Anything that needs to know where the keys are
/// goes through here, for the same reason the checkbox parser is single-homed:
/// a second scan drifts.
fn find_ticket_ranges(s: &str, cfg: &crate::tickets::Config) -> Vec<std::ops::Range<usize>> {
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
/// case-insensitively, needs a non-alphanumeric boundary on its LEFT (a match
/// may start at offset 0, or the preceding byte must not be ASCII
/// alphanumeric — narrower than the ticket key boundary above, which also
/// rejects a preceding `_` via `keyish`; this check does not, so `foo_https://x`
/// IS a URL while `foo_HM-1` is NOT a key), the URL runs to the next
/// whitespace, and at least one non-whitespace character must follow `//` —
/// a bare scheme is not a link.
fn find_url_ranges(s: &str) -> Vec<std::ops::Range<usize>> {
    const SCHEMES: [&str; 2] = ["https://", "http://"];
    let bytes = s.as_bytes();
    let mut out: Vec<std::ops::Range<usize>> = Vec::new();
    let mut resume = 0usize;
    for (i, _) in s.char_indices() {
        if i < resume {
            continue;
        }
        if i > 0 && bytes[i - 1].is_ascii_alphanumeric() {
            continue; // glued to a preceding word: `xhttps://` is not a link
        }
        let rest = &s[i..];
        let Some(scheme) = SCHEMES
            .iter()
            .find(|sc| {
                rest.len() >= sc.len()
                    && s.is_char_boundary(i + sc.len())
                    && rest[..sc.len()].eq_ignore_ascii_case(sc)
            })
        else {
            continue;
        };
        let body = i + scheme.len();
        let end = s[body..].find(char::is_whitespace).map_or(s.len(), |off| body + off);
        let end = trim_url_end(s, i, body, end);
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
/// the bracket back to the prose while `https://x/a_(b)` keeps it. A
/// trailing `*`/`_` run trims ONLY when the SAME marker char sits immediately
/// before `match_start` (the byte the scheme match itself began at) —
/// `**https://x/y**` and `_https://x/y_` are glued emphasis wrapping the
/// whole URL and must lose the wrapper, but `.../keys/report_` or
/// `.../Page_` followed by nothing but whitespace/end-of-line, with no
/// marker glued in front, is real URL content and must keep it: trimming
/// unconditionally (the first cut of this fix) silently dropped that
/// trailing character from BOTH the hit text and the `o` target, the exact
/// failure class this whole fix exists to close. Remaining imprecision,
/// deliberately not chased further: this only compares CHARACTERS, not
/// counts or balance, so `*https://x/a**` (one leading star, two trailing)
/// still trims both trailing stars, and a URL wrapped by a DIFFERENT marker
/// on each side (`_https://x/a*`) trims neither, since front and back don't
/// match. Both are rare, already-malformed markdown shapes; getting them
/// exactly right would mean tracking how many markers preceded the URL, not
/// just whether one did.
fn trim_url_end(s: &str, match_start: usize, start: usize, mut end: usize) -> usize {
    let bytes = s.as_bytes();
    let marker_before = match_start.checked_sub(1).map(|p| bytes[p]);
    while end > start {
        // Always safe: end > start and we're indexing bytes
        let last = bytes[end - 1];
        let opener = match last {
            b'.' | b',' | b';' | b':' | b'!' | b'?' | b'\'' | b'"' => {
                end -= 1;
                continue;
            }
            b'*' | b'_' if marker_before == Some(last) => {
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

/// `---` / `***` / `___` (3+ of the same marker, spaces allowed between).
fn is_hr(t: &str) -> bool {
    let bare: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    bare.len() >= 3
        && ['-', '*', '_']
            .iter()
            .any(|m| bare.chars().all(|c| c == *m))
}

/// Per-char mask, index-aligned with `s.chars()`: true where that char falls
/// inside a bare URL `find_url_ranges` finds in `s`. `parse_inline` consults
/// this at BOTH ends of a candidate `*`/`_` pair: a URL's own
/// underscores/asterisks (wiki titles, Confluence, S3 keys) are not emphasis,
/// and if `parse_inline` paired one of them with a marker — as either the
/// open or the close — and split the URL across spans, `style_links` — which
/// scans per-span, AFTER this runs — would only ever see the fragment in
/// whichever span happened to hold the scheme, silently truncating the hit
/// text and the `o` target. Ticket keys can never contain these chars, so
/// this codepath is URL-only; nothing here needs `cfg`.
fn url_char_mask(s: &str) -> Vec<bool> {
    let ranges = find_url_ranges(s);
    s.char_indices().map(|(i, _)| ranges.iter().any(|r| r.contains(&i))).collect()
}

/// Inline spans: `` `code` ``, `**bold**`, `*italic*` / `_italic_`. Markers
/// without a closing partner (or with empty content) render literally; no
/// nesting — styled content is taken as-is. A `*`/`_` never opens OR closes a
/// marker while it sits inside a bare URL (see `url_char_mask`). Both ends
/// matter: guarding only the open still lets an UNRELATED earlier marker
/// (`foo_bar, see https://x/a_b` — the `_` in `foo_bar` is a perfectly legal
/// opener on its own) reach forward and pair with the URL's own char as its
/// close, swallowing the tail of the URL exactly as an unguarded open would.
/// `find_marker_close`/`find_double_star` SKIP a URL-internal candidate
/// rather than giving up the search there — a real closing marker may still
/// follow later on the line — so the marker opens normally against whatever
/// closer comes after the URL, and simply renders literally (no italic/bold)
/// when none does.
fn parse_inline(s: &str, base: Style) -> Vec<(String, Style)> {
    let chars: Vec<char> = s.chars().collect();
    let in_url = url_char_mask(s);
    let is_in_url = |i: usize| in_url.get(i).copied().unwrap_or(false);
    let mut out: Vec<(String, Style)> = Vec::new();
    let mut plain = String::new();
    let flush = |plain: &mut String, out: &mut Vec<(String, Style)>| {
        if !plain.is_empty() {
            out.push((std::mem::take(plain), base));
        }
    };
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c == '`' {
            if let Some(off) = chars[i + 1..].iter().position(|&d| d == '`').filter(|&o| o > 0) {
                let close = i + 1 + off;
                flush(&mut plain, &mut out);
                out.push((chars[i + 1..close].iter().collect(), base.fg(CODE)));
                i = close + 1;
                continue;
            }
        } else if c == '*' && chars.get(i + 1) == Some(&'*') && !is_in_url(i) {
            if let Some(close) = find_double_star(&chars, i + 2, &in_url).filter(|&p| p > i + 2) {
                flush(&mut plain, &mut out);
                out.push((
                    chars[i + 2..close].iter().collect(),
                    base.add_modifier(Modifier::BOLD),
                ));
                i = close + 2;
                continue;
            }
        } else if (c == '*' || c == '_')
            && !is_in_url(i)
            && let Some(close) = find_marker_close(&chars, i + 1, c, &in_url).filter(|&p| p > i + 1)
        {
            flush(&mut plain, &mut out);
            out.push((
                chars[i + 1..close].iter().collect(),
                base.add_modifier(Modifier::ITALIC),
            ));
            i = close + 1;
            continue;
        }
        plain.push(c);
        i += 1;
    }
    flush(&mut plain, &mut out);
    out
}

/// First index at or after `from` holding `target`, SKIPPING any index a URL
/// owns (`in_url`) rather than stopping the search there — see `parse_inline`
/// for why a URL-internal candidate must not be accepted as a close.
fn find_marker_close(chars: &[char], from: usize, target: char, in_url: &[bool]) -> Option<usize> {
    (from..chars.len()).find(|&j| chars[j] == target && !in_url.get(j).copied().unwrap_or(false))
}

/// Same skip-and-keep-scanning rule as `find_marker_close`, for the two-char
/// `**` close.
fn find_double_star(chars: &[char], from: usize, in_url: &[bool]) -> Option<usize> {
    (from..chars.len().saturating_sub(1))
        .find(|&j| chars[j] == '*' && chars[j + 1] == '*' && !in_url.get(j).copied().unwrap_or(false))
}

/// `wrap_into` with the link pass in front of it: matched targets get their
/// own styled span, and each one's rendered row is recorded as a hit.
/// Bypassed entirely when `ctx` is disabled, so the link-free entry points
/// (`render_markdown`/`render_markdown_mapped`) cost nothing extra.
fn emit(
    out: &mut Vec<Line<'static>>,
    ctx: &mut LinkCtx<'_>,
    spans: Vec<(String, Style)>,
    width: usize,
    hang: usize,
) {
    if !ctx.enabled {
        wrap_into(out, spans, width, hang);
        return;
    }
    let (spans, marks) = style_links(spans, ctx);
    let base = out.len();
    let offsets: Vec<usize> = marks.iter().map(|(off, _, _)| *off).collect();
    let rows = wrap_into_marked(out, spans, width, hang, &offsets);
    for ((_, text, kind), row) in marks.into_iter().zip(rows) {
        ctx.hits.push(LinkHit { text, kind, row: base + row });
    }
}

/// Per-mark record from `style_links`: char offset into the flattened span
/// sequence, the matched text, and its kind. Char offsets rather than byte
/// offsets because `wrap_into` works in chars.
type Marks = Vec<(usize, String, LinkKind)>;

/// Splits every link out of `spans` into its own span — underlined, plus
/// REVERSED when its ordinal is the cursored one — keeping whatever style the
/// surrounding text already had (bold, code, dim quote). Returns the rebuilt
/// spans and the marks found.
fn style_links(spans: Vec<(String, Style)>, ctx: &LinkCtx<'_>) -> (Vec<(String, Style)>, Marks) {
    let mut out: Vec<(String, Style)> = Vec::new();
    let mut marks: Marks = Vec::new();
    let mut chars = 0usize;
    for (text, style) in spans {
        let mut last = 0usize;
        for (range, kind) in find_links(&text, ctx.cfg) {
            let head = &text[last..range.start];
            if !head.is_empty() {
                chars += head.chars().count();
                out.push((head.to_string(), style));
            }
            let hit_text = text[range.clone()].to_string();
            let ordinal = ctx.hits.len() + marks.len();
            let mut st = style.add_modifier(Modifier::UNDERLINED);
            if ctx.cursor == Some(ordinal) {
                st = st.add_modifier(Modifier::REVERSED);
            }
            marks.push((chars, hit_text.clone(), kind));
            chars += hit_text.chars().count();
            out.push((hit_text, st));
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

/// Greedy wrap at `width` display COLUMNS (breaking at the last space when
/// possible), giving continuation lines `hang` columns of indent. Wide
/// (CJK/emoji) chars count 2 so wrapped lines never overflow the no-wrap
/// Paragraph and get clipped off the right edge.
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

/// Consecutive same-styled chars collapse back into spans.
fn to_line(chars: &[(char, Style, usize)], indent: usize) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    if indent > 0 {
        spans.push(Span::raw(" ".repeat(indent)));
    }
    let mut cur = String::new();
    let mut cur_style = Style::default();
    for &(c, s, _) in chars {
        if !cur.is_empty() && s != cur_style {
            spans.push(Span::styled(std::mem::take(&mut cur), cur_style));
        }
        if cur.is_empty() {
            cur_style = s;
        }
        cur.push(c);
    }
    if !cur.is_empty() {
        spans.push(Span::styled(cur, cur_style));
    }
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(line: &Line) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn texts(md: &str, width: usize) -> Vec<String> {
        render_markdown(md, width).iter().map(text).collect()
    }

    #[test]
    fn headings_are_styled_by_level() {
        let lines = render_markdown("# One\n## Two\n### Three", 40);
        assert_eq!(text(&lines[0]), "One");
        assert!(lines[0].spans[0].style.add_modifier.contains(Modifier::BOLD | Modifier::UNDERLINED));
        assert!(lines[1].spans[0].style.add_modifier.contains(Modifier::BOLD));
        assert!(!lines[1].spans[0].style.add_modifier.contains(Modifier::UNDERLINED));
        assert_eq!(lines[2].spans[0].style.fg, Some(ACCENT));
        // No space after the hashes = not a heading.
        assert_eq!(texts("#nope", 40), vec!["#nope"]);
    }

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

    #[test]
    fn bullets_numbers_and_checkboxes() {
        assert_eq!(texts("- item\n* star\n+ plus", 40), vec!["• item", "• star", "• plus"]);
        assert_eq!(texts("1. first\n12) twelfth", 40), vec!["1. first", "12. twelfth"]);
        let lines = render_markdown("- [ ] todo\n- [x] done", 40);
        assert_eq!(text(&lines[0]), "[ ] todo");
        assert_eq!(text(&lines[1]), "[x] done");
        assert_eq!(lines[1].spans[0].style.fg, Some(CHECK));
        // Indented list keeps its indent.
        assert_eq!(texts("  - nested", 40), vec!["  • nested"]);
    }

    #[test]
    fn code_fences_and_inline_code() {
        let lines = render_markdown("```\nlet x = 1;\n```", 40);
        assert_eq!(text(&lines[1]), "let x = 1;");
        assert_eq!(lines[1].spans[0].style.fg, Some(CODE));
        // Markdown inside a fence is NOT interpreted.
        assert_eq!(texts("```\n# not a heading\n```", 40)[1], "# not a heading");
        let inline = render_markdown("a `b` c", 40);
        let code_span = inline[0].spans.iter().find(|s| s.content == "b").unwrap();
        assert_eq!(code_span.style.fg, Some(CODE));
    }

    #[test]
    fn bold_italic_and_unclosed_markers() {
        let lines = render_markdown("**bold** and *it* and _us_", 60);
        let spans = &lines[0].spans;
        assert!(spans.iter().any(|s| s.content == "bold"
            && s.style.add_modifier.contains(Modifier::BOLD)));
        assert!(spans.iter().any(|s| s.content == "it"
            && s.style.add_modifier.contains(Modifier::ITALIC)));
        assert!(spans.iter().any(|s| s.content == "us"
            && s.style.add_modifier.contains(Modifier::ITALIC)));
        // Unclosed / empty markers render literally.
        assert_eq!(texts("*unclosed", 40), vec!["*unclosed"]);
        assert_eq!(texts("``", 40), vec!["``"]);
    }

    #[test]
    fn blockquote_and_hr() {
        let lines = render_markdown("> quoted", 40);
        assert_eq!(text(&lines[0]), "▎ quoted");
        assert!(lines[0].spans[0].style.add_modifier.contains(Modifier::DIM));
        assert_eq!(texts("---", 12), vec!["─".repeat(12)]);
        assert_eq!(texts("* * *", 10), vec!["─".repeat(10)]);
    }

    #[test]
    fn long_lines_wrap_with_hanging_indent() {
        let lines = texts("- alpha beta gamma delta", 12);
        assert!(lines.len() > 1, "should wrap: {lines:?}");
        assert!(lines.iter().all(|l| l.chars().count() <= 12), "{lines:?}");
        assert!(lines[1].starts_with("  "), "hanging indent: {lines:?}");
        // Plain paragraphs wrap at spaces.
        let wrapped = texts("one two three four five", 10);
        assert!(wrapped.len() >= 2);
        assert!(wrapped.iter().all(|l| l.chars().count() <= 10));
    }

    #[test]
    fn wide_chars_wrap_by_display_width_not_char_count() {
        // Six double-width chars = 12 columns; an 8-column pane fits 4 per
        // line. Char-count wrapping would emit a 12-column line that the
        // no-wrap Paragraph clips.
        assert_eq!(texts("你好世界你好", 8), vec!["你好世界", "你好"]);
    }

    #[test]
    fn blank_lines_and_empty_input_survive() {
        assert_eq!(texts("a\n\nb", 40), vec!["a", "", "b"]);
        assert_eq!(texts("", 40), vec![""]);
    }

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

    fn cfg() -> crate::tickets::Config {
        crate::tickets::Config::from_json(
            r#"{"HM":"https://example.test/{key}","CR":"https://example.test/c/{key}"}"#,
        )
    }

    fn keys(s: &str) -> Vec<String> {
        find_links(s, &cfg())
            .into_iter()
            .map(|(r, _)| s[r].to_string())
            .collect()
    }

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
    fn a_scheme_glued_to_a_word_is_not_a_url() {
        assert!(links("xhttps://example.test/a").is_empty());
        assert_eq!(links("(https://example.test/a")[0].0, "https://example.test/a");
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

    #[test]
    fn two_keys_jammed_together_with_no_separator_match_neither() {
        // "HM-1CR-2": the boundary check fails on BOTH candidates — the `C`
        // right after HM-1's digit run means HM-1 has no right boundary, and
        // the `1` right before CR-2's prefix means CR-2 has no left boundary.
        assert!(keys("HM-1CR-2").is_empty());
    }

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
            render_markdown_links("To estimate HM-54561 today", 40, &cfg(), None);
        assert_eq!(hits, vec![LinkHit { text: "HM-54561".into(), kind: LinkKind::Ticket, row: 0 }]);
        let st = hit_style(&lines, "HM-54561");
        assert!(st.add_modifier.contains(Modifier::UNDERLINED));
        assert!(!st.add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn the_cursored_hit_is_reversed_and_only_it() {
        let (lines, _, hits) =
            render_markdown_links("HM-1 and CR-2", 40, &cfg(), Some(1));
        assert_eq!(hits.len(), 2);
        assert!(!hit_style(&lines, "HM-1").add_modifier.contains(Modifier::REVERSED));
        assert!(hit_style(&lines, "CR-2").add_modifier.contains(Modifier::REVERSED));
    }

    #[test]
    fn hits_carry_the_row_they_landed_on() {
        let text = "# Head\n\n- first HM-1\n- second CR-2";
        let (_, _, hits) = render_markdown_links(text, 40, &cfg(), None);
        let rows: Vec<usize> = hits.iter().map(|h| h.row).collect();
        assert_eq!(hits.iter().map(|h| h.text.as_str()).collect::<Vec<_>>(), ["HM-1", "CR-2"]);
        assert!(rows[0] < rows[1], "rows ascend with document order: {rows:?}");
    }

    #[test]
    fn a_key_inside_bold_is_one_hit_and_stays_bold() {
        let (lines, _, hits) = render_markdown_links("**HM-9** done", 40, &cfg(), None);
        assert_eq!(hits.len(), 1, "markdown markers must not split the key");
        let st = hit_style(&lines, "HM-9");
        assert!(st.add_modifier.contains(Modifier::BOLD));
        assert!(st.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn fenced_code_is_not_linkified() {
        let text = "```\nHM-1\n```";
        let (_, _, hits) = render_markdown_links(text, 40, &cfg(), None);
        assert!(hits.is_empty(), "fenced code is code, like checkbox_lines treats it");
    }

    #[test]
    fn a_wrapped_key_keeps_its_style_on_every_row() {
        // Width 10 forces the key onto its own continuation row; the char-level
        // wrap must carry the underline across.
        let (lines, _, hits) =
            render_markdown_links("aaaa bbbb cccc HM-12345", 10, &cfg(), None);
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
        let (lines, _, hits) = render_markdown_links("see `HM-8` please", 40, &cfg(), None);
        assert_eq!(hits.len(), 1);
        assert!(hit_style(&lines, "HM-8").add_modifier.contains(Modifier::UNDERLINED));
    }

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

    #[test]
    fn a_url_containing_underscores_is_not_split_by_emphasis_parsing() {
        // parse_inline runs BEFORE style_links; without the fix, a matched
        // `_..._` pair inside the URL gets parsed as italic and splits the
        // URL across spans, so style_links (which scans per-span) only ever
        // sees the fragment holding the scheme — a silently truncated hit.
        let (lines, _, hits) =
            render_markdown_links("see https://example.test/a_b_c now", 40, &cfg(), None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "https://example.test/a_b_c");
        let underlined: usize = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter(|s| s.style.add_modifier.contains(Modifier::UNDERLINED))
            .map(|s| s.content.chars().count())
            .sum();
        assert_eq!(underlined, "https://example.test/a_b_c".chars().count());
    }

    #[test]
    fn a_url_containing_asterisks_is_not_split_by_emphasis_parsing() {
        let (lines, _, hits) =
            render_markdown_links("see https://example.test/a*b*c now", 40, &cfg(), None);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "https://example.test/a*b*c");
        let underlined: usize = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter(|s| s.style.add_modifier.contains(Modifier::UNDERLINED))
            .map(|s| s.content.chars().count())
            .sum();
        assert_eq!(underlined, "https://example.test/a*b*c".chars().count());
    }

    #[test]
    fn a_url_inside_bold_still_yields_one_whole_hit() {
        let (lines, _, hits) =
            render_markdown_links("**https://example.test/a_b_c**", 40, &cfg(), None);
        assert_eq!(hits.len(), 1, "markdown markers must not split the url");
        assert_eq!(hits[0].text, "https://example.test/a_b_c");
        let st = hit_style(&lines, "https://example.test/a_b_c");
        assert!(st.add_modifier.contains(Modifier::BOLD));
        assert!(st.add_modifier.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn emphasis_still_works_when_no_url_is_present() {
        // Regression guard for the fix above: plain markers away from any URL
        // must still open/close exactly as before (same assertions as
        // `bold_italic_and_unclosed_markers`, which already covers this
        // through `render_markdown` — kept as its own test so it fails on its
        // own if a future change to `url_char_mask`/`is_in_url` regresses it).
        let lines = render_markdown("**bold** and *it* and _us_", 60);
        let spans = &lines[0].spans;
        assert!(spans.iter().any(|s| s.content == "bold"
            && s.style.add_modifier.contains(Modifier::BOLD)));
        assert!(spans.iter().any(|s| s.content == "it"
            && s.style.add_modifier.contains(Modifier::ITALIC)));
        assert!(spans.iter().any(|s| s.content == "us"
            && s.style.add_modifier.contains(Modifier::ITALIC)));
    }

    #[test]
    fn an_unrelated_earlier_underscore_does_not_reach_into_a_later_url() {
        // A lone `_` in an identifier is a legal opener on its own; the guard
        // must reject the URL's OWN `_` as its close, not just refuse to open
        // inside the URL — the earlier bug guarded only the open.
        let (lines, _, hits) = render_markdown_links(
            "rename foo_bar, see https://example.test/x_y for details",
            80,
            &cfg(),
            None,
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "https://example.test/x_y");
        let underlined: usize = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter(|s| s.style.add_modifier.contains(Modifier::UNDERLINED))
            .map(|s| s.content.chars().count())
            .sum();
        assert_eq!(underlined, "https://example.test/x_y".chars().count());
    }

    #[test]
    fn an_unrelated_earlier_asterisk_does_not_reach_into_a_later_url() {
        let (lines, _, hits) = render_markdown_links(
            "a * lonely star, see https://example.test/x*y for details",
            80,
            &cfg(),
            None,
        );
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].text, "https://example.test/x*y");
        let underlined: usize = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .filter(|s| s.style.add_modifier.contains(Modifier::UNDERLINED))
            .map(|s| s.content.chars().count())
            .sum();
        assert_eq!(underlined, "https://example.test/x*y".chars().count());
    }

    #[test]
    fn a_url_that_legitimately_ends_in_an_underscore_keeps_it() {
        // No marker glued in front, just whitespace after — the trailing `_`
        // is real URL content (S3-key-style), not an emphasis closer, and
        // must survive both the hit text and (by extension) the `o` target.
        let (_, _, hits) = render_markdown_links(
            "upload to https://s3.example.test/keys/report_ then ping me",
            80,
            &cfg(),
            None,
        );
        assert_eq!(hits.len(), 1);
        assert!(hits[0].text.ends_with('_'), "{:?}", hits[0].text);
        assert_eq!(hits[0].text, "https://s3.example.test/keys/report_");
    }

    #[test]
    fn a_url_that_legitimately_ends_in_an_asterisk_keeps_it() {
        let (_, _, hits) = render_markdown_links(
            "see https://wiki.example.test/Page* for the draft",
            80,
            &cfg(),
            None,
        );
        assert_eq!(hits.len(), 1);
        assert!(hits[0].text.ends_with('*'), "{:?}", hits[0].text);
        assert_eq!(hits[0].text, "https://wiki.example.test/Page*");
    }

    #[test]
    fn a_url_wrapped_in_a_single_underscore_marker_loses_only_the_wrapper() {
        let (lines, _, hits) = render_markdown_links("_https://example.test/a_", 40, &cfg(), None);
        assert_eq!(hits.len(), 1, "the wrapping underscores must not split the url");
        assert_eq!(hits[0].text, "https://example.test/a");
        let st = hit_style(&lines, "https://example.test/a");
        assert!(st.add_modifier.contains(Modifier::ITALIC));
        assert!(st.add_modifier.contains(Modifier::UNDERLINED));
    }
}
