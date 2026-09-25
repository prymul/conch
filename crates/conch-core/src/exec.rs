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
    AndOrList, CaseClause, Command, CommandList, CompoundCommand, CompoundCommandKind, ForClause,
    FunctionDefinition, IfClause, LogicalOp, Pipeline, Redirect, RedirectOperator, SimpleCommand,
    Word,
};

use crate::expand::{expand_word_as_pattern, glob_match};
use crate::{
    ControlFlow, ExpandError, Shell, brace_expand, expand_word_fields, expand_word_single,
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
        for variant in brace_expand(word) {
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
        status = exec_and_or(&item.and_or, shell);
        shell.last_status = status;
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

fn exec_and_or(and_or: &AndOrList, shell: &mut Shell) -> i32 {
    let mut status = exec_pipeline(&and_or.first, shell);
    if shell.pending_control_flow.is_some() {
        return status;
    }
    for (op, pipeline) in &and_or.rest {
        let should_run = match op {
            LogicalOp::And => status == 0,
            LogicalOp::Or => status != 0,
        };
        if should_run {
            status = exec_pipeline(pipeline, shell);
            if shell.pending_control_flow.is_some() {
                return status;
            }
        }
    }
    status
}

fn exec_pipeline(pipeline: &Pipeline, shell: &mut Shell) -> i32 {
    let mut carry_in: Option<Vec<u8>> = None;
    let mut status = 0;

    for (i, command) in pipeline.commands.iter().enumerate() {
        let is_last = i == pipeline.commands.len() - 1;
        let (this_status, output) = match command {
            Command::Simple(simple) => exec_simple(simple, shell, carry_in.take(), !is_last),
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
        .status();
    match status {
        Ok(status) => status.code().unwrap_or(1),
        Err(err) => {
            eprintln!("conch: subshell: {err}");
            1
        }
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

/// POSIX `if_clause`/`else_part` — see
/// [`conch_shell_parser::IfClause`]'s docs for how the AST already
/// flattens the `elif` chain, which is what keeps this a single loop
/// rather than needing recursion to mirror the grammar's own nesting.
fn exec_if(clause: &IfClause, shell: &mut Shell) -> i32 {
    for (condition, body) in &clause.branches {
        let cond_status = exec_command_list(condition, shell);
        if shell.pending_control_flow.is_some() {
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
        let cond_status = exec_command_list(condition, shell);
        if shell.pending_control_flow.is_some() {
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
///
/// Returns the exit status and, when `capture_output` was honored (no
/// output redirect overrode it), the captured stdout bytes.
fn exec_simple(
    cmd: &SimpleCommand,
    shell: &mut Shell,
    stdin_data: Option<Vec<u8>>,
    capture_output: bool,
) -> (i32, Option<Vec<u8>>) {
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
        // one command's environment.
        for (name, value) in assignments {
            shell.shell_vars.insert(name, value);
        }
        return (0, None);
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
        // Expanded to zero fields (e.g. an unset, unquoted parameter) —
        // nothing to run, matching real shells treating this as a no-op.
        return (0, None);
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
            capture_output,
        );
    }

    exec_external(
        &name,
        &args,
        shell,
        &assignments,
        stdin_data,
        &redirects,
        capture_output,
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
fn expand_assignments(
    cmd: &SimpleCommand,
    shell: &mut Shell,
) -> Result<Vec<(String, String)>, ExpandError> {
    cmd.assignments
        .iter()
        .map(|assignment| {
            expand_word_single(&assignment.value, shell)
                .map(|value| (assignment.name.clone(), value))
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
    if !shell.is_interactive && matches!(err, ExpandError::ParameterNullOrUnset(_)) {
        std::process::exit(1);
    }
}

fn exec_builtin(
    name: &str,
    shell: &mut Shell,
    args: &[String],
    assignments: &[(String, String)],
    redirects: &[ExpandedRedirect],
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
    let status = builtin.run(shell, args, &mut stdout_buf, &mut stderr_buf);
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

    if !stderr_buf.is_empty() {
        let _ = std::io::stderr().write_all(&stderr_buf);
    }

    if let Some(output_redirect) = redirects.iter().rev().find(|r| {
        r.fd == 1
            && matches!(
                r.operator,
                RedirectOperator::Output | RedirectOperator::Append
            )
    }) {
        if let Err(err) = write_redirect(output_redirect, &stdout_buf) {
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

fn exec_external(
    name: &str,
    args: &[String],
    shell: &Shell,
    assignments: &[(String, String)],
    stdin_data: Option<Vec<u8>>,
    redirects: &[ExpandedRedirect],
    capture_output: bool,
) -> (i32, Option<Vec<u8>>) {
    let mut effective_env: HashMap<String, String> = shell.env_vars.clone();
    for (name, value) in assignments {
        effective_env.insert(name.clone(), value.clone());
    }

    let mut command = std::process::Command::new(name);
    command
        .args(args)
        .current_dir(&shell.cwd)
        .env_clear()
        .envs(&effective_env);

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

    match input_redirect {
        Some(redirect) => match std::fs::File::open(&redirect.target) {
            Ok(file) => {
                command.stdin(file);
            }
            Err(err) => {
                eprintln!("conch: {}: {err}", redirect.target);
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
        Some(redirect) => {
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .append(matches!(redirect.operator, RedirectOperator::Append))
                .truncate(!matches!(redirect.operator, RedirectOperator::Append))
                .open(&redirect.target);
            match file {
                Ok(file) => {
                    command.stdout(file);
                }
                Err(err) => {
                    eprintln!("conch: {}: {err}", redirect.target);
                    return (1, None);
                }
            }
        }
        None => {
            command.stdout(if capture_output {
                Stdio::piped()
            } else {
                Stdio::inherit()
            });
        }
    }

    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(err) => {
            eprintln!("conch: {name}: {err}");
            return (127, None);
        }
    };

    if let (Some(data), Some(mut stdin)) = (stdin_data, child.stdin.take()) {
        let _ = stdin.write_all(&data);
        // Dropping `stdin` here closes the pipe, signaling EOF to the
        // child — required for commands that read to completion (`cat`,
        // `wc`, ...) rather than processing incrementally.
    }

    if output_redirect.is_none() && capture_output {
        match child.wait_with_output() {
            Ok(output) => (output.status.code().unwrap_or(1), Some(output.stdout)),
            Err(err) => {
                eprintln!("conch: {name}: {err}");
                (1, None)
            }
        }
    } else {
        match child.wait() {
            Ok(status) => (status.code().unwrap_or(1), None),
            Err(err) => {
                eprintln!("conch: {name}: {err}");
                (1, None)
            }
        }
    }
}

fn write_redirect(redirect: &ExpandedRedirect, data: &[u8]) -> std::io::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .append(matches!(redirect.operator, RedirectOperator::Append))
        .truncate(!matches!(redirect.operator, RedirectOperator::Append))
        .open(&redirect.target)?;
    file.write_all(data)
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
