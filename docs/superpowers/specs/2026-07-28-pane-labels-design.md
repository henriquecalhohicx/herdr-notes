# Pane labels and a live capture gate — design

Date: 2026-07-28

## Goal

Make the notes pane use the name you gave a pane, and start capturing prompts
the moment a Notes pane is open rather than the moment you happen to type into
the note.

This is **phase D**, added after live testing of phase C. Phases A
(`62a686e`/`91b35de`), B (`ed75fd4`) and C (`569b215`) are merged and pushed.

## What live testing turned up

Two things, both found by the user driving phase C on a real multi-agent tab:

1. Renaming a pane to `test-1` left its prompt group headed `claude pE`.
2. Prompts were captured only after typing something into the note — opening
   Notes was not enough.

The first is a mistake in phase C's spec. The second is an emergent consequence
nobody chose.

## Ground truth (verified live on herdr 0.7.4, not assumed)

- **herdr DOES expose a pane label.** `pane.list` reports `"label": "test-1"`
  on a renamed pane. **The key is absent from the JSON until a label is set**,
  which is why a dump taken before any rename shows no such field — the mistake
  that produced phase C's false claim. Verify a "field does not exist" claim
  against data that should contain it.
- The Notes pane's liveness token IS visible in `pane.list`:
  `wD:pH | label Notes | tokens {'herdr-notes': '1785239724'}` — a string
  holding unix seconds, re-stamped every 5s by `App::report_tokens`.
- `src/launch.rs` already carries everything needed to read it:
  `HEARTBEAT_STALE_SECS = 20`, a `token_stale(tokens, key, now)` helper, and
  pane-list parsing with `tokens`, `tab_id` and `label`.
- **`token_stale` returns `false` for a MISSING token.** That is deliberate for
  the launcher, which also accepts `label == "Notes"` as evidence of a Notes
  pane. It is wrong for the capture gate, which must require the token *present
  and fresh* — a label outlives a dead pane, the token does not.
- A Claude Code agent pane inherits `HERDR_ENV`, `HERDR_TAB_ID`,
  `HERDR_PANE_ID`, `HERDR_WORKSPACE_ID` and `HERDR_SOCKET_PATH`. The socket path
  is what makes a gate check from inside the hook possible at all.

## User decisions (approved)

- **Title chain: pane label, then terminal title, then git branch, then the
  oldest surviving prompt.** One better source in front of phase C's chain.
- **Re-derive while `title_auto` is true.** An auto title tracks its source
  instead of freezing at the first thing available. Typing a title with `r`
  still freezes it permanently.
- **The capture gate asks herdr.** One `pane.list` call, token freshness,
  falling back to today's note-file check on any failure.

## Features

### 1. `label` preference

`PaneInfo` gains `label: Option<String>`, read from `pane.list` in
`build_pane_index`.

Group headings become: label → meaningful terminal title →
`{agent} {pane-suffix}`. The title chain's source 1 becomes the label and its
source 2 the terminal title, with the branch and oldest prompt unchanged behind
them.

**A label does not go through `meaningful_title`.** That rejection list — the
generic tool names in `GENERIC_TITLES`, path-shaped strings, a `.exe` suffix —
exists because `terminal_title_stripped` is machine-set and unreliable. A label
is a string the user typed on purpose, so rejecting `src/app.rs` as path-shaped
would be wrong. A label needs only to be non-empty after trimming.

`PaneInfo::nice_title()` is the one place this pairing is spelled, and it stays
the one place: it returns the label when present and non-blank, else the
meaningful terminal title, else `None`. Both consumers (`pane_label` and the
title chain) keep reading it, so they cannot drift.

### 2. Live re-derive

`autotitle_wanted` drops its `note.title.trim().is_empty()` condition. It keeps
every other gate: `persist`, `showing_tab_note()`, `title_auto`, and
`!state::is_blank(&note)` from phase C.

So an auto title tracks the chain on every heartbeat until the user types one.
Renaming a pane updates the note within 5s even if the branch name had already
landed.

Two consequences the implementation must handle:

- **Only `touch()` when the derived value differs from the current title.**
  Otherwise every heartbeat dirties the note, the 2s autosave fires forever,
  `updated` keeps bumping, and the header age resets to `just now` on a loop.
- **`App.git_tried` must cache the successful branch, not merely record that a
  cwd was attempted.** Today success and failure are recorded identically, so
  once a cwd's one attempt is spent the branch is unavailable forever. Under
  re-derive that becomes visible: a pane that loses its label would fall past
  the branch to the prompt text. Changing `git_tried` to
  `HashMap<String, Option<String>>` keeps the one-spawn-per-cwd bound — the
  reason the bound exists is unchanged, see `CLAUDE.md`'s gotcha on the
  unbounded `git` spawn — while making a second derivation return the same
  branch it found the first time.

### 3. Capture gate

`prompts::capture` gains a check ahead of the existing note-file gate:

```
notes_pane_live(tab_id) -> Option<bool>
  Some(true)  a pane in this tab has a `herdr-notes` token < 20s old -> capture
  Some(false) the socket answered and no such pane exists          -> reject
  None        the call or the parse failed                          -> fall back
                                                                       to the
                                                                       note-file
                                                                       check
```

A broken socket therefore degrades to today's behavior, never to silent
no-capture. The token must be present AND fresh — see the `token_stale` trap
above.

**The socket read needs an explicit short timeout**, a few hundred
milliseconds. Claude Code kills a hook at its configured `timeout: 5`, and it is
not established here whether a killed hook blocks the user's prompt. Bounding
the read well short of that makes the question moot rather than answering it on
the user's keystrokes. This is a requirement on `src/ipc.rs`, which today sets
no read timeout.

The two hard hook rules are unchanged and bind this code as they bind the rest
of the capture path: **always exit 0** — a non-zero exit can block the prompt —
and **never write to stdout**, which Claude Code injects into the prompt as
context.

### 4. Documentation corrections

Two committed claims are false and get corrected, both with the reason the
mistake happened, because that is the part a future reader needs:

- The ground-truth section of
  `docs/superpowers/specs/2026-07-27-agent-grouping-design.md` states there is
  no dedicated pane label or name field.
- `CLAUDE.md`'s gotcha says `terminal_title_stripped` is the ONLY
  human-readable per-pane string herdr exposes.

`CLAUDE.md` also gains a gotcha for the `token_stale` missing-token asymmetry,
since a future reader reusing that helper for a liveness check would inherit
the wrong answer silently.

## Failure modes

- **Socket unreachable.** Headings fall back to `{agent} {pane-suffix}`; title
  sources 1 and 2 are unavailable, the branch and prompt still work; capture
  falls back to the note-file check.
- **No label set.** Everything behaves as it does after phase C.
- **Token present but stale** (a killed Notes pane). Treated as no live pane, so
  capture falls back to the note-file check rather than rejecting outright — a
  tab with a real note keeps capturing.
- **Not a git repo.** Source 3 is skipped for that cwd, as in phase C.

Nothing in this phase can fail loudly.

## Testing

- a label is preferred over the terminal title in both consumers
- a path-shaped or generic-looking LABEL is accepted where the same string as a
  terminal title is rejected
- a blank or whitespace-only label falls through to the terminal title
- re-derive picks up a label change; the title does not move once typed with `r`
- no `touch()` when the derived value is unchanged
- the branch is cached, so a second derivation for the same cwd returns it
  without a second spawn
- the gate's three outcomes, including token-present-but-stale and
  token-missing-entirely
- the capture path still exits 0 and prints nothing on every path, including a
  socket that fails and one that hangs past the read timeout
- socket-dependent logic is tested against injected data, never a live socket

`cargo build --release`, `cargo test`, and
`cargo clippy --all-targets -- -D warnings` must all pass. End-to-end check
against a real multi-agent tab, since both features depend on live `pane.list`
data.

## Out of scope

Codex capture, grouping or per-agent counts in the overlay's dashboard rows,
pruning orphaned pane files, and the `meaningful_title` slash rule — that stays
as-is for terminal titles, and labels now bypass it entirely, which removes most
of its sting.
