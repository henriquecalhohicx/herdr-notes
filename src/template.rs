//! The skeleton a fresh note is seeded with on its first edit. One const, so
//! `state::is_blank` (which treats the pristine template as nothing worth a
//! file) and the empty-note preview can never drift from what gets seeded.
//!
//! `Next` intentionally ships one empty bullet-less checkbox with a trailing
//! space — `markdown::checkbox` accepts the bare form, and the space puts the
//! edit cursor where the first task's text goes.

pub const DEFAULT: &str = "\
## Status
<one line: where this stands>

## Next
[ ] 

## Notes
";
