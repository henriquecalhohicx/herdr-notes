# herdr-notes

A single herdr plugin: a persistent markdown notes pane (one scrollable note
per tab, preview/edit modes). Standalone Rust crate — the repo root IS
the plugin root (`herdr plugin link .` from here).

**Living doc**: when you discover a non-obvious herdr/Windows/TUI behavior the hard
way, record it in the Gotchas section below before finishing the task. The fuller
findings doc (and the reference implementation, `herdr-sidebar`) lives in
`C:/Users/Alex/Projects/herdr/CLAUDE.md` — read it before deep herdr integration work.

## Layout

- `src/main.rs` — event loop (500ms poll); `--launch-decision` / `--focused-pane` /
  `--open-plan` stdin modes used by the launcher scripts
- `src/app.rs` — App state: preview/edit, clear-confirm overlay, 2s debounced
  autosave, 5s heartbeat, scrollbars, in-note title editing (`r` in preview
  sets/renames the note's title — Enter saves, Esc cancels — the header
  shows the title). Preview also carries a checkbox cursor (`box_cursor`, an
  ordinal into `markdown::checkbox_lines` — `j`/`k` hop between checkbox
  lines and clamp at either end, `space` flips the box straight in
  `note.text`, `esc` drops the cursor — the ONLY deliberate exit, every
  other one is a side effect; every rendered row of a wrapped item
  highlights together via
  `render_markdown_mapped`'s row→source-line map). Scrolling the cursor into
  view is one-shot (`follow_box`, cleared after the next draw) so it only
  fires right after `j`/`k`/`space` move the cursor — gating it on the
  cursor merely existing instead would re-force the viewport to it on every
  draw and make `Up`/`Down`/`g`/`G`/PgUp/PgDn look broken the instant a
  cursor is set. A fresh note is seeded with `template::DEFAULT` on the
  first INTERACTIVE `e` (lazy — a tab you only toggled Notes into writes no
  file; `enter_edit(seed)` takes the flag so restoring a persisted
  `mode: "edit"` never seeds), landing the cursor on the template's blank
  line under `## Status` so the first keystroke IS the status; the header
  shows mode, title, scroll hint, then the note's age last and
  ALL-OR-NOTHING (pushed only when the whole `2h ago` token fits the header
  width, measured in display columns — never clipped to `2h ag`); the footer
  has a full and a short form, chosen by pane width, which drops the floor
  for keeping `q quit` on screen from 69 columns to 37 — and a second pair
  (`PREVIEW_HINTS_CURSOR`/`_CURSOR_SHORT`, 79/39 cols) used only while a
  checkbox cursor is live, adding `esc drop` at the cost of `l list` in the
  narrow form. There is also a notes-list overlay
  (`l` in preview: navigate with Up/Down or j/k, `enter` opens a read-only
  scrollable preview of the selected note, `r` renames it, `d` deletes it
  with a y/N confirm, `esc`/`l` closes the overlay). v2 turned the overlay
  into a cross-session dashboard:
  each row shows session context (`workspace · agent` for live tabs, else a
  dim `closed`/`?`) instead of a status word, colored (green live / gray
  closed / cyan global) and sorted live-first-then-newest; `/` filters rows by
  title/context (case-insensitive, live; Enter keeps, Esc clears); `g` on a
  live row focuses that tab (`tab.focus`) and closes the overlay; a pinned
  `★ Global` top row toggles the pane between its tab note and a shared
  cross-session global note (`ActiveNote::Tab|Global`; save/load routes to
  `state::global_path()` → `global.json` when Global; header reads
  `[mode] — ★ Global`; the global row is immune to `r`/`d`/`g` and stays
  pinned+visible through any filter). Row layout fills the box inner width
  with balanced 1-space margins, truncating by unicode display width
  (`format_row`/`dwidth`/`truncate_w`); the note NAME keeps a `NAME_MIN`
  (8-column) floor before the right-hand segment gets any budget
  (`right_budget`), and that segment degrades in WHOLE tokens — progress
  count first, then context (`fit_right`) — so a 40-column dock still shows
  both a title and `workspace · agent` instead of one column of title.
  NOTE: `is_self` self-mutation on
  delete/rename is gated on `showing_tab_note()` — acting on your own
  tab-note row while viewing the global note must NOT touch the global buffer
  (that path silently deleted `global.json`; see Gotchas)
- `src/markdown.rs` — hand-rolled renderer (headings, lists, checkboxes —
  bare `[ ]`/`[x]` as well as `- [ ]`/`* [ ]` — quotes, code, bold/italic, hr)
  + display-width wrapping. Also the crate's ONLY checkbox parser:
  `checkbox_lines`/`checkbox_counts`/`toggle_checkbox` (all fence-aware) and
  `render_markdown_mapped`, which returns a per-rendered-row source-line map
  (one source line can wrap to several rows, which all map back to it);
  `render_markdown` is now a thin wrapper over the mapped form with its
  signature unchanged
- `src/prompts.rs` — prompt capture storage for the `--capture-prompt` hook
  mode (a `UserPromptSubmit` hook piping its JSON payload into the binary; see
  Gotchas). One file PER PANE, not per tab —
  `<tab-key>__<pane-key>.prompts.json` in the same store dir as note files,
  keys from `state::id_key` so both layouts agree on what an id spells on
  disk — because a tab can hold several agent panes and a shared per-tab file
  would mean concurrent read-modify-write from independent hook processes. A
  ring of the last `RING` (3) prompts per pane file; each prompt is condensed
  to its first line, trimmed, cut to `MAX_CHARS` (120) with a trailing
  ellipsis, so nothing sits on disk that isn't also shown. `load_for_tab`
  merges every pane file belonging to a tab, newest `ts` first (ties broken by
  pane then text, since `read_dir` order isn't guaranteed across platforms),
  capped at `RING`. `capture` is a gate chain — the `HERDR_NOTES_NO_CAPTURE`
  off switch, running inside herdr, filename-safe tab AND pane ids, an
  existing note file for this tab (no note, no capture — a tab that never
  opened Notes gets no prompt file either), a usable `prompt` field in the
  hook's stdin payload — every rejection silent and total, because the caller
  runs inside a `UserPromptSubmit` hook. Gate 4 asks `state::note_file_in` for
  the note path rather than spelling `<key>.json` again — the hook is a second
  process and prints nothing, so a layout drift would stop capture with no
  diagnostic anywhere. `App.prompts` (`app.rs`) re-reads via `load_for_tab`
  and renders a dim "Last Prompts" block above the note in preview — including
  above the empty-note help, since a titled body-less note keeps its file and
  so keeps accumulating prompts — gated on `showing_tab_note()` (the global
  note is not a tab and carries no prompts). `refresh_prompts` runs at
  construction, at the end of `toggle_global`, and on the 5s heartbeat: the
  first two exist so the block is never blank while the user waits out a
  throttled heartbeat, which reads exactly like capture being broken
- `src/template.rs` — the Status/Next/Notes skeleton, one const. Every
  section ships EMPTY — no placeholder prose: edit mode has no line-kill,
  word-delete or selection, so a placeholder would cost `End` plus one
  Backspace per character on every new note. The `[ ] ` line's TRAILING
  SPACE is load-bearing (`is_blank` compares the buffer to the const with
  `==`; whitespace-stripping editor tooling silently breaks it — verify with
  `git show HEAD:src/template.rs | cat -A`). `is_blank`
  treats the pristine template as blank, so seeding cannot leak orphan files
- `src/state.rs` — `{text, mode, title, tab_id, created, updated}` JSON (v2 —
  older `{text, mode}` files still load, missing fields fall back to
  defaults; `created` is stamped once, `updated` bumps on every save). A
  note with no text AND no title is DELETED on save instead of written —
  see Gotchas. One file PER TAB in herdr's
  plugin state dir (`HERDR_PLUGIN_STATE_DIR/<tab-key>.json`, e.g.
  `%LOCALAPPDATA%\herdr\plugins\herdr-notes\` — the docs-mandated home for
  durable plugin state; actions get the env var and the launchers pass it
  into the pane via `pane split --env`, the unix `[[panes]]` entry gets it
  natively). `store_base` has a THIRD tier between that and the config-dir
  fallback: `HERDR_ENV == "1"` with no explicit `HERDR_PLUGIN_STATE_DIR` also
  resolves the conventional plugin state dir (same path as above), rather than
  falling through to the config layout. This exists for the `--capture-prompt`
  hook — verified live, a Claude Code agent pane inside herdr inherits
  `HERDR_ENV`/`HERDR_TAB_ID`/`HERDR_PANE_ID` but NOT the plugin-scoped
  `HERDR_PLUGIN_STATE_DIR` (that var is injected only into the Notes pane
  itself). Without this tier the hook process and the Notes pane would
  resolve two different directories for the same tab and silently disagree
  about where a captured prompt lives. So: THREE tiers, in order — (1)
  explicit `HERDR_PLUGIN_STATE_DIR` → that dir; (2) `HERDR_ENV == "1"` with no
  such var (ANY pane inside herdr, including the binary or `open-notes.ps1`
  run by hand in one) → the conventional plugin state dir
  `%LOCALAPPDATA%\herdr\plugins\herdr-notes\` (unix
  `$XDG_DATA_HOME|~/.local/share/herdr/plugins/herdr-notes/`); (3) OUTSIDE
  herdr entirely, neither var → the config-dir fallback
  `%APPDATA%\herdr\notes\<tab-key>.json` (unix:
  `$XDG_CONFIG_HOME|~/.config/herdr/notes/`). Tiers 1–2 migrate
  config-layout files in on first load — the tab note (`load_state_dir`) AND
  the shared `global.json` (`load_global`/`load_global_state_dir`, added with
  prompt capture: without it the one explicitly cross-session document read
  back empty after tier 2 appeared). Keyed by the
  `HERDR_TAB_ID` herdr injects into every pane (form `<workspace>:<n>`, e.g.
  `w1:t2`; monotonic and never reused within a session, so a closed tab
  leaves a harmless orphan file that no future tab reclaims). The `:`
  separator is sanitized to `_` for the filename (`w1_t2.json`); herdr ids
  never contain `_`, so no collision. Unset or filename-unsafe (anything
  beyond alphanumerics + the single `:`) id → legacy single-note
  `herdr/notes.json`; first tab load MOVES a lingering legacy
  file into the tab's slot (read-in-place if the rename fails; the
  per-tab file wins when both exist). NOTE: old per-workspace `<w>.json`
  files are NOT migrated — they orphan; delete by hand. `note_key` exposes
  the note-FILE identity of a tab id (None = shared legacy file; Windows
  folds ASCII case because NTFS filenames are case-insensitive) — the
  launcher guard compares THESE keys so it can never drift from the on-disk
  layout. Forgiving parse, atomic save (temp + `sync_all` + rename); path
  logic takes an injected base dir so tests never touch the real APPDATA.
  Notes-manager helpers live here too: `list_notes` (enumerates every note
  file in the store dir), `store_dir`, `classify_tab`/`TabStatus` (live/
  closed/unknown, from a `pane.list` socket call matched on `tab_id`),
  `format_age` (e.g. `2h`), `set_title`, `persist_at`, and `is_blank` (the
  no-text-no-title check the delete-on-save rule uses)
- `src/launch.rs` — OPEN/FOCUS/CLOSE/REPLACE toggle decisions (20s stale heartbeat
  → REPLACE); matches any pane whose `note_key` (on the tab id) EQUALS the
  focused pane's, so a second instance on the same note file is never spawned
  (two live instances = last-writer-wins data loss) even when different raw tab
  ids coarsen to one file (unsafe/missing ids → legacy, NTFS case folding);
  Notes panes in other tabs are different documents and are ignored (each tab
  opens its own)
- `src/ipc.rs` — socket client: named pipe `\\.\pipe\<HERDR_SOCKET_PATH>` on
  Windows, unix socket elsewhere; one NDJSON request per connection
- `scripts/open-notes.ps1` / `open-notes.sh` — toggle launchers (right-dock);
  Windows entry goes through the inline-powershell action in `herdr-plugin.toml`

## Build / test / lint

```
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
```

All three must pass before shipping. `cargo build --release` fails with os error 5
while the TUI is running in a pane — quit/close the pane first (and
`Get-Process herdr-notes | Stop-Process` for stragglers).

## Plugin dev workflow

- `herdr plugin link .` registers this checkout; `herdr plugin list --json` shows it.
- Open/toggle: `herdr plugin action invoke herdr-notes.open-notes-windows`
  (unix: `herdr-notes.open-notes`).
- `herdr plugin log list --plugin herdr-notes` shows action/spawn logs.
- After a rebuild, close any open Notes pane and re-invoke the action (stale panes
  keep the old binary).
- End-to-end verification: drive the real binary in a throwaway pane —
  `herdr pane split` + `pane run` + `pane send-keys` + `pane read --source visible`,
  then check the note file. WHICH PATH depends on the pane's env (see the
  three tiers in `src/state.rs` above): a plain `pane split` pane has
  `HERDR_ENV=1` but NOT `HERDR_PLUGIN_STATE_DIR`, so it writes
  `%LOCALAPPDATA%\herdr\plugins\herdr-notes\<tab-key>.json` — NOT
  `%APPDATA%\herdr\notes\`, which only applies outside herdr. A pane launched
  through `open-notes.ps1`/the action gets the var explicitly and lands in the
  same plugin state dir. `<tab-key>` is the pane's `HERDR_TAB_ID` with `:`
  sanitized to `_` (lowercased on Windows). Cheap, catches what unit tests can't.

## Gotchas (verified against herdr 0.7.1)

Inherited from the sidebar plugin's findings:

- Windows herdr can NOT spawn a relative `[[panes]]` command (resolves against
  herdr's own dir) — Windows launches go through the action's inline powershell,
  which locates the plugin root via `herdr plugin list --json` (strip the `\\?\`
  prefix) and spawns the exe by absolute path.
- Action ids must be globally unique across platforms — hence the `-windows`
  suffix, both variants gated by the item-level `platforms` key.
- herdr panes run Windows PowerShell 5.1: chain with `;` / `if ($?)`, never `&&`.
  PS 5.1 prepends a UTF-8 BOM when piping into a native exe's stdin — everything
  parsing herdr JSON from stdin strips a leading `\u{feff}` (see `state.rs`/`launch.rs`).
- `pane split --ratio` is the ORIGINAL pane's share (the new pane gets 1 − ratio);
  ratios clamp to a 0.1 floor.
- Metadata token values must be STRINGS (numbers rejected silently); the heartbeat
  token (`herdr-notes` = unix-time string) re-stamps every ~5s so launchers can
  tell a live pane from a corpse (>20s stale → REPLACE).
- Esc must NEVER exit the TUI (only `q` quits); modifier+Enter is indistinguishable
  from plain Enter in herdr panes; avoid emoji with VS16 variation selectors.

Learned building this plugin:

- `herdr pane send-keys` rejects Home/End AND all PageDown/PageUp spellings —
  every scroll action needs a single-char fallback (`g`/`G` here) to stay drivable.
- A `pane list` snapshot goes stale the moment you close a pane: the REPLACE path
  must re-run `pane list` after closing the corpse before deriving split targets,
  or the split targets a dead pane id and the action exits 1.
- Plain `herdr pane list` is GLOBAL — panes from EVERY workspace/tab, exactly
  one `focused` pane in the whole list. The launchers deliberately pass this
  GLOBAL list: scoping with `--workspace`/`--tab` uses the launcher shell's
  SPAWN-TIME env id, which can diverge from the focused pane's actual tab
  (pane moved between tabs, action invoked under another tab's env) — the
  scoped list then omits the focused pane, `--launch-decision` degrades to
  OPEN, and a duplicate Notes pane spawns beside the focused tab's live one.
  All scoping happens in the binary off each pane's `tab_id` FIELD, compared
  by note-file identity (`state::note_key`) so the guard matches exactly the
  panes that share a file.
- `herdr plugin action invoke` runs the action in the GLOBALLY focused
  tab context, not the invoking pane's. Keybinding use is fine (the focused
  tab IS the intended one), but a background/scripted invoke races with the
  user switching tabs/workspaces: it toggles Notes in — and can legacy-migrate
  a note into — whatever tab happens to be focused. Scripted invocations MUST
  focus the target tab first and verify it stayed focused.
- Tab ids (`HERDR_TAB_ID`, e.g. `w1:t2`) are monotonic and NOT reused within a
  session; the session's id counter persists across a server restart (verified
  0.7.4). So an orphaned per-tab note file can never be reclaimed by a future
  tab — no stale-content risk, just dead files that accumulate as tabs close.
- A pane created with `pane run "<shell command>"` keeps its shell alive after
  the command exits — quitting the TUI with `q` left a dead PowerShell prompt
  still labeled "Notes". The ps1 launcher appends `; exit` to the pane run
  command (unix `exec`s) so the pane closes itself when the TUI quits; the
  CLOSE paths therefore treat `pane close` as best-effort cleanup (`*> $null`
  / `|| true`, exit 0) because the pane is usually already gone.
- `herdr pane close` kills the process with no signal — a dirty debounce buffer is
  lost. Launcher CLOSE/REPLACE paths first send `pane send-keys <id> Escape q`
  (graceful save-and-quit from any mode), sleep ~400ms, then close as cleanup.
- Heartbeat/autosave must run every event-loop iteration, not only on poll timeout:
  sustained input (<500ms gaps — auto-repeat, long paste) otherwise starves them
  until the launcher declares the live pane stale and REPLACEs it mid-edit.
- crossterm on Windows reports AltGr as CONTROL|ALT — treat CONTROL|ALT chars as
  text insertion or AltGr layouts can't type `@ { [ ] } \`.
- Wrap and horizontal cursor math must budget by display columns (unicode-width),
  not char count — CJK/emoji are double-width and get clipped otherwise.
- Empty (no text, no title) notes are deleted on save, so toggling Notes into
  a tab and closing without typing leaves no file.
- v2 gave the pane two possible buffers (`App.note` is the tab note OR the
  shared global note, per `App.active`). The overlay's `is_self` flag is FILE
  identity only — it does NOT track which buffer is showing. So any self-clear
  on an `is_self` row (delete clears text+title, rename sets title) MUST also
  gate on `showing_tab_note()`; otherwise deleting/renaming your own tab-note
  row while viewing the global note clobbered the global buffer, and the next
  autosave (blank-note delete rule) silently removed `global.json`. Caught by
  the whole-branch review, not per-task review — a two-document buffer added
  later has to audit every pre-existing single-document assumption.
- v2 overlay session-context uses `tab.list` (GLOBAL — all tabs: `tab_id`,
  `workspace_id`), `workspace.list` (`workspace_id`, `label`), `pane.list`
  (`tab_id`, `agent`; skip `agent == "usage"`, first non-null per tab wins).
  All three are best-effort: any call/parse/field failure collapses the whole
  index to `None` → every row reads Unknown, overlay works offline, never
  panics. Field names VERIFIED live on herdr 0.7.4: `workspaces[]` carry
  `workspace_id`+`label`, `tabs[]` carry `tab_id`+`workspace_id`, `panes[]`
  carry `tab_id` and — only once an agent is reported on the pane — `agent`
  (a bare shell pane has just `agent_status`, so the code's
  `else continue` on a missing `agent` is the normal path, not an error).
- The markdown checkbox parser accepts a BARE `[ ]` as well as `- [ ]` /
  `* [ ]`. It did not originally, which made the seed template's own tasks
  invisible to the cursor and the progress count while rendering identically
  (`- [ ] x` renders as `[ ] x`, so the bug was invisible on screen). Any
  second `[ ]` scan added anywhere else in the crate will drift from this one
  — count and toggle through `markdown::` only.
- Seeding a template into a fresh note re-opens the orphan-file hole the
  blank-note delete rule closed: the buffer is no longer empty, so the file
  persists for every tab where someone pressed `e` once and walked away, and
  tab ids are never reused to reclaim it. `is_blank` therefore also matches
  the pristine template exactly.
- One source line can render to several rows (width wrapping), so anything
  mapping a screen row back to the note text needs `render_markdown_mapped`,
  not row arithmetic. Highlight ALL rows of a wrapped item or it looks
  half-selected.
- The checkbox cursor's scroll-follow must be gated on the cursor having
  just MOVED (`follow_box`, a one-shot flag cleared after the next draw),
  not on a cursor merely existing. Gating on existence alone re-forces the
  viewport back to the cursor on every draw, so every other scroll key
  (`Up`/`Down`/`g`/`G`/PgUp/PgDn) looks broken the instant a checkbox cursor
  is set.
- EVERY piece of per-document state must reset in `toggle_global`:
  `preview_scroll`, `box_cursor` AND `follow_box` (`clear_box_cursor`).
  Same class of bug as the `global.json` clobber above — a field added later
  that the document swap does not know about. A `box_cursor` carried across
  the swap highlights an arbitrary checkbox in the note you just opened to
  READ, and one pager-habit `space` ticks it. The same clear belongs on
  every text-wiping path (`x` confirm, overlay self-delete): a stale ordinal
  is harmless only while the text is empty (`cursor_line()` returns None),
  and stops being harmless the moment `e` re-seeds text under it.
- `state::persist_at` stamps `created`/`updated` onto a CLONE of the note, so
  `App::save` has to mirror them back onto `self.note` (hence `&mut self`).
  Without that, the in-session note keeps whatever `load()` read at startup:
  a note CREATED this session never shows an age at all (the `updated > 0`
  gate), and an older one keeps ageing while you type into it — the header
  says `3h ago` about text written thirty seconds ago while the overlay row
  for the same file, which re-reads from disk, says `just now`.
- Residual hole in the blank-note delete rule: `is_blank` matches the
  pristine template EXACTLY, and preview-mode `space` writes into
  `note.text` outside edit mode. So `e` (seeds) → `Esc` (correctly writes no
  file) → `j` → `space` leaves the buffer at DEFAULT-with-`[x]`, no longer
  `== DEFAULT`, and a file containing nothing but an empty skeleton with one
  ticked empty box is written — orphaned forever (tab ids are never reused).
  Two keystrokes. Known and ACCEPTED: do NOT widen `is_blank` to chase
  template variants, that trades a dead file for a risk of deleting real
  notes.
- `mode: "edit"` persists on disk (`herdr pane close` sends no signal, so a
  note autosaved mid-edit keeps it), and `with_note` re-enters edit at
  startup. Seeding there — rather than only on the interactive `e` — would
  give a titled, bodyless note a body nobody typed and autosave it 2s later.
  Seed on the interactive path only.
- `UserPromptSubmit` hooks must exit 0 and print nothing: a non-zero exit
  blocks the user's prompt from being sent, and Claude Code injects whatever
  the hook writes to stdout into that prompt as context. `--capture-prompt`
  therefore always returns `Ok(())` regardless of whether `capture_from_env`
  actually wrote an entry, and never prints — a genuine capture failure must
  fail exactly as silently as an intentional gate rejection.
- `list_notes` filters on extension `json`, and `<tab-key>__<pane-key>.prompts.json`
  also ends in `.json` — without an explicit skip for that suffix, every
  prompt file becomes a junk row in the notes overlay.
- `Set-Content -Encoding UTF8` means two DIFFERENT things: utf8-no-BOM in
  pwsh 7, BOM-prefixed UTF-8 in Windows PowerShell 5.1 (measured on this
  machine: `EF BB BF` vs none). herdr panes run 5.1, so any script that writes
  a file another tool parses must write the encoding explicitly —
  `[System.IO.File]::WriteAllText($p, $s, (New-Object System.Text.UTF8Encoding($false)))`.
  `install-prompt-hook.ps1` writes the user's GLOBAL `~/.claude/settings.json`;
  a stray BOM there could take out every setting they have.
- `std::env::args()` PANICS on a non-Unicode argument, and `.nth(1)` forces
  argv[0] (the exe path) through that check first. On the `--capture-prompt`
  path that panic is exit 101 + stderr — the two things a `UserPromptSubmit`
  hook must never do — and it fires before the arm that would swallow it. Use
  `args_os()` + a LOSSY conversion (`first_arg` in main.rs) so a bad argument
  becomes an ordinary unknown argument instead of a crash or a silent TUI launch.
- Every new store tier needs a migration for EVERY document, not just the tab
  note. Adding the `HERDR_ENV=1` middle tier moved the tab note (which had
  `load_state_dir`) but left `global.json` behind, so the one explicitly
  cross-session document read back empty — presenting as data loss on exactly
  the note a user would most notice. Same class as the `toggle_global` reset
  bug: a second document added later has to be walked through every
  single-document code path.
- An installer's backup must not be re-taken on re-run. The README invites
  re-running `install-prompt-hook.ps1`; `Copy-Item -Force` made the second
  run's "backup" a copy of the already-modified file, destroying the only
  pristine copy. It now keeps the FIRST backup and says so.
- Prompt storage is one file PER PANE, not per tab: a tab can hold several
  agent panes, and a shared per-tab file would mean concurrent
  read-modify-write from independent hook processes (each `UserPromptSubmit`
  fires as its own short-lived process with no coordination between panes).

## README screenshots (Alex's criteria — follow on every reshoot)

The three shots in `docs/media/` (hero / edit / welcome) must show:

- **A 2×2 grid of agents beside the Notes pane: exactly 2 Claude Code + 2
  OpenAI Codex** (mixed diagonally looks best). NO Sidebar/explorer panel in
  any shot — the notes pane and the agents are the subject.
- **The CLI harness graphics must be visible** — Claude Code's logo art +
  version banner, Codex's boxed model/directory banner — with **some text in
  the agents**: type a realistic prompt into a couple of composers via
  `pane send-text` (NOT `pane run` — text must sit unsubmitted so no agent
  actually runs and no tokens burn).
- Exactly ONE title per pane: the border label says "Notes"; the in-app
  header shows only `[preview]`/`[edit]` + scroll position (user-reported
  duplicate — do not reintroduce).
- The note pinned to the TOP (`send-keys <pane> g` immediately before the
  capture — a mouse wheel over the focused pane can scroll it between steps,
  so keep g→capture in ONE command) showing the demo note with headings,
  checkboxes, a code fence, a quote, and the scrollbar visible.

- **Shared dummy backdrop** (agreed with the herdr-sidebar Coordinator
  agent — both repos' screenshots use the SAME roster; keep them in sync):
  herdr's left chrome must show the fictional acme universe, never Alex's
  real projects. Spaces: `acme-app` [main, 1↑ — the real demo repo built by
  the monorepo's `tools/screenshots/setup_demo.sh`], `acme-api` [main],
  `acme-web` [dev], `billing-service` [main] (backdrop cwds are throwaway
  git-init'd temp dirs so branch sublabels render). Agents panel: the four
  visible acme-app grid panes labeled `auth-refactor` (claude),
  `checkout-tests` (codex), `api-docs` (codex), `rate-limiter` (claude),
  plus FAKE background rows `flaky-tests` (codex, working, acme-api),
  `reviewer` (claude, idle, acme-web), `migrations` (codex, working,
  billing-service). Fake rows are reported via the socket API
  `pane.report_agent {pane_id, source, agent, state}` on plain shell panes
  (no CLI spawned, persists over detection); in-universe composer texts:
  "Draft OpenAPI docs for the billing endpoints" (api-docs), "Add a
  sliding-window rate limiter to the gateway" (rate-limiter).
- **Staging happens in an isolated named session**, never Alex's real one:
  `herdr --session shoot server` (headless), then point HERDR_SOCKET_PATH
  at `%APPDATA%\herdr\sessions\shoot\herdr.sock` for every CLI/RPC call.
  Display window: a separate WT window running a script that CLEARS the
  inherited HERDR_* env (herdr refuses "nested" otherwise) then
  `herdr session attach shoot`; find/resize/capture that window BY TITLE
  (WT is single-process, MainWindowHandle is ambiguous). **One-shot restage:
  `tools/screenshots/stage-shoot-session.ps1 -WithGrid` in the monorepo**
  rebuilds the whole backdrop idempotently (server, demo repos, workspaces,
  fake agent rows, grid, display window); helper scripts
  (capture_titled.ps1, resize_titled.ps1, attach_shoot.ps1, herdr_rpc.py —
  JSON params via stdin, PS 5.1 mangles quoted JSON argv) live beside it.
  The demo note markdown is `tools/demo-note.md` in THIS repo — seed it
  into the shoot workspace's `notes\<ws>.json` before capturing (read
  with `-Encoding UTF8`!). The shoot session shares the real config dir:
  its note files live under `notes\` — back up/clean up.

Hard constraints learned live:

- **The user's email must never appear**: Claude Code's welcome banner
  includes it in its wide two-column variant. Verified live (both repos'
  shoots): compact/no-email at ≤63 cols, email variant at 74+ cols — keep
  agent grid columns ≤63, target ~60 for margin. Verify every image before
  shipping; `blur_region.py` is the fallback. (First name "Alex" in the
  compact banner is acceptable.)
- Procedure/tools: monorepo `tools/screenshots/` — `resize_wt.ps1 1760 996`
  (note the printed "was" size and restore it), stage a `--focus` tab in
  THIS workspace, seed the demo note into `notes\<ws>.json` (backup and
  restore the user's file; read the seed markdown with
  `Get-Content -Raw -Encoding UTF8` or em-dashes mojibake), close any
  existing same-workspace Notes pane first (the launcher would FOCUS it
  instead of opening in the staging tab), `capture.ps1` → `crop.ps1 8 48
  1744 940` → frame via `frame_pil.frame` into `docs/media/`. Keep framed
  titles/filenames stable (hero/edit/welcome).
