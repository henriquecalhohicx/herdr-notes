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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_has_a_blank_line_after_every_heading() {
        // Pins the exact line list rather than just the Status insertion
        // point (`app::tests::first_edit_seeds_the_template` covers that):
        // a future edit to this const could reflow the blank lines around
        // `## Next`/`## Notes` while still passing every OTHER existing
        // check (`is_blank`'s `==`, `enter_edit`'s row-1 cursor) since none
        // of those look past the Status section. The `[ ] ` line's trailing
        // space is part of this literal comparison too — `cat -A` is the
        // manual equivalent for anyone editing the const by hand.
        let lines: Vec<&str> = DEFAULT.split('\n').collect();
        assert_eq!(
            lines,
            vec!["## Status", "", "", "## Next", "", "[ ] ", "", "## Notes", "", ""],
            "a blank line must sit between every heading and its content"
        );
    }
}
