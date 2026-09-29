//! Command execution: simple commands, pipelines, `;`/`&&`/`||`-joined
//! lists (Phase 1), and compound commands — `if`/`for`/`while`/`until`/
//! `case`, subshells, and brace groups, with `break`/`continue` (Phase 3).
//!
//! **Known Phase 1 simplification:** pipeline stages are run sequentially,
//! with each stage's stdout fully buffered in memory before the next stage
//! runs (rather than wiring real concurrent OS pipes between child
//! processes). This is what lets a builtin — which has no OS process or
//! file descriptors of its own — sit anywhere in a pipeline next to
//! external commands without special-casing. The real cost: an infinite or
//! very large producer (`yes | head`) will hang or exhaust memory instead
//! of streaming. Concurrent OS-pipe wiring for all-external pipeline runs
//! is a reasonable later improvement; not attempted here.
//!
//! **Known Phase 3 simplification, for the same underlying reason:** a
//! *compound* command used as a pipeline stage (`(cmd) | grep x`, `{
//! cmd; } | grep x`, `if ...; fi | grep x`, ...) doesn't participate in
//! that same stdin/stdout buffering — it always reads/writes the real
//! inherited streams, rather than having its combined output captured and
//! piped the way a simple command's is. Only `exec_simple` has the
//! in-memory buffer plumbing today; extending that same treatment to an
//! entire compound command's body (potentially many nested commands) is a
//! reasonable later improvement, not attempted here. Relatedly, a
//! compound command's own trailing redirect (`{ ...; } > file`, `if
//! ...; fi > file`, ...) is parsed (`conch-shell-parser`) but not yet
//! executed — seeing one is a clear "not yet supported" error rather than
//! silently running the body with its output going to the wrong place;
//! see [`exec_compound`].
//!
//! Async (`&`) pipelines are parsed (see [`conch_shell_parser::Separator`])
//! but always run synchronously in the foreground — real backgrounding is
//! Phase 4 job control, per the project roadmap.
//!
//! # `break`/`continue` propagation
//!
//! The `break`/`continue` builtins (`conch-shell-builtins`) can only
//! communicate through [`Shell::pending_control_flow`] — see
//! [`crate::ControlFlow`]'s docs for why. Every function in this module
//! that runs a sequence of things ([`exec_command_list`], [`exec_and_or`])
//! checks it after each step and stops early if it's set, so a pending
//! signal reaches the nearest loop-execution code
//! ([`exec_while_or_until`], [`exec_for`]) without whatever would
//! otherwise run next (another `&&`/`||` pipeline, another list item)
//! running in between — confirmed against real bash this matters even for
//! exit status: `break && echo unreached` must not run the `echo`, even
//! though `break`'s own exit status is `0` (which `&&` would otherwise
//! treat as "keep going"). Each loop level a signal passes through
//! decrements its `n` by one ([`take_loop_control_flow`]), stopping there
//! once `n` reaches `1`.
//!
//! A subshell ([`exec_subshell`]) never has to sever this propagation at
//! all, let alone specially: it runs as a genuinely separate process
//! (see that function's docs for why that's a hard requirement, not a
//! style choice), so its own `break`/`continue`s only ever exist in that
//! child's own, entirely separate [`Shell`] — there's no shared
//! `pending_control_flow`/`loop_depth` state to leak *into* in the first
//! place.

use std::collections::HashMap;
use std::io::Write as _;
use std::process::Stdio;

use conch_shell_parser::{
    AndOrList, CaseClause, Command, CommandList, CommandListItem, CompoundCommand,
    CompoundCommandKind, ForClause, FunctionDefinition, IfClause, LogicalOp, Parameter, Pipeline,
    Redirect, RedirectOperator, Separator, SimpleCommand, SpecialParameter, Word, WordSegment,
};
use nix::errno::Errno;
use nix::sys::signal::Signal;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::{Pid, tcsetpgrp};

use crate::expand::{expand_word_as_pattern, glob_match};
use crate::{
    ControlFlow, ExpandError, ForegroundInterrupt, Shell, brace_expand, expand_word_fields,
    expand_word_single,
};

/// Brace-expands then field-expands each word in `words`, in order,
/// flattening every resulting field into one sequence — the combined
/// pass command names and arguments both go through (POSIX doesn't
/// special-case either position for brace expansion or splitting).
fn expand_words_fields<'a>(
    words: impl Iterator<Item = &'a Word>,
    shell: &mut Shell,
) -> Result<Vec<String>, ExpandError> {
    let mut fields = Vec::new();
    for word in words {
        for variant in brace_expand(word)? {
            fields.extend(expand_word_fields(&variant, shell)?);
        }
    }
    Ok(fields)
}

/// Runs a full parsed command list against `shell`, returning the exit
/// status of the last command run (POSIX `$?`).
///
/// Also the function that runs the *body* of every compound command
/// (POSIX `compound_list` — an `if`/`while`/`until`/`for`'s condition and
/// body, a brace group's or subshell's contents, one `case` arm) — see
/// the module docs for why an early stop here (on a pending `break`/
/// `continue`) is important even for the top-level program, not just
/// loop bodies specifically.
pub fn exec_command_list(list: &CommandList, shell: &mut Shell) -> i32 {
    let mut status = shell.last_status;
    for item in &list.items {
        let (item_status, errexit_eligible) = exec_command_list_item(item, shell);
        status = item_status;
        shell.last_status = status;
        // `set -e` (errexit) — see `Shell::errexit`'s own docs for
        // exactly what this checkpoint's placement does and doesn't
        // exempt. Skipped while `unwinding` (a pending `break`/
        // `continue`/`return`, or a stopped/SIGINT-killed foreground
        // job) — that's a distinct, higher-priority signal already
        // being propagated, and letting *this* additionally trigger a
        // hard process exit on the same statement would race the two
        // rather than cleanly deferring to whichever the caller
        // actually needs to see first. Also skipped when
        // `!errexit_eligible` — an `&&`/`||` chain's own final resolved
        // status that came from a non-last member (short-circuiting the
        // rest) is POSIX-exempt even though it's still what `$?` reports
        // for the whole statement; see [`exec_and_or`]'s own docs for
        // the exact rule and empirical grounding.
        if shell.errexit
            && status != 0
            && errexit_eligible
            && shell.errexit_suppressed == 0
            && !unwinding(shell)
        {
            shell.run_exit_trap();
            std::process::exit(status);
        }
        // A safe checkpoint (per `conch-shell-core::signals`' module
        // docs) between every command — reaps any background job that
        // changed state and runs any `trap`'d signal's command text.
        // Never from inside a signal handler itself.
        shell.process_pending_signals();
        if shell.foreground_interrupt.is_some() {
            // The job just run (this iteration or a nested one still
            // unwinding through) was stopped or killed by SIGINT — see
            // `ForegroundInterrupt`'s docs for why this propagates all
            // the way back to the prompt unconditionally, the same way
            // an over-large `break`/`continue` still stops *this* list
            // even though it isn't this list's own signal to fully
            // consume.
            break;
        }
        match shell.pending_control_flow {
            // A break/continue level exceeding every loop that actually
            // encloses it (`shell.loop_depth == 0` here means: none do,
            // not even one level up) has nowhere left to go — confirmed
            // against real bash/dash this discards the signal (with the
            // same diagnostic) and *keeps running* the rest of this
            // list, rather than treating it as fatal to the whole list
            // the way a level that's still headed for a real enclosing
            // loop must.
            Some(ControlFlow::Break(_) | ControlFlow::Continue(_)) if shell.loop_depth == 0 => {
                warn_on_unhandled_control_flow(shell);
            }
            Some(_) => break,
            None => {}
        }
    }
    status
}

/// Dispatches one [`CommandListItem`] on its [`Separator`]: an `&`-marked
/// item backgrounds the *entire* `and_or` chain (see [`exec_async`] and
/// [`Separator::Async`]'s own docs for why the whole chain, not just its
/// first pipeline); every other item just runs synchronously as before.
///
/// Returns the item's exit status plus whether that status is eligible
/// to trigger `errexit` — see [`exec_and_or`]'s own docs for the real
/// rule this threads through. An async item's status is always `0`
/// (POSIX; see [`exec_async`]'s own docs), so its eligibility bit is
/// unobservable either way — `true` here is an arbitrary but harmless
/// placeholder, never one that could actually cause a spurious exit.
fn exec_command_list_item(item: &CommandListItem, shell: &mut Shell) -> (i32, bool) {
    match &item.separator {
        Separator::Async(source) => (exec_async(&item.and_or, source, shell), true),
        Separator::Sequential | Separator::None => exec_and_or(&item.and_or, shell),
    }
}

/// Runs a full parsed *program* (the top-level input — a whole `-c`
/// string, script file, or one interactive line) against `shell`. This
/// is what `conch`'s binary crate should call, not [`exec_command_list`]
/// directly — it's [`exec_command_list`] plus the one thing only a true
/// top-level call needs: discarding (with the same diagnostic real
/// bash's own "only meaningful in a `for', `while', or `until' loop"
/// message conveys) any `break`/`continue` still pending once there's
/// truly no more program left to run it against.
///
/// Confirmed against real bash this is exactly what happens for `break
/// n`/`continue n` with `n` greater than the current loop nesting depth:
/// it unwinds *every* enclosing loop (same as `n` exactly matching the
/// depth — see [`take_loop_control_flow`]) *and* prints that same
/// diagnostic, since by the time the outermost loop's own level is
/// reached there's nothing left to catch the remainder. [`exec_command_list`]
/// itself can't be the one to do this discarding: it's also the function
/// every loop body/if branch recurses through (a subshell body doesn't —
/// see [`exec_subshell`]'s docs), and *those* callers need to still see
/// a pending signal after it returns in order to act on it correctly.
pub fn exec_program(list: &CommandList, shell: &mut Shell) -> i32 {
    let status = exec_command_list(list, shell);
    warn_on_unhandled_control_flow(shell);
    // A `ForegroundInterrupt` (see its own docs) has, by construction,
    // already fully unwound every intervening loop/`if`/function-call
    // frame by the time it reaches here — nothing further to warn about,
    // just clear it so it doesn't incorrectly also abort the *next*
    // top-level program run against this same `Shell` (the interactive
    // REPL loop calls this once per line, reusing one `Shell` across
    // every line).
    shell.foreground_interrupt = None;
    status
}

/// The exact message real bash prints for a `break`/`continue` with
/// nowhere left to go — shared by [`warn_on_unhandled_control_flow`] (the
/// top-level-program backstop) and [`exec_function_call`] (the
/// function-call-boundary backstop — see its docs for why a function call
/// needs this exact same discard-and-warn treatment).
const UNHANDLED_LOOP_CONTROL_MSG: &str =
    "conch: break/continue: only meaningful in a `for', `while', or `until' loop";

/// See [`exec_program`]'s docs.
fn warn_on_unhandled_control_flow(shell: &mut Shell) {
    // `Return` should never actually reach here — the `return` builtin
    // (`conch-shell-builtins`) only ever sets it when
    // `Shell::in_function_call()` is true, and `exec_function_call`
    // always consumes it before returning — but this is still written to
    // handle it correctly (rather than printing a misleading
    // break/continue-shaped message) if that invariant were ever
    // violated, instead of relying on it silently.
    match shell.pending_control_flow.take() {
        Some(ControlFlow::Break(_) | ControlFlow::Continue(_)) => {
            eprintln!("{UNHANDLED_LOOP_CONTROL_MSG}");
        }
        Some(ControlFlow::Return(_)) => {
            eprintln!("conch: return: can only `return' from a function or sourced script");
        }
        None => {}
    }
}

/// Whether the executor should stop running whatever list/loop/`if`-chain
/// it's in the middle of, right now, without running anything else —
/// true for a pending `break`/`continue`/`return` ([`ControlFlow`]) *or*
/// a [`ForegroundInterrupt`] (a foreground job just stopped or was
/// killed by `SIGINT`) — see [`ForegroundInterrupt`]'s own docs for why
/// these are two separate fields checked together here rather than one
/// merged enum.
fn unwinding(shell: &Shell) -> bool {
    shell.pending_control_flow.is_some() || shell.foreground_interrupt.is_some()
}

/// Runs a full `&&`/`||`-joined pipeline chain, returning its final exit
/// status plus whether that status is *eligible* to trigger `errexit`
/// (POSIX 2.14 / bash manual §4.3.1's "...except the command following
/// the final `&&` or `||`" exemption — see [`Shell::errexit`]'s own docs
/// for the full rule this implements).
///
/// The exemption is broader than "an intermediate command's own failure
/// is never separately visible to the errexit checkpoint" (trivially
/// true by construction — only this function's own final, fully-resolved
/// status ever reaches [`exec_command_list`]'s checkpoint at all): it
/// also covers the *chain's own final resolved status*, whenever that
/// status came from a member other than the chain's syntactically last
/// one — i.e. an earlier command's failure short-circuited the rest, so
/// the last member never even ran. Confirmed directly against real bash
/// and dash: `set -e; false && echo hi` does **not** abort (`false` is
/// not the chain's last command — `echo` is, and it never got a chance
/// to run — so `false`'s failure is exempt even though it's also the
/// whole chain's own final result, since nothing after it ran to
/// override it).
///
/// Eligibility tracks whether the chain's syntactically *last* member
/// actually ran — not merely "no short-circuit happened anywhere in the
/// chain": a middle member can be skipped while a later one still runs
/// and updates the status, if its own operator's short-circuit condition
/// happens to already be satisfied by whatever status was left over.
/// Confirmed against real bash: `set -e; false && true || false` *does*
/// abort — the middle `true` is skipped (the preceding `&&` needs a
/// zero status, but `false` left it nonzero), yet the final `false` still
/// runs (its own preceding `||` needs a *nonzero* status, which the
/// skipped-over failure conveniently still provides) — and since it *is*
/// the chain's actual last member and it failed, errexit correctly
/// applies.
fn exec_and_or(and_or: &AndOrList, shell: &mut Shell) -> (i32, bool) {
    let mut status = exec_pipeline(&and_or.first, shell);
    // `first` is trivially the chain's own "last member" whenever there
    // are no further `&&`/`||`-joined pipelines at all.
    let mut last_member_ran = and_or.rest.is_empty();
    if unwinding(shell) {
        return (status, last_member_ran);
    }
    let last_index = and_or.rest.len().saturating_sub(1);
    for (index, (op, pipeline)) in and_or.rest.iter().enumerate() {
        let should_run = match op {
            LogicalOp::And => status == 0,
            LogicalOp::Or => status != 0,
        };
        if should_run {
            status = exec_pipeline(pipeline, shell);
            last_member_ran = index == last_index;
            if unwinding(shell) {
                return (status, last_member_ran);
            }
        } else {
            // Skipped by short-circuiting: whatever status is currently
            // held over is *not* this (skipped) member's own last-ness,
            // even if a later member goes on to re-satisfy its own
            // operator and actually run (see this function's own docs'
            // `false && true || false` example) -- that later run's own
            // branch above will correctly overwrite this if so.
            last_member_ran = false;
        }
    }
    (status, last_member_ran)
}

/// Backgrounds an entire `and_or` list (`&`) — POSIX 2.9.3.1: run in "a
/// subshell environment," exactly the isolation [`exec_subshell`] already
/// provides for `(...)`, generalized here rather than building parallel
/// infrastructure (per this phase's design review): re-exec `conch -c
/// <source>` as a genuine child process, given its own process group so
/// it's a real job (see `conch-shell-core::signals`), and — the one
/// thing that makes this genuinely *async* rather than just another
/// subshell — never waited for here at all; [`Shell::reap_children`]
/// (driven off `SIGCHLD`, see that module's docs) is what eventually
/// notices it finished.
///
/// `source` is `item.separator`'s own captured raw text — *except* in two
/// cases, both of which exist for the identical underlying reason (avoid
/// double-wrapping a re-exec that the freshly spawned child would just
/// immediately do *again*, one layer down, itself):
///
/// - `and_or` is already, itself, exactly one bare subshell (`(cmd) &`,
///   no `&&`/`||`, no pipe): reusing
///   [`conch_shell_parser::SubshellBody::source`] directly there avoids
///   double-wrapping (re-exec'ing `conch -c "(cmd)"` would itself spawn a
///   second, redundant subshell re-exec internally once that child parses
///   it back out) — see this phase's design review for the same
///   reasoning.
/// - `and_or` is exactly one bare `"$0" -c <script>` (or `<this binary's
///   own path> -c <script>`) invocation — see
///   [`bare_self_reexec_script`]'s docs for why this is the same
///   double-wrapping hazard in different clothes, and a real, observed
///   bug (not a theoretical one) fixed here: `kill $!`/`wait $!` need
///   `$!` to be the pid of the process that's actually going to run
///   `<script>` (and thus actually own whatever `trap`/signal
///   disposition it sets for itself), not an intermediate hop that never
///   itself runs any of `<script>`'s own text.
///
/// POSIX: the exit status of an asynchronous list is always `0`
/// (confirmed against real bash) — this shell has never even *started*
/// waiting for the job by the time this returns, so `0` is the only
/// value that could ever be correct here regardless.
fn exec_async(and_or: &AndOrList, source: &str, shell: &mut Shell) -> i32 {
    if let [
        Command::Compound(CompoundCommand {
            kind: CompoundCommandKind::Subshell(body),
            redirects,
        }),
    ] = and_or.first.commands.as_slice()
        && and_or.rest.is_empty()
        && redirects.is_empty()
    {
        spawn_background_job(body.source.as_str(), shell, true);
        return 0;
    }

    // The self-reexec shape needs one real expansion (the inner
    // `<script>` word) to produce the text a nested `conch -c` would
    // actually run — unlike the subshell case above, there's no raw
    // source span to reuse verbatim, since `<script>` is an ordinary
    // *word* (quoting/expansion sites included), not literal shell
    // script text framed by parens. See `bare_self_reexec_script`'s docs
    // for why `shell` is only ever consulted here for a zero-expansion
    // AST-shape check, and this is the *only* expansion this function
    // ever performs — a non-matching shape falls through to the
    // `spawn_background_job(source, ...)` call below having expanded
    // nothing at all, so there's no risk of the fallback path's own
    // (freshly spawned child's) expansion of the same text ever running
    // twice.
    if let Some(script) = bare_self_reexec_script(and_or, shell)
        && let Ok(inner_source) = expand_word_single(script, shell)
    {
        // `subshell_pid: false` — see `spawn_background_job`'s own docs
        // for why this one call site is the exception: this spawn is
        // standing in for what would otherwise be an *ordinary
        // external-command* spawn (`exec_external`, from a would-be
        // wrapper's own foreground execution of `<self> -c <script>`),
        // never a subshell of the *current* script, so `$$` inside
        // `<script>` must read as this spawned process's own real pid
        // (exactly the pid registered as `$!` here — matching real
        // bash's single fork+exec, where the same process serves both
        // roles) rather than being pinned to the top-level shell's pid
        // the way a genuine async-subshell spawn's `$$` is.
        spawn_background_job(&inner_source, shell, false);
        return 0;
    }

    spawn_background_job(source, shell, true);
    0
}

/// Recognizes `and_or` as exactly one bare `Command::Simple` invocation of
/// the shape `<self> -c <script>` — where `<self>` is `$0` (bare or
/// double-quoted — both spellings are common) or a literal path identical
/// to either this process's own [`std::env::current_exe`] or
/// [`Shell::arg0`] — with no assignments, no redirects, and no trailing
/// arguments beyond `<script>` itself. Returns the `<script>` word on a
/// match, for [`exec_async`] to expand.
///
/// **Why this matters**: [`spawn_background_job`] always re-execs `conch
/// -c <source>` as the backgrounded job. If `<source>` is itself exactly
/// `<self> -c <script>`, the freshly spawned job (call it wrapper-1) does
/// nothing but turn around and run *that* as an ordinary foreground
/// external command — spawning a *second* nested `conch -c <script>`
/// (wrapper-2) and blockingly waiting for it. `$!`, though, is already
/// fixed to wrapper-1's pid by the time wrapper-2 is ever spawned:
/// wrapper-1 never itself runs any of `<script>`'s own text (in
/// particular, never installs any `trap` `<script>` sets on itself), so
/// signaling `$!` reaches the wrong process entirely — confirmed via a
/// real reproduction, not a theoretical concern: `"$0" -c "trap '' TERM;
/// sleep 5" &` backgrounded this way has `kill $!` hit wrapper-1 (which
/// dies immediately, TERM's disposition never having been touched there)
/// instead of wrapper-2 (which actually ran `trap '' TERM` on itself).
/// Real bash never has this problem because it forks exactly once for the
/// whole `&`: that one child directly `execve`s into `<self>`, becoming
/// wrapper-2 in place rather than forking it as a distinct process one
/// layer down.
///
/// This function only ever inspects `and_or`'s AST shape and (for the
/// literal-path spelling) reads `shell.arg0`/`current_exe()` — it
/// deliberately never *expands* the command name itself, even though
/// `expand_word_single` could resolve a bare `$0` cheaply and
/// side-effect-free: a command name built from something with genuine
/// expansion side effects (a contrived `` `echo "$0"` -c "..." & `` using
/// command substitution to spell out the name, say) must never be
/// evaluated *here*, on a path that might turn out not to match and fall
/// back to [`spawn_background_job`]'s own from-scratch re-parse — that
/// would run the exact same side-effecting expansion a second time. Every
/// check below is either pure AST-shape matching or a side-effect-free
/// OS query, so a non-matching shape is guaranteed to reach
/// [`exec_async`]'s fallback having evaluated nothing at all.
///
/// Known, accepted narrow gap (consistent with this crate's existing
/// precedent for similar re-exec plumbing — see [`exec_subshell`]'s own
/// "known gap" docs): the one expansion this enables ([`exec_async`]'s
/// call to `expand_word_single` on `<script>`) now runs directly against
/// the *live* top-level `shell`, rather than inside a freshly spawned
/// child's own disposable, from-scratch-reconstructed `Shell` the way it
/// would for the ordinary (non-unwrapped) async path. For the overwhelming
/// majority of real uses (a literal or variable-interpolated script
/// string) this is unobservable — the expanded text is identical either
/// way. The one case where it isn't: a `${var:=default}`-shaped
/// assign-if-unset inside `<script>` would, with this fast path taken,
/// mutate `var` in the *parent* shell's own live variable table (matching
/// what a genuine `fork` would do, since `<script>` was always going to
/// see the parent's variables one way or another) rather than only in a
/// disposable copy that dies with the job — a real difference from the
/// general async path's isolation, but one confirmed to require a
/// deliberately unusual construction to observe at all, and not something
/// this pass's fix is worth further scope to close.
fn bare_self_reexec_script<'a>(and_or: &'a AndOrList, shell: &Shell) -> Option<&'a Word> {
    if !and_or.rest.is_empty() {
        return None;
    }
    let [Command::Simple(simple)] = and_or.first.commands.as_slice() else {
        return None;
    };
    if !simple.assignments.is_empty() || !simple.redirects.is_empty() {
        return None;
    }
    let name = simple.name.as_ref()?;
    if !is_self_invocation(name, shell) {
        return None;
    }
    let [flag, script] = simple.args.as_slice() else {
        return None;
    };
    if flag.as_plain_literal() != Some("-c") {
        return None;
    }
    Some(script)
}

/// Whether `name` is, purely by its AST shape or a literal-text
/// comparison (never by expanding anything — see
/// [`bare_self_reexec_script`]'s docs for why that matters), certain to
/// resolve to invoking this exact shell binary again: a bare or
/// double-quoted `$0` (`$0`/`"$0"` — both lex to the same
/// [`SpecialParameter::Zero`], see `conch-shell-lexer`'s own token docs),
/// or a plain literal (no quoting or expansion sites at all —
/// [`Word::as_plain_literal`]) identical to either this running process's
/// own [`std::env::current_exe`] or [`Shell::arg0`] (the shell's own
/// recorded `$0`, which `$0`'s own expansion would produce anyway — see
/// `conch-shell-core::expand`'s handling of [`SpecialParameter::Zero`]).
fn is_self_invocation(name: &Word, shell: &Shell) -> bool {
    let is_dollar_zero = |segments: &[WordSegment]| {
        matches!(
            segments,
            [WordSegment::Parameter(Parameter::Special(
                SpecialParameter::Zero
            ))]
        )
    };
    if is_dollar_zero(&name.segments) {
        return true;
    }
    if let [WordSegment::DoubleQuoted(inner)] = name.segments.as_slice()
        && is_dollar_zero(inner)
    {
        return true;
    }
    let Some(literal) = name.as_plain_literal() else {
        return false;
    };
    if literal == shell.arg0 {
        return true;
    }
    std::env::current_exe().is_ok_and(|exe| std::path::Path::new(literal) == exe)
}

/// The actual `conch -c <source>` re-exec + process-group + job-table
/// registration behind [`exec_async`] — see that function's docs.
///
/// Blocks `SIGCHLD` for the fork-and-register window (spawn, then
/// [`crate::job::JobTable::register`]): the GNU libc manual's
/// job-control chapter specifically calls out that a child which exits
/// before its parent finishes its own bookkeeping for it must not get
/// reaped first — without this, an extremely short-lived backgrounded
/// job (`true &`) could have its `SIGCHLD` delivered and (via
/// [`Shell::process_pending_signals`], if some *other* checkpoint
/// happened to run first) reaped before [`crate::job::JobTable::register`]
/// below ever runs, leaving a job-table entry that's permanently stuck
/// `Running` for a process that's already gone.
///
/// `subshell_pid`: `true` for every ordinary async list (the common
/// case, and the bare-subshell fast path in [`exec_async`]) — `$$`
/// inside `source` must read as the *top-level* shell's own pid, per
/// POSIX 2.12's "subshell environment," so this sets `__CONCH_PID`
/// accordingly (see the `env` call below). `false` *only* for
/// [`exec_async`]'s self-reexec fast path
/// ([`bare_self_reexec_script`]): that spawn is standing in for what
/// would otherwise be an ordinary *external-command* spawn
/// (`exec_external`, from a would-be wrapper's own foreground execution
/// of `<self> -c <script>`) one layer down, never a subshell of the
/// *current* script — confirmed as a real, observed regression (not a
/// theoretical concern), caught by this crate's own differential suite:
/// `corpus/phase4/background_and_wait.toml`'s
/// `wait-reports-a-background-jobs-death-by-uncaught-signal-as-128-plus-signal-number`
/// deliberately constructs `"$0" -c "kill -TERM \$\$" &` specifically
/// *because* `$$` inside a genuine subshell can't be used to target only
/// itself (it reads as the top-level shell's own pid there); pinning
/// `__CONCH_PID` for this fast path too would make `kill -TERM $$` hit
/// the top-level shell instead of the freshly spawned job, killing the
/// entire top-level process out from under the very script that
/// backgrounded it.
fn spawn_background_job(source: &str, shell: &mut Shell, subshell_pid: bool) {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            eprintln!("conch: {err}");
            return;
        }
    };

    let mut command = std::process::Command::new(exe);
    command
        .arg("-c")
        .arg(source)
        .current_dir(&shell.cwd)
        .env_clear()
        .envs(&shell.env_vars)
        // POSIX 2.9.3.1: an *ignored* trap (`trap '' SIG`, no command
        // text) is inherited into an async subshell environment too,
        // unlike a *caught* one — see `Shell::ignored_trap_names`'s own
        // docs for why propagating exactly this (and only this) subset
        // of trap state through an environment variable is safe.
        .env("__CONCH_IGNORED_SIGNALS", shell.ignored_trap_names());
    if subshell_pid {
        // An asynchronous list is a POSIX 2.12 "subshell environment"
        // exactly like `(...)` is -- `$$` inside the backgrounded list
        // itself must still read as the *top-level* shell's PID, not
        // this wrapper's own new one (see `Shell::pid`'s docs). This
        // does *not* leak into some further, genuinely separate shell
        // invocation the backgrounded command itself might spawn (e.g.
        // `"$0" -c '...' &`'s own inner `"$0" -c` call) -- that's an
        // ordinary external-command spawn (`exec_external`), which
        // builds its child's environment from `shell.env_vars` alone
        // (already stripped of this key by `Shell::new`), never from
        // this process's raw OS environment -- see this function's own
        // `subshell_pid` docs for the one call site that deliberately
        // sets `subshell_pid: false` because *it itself* is standing in
        // for exactly that kind of ordinary external-command spawn.
        command.env("__CONCH_PID", shell.pid.to_string());
    }
    // New process group (this child becomes its own leader) regardless
    // of whether *this* session has an interactive terminal to hand
    // around — a background job still needs its own pgid so a later
    // `bg`/`fg`-style `SIGCONT` (or any other job-targeted signal, see
    // `crate::job`'s module docs on signaling by pgid) reaches exactly
    // this job and nothing else.
    shell.prepare_child_for_job_control(&mut command, true);

    let mut sigchld = nix::sys::signal::SigSet::empty();
    sigchld.add(Signal::SIGCHLD);
    let _ = sigchld.thread_block();

    match command.spawn() {
        Ok(child) => {
            let pid = Pid::from_raw(child.id() as i32);
            let id = shell.job_table.register(pid, pid, source.to_string());
            shell.last_background_pid = Some(pid);
            if shell.job_control_active {
                eprintln!("[{id}] {pid}");
            }
            // Deliberately not `child.wait()`/dropped-and-forgotten in
            // any special way — reaping happens exclusively through
            // `Shell::reap_children`'s own `waitpid`, never through this
            // `std::process::Child` handle (letting it drop here doesn't
            // reap it either way — see `std::process::Child`'s own docs
            // — so there's nothing extra to do with it).
        }
        Err(err) => {
            eprintln!("conch: {source}: {err}");
        }
    }

    let _ = sigchld.thread_unblock();
}

/// Runs `pgid_leader` (a foreground job's process-group leader — and, in
/// every case this function is used for today, its *only* process) to
/// completion or until it stops, handing it the controlling terminal
/// first (when [`Shell::job_control_active`]) and reclaiming it
/// afterward regardless of outcome. Sets [`Shell::foreground_interrupt`]
/// when the job stopped or was killed by `SIGINT` — see that field's
/// docs for why every "runs a sequence of things" function in this
/// module needs to notice that and unwind.
///
/// Deliberately does *not* pre-register `pgid_leader` in
/// [`Shell::job_table`] the way [`spawn_background_job`] does — matching
/// real bash, only a job that ends up `Stopped` (or was ever
/// backgrounded to begin with) shows up in `jobs` at all. This is
/// race-free under this crate's single-threaded execution model without
/// the `SIGCHLD`-blocking [`spawn_background_job`] needs: nothing else
/// can concurrently call `waitpid` on this exact, not-yet-disclosed pid
/// out from under the blocking call below (control genuinely doesn't
/// return to any other Rust code — including the checkpoint that drives
/// [`Shell::process_pending_signals`]/[`Shell::reap_children`] — until
/// this function's own `waitpid` call itself returns).
/// `existing`: `None` for a freshly spawned job (the common case, from
/// [`exec_external`]'s own job-control branch) — a job that turns out to
/// stop gets a brand-new [`Shell::job_table`] entry
/// ([`crate::job::JobTable::register_stopped`]). `Some(id)` for
/// [`resume_job_in_foreground`]'s use (`fg`, `conch-shell-builtins`):
/// `pgid_leader` here already *is* that existing job's own pgid, so a
/// fresh stop needs to update that *same* entry in place, and a
/// completion (`Exited`/`Signaled`) needs to remove it — there's nothing
/// left to `wait`/`fg`/`bg` once a job that was just brought to the
/// foreground actually finishes there, unlike a plain background
/// completion (which stays queryable — see
/// [`Shell::purge_finished_notified_jobs`]'s docs — until something
/// actually reports it).
fn run_foreground_job(
    shell: &mut Shell,
    pgid_leader: Pid,
    command_text: &str,
    existing: Option<u32>,
) -> i32 {
    let stdin = std::io::stdin();
    if shell.job_control_active {
        let _ = tcsetpgrp(&stdin, pgid_leader);
    }

    let status = loop {
        match waitpid(pgid_leader, Some(WaitPidFlag::WUNTRACED)) {
            Ok(status) => break status,
            Err(Errno::EINTR) => continue,
            Err(_) => break WaitStatus::Exited(pgid_leader, 1),
        }
    };

    let exit_status = match status {
        WaitStatus::Exited(_, code) => {
            if let Some(id) = existing {
                shell.job_table.remove(id);
            }
            code
        }
        WaitStatus::Signaled(_, sig, _) => {
            if let Some(id) = existing {
                shell.job_table.remove(id);
            }
            if sig == Signal::SIGINT {
                shell.foreground_interrupt = Some(ForegroundInterrupt::Interrupted);
            }
            128 + sig as i32
        }
        WaitStatus::Stopped(..) => {
            let id = match existing {
                Some(id) => {
                    if let Some(job) = shell.job_table.get_mut(id) {
                        job.state = crate::JobState::Stopped;
                        job.notified = false;
                    }
                    id
                }
                None => shell.job_table.register_stopped(
                    pgid_leader,
                    pgid_leader,
                    command_text.to_string(),
                ),
            };
            if shell.job_control_active {
                eprintln!("[{id}]+  Stopped                 {command_text}");
                // This *is* the report for this stop -- without marking
                // it here, the interactive prompt loop's own per-prompt
                // notification pass (`report_finished_jobs`, `conch`)
                // would see the same still-`notified: false` job and
                // print the exact same line a second time right before
                // the next prompt (confirmed via a real pty-driven
                // Ctrl-Z smoke test, not assumed).
                if let Some(job) = shell.job_table.get_mut(id) {
                    job.notified = true;
                }
            }
            shell.foreground_interrupt = Some(ForegroundInterrupt::Stopped(id));
            128 + Signal::SIGTSTP as i32
        }
        _ => 1,
    };

    if shell.job_control_active
        && let Some(shell_pgid) = shell.shell_pgid
    {
        let _ = tcsetpgrp(&stdin, shell_pgid);
    }

    exit_status
}

/// `fg [job_spec]` (`conch-shell-builtins`)'s core mechanic: resumes an
/// already-[`Stopped`](crate::JobState::Stopped)-or-backgrounded job
/// (`SIGCONT` to its process group) and brings it into the foreground —
/// terminal ownership, blocking wait, stoppable via Ctrl-Z again exactly
/// like any other foreground job (see [`run_foreground_job`]).
///
/// Hands the terminal over via `tcsetpgrp` *before* sending `SIGCONT`,
/// matching the GNU libc manual's own `put_job_in_foreground` reference
/// sequence (`tcsetpgrp` → restore saved terminal modes → `SIGCONT` →
/// wait) — caught by a security review, not by any local testing: doing
/// it in the other order (as an earlier version of this function did)
/// is a real, timing-dependent flake, not merely non-idiomatic — if the
/// resumed job's first terminal access happens before this shell's own
/// `tcsetpgrp` call actually lands, the kernel immediately re-stops it
/// with `SIGTTIN`/`SIGTTOU`, since it would still be trying to use the
/// terminal from a *background* process group's perspective at that
/// exact instant. `run_foreground_job` below still does its own
/// `tcsetpgrp` to the same `pgid` immediately afterward (correctly
/// redundant, not a second bug: it's shared with the "freshly spawned,
/// not resumed" call path, where there's no prior stop/`SIGCONT` for
/// this ordering concern to apply to at all).
///
/// Known gap, not addressed here: no `tcgetattr`/`tcsetattr` save/restore
/// of the terminal's *mode* (as opposed to its foreground process group)
/// around a stop/resume — a job that changes terminal modes while
/// running (`vim`'s raw mode, say) doesn't have that mode saved when it
/// stops or restored when it resumes. Flagged by the same security
/// review as a separate, lower-severity, likely-out-of-scope-for-this-pass
/// gap, not a required fix alongside the ordering bug above.
///
/// # Errors
///
/// `id` isn't a known job, or the underlying `killpg`
/// ([`Shell::signal_job`]) call failed.
pub fn resume_job_in_foreground(shell: &mut Shell, id: u32) -> Result<i32, String> {
    let Some(job) = shell.job_table.get(id) else {
        return Err(format!("fg: {id}: no such job"));
    };
    let pgid = job.pgid;
    let command = job.command.clone();
    if shell.job_control_active {
        eprintln!("{command}");
        let stdin = std::io::stdin();
        let _ = tcsetpgrp(&stdin, pgid);
    }
    if let Err(err) = shell.signal_job(id, Signal::SIGCONT) {
        return Err(format!("fg: {err}"));
    }
    Ok(run_foreground_job(shell, pgid, &command, Some(id)))
}

fn exec_pipeline(pipeline: &Pipeline, shell: &mut Shell) -> i32 {
    let mut carry_in: Option<Vec<u8>> = None;
    let mut status = 0;
    // A single-stage pipeline is eligible to become its own foreground
    // job (its own process group, real terminal ownership, stoppable via
    // Ctrl-Z) if it turns out to resolve to an external command — see
    // `exec_external`'s own docs for why that decision has to be made
    // there, after expansion, rather than here from the AST shape alone.
    // A multi-stage pipeline (`cmd1 | cmd2`) is a documented, carried-
    // forward gap for *this* treatment specifically — see the module
    // docs' pipeline-buffering note — even though each external stage
    // still spawns a real, if un-pgid'd, child process exactly as before.
    let is_standalone = pipeline.commands.len() == 1;

    for (i, command) in pipeline.commands.iter().enumerate() {
        let is_last = i == pipeline.commands.len() - 1;
        let (this_status, output) = match command {
            Command::Simple(simple) => {
                exec_simple(simple, shell, carry_in.take(), !is_last, is_standalone)
            }
            Command::Compound(compound) => {
                // Known simplification — see the module docs. Any
                // stdin_data carried in from a previous stage is
                // dropped here (carry_in.take() below discards it via
                // the loop's own bookkeeping), matching "reads the real
                // inherited stdin instead".
                let _ = carry_in.take();
                (exec_compound(compound, shell), None)
            }
            Command::Function(func) => {
                // POSIX: defining a function is itself a command with
                // exit status 0 (confirmed against real bash: `foo() {
                // :; }; echo $?` prints 0) — it doesn't *run* anything,
                // just registers it. A later definition of the same
                // name silently replaces the earlier one (also confirmed
                // against real bash).
                let _ = carry_in.take();
                shell.functions.insert(func.name.clone(), func.clone());
                (0, None)
            }
            // Command is #[non_exhaustive] in conch-shell-parser purely
            // as a general forward-compatibility precaution; it already
            // covers all three cases the grammar produces.
            _ => unreachable!("conch-shell-parser only produces Simple/Compound/Function commands"),
        };
        status = this_status;
        carry_in = output;
    }

    status
}

// ---- compound commands (POSIX 2.10.2's `compound_command`) --------------

/// Runs a [`CompoundCommand`], dispatching on its kind.
///
/// A compound command's own trailing redirect (`command : compound_command
/// redirect_list`) is parsed but not yet executed (see the module docs)
/// — deliberately checked *before* running the body at all, rather than
/// after, so a `{ side_effect; } > file` we can't correctly redirect
/// doesn't still run `side_effect` with its output going to the wrong
/// place.
fn exec_compound(compound: &CompoundCommand, shell: &mut Shell) -> i32 {
    if !compound.redirects.is_empty() {
        eprintln!(
            "conch: a compound command's own trailing redirect (`{{ ...; }} > file`, `if ...; fi > file`, `(...) > file`, ...) is not yet supported"
        );
        return 1;
    }
    match &compound.kind {
        CompoundCommandKind::BraceGroup(body) => {
            // Runs in the *current* environment — no isolation, no
            // loop_depth/pending_control_flow interception; a `break`
            // lexically inside a brace group correctly reaches whatever
            // loop encloses the brace group itself.
            exec_command_list(body, shell)
        }
        CompoundCommandKind::Subshell(subshell) => exec_subshell(&subshell.source, shell),
        CompoundCommandKind::If(clause) => exec_if(clause, shell),
        CompoundCommandKind::While(clause) => {
            exec_while_or_until(&clause.condition, &clause.body, true, shell)
        }
        CompoundCommandKind::Until(clause) => {
            exec_while_or_until(&clause.condition, &clause.body, false, shell)
        }
        CompoundCommandKind::For(clause) => exec_for(clause, shell),
        CompoundCommandKind::Case(clause) => exec_case(clause, shell),
        // CompoundCommandKind is #[non_exhaustive] in conch-shell-parser
        // purely as a general forward-compatibility precaution; it
        // already covers all seven POSIX compound_command alternatives.
        _ => unreachable!("conch-shell-parser only produces the seven documented kinds"),
    }
}

/// POSIX `subshell : '(' compound_list ')'` — runs `source` (the
/// subshell body's *raw* text — see [`conch_shell_parser::SubshellBody`]'s
/// docs for why the parsed body isn't what gets executed) with real
/// isolation from the parent `shell`: a variable assignment, `cd`, or
/// `exit` inside must never affect the parent.
///
/// Implemented by re-exec'ing `conch -c <source>` as a genuine child
/// process — exactly [`run_command_substitution`]'s own pattern in
/// `conch-shell-core::expand`, minus the stdout capture (a subshell's
/// output goes to the real inherited streams, the same as any other
/// compound command — see the module docs' "Known Phase 3
/// simplification" for the one place that still falls short of full
/// pipeline integration). This isn't just the simplest correct option;
/// it's the *only* correct one available: `cd`'s underlying
/// `std::env::set_current_dir` and `exit`'s `std::process::exit` both
/// mutate real OS-level process state that no amount of restoring
/// `Shell`'s own fields afterward could undo, so an in-process
/// snapshot/restore of `Shell` (an earlier design this went through)
/// is fundamentally unable to isolate a subshell correctly — only a
/// genuinely separate process can. `last_status` (`$?`) doesn't need
/// any special handling here: POSIX gives a subshell the same
/// exit-status-propagation behavior as any other command, and the
/// child's own exit code, used directly as this function's return
/// value, already *is* that.
///
/// Known gap, shared with command substitution: only *exported*
/// variables (`shell.env_vars`) reach the child; a shell-local variable
/// (`shell.shell_vars`) does not, so `x=1; (echo $x)` prints nothing,
/// same as `x=1; echo $(echo $x)` already does today. Deliberately not
/// fixed by serializing `shell_vars` through an environment variable for
/// the child to parse back out — that shape (interpreter state
/// serialized into an env var, later parsed back out by a re-invoked
/// interpreter) is structurally the same one Shellshock (CVE-2014-6271)
/// exploited, and isn't worth the scrutiny it would need to get right
/// unless this gap turns out to matter in practice.
fn exec_subshell(source: &str, shell: &Shell) -> i32 {
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            eprintln!("conch: subshell: {err}");
            return 1;
        }
    };
    let status = std::process::Command::new(exe)
        .arg("-c")
        .arg(source)
        .current_dir(&shell.cwd)
        .env_clear()
        .envs(&shell.env_vars)
        // `$$` must read as the *top-level* shell's PID even inside a
        // subshell -- see `Shell::pid`'s own docs.
        .env("__CONCH_PID", shell.pid.to_string())
        // An *ignored* trap propagates into a subshell environment too
        // (unlike a *caught* one, which is deliberately never
        // propagated this way -- see this function's own doc comment
        // above, and `Shell::ignored_trap_names`'s docs, for why the
        // two get different treatment).
        .env("__CONCH_IGNORED_SIGNALS", shell.ignored_trap_names())
        .status();
    match status {
        Ok(status) => exit_code_of(status),
        Err(err) => {
            eprintln!("conch: subshell: {err}");
            1
        }
    }
}

/// Converts a real child process's [`std::process::ExitStatus`] into a
/// POSIX-shaped shell exit status: the process's own exit code if it
/// exited normally, or `128 + signal` if it was killed by an uncaught
/// signal (POSIX 2.8.2 / confirmed against real bash's own convention —
/// e.g. `SIGTERM` (15) → `143`, `SIGKILL` (9) → `137`) — `ExitStatus::code()`
/// alone only ever gives `None` in the signaled case, with no way to
/// recover *which* signal from that method alone, so every call site
/// that needs this exact convention (a subshell's own exit status, an
/// external command's, and — the case that specifically motivated
/// pulling this into one shared helper rather than leaving each site's
/// own `.code().unwrap_or(1)` as it was pre-Phase-4 — a *backgrounded*
/// job's, since `wait`'s "reports a background job's death by signal as
/// 128+n" behavior (`conch-shell-builtins`) depends on
/// [`spawn_background_job`]'s re-exec'd wrapper child correctly
/// propagating *its own* signal-death status this same way) goes through
/// this instead. `pub(crate)` (not `fn`) so `crate::expand`'s own command
/// substitution (`run_command_substitution`) can reuse the identical
/// convention for `$(...)`/`` `...` ``'s own exit status — needed for
/// POSIX 2.9.1.3's own "no command name" rule (see
/// `crate::expand::run_command_substitution`'s own docs for the exact
/// text and how it's implemented).
pub(crate) fn exit_code_of(status: std::process::ExitStatus) -> i32 {
    use std::os::unix::process::ExitStatusExt as _;
    match status.code() {
        Some(code) => code,
        None => 128 + status.signal().unwrap_or(0),
    }
}

/// Calls a shell function — POSIX 2.9.5 / bash manual §3.3. `args`
/// excludes the function's own name (matches an ordinary command's own
/// `args`, sans `name`). Runs entirely *in-process*, unlike a subshell
/// ([`exec_subshell`]): a function call isn't a real fork boundary in any
/// shell — it shares the caller's variables, `cwd`, and everything else
/// by design (confirmed against real bash: `cd`/a non-`local` assignment
/// inside a function are visible to the caller after it returns).
///
/// Four pieces of state get temporarily replaced for the duration of the
/// call, each restored afterward regardless of how the call ended (normal
/// completion, a pending `break`/`continue` with nowhere left to go, or
/// `return`):
///
/// - [`Shell::positional_params`]: replaced with `args` (confirmed
///   against real bash: `$1`/`$#`/`"$@"`/... inside a function are the
///   function's own arguments, not the caller's).
/// - [`Shell::loop_depth`]: reset to `0`. A function call is its own
///   `break`/`continue` boundary, exactly like the top-level program is
///   ([`exec_program`]) — confirmed against real bash: a `break` inside a
///   function only ever stops a loop lexically inside that *same*
///   function, never a loop merely *calling* it, even though
///   `loop_depth` would otherwise still be nonzero at that point. Without
///   this reset, a `break`/`continue` whose level exceeds the function's
///   own internal loop nesting would incorrectly keep unwinding into the
///   caller's own enclosing loop instead of stopping (with the same
///   diagnostic real bash prints) right here.
/// - A fresh [`Shell::push_local_frame`]: so `local` declarations made
///   during this call restore correctly once it ends
///   ([`Shell::pop_local_frame`]) — see [`Shell::local_stack`]'s docs for
///   why this, deliberately, is *not* a new variable-lookup scope layer.
/// - [`Shell::function_depth`]: incremented so `return` (`conch-shell-builtins`)
///   can tell it's valid to use.
///
/// [`Shell::arg0`] (`$0`) is deliberately *not* touched — confirmed
/// against real bash `$0` inside a function is still the shell's own
/// name/script path, never the function's name.
fn exec_function_call(func: &FunctionDefinition, args: &[String], shell: &mut Shell) -> i32 {
    let previous_positional = std::mem::replace(&mut shell.positional_params, args.to_vec());
    let previous_loop_depth = std::mem::replace(&mut shell.loop_depth, 0);
    shell.push_local_frame();
    shell.function_depth += 1;

    let status = exec_compound(&func.body, shell);

    // POSIX: falling off the end of a function body uses the last
    // command's own exit status (confirmed against real bash: `f() {
    // false; }; f; echo $?` -> 1) — exactly what `status` above already
    // is, so the `None` arm below leaves it untouched.
    let status = match shell.pending_control_flow.take() {
        Some(ControlFlow::Return(code)) => code,
        Some(ControlFlow::Break(_) | ControlFlow::Continue(_)) => {
            eprintln!("{UNHANDLED_LOOP_CONTROL_MSG}");
            status
        }
        None => status,
    };

    shell.function_depth -= 1;
    shell.pop_local_frame();
    shell.positional_params = previous_positional;
    shell.loop_depth = previous_loop_depth;

    status
}

/// Runs a condition list for `if`/`while`/`until` — identical to
/// [`exec_command_list`] except it also increments/decrements
/// [`Shell::errexit_suppressed`] around the call, so a failing condition
/// never triggers `set -e` (see that field's own docs for why: without
/// this, `set -e; if grep -q foo file; then ...; fi` would exit the
/// whole shell the first time `grep` found no match).
fn exec_condition_list(condition: &CommandList, shell: &mut Shell) -> i32 {
    shell.errexit_suppressed += 1;
    let status = exec_command_list(condition, shell);
    shell.errexit_suppressed -= 1;
    status
}

/// POSIX `if_clause`/`else_part` — see
/// [`conch_shell_parser::IfClause`]'s docs for how the AST already
/// flattens the `elif` chain, which is what keeps this a single loop
/// rather than needing recursion to mirror the grammar's own nesting.
fn exec_if(clause: &IfClause, shell: &mut Shell) -> i32 {
    for (condition, body) in &clause.branches {
        let cond_status = exec_condition_list(condition, shell);
        if unwinding(shell) {
            return cond_status;
        }
        if cond_status == 0 {
            return exec_command_list(body, shell);
        }
    }
    if let Some(else_branch) = &clause.else_branch {
        return exec_command_list(else_branch, shell);
    }
    // POSIX: no branch taken and no `else` — exit status 0.
    0
}

/// Shared `while_clause`/`until_clause` execution (`continue_while`:
/// `true` for `while`, `false` for `until` — POSIX's only difference
/// between the two is which way the condition's exit status is read).
///
/// POSIX: "The exit status of the while/until command shall be the exit
/// status of the last compound-list-2 [body] that was executed, or zero
/// if none was executed" — confirmed against real bash (a loop that runs
/// its body and then stops normally keeps the *body's* last exit status,
/// not `0`; only a loop whose body never ran at all reports `0`), which
/// is why `status` is only ever assigned from a body execution, never
/// reset back to `0` when the condition later becomes false.
fn exec_while_or_until(
    condition: &CommandList,
    body: &CommandList,
    continue_while: bool,
    shell: &mut Shell,
) -> i32 {
    shell.loop_depth += 1;
    let mut status = 0;
    loop {
        let cond_status = exec_condition_list(condition, shell);
        if unwinding(shell) {
            status = cond_status;
            if matches!(take_loop_control_flow(shell), LoopStep::Continue) {
                continue;
            }
            break;
        }
        if (cond_status == 0) != continue_while {
            break;
        }
        status = exec_command_list(body, shell);
        if matches!(take_loop_control_flow(shell), LoopStep::Break) {
            break;
        }
    }
    shell.loop_depth -= 1;
    status
}

/// POSIX `for_clause` — see [`conch_shell_parser::ForClause`]'s docs for
/// the `words: None` (no `in` clause) case, which POSIX defines as
/// implicitly `for name in "$@"`: iterating each current positional
/// parameter as its own value, verbatim (POSIX 2.5.2's quoted-`"$@"`
/// "one field per positional parameter" behavior — not the ordinary
/// brace/field-splitting/globbing expansion the `in`-clause case below
/// gets, since there's no *word* here at all for that to apply to).
fn exec_for(clause: &ForClause, shell: &mut Shell) -> i32 {
    let values = match &clause.words {
        Some(words) => {
            // Each wordlist item undergoes the exact same
            // brace/field-splitting/globbing expansion a command
            // argument would (POSIX: `wordlist`'s `WORD`s are ordinary
            // words), flattened into one sequence of iteration values —
            // reusing expand_words_fields is what makes `for f in
            // *.txt; do ...; done` (one AST word, many loop iterations
            // once the glob expands) work correctly.
            match expand_words_fields(words.iter(), shell) {
                Ok(values) => values,
                Err(err) => {
                    report_expand_error(&err, shell);
                    return 1;
                }
            }
        }
        None => shell.positional_params.clone(),
    };

    shell.loop_depth += 1;
    let mut status = 0;
    for value in values {
        shell.shell_vars.insert(clause.name.clone(), value);
        status = exec_command_list(&clause.body, shell);
        if matches!(take_loop_control_flow(shell), LoopStep::Break) {
            break;
        }
    }
    shell.loop_depth -= 1;
    status
}

/// POSIX `case_clause`. Pattern matching reuses
/// `conch-shell-core::expand`'s [`glob_match`] (the same POSIX 2.13
/// pattern engine pathname expansion uses) via [`expand_word_as_pattern`]
/// for the same quote-aware pattern text — see that function's docs.
fn exec_case(clause: &CaseClause, shell: &mut Shell) -> i32 {
    let word = match expand_word_single(&clause.word, shell) {
        Ok(word) => word,
        Err(err) => {
            report_expand_error(&err, shell);
            return 1;
        }
    };
    for arm in &clause.arms {
        for pattern in &arm.patterns {
            let pattern_text = match expand_word_as_pattern(pattern, shell) {
                Ok(text) => text,
                Err(err) => {
                    report_expand_error(&err, shell);
                    return 1;
                }
            };
            if glob_match(&pattern_text, &word) {
                return exec_command_list(&arm.body, shell);
            }
        }
    }
    // POSIX: no pattern matched — exit status 0.
    0
}

/// What a loop should do next after running one iteration of its body
/// (or, for `while`/`until`, its condition) — see
/// [`take_loop_control_flow`].
enum LoopStep {
    /// Run the next iteration normally (no control flow was pending, or
    /// a `continue` targeted this exact loop).
    Continue,
    /// Stop this loop — either nothing was pending, and the loop's own
    /// termination condition already said so, or a `break`/`continue`
    /// targeted this loop or an outer one (in the outer case,
    /// `shell.pending_control_flow` is left set, one level lower, for
    /// the caller to keep propagating).
    Break,
}

/// Consumes `shell.pending_control_flow` (see [`crate::ControlFlow`]'s
/// docs) and decides what the loop that just finished running its body
/// should do: `None` pending → [`LoopStep::Continue`] (nothing special
/// happened, run the next iteration); `Break(1)`/`Continue(1)` → this
/// loop is the target, so `Break`/[`LoopStep::Continue`] respectively,
/// fully consumed; `Break(n)`/`Continue(n)` for `n > 1` → not this loop's
/// signal, decrement to `n - 1`, leave it pending, and report
/// [`LoopStep::Break`] so *this* loop still stops (unwinding is required
/// either way — a `continue 2` from a doubly-nested loop still needs the
/// inner loop to stop entirely, not just skip one inner iteration, since
/// the target is the *outer* loop's next iteration).
fn take_loop_control_flow(shell: &mut Shell) -> LoopStep {
    if shell.foreground_interrupt.is_some() {
        // A stopped/SIGINT-killed foreground job (see
        // `ForegroundInterrupt`'s docs) always fully stops this loop and
        // keeps propagating, unconditionally — it isn't `pending_control_flow`
        // at all, so it's checked and handled here first, before ever
        // touching that separate field.
        return LoopStep::Break;
    }
    match shell.pending_control_flow.take() {
        None => LoopStep::Continue,
        Some(ControlFlow::Break(1)) => LoopStep::Break,
        Some(ControlFlow::Break(n)) => {
            shell.pending_control_flow = Some(ControlFlow::Break(n.saturating_sub(1)));
            LoopStep::Break
        }
        Some(ControlFlow::Continue(1)) => LoopStep::Continue,
        // A continue level exceeding every loop that actually encloses
        // it needs to be *clamped*, not just discarded once it runs out
        // of loops to decrement through the way an over-large break is
        // (see exec_command_list's docs) — by the time propagation would
        // otherwise stop this (the outermost) loop entirely, it's too
        // late to still act like a *continue* of it. `shell.loop_depth
        // == 1` here means "I am that outermost loop" (it hasn't been
        // decremented back yet at this point in exec_for/
        // exec_while_or_until, so it still counts this loop itself).
        // Confirmed against real bash: `continue 3` one loop deep
        // behaves exactly like a plain `continue`.
        Some(ControlFlow::Continue(_)) if shell.loop_depth == 1 => LoopStep::Continue,
        Some(ControlFlow::Continue(n)) => {
            shell.pending_control_flow = Some(ControlFlow::Continue(n.saturating_sub(1)));
            LoopStep::Break
        }
        // `return` never targets a loop at all -- it's always headed for
        // the nearest enclosing function call's own boundary
        // (`exec_function_call`), however many loops it has to unwind
        // through to get there. Put back unchanged (no level to
        // decrement -- see `ControlFlow::Return`'s docs) and report
        // `Break` so this loop still stops, exactly like an over-large
        // break/continue does on its way further out.
        Some(ControlFlow::Return(code)) => {
            shell.pending_control_flow = Some(ControlFlow::Return(code));
            LoopStep::Break
        }
    }
}

/// Runs one [`SimpleCommand`].
///
/// `stdin_data`: piped-in bytes from the previous pipeline stage, if any.
/// `capture_output`: `true` if this command is *not* the pipeline's last
/// stage, so its stdout should be captured and returned rather than
/// written to the real process stdout.
/// `standalone`: `true` if this is the *only* command in its pipeline
/// (see [`exec_pipeline`]'s own docs) — passed through to
/// [`exec_external`], the only place it matters (see that function's
/// docs for why it's the one that gets to decide, not here).
///
/// Returns the exit status and, when `capture_output` was honored (no
/// output redirect overrode it), the captured stdout bytes.
fn exec_simple(
    cmd: &SimpleCommand,
    shell: &mut Shell,
    stdin_data: Option<Vec<u8>>,
    capture_output: bool,
    standalone: bool,
) -> (i32, Option<Vec<u8>>) {
    // POSIX 2.9.1.3: "if there is no command name but the command
    // contains a command substitution, the command shall complete with
    // the exit status of the command substitution whose exit status was
    // the last to be obtained" -- captured *before* any expansion below
    // runs, so both of this function's own "nothing actually ran"
    // branches (a bare assignment; a command name that expanded to zero
    // fields) can tell "a substitution ran during this command's own
    // expansion and updated `shell.last_status` as a direct side effect
    // (see `expand::run_command_substitution`'s own docs)" apart from
    // "nothing touched it, so the POSIX-mandated plain default of `0`
    // applies" -- simply reading `shell.last_status` directly, with no
    // baseline to compare against, could otherwise leak an entirely
    // unrelated *prior* command's status into a substitution-free bare
    // assignment (confirmed against real bash: `false; x=5; echo $?` is
    // `0`, not `1`, even though the shell's `$?` was `1` immediately
    // before `x=5` ran).
    let status_before_expansion = shell.last_status;

    let assignments = match expand_assignments(cmd, shell) {
        Ok(assignments) => assignments,
        Err(err) => {
            report_expand_error(&err, shell);
            return (1, None);
        }
    };

    let Some(name_word) = &cmd.name else {
        // A bare assignment-only simple command (`FOO=bar`, no command
        // name) assigns persistently to the shell rather than just the
        // one command's environment. See this function's own
        // `status_before_expansion` docs above for the POSIX 2.9.1.3
        // exit-status rule this implements.
        let mut status = if shell.last_status == status_before_expansion {
            0
        } else {
            shell.last_status
        };
        for (name, value) in assignments {
            if shell.readonly_vars.contains(&name) {
                eprintln!("conch: {name}: readonly variable");
                status = 1;
                continue;
            }
            shell.shell_vars.insert(name, value);
        }
        return (status, None);
    };

    // Brace expansion (`{a,b,c}`) runs before anything else and can turn
    // one word into several; the command name undergoes the same
    // brace/field-splitting/globbing as any other word (POSIX doesn't
    // special-case cmd_word here), so if it produces more than one
    // resulting field, the first becomes the command name and the rest
    // become leading arguments — e.g. `$CMD` with CMD="echo hi" runs
    // `echo` with `hi` prepended before any other args.
    let mut all_fields = match expand_words_fields(std::iter::once(name_word), shell) {
        Ok(fields) => fields.into_iter(),
        Err(err) => {
            report_expand_error(&err, shell);
            return (1, None);
        }
    };
    let Some(name) = all_fields.next() else {
        // Expanded to zero fields (e.g. an unset, unquoted parameter, or
        // a command substitution with no output) -- nothing to run,
        // matching real shells treating this as a no-op *unless* a
        // command substitution ran somewhere in this command's own
        // expansion (assignments or the name word itself), per the same
        // POSIX 2.9.1.3 rule and `status_before_expansion` docs as the
        // bare-assignment branch above.
        let status = if shell.last_status == status_before_expansion {
            0
        } else {
            shell.last_status
        };
        return (status, None);
    };

    let mut args: Vec<String> = all_fields.collect();
    match expand_words_fields(cmd.args.iter(), shell) {
        Ok(fields) => args.extend(fields),
        Err(err) => {
            report_expand_error(&err, shell);
            return (1, None);
        }
    }

    let redirects = match expand_redirects(cmd, shell) {
        Ok(redirects) => redirects,
        Err(err) => {
            report_expand_error(&err, shell);
            return (1, None);
        }
    };

    // `set -x` (xtrace) — printed after expansion (so this shows the
    // *actual* command/args, not the unexpanded source text) but before
    // dispatch, matching real bash's own ordering. See `Shell::xtrace`'s
    // own docs for the "no configurable `PS4`" simplification.
    if shell.xtrace {
        let mut line = String::from("+ ");
        line.push_str(&name);
        for arg in &args {
            line.push(' ');
            line.push_str(arg);
        }
        eprintln!("{line}");
    }

    // Command search order: functions → builtins (special or regular
    // alike) → PATH. POSIX 2.9.1/2.9.5 actually puts special builtins
    // *ahead* of functions, specifically because a function is never
    // allowed to shadow one — but confirmed against real bash 5.3 in its
    // *default* (non-`--posix`) mode, a function shadows even a special
    // builtin like `export`/`break` just fine (`export() { echo fake;
    // }; export FOO=bar` runs the fake function, never sets `FOO`; only
    // `bash --posix` refuses with "`export': is a special builtin").
    // This project tracks bash's real default behavior over strict
    // POSIX-only where the two diverge (see e.g. the arithmetic module's
    // own docs for the same policy applied elsewhere), and the
    // differential suite's primary oracle is real (non-`--posix`) bash,
    // so a function is checked *first* here, unconditionally — the
    // special/regular builtin distinction
    // ([`Shell::is_special_builtin`]) is kept (still structurally useful
    // for anything that needs POSIX's own categorization later) but no
    // longer used to block a function from shadowing one.
    if let Some(func) = shell.functions.get(&name).cloned() {
        let status = exec_function(&func, shell, &args, &assignments, &redirects);
        return (status, None);
    }

    if shell.builtin(&name).is_some() {
        return exec_builtin(
            &name,
            shell,
            &args,
            &assignments,
            &redirects,
            stdin_data,
            capture_output,
        );
    }

    exec_external(
        &name,
        &args,
        shell,
        &assignments,
        &redirects,
        PipelineStdio {
            stdin_data,
            capture_output,
            standalone,
        },
    )
}

/// Calls a shell function that was found via ordinary command-name
/// lookup (`exec_simple`) — i.e. an actual *call* (`f`, `f arg1 arg2`,
/// `FOO=bar f`), as opposed to a `Command::Function` *definition*
/// (`f() { ...; }`, handled entirely separately in [`exec_pipeline`]).
///
/// Prefix assignments (`FOO=bar f`) get the same temporary,
/// per-call-only treatment [`exec_builtin`] already gives a builtin
/// (confirmed against real bash: `FOO=temp f` doesn't leave `FOO` set
/// afterward) — everything else about actually running the call is
/// [`exec_function_call`]'s job.
///
/// Known simplification, consistent with the module's other "a compound
/// command doesn't get the in-memory stdout-buffer treatment a plain
/// external/builtin command does" gap (see the module docs): a
/// function's own combined output isn't captured into a buffer the way
/// [`exec_builtin`]'s is, so a function call used as a non-last pipeline
/// stage (`f | grep x`) or with its own output redirect (`f > file`)
/// doesn't yet work correctly — confirmed against real bash both of
/// those *do* work there, so this is a real, deliberate gap, not a
/// theoretical one. Implementing it correctly would need genuine OS-level
/// file-descriptor redirection around the whole call (every nested
/// builtin/external command inside the function body writes to the real
/// inherited stdout today, not through any single capturable sink this
/// crate controls) — meaningfully riskier and outside this follow-up's
/// asked-for scope, so a redirect on a function call site is reported
/// clearly instead of silently misbehaving, matching
/// [`exec_compound`]'s own precedent for a compound command's own
/// trailing redirect.
fn exec_function(
    func: &FunctionDefinition,
    shell: &mut Shell,
    args: &[String],
    assignments: &[(String, String)],
    redirects: &[ExpandedRedirect],
) -> i32 {
    if !redirects.is_empty() {
        eprintln!("conch: a function call's own output redirect (`f > file`) is not yet supported");
        return 1;
    }

    let mut previous = Vec::with_capacity(assignments.len());
    for (name, value) in assignments {
        previous.push((
            name.clone(),
            shell.env_vars.insert(name.clone(), value.clone()),
        ));
    }

    let status = exec_function_call(func, args, shell);

    for (name, previous_value) in previous {
        match previous_value {
            Some(value) => {
                shell.env_vars.insert(name, value);
            }
            None => {
                shell.env_vars.remove(&name);
            }
        }
    }

    status
}

/// `NAME=value` prefix assignments, expanded to their final strings.
/// Assignment values never undergo field splitting or pathname expansion
/// (POSIX 2.6) — `expand_word_single` is the correct mode here, not
/// `expand_word_fields`.
/// Expands each of `cmd`'s assignments to its final `(name, value)` pair
/// — resolving [`Assignment::is_append`]'s bash-extension `NAME+=value`
/// semantics here (concatenating onto the name's *current* value, an
/// unset name treated as empty, exactly like a plain assignment — see
/// that field's own docs) so every consumer of this function's return
/// value (a bare assignment persisting to the shell; a prefix assignment
/// temporarily set for one command) only ever has to deal with an
/// already-fully-resolved value to *set*, never `+=`'s own
/// read-then-concatenate step.
///
/// Known simplification, not currently reachable by anything in this
/// project's own differential corpus: two `+=` assignments to the *same*
/// name within one single command line (`x=a; x+=b x+=c` as one prefix
/// list, as opposed to separate statements) would each read `name`'s
/// value from *before* this whole batch started, rather than seeing the
/// earlier one's own effect — every assignment in `cmd.assignments` is
/// expanded from the same starting `shell` state before any of them are
/// actually applied (that application happens in this function's own
/// callers, afterward). Accumulating across *separate statements*
/// (`x=a; x+=b; x+=c`, the common, real-world idiom this bash extension
/// exists for) is unaffected, since each statement is its own separate
/// `expand_assignments` call against the shell state the *previous*
/// statement already updated.
fn expand_assignments(
    cmd: &SimpleCommand,
    shell: &mut Shell,
) -> Result<Vec<(String, String)>, ExpandError> {
    cmd.assignments
        .iter()
        .map(|assignment| {
            let expanded = expand_word_single(&assignment.value, shell)?;
            let value = if assignment.is_append {
                let mut combined = shell.get_var(&assignment.name).unwrap_or("").to_string();
                combined.push_str(&expanded);
                combined
            } else {
                expanded
            };
            Ok((assignment.name.clone(), value))
        })
        .collect()
}

struct ExpandedRedirect {
    fd: u32,
    operator: RedirectOperator,
    target: String,
}

fn expand_redirects(
    cmd: &SimpleCommand,
    shell: &mut Shell,
) -> Result<Vec<ExpandedRedirect>, ExpandError> {
    cmd.redirects
        .iter()
        .map(|redirect: &Redirect| {
            let target = expand_word_single(&redirect.target, shell)?;
            let default_fd = match redirect.operator {
                RedirectOperator::Input => 0,
                RedirectOperator::Output | RedirectOperator::Append => 1,
            };
            Ok(ExpandedRedirect {
                fd: redirect.fd.unwrap_or(default_fd),
                operator: redirect.operator,
                target,
            })
        })
        .collect()
}

/// Reports an expansion failure the way an ordinary command failure is
/// reported (print to stderr, let the caller treat it as exit status 1
/// and move on) — *unless* `err` is the one POSIX 2.6.2 mandates is fatal
/// to the whole (non-interactive) shell: `${parameter:?word}` /
/// `${parameter?word}` on an unset (or, colon form, null) parameter. In
/// that case this exits the process outright instead of returning.
///
/// Confirmed against both real bash and dash: regardless of shell or
/// invocation mode (`-c '...'` vs a script file), *nothing after* a
/// triggered `${var:?...}` ever runs — only the specific exit code
/// varies (127/1 for bash depending on invocation mode, 2 for dash),
/// which conch doesn't attempt to replicate exactly (this always uses 1)
/// since nothing downstream depends on matching it — see
/// `tests/conch-difftest/known-differences.md`'s "Cross-shell quirks"
/// section for the full grounding.
///
/// Only checked when `!shell.is_interactive`: an interactive shell
/// returns to its prompt on this error rather than exiting the whole
/// session (POSIX 2.6.2) — conch doesn't yet abort just "the rest of the
/// current input line" for the interactive case (a smaller, distinct gap
/// from exiting the process, and not one the differential suite's
/// non-interactive `-c`/script-file cases exercise), so interactive mode
/// keeps today's "print and move on to the next command" behavior for
/// every `ExpandError` variant, same as before this function existed.
fn report_expand_error(err: &ExpandError, shell: &Shell) {
    eprintln!("conch: {err}");
    // `ExpandError::UnboundVariable` (`set -u`) joins `ParameterNullOrUnset`
    // (`${var:?}`) here — confirmed directly against real bash (not
    // assumed) that a `set -u` violation aborts the whole non-interactive
    // shell exactly like `${var:?}` does, despite looking at first like
    // an ordinary "ignorable special-builtin error" case — see
    // `Shell::nounset`'s own docs for the full reasoning on why the two
    // are actually governed by the same POSIX rule.
    if !shell.is_interactive
        && matches!(
            err,
            ExpandError::ParameterNullOrUnset(_) | ExpandError::UnboundVariable(_)
        )
    {
        std::process::exit(1);
    }
}

/// Builds whichever of the three stdin sources a builtin should see for
/// this invocation, in the same priority order [`exec_external`] already
/// uses for an external command: an explicit `<`-style input redirect on
/// the command itself first (opened fresh, so a builtin genuinely reads
/// the file rather than whatever happened to be piped in from an earlier
/// pipeline stage); failing that, `stdin_data` piped in from the previous
/// pipeline stage, if any; failing that, the real inherited process
/// stdin — the common case for an interactive/script `read` with no
/// redirect or pipe at all.
///
/// Returns `Err` (already reported to stderr, matching
/// [`exec_external`]'s own "can't open the redirect target" handling) if
/// the input redirect's target couldn't be opened.
fn builtin_stdin(
    redirects: &[ExpandedRedirect],
    stdin_data: Option<Vec<u8>>,
) -> Result<Box<dyn std::io::Read>, ()> {
    if let Some(redirect) = redirects
        .iter()
        .rev()
        .find(|r| r.fd == 0 && matches!(r.operator, RedirectOperator::Input))
    {
        return match std::fs::File::open(&redirect.target) {
            Ok(file) => Ok(Box::new(file)),
            Err(err) => {
                eprintln!("conch: {}: {err}", redirect.target);
                Err(())
            }
        };
    }
    if let Some(data) = stdin_data {
        return Ok(Box::new(std::io::Cursor::new(data)));
    }
    Ok(Box::new(std::io::stdin()))
}

fn exec_builtin(
    name: &str,
    shell: &mut Shell,
    args: &[String],
    assignments: &[(String, String)],
    redirects: &[ExpandedRedirect],
    stdin_data: Option<Vec<u8>>,
    capture_output: bool,
) -> (i32, Option<Vec<u8>>) {
    // Prefix assignments before a builtin are applied to the shell's own
    // environment for the duration of the call and then rolled back —
    // matching the "temporary, per-command" semantics real assignments
    // before a command name have, without persisting them the way a
    // bare `FOO=bar` (no command) does.
    let mut previous = Vec::with_capacity(assignments.len());
    for (name, value) in assignments {
        previous.push((
            name.clone(),
            shell.env_vars.insert(name.clone(), value.clone()),
        ));
    }

    let mut stdin_reader = match builtin_stdin(redirects, stdin_data) {
        Ok(reader) => reader,
        Err(()) => return (1, None),
    };
    let mut stdout_buf: Vec<u8> = Vec::new();
    let mut stderr_buf: Vec<u8> = Vec::new();
    // Take the builtin out of the registry for the duration of the call:
    // `run` needs `&mut Shell`, which the registry itself lives inside, so
    // holding a borrow of the boxed builtin while also passing `&mut
    // shell` in is a self-referential borrow the compiler correctly
    // rejects. None of Phase 1's builtins call back into the registry
    // (e.g. by invoking another builtin), so this is a non-issue in
    // practice, not just a workaround.
    let builtin = shell
        .take_builtin(name)
        .expect("caller already checked this builtin exists");
    let status = builtin.run(
        shell,
        args,
        &mut *stdin_reader,
        &mut stdout_buf,
        &mut stderr_buf,
    );
    shell.register_builtin(name.to_string(), builtin);

    for (name, previous_value) in previous {
        match previous_value {
            Some(value) => {
                shell.env_vars.insert(name, value);
            }
            None => {
                shell.env_vars.remove(&name);
            }
        }
    }

    // A builtin's *stderr* output must respect a `2>`/`2>>` redirect on
    // the command exactly the same way its stdout already respects a
    // `1>`/`>`/`>>` one just below — caught by a security/testing review
    // as a real bug (not merely a gap): `eval "if" 2>/dev/null`'s own
    // syntax-error diagnostic (written through `Eval`'s own `stderr`
    // parameter, exactly as every builtin here is supposed to) was
    // leaking to the real process stderr regardless, since this used to
    // unconditionally flush `stderr_buf` there with no redirect check at
    // all -- a general gap in this function affecting *every* builtin's
    // stderr, not anything specific to `eval`.
    if let Some(err_redirect) = redirects.iter().rev().find(|r| {
        r.fd == 2
            && matches!(
                r.operator,
                RedirectOperator::Output | RedirectOperator::Append
            )
    }) {
        if let Err(err) = write_redirect(err_redirect, &stderr_buf, shell.noclobber) {
            eprintln!("conch: {}: {err}", err_redirect.target);
            return (1, None);
        }
    } else if !stderr_buf.is_empty() {
        let _ = std::io::stderr().write_all(&stderr_buf);
    }

    if let Some(output_redirect) = redirects.iter().rev().find(|r| {
        r.fd == 1
            && matches!(
                r.operator,
                RedirectOperator::Output | RedirectOperator::Append
            )
    }) {
        if let Err(err) = write_redirect(output_redirect, &stdout_buf, shell.noclobber) {
            eprintln!("conch: {}: {err}", output_redirect.target);
            return (1, None);
        }
        return (status, None);
    }

    if capture_output {
        (status, Some(stdout_buf))
    } else {
        let _ = std::io::stdout().write_all(&stdout_buf);
        (status, None)
    }
}

/// How an external command's stdio should be wired up, decided entirely
/// by its *position* within its pipeline (see [`exec_pipeline`]'s own
/// docs) — bundled into one struct purely to keep [`exec_external`]'s
/// own argument count reasonable, not because these three are otherwise
/// conceptually one thing.
struct PipelineStdio {
    /// Piped-in bytes from the previous pipeline stage, if any.
    stdin_data: Option<Vec<u8>>,
    /// `true` if this command is *not* the pipeline's last stage, so its
    /// stdout should be captured and returned rather than written to the
    /// real process stdout.
    capture_output: bool,
    /// `true` iff this external command is its *whole* pipeline — in
    /// which case, when [`Shell::job_control_active`], *this spawn
    /// itself* becomes a real foreground job: its own new process group
    /// ([`Shell::prepare_child_for_job_control`]), the controlling
    /// terminal handed to it for the duration, and stoppable via Ctrl-Z
    /// into [`Shell::job_table`] ([`run_foreground_job`]) — the
    /// common-case, no-extra-indirection path (contrast
    /// [`spawn_background_job`], which needs a whole extra `conch -c`
    /// re-exec layer only because *that* case can't assume "one external
    /// command" at all). `standalone` alone isn't sufficient — job
    /// control must also actually be active
    /// ([`Shell::job_control_active`]): a `-c`/script invocation, or an
    /// already-backgrounded/subshell'd child, has no controlling
    /// terminal to hand around and runs this exact same command with the
    /// ordinary, pre-Phase-4 `Stdio`/`child.wait()` path instead,
    /// unchanged.
    standalone: bool,
}

/// Reports one of `exec_external`'s own diagnostics (a redirect target
/// that couldn't be opened, the command itself not being found, a
/// `wait`/spawn failure) the way real bash does: as if printed *by* the
/// failed command itself, so an existing `2>`/`2>>` redirect on that same
/// command also applies to it — not just to whatever the command would
/// have written on its own. Real bug, caught by a testing/security
/// review sweeping for more instances of the exact class the builtin
/// stderr-redirect fix (`exec_builtin`, right above this function) had
/// already closed: `f() { :; }; unset -f f; f 2>/dev/null`'s "command not
/// found" diagnostic (once `f` falls through to external-command lookup)
/// was leaking to the real process stderr regardless of the same
/// command's own `2>/dev/null` — this is the identical bug shape, one
/// call site removed, since external-command failures go through this
/// function, never `exec_builtin`.
///
/// Falls back to the real process stderr if there's no such redirect, or
/// if the redirect target itself can't be opened (nothing better to
/// report through in that case).
fn report_external_diagnostic(message: &str, redirects: &[ExpandedRedirect], noclobber: bool) {
    let err_redirect = redirects.iter().rev().find(|r| {
        r.fd == 2
            && matches!(
                r.operator,
                RedirectOperator::Output | RedirectOperator::Append
            )
    });
    if let Some(err_redirect) = err_redirect
        && let Ok(mut file) = open_output_redirect(err_redirect, noclobber)
    {
        let _ = writeln!(file, "{message}");
        return;
    }
    eprintln!("{message}");
}

fn exec_external(
    name: &str,
    args: &[String],
    shell: &mut Shell,
    assignments: &[(String, String)],
    redirects: &[ExpandedRedirect],
    stdio: PipelineStdio,
) -> (i32, Option<Vec<u8>>) {
    let PipelineStdio {
        stdin_data,
        capture_output,
        standalone,
    } = stdio;

    let mut effective_env: HashMap<String, String> = shell.env_vars.clone();
    for (name, value) in assignments {
        effective_env.insert(name.clone(), value.clone());
    }

    let job_control = standalone && shell.job_control_active;

    let mut command = std::process::Command::new(name);
    command
        .args(args)
        .current_dir(&shell.cwd)
        .env_clear()
        .envs(&effective_env);
    // Every external spawn gets its shell-installed signal dispositions
    // reset before it execs (see that function's docs) — `job_control`
    // additionally makes *this* spawn the leader of a brand-new process
    // group. Since `standalone` (which `job_control` implies) means this
    // pipeline has exactly one stage, `stdin_data`/`capture_output`
    // below are always `None`/`false` in that case (nothing could have
    // piped into or out of a pipeline with no other stage) — so the
    // ordinary `Stdio::inherit()` branches below are always what's taken
    // for a job-controlled spawn, with no interaction between the two
    // concerns to reason about further.
    shell.prepare_child_for_job_control(&mut command, job_control);

    let input_redirect = redirects
        .iter()
        .rev()
        .find(|r| r.fd == 0 && matches!(r.operator, RedirectOperator::Input));
    let output_redirect = redirects.iter().rev().find(|r| {
        r.fd == 1
            && matches!(
                r.operator,
                RedirectOperator::Output | RedirectOperator::Append
            )
    });
    let err_redirect = redirects.iter().rev().find(|r| {
        r.fd == 2
            && matches!(
                r.operator,
                RedirectOperator::Output | RedirectOperator::Append
            )
    });

    match input_redirect {
        Some(redirect) => match std::fs::File::open(&redirect.target) {
            Ok(file) => {
                command.stdin(file);
            }
            Err(err) => {
                report_external_diagnostic(
                    &format!("conch: {}: {err}", redirect.target),
                    redirects,
                    shell.noclobber,
                );
                return (1, None);
            }
        },
        None => {
            command.stdin(if stdin_data.is_some() {
                Stdio::piped()
            } else {
                Stdio::inherit()
            });
        }
    }

    match output_redirect {
        Some(redirect) => match open_output_redirect(redirect, shell.noclobber) {
            Ok(file) => {
                command.stdout(file);
            }
            Err(err) => {
                report_external_diagnostic(
                    &format!("conch: {}: {err}", redirect.target),
                    redirects,
                    shell.noclobber,
                );
                return (1, None);
            }
        },
        None => {
            command.stdout(if capture_output {
                Stdio::piped()
            } else {
                Stdio::inherit()
            });
        }
    }

    // The external command's own real-time stderr output (as opposed to
    // *conch's own* diagnostics about it, handled via
    // `report_external_diagnostic` throughout this function) — mirrors
    // `input_redirect`/`output_redirect`'s exact shape immediately above.
    // Real bug, found and fixed alongside this function's diagnostic
    // paths: before this, a `2>`/`2>>` redirect on an external command
    // was silently never honored at all for the command's own output
    // (only conch's *builtin* stderr-redirect handling, `exec_builtin`,
    // existed) — the child always inherited the real process stderr
    // regardless of any redirect specified.
    match err_redirect {
        Some(redirect) => match open_output_redirect(redirect, shell.noclobber) {
            Ok(file) => {
                command.stderr(file);
            }
            Err(err) => {
                report_external_diagnostic(
                    &format!("conch: {}: {err}", redirect.target),
                    redirects,
                    shell.noclobber,
                );
                return (1, None);
            }
        },
        None => {
            command.stderr(Stdio::inherit());
        }
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            report_external_diagnostic(
                &format!("conch: {name}: {err}"),
                redirects,
                shell.noclobber,
            );
            return (127, None);
        }
    };

    if job_control {
        // `standalone` (which `job_control` implies) guarantees
        // `stdin_data`/`capture_output` are already `None`/`false` (see
        // this function's own docs), so there's nothing else to wire up
        // here — just hand the job the terminal, wait for it, and
        // reclaim the terminal afterward.
        let pid = Pid::from_raw(child.id() as i32);
        let command_text = std::iter::once(name.to_string())
            .chain(args.iter().cloned())
            .collect::<Vec<_>>()
            .join(" ");
        return (run_foreground_job(shell, pid, &command_text, None), None);
    }

    if let (Some(data), Some(mut stdin)) = (stdin_data, child.stdin.take()) {
        let _ = stdin.write_all(&data);
        // Dropping `stdin` here closes the pipe, signaling EOF to the
        // child — required for commands that read to completion (`cat`,
        // `wc`, ...) rather than processing incrementally.
    }

    if output_redirect.is_none() && capture_output {
        match child.wait_with_output() {
            Ok(output) => (exit_code_of(output.status), Some(output.stdout)),
            Err(err) => {
                report_external_diagnostic(
                    &format!("conch: {name}: {err}"),
                    redirects,
                    shell.noclobber,
                );
                (1, None)
            }
        }
    } else {
        match child.wait() {
            Ok(status) => (exit_code_of(status), None),
            Err(err) => {
                report_external_diagnostic(
                    &format!("conch: {name}: {err}"),
                    redirects,
                    shell.noclobber,
                );
                (1, None)
            }
        }
    }
}

fn write_redirect(
    redirect: &ExpandedRedirect,
    data: &[u8],
    noclobber: bool,
) -> std::io::Result<()> {
    let mut file = open_output_redirect(redirect, noclobber)?;
    file.write_all(data)
}

/// Opens `redirect`'s target for an output (`>`) or append (`>>`)
/// redirect — shared by a builtin's own redirect application
/// ([`write_redirect`]) and an external command's ([`exec_external`]),
/// which both need the identical `set -C` (noclobber) behavior: `>>` is
/// never affected by it (appending to an existing file was never
/// "clobbering" it in the first place), but a plain `>` to a file that
/// already exists is refused outright rather than silently truncated.
///
/// Uses `create_new` (an atomic "fail if it already exists" open) for
/// the noclobber case rather than a separate existence check followed by
/// a later, distinct open — the same check-and-use-must-be-one-syscall,
/// TOCTOU-avoiding pattern already followed everywhere else in this
/// codebase that resolves a path before using it (see e.g.
/// `conch-shell-builtins`'s `.`/`source` docs for the same reasoning
/// applied there).
fn open_output_redirect(
    redirect: &ExpandedRedirect,
    noclobber: bool,
) -> std::io::Result<std::fs::File> {
    let is_append = matches!(redirect.operator, RedirectOperator::Append);
    if noclobber && !is_append {
        return std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&redirect.target);
    }
    std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .append(is_append)
        .truncate(!is_append)
        .open(&redirect.target)
}

// ---- command resolution: shared by the `type`/`command` builtins --------

/// What kind of command `name` resolves to, in the same function → builtin
/// → `PATH` order [`exec_simple`] itself already uses (see that function's
/// own docs for why POSIX's "special builtins ahead of functions" ordering
/// is deliberately not followed here) — shared by the `type`/`command`
/// builtins (`conch-shell-builtins`) so neither reimplements command
/// resolution, per this phase's own design note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandResolution {
    /// A shell function (`name() { ...; }`).
    Function,
    /// A builtin (special or regular alike — `type`/`command` don't need
    /// to distinguish the two).
    Builtin,
    /// An external command found on `PATH` (or, if `name` itself
    /// contained a `/`, at that exact path) — carries the resolved path
    /// exactly as `type`/`command -v` print it.
    External(String),
    /// Nothing by that name resolves at all.
    NotFound,
}

/// Resolves `name` the same way [`exec_simple`] would actually run it —
/// see [`CommandResolution`]'s own docs.
#[must_use]
pub fn resolve_command(shell: &Shell, name: &str) -> CommandResolution {
    if shell.functions.contains_key(name) {
        return CommandResolution::Function;
    }
    if shell.builtin(name).is_some() {
        return CommandResolution::Builtin;
    }
    match find_in_path(shell, name) {
        Some(path) => CommandResolution::External(path),
        None => CommandResolution::NotFound,
    }
}

/// Searches `shell`'s `PATH` for an executable file named `name`,
/// returning its resolved path — the `PATH`-search half of command
/// resolution POSIX 2.9.1.1 describes, and the one piece of that
/// resolution `exec_external`'s own spawn doesn't need to do itself
/// (`std::process::Command::new` already performs the equivalent search
/// internally, via `execvp`-shaped behavior, using the child's own
/// environment) — this exists purely for `type`/`command -v`
/// (`conch-shell-builtins`), which need to *report* the resolved path
/// without actually running anything.
///
/// A `name` containing a `/` is never searched for on `PATH` at all
/// (POSIX 2.9.1.1: "If the command name contains a <slash>, ... a
/// pathname search shall not be performed") — it's checked directly, and
/// only that one path is ever returned.
#[must_use]
pub fn find_in_path(shell: &Shell, name: &str) -> Option<String> {
    if name.contains('/') {
        return is_executable_file(std::path::Path::new(name)).then(|| name.to_string());
    }
    let path_var = shell.get_var("PATH").unwrap_or("");
    for dir in path_var.split(':') {
        let dir = if dir.is_empty() { "." } else { dir };
        let candidate = std::path::Path::new(dir).join(name);
        if is_executable_file(&candidate) {
            return Some(candidate.to_string_lossy().into_owned());
        }
    }
    None
}

/// Enumerates every executable name on `$PATH` (`path_var`, in the same
/// colon-separated form [`Shell::get_var`]`("PATH")` returns) that starts
/// with `prefix` — the tab-completion *candidate-listing* sibling of
/// [`find_in_path`] (which only ever resolves one exact `name` to its
/// first match, for `type`/`command -v`). Used by the interactive line
/// editor's command-position completion (`conch`'s own binary crate),
/// which needs every match, not just the one that would actually run.
///
/// Deliberately takes the raw `$PATH` string rather than `&Shell` (unlike
/// [`find_in_path`]/[`resolve_command`]): the completer only ever has a
/// point-in-time *snapshot* of the shell's completable state on hand
/// while the line editor owns the terminal (see `conch`'s own
/// `completion` module docs for why), not a live `&Shell` — a plain
/// `&str` parameter is both what that snapshot can actually cheaply hold
/// and easier to unit-test standalone.
///
/// Deliberately does **not** replicate [`find_in_path`]'s "a name
/// containing `/` is never `PATH`-searched" rule: that POSIX 2.9.1.1 rule
/// is about *resolving one exact command name* for execution, not about
/// what's valid to type at command position — a `prefix` containing `/`
/// simply won't match any bare executable *name* here (this only ever
/// compares against a directory entry's own file name, never a full
/// path), which in practice means it just returns nothing, correctly
/// deferring that case to filename completion instead (`conch`'s own
/// word-boundary scanner only ever calls this at command position, which
/// by construction is checked separately from argument-position filename
/// completion).
///
/// A name that exists in more than one `PATH` directory is only reported
/// once — the *first* directory's copy is what `PATH`-search would
/// actually run (see [`find_in_path`]), so listing the same name again
/// for a shadowed copy further down `PATH` would be misleading. Results
/// are returned in `PATH`-scan order, not sorted; callers that want a
/// stable display order should sort themselves (`conch`'s own completer
/// does, alongside function/builtin/alias names from other sources).
#[must_use]
pub fn list_path_executables(path_var: &str, prefix: &str) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut results = Vec::new();
    for dir in path_var.split(':') {
        let dir = if dir.is_empty() { "." } else { dir };
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            continue;
        };
        for entry in read_dir.flatten() {
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                continue;
            };
            if !name.starts_with(prefix) || !seen.insert(name.clone()) {
                continue;
            }
            if is_executable_file(&entry.path()) {
                results.push(name);
            }
        }
    }
    results
}

/// Whether `path` is a regular file with at least one executable bit
/// set — POSIX's own definition of a `PATH`-search match (2.9.1.1: "the
/// utility shall be searched for using the value of PATH ... an
/// executable file"), not merely a file that exists.
fn is_executable_file(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

// ---- umask ----------------------------------------------------------------

/// The process's current umask (POSIX permission-bits form, e.g. `0o022`)
/// — the `umask` builtin (`conch-shell-builtins`)'s read half. There's no
/// syscall that only *reads* the umask without also setting it
/// (`umask(2)` always both sets a new value and returns the old one), so
/// this uses the standard read-old/write-back-immediately trick: briefly
/// set it to an arbitrary value, capture what it was, then restore that
/// exact value — no window where some *other* thread's file creation
/// could observe the wrong mask matters here (this crate's real
/// execution model is single-threaded — see e.g.
/// `Shell::reap_children`'s own docs making the same assumption
/// explicit), so this is safe in practice despite looking racy in
/// isolation.
#[must_use]
pub fn current_umask() -> u32 {
    let previous = nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(0o777));
    nix::sys::stat::umask(previous);
    u32::from(previous.bits())
}

/// Sets the process's umask to `mask` (only the low 9 permission bits are
/// meaningful — matching real bash silently truncating a wider value the
/// same way) — the `umask` builtin's write half.
pub fn set_umask(mask: u32) {
    let bits = u16::try_from(mask & 0o777).unwrap_or(0o777);
    nix::sys::stat::umask(nix::sys::stat::Mode::from_bits_truncate(bits));
}

// ---- system identity: `PS1`'s `\h`/`\H`/`\$` prompt escapes ---------------

/// The local system's host name (POSIX `gethostname(2)`) — `PS1`'s `\h`/
/// `\H` prompt escapes' own read (`conch`'s binary crate; see that
/// crate's `prompt` module). Deliberately lives here, not in `conch`
/// itself: this crate already depends on `nix` for job control/signals,
/// while the binary crate has none of its own (matching
/// `conch-shell-builtins`'s own "no direct `nix` dependency" precedent,
/// for the same reason — see that crate's module docs), so wrapping the
/// one syscall here avoids adding a second, `conch`-crate-only edge to it
/// for what both crates would otherwise duplicate.
///
/// Returns `None` if `gethostname(2)` itself fails, or if the result
/// isn't valid UTF-8 (a hostname is conventionally ASCII; a caller
/// falling back to an empty string in that unlikely case, matching bash's
/// own silent-empty fallback, is preferable to lossily mangling raw
/// bytes).
#[must_use]
pub fn hostname() -> Option<String> {
    nix::unistd::gethostname().ok()?.into_string().ok()
}

/// Whether this process's *effective* user ID is root (POSIX
/// `geteuid(2)` == 0) — `PS1`'s `\$` prompt escape's own read (`#` for
/// root, `$` otherwise, matching real bash). See [`hostname`]'s docs for
/// why this lives here rather than in `conch`'s own binary crate.
#[must_use]
pub fn is_effective_root() -> bool {
    nix::unistd::geteuid().is_root()
}

// ---- file-permission checks: shared by the `test`/`[` builtin -------------

/// Whether the *real* process (this shell) has read/write/execute
/// permission on `path`, per `access(2)`'s own uid/gid-aware semantics —
/// deliberately not approximated from `std::fs::Metadata`'s raw mode
/// bits (which say nothing about *this process's* uid/gid relative to the
/// file's owner/group), and deliberately in `conch-shell-core` rather
/// than `conch-shell-builtins` (which has no direct `nix` dependency of
/// its own — see this crate's own module docs) even though the only
/// caller today is the `test`/`[` builtin's `-r`/`-w`/`-x` unary
/// operators.
#[must_use]
pub fn path_readable(path: &str) -> bool {
    nix::unistd::access(path, nix::unistd::AccessFlags::R_OK).is_ok()
}

/// See [`path_readable`]'s docs.
#[must_use]
pub fn path_writable(path: &str) -> bool {
    nix::unistd::access(path, nix::unistd::AccessFlags::W_OK).is_ok()
}

/// See [`path_readable`]'s docs.
#[must_use]
pub fn path_executable(path: &str) -> bool {
    nix::unistd::access(path, nix::unistd::AccessFlags::X_OK).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use conch_shell_parser::parse;

    fn run(input: &str, shell: &mut Shell) -> i32 {
        exec_command_list(&parse(input).unwrap(), shell)
    }

    #[test]
    fn true_and_false_builtins_via_external_binaries() {
        let mut shell = Shell::new();
        assert_eq!(run("true", &mut shell), 0);
        assert_eq!(run("false", &mut shell), 1);
    }

    #[test]
    fn and_short_circuits_on_failure() {
        let mut shell = Shell::new();
        crate::Shell::register_builtin(&mut shell, "would_not_run", Box::new(RecordingBuiltin));
        // false && would_not_run: the second must not execute.
        assert_eq!(run("false && would_not_run", &mut shell), 1);
    }

    #[test]
    fn or_runs_only_on_failure() {
        let mut shell = Shell::new();
        assert_eq!(run("true || false", &mut shell), 0);
    }

    #[test]
    fn bare_assignment_persists_in_shell_vars() {
        let mut shell = Shell::new();
        run("FOO=bar", &mut shell);
        assert_eq!(shell.get_var("FOO"), Some("bar"));
    }

    #[test]
    fn prefix_assignment_does_not_persist() {
        let mut shell = Shell::new();
        // `FOO=bar true` should not leave FOO set afterward.
        run("FOO=temp true", &mut shell);
        assert_eq!(shell.get_var("FOO"), None);
    }

    // ---- POSIX 2.9.1.3 "no command name" exit-status rule -------------------
    //
    // `run_command_substitution` itself is deliberately not unit-tested
    // here (see `crate::expand`'s own test module's pre-existing note on
    // why: `env::current_exe()` resolves to *this test binary* during
    // `cargo test`, not the real `conch` executable, so any command
    // substitution genuinely exercised through this crate's own test
    // suite would spawn nonsense) -- the differential suite
    // (`tests/conch-difftest/corpus/phase7/command_substitution_exit_status_hardening.toml`)
    // covers that against the real, compiled binary instead, and was
    // directly confirmed passing (all five cases, including the stderr
    // one) while implementing this fix. What *is* unit-testable here,
    // and what these two tests pin down, is `exec_simple`'s own
    // `status_before_expansion` comparison logic in isolation: a
    // substitution-free bare assignment (or zero-fields command name)
    // must reset to the POSIX-mandated default of `0`, never leak an
    // entirely unrelated *prior* command's own exit status just because
    // nothing touched `shell.last_status` during this command's own
    // expansion.

    #[test]
    fn bare_assignment_with_no_substitution_resets_status_to_zero() {
        // Confirmed against real bash: `false; x=5; echo $?` is `0`, not
        // `1`, even though `$?` was `1` immediately before `x=5` ran.
        let mut shell = Shell::new();
        assert_eq!(run("false; x=5", &mut shell), 0);
        assert_eq!(shell.get_var("x"), Some("5"));
    }

    #[test]
    fn command_name_expanding_to_zero_fields_with_no_substitution_resets_status_to_zero() {
        // Same rule, reached via the sibling "nothing to run" branch
        // (an unset, unquoted parameter as the command name, rather than
        // a bare assignment) -- confirmed against real bash:
        // `false; $UNSET_CONCH_TEST_VAR; echo $?` is `0`.
        let mut shell = Shell::new();
        assert_eq!(run("false; $UNSET_CONCH_TEST_VAR", &mut shell), 0);
    }

    #[test]
    fn pipeline_pipes_output_between_external_commands() {
        let mut shell = Shell::new();
        // exit status of a pipeline is the last command's.
        assert_eq!(run("echo hi | true", &mut shell), 0);
    }

    struct RecordingBuiltin;
    impl crate::Builtin for RecordingBuiltin {
        fn run(
            &self,
            _shell: &mut Shell,
            _args: &[String],
            _stdin: &mut dyn std::io::Read,
            _stdout: &mut dyn std::io::Write,
            _stderr: &mut dyn std::io::Write,
        ) -> i32 {
            panic!("should have been short-circuited");
        }
    }

    // ---- compound commands: if/case ----------------------------------------

    #[test]
    fn if_runs_the_first_true_branch() {
        let mut shell = Shell::new();
        assert_eq!(run("if true; then echo a; else echo b; fi", &mut shell), 0);
    }

    #[test]
    fn if_falls_through_to_else() {
        let mut shell = Shell::new();
        run("if false; then X=a; else X=b; fi", &mut shell);
        assert_eq!(shell.get_var("X"), Some("b"));
    }

    #[test]
    fn if_with_no_matching_branch_and_no_else_is_status_zero() {
        let mut shell = Shell::new();
        assert_eq!(run("if false; then echo a; fi", &mut shell), 0);
    }

    #[test]
    fn case_runs_the_first_matching_arm_only() {
        let mut shell = Shell::new();
        run(
            "X=b; case $X in a) Y=A;; b|c) Y=BC;; *) Y=other;; esac",
            &mut shell,
        );
        assert_eq!(shell.get_var("Y"), Some("BC"));
    }

    #[test]
    fn case_with_no_matching_arm_is_status_zero() {
        let mut shell = Shell::new();
        assert_eq!(run("case zzz in a) echo a;; esac", &mut shell), 0);
    }

    // ---- compound commands: for/while/until ---------------------------------

    #[test]
    fn for_loop_iterates_over_wordlist() {
        let mut shell = Shell::new();
        run("for x in a b c; do Y=$x; done", &mut shell);
        assert_eq!(shell.get_var("Y"), Some("c"));
    }

    #[test]
    fn while_loop_runs_until_condition_fails() {
        let mut shell = Shell::new();
        run(
            "I=0; while [ \"$I\" != 3 ]; do I=$((I+1)); done",
            &mut shell,
        );
        assert_eq!(shell.get_var("I"), Some("3"));
    }

    #[test]
    fn while_loop_exit_status_is_last_body_execution_not_zero() {
        // Confirmed against real bash: a while loop that runs its body
        // and stops normally keeps the body's last exit status.
        let mut shell = Shell::new();
        assert_eq!(
            run(
                "I=0; while [ \"$I\" != 2 ]; do I=$((I+1)); false; done",
                &mut shell
            ),
            1
        );
    }

    #[test]
    fn while_loop_that_never_runs_its_body_is_status_zero() {
        let mut shell = Shell::new();
        assert_eq!(run("while false; do false; done", &mut shell), 0);
    }

    // ---- set -e (errexit) ---------------------------------------------------
    //
    // A failing command with `errexit` actually enabled calls
    // `std::process::exit` directly, which would abort this entire test
    // binary (Rust unit tests share one process) -- so, matching this
    // module's own existing precedent for `report_expand_error`'s
    // identically-shaped fatal path (never unit-tested for the actual
    // process exit either, only for the value that would trigger it),
    // these tests only ever exercise the *suppression* paths: if the
    // suppression logic below is correct, none of them ever reach the
    // `std::process::exit` call at all, so the test process surviving
    // normally *is* the assertion. A real end-to-end "does it actually
    // exit" check belongs in `tests/conch-difftest`, against the real
    // compiled binary in its own process, not here.

    #[test]
    fn errexit_does_not_trigger_for_a_failing_if_condition() {
        let mut shell = Shell::new();
        shell.errexit = true;
        assert_eq!(run("if false; then :; fi", &mut shell), 0);
    }

    #[test]
    fn errexit_does_not_trigger_for_a_failing_while_condition() {
        let mut shell = Shell::new();
        shell.errexit = true;
        assert_eq!(run("while false; do :; done", &mut shell), 0);
    }

    #[test]
    fn errexit_does_not_trigger_on_a_non_last_member_of_an_and_or_chain() {
        // Confirmed against real bash/dash: `set -e; false && FOO=ran`
        // does not abort -- `false` is not the chain's last member
        // (`FOO=ran` is, and it never got a chance to run, confirmed by
        // `FOO` staying unset below), so its failure is exempt even
        // though it's also the whole chain's own final resolved status.
        // Before this fix, conch called `std::process::exit(1)` right
        // here, which would abort this entire test binary -- so this
        // test's own survival (reaching either assertion at all) is part
        // of what it's confirming, exactly like this module's other
        // errexit tests.
        let mut shell = Shell::new();
        shell.errexit = true;
        assert_eq!(run("false && FOO=ran", &mut shell), 1);
        assert_eq!(shell.get_var("FOO"), None);
        // And the script must keep running afterward, exactly like real
        // bash/dash's `echo "reached-after-and"` in the equivalent
        // differential-suite case -- not just "didn't crash on this one
        // line".
        assert_eq!(run("false && FOO=ran; BAR=reached", &mut shell), 0);
        assert_eq!(shell.get_var("BAR"), Some("reached"));
    }

    #[test]
    fn exec_and_or_eligibility_tracks_whether_the_syntactic_last_member_ran() {
        // Direct unit tests of `exec_and_or`'s own (status, eligible)
        // pair -- deliberately bypassing `shell.errexit`/`exec_command_list`
        // entirely (no risk of an accidental `std::process::exit` no
        // matter what this function returns) so every case from its own
        // doc comment, including the "still aborts" ones that can't
        // safely be exercised end-to-end in this same process, gets
        // direct coverage.
        let mut shell = Shell::new();

        // Single pipeline, no chain at all: trivially "the last member".
        assert_eq!(exec_and_or(&one_and_or("false"), &mut shell), (1, true));

        // The failing member is *not* the chain's last one (it short-
        // circuits the real last member, which never runs) -- exempt.
        assert_eq!(
            exec_and_or(&one_and_or("false && true"), &mut shell),
            (1, false)
        );
        assert_eq!(
            exec_and_or(&one_and_or("true || false"), &mut shell),
            (0, false)
        );

        // The chain's actual last member runs and fails -- not exempt,
        // confirmed against real bash even though a middle member was
        // itself skipped along the way (see this function's own docs).
        assert_eq!(
            exec_and_or(&one_and_or("false || false"), &mut shell),
            (1, true)
        );
        assert_eq!(
            exec_and_or(&one_and_or("true && false"), &mut shell),
            (1, true)
        );
        assert_eq!(
            exec_and_or(&one_and_or("false && true || false"), &mut shell),
            (1, true)
        );
        assert_eq!(
            exec_and_or(&one_and_or("true && true && false"), &mut shell),
            (1, true)
        );
    }

    #[test]
    fn errexit_suppression_resets_after_the_condition_finishes() {
        // Once the loop's *condition* stops being evaluated, an ordinary
        // failing command in the *body* must still be free to trigger
        // errexit normally -- this only confirms `errexit_suppressed`
        // doesn't leak past the condition it was scoped to (the run
        // itself never reaches a failing body command, so this can't
        // actually crash the test binary either way).
        let mut shell = Shell::new();
        assert_eq!(shell.errexit_suppressed, 0);
        run("if true; then :; fi", &mut shell);
        assert_eq!(shell.errexit_suppressed, 0);
    }

    // ---- set -x (xtrace) -----------------------------------------------------
    //
    // The trace line itself goes straight to the real process stderr
    // (`eprintln!`), not through any capturable writer this module's own
    // tests already have a handle on -- so, like `run_foreground_job`'s
    // own diagnostics, this only confirms xtrace doesn't change
    // behavior/exit status, not the exact printed text.

    #[test]
    fn xtrace_does_not_change_exit_status_or_behavior() {
        let mut shell = Shell::new();
        shell.xtrace = true;
        assert_eq!(run("true", &mut shell), 0);
        assert_eq!(run("false", &mut shell), 1);
    }

    // ---- set -C (noclobber) ---------------------------------------------------

    #[test]
    fn noclobber_refuses_to_overwrite_an_existing_file_via_output_redirect() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "first\n").unwrap();
        let mut shell = Shell::new();
        shell.noclobber = true;
        let script = format!("echo second > {}", path.display());
        run(&script, &mut shell);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\n");
    }

    #[test]
    fn without_noclobber_the_same_redirect_overwrites_normally() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "first\n").unwrap();
        let mut shell = Shell::new();
        let script = format!("echo second > {}", path.display());
        run(&script, &mut shell);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second\n");
    }

    #[test]
    fn noclobber_does_not_affect_append_redirects() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "first\n").unwrap();
        let mut shell = Shell::new();
        shell.noclobber = true;
        let script = format!("echo second >> {}", path.display());
        run(&script, &mut shell);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\nsecond\n");
    }

    // ---- a builtin's stderr respects a `2>` redirect on its own command -----
    //
    // Real bug, caught by a testing/security review: `eval "if"
    // 2>/dev/null`'s own syntax-error diagnostic (written through
    // `Eval`'s own `stderr` parameter, exactly as every builtin here is
    // supposed to) was leaking to the real process stderr regardless --
    // `exec_builtin` unconditionally flushed a builtin's captured
    // `stderr_buf` to the real stderr with no redirect check at all, a
    // general gap affecting *every* builtin's stderr, not anything
    // `eval`-specific (its own stdout already correctly respected a
    // `1>`/`>`/`>>` redirect; only the stderr half was missing the
    // identical treatment).

    struct StderrWriter;
    impl crate::Builtin for StderrWriter {
        fn run(
            &self,
            _shell: &mut Shell,
            _args: &[String],
            _stdin: &mut dyn std::io::Read,
            _stdout: &mut dyn std::io::Write,
            stderr: &mut dyn std::io::Write,
        ) -> i32 {
            let _ = writeln!(stderr, "oops");
            0
        }
    }

    #[test]
    fn a_builtins_stderr_output_respects_a_2_redirect_on_its_own_command() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("err.txt");
        let mut shell = Shell::new();
        shell.register_builtin("stderrwriter", Box::new(StderrWriter));
        let script = format!("stderrwriter 2> {}", path.display());
        run(&script, &mut shell);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "oops\n");
    }

    #[test]
    fn a_builtins_stderr_output_still_goes_to_real_stderr_with_no_redirect() {
        // Not directly assertable (real stderr isn't capturable from
        // inside a unit test), but confirms the redirect-absent path
        // still runs to completion normally rather than erroring or
        // panicking now that a redirect-present branch exists alongside
        // it.
        let mut shell = Shell::new();
        shell.register_builtin("stderrwriter", Box::new(StderrWriter));
        assert_eq!(run("stderrwriter", &mut shell), 0);
    }

    // ---- an external command's own stderr respects a `2>` redirect ----------
    //
    // Same bug class as the builtin fix just above, one call site removed
    // (`exec_external`, not `exec_builtin`) — found and fixed together:
    // a command-not-found (or other `exec_external`-reported) diagnostic
    // was leaking to the real process stderr regardless of a `2>`
    // redirect on that same command, and an external command's own
    // real-time stderr output wasn't being redirected at all (the child
    // always inherited the real process stderr unconditionally, since
    // `exec_external` never called `Command::stderr` at all before this).

    #[test]
    fn command_not_found_diagnostic_respects_a_2_redirect_on_the_same_command() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("err.txt");
        let mut shell = Shell::new();
        let script = format!("nope_not_a_real_command_hopefully 2> {}", path.display());
        let status = run(&script, &mut shell);
        assert_eq!(status, 127);
        assert!(
            std::fs::read_to_string(&path)
                .unwrap()
                .contains("nope_not_a_real_command_hopefully")
        );
    }

    #[test]
    fn an_external_commands_own_stderr_output_respects_a_2_redirect() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("does_not_exist");
        let err_path = dir.path().join("err.txt");
        let mut shell = Shell::new();
        let script = format!("ls {} 2> {}", missing.display(), err_path.display());
        run(&script, &mut shell);
        let captured = std::fs::read_to_string(&err_path).unwrap();
        assert!(
            !captured.is_empty(),
            "expected `ls`'s own stderr to land in the redirect target"
        );
    }

    // ---- subshell / brace group isolation -----------------------------------
    //
    // Subshell *execution* (isolation of cwd/variables/exit, exit-status
    // propagation to the parent) is deliberately not unit-tested here:
    // exec_subshell spawns `env::current_exe()`, which resolves to this
    // test binary during `cargo test`, not the real `conch` executable —
    // the same reason `run_command_substitution`'s own tests
    // (`conch-shell-core::expand`) don't cover that half of command
    // substitution either. This is exactly what `tests/conch-difftest`'s
    // Phase 3 corpus is for, and it covers this against the real
    // compiled binary.

    #[test]
    fn brace_group_leaks_variable_assignments() {
        let mut shell = Shell::new();
        shell.shell_vars.insert("X".into(), "outer".into());
        run("{ X=inner; }", &mut shell);
        assert_eq!(shell.get_var("X"), Some("inner"));
    }

    // ---- exec_async's bare-`"$0" -c <script>` unwrapping ---------------------
    //
    // `exec_async` itself is deliberately not unit-tested end-to-end here,
    // for the identical reason `exec_subshell` isn't (see the section
    // above): `spawn_background_job` re-execs `env::current_exe()`, which
    // during `cargo test` is this test binary, not the real `conch`
    // executable. `bare_self_reexec_script`/`is_self_invocation` are pure,
    // side-effect-free AST/string checks with no process of their own to
    // spawn, though, so *those* are fully testable directly — this is
    // exactly the shape the real bug lived in (see `exec_async`'s own
    // docs), and the differential suite's `phase5/kill.toml` covers the
    // real, end-to-end process behavior this exists to fix.

    fn one_and_or(input: &str) -> AndOrList {
        let mut list = parse(input).unwrap();
        assert_eq!(list.items.len(), 1);
        list.items.remove(0).and_or
    }

    #[test]
    fn recognizes_bare_and_double_quoted_dollar_zero_as_self() {
        let shell = Shell::new();
        assert!(is_self_invocation(&parse_word("$0"), &shell));
        assert!(is_self_invocation(&parse_word("\"$0\""), &shell));
    }

    #[test]
    fn does_not_recognize_an_unrelated_variable_as_self() {
        let shell = Shell::new();
        assert!(!is_self_invocation(&parse_word("$OTHER"), &shell));
        assert!(!is_self_invocation(&parse_word("some_prog"), &shell));
    }

    #[test]
    fn recognizes_a_literal_path_matching_arg0_as_self() {
        let mut shell = Shell::new();
        shell.arg0 = "/path/to/myshell".to_string();
        assert!(is_self_invocation(&parse_word("/path/to/myshell"), &shell));
        assert!(!is_self_invocation(&parse_word("/some/other/bin"), &shell));
    }

    #[test]
    fn bare_self_reexec_script_matches_dollar_zero_dash_c_script() {
        let shell = Shell::new();
        let and_or = one_and_or("\"$0\" -c \"trap '' TERM; sleep 5\"");
        let script = bare_self_reexec_script(&and_or, &shell).expect("shape should match");
        // The script word round-trips (quote removal only, no variables
        // here) to exactly the text that would become the nested `-c`
        // argument.
        let mut shell = shell;
        assert_eq!(
            expand_word_single(script, &mut shell).unwrap(),
            "trap '' TERM; sleep 5"
        );
    }

    #[test]
    fn bare_self_reexec_script_rejects_an_ordinary_external_command() {
        let shell = Shell::new();
        let and_or = one_and_or("sleep 5");
        assert!(bare_self_reexec_script(&and_or, &shell).is_none());
    }

    #[test]
    fn bare_self_reexec_script_rejects_trailing_args_past_the_script() {
        let shell = Shell::new();
        let and_or = one_and_or("\"$0\" -c \"echo hi\" extra");
        assert!(bare_self_reexec_script(&and_or, &shell).is_none());
    }

    #[test]
    fn bare_self_reexec_script_rejects_a_pipeline_or_and_or_chain() {
        let shell = Shell::new();
        assert!(
            bare_self_reexec_script(&one_and_or("\"$0\" -c \"echo hi\" | cat"), &shell).is_none()
        );
        assert!(
            bare_self_reexec_script(&one_and_or("\"$0\" -c \"echo hi\" && echo also"), &shell)
                .is_none()
        );
    }

    #[test]
    fn bare_self_reexec_script_rejects_a_prefix_assignment_or_redirect() {
        let shell = Shell::new();
        assert!(
            bare_self_reexec_script(&one_and_or("FOO=bar \"$0\" -c \"echo hi\""), &shell).is_none()
        );
        assert!(
            bare_self_reexec_script(&one_and_or("\"$0\" -c \"echo hi\" > out.txt"), &shell)
                .is_none()
        );
    }

    /// Parses `input` as a single word (via a one-argument `echo` command,
    /// since the parser has no standalone "parse just a word" entry
    /// point) — the private `is_self_invocation`/`bare_self_reexec_script`
    /// helpers above only ever need a [`Word`], not a whole command.
    fn parse_word(input: &str) -> Word {
        let and_or = one_and_or(&format!("echo {input}"));
        let [Command::Simple(simple)] = and_or.first.commands.as_slice() else {
            panic!("expected a simple command");
        };
        simple.args[0].clone()
    }

    // ---- break/continue propagation mechanism --------------------------------
    //
    // conch-core doesn't depend on conch-shell-builtins (the real `break`/
    // `continue` implementations live there, on top of this crate) --
    // these test doubles set `Shell::pending_control_flow` exactly the
    // way those builtins do, so the propagation mechanism itself
    // (`exec_command_list`/`exec_for`/`exec_while_or_until`/
    // `take_loop_control_flow`/`exec_subshell`) can be exercised in
    // isolation, per-level, without spawning a real conch binary the way
    // `tests/conch-difftest`'s Phase 3 corpus does for full end-to-end
    // coverage.

    struct TestBreak(u32);
    impl crate::Builtin for TestBreak {
        fn run(
            &self,
            shell: &mut Shell,
            _args: &[String],
            _stdin: &mut dyn std::io::Read,
            _stdout: &mut dyn std::io::Write,
            _stderr: &mut dyn std::io::Write,
        ) -> i32 {
            shell.pending_control_flow = Some(ControlFlow::Break(self.0));
            0
        }
    }

    struct TestContinue(u32);
    impl crate::Builtin for TestContinue {
        fn run(
            &self,
            shell: &mut Shell,
            _args: &[String],
            _stdin: &mut dyn std::io::Read,
            _stdout: &mut dyn std::io::Write,
            _stderr: &mut dyn std::io::Write,
        ) -> i32 {
            shell.pending_control_flow = Some(ControlFlow::Continue(self.0));
            0
        }
    }

    fn shell_with_control_flow_builtins() -> Shell {
        let mut shell = Shell::new();
        shell.register_builtin("tbreak", Box::new(TestBreak(1)));
        shell.register_builtin("tbreak2", Box::new(TestBreak(2)));
        shell.register_builtin("tbreak5", Box::new(TestBreak(5)));
        shell.register_builtin("tcontinue", Box::new(TestContinue(1)));
        shell.register_builtin("tcontinue3", Box::new(TestContinue(3)));
        shell
    }

    #[test]
    fn break_stops_a_while_loop_immediately() {
        let mut shell = shell_with_control_flow_builtins();
        run("I=0; while true; do I=$((I+1)); tbreak; done", &mut shell);
        assert_eq!(shell.get_var("I"), Some("1"));
        assert_eq!(shell.pending_control_flow, None);
        assert_eq!(shell.loop_depth, 0);
    }

    #[test]
    fn continue_skips_the_rest_of_the_current_iteration() {
        let mut shell = shell_with_control_flow_builtins();
        run(
            "for x in a b; do Y=$Y$x; tcontinue; Y=${Y}Z; done",
            &mut shell,
        );
        assert_eq!(shell.get_var("Y"), Some("ab"));
    }

    #[test]
    fn nested_break_2_unwinds_both_loops() {
        let mut shell = shell_with_control_flow_builtins();
        run(
            "for i in 1 2; do for j in a b; do L=$L$i$j; tbreak2; done; done",
            &mut shell,
        );
        // Only the very first inner iteration (i=1,j=a) ever runs.
        assert_eq!(shell.get_var("L"), Some("1a"));
        assert_eq!(shell.loop_depth, 0);
    }

    #[test]
    fn nested_continue_targeting_outer_loop_via_explicit_level() {
        let mut shell = shell_with_control_flow_builtins();
        shell.register_builtin("tcontinue2", Box::new(TestContinue(2)));
        run(
            "for i in 1 2; do L=$L\"(\"$i; for j in a b; do L=$L$j; tcontinue2; L=${L}Z; done; L=$L\")\"; done",
            &mut shell,
        );
        // The inner loop's first iteration (j=a) runs partially (L gets
        // "a" appended) before `continue 2` skips straight to the outer
        // loop's *next* iteration -- not just j=b and the "Z" after
        // tcontinue2, but also the outer body's own trailing `L=$L")"`,
        // since a continue targeting the outer loop skips the rest of
        // its current iteration too, not merely the inner loop.
        assert_eq!(shell.get_var("L"), Some("(1a(2a"));
    }

    #[test]
    fn break_level_exceeding_nesting_depth_breaks_every_loop_and_resumes_after() {
        let mut shell = shell_with_control_flow_builtins();
        run(
            "for i in 1 2; do L=$L$i; tbreak5; done; L=${L}after",
            &mut shell,
        );
        assert_eq!(shell.get_var("L"), Some("1after"));
        assert_eq!(shell.pending_control_flow, None);
    }

    #[test]
    fn continue_level_exceeding_nesting_depth_clamps_to_outermost_loop() {
        let mut shell = shell_with_control_flow_builtins();
        run(
            "for i in 1 2 3; do if [ \"$i\" = 2 ]; then tcontinue3; fi; L=$L$i; done",
            &mut shell,
        );
        assert_eq!(shell.get_var("L"), Some("13"));
    }

    #[test]
    fn break_outside_any_loop_does_not_stop_later_commands() {
        let mut shell = shell_with_control_flow_builtins();
        run("tbreak; X=ran", &mut shell);
        assert_eq!(shell.get_var("X"), Some("ran"));
        assert_eq!(shell.pending_control_flow, None);
    }

    // `exit`-inside-a-subshell and break/continue-inside-a-subshell
    // isolation both require actually *running* a subshell (a spawned
    // process, per exec_subshell's docs) to observe, so — like the
    // isolation tests above — they live in `tests/conch-difftest`'s
    // Phase 3 corpus instead of here.

    // ---- functions, `local`, `return`, positional parameters ---------------
    //
    // Like the break/continue section above, conch-core doesn't depend on
    // conch-shell-builtins (the real `local`/`return` builtins live
    // there) — `TestLocal`/`TestReturn` set `Shell` state exactly the way
    // those builtins do (see their own docs: `Shell::capture_var`/
    // `record_local`/`set_local`, and `ControlFlow::Return`), so
    // `exec_function_call`'s own mechanism (frame push/pop timing,
    // `loop_depth` reset, `Return` consumption) can be exercised in
    // isolation here.

    #[test]
    fn defining_a_function_is_status_zero_and_does_not_run_it() {
        let mut shell = Shell::new();
        assert_eq!(run("f() { X=ran; }", &mut shell), 0);
        assert_eq!(shell.get_var("X"), None);
    }

    #[test]
    fn later_function_definition_replaces_the_earlier_one() {
        let mut shell = Shell::new();
        run("f() { X=first; }; f() { X=second; }; f", &mut shell);
        assert_eq!(shell.get_var("X"), Some("second"));
    }

    #[test]
    fn a_function_can_shadow_even_a_special_builtin() {
        // Confirmed against real bash: outside `--posix` mode, a
        // function shadows even a special builtin like `export`/`break`
        // (only `--posix` refuses, with "is a special builtin") -- this
        // project tracks bash's real default over strict POSIX-only
        // where the two diverge (see `Shell::special_builtins`'s docs).
        struct FakeSpecial;
        impl crate::Builtin for FakeSpecial {
            fn run(
                &self,
                _shell: &mut Shell,
                _args: &[String],
                _stdin: &mut dyn std::io::Read,
                _stdout: &mut dyn std::io::Write,
                _stderr: &mut dyn std::io::Write,
            ) -> i32 {
                panic!("the real special builtin must never run once shadowed by a function");
            }
        }
        let mut shell = Shell::new();
        shell.register_special_builtin("myspecial", Box::new(FakeSpecial));
        run("myspecial() { X=shadowed; }; myspecial", &mut shell);
        assert_eq!(shell.get_var("X"), Some("shadowed"));
    }

    #[test]
    fn function_call_gets_its_own_positional_parameters_and_restores_the_callers_afterward() {
        let mut shell = Shell::new();
        shell.positional_params = vec!["top1".to_string(), "top2".to_string()];
        run("f() { INSIDE=\"$1:$2:$#\"; }; f a b c", &mut shell);
        assert_eq!(shell.get_var("INSIDE"), Some("a:b:3"));
        assert_eq!(
            shell.positional_params,
            vec!["top1".to_string(), "top2".to_string()]
        );
    }

    #[test]
    fn function_call_falling_off_the_end_uses_the_last_commands_exit_status() {
        let mut shell = Shell::new();
        assert_eq!(run("f() { true; false; }; f", &mut shell), 1);
    }

    #[test]
    fn non_local_assignment_inside_a_function_mutates_the_global() {
        // The critical subtlety this follow-up's brief called out: an
        // assignment inside a function that was *not* `local`-declared
        // there must still mutate the global, not silently shadow it.
        let mut shell = Shell::new();
        shell
            .shell_vars
            .insert("X".to_string(), "outer".to_string());
        run("f() { X=changed; }; f", &mut shell);
        assert_eq!(shell.get_var("X"), Some("changed"));
    }

    #[test]
    fn for_without_in_clause_iterates_positional_parameters() {
        let mut shell = Shell::new();
        shell.positional_params = vec!["x".to_string(), "y".to_string(), "z".to_string()];
        run("for v; do L=$L$v; done", &mut shell);
        assert_eq!(shell.get_var("L"), Some("xyz"));
    }

    struct TestReturn(i32);
    impl crate::Builtin for TestReturn {
        fn run(
            &self,
            shell: &mut Shell,
            _args: &[String],
            _stdin: &mut dyn std::io::Read,
            _stdout: &mut dyn std::io::Write,
            _stderr: &mut dyn std::io::Write,
        ) -> i32 {
            shell.pending_control_flow = Some(ControlFlow::Return(self.0));
            0
        }
    }

    #[test]
    fn return_stops_the_function_call_with_the_given_status() {
        let mut shell = Shell::new();
        shell.register_builtin("treturn5", Box::new(TestReturn(5)));
        let status = run("f() { X=before; treturn5; X=after; }; f", &mut shell);
        assert_eq!(status, 5);
        assert_eq!(shell.get_var("X"), Some("before"));
        assert_eq!(shell.pending_control_flow, None);
    }

    #[test]
    fn break_inside_a_function_does_not_escape_to_the_callers_own_loop() {
        // Confirmed against real bash: `break` inside a function only
        // ever stops a loop lexically inside that *same* function --
        // never a loop merely *calling* it, even one already running
        // when the call happens.
        let mut shell = shell_with_control_flow_builtins();
        run(
            "f() { tbreak; }; for i in 1 2 3; do L=$L$i; f; L=${L}Z; done",
            &mut shell,
        );
        assert_eq!(shell.get_var("L"), Some("1Z2Z3Z"));
        assert_eq!(shell.pending_control_flow, None);
    }

    #[test]
    fn break_inside_a_functions_own_loop_stays_inside_the_function() {
        let mut shell = shell_with_control_flow_builtins();
        run(
            "f() { for j in a b c; do L=$L$j; tbreak; done; }; f",
            &mut shell,
        );
        assert_eq!(shell.get_var("L"), Some("a"));
    }

    struct TestLocal {
        name: &'static str,
        value: &'static str,
    }
    impl crate::Builtin for TestLocal {
        fn run(
            &self,
            shell: &mut Shell,
            _args: &[String],
            _stdin: &mut dyn std::io::Read,
            _stdout: &mut dyn std::io::Write,
            _stderr: &mut dyn std::io::Write,
        ) -> i32 {
            let previous = shell.capture_var(self.name);
            shell.record_local(self.name.to_string(), previous);
            shell.set_local(self.name, Some(self.value.to_string()));
            0
        }
    }

    #[test]
    fn local_declaration_is_restored_once_the_function_call_returns() {
        let mut shell = Shell::new();
        shell
            .shell_vars
            .insert("X".to_string(), "outer".to_string());
        shell.register_builtin(
            "tlocal_inner",
            Box::new(TestLocal {
                name: "X",
                value: "inner",
            }),
        );
        run("f() { tlocal_inner; INSIDE=$X; }; f", &mut shell);
        assert_eq!(shell.get_var("INSIDE"), Some("inner"));
        assert_eq!(shell.get_var("X"), Some("outer"));
    }

    #[test]
    fn nested_call_bare_assignment_mutates_the_nearest_active_local_not_the_true_global() {
        // Confirmed against real bash: `local` is *dynamically* scoped —
        // X=g; outer() { local X=o; inner; }; inner() { X=set_by_inner; };
        // outer leaves the global X at "g", unchanged, because inner's
        // plain assignment targets outer's still-active `local X`.
        let mut shell = Shell::new();
        shell
            .shell_vars
            .insert("X".to_string(), "global".to_string());
        shell.register_builtin(
            "tlocal_outer",
            Box::new(TestLocal {
                name: "X",
                value: "outer_local",
            }),
        );
        run(
            "inner() { X=set_by_inner; }; outer() { tlocal_outer; inner; AFTER_INNER=$X; }; outer",
            &mut shell,
        );
        assert_eq!(shell.get_var("AFTER_INNER"), Some("set_by_inner"));
        assert_eq!(shell.get_var("X"), Some("global"));
    }
}
