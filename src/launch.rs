//! Launcher helpers behind `scripts/open-notes.{sh,ps1}` — kept in Rust so the
//! logic is unit-tested and ids extracted from herdr's JSON are validated
//! before they reach an argv. Three stdin→stdout modes:
//!
//! - `--launch-decision`: `herdr pane list` JSON → `OPEN` | `FOCUS <id>` |
//!   `CLOSE <id>` | `REPLACE <id>`, matching a Notes pane that edits the
//!   focused pane's NOTE FILE ([`crate::state::note_key`], keyed on the tab
//!   id) — a duplicate on one file would clobber it on save, while a Notes
//!   pane in another tab is a different document. The launchers pass the
//!   GLOBAL (unscoped) pane list: scoping it by the launcher shell's
//!   `HERDR_TAB_ID` would drop the (globally unique) focused pane whenever
//!   that env id diverges from the focused pane's actual tab, degrading to
//!   `OPEN` and spawning a duplicate. All tab scoping happens HERE, off each
//!   pane's `tab_id` field.
//! - `--focused-pane`: `herdr pane list` JSON → `<pane_id>\t<cwd>` of the
//!   focused pane (cwd stripped of the Windows `\\?\` verbatim prefix).
//! - `--open-plan`: `herdr pane layout` JSON → `<rightmost_pane_id>\t<ratio>`,
//!   the split target and ORIGINAL-pane share that docks Notes on the RIGHT
//!   edge (`pane split --ratio` is the original pane's share; no swap needed).

use serde::Deserialize;

use crate::state::{note_key, METADATA_SOURCE, PANE_LABEL};

/// Preferred notes width in columns; the ratio is derived from the target pane.
const TARGET_COLS: f64 = 42.0;

/// A live TUI re-stamps its identity token every few seconds; a token older
/// than this marks a dead pane that should be replaced, not focused.
pub const HEARTBEAT_STALE_SECS: u64 = 20;

#[derive(Deserialize)]
struct PaneListMsg {
    result: PaneListResult,
}

#[derive(Deserialize)]
struct PaneListResult {
    #[serde(default)]
    panes: Vec<Pane>,
}

#[derive(Deserialize)]
struct Pane {
    pane_id: Option<String>,
    label: Option<String>,
    cwd: Option<String>,
    #[serde(default)]
    focused: bool,
    tab_id: Option<String>,
    #[serde(default)]
    tokens: serde_json::Map<String, serde_json::Value>,
}

impl Pane {
    /// A Notes pane is recognized by its heartbeat token (reported by the TUI)
    /// or by the "Notes" label (present from the moment the launcher renames
    /// the fresh pane, before the TUI has reported its token).
    fn is_notes(&self) -> bool {
        self.tokens.contains_key(METADATA_SOURCE) || self.label.as_deref() == Some(PANE_LABEL)
    }
}

#[derive(Deserialize)]
struct LayoutMsg {
    result: LayoutResult,
}

#[derive(Deserialize)]
struct LayoutResult {
    layout: Layout,
}

#[derive(Deserialize)]
struct Layout {
    #[serde(default)]
    panes: Vec<LayoutPane>,
}

#[derive(Deserialize)]
struct LayoutPane {
    pane_id: Option<String>,
    rect: Option<Rect>,
}

#[derive(Deserialize)]
struct Rect {
    x: i64,
    y: i64,
    width: i64,
}

/// Windows PowerShell 5.1 prepends a UTF-8 BOM when piping into a native
/// process's stdin; serde_json rejects a BOM before `{`.
fn strip_bom(input: &str) -> &str {
    input.trim_start_matches('\u{feff}')
}

/// True when `key` is present but its heartbeat timestamp is missing,
/// unparsable, or older than [`HEARTBEAT_STALE_SECS`]. Absent key = false
/// (a fresh pane the launcher labeled but whose TUI hasn't reported yet).
fn token_stale(tokens: &serde_json::Map<String, serde_json::Value>, key: &str, now: u64) -> bool {
    let Some(value) = tokens.get(key) else { return false };
    let ts = value
        .as_u64()
        .or_else(|| value.as_str().and_then(|s| s.parse().ok()));
    match ts {
        Some(ts) => now.saturating_sub(ts) > HEARTBEAT_STALE_SECS,
        None => true,
    }
}

/// Whether THIS tab has a Notes pane that is alive right now: `Some(true)` when
/// a pane in `tab_id` carries a `herdr-notes` heartbeat token that is present
/// AND fresh, `Some(false)` when the list parsed and no such pane exists,
/// `None` when the JSON could not be parsed at all.
///
/// Deliberately does NOT use [`Pane::is_notes`], which also accepts the "Notes"
/// LABEL, nor [`token_stale`] alone, which reports a MISSING token as not
/// stale. Both are right for the launcher — it must recognize a pane it has
/// just labeled, before that pane's TUI has reported anything. Both are wrong
/// for the capture gate, which needs evidence the pane is running: a label
/// outlives a dead pane, the token does not.
// Not yet called outside tests — the capture-gate wiring lands in Task 3.
#[allow(dead_code)]
pub fn notes_pane_fresh(pane_list_json: &str, tab_id: &str, now: u64) -> Option<bool> {
    let msg = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)).ok()?;
    Some(msg.result.panes.iter().any(|p| {
        p.tab_id.as_deref() == Some(tab_id)
            && p.tokens.contains_key(METADATA_SOURCE)
            && !token_stale(&p.tokens, METADATA_SOURCE, now)
    }))
}

/// `OPEN`, `FOCUS <id>`, `CLOSE <id>`, or `REPLACE <id>` (dead pane: close it,
/// then open fresh) from a `pane list` JSON. Unparseable input, no focused
/// pane, or an unsafe id all degrade to `OPEN`.
pub fn launch_decision(pane_list_json: &str, now: u64) -> String {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return "OPEN".to_string();
    };
    let panes = &msg.result.panes;
    let Some(focused) = panes.iter().find(|p| p.focused) else {
        return "OPEN".to_string();
    };
    // Match a Notes pane editing the SAME NOTE FILE as the focused pane — one
    // file per tab, loaded once at startup, so a second live instance on one
    // file would silently overwrite the other's edits (last-writer-wins) on
    // every save. A Notes pane in a DIFFERENT tab is a different document and
    // is deliberately ignored, so each tab opens its own. The comparison uses
    // state.rs's note_key on the tab id — the exact identity that picks the
    // file on disk — never raw tab ids: ids that both coarsen to the shared
    // legacy file (missing or filename-unsafe), or that differ only by ASCII
    // case on case-insensitive NTFS, are one document here too.
    let focused_key = note_key(focused.tab_id.as_deref());
    let same_note = |p: &&Pane| note_key(p.tab_id.as_deref()) == focused_key;
    let notes = panes.iter().filter(same_note).find(|p| p.is_notes());
    let Some(pane) = notes else {
        return "OPEN".to_string();
    };
    let Some(id) = pane.pane_id.as_deref().filter(|id| is_flag_safe(id)) else {
        return "OPEN".to_string();
    };
    if token_stale(&pane.tokens, METADATA_SOURCE, now) {
        return format!("REPLACE {id}");
    }
    if Some(id) == focused.pane_id.as_deref() {
        format!("CLOSE {id}")
    } else {
        format!("FOCUS {id}")
    }
}

/// `<pane_id>\t<cwd>` of the focused pane, or empty on any failure.
pub fn focused_pane(pane_list_json: &str) -> String {
    let Ok(msg) = serde_json::from_str::<PaneListMsg>(strip_bom(pane_list_json)) else {
        return String::new();
    };
    let Some(focused) = msg.result.panes.iter().find(|p| p.focused) else {
        return String::new();
    };
    let Some(id) = focused.pane_id.as_deref().filter(|id| is_flag_safe(id)) else {
        return String::new();
    };
    let cwd = focused.cwd.as_deref().map(strip_verbatim).unwrap_or_default();
    format!("{id}\t{cwd}")
}

/// `<pane_id>\t<ratio>` for the right dock: the RIGHTMOST pane of the layout
/// and the share the ORIGINAL pane keeps so Notes gets ~[`TARGET_COLS`]
/// columns on the right edge. Empty on any failure.
pub fn open_plan(layout_json: &str) -> String {
    let Ok(msg) = serde_json::from_str::<LayoutMsg>(strip_bom(layout_json)) else {
        return String::new();
    };
    let mut best: Option<(&str, &Rect)> = None;
    for pane in &msg.result.layout.panes {
        let (Some(id), Some(rect)) = (pane.pane_id.as_deref(), pane.rect.as_ref()) else {
            continue;
        };
        if !is_flag_safe(id) || rect.width <= 0 {
            continue;
        }
        // Rightmost edge wins; among a right column of stacked panes, topmost.
        let better = match best {
            None => true,
            Some((_, b)) => (rect.x + rect.width, -rect.y) > (b.x + b.width, -b.y),
        };
        if better {
            best = Some((id, rect));
        }
    }
    let Some((id, rect)) = best else {
        return String::new();
    };
    let notes_share = (TARGET_COLS / rect.width as f64).clamp(0.15, 0.5);
    let ratio = 1.0 - notes_share;
    format!("{id}\t{ratio:.2}")
}

/// True when the id can be passed as a positional argument to the herdr CLI
/// without any risk of being parsed as a flag.
fn is_flag_safe(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('-')
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ':' | '.' | '_' | '-'))
}

fn strip_verbatim(path: &str) -> &str {
    path.strip_prefix(r"\\?\").unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane_list(panes: &str) -> String {
        format!(r#"{{"id":"cli:pane:list","result":{{"panes":[{panes}]}}}}"#)
    }

    const FOCUSED: &str =
        r#"{"pane_id":"w1:p1","focused":true,"tab_id":"w1:t1","cwd":"C:\\work\\my repo"}"#;

    #[test]
    fn decision_open_focus_close() {
        // A Notes pane in ANOTHER tab edits a different note file — not a
        // duplicate: OPEN a fresh one for the focused tab.
        let other_tab = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p9","label":"Notes","tab_id":"w1:t2"}}"#
        ));
        assert_eq!(launch_decision(&other_tab, 100), "OPEN");
        // A Notes pane in the focused tab is focused, ignoring an other-tab one.
        let both_tabs = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p9","label":"Notes","tab_id":"w1:t2"}},{{"pane_id":"w1:p2","label":"Notes","tab_id":"w1:t1"}}"#
        ));
        assert_eq!(launch_decision(&both_tabs, 100), "FOCUS w1:p2");
        // Same-tab unfocused pane is focused.
        let same_tab = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p2","label":"Notes","tab_id":"w1:t1"}}"#
        ));
        assert_eq!(launch_decision(&same_tab, 100), "FOCUS w1:p2");
        // Focused Notes pane toggles closed.
        let focused_notes =
            pane_list(r#"{"pane_id":"w1:p2","label":"Notes","tab_id":"w1:t1","focused":true}"#);
        assert_eq!(launch_decision(&focused_notes, 100), "CLOSE w1:p2");
        // Token without label also identifies the pane.
        let token_only = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p2","tab_id":"w1:t1","tokens":{{"herdr-notes":"95"}}}}"#
        ));
        assert_eq!(launch_decision(&token_only, 100), "FOCUS w1:p2");
    }

    #[test]
    fn decision_ignores_notes_panes_in_other_tabs() {
        let focused = r#"{"pane_id":"w1:p1","focused":true,"tab_id":"w1:t1"}"#;
        // A Notes pane in another tab (same or different workspace) edits a
        // different note file — not a duplicate, not focusable from here: OPEN.
        let other_ws_tab = pane_list(&format!(
            r#"{focused},{{"pane_id":"w2:p9","label":"Notes","tab_id":"w2:t1"}}"#
        ));
        assert_eq!(launch_decision(&other_ws_tab, 100), "OPEN");
        let same_ws_other_tab = pane_list(&format!(
            r#"{focused},{{"pane_id":"w1:p9","label":"Notes","tab_id":"w1:t2"}}"#
        ));
        assert_eq!(launch_decision(&same_ws_other_tab, 100), "OPEN");
        // Same tab: matched (the real duplicate risk on one note file).
        let same_tab = pane_list(&format!(
            r#"{focused},{{"pane_id":"w1:p9","label":"Notes","tab_id":"w1:t1"}}"#
        ));
        assert_eq!(launch_decision(&same_tab, 100), "FOCUS w1:p9");
    }

    #[test]
    fn decision_matches_on_note_file_identity_not_raw_tab_id() {
        // Filename-unsafe tab ids ("w-1", "w 2") and a MISSING id all load and
        // save the same shared legacy notes.json (state.rs note_key = None): a
        // Notes pane under any of them is a true duplicate of the focused pane's
        // note and must be matched, never OPEN'd over.
        let focused = r#"{"pane_id":"w1:p1","focused":true,"tab_id":"w-1"}"#;
        let legacy_mate = pane_list(&format!(
            r#"{focused},{{"pane_id":"w2:p9","label":"Notes","tab_id":"w 2"}}"#
        ));
        assert_eq!(launch_decision(&legacy_mate, 100), "FOCUS w2:p9");
        let missing_id = pane_list(&format!(
            r#"{focused},{{"pane_id":"w2:p9","label":"Notes"}}"#
        ));
        assert_eq!(launch_decision(&missing_id, 100), "FOCUS w2:p9");
        // A safe tab id has its own file — NOT a duplicate of a legacy-keyed pane.
        let safe_focused = r#"{"pane_id":"w1:p1","focused":true,"tab_id":"w1:t1"}"#;
        let mixed = pane_list(&format!(
            r#"{safe_focused},{{"pane_id":"w2:p9","label":"Notes","tab_id":"w-2"}}"#
        ));
        assert_eq!(launch_decision(&mixed, 100), "OPEN");
    }

    #[cfg(windows)]
    #[test]
    fn decision_folds_tab_id_case_on_windows() {
        // NTFS filenames are case-insensitive: tab ids "W1:T1" and "w1:t1"
        // share one w1_t1.json, so their Notes panes are duplicates.
        let focused = r#"{"pane_id":"w1:p1","focused":true,"tab_id":"W1:T1"}"#;
        let other_case = pane_list(&format!(
            r#"{focused},{{"pane_id":"w1:p9","label":"Notes","tab_id":"w1:t1"}}"#
        ));
        assert_eq!(launch_decision(&other_case, 100), "FOCUS w1:p9");
    }

    #[test]
    fn decision_replaces_dead_panes() {
        let stale = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p2","tab_id":"w1:t1","tokens":{{"herdr-notes":"40"}}}}"#
        ));
        assert_eq!(launch_decision(&stale, 100), "REPLACE w1:p2");
        let garbled = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p2","tab_id":"w1:t1","tokens":{{"herdr-notes":{{"v":1}}}}}}"#
        ));
        assert_eq!(launch_decision(&garbled, 100), "REPLACE w1:p2");
        // Label-only (no token yet) = launcher-fresh, NOT dead.
        let fresh = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"w1:p2","label":"Notes","tab_id":"w1:t1"}}"#
        ));
        assert_eq!(launch_decision(&fresh, 100), "FOCUS w1:p2");
    }

    #[test]
    fn decision_degrades_to_open_on_garbage_or_unsafe_ids() {
        assert_eq!(launch_decision("not json", 100), "OPEN");
        assert_eq!(launch_decision(&pane_list(r#"{"pane_id":"w1:p1"}"#), 100), "OPEN");
        let evil = pane_list(&format!(
            r#"{FOCUSED},{{"pane_id":"--evil","label":"Notes","tab_id":"w1:t1"}}"#
        ));
        assert_eq!(launch_decision(&evil, 100), "OPEN");
    }

    #[test]
    fn focused_pane_reports_id_and_stripped_cwd() {
        let json = pane_list(
            r#"{"pane_id":"w1:p3","focused":true,"tab_id":"w1:t1","cwd":"\\\\?\\C:\\work\\my repo"}"#,
        );
        assert_eq!(focused_pane(&json), "w1:p3\tC:\\work\\my repo");
        assert_eq!(focused_pane("not json"), "");
        assert_eq!(focused_pane(&pane_list(r#"{"pane_id":"w1:p1"}"#)), "");
    }

    fn layout(panes: &str) -> String {
        format!(r#"{{"id":"cli:pane:layout","result":{{"layout":{{"panes":[{panes}]}}}}}}"#)
    }

    #[test]
    fn open_plan_picks_rightmost_topmost_pane() {
        let json = layout(
            r#"{"pane_id":"w1:p1","rect":{"x":0,"y":0,"width":30,"height":50}},
               {"pane_id":"w1:p3","rect":{"x":30,"y":25,"width":140,"height":25}},
               {"pane_id":"w1:p2","rect":{"x":30,"y":0,"width":140,"height":25}}"#,
        );
        let (id, ratio) = open_plan(&json).split_once('\t').map(|(a, b)| (a.to_string(), b.to_string())).unwrap();
        assert_eq!(id, "w1:p2", "rightmost column, topmost pane");
        assert_eq!(ratio, "0.70"); // 1 - 42/140
    }

    #[test]
    fn open_plan_clamps_ratio() {
        let wide = layout(r#"{"pane_id":"w1:p1","rect":{"x":0,"y":0,"width":400,"height":50}}"#);
        assert_eq!(open_plan(&wide), "w1:p1\t0.85"); // notes share clamped to 0.15
        let narrow = layout(r#"{"pane_id":"w1:p1","rect":{"x":0,"y":0,"width":60,"height":50}}"#);
        assert_eq!(open_plan(&narrow), "w1:p1\t0.50"); // notes share clamped to 0.5
    }

    #[test]
    fn open_plan_is_empty_on_failure() {
        assert_eq!(open_plan("not json"), "");
        assert_eq!(open_plan(&layout("")), "");
        let unsafe_id = layout(r#"{"pane_id":"--x","rect":{"x":0,"y":0,"width":90,"height":50}}"#);
        assert_eq!(open_plan(&unsafe_id), "");
    }

    #[test]
    fn utf8_bom_from_powershell_pipe_is_stripped() {
        let json = format!("\u{feff}{}", pane_list(FOCUSED));
        assert_eq!(launch_decision(&json, 100), "OPEN");
        assert!(focused_pane(&json).starts_with("w1:p1\t"));
    }

    #[test]
    fn notes_pane_fresh_requires_a_present_and_fresh_token() {
        let json = pane_list(
            r#"{"pane_id":"w1:p2","tab_id":"w1:t1","tokens":{"herdr-notes":"95"}}"#,
        );
        assert_eq!(notes_pane_fresh(&json, "w1:t1", 100), Some(true), "5s old is fresh");
        assert_eq!(notes_pane_fresh(&json, "w1:t9", 100), Some(false), "other tab");
    }

    #[test]
    fn notes_pane_fresh_rejects_a_stale_token() {
        // 60s old against a 20s threshold.
        let json = pane_list(
            r#"{"pane_id":"w1:p2","tab_id":"w1:t1","tokens":{"herdr-notes":"40"}}"#,
        );
        assert_eq!(notes_pane_fresh(&json, "w1:t1", 100), Some(false));
    }

    #[test]
    fn notes_pane_fresh_rejects_a_missing_token_even_with_the_notes_label() {
        // `token_stale` says "not stale" for an absent token and `is_notes`
        // accepts the label alone — both right for the launcher, both wrong
        // here. A label outlives a dead pane; the token does not.
        let json = pane_list(
            r#"{"pane_id":"w1:p2","tab_id":"w1:t1","label":"Notes"}"#,
        );
        assert_eq!(notes_pane_fresh(&json, "w1:t1", 100), Some(false));
    }

    #[test]
    fn notes_pane_fresh_rejects_an_unparsable_token_value() {
        let json = pane_list(
            r#"{"pane_id":"w1:p2","tab_id":"w1:t1","tokens":{"herdr-notes":{"v":1}}}"#,
        );
        assert_eq!(notes_pane_fresh(&json, "w1:t1", 100), Some(false));
    }

    #[test]
    fn notes_pane_fresh_is_none_on_unparsable_json() {
        assert_eq!(notes_pane_fresh("not json", "w1:t1", 100), None);
        assert_eq!(notes_pane_fresh("", "w1:t1", 100), None);
    }

    #[test]
    fn notes_pane_fresh_strips_a_bom() {
        let json = format!(
            "\u{feff}{}",
            pane_list(r#"{"pane_id":"w1:p2","tab_id":"w1:t1","tokens":{"herdr-notes":"95"}}"#)
        );
        assert_eq!(notes_pane_fresh(&json, "w1:t1", 100), Some(true));
    }
}
