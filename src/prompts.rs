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
pub const RING: usize = 3;
/// Characters kept per prompt, ellipsis included.
pub const MAX_CHARS: usize = 120;

#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct Prompt {
    pub ts: u64,
    pub pane: String,
    pub agent: String,
    pub text: String,
}

/// A prompt reduced to what gets stored and shown: its first line, trimmed,
/// cut to `MAX_CHARS` with a trailing ellipsis when it overflows.
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
pub fn payload_prompt(json: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(json.trim_start_matches('\u{feff}')).ok()?;
    let prompt = value.get("prompt")?.as_str()?;
    (!prompt.trim().is_empty()).then(|| prompt.to_string())
}

/// `<dir>/<tab-key>__<pane-key>.prompts.json`. Both keys come from
/// `state::id_key`, so they are already filename-safe.
pub fn prompts_file(dir: &Path, tab_key: &str, pane_key: &str) -> PathBuf {
    dir.join(format!("{tab_key}__{pane_key}.prompts.json"))
}

/// Forgiving parse, matching the notes files: a garbled file or a missing
/// field degrades to a default rather than wedging the pane.
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
pub fn append_at(path: &Path, entry: Prompt) {
    let mut entries = std::fs::read_to_string(path).map(|j| parse_file(&j)).unwrap_or_default();
    entries.push(entry);
    let overflow = entries.len().saturating_sub(RING);
    entries.drain(..overflow);
    crate::state::write_atomic(path, &to_json(&entries));
}

/// Every pane file belonging to `tab_key`, merged and sorted newest `ts`
/// first, ties broken by `pane` ascending then `text` ascending (so panes
/// racing within the same second still order deterministically — `read_dir`
/// order is not guaranteed across platforms or filesystems), capped at
/// `RING`. The `__` in the filename is what keeps `w1_t1` from matching
/// `w1_t10`'s files — the separator is part of the prefix. Best-effort: an
/// unreadable dir or file contributes nothing.
pub fn load_for_tab(dir: &Path, tab_key: &str) -> Vec<Prompt> {
    let prefix = format!("{tab_key}__");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<Prompt> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        if !name.starts_with(&prefix) || !name.ends_with(".prompts.json") {
            continue;
        }
        if let Ok(json) = std::fs::read_to_string(&path) {
            out.extend(parse_file(&json));
        }
    }
    out.sort_by(|a, b| b.ts.cmp(&a.ts).then_with(|| a.pane.cmp(&b.pane)).then_with(|| a.text.cmp(&b.text)));
    out.truncate(RING);
    out
}

/// The environment the capture gate chain reads, lifted out of `std::env` so
/// every gate is testable without mutating process-global state.
pub struct CaptureEnv {
    /// `HERDR_NOTES_NO_CAPTURE` is set and non-empty.
    pub no_capture: bool,
    /// `HERDR_ENV == "1"`.
    pub in_herdr: bool,
    pub tab_id: Option<String>,
    pub pane_id: Option<String>,
}

impl CaptureEnv {
    /// Read the real process environment.
    pub fn from_process() -> Self {
        let var = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        CaptureEnv {
            no_capture: var("HERDR_NOTES_NO_CAPTURE").is_some(),
            in_herdr: std::env::var("HERDR_ENV").as_deref() == Ok("1"),
            tab_id: var("HERDR_TAB_ID"),
            pane_id: var("HERDR_PANE_ID"),
        }
    }
}

/// The `UserPromptSubmit` gate chain. Returns `true` only when an entry was
/// written; every rejection is silent and total, because the caller runs
/// inside a prompt-submit hook.
///
/// Gates, in order: the off switch, running inside herdr, filename-safe tab
/// AND pane ids (no legacy fallback — prompts are per-tab or nothing), an
/// existing note file for the tab, and a usable payload.
pub fn capture(dir: Option<&Path>, env: &CaptureEnv, stdin: &str, now: u64) -> bool {
    if env.no_capture || !env.in_herdr {
        return false;
    }
    let (Some(dir), Some(tab_id), Some(pane_id)) = (dir, env.tab_id.as_deref(), env.pane_id.as_deref())
    else {
        return false;
    };
    let (Some(tab_key), Some(pane_key)) = (crate::state::id_key(tab_id), crate::state::id_key(pane_id))
    else {
        return false;
    };
    // No note for this tab means the user has not opened Notes here; writing
    // would leave a file behind for a tab that never wanted one. The path
    // comes from `state::note_file_in` rather than being spelled again here:
    // this runs in a SECOND process, and a note layout that grew a suffix or a
    // subdirectory would otherwise stop capture silently, with no diagnostic
    // anywhere (the hook prints nothing by design).
    if !crate::state::note_file_in(dir, &tab_key).exists() {
        return false;
    }
    let Some(text) = payload_prompt(stdin) else {
        return false;
    };
    append_at(
        &prompts_file(dir, &tab_key, &pane_key),
        Prompt { ts: now, pane: pane_id.to_string(), agent: "claude".to_string(), text: condense(&text) },
    );
    true
}

/// `capture` against the real environment and store dir.
pub fn capture_from_env(stdin: &str) -> bool {
    capture(
        crate::state::store_dir().as_deref(),
        &CaptureEnv::from_process(),
        stdin,
        crate::state::unix_now(),
    )
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

    #[test]
    fn equal_timestamps_in_one_file_order_by_text_not_by_file_order() {
        // Deliberately stored zebra-then-alpha: a ts-only stable sort would
        // preserve that, so this is the case that actually discriminates the
        // tie-break from the pre-fix comparator. Unlike the cross-pane test
        // below, it does not lean on read_dir order, which on NTFS is
        // already alphabetical and would mask the bug.
        let dir = tempdir().join("text_ties");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let entries = vec![
            Prompt { ts: 100, pane: "w1:p5".into(), agent: "claude".into(), text: "zebra".into() },
            Prompt { ts: 100, pane: "w1:p5".into(), agent: "claude".into(), text: "alpha".into() },
        ];
        std::fs::write(prompts_file(&dir, "w1_t1", "w1_p5"), to_json(&entries)).unwrap();
        let got = load_for_tab(&dir, "w1_t1");
        assert_eq!(
            got.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "zebra"],
            "equal ts in one file order by text, not by position in the file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_for_tab_merges_pane_files_newest_first() {
        let dir = tempdir().join("merge");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let p5 = vec![
            Prompt { ts: 10, pane: "w1:p5".into(), agent: "claude".into(), text: "old p5".into() },
            Prompt { ts: 40, pane: "w1:p5".into(), agent: "claude".into(), text: "new p5".into() },
        ];
        let p6 = vec![
            Prompt { ts: 20, pane: "w1:p6".into(), agent: "claude".into(), text: "old p6".into() },
            Prompt { ts: 30, pane: "w1:p6".into(), agent: "claude".into(), text: "new p6".into() },
        ];
        std::fs::write(prompts_file(&dir, "w1_t1", "w1_p5"), to_json(&p5)).unwrap();
        std::fs::write(prompts_file(&dir, "w1_t1", "w1_p6"), to_json(&p6)).unwrap();

        let got = load_for_tab(&dir, "w1_t1");
        assert_eq!(
            got.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
            vec!["new p5", "new p6", "old p6"],
            "newest first across both panes, capped at RING"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_for_tab_breaks_ts_ties_deterministically() {
        // Four agent panes in one tab write within the same second; read_dir
        // order is not guaranteed, so the merge must impose its own. NOTE:
        // on a filesystem whose read_dir already returns entries in
        // filename-alphabetical order (e.g. NTFS), this test cannot
        // discriminate the tie-break fix from the pre-fix ts-only sort,
        // since "alphabetical by filename" and "pane ascending" coincide for
        // these inputs. It stays as a useful determinism/idempotence check;
        // `equal_timestamps_in_one_file_order_by_text_not_by_file_order`
        // above is the test that actually fails against the pre-fix
        // comparator on every platform.
        let dir = tempdir().join("ties");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for pane in ["w1_p9", "w1_p5", "w1_p7"] {
            let raw = pane.replace('_', ":");
            let entry = Prompt { ts: 100, pane: raw, agent: "claude".into(), text: format!("from {pane}") };
            std::fs::write(prompts_file(&dir, "w1_t1", pane), to_json(&[entry])).unwrap();
        }
        let got = load_for_tab(&dir, "w1_t1");
        assert_eq!(
            got.iter().map(|p| p.pane.as_str()).collect::<Vec<_>>(),
            vec!["w1:p5", "w1:p7", "w1:p9"],
            "equal timestamps order by pane, not by read_dir"
        );
        // Same input, same output, every time.
        assert_eq!(load_for_tab(&dir, "w1_t1"), got);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_for_tab_ignores_other_tabs_and_the_notes_themselves() {
        let dir = tempdir().join("isolate");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mine = vec![Prompt { ts: 1, pane: "w1:p5".into(), agent: "claude".into(), text: "mine".into() }];
        let theirs = vec![Prompt { ts: 9, pane: "w2:p1".into(), agent: "claude".into(), text: "theirs".into() }];
        std::fs::write(prompts_file(&dir, "w1_t1", "w1_p5"), to_json(&mine)).unwrap();
        std::fs::write(prompts_file(&dir, "w2_t7", "w2_p1"), to_json(&theirs)).unwrap();
        std::fs::write(dir.join("w1_t1.json"), r#"{"text":"the note"}"#).unwrap();

        let got = load_for_tab(&dir, "w1_t1");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "mine");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_for_tab_is_empty_for_a_missing_dir() {
        assert_eq!(load_for_tab(std::path::Path::new("/no/such/dir/anywhere"), "w1_t1"), vec![]);
    }

    #[test]
    fn load_for_tab_does_not_match_a_tab_key_that_is_a_prefix_of_another() {
        // `w1_t1` must not pick up `w1_t10`'s files.
        let dir = tempdir().join("prefix");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let other = vec![Prompt { ts: 5, pane: "w1:p9".into(), agent: "claude".into(), text: "t10".into() }];
        std::fs::write(prompts_file(&dir, "w1_t10", "w1_p9"), to_json(&other)).unwrap();
        assert_eq!(load_for_tab(&dir, "w1_t1"), vec![]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn env_ok() -> CaptureEnv {
        CaptureEnv {
            no_capture: false,
            in_herdr: true,
            tab_id: Some("w1:t1".into()),
            pane_id: Some("w1:p5".into()),
        }
    }

    /// A store dir holding a note file for `w1:t1`, so gate 4 passes.
    fn store_with_note(name: &str) -> std::path::PathBuf {
        let dir = tempdir().join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let key = crate::state::id_key("w1:t1").unwrap();
        std::fs::write(dir.join(format!("{key}.json")), r#"{"text":"a note"}"#).unwrap();
        dir
    }

    fn captured(dir: &std::path::Path) -> Vec<Prompt> {
        load_for_tab(dir, &crate::state::id_key("w1:t1").unwrap())
    }

    #[test]
    fn capture_writes_a_condensed_entry_when_every_gate_passes() {
        let dir = store_with_note("gate_pass");
        assert!(capture(Some(&dir), &env_ok(), r#"{"prompt":"fix the auth test\nsecond line"}"#, 99));
        let got = captured(&dir);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "fix the auth test", "first line only");
        assert_eq!(got[0].ts, 99);
        assert_eq!(got[0].pane, "w1:p5");
        assert_eq!(got[0].agent, "claude");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_gate_1_off_switch() {
        let dir = store_with_note("gate1");
        let env = CaptureEnv { no_capture: true, ..env_ok() };
        assert!(!capture(Some(&dir), &env, r#"{"prompt":"x"}"#, 1));
        assert_eq!(captured(&dir), vec![]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_gate_2_outside_herdr() {
        let dir = store_with_note("gate2");
        let env = CaptureEnv { in_herdr: false, ..env_ok() };
        assert!(!capture(Some(&dir), &env, r#"{"prompt":"x"}"#, 1));
        assert_eq!(captured(&dir), vec![]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_gate_3_missing_or_unsafe_ids() {
        let dir = store_with_note("gate3");
        for env in [
            CaptureEnv { tab_id: None, ..env_ok() },
            CaptureEnv { tab_id: Some("bad id".into()), ..env_ok() },
            CaptureEnv { pane_id: None, ..env_ok() },
            CaptureEnv { pane_id: Some("../escape".into()), ..env_ok() },
        ] {
            assert!(!capture(Some(&dir), &env, r#"{"prompt":"x"}"#, 1));
        }
        assert_eq!(captured(&dir), vec![], "no legacy fallback: per-tab or nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_gate_4_no_note_file_for_this_tab() {
        let dir = tempdir().join("gate4");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!capture(Some(&dir), &env_ok(), r#"{"prompt":"x"}"#, 1));
        assert_eq!(captured(&dir), vec![], "no note, no capture, no file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_gate_5_unusable_payloads() {
        let dir = store_with_note("gate5");
        for stdin in ["", "not json", r#"{}"#, r#"{"prompt":""}"#, r#"{"prompt":"  "}"#] {
            assert!(!capture(Some(&dir), &env_ok(), stdin, 1), "stdin {stdin:?} must not write");
        }
        assert_eq!(captured(&dir), vec![]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_without_a_store_dir_is_a_noop() {
        assert!(!capture(None, &env_ok(), r#"{"prompt":"x"}"#, 1));
    }

    #[test]
    fn capture_writes_only_this_panes_file() {
        let dir = store_with_note("per_pane");
        let p5 = CaptureEnv { pane_id: Some("w1:p5".into()), ..env_ok() };
        let p6 = CaptureEnv { pane_id: Some("w1:p6".into()), ..env_ok() };
        assert!(capture(Some(&dir), &p5, r#"{"prompt":"from p5"}"#, 10));
        assert!(capture(Some(&dir), &p6, r#"{"prompt":"from p6"}"#, 20));
        assert!(prompts_file(&dir, "w1_t1", &crate::state::id_key("w1:p5").unwrap()).exists());
        assert!(prompts_file(&dir, "w1_t1", &crate::state::id_key("w1:p6").unwrap()).exists());
        let got = captured(&dir);
        assert_eq!(got.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), vec!["from p6", "from p5"]);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
