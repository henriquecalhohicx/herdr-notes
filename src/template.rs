//! The skeleton a fresh note is seeded with on its first edit. One const, so
//! `state::is_blank` (which treats the pristine template as nothing worth a
//! file) and the empty-note preview can never drift from what gets seeded.
//!
//! Every section ships EMPTY — no placeholder prose. `enter_edit` lands the
//! cursor on the blank line under `## Status`, so the first keystroke IS the
//! status; edit mode has no line-kill, word-delete or selection, so any
//! placeholder would cost End plus one Backspace per character on every new
//! note. Every heading is followed by a blank line before its content, so a
//! heading never sits flush against the text under it (`enter_edit`'s cursor
//! row is unaffected — it is still the first line under `## Status`, which
//! was already blank). `Next` ships one bullet-less checkbox with a TRAILING
//! SPACE: `markdown::checkbox` accepts the bare form, and the space is
//! load-bearing because `state::is_blank` compares the buffer to this const
//! with `==`.

pub const DEFAULT: &str = "\
## Status


## Next

[ ] 

## Notes

";
