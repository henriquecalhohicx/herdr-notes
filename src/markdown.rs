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

pub fn render_markdown(text: &str, width: usize) -> Vec<Line<'static>> {
    let width = width.max(8);
    let mut out = Vec::new();
    let mut in_code = false;
    for raw in text.lines() {
        let line = raw.trim_end();
        if line.trim_start().starts_with("```") {
            in_code = !in_code;
            out.push(Line::from(Span::styled(
                line.to_string(),
                Style::default().fg(CODE).add_modifier(Modifier::DIM),
            )));
            continue;
        }
        if in_code {
            wrap_into(&mut out, vec![(line.to_string(), Style::default().fg(CODE))], width, 0);
            continue;
        }
        render_line(&mut out, line, width);
    }
    if out.is_empty() {
        out.push(Line::raw(""));
    }
    out
}

fn render_line(out: &mut Vec<Line<'static>>, line: &str, width: usize) {
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
        wrap_into(out, parse_inline(trimmed[hashes..].trim_start(), style), width, 0);
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
        wrap_into(out, spans, width, 2);
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
        wrap_into(out, spans, width, indent + 4);
        return;
    }

    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(bullet) {
            let mut spans = vec![(format!("{pad}• "), Style::default().fg(ACCENT))];
            spans.extend(parse_inline(rest, Style::default()));
            wrap_into(out, spans, width, indent + 2);
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
            wrap_into(out, spans, width, indent + digits + 2);
            return;
        }
    }

    let mut spans = Vec::new();
    if indent > 0 {
        spans.push((pad, Style::default()));
    }
    spans.extend(parse_inline(trimmed, Style::default()));
    wrap_into(out, spans, width, 0);
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
// Consumed by later tasks (preview cursor, overlay progress column); unused
// for now so clippy's dead_code lint needs a nudge.
#[allow(dead_code)]
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
#[allow(dead_code)]
pub fn checkbox_counts(text: &str) -> (usize, usize) {
    let boxes = checkbox_lines(text);
    (boxes.iter().filter(|(_, done)| *done).count(), boxes.len())
}

/// `text` with the checkbox on `line_idx` flipped, or `None` when that line
/// is not a checkbox. Splits on `'\n'` rather than `lines()` so a trailing
/// newline survives the round-trip; the two index identically for every line
/// `lines()` yields, so a `checkbox_lines` index is safe here.
#[allow(dead_code)]
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

/// `---` / `***` / `___` (3+ of the same marker, spaces allowed between).
fn is_hr(t: &str) -> bool {
    let bare: String = t.chars().filter(|c| !c.is_whitespace()).collect();
    bare.len() >= 3
        && ['-', '*', '_']
            .iter()
            .any(|m| bare.chars().all(|c| c == *m))
}

/// Inline spans: `` `code` ``, `**bold**`, `*italic*` / `_italic_`. Markers
/// without a closing partner (or with empty content) render literally; no
/// nesting — styled content is taken as-is.
fn parse_inline(s: &str, base: Style) -> Vec<(String, Style)> {
    let chars: Vec<char> = s.chars().collect();
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
        } else if c == '*' && chars.get(i + 1) == Some(&'*') {
            if let Some(close) = find_double_star(&chars, i + 2).filter(|&p| p > i + 2) {
                flush(&mut plain, &mut out);
                out.push((
                    chars[i + 2..close].iter().collect(),
                    base.add_modifier(Modifier::BOLD),
                ));
                i = close + 2;
                continue;
            }
        } else if (c == '*' || c == '_')
            && let Some(off) = chars[i + 1..].iter().position(|&d| d == c).filter(|&o| o > 0)
        {
            let close = i + 1 + off;
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

fn find_double_star(chars: &[char], from: usize) -> Option<usize> {
    (from..chars.len().saturating_sub(1)).find(|&j| chars[j] == '*' && chars[j + 1] == '*')
}

/// Greedy wrap at `width` display COLUMNS (breaking at the last space when
/// possible), giving continuation lines `hang` columns of indent. Wide
/// (CJK/emoji) chars count 2 so wrapped lines never overflow the no-wrap
/// Paragraph and get clipped off the right edge.
fn wrap_into(out: &mut Vec<Line<'static>>, spans: Vec<(String, Style)>, width: usize, hang: usize) {
    let chars: Vec<(char, Style, usize)> = spans
        .iter()
        .flat_map(|(t, s)| {
            t.chars().map(|c| (c, *s, c.width().unwrap_or(0))).collect::<Vec<_>>()
        })
        .collect();
    let mut start = 0;
    let mut first = true;
    loop {
        let budget = if first { width } else { width.saturating_sub(hang).max(4) };
        // Longest slice from `start` that fits the column budget.
        let mut end = start;
        let mut cols = 0;
        while end < chars.len() && cols + chars[end].2 <= budget {
            cols += chars[end].2;
            end += 1;
        }
        if end >= chars.len() {
            out.push(to_line(&chars[start..], if first { 0 } else { hang }));
            return;
        }
        if let Some(pos) =
            chars[start..end].iter().rposition(|(c, _, _)| *c == ' ').filter(|&p| p > 0)
        {
            end = start + pos;
        }
        // A single char wider than the whole budget still makes progress.
        if end == start {
            end = start + 1;
        }
        out.push(to_line(&chars[start..end], if first { 0 } else { hang }));
        start = end;
        while start < chars.len() && chars[start].0 == ' ' {
            start += 1;
        }
        first = false;
        if start >= chars.len() {
            return;
        }
    }
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
}
