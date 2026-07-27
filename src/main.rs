//! herdr-notes — a persistent markdown notes pane for herdr: one scrollable
//! note with preview/edit modes, autosaved to a JSON file that survives
//! restarts.
//!
//! The `--*` stdin→stdout helper modes serve the launcher scripts — see
//! launch.rs.

mod app;
mod ipc;
mod launch;
mod markdown;
mod prompts;
mod state;
mod template;

use std::io::Read;
use std::time::Duration;

use crossterm::event::{self, Event};

/// argv[1] as a matchable string. Deliberately NOT `std::env::args()`, which
/// PANICS on a non-Unicode argument — and `nth(1)` forces argv[0] (the exe
/// path) through that check first, so the panic would fire before the
/// `--capture-prompt` arm exists to swallow anything. A panic there is exit
/// 101 plus a stderr dump: precisely the two things the `UserPromptSubmit`
/// contract forbids, on the one path whose whole point is that no input can
/// cost the user their message.
///
/// The conversion is LOSSY rather than dropping: a non-Unicode argument keeps
/// its place in the match and lands on the unknown-argument arm, exactly where
/// any other unrecognized argument goes, instead of silently reading as "no
/// argument" and launching the TUI.
fn first_arg(argv1: Option<std::ffi::OsString>) -> Option<String> {
    argv1.map(|a| a.to_string_lossy().into_owned())
}

fn main() -> std::io::Result<()> {
    match first_arg(std::env::args_os().nth(1)).as_deref() {
        Some("--launch-decision") => {
            println!("{}", launch::launch_decision(&read_stdin()?, state::unix_now()));
            return Ok(());
        }
        Some("--focused-pane") => {
            println!("{}", launch::focused_pane(&read_stdin()?));
            return Ok(());
        }
        Some("--open-plan") => {
            println!("{}", launch::open_plan(&read_stdin()?));
            return Ok(());
        }
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
        Some(other) => {
            eprintln!("herdr-notes: unknown argument `{other}`");
            eprintln!("usage: herdr-notes [--launch-decision|--focused-pane|--open-plan|--capture-prompt]");
            std::process::exit(2);
        }
        None => {}
    }

    let mut terminal = ratatui::init();
    let mut app = app::App::new();
    let result = run(&mut terminal, &mut app);
    app.finalize();
    ratatui::restore();
    result
}

/// Event loop with a short poll so the liveness heartbeat keeps stamping and
/// the debounced autosave keeps flushing while idle.
fn run(terminal: &mut ratatui::DefaultTerminal, app: &mut app::App) -> std::io::Result<()> {
    loop {
        terminal.draw(|frame| app.draw(frame))?;
        // Non-key events (resize, focus, …) simply fall through to a redraw.
        if event::poll(Duration::from_millis(500))?
            && let Event::Key(key) = event::read()?
            && app.on_key(key)
        {
            return Ok(());
        }
        // Every iteration — not only on poll timeout — so sustained input
        // (held-key auto-repeat, a long paste) can never starve the liveness
        // stamp into REPLACE territory or hold back the debounced autosave.
        // Both self-throttle, so this is cheap.
        app.heartbeat();
        app.maybe_flush();
    }
}

fn read_stdin() -> std::io::Result<String> {
    let mut buf = String::new();
    std::io::stdin().read_to_string(&mut buf)?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::first_arg;
    use std::ffi::OsString;

    /// An `OsString` that is not valid Unicode — the input `std::env::args()`
    /// panics on. Built per-platform because that is the only way to make one.
    fn non_unicode() -> OsString {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt;
            // A lone high surrogate: valid UTF-16 storage, not valid Unicode.
            OsString::from_wide(&[0x0041, 0xD800, 0x0042])
        }
        #[cfg(not(windows))]
        {
            use std::os::unix::ffi::OsStringExt;
            OsString::from_vec(vec![0x41, 0x80, 0x42])
        }
    }

    #[test]
    fn a_non_unicode_argument_does_not_panic_and_is_just_an_unknown_argument() {
        let got = first_arg(Some(non_unicode())).expect("a non-Unicode argument still yields a string");
        // Whatever it lossily spells, it must not collide with a real mode —
        // so `main`'s match lands on the `Some(other)` usage-and-exit-2 arm,
        // the same place any other unrecognized argument goes.
        for mode in ["--launch-decision", "--focused-pane", "--open-plan", "--capture-prompt"] {
            assert_ne!(got, mode);
        }
        assert!(got.contains('\u{fffd}'), "the invalid unit is replaced, not dropped: {got:?}");
    }

    #[test]
    fn first_arg_passes_real_modes_through_unchanged() {
        assert_eq!(first_arg(Some(OsString::from("--capture-prompt"))).as_deref(), Some("--capture-prompt"));
        assert_eq!(first_arg(None), None, "no argument at all still starts the TUI");
    }
}
