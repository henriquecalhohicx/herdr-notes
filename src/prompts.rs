//! Prompt capture storage: the last few prompts submitted in each Claude pane
//! of a tab, written by the `--capture-prompt` hook mode and rendered by the
//! pane above the note.
//!
//! One file PER PANE (`<tab-key>__<pane-key>.prompts.json`) rather than one
//! per tab: a tab can hold several agent panes, and one file per pane means
//! no two hook processes ever read-modify-write the same file, and each agent
//! keeps its own history. The pane merges them on read.
//!
//! Prompt text is stored exactly as it is displayed — first line, truncated —
//! so nothing sits on disk that the pane does not show.

use std::path::{Path, PathBuf};

/// Entries kept per pane. Oldest evicted on append.
#[allow(dead_code)] // wired up by the capture gate chain (Task 4)
pub const RING: usize = 3;
/// Characters kept per prompt, ellipsis included.
#[allow(dead_code)] // wired up by the capture gate chain (Task 4)
pub const MAX_CHARS: usize = 120;

#[allow(dead_code)] // constructed once the capture gate chain lands (Task 4)
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Prompt {
    pub ts: u64,
    pub pane: String,
    pub agent: String,
    pub text: String,
}

/// A prompt reduced to what gets stored and shown: its first line, trimmed,
/// cut to `MAX_CHARS` with a trailing ellipsis when it overflows.
#[allow(dead_code)] // called by the capture gate chain (Task 4)
pub fn condense(prompt: &str) -> String {
    let first = prompt.lines().next().unwrap_or("").trim();
    if first.chars().count() <= MAX_CHARS {
        return first.to_string();
    }
    let kept: String = first.chars().take(MAX_CHARS - 1).collect();
    format!("{kept}…")
}

/// The `prompt` field of a `UserPromptSubmit` payload, or `None` when the
/// payload is unparseable, carries no `prompt` string, or the prompt is blank.
/// Strips a leading BOM — PS 5.1 prepends one when piping into a native exe.
#[allow(dead_code)] // called by the `--capture-prompt` arg (Task 4)
pub fn payload_prompt(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json.trim_start_matches('\u{feff}')).ok()?;
    let prompt = value.get("prompt")?.as_str()?;
    (!prompt.trim().is_empty()).then(|| prompt.to_string())
}

/// `<dir>/<tab-key>__<pane-key>.prompts.json`. Both keys come from
/// `state::id_key`, so they are already filename-safe.
#[allow(dead_code)] // called by the capture gate chain and load_for_tab (Tasks 3-4)
pub fn prompts_file(dir: &Path, tab_key: &str, pane_key: &str) -> PathBuf {
    dir.join(format!("{tab_key}__{pane_key}.prompts.json"))
}

/// Forgiving parse, matching the notes files: a garbled file or a missing
/// field degrades to a default rather than wedging the pane.
#[allow(dead_code)] // called by load_for_tab and append_at's callers (Task 3)
pub fn parse_file(json: &str) -> Vec<Prompt> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json.trim_start_matches('\u{feff}'))
    else {
        return Vec::new();
    };
    let Some(items) = value.get("prompts").and_then(|p| p.as_array()) else {
        return Vec::new();
    };
    items
        .iter()
        .map(|item| Prompt {
            ts: item.get("ts").and_then(|v| v.as_u64()).unwrap_or(0),
            pane: item.get("pane").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            agent: item.get("agent").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            text: item.get("text").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
        })
        .collect()
}

/// `{ "version": 1, "prompts": [ { ts, pane, agent, text } ] }`, oldest first.
#[allow(dead_code)] // called by append_at's persistence path once wired (Task 3)
pub fn to_json(prompts: &[Prompt]) -> String {
    let items: Vec<serde_json::Value> = prompts
        .iter()
        .map(|p| serde_json::json!({"ts": p.ts, "pane": p.pane, "agent": p.agent, "text": p.text}))
        .collect();
    serde_json::json!({"version": 1, "prompts": items}).to_string()
}

/// Append one entry to a pane's ring, evicting the oldest beyond `RING`.
/// Best-effort throughout: an unreadable file is treated as empty and a failed
/// write is silently dropped, because this runs inside a prompt-submit hook
/// where surfacing an error would cost the user their message.
#[allow(dead_code)] // called by the `--capture-prompt` arg (Task 4)
pub fn append_at(path: &Path, entry: Prompt) {
    let mut entries = std::fs::read_to_string(path).map(|j| parse_file(&j)).unwrap_or_default();
    entries.push(entry);
    let overflow = entries.len().saturating_sub(RING);
    entries.drain(..overflow);
    crate::state::write_atomic(path, &to_json(&entries));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir() -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!("herdr-notes-prompts-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&base);
        base
    }

    #[test]
    fn condense_takes_the_first_line_and_truncates() {
        assert_eq!(condense("one line"), "one line");
        assert_eq!(condense("first\nsecond\nthird"), "first");
        assert_eq!(condense("  padded  "), "padded");
        let long = "x".repeat(200);
        let out = condense(&long);
        assert_eq!(out.chars().count(), MAX_CHARS, "truncated to MAX_CHARS including the ellipsis");
        assert!(out.ends_with('…'));
        // Exactly MAX_CHARS is left alone — no ellipsis for a prompt that fits.
        let exact = "y".repeat(MAX_CHARS);
        assert_eq!(condense(&exact), exact);
    }

    #[test]
    fn condense_never_splits_a_wide_char() {
        // Truncation counts CHARS (the spec's unit), but must not panic or
        // produce invalid UTF-8 on multi-byte input.
        let cjk = "文".repeat(200);
        let out = condense(&cjk);
        assert_eq!(out.chars().count(), MAX_CHARS);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn payload_prompt_reads_the_prompt_field() {
        assert_eq!(
            payload_prompt(r#"{"hook_event_name":"UserPromptSubmit","prompt":"do the thing"}"#).as_deref(),
            Some("do the thing")
        );
        // A BOM from PS 5.1 must not defeat the parse.
        assert_eq!(payload_prompt("\u{feff}{\"prompt\":\"bom\"}").as_deref(), Some("bom"));
        assert_eq!(payload_prompt("not json"), None);
        assert_eq!(payload_prompt(r#"{"prompt":""}"#), None, "an empty prompt is nothing to record");
        assert_eq!(payload_prompt(r#"{"prompt":"   "}"#), None);
        assert_eq!(payload_prompt(r#"{"session_id":"abc"}"#), None);
        assert_eq!(payload_prompt(r#"{"prompt":42}"#), None);
    }

    #[test]
    fn prompts_file_joins_the_two_keys() {
        let p = prompts_file(std::path::Path::new("/store"), "w1_t1", "w1_p5");
        assert_eq!(p.file_name().unwrap().to_str().unwrap(), "w1_t1__w1_p5.prompts.json");
    }

    #[test]
    fn parse_file_is_forgiving() {
        assert_eq!(parse_file("garbage"), vec![]);
        assert_eq!(parse_file("{}"), vec![]);
        let json = r#"{"version":1,"prompts":[
            {"ts":10,"pane":"w1:p5","agent":"claude","text":"one"},
            {"ts":20,"text":"two"}
        ]}"#;
        let got = parse_file(json);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], Prompt { ts: 10, pane: "w1:p5".into(), agent: "claude".into(), text: "one".into() });
        assert_eq!(got[1].ts, 20);
        assert_eq!(got[1].text, "two");
        assert_eq!(got[1].pane, "", "missing fields fall back to defaults, they do not drop the entry");
    }

    #[test]
    fn to_json_round_trips_through_parse_file() {
        let entries = vec![
            Prompt { ts: 1, pane: "w1:p5".into(), agent: "claude".into(), text: "a".into() },
            Prompt { ts: 2, pane: "w1:p6".into(), agent: "claude".into(), text: "b".into() },
        ];
        assert_eq!(parse_file(&to_json(&entries)), entries);
    }

    #[test]
    fn append_at_keeps_only_the_newest_ring_entries() {
        let dir = tempdir();
        let path = dir.join("ring_test.prompts.json");
        let _ = std::fs::remove_file(&path);
        for i in 1..=5u64 {
            append_at(&path, Prompt { ts: i, pane: "w1:p5".into(), agent: "claude".into(), text: format!("p{i}") });
        }
        let got = parse_file(&std::fs::read_to_string(&path).unwrap());
        assert_eq!(got.len(), RING);
        assert_eq!(
            got.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
            vec!["p3", "p4", "p5"],
            "oldest evicted, order preserved oldest-first in the file"
        );
        let _ = std::fs::remove_file(&path);
    }
}
