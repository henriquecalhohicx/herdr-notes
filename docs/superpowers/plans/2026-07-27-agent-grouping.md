# Per-Agent Grouping and Auto-Default Titles Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Group a tab's captured prompts under the agent that sent them, and give an untitled note a name derived from what the tab is actually working on.

**Architecture:** `load_for_tab` stops merge-truncating and returns one group per pane, each keeping its own `RING`. A pane index built from one best-effort `pane.list` call turns a pane id into a heading — the terminal title when it is meaningful, `claude p8` when it is not. The same index feeds a title chain (terminal title → git branch → oldest surviving prompt) that fills an untitled note once, guarded by a persisted `title_auto` flag whose missing-field default migrates existing notes for free.

**Tech Stack:** Rust 2024, ratatui + crossterm, `unicode-width`, `serde_json`, one `git` subprocess. No new dependencies.

## Global Constraints

- Phase C only. Codex capture, re-deriving a title when the terminal title changes later, grouping or per-agent counts in the overlay's dashboard rows, and pruning orphaned pane files are all **out of scope** — do not build them.
- `cargo build --release`, `cargo test`, and `cargo clippy --all-targets -- -D warnings` must all pass. `cargo build --release` fails with os error 5 while a `herdr-notes` TUI is running in a pane — close the pane first.
- Esc must NEVER exit the TUI. Only `q` quits.
- Wrap and cursor math budget by display columns (`unicode-width`), never char count.
- `render_markdown(text, width) -> Vec<Line>` must keep its exact signature and behavior.
- Socket calls are best-effort: any call, parse, or field failure collapses the whole index to `None` and every consumer falls back. The pane must work offline and never panic.
- `UserPromptSubmit` hooks must always exit 0 and never write to stdout. Nothing in this phase touches that path, but do not disturb it.
- Ring size stays 3 — per pane now, not shared across the tab.

---

## File Structure

- **Modify `src/prompts.rs`** — `PromptGroup` and the grouped `load_for_tab`. Storage and grouping stay together; nothing else in the crate knows the file layout.
- **Modify `src/app.rs`** — the pane index and label rule, the grouped renderer, the title chain, and the `r` flag semantics. This file is already large, but every piece here is a consumer of existing `App` state and splitting it out would fragment the draw path.
- **Modify `src/state.rs`** — the `title_auto` field on `Note`, its parse/serialize, and `set_title`.
- **Modify `README.md` and `CLAUDE.md`** — living-doc updates.

---

### Task 1: `PromptGroup` and the grouped `load_for_tab`

Each pane file already holds at most `RING`, so the per-pane cap costs nothing — the merge simply stops truncating. Grouping is by the `pane` FIELD on each stored prompt rather than by file, so a hand-edited file holding two panes' entries still groups correctly.

**Files:**
- Modify: `src/prompts.rs:103-122` (`load_for_tab`)
- Test: `src/prompts.rs` (the existing `mod tests`)

**Interfaces:**
- Consumes: `Prompt`, `RING`, `parse_file` (already in the module).
- Produces:
  - `pub struct PromptGroup { pub pane: String, pub prompts: Vec<Prompt> }` — `Clone, PartialEq, Eq, Debug`
  - `pub fn load_for_tab(dir: &Path, tab_key: &str) -> Vec<PromptGroup>` — one group per distinct `pane`, each newest-first and capped at `RING`; groups ordered by their newest `ts` descending, ties on `pane` ascending.

- [ ] **Step 1: Write the failing tests**

Replace the three existing `load_for_tab_*` tests in `src/prompts.rs`'s `mod tests` with these, and add the two new ones. (The old ones assert a flat `Vec<Prompt>` and cannot compile against the new signature — rewriting them is expected, not a shortcut.)

```rust
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test prompts`
Expected: FAIL to compile — `no field prompts on type Prompt` / `cannot find struct PromptGroup in this scope`.

- [ ] **Step 3: Implement the grouped load**

Replace `load_for_tab` at `src/prompts.rs:103-122` with:

```rust
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
        let newest = |g: &PromptGroup| g.prompts.first().map(|p| p.ts).unwrap_or(0);
        newest(b).cmp(&newest(a)).then_with(|| a.pane.cmp(&b.pane))
    });
    groups
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test prompts && cargo clippy --all-targets -- -D warnings`
Expected: `src/prompts.rs` tests PASS. `src/app.rs` will NOT compile yet — `refresh_prompts` and `prompt_block` still expect `Vec<Prompt>`. That is Task 3's job. To keep this task's commit green, add the minimal adaptation at the `App.prompts` assignment site: change the field's type to `Vec<crate::prompts::PromptGroup>` and, in `prompt_block`'s caller, flatten with `groups.iter().flat_map(|g| g.prompts.iter()).cloned().collect::<Vec<_>>()` so rendering is byte-identical to today. Task 3 replaces that flattening with real grouping. Say in your report that you did this and why.

- [ ] **Step 5: Commit**

```bash
git add src/prompts.rs src/app.rs
git commit -m "feat(prompts): group a tab's prompts per pane, RING each"
```

---

### Task 2: The pane index and the heading label rule

A pane id must become a human-readable label. `pane.list` is the only source, and its `terminal_title_stripped` is unreliable — a working Claude pane reads `"HM-54271 Generic Importer Config API"`, an idle one reads `"Claude Code"`, and a shell pane reads a `powershell.exe` path.

The socket fetch mirrors the existing `tab_contexts()`/`build_tab_index()` split exactly: a thin fetcher plus a pure builder that is unit-tested against captured live JSON, with no I/O.

**Files:**
- Modify: `src/app.rs` (add next to `tab_contexts`/`build_tab_index`, around `:137-200`)
- Test: `src/app.rs` (`mod tests`)

**Interfaces:**
- Consumes: the existing `fetch_array(method: &str, key: &str) -> Option<Vec<serde_json::Value>>`.
- Produces:
  - `struct PaneInfo { pub agent: String, pub tab_id: String, pub title: Option<String>, pub cwd: Option<String> }`
  - `type PaneIndex = std::collections::HashMap<String, PaneInfo>` keyed by `pane_id`
  - `fn build_pane_index(panes: &[serde_json::Value]) -> PaneIndex`
  - `fn pane_index() -> Option<PaneIndex>`
  - `fn meaningful_title(title: &str, agent: &str) -> Option<String>`
  - `fn pane_label(pane_id: &str, agent: &str, index: Option<&PaneIndex>) -> String`

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/app.rs`:

```rust
    fn pane_json(pane_id: &str, tab_id: &str, agent: Option<&str>, title: &str, cwd: &str) -> serde_json::Value {
        let mut v = serde_json::json!({
            "pane_id": pane_id,
            "tab_id": tab_id,
            "terminal_title_stripped": title,
            "cwd": cwd,
        });
        if let Some(a) = agent {
            v["agent"] = serde_json::Value::String(a.to_string());
        }
        v
    }

    #[test]
    fn build_pane_index_keeps_agent_panes_and_their_fields() {
        // Shapes captured from a live `pane.list` on herdr 0.7.4.
        let panes = vec![
            pane_json("wD:p8", "wD:t2", Some("claude"), "Claude Code", "C:\\repo"),
            pane_json("wD:pB", "wD:t2", None, "C:\\WINDOWS\\powershell.exe", "C:\\repo"),
        ];
        let idx = build_pane_index(&panes);
        let p8 = idx.get("wD:p8").expect("agent pane indexed");
        assert_eq!(p8.agent, "claude");
        assert_eq!(p8.tab_id, "wD:t2");
        assert_eq!(p8.title.as_deref(), Some("Claude Code"));
        assert_eq!(p8.cwd.as_deref(), Some("C:\\repo"));
        let shell = idx.get("wD:pB").expect("shell panes are indexed too");
        assert_eq!(shell.agent, "", "no agent reported yet");
    }

    #[test]
    fn build_pane_index_skips_items_missing_a_pane_id() {
        let panes = vec![serde_json::json!({"tab_id": "wD:t2", "agent": "claude"})];
        assert!(build_pane_index(&panes).is_empty());
    }

    #[test]
    fn meaningful_title_rejects_generic_names_and_paths() {
        assert_eq!(meaningful_title("HM-54271 Generic Importer", "claude").as_deref(), Some("HM-54271 Generic Importer"));
        assert_eq!(meaningful_title("  spaced  ", "claude").as_deref(), Some("spaced"), "trimmed");
        assert_eq!(meaningful_title("", "claude"), None);
        assert_eq!(meaningful_title("   ", "claude"), None);
        assert_eq!(meaningful_title("Claude Code", "claude"), None);
        assert_eq!(meaningful_title("claude code", "claude"), None, "case-insensitive");
        assert_eq!(meaningful_title("CLAUDE", "claude"), None);
        assert_eq!(meaningful_title("Codex", "codex"), None);
        assert_eq!(meaningful_title("C:\\WINDOWS\\powershell.exe", ""), None, "path-shaped");
        assert_eq!(meaningful_title("/usr/bin/bash", ""), None);
        assert_eq!(meaningful_title("something.exe", ""), None);
        assert_eq!(meaningful_title("SOMETHING.EXE", ""), None, "suffix is case-insensitive");
    }

    #[test]
    fn pane_label_prefers_a_meaningful_title() {
        let panes = vec![pane_json("wD:p8", "wD:t2", Some("claude"), "HM-54271 Importer", "C:\\repo")];
        let idx = build_pane_index(&panes);
        assert_eq!(pane_label("wD:p8", "claude", Some(&idx)), "HM-54271 Importer");
    }

    #[test]
    fn pane_label_falls_back_to_agent_and_pane_suffix() {
        let panes = vec![pane_json("wD:p8", "wD:t2", Some("claude"), "Claude Code", "C:\\repo")];
        let idx = build_pane_index(&panes);
        // Generic title -> fallback.
        assert_eq!(pane_label("wD:p8", "claude", Some(&idx)), "claude p8");
        // Pane closed since capture -> not in the index at all.
        assert_eq!(pane_label("wD:p9", "claude", Some(&idx)), "claude p9");
        // Socket unreachable -> no index at all.
        assert_eq!(pane_label("wD:p8", "claude", None), "claude p8");
        // A pane id with no colon still yields something.
        assert_eq!(pane_label("odd", "claude", None), "claude odd");
        // No agent recorded either.
        assert_eq!(pane_label("wD:p8", "", None), "p8");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --  build_pane_index meaningful_title pane_label`
Expected: FAIL to compile — `cannot find function build_pane_index in this scope`.

- [ ] **Step 3: Implement the index and the label rule**

Add to `src/app.rs`, immediately after `build_tab_index`:

```rust
/// What the prompt block needs to know about a pane. Built from one
/// `pane.list` call; every field is optional at the source, so a missing one
/// degrades rather than dropping the pane.
struct PaneInfo {
    /// "" when herdr has not reported an agent on this pane yet — a bare
    /// shell pane carries only `agent_status`.
    agent: String,
    tab_id: String,
    title: Option<String>,
    cwd: Option<String>,
}

type PaneIndex = std::collections::HashMap<String, PaneInfo>;

/// Titles herdr reports that name the tool rather than the work. Compared
/// case-insensitively against the trimmed title.
const GENERIC_TITLES: [&str; 4] = ["claude code", "claude", "codex", "codex cli"];

/// One `pane.list` round-trip. `None` on any call or parse failure — every
/// caller falls back, so the block works offline.
fn pane_index() -> Option<PaneIndex> {
    Some(build_pane_index(&fetch_array("pane.list", "panes")?))
}

/// Pure builder over an already-fetched `panes` array — no I/O, so it is
/// unit-tested against captured live responses. An item with no `pane_id` is
/// the only thing skipped; everything else degrades to a default.
fn build_pane_index(panes: &[serde_json::Value]) -> PaneIndex {
    let mut out = PaneIndex::new();
    for p in panes {
        let Some(pane_id) = p.get("pane_id").and_then(|v| v.as_str()) else { continue };
        out.insert(
            pane_id.to_string(),
            PaneInfo {
                agent: p.get("agent").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                tab_id: p.get("tab_id").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                title: p
                    .get("terminal_title_stripped")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                cwd: p.get("cwd").and_then(|v| v.as_str()).map(|s| s.to_string()),
            },
        );
    }
    out
}

/// A terminal title worth showing: trimmed, non-empty, not the tool naming
/// itself (`Claude Code` on an idle pane), and not a filesystem path (a bare
/// shell pane reports its `powershell.exe` path). `None` when it says nothing.
fn meaningful_title(title: &str, agent: &str) -> Option<String> {
    let t = title.trim();
    if t.is_empty() {
        return None;
    }
    let lower = t.to_ascii_lowercase();
    if GENERIC_TITLES.contains(&lower.as_str()) || lower == agent.trim().to_ascii_lowercase() {
        return None;
    }
    if t.contains('/') || t.contains('\\') || lower.ends_with(".exe") {
        return None;
    }
    Some(t.to_string())
}

/// The heading for a pane's prompt group: its terminal title when meaningful,
/// otherwise `{agent} {pane-suffix}` (`claude p8`) built from data the stored
/// prompt always carries — so a closed pane or an unreachable socket still
/// names its group.
fn pane_label(pane_id: &str, agent: &str, index: Option<&PaneIndex>) -> String {
    if let Some(info) = index.and_then(|i| i.get(pane_id))
        && let Some(title) = info.title.as_deref().and_then(|t| meaningful_title(t, &info.agent))
    {
        return title;
    }
    let suffix = pane_id.rsplit(':').next().unwrap_or(pane_id);
    if agent.trim().is_empty() {
        suffix.to_string()
    } else {
        format!("{agent} {suffix}")
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS. Clippy may flag `PaneInfo`'s unread fields — `cwd` and `tab_id` are consumed by Task 5. Add a narrowly-scoped `#[allow(dead_code)]` with a comment naming Task 5 only if the gate demands it, and report which.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "feat(notes): pane index and the heading label rule"
```

---

### Task 3: Render the grouped block

The block gains one heading per agent and drops the single `Last Prompts` line — the agent's name is the more informative heading, and two heading levels in a five-row block is noise. This matches the mockup the design was approved against.

**Files:**
- Modify: `src/app.rs` — `App.prompts` type and its refresh, `prompt_block`, and the `draw_preview`/empty-note call sites
- Test: `src/app.rs` (`mod tests`)

**Interfaces:**
- Consumes: `prompts::PromptGroup` (Task 1); `pane_index`, `pane_label`, `PaneIndex` (Task 2).
- Produces: `fn prompt_block(groups: &[(String, Vec<crate::prompts::Prompt>)], width: usize) -> Vec<Line<'static>>` — labelled groups in, rendered rows out. Pure; the label resolution happens in `refresh_prompts`.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/app.rs`. Replace the existing `prompt`/block tests' construction where they pass a flat slice — the helper below keeps them readable.

```rust
    fn group(label: &str, texts: &[&str]) -> (String, Vec<crate::prompts::Prompt>) {
        let prompts = texts
            .iter()
            .enumerate()
            .map(|(i, t)| crate::prompts::Prompt {
                ts: (100 - i) as u64,
                pane: "w1:p5".into(),
                agent: "claude".into(),
                text: (*t).into(),
            })
            .collect();
        (label.to_string(), prompts)
    }

    #[test]
    fn prompt_block_heads_each_group_with_its_label() {
        let groups = vec![
            group("HM-54271 Importer", &["add the rate limiter", "why is auth flaky"]),
            group("claude pB", &["run the migration"]),
        ];
        let rows: Vec<String> = prompt_block(&groups, 60).iter().map(line_text).collect();
        let joined = rows.join("\n");
        assert!(joined.contains("HM-54271 Importer"), "{joined}");
        assert!(joined.contains("claude pB"), "{joined}");
        assert!(joined.contains("1. add the rate limiter"), "{joined}");
        assert!(joined.contains("2. why is auth flaky"), "numbering restarts per group: {joined}");
        assert!(
            rows.iter().position(|r| r.contains("HM-54271")).unwrap()
                < rows.iter().position(|r| r.contains("claude pB")).unwrap(),
            "group order is preserved: {joined}"
        );
        assert!(!joined.contains("Last Prompts"), "the single heading is gone: {joined}");
    }

    #[test]
    fn prompt_block_is_empty_without_groups() {
        assert!(prompt_block(&[], 60).is_empty());
        assert!(prompt_block(&[("solo".into(), vec![])], 60).is_empty(), "a group with no prompts renders nothing");
    }

    #[test]
    fn prompt_block_truncates_labels_and_bodies_by_display_columns() {
        // Storage truncates by CHAR count, so a 120-char CJK prompt is ~240
        // columns; only this render-side truncation keeps it in the pane. The
        // heading is user-supplied too and gets the same treatment.
        let groups = vec![group(&"文".repeat(80), &[&"文".repeat(80)])];
        for width in [12usize, 30, 60] {
            for line in prompt_block(&groups, width) {
                let text = line_text(&line);
                assert!(dwidth(&text) <= width, "row {text:?} is {} cols, want <= {width}", dwidth(&text));
            }
        }
    }

    #[test]
    fn preview_renders_grouped_prompts_above_the_note() {
        let mut a = app("## Status\nmid-refactor");
        a.prompts = vec![crate::prompts::PromptGroup {
            pane: "w1:p5".into(),
            prompts: vec![crate::prompts::Prompt {
                ts: 2, pane: "w1:p5".into(), agent: "claude".into(), text: "add the rate limiter".into(),
            }],
        }];
        let screen = rendered(&mut a, 60, 14);
        assert!(screen.contains("add the rate limiter"), "{screen}");
        let block_at = screen.find("add the rate limiter").unwrap();
        let note_at = screen.find("mid-refactor").unwrap();
        assert!(block_at < note_at, "the block sits above the note: {screen}");
    }
```

Update every pre-existing test that assigns `a.prompts` to build `PromptGroup`s instead of bare `Prompt`s, and drop any assertion on the literal string `"Last Prompts"`. The tests that must change are `the_prompt_block_is_absent_without_prompts`, `the_prompt_block_never_shows_on_the_global_note_or_in_edit_mode`, `the_checkbox_cursor_ignores_the_prompt_block`, `the_scroll_follow_accounts_for_the_prompt_block_rows`, `long_prompts_are_truncated_to_the_pane_width`, `the_prompt_block_shows_above_the_empty_note_help`, and `prompts_load_at_construction_and_on_every_global_toggle`. Keep what each one asserts — only the fixture shape and the heading string change.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test`
Expected: FAIL to compile — `prompt_block` still takes `&[Prompt]`.

- [ ] **Step 3: Implement the grouped renderer**

Change `App.prompts`'s type to carry the resolved label alongside each group:

```rust
    /// The tab's captured prompts, grouped per agent pane and newest group
    /// first, each with the heading resolved at refresh time. Refreshed on the
    /// heartbeat rather than per draw. Rendered above the note, never part of
    /// the edit buffer.
    prompts: Vec<crate::prompts::PromptGroup>,
    /// Heading per group, index-aligned with `prompts`. Resolved from one
    /// `pane.list` call at refresh time so the draw path stays I/O-free.
    prompt_labels: Vec<String>,
```

Initialize `prompt_labels: Vec::new()` beside `prompts` in `with_note`.

Extend `refresh_prompts` (`src/app.rs:426`) to resolve labels, clearing both vectors on every early-return path:

```rust
    fn refresh_prompts(&mut self) {
        self.prompts.clear();
        self.prompt_labels.clear();
        if !self.persist || !self.showing_tab_note() {
            return;
        }
        let Some(dir) = state::store_dir() else { return };
        let Some(key) = state::tab_env().as_deref().and_then(state::id_key) else { return };
        self.prompts = crate::prompts::load_for_tab(&dir, &key);
        if self.prompts.is_empty() {
            return;
        }
        // One socket round-trip per refresh, and only when there is something
        // to label. `None` (socket unreachable) falls every group back to
        // `{agent} {pane-suffix}`.
        let index = pane_index();
        self.prompt_labels = self
            .prompts
            .iter()
            .map(|g| {
                let agent = g.prompts.first().map(|p| p.agent.as_str()).unwrap_or("");
                pane_label(&g.pane, agent, index.as_ref())
            })
            .collect();
    }
```

Replace `prompt_block` at `src/app.rs:1400-1416`:

```rust
/// The dim per-agent prompt block rendered above the note: one heading per
/// group, its prompts numbered from 1, a blank line between groups, and a
/// rule at the end. Empty groups and an empty list render nothing at all, so
/// the note keeps the space. There is deliberately no single "Last Prompts"
/// heading — the agent's own label is the more informative one, and two
/// heading levels in a five-row block is noise.
fn prompt_block(
    groups: &[(String, Vec<crate::prompts::Prompt>)],
    width: usize,
) -> Vec<Line<'static>> {
    let dim = Style::default().add_modifier(Modifier::DIM);
    let head = Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM);
    let mut out: Vec<Line<'static>> = Vec::new();
    for (label, prompts) in groups.iter().filter(|(_, p)| !p.is_empty()) {
        if !out.is_empty() {
            out.push(Line::raw(""));
        }
        out.push(Line::from(Span::styled(truncate_w(label, width), head)));
        for (i, p) in prompts.iter().enumerate() {
            // The number and its separator cost 3 columns.
            let body = truncate_w(&p.text, width.saturating_sub(3));
            out.push(Line::from(Span::styled(format!("{}. {body}", i + 1), dim)));
        }
    }
    if !out.is_empty() {
        out.push(Line::from(Span::styled("─".repeat(width), dim)));
    }
    out
}
```

At both call sites in `draw_preview` — the empty-note branch and the main branch — build the labelled pairs. Add this small helper method on `App` and call it from both:

```rust
    /// `prompts` zipped with their resolved headings, for the renderer. A
    /// group whose label is missing (labels cleared, prompts not) falls back
    /// to the raw pane id rather than dropping the group.
    fn labelled_prompts(&self) -> Vec<(String, Vec<crate::prompts::Prompt>)> {
        self.prompts
            .iter()
            .enumerate()
            .map(|(i, g)| {
                let label = self.prompt_labels.get(i).cloned().unwrap_or_else(|| g.pane.clone());
                (label, g.prompts.clone())
            })
            .collect()
    }
```

Then each call site becomes `prompt_block(&self.labelled_prompts(), text_w)`, keeping the `showing_tab_note()` gate exactly as it is.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS. If `the_scroll_follow_accounts_for_the_prompt_block_rows` breaks, the block's height changed with grouping — recompute the expected offset from `prompt_block(...).len()` as that test already does; do NOT loosen its assertion.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "feat(notes): head each prompt group with its agent"
```

---

### Task 4: `title_auto` on the note, and what `r` does to it

The flag records whether the title was derived or typed. Its missing-field default is the whole migration: an existing note WITH a title reads as manual, an existing UNTITLED note reads as auto, so current notes get titled on next open with no migration pass.

**Files:**
- Modify: `src/state.rs` — the `Note` struct, its `Default`, `parse`, `to_json`, and `set_title`
- Modify: `src/app.rs` — `on_key_title`'s Enter arm (`:506-513`)
- Test: `src/state.rs` and `src/app.rs` (`mod tests`)

**Interfaces:**
- Consumes: nothing from Tasks 1-3.
- Produces: `Note.title_auto: bool` — true when the title is derived (or absent and derivable), false when the user typed one.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/state.rs`:

```rust
    #[test]
    fn title_auto_defaults_from_whether_a_title_exists() {
        // The free migration: a v2 file with no `title_auto` reads as manual
        // when it has a title, auto when it does not.
        assert!(parse(r#"{"text":"body"}"#).title_auto, "untitled -> auto");
        assert!(!parse(r#"{"text":"body","title":"Mine"}"#).title_auto, "titled -> manual");
        assert!(parse(r#"{"text":"body","title":"  "}"#).title_auto, "whitespace title -> auto");
        // An explicit value always wins over the default.
        assert!(!parse(r#"{"title":"","title_auto":false}"#).title_auto);
        assert!(parse(r#"{"title":"Mine","title_auto":true}"#).title_auto);
    }

    #[test]
    fn title_auto_round_trips_through_to_json() {
        let mut n = Note { title: "Mine".into(), title_auto: false, ..Note::default() };
        assert!(!parse(&to_json(&n)).title_auto);
        n.title_auto = true;
        assert!(parse(&to_json(&n)).title_auto);
    }

    #[test]
    fn a_default_note_is_auto_titled() {
        assert!(Note::default().title_auto, "a fresh note has no title, so it is derivable");
    }

    #[test]
    fn set_title_marks_the_note_manual_and_clearing_marks_it_auto() {
        let dir = temp_base("set-title-auto");
        let file = dir.join("w1_t1.json");
        persist_at(&file, &Note { text: "body".into(), ..Note::default() }, "w1:t1", 100);
        set_title(&file, "Named By Hand");
        let n = read_note(&file);
        assert_eq!(n.title, "Named By Hand");
        assert!(!n.title_auto, "an overlay rename is a manual title");
        set_title(&file, "   ");
        assert!(read_note(&file).title_auto, "clearing hands it back to auto");
    }
```

Adapt `temp_base` to whatever the existing tests in that file use.

Add to `mod tests` in `src/app.rs`:

```rust
    #[test]
    fn typing_a_title_freezes_it_and_clearing_re_enables_auto() {
        let mut a = app("body");
        assert!(a.note.title_auto, "an untitled note starts derivable");
        a.on_key(key(KeyCode::Char('r')));
        for c in "HM-1".chars() {
            a.on_key(key(KeyCode::Char(c)));
        }
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.note.title, "HM-1");
        assert!(!a.note.title_auto, "a typed title is frozen");

        a.on_key(key(KeyCode::Char('r')));
        for _ in 0..8 {
            a.on_key(key(KeyCode::Backspace));
        }
        a.on_key(key(KeyCode::Enter));
        assert_eq!(a.note.title, "");
        assert!(a.note.title_auto, "clearing hands it back to auto");
    }

    #[test]
    fn escaping_the_title_editor_leaves_the_flag_alone() {
        let mut a = app("body");
        a.note.title = "Mine".into();
        a.note.title_auto = false;
        a.on_key(key(KeyCode::Char('r')));
        a.on_key(key(KeyCode::Char('x')));
        a.on_key(key(KeyCode::Esc));
        assert_eq!(a.note.title, "Mine");
        assert!(!a.note.title_auto);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test`
Expected: FAIL to compile — `no field title_auto on type Note`.

- [ ] **Step 3: Add the field and its semantics**

In `src/state.rs`, add the field to `Note` beside `title` with a doc comment:

```rust
    /// True when the title was derived rather than typed. The missing-field
    /// default is `title.trim().is_empty()`, which migrates existing files for
    /// free: one that already has a title reads as manual, an untitled one
    /// reads as derivable.
    pub title_auto: bool,
```

`Note` currently derives `Default`, which would give `title_auto: false` — wrong for a fresh untitled note. Remove `Default` from the derive list and add:

```rust
impl Default for Note {
    fn default() -> Self {
        Note {
            text: String::new(),
            mode: Mode::Preview,
            title: String::new(),
            title_auto: true,
            tab_id: String::new(),
            created: 0,
            updated: 0,
        }
    }
}
```

(Match the field list to the struct as it actually stands; do not drop a field.)

In `parse`, after `title` is read:

```rust
    let title_auto = value
        .get("title_auto")
        .and_then(|v| v.as_bool())
        .unwrap_or_else(|| title.trim().is_empty());
```

and include `title_auto` in the constructed `Note`.

In `to_json`, add `"title_auto": note.title_auto` to the object.

In `set_title`, set the flag from what was passed:

```rust
pub fn set_title(file: &Path, title: &str) {
    let mut note = read_note(file);
    note.title = title.trim().to_string();
    // A rename from the overlay is a manual title; clearing it hands the note
    // back to auto-titling.
    note.title_auto = note.title.is_empty();
    let tab_id = note.tab_id.clone();
    persist_at(file, &note, &tab_id, unix_now());
}
```

In `src/app.rs`'s `on_key_title` Enter arm (`:508-513`):

```rust
            KeyCode::Enter => {
                if let Some(buf) = self.title_input.take() {
                    self.note.title = buf.trim().to_string();
                    // Typing a title freezes it; clearing it hands the note
                    // back to auto-titling on the next heartbeat.
                    self.note.title_auto = self.note.title.is_empty();
                    self.save();
                }
            }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS. Any pre-existing test constructing `Note { .. }` literally without `..Note::default()` will need the new field — add it rather than changing what the test asserts.

- [ ] **Step 5: Commit**

```bash
git add src/state.rs src/app.rs
git commit -m "feat(notes): title_auto flag, migrating existing notes by default"
```

---

### Task 5: The title resolution chain

Fills an untitled note once, from the first source that yields something.

**Files:**
- Modify: `src/app.rs` — a `git_tried` field, `pick_title`, `maybe_autotitle`, and the `heartbeat` call
- Test: `src/app.rs` (`mod tests`)

**Interfaces:**
- Consumes: `PaneInfo`, `PaneIndex`, `pane_index`, `meaningful_title` (Task 2); `prompts::PromptGroup` (Task 1); `Note.title_auto` (Task 4).
- Produces: `fn pick_title(agent_pane: Option<&PaneInfo>, branch: Option<&str>, oldest_prompt: Option<&str>) -> Option<String>` — the pure chain, unit-tested without a socket or a subprocess.

- [ ] **Step 1: Write the failing tests**

Add to `mod tests` in `src/app.rs`:

```rust
    fn info(agent: &str, title: Option<&str>, cwd: Option<&str>) -> PaneInfo {
        PaneInfo {
            agent: agent.into(),
            tab_id: "wD:t2".into(),
            title: title.map(|s| s.to_string()),
            cwd: cwd.map(|s| s.to_string()),
        }
    }

    #[test]
    fn pick_title_prefers_a_meaningful_terminal_title() {
        let p = info("claude", Some("HM-54271 Importer"), Some("C:\\repo"));
        assert_eq!(
            pick_title(Some(&p), Some("some-branch"), Some("a prompt")).as_deref(),
            Some("HM-54271 Importer")
        );
    }

    #[test]
    fn pick_title_falls_through_to_the_branch_then_the_prompt() {
        // Generic title -> branch wins.
        let generic = info("claude", Some("Claude Code"), Some("C:\\repo"));
        assert_eq!(
            pick_title(Some(&generic), Some("20260727-team-solutions"), Some("a prompt")).as_deref(),
            Some("20260727-team-solutions")
        );
        // No branch either -> the prompt.
        assert_eq!(pick_title(Some(&generic), None, Some("a prompt")).as_deref(), Some("a prompt"));
        // Nothing at all.
        assert_eq!(pick_title(Some(&generic), None, None), None);
        // No agent pane in the tab: branch and prompt still work.
        assert_eq!(pick_title(None, Some("br"), Some("a prompt")).as_deref(), Some("br"));
        assert_eq!(pick_title(None, None, Some("a prompt")).as_deref(), Some("a prompt"));
        assert_eq!(pick_title(None, None, None), None);
    }

    #[test]
    fn pick_title_rejects_a_detached_head_and_blank_sources() {
        let generic = info("claude", Some("Claude Code"), Some("C:\\repo"));
        assert_eq!(
            pick_title(Some(&generic), Some("HEAD"), Some("a prompt")).as_deref(),
            Some("a prompt"),
            "a detached HEAD is not a name"
        );
        assert_eq!(pick_title(Some(&generic), Some("   "), Some("a prompt")).as_deref(), Some("a prompt"));
        assert_eq!(pick_title(Some(&generic), Some("br"), Some("   ")).as_deref(), Some("br"));
    }

    #[test]
    fn autotitle_only_runs_while_the_note_is_untitled_and_auto() {
        let mut a = app("body");
        a.note.title = "Mine".into();
        a.note.title_auto = false;
        a.maybe_autotitle();
        assert_eq!(a.note.title, "Mine", "a manual title is never touched");

        let mut b = app("body");
        b.note.title = "Derived".into();
        b.note.title_auto = true;
        b.maybe_autotitle();
        assert_eq!(b.note.title, "Derived", "an already-derived title is set once, not re-derived");
    }

    #[test]
    fn autotitle_uses_the_oldest_surviving_prompt() {
        // The ring holds RING, so the genuinely-first prompt is gone after
        // enough submissions — the oldest SURVIVING one is what source 3 gives.
        let mut a = app("body");
        a.prompts = vec![crate::prompts::PromptGroup {
            pane: "w1:p5".into(),
            prompts: vec![
                crate::prompts::Prompt { ts: 30, pane: "w1:p5".into(), agent: "claude".into(), text: "newest".into() },
                crate::prompts::Prompt { ts: 10, pane: "w1:p5".into(), agent: "claude".into(), text: "oldest".into() },
            ],
        }];
        assert_eq!(a.oldest_prompt_text().as_deref(), Some("oldest"));
    }

    #[test]
    fn oldest_prompt_text_spans_every_group() {
        let mut a = app("body");
        a.prompts = vec![
            crate::prompts::PromptGroup {
                pane: "w1:p5".into(),
                prompts: vec![crate::prompts::Prompt { ts: 30, pane: "w1:p5".into(), agent: "claude".into(), text: "p5".into() }],
            },
            crate::prompts::PromptGroup {
                pane: "w1:p6".into(),
                prompts: vec![crate::prompts::Prompt { ts: 5, pane: "w1:p6".into(), agent: "claude".into(), text: "p6".into() }],
            },
        ];
        assert_eq!(a.oldest_prompt_text().as_deref(), Some("p6"), "oldest across all groups");
    }

    #[test]
    fn the_git_branch_is_attempted_at_most_once_per_cwd() {
        // An unresolvable tab would otherwise spawn git every 5s forever.
        let mut a = app("body");
        let cwd = "C:\\definitely\\not\\a\\repo\\anywhere";
        assert_eq!(a.git_branch(cwd), None);
        assert!(a.git_tried.contains(cwd), "the failure is remembered");
        assert_eq!(a.git_branch(cwd), None, "second call is a no-op");
        assert_eq!(a.git_tried.len(), 1);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -- pick_title autotitle oldest_prompt the_git_branch`
Expected: FAIL to compile — `cannot find function pick_title in this scope`.

- [ ] **Step 3: Implement the chain**

Add the field to `App` beside `prompt_labels`:

```rust
    /// Cwds where `git rev-parse` has already been tried and failed. Without
    /// this, a tab that is not a repo would spawn git on every heartbeat for
    /// the life of the pane.
    git_tried: std::collections::HashSet<String>,
```

Initialize `git_tried: std::collections::HashSet::new()` in `with_note`.

Add the pure chain next to `pane_label`:

```rust
/// The title chain: the agent pane's terminal title when meaningful, then the
/// git branch, then the oldest surviving captured prompt. `None` when nothing
/// has resolved yet — the caller retries on the next heartbeat.
fn pick_title(
    agent_pane: Option<&PaneInfo>,
    branch: Option<&str>,
    oldest_prompt: Option<&str>,
) -> Option<String> {
    if let Some(p) = agent_pane
        && let Some(t) = p.title.as_deref().and_then(|t| meaningful_title(t, &p.agent))
    {
        return Some(t);
    }
    // A detached HEAD names nothing.
    if let Some(b) = branch.map(str::trim).filter(|b| !b.is_empty() && *b != "HEAD") {
        return Some(b.to_string());
    }
    oldest_prompt.map(str::trim).filter(|p| !p.is_empty()).map(|p| p.to_string())
}
```

Add the `App` methods next to `refresh_prompts`:

```rust
    /// The oldest captured prompt still on disk, across every group. The ring
    /// evicts, so this is the oldest SURVIVING prompt, not necessarily the
    /// first one ever sent.
    fn oldest_prompt_text(&self) -> Option<String> {
        self.prompts
            .iter()
            .flat_map(|g| g.prompts.iter())
            .min_by_key(|p| p.ts)
            .map(|p| p.text.clone())
    }

    /// `git rev-parse --abbrev-ref HEAD` in `cwd`, at most once per cwd for
    /// the life of this process. On Windows the child is spawned with
    /// CREATE_NO_WINDOW so a console never flashes over the TUI.
    fn git_branch(&mut self, cwd: &str) -> Option<String> {
        if !self.git_tried.insert(cwd.to_string()) {
            return None;
        }
        let mut cmd = std::process::Command::new("git");
        cmd.args(["rev-parse", "--abbrev-ref", "HEAD"]).current_dir(cwd);
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let out = cmd.output().ok()?;
        if !out.status.success() {
            return None;
        }
        let branch = String::from_utf8_lossy(&out.stdout).trim().to_string();
        (!branch.is_empty()).then_some(branch)
    }

    /// Derive a title for an untitled, auto-titled note. Runs on the
    /// heartbeat; stops for good once a title is set, because `title` is then
    /// non-empty. `title_auto` stays true — it records that the title was
    /// derived, not that one is still pending.
    fn maybe_autotitle(&mut self) {
        if !self.persist || !self.showing_tab_note() {
            return;
        }
        if !self.note.title_auto || !self.note.title.trim().is_empty() {
            return;
        }
        let Some(tab) = state::tab_env() else { return };
        let index = pane_index();
        let agent_pane = index.as_ref().and_then(|i| {
            i.values().find(|p| p.tab_id == tab && !p.agent.trim().is_empty())
        });
        let cwd = agent_pane.and_then(|p| p.cwd.clone());
        // Cloned so the immutable borrow of `index` ends before `git_branch`
        // takes `&mut self`.
        let agent_pane = agent_pane.map(|p| PaneInfo {
            agent: p.agent.clone(),
            tab_id: p.tab_id.clone(),
            title: p.title.clone(),
            cwd: p.cwd.clone(),
        });
        let branch = cwd.and_then(|c| self.git_branch(&c));
        let oldest = self.oldest_prompt_text();
        if let Some(title) = pick_title(agent_pane.as_ref(), branch.as_deref(), oldest.as_deref()) {
            self.note.title = title;
            self.touch();
        }
    }
```

Call it from `heartbeat`, immediately after `self.refresh_prompts();` — prompts must be loaded before source 3 can see them.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test && cargo clippy --all-targets -- -D warnings`
Expected: PASS. `the_git_branch_is_attempted_at_most_once_per_cwd` does spawn a real `git` on its first call against a non-existent directory; that is intended and fast. If `git` is not on PATH the call still returns `None` via `cmd.output().ok()?`, so the test holds either way.

- [ ] **Step 5: Commit**

```bash
git add src/app.rs
git commit -m "feat(notes): derive a title from the terminal title, branch, or first prompt"
```

---

### Task 6: Documentation

**Files:**
- Modify: `CLAUDE.md`, `README.md`
- Test: manual re-reading, described below

**Interfaces:**
- Consumes: everything from Tasks 1-5.
- Produces: nothing.

- [ ] **Step 1: Update CLAUDE.md**

In the `src/prompts.rs` bullet, replace the sentence describing the merge with one describing grouping: `load_for_tab` returns one `PromptGroup` per pane, each keeping its own `RING`, groups ordered by newest `ts` with ties on `pane`; grouping reads the stored `pane` field, not the filename.

In the `src/app.rs` bullet, add: the block heads each group with `pane_label` — the pane's `terminal_title_stripped` when meaningful, else `{agent} {pane-suffix}` — resolved from one best-effort `pane.list` per refresh; and an untitled note with `title_auto` derives a title from the terminal title, then the git branch, then the oldest surviving prompt.

In the `src/state.rs` bullet, add `title_auto` to the JSON field list and state the missing-field default.

Add these Gotchas:

- `terminal_title_stripped` is the ONLY human-readable per-pane string herdr exposes — there is no pane label or name field (verified against a live `pane.list` on 0.7.4, whose full key set is `agent`, `agent_session`, `agent_status`, `cwd`, `focused`, `pane_id`, `revision`, `scroll`, `tab_id`, `terminal_id`, `terminal_title`, `terminal_title_stripped`, `tokens`, `workspace_id`). It is unreliable: a working Claude pane reads `HM-54271 Generic Importer Config API`, an idle one reads `Claude Code`, a shell pane reads its `powershell.exe` path. Anything using it needs the generic-name and path-shaped rejections in `meaningful_title`.
- `title_auto`'s missing-field default is `title.trim().is_empty()`. That is the entire migration for existing notes, and inverting it would make every note the user has already named start re-deriving over the top of them.
- The auto-title git call is bounded to one attempt per cwd per process (`App.git_tried`). Without the bound, a tab that is not a repo spawns `git` every 5 seconds for the life of the pane. On Windows it is spawned with `CREATE_NO_WINDOW`, or a console flashes over the TUI on every attempt.
- Title source 3 is the oldest SURVIVING prompt, not the first one ever sent — the ring evicts after `RING` submissions.

- [ ] **Step 2: Update README.md**

In the Prompt capture section, note that prompts are grouped per agent pane with a heading, and that each pane keeps its own last 3. Add a short paragraph on auto-titles: where the title comes from, that `r` freezes it, and that `r` with an empty value hands it back.

- [ ] **Step 3: Verify every claim against the code**

Re-read each new sentence against the diff: `pane_label`, `meaningful_title`, `GENERIC_TITLES`, `PromptGroup`, `title_auto`, `git_tried`, `CREATE_NO_WINDOW`, `pick_title` must all exist with those exact names and behaviors.

- [ ] **Step 4: Commit**

```bash
git add CLAUDE.md README.md
git commit -m "docs: per-agent grouping and auto-default titles"
```

---

### Task 7: Full verification

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

**Ask the human partner before touching any live herdr pane** — they have real work running. With their go-ahead, in a tab with two Claude panes:

1. Submit prompts from both panes. The block shows two groups, each with its own heading, most-recently-active first.
2. A pane whose Claude has set a task title heads its group with that title; a fresh one reads `claude p8`.
3. Submit four prompts in one pane — that group keeps 3, the other group is untouched.
4. Open Notes in an untitled tab; within ~5s the header shows a derived title.
5. Press `r`, type a name, Enter — the header shows it and it survives the next heartbeat.
6. Press `r`, clear it, Enter — the derived title comes back on the next beat.
7. Check an existing note that already had a title: it must NOT be overwritten.

- [ ] **Step 3: Report**

Record which steps passed and any that could not be run, and why. Do not report the feature verified on the strength of the unit tests alone.

---

## Self-Review

**Spec coverage:**

| Spec section | Task |
|---|---|
| 3 per pane, grouped; group ordering and tie-break | 1 |
| Grouping by the `pane` field | 1 |
| Heading rule incl. every rejection case | 2 |
| Best-effort `pane.list`, fallback to `{agent} {pane-suffix}` | 2, 3 |
| Rendering the grouped block | 3 |
| `title_auto` field and its free migration | 4 |
| `r` text/empty semantics, incl. the overlay rename | 4 |
| Title chain, order, and the detached-HEAD rejection | 5 |
| Oldest *surviving* prompt as source 3 | 5 |
| One git attempt per cwd | 5 |
| Failure modes (socket, not-a-repo, no prompts, no agent pane) | 2, 5 |
| Testing: every listed case | 1-5 |
| End-to-end against a real multi-agent tab | 7 |

No gaps. One deliberate deviation: the block drops the single `Last Prompts` heading, matching the mockup the design was approved against — recorded in Task 3's rationale.

**Placeholder scan:** none — every step carries its code or exact commands.

**Type consistency:** `PromptGroup { pane: String, prompts: Vec<Prompt> }` is produced by `load_for_tab` (Task 1) and consumed by `App.prompts`, `labelled_prompts`, and `oldest_prompt_text` (Tasks 3, 5). `prompt_block` takes `&[(String, Vec<Prompt>)]` at both call sites. `PaneInfo { agent, tab_id, title, cwd }` is built in Task 2 and read by `pick_title` and `maybe_autotitle` in Task 5. `pane_label(&str, &str, Option<&PaneIndex>) -> String` and `meaningful_title(&str, &str) -> Option<String>` keep their signatures across Tasks 2, 3, and 5. `Note.title_auto: bool` is written by `set_title`, `on_key_title`, and `parse`, and read by `maybe_autotitle`.
