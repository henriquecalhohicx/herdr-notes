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
/// Wall-clock bound on the capture gate's `pane.list` call. Well short of the
/// hook's own `timeout: 5`, because a hook killed at its limit is a risk to the
/// user's prompt and this path must never approach it.
pub const GATE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(300);

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

/// One pane's prompts, newest first, capped at `RING`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct PromptGroup {
    /// Raw pane id as recorded at capture time, e.g. "wD:p8".
    pub pane: String,
    pub prompts: Vec<Prompt>,
}

/// Every pane file belonging to `tab_key`, grouped by the pane that sent
/// each prompt. Each group keeps its own `RING` — four agents keep three
/// each, not three between them, which is the whole point of grouping.
/// Groups are ordered by their newest `ts` descending, ties on `pane`
/// ascending, because `read_dir` order is not guaranteed across platforms.
/// Grouping reads the stored `pane` FIELD rather than the filename, so a
/// hand-edited file holding two panes' entries still splits correctly.
/// The `__` in the filename prefix is what keeps `w1_t1` from matching
/// `w1_t10`'s files. Best-effort: an unreadable dir or file contributes
/// nothing.
pub fn load_for_tab(dir: &Path, tab_key: &str) -> Vec<PromptGroup> {
    let prefix = format!("{tab_key}__");
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut all: Vec<Prompt> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
        if !name.starts_with(&prefix) || !name.ends_with(".prompts.json") {
            continue;
        }
        if let Ok(json) = std::fs::read_to_string(&path) {
            all.extend(parse_file(&json));
        }
    }

    let mut groups: Vec<PromptGroup> = Vec::new();
    for p in all {
        match groups.iter_mut().find(|g| g.pane == p.pane) {
            Some(g) => g.prompts.push(p),
            None => groups.push(PromptGroup { pane: p.pane.clone(), prompts: vec![p] }),
        }
    }
    for g in &mut groups {
        g.prompts.sort_by(|a, b| b.ts.cmp(&a.ts).then_with(|| a.text.cmp(&b.text)));
        g.prompts.truncate(RING);
    }
    groups.sort_by(|a, b| {
        // `unwrap_or(0)` is unreachable in practice: every `PromptGroup` above
        // is created together with its first prompt (`vec![p]`), so `prompts`
        // is never empty here. Kept defensive rather than `.unwrap()` in case
        // that invariant ever grows an exception, without implying one exists.
        let newest = |g: &PromptGroup| g.prompts.first().map(|p| p.ts).unwrap_or(0);
        newest(b).cmp(&newest(a)).then_with(|| a.pane.cmp(&b.pane))
    });
    groups
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
/// AND pane ids (no legacy fallback — prompts are per-tab or nothing), a live
/// Notes pane in this tab OR an existing note file, and a usable payload.
///
/// `notes_live` is injected so every gate stays testable without a socket:
/// `Some(true)` a Notes pane in this tab is alive, `Some(false)` the socket
/// answered and none is, `None` the socket could not be reached. It is
/// ADDITIVE — it can only open the gate earlier, never close it on something
/// that captures today — so a socket failure degrades to the note-file check
/// rather than to silent no-capture.
pub fn capture(
    dir: Option<&Path>,
    env: &CaptureEnv,
    stdin: &str,
    now: u64,
    notes_live: Option<bool>,
) -> bool {
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
    // A live Notes pane means the user is looking at this tab's note right
    // now, so capture without waiting for them to type something into it —
    // an empty note is `state::is_blank` and therefore has no file at all.
    if notes_live != Some(true) && !crate::state::note_file_in(dir, &tab_key).exists() {
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

/// `capture` against the real environment, store dir, and herdr socket.
pub fn capture_from_env(stdin: &str) -> bool {
    let env = CaptureEnv::from_process();
    let now = crate::state::unix_now();
    // `capture` itself stays the sole authority on whether to write — this is
    // only about not paying for a `pane.list` round trip when the answer
    // could not possibly change what `capture` decides. Skip the socket for
    // the off switch, outside herdr, and an unusable payload: all three
    // reject unconditionally in `capture` regardless of `notes_live`, and the
    // off switch in particular exists so a user can opt out of exactly this
    // kind of round trip on every prompt they submit. `payload_prompt` is a
    // JSON parse with no I/O, so checking it here is cheap. The tab-id
    // condition is kept as before: no tab id, no meaningful answer to ask for.
    let notes_live = (!env.no_capture && env.in_herdr && payload_prompt(stdin).is_some())
        .then_some(env.tab_id.as_deref())
        .flatten()
        .and_then(|tab| {
            crate::ipc::call_text_bounded("pane.list", serde_json::json!({}), GATE_TIMEOUT)
                .ok()
                .and_then(|json| crate::launch::notes_pane_fresh(&json, tab, now))
        });
    capture(crate::state::store_dir().as_deref(), &env, stdin, now, notes_live)
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
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].prompts.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
            vec!["alpha", "zebra"],
            "equal ts in one file order by text, not by position in the file"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_for_tab_groups_by_pane_newest_group_first() {
        let dir = tempdir().join("groups");
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
        assert_eq!(got.len(), 2, "one group per pane");
        assert_eq!(got[0].pane, "w1:p5", "p5's newest (40) beats p6's newest (30)");
        assert_eq!(
            got[0].prompts.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
            vec!["new p5", "old p5"],
            "newest first WITHIN a group"
        );
        assert_eq!(got[1].pane, "w1:p6");
        assert_eq!(
            got[1].prompts.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
            vec!["new p6", "old p6"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_for_tab_keeps_ring_per_pane_not_across_the_tab() {
        // The whole point of grouping: four agents keep 3 each, not 3 between
        // them. Twelve prompts survive where the old merge kept three.
        let dir = tempdir().join("per_pane_ring");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for (i, pane) in ["w1_p5", "w1_p6", "w1_p7", "w1_p8"].iter().enumerate() {
            let raw = pane.replace('_', ":");
            let entries: Vec<Prompt> = (0..3)
                .map(|j| Prompt {
                    ts: (i * 10 + j) as u64,
                    pane: raw.clone(),
                    agent: "claude".into(),
                    text: format!("{pane}-{j}"),
                })
                .collect();
            std::fs::write(prompts_file(&dir, "w1_t1", pane), to_json(&entries)).unwrap();
        }
        let got = load_for_tab(&dir, "w1_t1");
        assert_eq!(got.len(), 4);
        assert!(got.iter().all(|g| g.prompts.len() == RING));
        assert_eq!(got.iter().map(|g| g.prompts.len()).sum::<usize>(), 12);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_for_tab_caps_an_overlong_file_at_ring() {
        // A hand-edited or future-version file can hold more than RING.
        let dir = tempdir().join("overlong");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let entries: Vec<Prompt> = (0..10)
            .map(|j| Prompt { ts: j, pane: "w1:p5".into(), agent: "claude".into(), text: format!("p{j}") })
            .collect();
        std::fs::write(prompts_file(&dir, "w1_t1", "w1_p5"), to_json(&entries)).unwrap();
        let got = load_for_tab(&dir, "w1_t1");
        assert_eq!(got.len(), 1);
        assert_eq!(
            got[0].prompts.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(),
            vec!["p9", "p8", "p7"],
            "newest RING kept, newest first"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_for_tab_groups_by_the_pane_field_not_the_filename() {
        // Grouping reads the stored `pane`, so a file holding two panes'
        // entries still splits correctly.
        let dir = tempdir().join("by_field");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let mixed = vec![
            Prompt { ts: 1, pane: "w1:p5".into(), agent: "claude".into(), text: "from p5".into() },
            Prompt { ts: 2, pane: "w1:p6".into(), agent: "claude".into(), text: "from p6".into() },
        ];
        std::fs::write(prompts_file(&dir, "w1_t1", "w1_p5"), to_json(&mixed)).unwrap();
        let got = load_for_tab(&dir, "w1_t1");
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].pane, "w1:p6", "ts 2 is newer");
        assert_eq!(got[1].pane, "w1:p5");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_for_tab_breaks_group_ties_on_pane() {
        let dir = tempdir().join("group_ties");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for pane in ["w1_p9", "w1_p5", "w1_p7"] {
            let raw = pane.replace('_', ":");
            let e = Prompt { ts: 100, pane: raw, agent: "claude".into(), text: format!("from {pane}") };
            std::fs::write(prompts_file(&dir, "w1_t1", pane), to_json(&[e])).unwrap();
        }
        let got = load_for_tab(&dir, "w1_t1");
        assert_eq!(
            got.iter().map(|g| g.pane.as_str()).collect::<Vec<_>>(),
            vec!["w1:p5", "w1:p7", "w1:p9"],
            "equal newest ts orders by pane, not read_dir"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_for_tab_is_empty_for_a_missing_dir() {
        assert_eq!(load_for_tab(std::path::Path::new("/no/such/dir/anywhere"), "w1_t1"), vec![]);
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
        // Right prefix, wrong suffix — isolates the suffix guard.
        std::fs::write(dir.join("w1_t1__w1_p9.json"), to_json(&theirs)).unwrap();

        let got = load_for_tab(&dir, "w1_t1");
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].prompts[0].text, "mine");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_for_tab_does_not_match_a_tab_key_that_is_a_prefix_of_another() {
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

    /// Flattened view of `load_for_tab` for the gate tests below, which only
    /// ever exercise a single pane and care about `capture`'s gate chain, not
    /// grouping.
    fn captured(dir: &std::path::Path) -> Vec<Prompt> {
        load_for_tab(dir, &crate::state::id_key("w1:t1").unwrap())
            .into_iter()
            .flat_map(|g| g.prompts)
            .collect()
    }

    #[test]
    fn capture_writes_a_condensed_entry_when_every_gate_passes() {
        let dir = store_with_note("gate_pass");
        assert!(capture(Some(&dir), &env_ok(), r#"{"prompt":"fix the auth test\nsecond line"}"#, 99, None));
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
        assert!(!capture(Some(&dir), &env, r#"{"prompt":"x"}"#, 1, None));
        assert_eq!(captured(&dir), vec![]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_gate_2_outside_herdr() {
        let dir = store_with_note("gate2");
        let env = CaptureEnv { in_herdr: false, ..env_ok() };
        assert!(!capture(Some(&dir), &env, r#"{"prompt":"x"}"#, 1, None));
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
            assert!(!capture(Some(&dir), &env, r#"{"prompt":"x"}"#, 1, None));
        }
        assert_eq!(captured(&dir), vec![], "no legacy fallback: per-tab or nothing");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_gate_4_no_note_file_for_this_tab() {
        let dir = tempdir().join("gate4");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!capture(Some(&dir), &env_ok(), r#"{"prompt":"x"}"#, 1, None));
        assert_eq!(captured(&dir), vec![], "no note, no capture, no file");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_gate_5_unusable_payloads() {
        let dir = store_with_note("gate5");
        for stdin in ["", "not json", r#"{}"#, r#"{"prompt":""}"#, r#"{"prompt":"  "}"#] {
            assert!(!capture(Some(&dir), &env_ok(), stdin, 1, None), "stdin {stdin:?} must not write");
        }
        assert_eq!(captured(&dir), vec![]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_without_a_store_dir_is_a_noop() {
        assert!(!capture(None, &env_ok(), r#"{"prompt":"x"}"#, 1, None));
    }

    #[test]
    fn capture_writes_only_this_panes_file() {
        let dir = store_with_note("per_pane");
        let p5 = CaptureEnv { pane_id: Some("w1:p5".into()), ..env_ok() };
        let p6 = CaptureEnv { pane_id: Some("w1:p6".into()), ..env_ok() };
        assert!(capture(Some(&dir), &p5, r#"{"prompt":"from p5"}"#, 10, None));
        assert!(capture(Some(&dir), &p6, r#"{"prompt":"from p6"}"#, 20, None));
        assert!(prompts_file(&dir, "w1_t1", &crate::state::id_key("w1:p5").unwrap()).exists());
        assert!(prompts_file(&dir, "w1_t1", &crate::state::id_key("w1:p6").unwrap()).exists());
        let got = captured(&dir);
        assert_eq!(got.iter().map(|p| p.text.as_str()).collect::<Vec<_>>(), vec!["from p6", "from p5"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_needs_no_note_file_when_a_notes_pane_is_live() {
        // The whole point of the phase: opening Notes is enough, without
        // typing anything into the note first.
        let dir = tempdir().join("gate_live_pane");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(capture(Some(&dir), &env_ok(), r#"{"prompt":"from a live pane"}"#, 7, Some(true)));
        let got = load_for_tab(&dir, &crate::state::id_key("w1:t1").unwrap());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].prompts[0].text, "from a live pane");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_falls_back_to_the_note_file_when_no_pane_is_live() {
        // Socket answered "no live Notes pane": everything that captures today
        // must keep capturing. The answer is additive, never subtractive.
        let dir = store_with_note("gate_no_pane");
        assert!(capture(Some(&dir), &env_ok(), r#"{"prompt":"still captured"}"#, 7, Some(false)));
        assert_eq!(captured(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_rejects_with_neither_a_live_pane_nor_a_note_file() {
        let dir = tempdir().join("gate_neither");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for live in [Some(false), None] {
            assert!(!capture(Some(&dir), &env_ok(), r#"{"prompt":"x"}"#, 7, live));
        }
        assert_eq!(load_for_tab(&dir, &crate::state::id_key("w1:t1").unwrap()), vec![]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_falls_back_to_the_note_file_when_the_socket_failed() {
        let dir = store_with_note("gate_socket_down");
        assert!(capture(Some(&dir), &env_ok(), r#"{"prompt":"offline"}"#, 7, None));
        assert_eq!(captured(&dir).len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_live_pane_does_not_bypass_the_earlier_gates() {
        // The off switch, the in-herdr check and the id checks all still bind
        // regardless of what the socket says.
        let dir = tempdir().join("gate_precedence");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        for env in [
            CaptureEnv { no_capture: true, ..env_ok() },
            CaptureEnv { in_herdr: false, ..env_ok() },
            CaptureEnv { tab_id: None, ..env_ok() },
            CaptureEnv { pane_id: Some("../escape".into()), ..env_ok() },
        ] {
            assert!(!capture(Some(&dir), &env, r#"{"prompt":"x"}"#, 7, Some(true)));
        }
        // And a live pane still cannot rescue an unusable payload.
        assert!(!capture(Some(&dir), &env_ok(), "not json", 7, Some(true)));
        assert_eq!(captured(&dir), vec![], "none of the above may have written anything");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn capture_from_env_makes_no_socket_call_when_the_off_switch_is_set() {
        // The users paying for a pointless round trip would be exactly the
        // ones who opted out of this feature.
        //
        // Discrimination note (see task-3 report): on this machine, opening a
        // nonexistent named pipe fails FAST (the same reason
        // `ipc::tests::call_text_bounded_returns_promptly_when_there_is_no_socket`
        // has to tolerate either outcome), so the elapsed-time assertion below
        // cannot, by itself, distinguish "the socket call was skipped" from
        // "the socket call was attempted and failed instantly". It is kept as
        // a regression guard against anything that reintroduces
        // GATE_TIMEOUT-scale latency. The real proof the off switch is
        // checked BEFORE the call is structural: `capture_from_env` gates
        // `notes_live` behind `(!env.no_capture && ...).then_some(..).flatten()
        // .and_then(|tab| { ...call_text_bounded... })`, and `Option::and_then`
        // never invokes its closure on a `None` receiver — so when the guard
        // is false the socket-calling closure is not merely fast here, it
        // never runs at all. `HERDR_PLUGIN_STATE_DIR` is pointed at an
        // isolated temp dir so the "nothing was written" assertion is real
        // evidence about THIS run, not a coincidence of the real note store
        // never having a file for this made-up tab.
        let _guard = crate::state::ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let store = tempdir().join("off_switch_no_socket");
        let _ = std::fs::remove_dir_all(&store);
        std::fs::create_dir_all(&store).unwrap();

        let prev_no_capture = std::env::var_os("HERDR_NOTES_NO_CAPTURE");
        let prev_env = std::env::var_os("HERDR_ENV");
        let prev_plugin_dir = std::env::var_os("HERDR_PLUGIN_STATE_DIR");
        let prev_tab = std::env::var_os("HERDR_TAB_ID");
        let prev_pane = std::env::var_os("HERDR_PANE_ID");
        let prev_socket = std::env::var_os("HERDR_SOCKET_PATH");
        // SAFETY: serialized by ENV_LOCK; every var restored below.
        unsafe {
            std::env::set_var("HERDR_NOTES_NO_CAPTURE", "1");
            std::env::set_var("HERDR_ENV", "1");
            std::env::set_var("HERDR_PLUGIN_STATE_DIR", &store);
            std::env::set_var("HERDR_TAB_ID", "w1:t1");
            std::env::set_var("HERDR_PANE_ID", "w1:p5");
            // Does not exist, so if the call were (wrongly) attempted it
            // would fail rather than hang against a real, slow-to-answer
            // socket.
            std::env::set_var("HERDR_SOCKET_PATH", "herdr-notes-test-no-such-socket");
        }

        let started = std::time::Instant::now();
        let result = capture_from_env(r#"{"prompt":"x"}"#);
        let elapsed = started.elapsed();

        unsafe {
            match prev_no_capture {
                Some(v) => std::env::set_var("HERDR_NOTES_NO_CAPTURE", v),
                None => std::env::remove_var("HERDR_NOTES_NO_CAPTURE"),
            }
            match prev_env {
                Some(v) => std::env::set_var("HERDR_ENV", v),
                None => std::env::remove_var("HERDR_ENV"),
            }
            match prev_plugin_dir {
                Some(v) => std::env::set_var("HERDR_PLUGIN_STATE_DIR", v),
                None => std::env::remove_var("HERDR_PLUGIN_STATE_DIR"),
            }
            match prev_tab {
                Some(v) => std::env::set_var("HERDR_TAB_ID", v),
                None => std::env::remove_var("HERDR_TAB_ID"),
            }
            match prev_pane {
                Some(v) => std::env::set_var("HERDR_PANE_ID", v),
                None => std::env::remove_var("HERDR_PANE_ID"),
            }
            match prev_socket {
                Some(v) => std::env::set_var("HERDR_SOCKET_PATH", v),
                None => std::env::remove_var("HERDR_SOCKET_PATH"),
            }
        }

        assert!(!result, "the off switch must reject regardless of the socket");
        assert!(
            elapsed < GATE_TIMEOUT,
            "the off switch must not wait on the socket: {elapsed:?}"
        );
        assert_eq!(
            std::fs::read_dir(&store).unwrap().count(),
            0,
            "nothing should be written when the off switch is set"
        );
        let _ = std::fs::remove_dir_all(&store);
    }
}
