# Prompt Capture Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Capture the last 3 prompts submitted in each Claude pane of a herdr tab and render them above that tab's note, so returning to a tab after an hour shows what you last asked.

**Architecture:** A Claude Code `UserPromptSubmit` hook pipes its JSON payload into `herdr-notes --capture-prompt`, which — after a five-gate check that always exits 0 and never prints — appends one condensed entry to `<tab>__<pane>.prompts.json`, its own file, atomically. The TUI globs the tab's pane files on the existing 5s heartbeat, merges them newest-first, and prepends the block to the preview's rendered rows with `None` provenance so the phase A checkbox cursor can never land on them.

**Tech Stack:** Rust 2024, ratatui + crossterm, `unicode-width`, `serde_json`. No new dependencies. One PowerShell install script.

## Global Constraints

- Phase B only. Per-agent grouping of rendered prompts, auto-default titles, Codex capture, expanding a truncated prompt, and pruning orphaned pane files are all **out of scope** — do not build them.
- `cargo build --release`, `cargo test`, and `cargo clippy --all-targets -- -D warnings` must all pass. `cargo build --release` fails with os error 5 while a `herdr-notes` TUI is running in a pane — close the pane first.
- **The capture path must always exit 0.** A non-zero exit from a `UserPromptSubmit` hook can block the user's prompt from being sent.
- **The capture path must never write to stdout.** Whatever a `UserPromptSubmit` hook prints on stdout is injected into the prompt as context.
- Esc must NEVER exit the TUI. Only `q` quits.
- Wrap and cursor math budget by display columns (`unicode-width`), never char count.
- Every stdin path strips a leading `\u{feff}` — PowerShell 5.1 prepends a UTF-8 BOM when piping into a native exe.
- Ring size is 3; each stored prompt is its first line truncated to 120 characters. Stored text equals displayed text.
- `render_markdown(text, width) -> Vec<Line>` must keep its exact signature and behavior.

---

## File Structure

- **Create `src/prompts.rs`** — the whole subsystem: the `Prompt` type, condensing, payload and file parsing, path building, append, and the merge read. Pure functions taking an injected directory, so no test touches the real store.
- **Create `scripts/install-prompt-hook.ps1`** — idempotent merge of the hook entry into the user's global Claude Code settings, with a backup.
- **Modify `src/state.rs`** — extract the id sanitizer and the atomic writer so `prompts.rs` reuses them instead of growing a second copy; teach `list_notes` to skip `*.prompts.json`.
- **Modify `src/main.rs`** — one `mod prompts;` line and the `--capture-prompt` arm.
- **Modify `src/app.rs`** — hold the merged prompts, refresh them on the heartbeat, render the block.
- **Modify `README.md` and `CLAUDE.md`** — install instructions and living-doc updates.

---

### Task 1: Shared sanitizer, shared atomic writer, and the `list_notes` skip

Three small changes in `state.rs` that everything downstream leans on. They ship together because each one alone is too small for its own review gate, and all three exist only to serve `prompts.rs`.

`note_key` already sanitizes a herdr id into a filename-safe key, but its name and docs say "the note-FILE identity of a tab id". Pane ids have the identical shape (`wA:p5`), so the sanitizer is extracted and `note_key` becomes a caller — prompts get the same rules without pretending a pane id is a tab id.

`persist_at` already does the temp + `sync_all` + rename dance. Extracting it stops `prompts.rs` from growing a second, subtly different copy.

The `list_notes` skip is the trap the spec called out: `list_notes` filters on extension `json`, and `wA_t1__wA_p5.prompts.json` HAS extension `json`. Without the skip every prompt file becomes a junk row in the notes overlay.

**Files:**
- Modify: `src/state.rs` (`note_key` at :128-134, `list_notes` at :261-280, `persist_at` at :422-445)
- Test: `src/state.rs` (the existing `mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub fn id_key(id: &str) -> Option<String>` — a herdr id sanitized to a filename-safe key (`:` → `_`, ASCII-lowercased on Windows), or `None` when the id is unsafe.
  - `pub(crate) fn write_atomic(path: &Path, contents: &str) -> bool` — creates the parent dir, writes to a `.tmp` sibling, `sync_all`s, renames. `true` on success.
  - `list_notes` unchanged in signature; now skips `*.prompts.json`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/state.rs`:

```rust
    #[test]
    fn id_key_sanitizes_pane_and_tab_ids_alike() {
        assert_eq!(id_key("wA:t1").as_deref(), Some(if cfg!(windows) { "wa_t1" } else { "wA_t1" }));
        assert_eq!(id_key("wA:p5").as_deref(), Some(if cfg!(windows) { "wa_p5" } else { "wA_p5" }));
        assert_eq!(id_key(""), None);
        assert_eq!(id_key("has space"), None);
        assert_eq!(id_key("../escape"), None);
        assert_eq!(id_key("under_score"), None, "herdr ids never contain _; it is our separator");
    }

    #[test]
    fn note_key_still_routes_through_id_key() {
        assert_eq!(note_key(Some("w1:t2")), id_key("w1:t2"));
        assert_eq!(note_key(Some("bad id")), None);
        assert_eq!(note_key(None), None);
    }

    #[test]
    fn write_atomic_creates_the_dir_and_leaves_no_temp_behind() {
        let dir = tempdir();
        let path = dir.path().join("nested").join("thing.json");
        assert!(write_atomic(&path, "{\"a\":1}"));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{\"a\":1}");
        assert!(!path.with_extension("json.tmp").exists(), "temp file must be renamed away");
    }

    #[test]
    fn list_notes_skips_prompt_files() {
        let dir = tempdir();
        std::fs::write(dir.path().join("w1_t1.json"), r#"{"text":"real note"}"#).unwrap();
        std::fs::write(dir.path().join("w1_t1__w1_p5.prompts.json"), r#"{"prompts":[]}"#).unwrap();
        let rows = list_notes(dir.path());
        assert_eq!(rows.len(), 1, "the prompts file must not become a note row: {rows:?}");
        assert_eq!(rows[0].text, "real note");
    }
```

If `mod tests` has no `tempdir()` helper, use whatever temp-dir helper the existing tests already use — read the top of `mod tests` and match it rather than introducing a new dependency.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test state`
Expected: FAIL — `cannot find function id_key in this scope` (and `write_atomic`), plus `list_notes_skips_prompt_files` failing on `rows.len() == 2`.

- [ ] **Step 3: Extract the two helpers and add the skip**

Replace `note_key` at `src/state.rs:128-134` with the extracted pair:

```rust
/// A herdr id (`<a>:<n>`, e.g. `w6:t1` or `wA:p5`) sanitized into a
/// filename-safe key: the single `:` becomes `_` (herdr ids never contain
/// `_`, so no collision), and on Windows ASCII case is folded because NTFS
/// filenames are case-insensitive ("W6_T1" and "w6_t1" are one file).
/// `None` when the id is empty or holds anything beyond alphanumerics and
/// that one `:`. Shared by note files and prompt files so the two layouts
/// can never disagree about what a given id spells on disk.
pub fn id_key(id: &str) -> Option<String> {
    if !is_filename_safe(id) {
        return None;
    }
    let key = id.replace(':', "_");
    #[cfg(windows)]
    let key = key.to_ascii_lowercase();
    Some(key)
}

/// The note-FILE identity of a tab id: `Some(key)` when the id gets its own
/// per-tab file, `None` when it falls back to the shared legacy `notes.json`.
/// Panes whose keys are EQUAL load and save the SAME file. This is the identity
/// the launcher's duplicate-instance guard (launch.rs) compares — never raw tab
/// ids — so the guard can't drift from the on-disk layout.
pub fn note_key(tab_id: Option<&str>) -> Option<String> {
    tab_id.and_then(id_key)
}
```

Add the atomic writer next to `persist_at`:

```rust
/// Atomic best-effort write: create the parent dir, write a `.tmp` sibling,
/// `sync_all`, rename over the target. `true` when the rename landed. Shared
/// by note and prompt files so both get the same crash behavior.
pub(crate) fn write_atomic(path: &Path, contents: &str) -> bool {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_extension("json.tmp");
    let written = std::fs::File::create(&tmp).and_then(|mut f| {
        use std::io::Write;
        f.write_all(contents.as_bytes())?;
        f.sync_all()
    });
    written.is_ok() && std::fs::rename(&tmp, path).is_ok()
}
```

Rewrite the tail of `persist_at` to use it, replacing everything from `if let Some(dir) = path.parent()` to the end of the function:

```rust
    write_atomic(path, &to_json(&out));
}
```

In `list_notes` at `src/state.rs:261-280`, add the skip immediately after the extension check:

```rust
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        // `<tab>__<pane>.prompts.json` also ends in `.json`; without this it
        // would list as a note and fill the overlay with junk rows.
        if path.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.ends_with(".prompts.json")) {
            continue;
        }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS, no warnings. The pre-existing `persist_at` tests must be green without being edited — if one broke, the extraction changed behavior.

- [ ] **Step 5: Commit**

```bash
git add src/state.rs
git commit -m "refactor(state): share the id sanitizer and atomic writer, skip prompt files in list_notes"
```

---

### Task 2: The prompts module — types, condensing, parsing, append

The storage layer. Pure functions over an injected directory, so no test touches the real store dir.

**Files:**
- Create: `src/prompts.rs`
- Modify: `src/main.rs:8-13` (module list)
- Test: `src/prompts.rs` (a `mod tests` at the bottom, matching the other modules)

**Interfaces:**
- Consumes: `state::id_key(&str) -> Option<String>`, `state::write_atomic(&Path, &str) -> bool` (Task 1).
- Produces:
  - `pub const RING: usize = 3;` and `pub const MAX_CHARS: usize = 120;`
  - `pub struct Prompt { pub ts: u64, pub pane: String, pub agent: String, pub text: String }` — `Clone, PartialEq, Eq, Debug`
  - `pub fn condense(prompt: &str) -> String`
  - `pub fn payload_prompt(json: &str) -> Option<String>`
  - `pub fn prompts_file(dir: &Path, tab_key: &str, pane_key: &str) -> PathBuf`
  - `pub fn parse_file(json: &str) -> Vec<Prompt>`
  - `pub fn to_json(prompts: &[Prompt]) -> String`
  - `pub fn append_at(path: &Path, entry: Prompt)`

- [ ] **Step 1: Write the failing tests**

Create `src/prompts.rs` containing ONLY the test module for now, so the tests compile against nothing and fail loudly:

```rust
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
```

- [ ] **Step 2: Run the tests to verify they fail**

First add `mod prompts;` to `src/main.rs`'s module list, keeping it alphabetical:

```rust
mod markdown;
mod prompts;
mod state;
```

Run: `cargo test prompts`
Expected: FAIL to compile — `cannot find function condense in this scope`, and the same for every other name.

- [ ] **Step 3: Write the implementation above the test module**

Put this at the top of `src/prompts.rs`, before the `#[cfg(test)] mod tests`:

```rust
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
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test prompts && cargo clippy --all-targets -- -D warnings`
Expected: PASS, no warnings. Clippy may report `dead_code` on items nothing calls yet — add a narrowly-scoped `#[allow(dead_code)]` per item if and only if the gate demands it, and note in your report which ones, so the task that adds the call site can remove them.

- [ ] **Step 5: Commit**

```bash
git add src/prompts.rs src/main.rs
git commit -m "feat(prompts): per-pane prompt ring storage"
```

---

### Task 3: Merge the tab's pane files on read

The reader half. A tab with four agent panes has four prompt files; the pane shows the newest 3 across all of them.

**Files:**
- Modify: `src/prompts.rs`
- Test: `src/prompts.rs` (`mod tests`)

**Interfaces:**
- Consumes: `parse_file`, `Prompt`, `RING` (Task 2).
- Produces: `pub fn load_for_tab(dir: &Path, tab_key: &str) -> Vec<Prompt>` — every `<tab_key>__*.prompts.json` in `dir`, merged, sorted newest-first by `ts`, truncated to `RING`. Empty vec when the dir is unreadable or holds none.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/prompts.rs`:

```rust
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test prompts`
Expected: FAIL — `cannot find function load_for_tab in this scope`.

- [ ] **Step 3: Implement the merge**

Add to `src/prompts.rs`, below `append_at`:

```rust
/// Every pane file belonging to `tab_key`, merged and sorted newest-first,
/// capped at `RING`. The `__` in the filename is what keeps `w1_t1` from
/// matching `w1_t10`'s files — the separator is part of the prefix.
/// Best-effort: an unreadable dir or file contributes nothing.
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
    out.sort_by_key(|p| std::cmp::Reverse(p.ts));
    out.truncate(RING);
    out
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS, no warnings.

- [ ] **Step 5: Commit**

```bash
git add src/prompts.rs
git commit -m "feat(prompts): merge a tab's pane files newest-first"
```

---

### Task 4: The capture entry point and its gate chain

The hook's target. This is the task where the two hard safety rules live, so treat them as the deliverable, not as trimming.

**Files:**
- Modify: `src/prompts.rs` (add the gate chain)
- Modify: `src/main.rs:21-40` (the arg match)
- Test: `src/prompts.rs` (`mod tests`)

**Interfaces:**
- Consumes: `condense`, `payload_prompt`, `prompts_file`, `append_at`, `Prompt` (Task 2); `state::id_key` (Task 1).
- Produces:
  - `pub struct CaptureEnv { pub no_capture: bool, pub in_herdr: bool, pub tab_id: Option<String>, pub pane_id: Option<String> }`
  - `pub fn capture(dir: Option<&Path>, env: &CaptureEnv, stdin: &str, now: u64) -> bool` — runs the gate chain and appends on success. `true` only when an entry was written. Never panics, never prints.
  - `pub fn capture_from_env(stdin: &str) -> bool` — reads the real environment and store dir, then calls `capture`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/prompts.rs`:

```rust
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test prompts`
Expected: FAIL — `cannot find struct CaptureEnv in this scope`.

- [ ] **Step 3: Implement the gate chain and wire up main**

Add to `src/prompts.rs`, below `load_for_tab`:

```rust
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
    // would leave a file behind for a tab that never wanted one.
    if !dir.join(format!("{tab_key}.json")).exists() {
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
```

In `src/main.rs`, add the arm BEFORE the `Some(other)` catch-all:

```rust
        // A UserPromptSubmit hook. Two hard rules, both from how that hook
        // works: ALWAYS exit 0, because a non-zero exit can block the user's
        // prompt from being sent; and NEVER write to stdout, because whatever
        // this prints is injected into that prompt as context. So the return
        // value is deliberately discarded and nothing is printed.
        Some("--capture-prompt") => {
            let stdin = read_stdin().unwrap_or_default();
            let _ = prompts::capture_from_env(&stdin);
            return Ok(());
        }
```

Update the usage line in the catch-all arm:

```rust
            eprintln!("usage: herdr-notes [--launch-decision|--focused-pane|--open-plan|--capture-prompt]");
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS, no warnings.

- [ ] **Step 5: Verify the two safety rules by hand**

The rules are about process behavior, which the unit tests cannot see. Run these against the debug binary and paste the output into your report:

```bash
cargo build
echo '{"prompt":"hello"}' | ./target/debug/herdr-notes --capture-prompt; echo "exit=$?"
echo 'not json at all' | ./target/debug/herdr-notes --capture-prompt; echo "exit=$?"
printf '' | ./target/debug/herdr-notes --capture-prompt; echo "exit=$?"
```

Expected, all three: `exit=0` and NOTHING printed on stdout before it. (These run outside herdr, so gate 2 rejects them — that is the point: even the rejection path must be silent and zero.)

- [ ] **Step 6: Commit**

```bash
git add src/prompts.rs src/main.rs
git commit -m "feat(prompts): --capture-prompt hook mode with a silent gate chain"
```

---

### Task 5: Render the prompt block above the note

**Files:**
- Modify: `src/app.rs` — `App` fields, `with_note`, `heartbeat`, `draw_preview`
- Test: `src/app.rs` (`mod tests`)

**Interfaces:**
- Consumes: `prompts::{Prompt, load_for_tab}` (Tasks 2-3); `state::{store_dir, tab_env, id_key}`.
- Produces: nothing consumed by later tasks.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/app.rs`:

```rust
    fn prompt(ts: u64, text: &str) -> crate::prompts::Prompt {
        crate::prompts::Prompt { ts, pane: "w1:p5".into(), agent: "claude".into(), text: text.into() }
    }

    #[test]
    fn preview_renders_the_prompt_block_above_the_note() {
        let mut a = app("## Status\nmid-refactor");
        a.prompts = vec![prompt(2, "add the rate limiter"), prompt(1, "why is auth flaky")];
        let screen = rendered(&mut a, 60, 14);
        assert!(screen.contains("Last Prompts"), "{screen}");
        assert!(screen.contains("add the rate limiter"), "{screen}");
        let block_at = screen.find("add the rate limiter").unwrap();
        let note_at = screen.find("mid-refactor").unwrap();
        assert!(block_at < note_at, "the block sits above the note: {screen}");
    }

    #[test]
    fn the_prompt_block_is_absent_without_prompts() {
        let mut a = app("## Status\nmid-refactor");
        assert!(!rendered(&mut a, 60, 14).contains("Last Prompts"));
    }

    #[test]
    fn the_prompt_block_never_shows_on_the_global_note_or_in_edit_mode() {
        let mut a = app("## Status\nmid-refactor");
        a.prompts = vec![prompt(1, "add the rate limiter")];
        a.active = ActiveNote::Global;
        assert!(!rendered(&mut a, 60, 14).contains("Last Prompts"), "global is not a tab");

        let mut b = app("## Status\nmid-refactor");
        b.prompts = vec![prompt(1, "add the rate limiter")];
        b.on_key(key(KeyCode::Char('e')));
        assert!(!rendered(&mut b, 60, 14).contains("Last Prompts"), "the edit buffer is yours alone");
    }

    #[test]
    fn the_checkbox_cursor_ignores_the_prompt_block() {
        // The block's rows carry None in the provenance map, so j/k and the
        // highlight must still resolve to the note's own checkbox lines.
        let mut a = app("[ ] first\n[ ] second");
        a.prompts = vec![prompt(2, "add the rate limiter"), prompt(1, "why is auth flaky")];
        let _ = rendered(&mut a, 60, 14);
        a.on_key(key(KeyCode::Char('j')));
        assert_eq!(a.box_cursor, Some(0));
        a.on_key(key(KeyCode::Char(' ')));
        assert_eq!(a.note.text, "[x] first\n[ ] second", "space hit the note's first box, not a prompt row");
        let _ = rendered(&mut a, 60, 14);
    }

    #[test]
    fn long_prompts_are_truncated_to_the_pane_width() {
        let mut a = app("## Status\nmid-refactor");
        a.prompts = vec![prompt(1, &"z".repeat(200))];
        let screen = rendered(&mut a, 30, 14);
        for line in screen.lines() {
            assert!(dwidth(line.trim_end()) <= 30, "row overflows the pane: {line:?}");
        }
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --  preview_renders_the_prompt_block the_prompt_block the_checkbox_cursor_ignores long_prompts_are_truncated`
Expected: FAIL — `no field prompts on type App`.

- [ ] **Step 3: Hold, refresh, and render the block**

Add the field to `App`, after `box_cursor`/`follow_box`:

```rust
    /// The tab's captured prompts, newest first — merged from every agent
    /// pane's file by `prompts::load_for_tab`, refreshed on the heartbeat
    /// rather than per draw (a directory scan every 500ms frame is waste).
    /// Rendered above the note, never part of the edit buffer.
    prompts: Vec<crate::prompts::Prompt>,
```

Initialize it in `with_note` beside the other new fields:

```rust
            prompts: Vec::new(),
```

Refresh inside `heartbeat`, which already self-throttles to 5s — add after `self.report_tokens();`:

```rust
        self.refresh_prompts();
```

Add the refresh method next to `report_tokens`:

```rust
    /// Re-read the tab's prompt files. Only the tab note has prompts — the
    /// global note is not a tab. Gated on `persist` so unit tests never touch
    /// the real store dir.
    fn refresh_prompts(&mut self) {
        if !self.persist || !self.showing_tab_note() {
            self.prompts.clear();
            return;
        }
        let Some(dir) = state::store_dir() else { return };
        let Some(key) = state::tab_env().as_deref().and_then(state::id_key) else { return };
        self.prompts = crate::prompts::load_for_tab(&dir, &key);
    }
```

Add the block builder near `format_row`:

```rust
/// The dim `Last Prompts` block rendered above the note: a heading, up to
/// `RING` numbered entries truncated to the pane width, and a rule. Returns
/// no rows at all when there is nothing to show, so the note keeps the space.
fn prompt_block(prompts: &[crate::prompts::Prompt], width: usize) -> Vec<Line<'static>> {
    if prompts.is_empty() {
        return Vec::new();
    }
    let dim = Style::default().add_modifier(Modifier::DIM);
    let mut out = vec![Line::from(Span::styled(
        "Last Prompts",
        Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
    ))];
    for (i, p) in prompts.iter().enumerate() {
        // The number and its separator cost 3 columns.
        let body = truncate_w(&p.text, width.saturating_sub(3));
        out.push(Line::from(Span::styled(format!("{}. {body}", i + 1), dim)));
    }
    out.push(Line::from(Span::styled("─".repeat(width), dim)));
    out
}
```

In `draw_preview`, immediately after the `render_markdown_mapped` call and before `total` is computed, prepend the block and pad the map:

```rust
        let (mut lines, mut map) = render_markdown_mapped(&self.note.text, text_w);
        // The block's rows map to NO source line, so the checkbox cursor can
        // never land on one and the highlight/scroll-follow keep pointing at
        // real note lines. Edit mode never reaches here, and the global note
        // has no prompts (refresh_prompts clears them).
        let block = prompt_block(&self.prompts, text_w);
        if !block.is_empty() {
            let n = block.len();
            let mut merged = block;
            merged.append(&mut lines);
            lines = merged;
            let mut merged_map = vec![None; n];
            merged_map.append(&mut map);
            map = merged_map;
        }
        let total = lines.len();
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS, no warnings. If `cursor_scrolls_itself_into_view` or `manual_scrolling_survives_a_live_checkbox_cursor` broke, the map padding is wrong — those tests set no prompts, so the block must be empty and both vectors untouched.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "feat(notes): render captured prompts above the note"
```

---

### Task 6: Hook installation and documentation

**Files:**
- Create: `scripts/install-prompt-hook.ps1`
- Modify: `README.md`, `CLAUDE.md`
- Test: manual, described below

**Interfaces:**
- Consumes: the `--capture-prompt` arg (Task 4).
- Produces: nothing.

- [ ] **Step 1: Write the install script**

Create `scripts/install-prompt-hook.ps1`:

```powershell
# Registers herdr-notes' prompt-capture hook in the user's GLOBAL Claude Code
# settings (~/.claude/settings.json). Idempotent: re-running replaces this
# plugin's entry and leaves every other hook alone. Backs up first.
#
# Uninstall: pass -Remove.

param([switch]$Remove)

$ErrorActionPreference = 'Stop'

$exe = Join-Path (Split-Path -Parent $PSScriptRoot) 'target\release\herdr-notes.exe'
if (-not $Remove -and -not (Test-Path $exe)) {
    Write-Error "herdr-notes.exe not found at $exe - run 'cargo build --release' first."
}

$settingsPath = Join-Path $env:USERPROFILE '.claude\settings.json'
if (-not (Test-Path $settingsPath)) {
    Write-Error "No Claude Code settings at $settingsPath."
}

$backup = "$settingsPath.herdr-notes.bak"
Copy-Item $settingsPath $backup -Force
Write-Host "Backed up to $backup"

$settings = Get-Content $settingsPath -Raw -Encoding UTF8 | ConvertFrom-Json
if (-not $settings.hooks) {
    $settings | Add-Member -NotePropertyName hooks -NotePropertyValue ([pscustomobject]@{}) -Force
}

# Drop any previous herdr-notes entry so re-running never stacks duplicates.
$existing = @()
if ($settings.hooks.UserPromptSubmit) {
    $existing = @($settings.hooks.UserPromptSubmit | Where-Object {
        -not ($_.hooks | Where-Object { $_.command -like '*herdr-notes*--capture-prompt*' })
    })
}

if (-not $Remove) {
    $entry = [pscustomobject]@{
        hooks = @([pscustomobject]@{
            type    = 'command'
            command = "`"$exe`" --capture-prompt"
            timeout = 5
        })
    }
    $existing = @($existing) + $entry
}

$settings.hooks | Add-Member -NotePropertyName UserPromptSubmit -NotePropertyValue @($existing) -Force
$settings | ConvertTo-Json -Depth 20 | Set-Content $settingsPath -Encoding UTF8

if ($Remove) { Write-Host "Removed the herdr-notes prompt hook." }
else { Write-Host "Installed. Restart any running Claude Code session to pick it up." }
```

- [ ] **Step 2: Verify the script is idempotent and reversible**

Against a COPY of your settings, never the real file:

```bash
cp ~/.claude/settings.json /tmp/settings-probe.json
```

Then run the script twice with `$env:USERPROFILE` pointed at a scratch dir holding that copy at `.claude/settings.json`, and confirm: after two runs there is exactly ONE `herdr-notes` entry, other hooks are untouched, and `-Remove` leaves the file with none. Paste the before/after `UserPromptSubmit` arrays into your report.

- [ ] **Step 3: Document it**

Add to `README.md`, in a `## Prompt capture` section: what it does, the one-line install (`pwsh scripts/install-prompt-hook.ps1`), the manual JSON snippet for people who would rather not run a script against their global settings, the `HERDR_NOTES_NO_CAPTURE=1` off switch, and the three known limits verbatim from the spec (Codex panes capture nothing; the note must exist first; pane files orphan).

Add to `CLAUDE.md`: a `src/prompts.rs` bullet in the Layout section describing per-pane files, the ring, and the merge-on-read; and Gotchas for
(a) `UserPromptSubmit` hooks must exit 0 and print nothing, because a non-zero exit blocks the user's prompt and stdout is injected into it;
(b) `list_notes` filters on extension `json` and `*.prompts.json` matches, so prompt files need an explicit skip or they become junk overlay rows;
(c) one file per pane rather than per tab, because a tab can hold several agent panes and a shared file would mean concurrent read-modify-write from independent hook processes.

Match CLAUDE.md's voice — terse, specific, present tense.

- [ ] **Step 4: Verify the docs match the code**

Re-read each claim against the diff: paths, flag names, env var names, and the ring size must all exist as written.

- [ ] **Step 5: Commit**

```bash
git add scripts/install-prompt-hook.ps1 README.md CLAUDE.md
git commit -m "docs: prompt-capture install script and living-doc updates"
```

---

### Task 7: End-to-end verification against a real Claude pane

The hook contract cannot be unit-tested. This is the only step that proves the feature works.

**Files:** none — verification only.

**Interfaces:**
- Consumes: everything from Tasks 1-6.
- Produces: nothing.

- [ ] **Step 1: Run the full gate**

Close any open Notes pane first (`cargo build --release` fails with os error 5 while the TUI holds the binary), then:

```bash
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
```

- [ ] **Step 2: Drive it end to end**

**Ask the human partner before touching any live herdr pane** — they may have real work running. With their go-ahead:

1. Install the hook (`pwsh scripts/install-prompt-hook.ps1`) and restart a Claude Code session inside a herdr pane.
2. In a tab with NO Notes pane, submit a prompt. Confirm the store dir gains no `.prompts.json` file — gate 4.
3. Open Notes in that tab, write a Status line so a note file exists, submit another prompt. Within ~5s the pane shows a `Last Prompts` block with that prompt's first line.
4. Confirm the note's own text is unchanged and `e` shows only your text — no prompt lines in the buffer.
5. `l` — confirm the overlay lists ONE row for the tab, not a second junk row for the prompts file.
6. Submit four more prompts; confirm only the newest 3 remain.
7. Set `HERDR_NOTES_NO_CAPTURE=1` in a pane, submit a prompt, confirm nothing is appended.
8. With two Claude panes in one tab, submit from each; confirm two separate `.prompts.json` files and a merged, newest-first block.

- [ ] **Step 3: Report**

Record which steps passed, and any that could not be run and why. Do not report the feature verified on the strength of the unit tests alone.

---

## Self-Review

**Spec coverage:**

| Spec section | Task |
|---|---|
| Writer: `--capture-prompt` stdin mode, BOM strip | 4 |
| Gate chain, all five gates | 4 |
| Always exit 0, never write stdout | 4 (step 5 verifies by hand) |
| Storage: `<tab>__<pane>.prompts.json`, ring of 3, atomic, forgiving parse | 1 (atomic writer), 2 |
| `pane`/`agent` recorded for phase C | 2, 4 |
| `list_notes` skips `*.prompts.json` | 1 |
| Reader: glob, merge newest-first, 5s heartbeat refresh | 3, 5 |
| Render prepended with `None` provenance | 5 |
| Not in edit mode / overlay preview / global note | 5 |
| Install script + README snippet, `timeout: 5` | 6 |
| Known limits documented | 6 |
| Testing: every listed case | 1-5 |
| End-to-end check against a real Claude pane | 7 |

One spec line has no task by design: "not rendered in the overlay's read-only preview of another tab's note" needs no code — that path calls `render_markdown` on the other note's text and never consults `App.prompts`. Task 5's tests cover the two paths that could regress (global note, edit mode).

**Placeholder scan:** none — every step carries the code or the exact commands it needs.

**Type consistency:** `Prompt { ts: u64, pane: String, agent: String, text: String }` is constructed identically in `capture` (Task 4) and both test helpers. `load_for_tab(&Path, &str) -> Vec<Prompt>` is consumed by `refresh_prompts` (Task 5) and by the `captured()` test helper (Task 4). `id_key(&str) -> Option<String>` (Task 1) is called by `capture`, `refresh_prompts`, and the tests. `write_atomic(&Path, &str) -> bool` (Task 1) is called by `append_at` (Task 2) and `persist_at`. `prompts_file(&Path, &str, &str) -> PathBuf` takes KEYS, never raw ids — every call site sanitizes first.
