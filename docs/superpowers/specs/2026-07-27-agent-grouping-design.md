# Per-agent grouping and auto-default titles — design

Date: 2026-07-27

## Goal

Make a tab with several agent panes readable at a glance: group the captured
prompts under the agent that sent them, and give an untitled note a name
derived from what the tab is actually working on.

This is **phase C** of three. Phase A (the Status/Next/Notes seed template, the
preview checkbox cursor, header age, overlay TODO progress) shipped in
`62a686e`/`91b35de`. Phase B (prompt capture via a `UserPromptSubmit` hook,
one file per agent pane) shipped in `ed75fd4`. This phase needs no data
migration: phase B already records `pane` and `agent` on every stored prompt.

## Scope

One spec, two features. They share exactly one thing — a pane-metadata lookup
over `pane.list` — and are otherwise independent. Splitting them would mean
running the process twice for two small changes; building the shared lookup
once and using it for both is the better trade.

## Ground truth (verified live, not assumed)

Read off a real `pane.list` on this machine:

- Every pane carries `cwd` (e.g. `C:\git-repositories\1-main\hicxesm`), so a
  git branch is derivable per pane.
- `agent` is `"claude"` on a Claude Code pane and `null` on a plain shell pane.
- **There is no dedicated pane label or name field.** The full key set is
  `agent`, `agent_session`, `agent_status`, `cwd`, `focused`, `pane_id`,
  `revision`, `scroll`, `tab_id`, `terminal_id`, `terminal_title`,
  `terminal_title_stripped`, `tokens`, `workspace_id`.
- `terminal_title_stripped` is the only human-readable per-pane string, and it
  is unreliable: a Claude pane mid-task reads
  `"HM-54271 Generic Importer Config API"`, but an idle or fresh one reads just
  `"Claude Code"`, and a shell pane reads
  `"C:\WINDOWS\System32\WindowsPowerShell\v1.0\powershell.exe"`.

> **CORRECTED in Task 6 documentation pass.** herdr DOES expose a dedicated
> pane label field (`label`) — the bullet above is wrong. The dump it was
> read off was taken before any pane had ever been renamed, and `pane.list`
> omits the `label` key ENTIRELY until one is set; a key's absence in a dump
> taken before any rename is therefore not evidence the field does not
> exist, only that nothing had set it yet. `terminal_title_stripped` is
> still the only human-readable string herdr reports UNPROMPTED, which is
> the narrower, still-true claim. See `PaneInfo.label` / `nice_title` in
> `src/app.rs` and the corresponding gotcha in `CLAUDE.md`.

Also worth recording, because it shaped the title chain: on this user's machine
the ticket id appears in the terminal title, not the branch — the branch at the
time of writing was `20260727-team-solutions`.

## User decisions (approved)

- **3 prompts per pane, grouped under agent headings.** Not 3 shared across the
  tab. With a shared cap of 3, four agents split three slots and grouping only
  adds headings to a list already too short to be useful.
- **Heading is the terminal title when meaningful, else `{agent} {pane-suffix}`.**
  Never a heading that says nothing.
- **Title chain: terminal title, then git branch, then the first captured
  prompt.** Set once; `r` freezes it.
- **`r` with empty text re-enables auto-titling**, and the rule applies
  retroactively to existing untitled notes.

## Features

### 1. Per-agent grouping

Each pane file already holds at most `RING` (3) prompts, so the per-pane cap
costs nothing: `load_for_tab` simply stops merge-truncating the combined list.

New return shape:

```rust
pub struct PromptGroup {
    /// Raw pane id as recorded at capture time, e.g. "wD:p8".
    pub pane: String,
    /// Newest first, at most RING.
    pub prompts: Vec<Prompt>,
}

pub fn load_for_tab(dir: &Path, tab_key: &str) -> Vec<PromptGroup>
```

Prompts stay newest-first within a group, as today. Groups are ordered by their
newest prompt's `ts`, descending, so the agent touched most recently sits on
top. Ties break on `pane` ascending, keeping the existing determinism rule
(`read_dir` order is not guaranteed across platforms).

### 2. Heading resolution

A pane id must become a human-readable label. The rule, in order:

1. `terminal_title_stripped`, when it is **meaningful**: non-empty, not equal
   (case-insensitively) to the agent's own generic name (`Claude Code`,
   `Codex`), and not path-shaped — containing `/` or `\`, or ending `.exe`.
2. Otherwise `{agent} {pane-suffix}`, where the suffix is the part of the pane
   id after the `:` — `claude p8`.
3. When the pane is absent from `pane.list` entirely (closed since capture, or
   the socket is unreachable), `{agent} {pane-suffix}` from the stored prompt's
   own `pane` and `agent` fields, which are always present.

Resolved on the existing 5s heartbeat via one `pane.list` call, best-effort in
exactly the pattern the overlay's context index already uses: any call, parse,
or field failure collapses the whole index to `None` and every group falls back
to rule 3. The block works offline and never panics.

The pure label logic takes an injected map and is unit-tested without a socket.

### 3. Auto-default title

`Note` gains `title_auto: bool`, serialized with the other fields.

**The migration is free.** A missing `title_auto` parses as
`title.trim().is_empty()`: an existing note WITH a title reads as manual, an
existing UNTITLED note reads as auto. No migration code, and existing untitled
notes get a title on next open — which is what the user chose.

Resolution runs on the heartbeat while `title_auto && title.trim().is_empty()`.
First hit wins:

1. The terminal title of the tab's agent pane — the first pane in this tab whose
   `agent` is non-null, matching how the overlay already picks a tab's agent —
   subject to the same meaningful-title rule as headings.
2. `git rev-parse --abbrev-ref HEAD` run in that pane's `cwd`.
3. The oldest surviving captured prompt for the tab.

On a hit the title is set and the note marked dirty; the existing 2s debounce
persists it. `title_auto` stays true — it records that the title was derived,
not that it is still pending.

**Source 3 is approximate and the spec says so rather than pretending
otherwise.** The ring holds 3, so the genuinely-first prompt is evicted after
the fourth submission. What source 3 yields is the oldest prompt still on disk.

**Bounding the git spawn.** Once a title is set the chain never runs again, but
an unresolvable tab — no agent pane, not a repo, no prompts yet — would spawn
`git` every 5 seconds indefinitely. So: at most one git attempt per `cwd` per
process, remembered after it fails. Sources 1 and 3 are cheap and keep retrying.

`r` with text sets `title_auto = false` and the title is frozen. `r` with an
empty value clears the title and sets `title_auto = true`, so the next beat
re-derives. That keystroke is the only way back to automatic.

## Failure modes

- **Socket unreachable.** Headings fall back to `claude p8`; title source 1 is
  unavailable; sources 2 and 3 still work if a cwd is already known, otherwise
  only source 3.
- **Not a git repo.** Source 2 is skipped permanently for that cwd.
- **No prompts yet.** Source 3 is unavailable and retried each beat.
- **No agent pane in the tab.** Sources 1 and 2 unavailable; the note stays
  untitled until a prompt is captured.

Every path degrades to "no title, headings that name the pane" rather than an
error. Nothing in this phase can fail loudly.

## Testing

- group ordering by newest `ts`, and the `pane`-ascending tie-break
- the per-pane cap holds at 3 with four panes contributing
- the meaningful-title rule against every rejection case: empty, the agent's
  own generic name in either case, a Windows path, a `.exe` suffix, and a pane
  missing from the index entirely
- `title_auto` parses both ways on a file that lacks the field, and round-trips
  once written
- each link of the title chain in isolation, and the fallthrough order when
  earlier links miss
- `r` with text sets the flag false; `r` with empty sets it true and clears
- the git attempt happens at most once per cwd after a failure
- pane-metadata tests take an injected map and never touch a socket

`cargo build --release`, `cargo test`, and
`cargo clippy --all-targets -- -D warnings` must all pass. End-to-end check
against a real multi-agent tab, since heading resolution depends on live
`pane.list` data.

## Out of scope

Codex capture (no submit-time hook exists), re-deriving a title when the
terminal title changes later, grouping or per-agent counts in the overlay's
dashboard rows, and pruning orphaned pane files.
