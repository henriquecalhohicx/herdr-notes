# Handoff — notes-v2 build

**For the next session.** Everything you need to resume is here.

## Where things stand

- Repo: `C:\git-repositories\4-poc\herdr-notes` (a single herdr plugin, Rust).
- Git remotes: `origin` = **github.com/henriquecalhohicx/herdr-notes** (the
  user's fork — push here), `upstream` = alexarthurs/herdr-notes. Default branch
  `main`.
- **You are on branch `notes-v2`** (branched from `main` @ 55e8e15). The v2 spec
  lives here; no v2 code yet.
- `main` already contains: per-tab notes (keyed by `HERDR_TAB_ID`, `:`→`_`), and
  the notes-manager (title/timestamps, `l` list overlay with preview/rename/
  delete, `r` title, `live`/`closed` status, empty-note deletion) + overlay
  readability fixes (header `[preview] — title`, split top/bottom hint borders,
  tall preview). All shipped + pushed.
- Gates on `main`: 48 tests, clippy clean, release builds.

## The task

Build **notes-v2** per the spec:
`docs/superpowers/specs/2026-07-19-notes-v2-design.md`.

Six items: (1) session context in rows, (2) filter `/`, (3) global note (pinned
`★` row toggles the pane's active note), (4) color + live-first sort — **NO bulk
clean, delete stays single `d`**, (5) go-to-tab `g` (live rows → `tab.focus`),
(6) row margin fix (unicode-width, balanced margins). The spec has the exact
data sources, key map, and test list.

## How to execute

1. Read the spec. Invoke **superpowers:writing-plans** to produce
   `docs/superpowers/plans/2026-07-19-notes-v2.md` (TDD, bite-sized tasks; the
   spec's Global Constraints section is the plan's global constraints).
2. Then **superpowers:subagent-driven-development**: task-brief per task,
   fresh implementer subagent (model `sonnet`; `haiku` for trivial pure fns),
   task reviewer per task, one opus whole-branch review at the end. Progress
   ledger at `.superpowers/sdd/progress.md` (gitignored, resumable). Scripts:
   `<superpowers>/skills/subagent-driven-development/scripts/{task-brief,
   review-package}`.
3. Finish with **superpowers:finishing-a-development-branch** → user wants
   **merge to main + push to origin, NO PR** (that has been the pattern).

## herdr / environment gotchas (learned this project)

- Windows; use the Bash tool for cargo/git. Binary is a **bin crate, no lib** —
  `cargo test <name>`, not `cargo test --lib`.
- `cargo build --release` fails **os error 5** while a Notes TUI pane is open —
  the running exe is locked. Ask the user to close the Notes pane (focus it,
  `prefix+n`), then rebuild. Unit tests + `cargo build` (debug) are unaffected.
- Deploy = after merge, user closes the Notes pane, you `cargo build --release`,
  user reopens with `prefix+n`. The action's registered plugin_root IS this
  checkout, so the rebuilt `target/release/herdr-notes.exe` is what runs.
- End-to-end verify by driving the DEBUG binary in a throwaway session:
  `herdr --session probeN server` (headless), set
  `HERDR_SOCKET_PATH=%APPDATA%\herdr\sessions\probeN\herdr.sock`, create a
  workspace + tabs, `pane run "$env:HERDR_PLUGIN_STATE_DIR='<tmp>'; & '<bin>'; exit"`,
  drive with `pane send-keys` / `pane send-text`, read with
  `pane read <id> --source visible`. Pre-seed note JSON files into the temp
  state dir to populate the overlay. Tear down with `herdr server stop`.
  **Do NOT touch the user's `default` session's notes.**
- `pane send-keys` rejects Home/End/PageUp/PageDown — the app has `g`/`G`
  single-char fallbacks. Esc must NEVER quit (only `q`). Metadata tokens must be
  strings. crossterm reports AltGr as CONTROL|ALT (treat as text).
- Socket API methods used: `pane.list`, `tab.list` (GLOBAL — all tabs),
  `workspace.list`, `tab.focus`, `pane.report_metadata`. `pane.list`/`tab.list`
  only see THIS session's server (cross-session notes read as `closed` — known,
  documented, harmless since delete is manual).

## Keybindings the user set (herdr `config.toml`, %APPDATA%\herdr)

- `prefix+n` = toggle Notes pane. `alt+←/→` = prev/next tab. `alt+↑/↓` =
  prev/next workspace. (`prefix` = ctrl+b.)

## Accepted minor leftovers (not blocking; fix if convenient during v2)

- `NoteSummary.preview`/`nonempty` computed but `open_overlay` re-reads the file
  for full text (2 reads/note on overlay open).
- `store_dir` config-fallback excludes the legacy single `notes.json` from the
  list (non-herdr edge case only).
- Overlay Preview scroll has no upper clamp (scroll past end = blank, no crash).

## Style

- Match existing code density/idiom. Caveman-terse chat is the user's mode
  (technical substance intact); prose/commits/PRs normal. Commits end with the
  Co-Authored-By trailer already in use.
