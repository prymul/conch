//! Syntax highlighting's thin [`rustyline::highlight::Highlighter`]
//! adapter — ANSI-wraps [`conch_shell_core::classify`]'s pure span output
//! (that's where the actual classification logic and its own unit tests
//! live; see that module's docs for why: `tests/conch-difftest`-style
//! reachability isn't actually relevant here, since bash has no syntax
//! highlighting to compare against, but it's the same "pure core in the
//! library crate, thin adapter in the binary crate" shape every other
//! Phase 6 feature uses).

use std::borrow::Cow;
use std::fmt::Write as _;

use conch_shell_core::SpanKind;
use rustyline::highlight::{CmdKind, Highlighter};

use crate::helper::ConchHelper;
use crate::quoting::find_unclosed_quote;

/// ANSI SGR "reset" — closes whatever [`sgr_for`] opened.
const RESET: &str = "\x1b[0m";

/// The ANSI SGR sequence used to color a given [`SpanKind`] — a fixed,
/// conventional palette (green for a recognized command, yellow for a
/// quoted string, magenta for an expansion, bold blue for a keyword,
/// cyan for an operator, dim for a comment) matching the same
/// color-per-token-kind convention most shells/editors with syntax
/// highlighting already use, not a novel scheme.
fn sgr_for(kind: SpanKind) -> &'static str {
    match kind {
        SpanKind::Keyword => "\x1b[1;34m",
        SpanKind::Command => "\x1b[32m",
        SpanKind::SingleQuoted | SpanKind::DoubleQuoted => "\x1b[33m",
        SpanKind::Variable => "\x1b[35m",
        SpanKind::Operator => "\x1b[36m",
        SpanKind::Comment => "\x1b[2m",
    }
}

impl Highlighter for ConchHelper {
    fn highlight<'l>(&self, line: &'l str, _pos: usize) -> Cow<'l, str> {
        let known_commands = self.completion_state.borrow();
        let mut names: Vec<String> = Vec::with_capacity(
            known_commands.functions.len()
                + known_commands.aliases.len()
                + known_commands.builtins.len(),
        );
        names.extend(known_commands.functions.iter().cloned());
        names.extend(known_commands.aliases.iter().cloned());
        names.extend(known_commands.builtins.iter().cloned());
        drop(known_commands);

        let spans = conch_shell_core::classify(line, &names);
        if spans.is_empty() {
            return Cow::Borrowed(line);
        }

        let mut out = String::with_capacity(line.len() + spans.len() * 12);
        let mut last = 0;
        for span in spans {
            out.push_str(&line[last..span.start]);
            let _ = write!(out, "{}", sgr_for(span.kind));
            out.push_str(&line[span.start..span.end]);
            out.push_str(RESET);
            last = span.end;
        }
        out.push_str(&line[last..]);
        Cow::Owned(out)
    }

    fn highlight_char(&self, line: &str, pos: usize, _kind: CmdKind) -> bool {
        let pos = pos.min(line.len());
        if find_unclosed_quote(&line[..pos]).is_some() {
            return true;
        }
        line[..pos].chars().next_back().is_some_and(|c| {
            matches!(c, '\'' | '"' | '#' | '$' | '`') || conch_shell_core::is_break_char(c)
        })
    }
}
