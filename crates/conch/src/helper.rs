//! [`ConchHelper`]: the single struct implementing every one of
//! rustyline's four small `Completer`/`Highlighter`/`Hinter`/`Validator`
//! traits, combined via the [`rustyline::Helper`] supertrait and wired in
//! with `Editor::set_helper` — see `main.rs`'s own `run_interactive`.
//!
//! # Why this is hand-implemented rather than `#[derive(Helper, ...)]`
//!
//! rustyline 18.0.1's own `derive` feature (`rustyline-derive`) is
//! **not** one of its five default features — confirmed by reading that
//! crate's own `Cargo.toml`: `default = ["custom-bindings", "with-dirs",
//! "with-file-history"]`, with `derive` requiring `features = ["derive"]`
//! explicitly. Rather than take on that extra feature/dependency edge,
//! this hand-writes the four trait impls instead (split across this
//! module, `completion.rs`, and `highlight.rs`) — straightforward since
//! [`rustyline::Helper`] itself is just an empty marker trait requiring
//! `Self: Completer + Hinter + Highlighter + Validator` (confirmed by
//! reading rustyline 18.0.1's own `lib.rs`: `pub trait Helper where Self:
//! Completer + Hinter + Highlighter + Validator {}`), and every method on
//! all four already has a sensible default impl, so nothing here needs
//! to reinvent what the derive macro would have generated beyond the
//! handful of methods each trait impl actually overrides.
//!
//! # Why `Validator` is left at its default (always `Valid`)
//!
//! Real bash's `PS2` is a genuinely *different* prompt string shown only
//! while a construct is still open — but rustyline's own `Validator`/
//! `ValidationResult::Incomplete` mechanism (which drives its *built-in*
//! multi-line-editing-within-one-`readline()`-call feature) has no
//! continuation-prompt hook at all: confirmed by reading rustyline
//! 18.0.1's own `edit.rs`/`validate.rs` — an `Incomplete` result just
//! inserts a newline into the *same* buffer under the *same* prompt, with
//! no way to swap in a different prompt string for the continuation
//! line(s). Getting an actual distinct `PS2` therefore means **not**
//! using that built-in feature at all: `main.rs`'s own `run_interactive`
//! hand-rolls the continuation loop instead (separate `readline(PS2)`
//! calls, joined with `\n`), driven by
//! `conch_shell_parser::ParseError::is_incomplete_input` rather than a
//! duplicate ad hoc "is this incomplete" check. Since nothing here ever
//! relies on rustyline's own `Incomplete` handling, [`Validator`] is left
//! at its default (`Ok(ValidationResult::Valid(None))` — Enter always
//! submits immediately), which is exactly what lets `main.rs`'s own loop
//! see every line back right away and decide for itself.

use std::cell::RefCell;
use std::rc::Rc;

use rustyline::Helper;
use rustyline::completion::FilenameCompleter;
use rustyline::hint::{Hinter, HistoryHinter};
use rustyline::validate::Validator;

use conch_shell_core::CompletionState;

/// See this module's own docs for the overall shape and why each trait
/// is implemented the way it is.
pub struct ConchHelper {
    /// The argument-position (filename) completion half — see
    /// `completion.rs`'s own module docs for why this is delegated to
    /// rustyline's own battle-tested implementation wholesale rather than
    /// reimplemented.
    pub(crate) filename_completer: FilenameCompleter,
    /// The history-based inline-suggestion half ("fish-style
    /// autosuggestion") — rustyline's own built-in
    /// [`HistoryHinter`] is exactly this behavior already, reused
    /// directly rather than reimplemented (see [`rustyline::hint::Hinter`]'s
    /// own impl below).
    pub(crate) hinter: HistoryHinter,
    /// See [`CompletionState`]'s own docs for why this lives behind
    /// `Rc<RefCell<_>>` rather than as a plain field.
    pub(crate) completion_state: Rc<RefCell<CompletionState>>,
}

impl ConchHelper {
    #[must_use]
    pub fn new(completion_state: Rc<RefCell<CompletionState>>) -> Self {
        Self {
            filename_completer: FilenameCompleter::new(),
            hinter: HistoryHinter::new(),
            completion_state,
        }
    }
}

impl Hinter for ConchHelper {
    type Hint = String;

    fn hint(&self, line: &str, pos: usize, ctx: &rustyline::Context<'_>) -> Option<Self::Hint> {
        self.hinter.hint(line, pos, ctx)
    }
}

/// See this module's own docs for why this stays at rustyline's default
/// (always `Valid`) rather than implementing `ValidationResult::Incomplete`
/// detection here.
impl Validator for ConchHelper {}

impl Helper for ConchHelper {}
