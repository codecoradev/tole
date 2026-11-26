//! The terminal state the UI owns for its lifetime: raw mode, keyboard
//! enhancement and bracketed paste, restored on drop.

use std::io;

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use crossterm::{execute, queue};

/// Raw mode (+ keyboard enhancement, so Shift+Enter is reported on terminals
/// that speak the kitty protocol; others ignore the sequence; + bracketed
/// paste, #560, so a pasted block arrives as one event instead of keys) for
/// the guard's lifetime; Drop restores all three even when a panic unwinds
/// through `run`.
pub struct TerminalGuard;

impl TerminalGuard {
    pub fn enter() -> io::Result<Self> {
        enable_raw_mode()?;
        let mut out = io::stdout();
        let _ = queue!(
            out,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES),
            EnableBracketedPaste
        );
        let _ = io::Write::flush(&mut out);
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            PopKeyboardEnhancementFlags
        );
        let _ = disable_raw_mode();
    }
}
