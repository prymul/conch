//! Shell state, word expansion, and the command executor.

mod brace;
mod exec;
mod expand;
mod job;
mod signals;

pub use brace::brace_expand;
pub use exec::{exec_command_list, exec_program, resume_job_in_foreground};
pub use expand::{ExpandError, expand_word_fields, expand_word_single};
pub use job::{Job, JobState, JobTable};
pub use signals::TrapAction;

/// A `break [n]`/`continue [n]` (POSIX special builtins) in progress,
/// working its way back up through the executor to whichever enclosing
/// loop should catch it.
///
/// Builtins only ever get to communicate through their `i32` exit status
/// and whatever they write to `stdout`/`stderr` (see [`Builtin::run`]) —
/// there's no separate "and also do this control-flow thing" channel in
/// that trait, matching every other POSIX special builtin. So `break`/
/// `continue` (`conch-shell-builtins`) instead set
/// [`Shell::pending_control_flow`], and every loop/list-execution site in
/// `conch-shell-core`'s executor checks it after each command — see that
/// crate's `exec.rs` for the full propagation mechanism (each loop level
/// a signal passes through decrements its `n` by one, stopping there once
/// `n` reaches `1`).
///
/// Deliberately *not* a two-variant enum hardcoded to just these two
/// cases: a subshell-scoped `exit [n]` was an earlier candidate third
/// variant here, but doesn't belong — POSIX requires real fork semantics
/// for a subshell (`cd`/`exit`/every variable assignment inside must
/// never affect the parent), and `conch-shell-core::exec` gets that for
/// free by running a subshell as a genuine child process rather than
/// in-process (see [`crate::exec::exec_subshell`]'s docs), so `exit`
/// inside one already only terminates that child — no in-process signal
/// needed at all. [`ControlFlow::Return`] *does* need exactly this
/// shape, though: a function call is not a process boundary, so
/// unwinding out of one has to happen through this same kind of
/// pending, decrementing-per-level (well — not decrementing, for
/// `Return`; see its own docs) signal. This type is kept as its own
/// enum (not e.g. folded into a bool/two separate `Option` fields)
/// specifically so that variant was a small, natural extension rather
/// than a redesign.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlFlow {
    /// `break n` — stop the loop `n` levels up entirely (`n == 1` for
    /// the innermost enclosing loop).
    Break(u32),
    /// `continue n` — skip straight to the next iteration of the loop
    /// `n` levels up.
    Continue(u32),
    /// `return [n]` — stop the *function call* currently executing, with
    /// exit status `n`. Unlike `Break`/`Continue`, this never decrements
    /// as it propagates through nested loops/`if`/`case` bodies inside
    /// the function — there's only ever one function-call boundary to
    /// reach, not `n` levels of them — it just needs to keep unwinding,
    /// unchanged, until `conch-shell-core::exec`'s function-call
    /// execution path (the only thing that ever constructs this variant
    /// is the `return` builtin, `conch-shell-builtins`) catches it and
    /// converts it into that call's own return value. Confirmed against
    /// real bash that a `break`/`continue` still pending when a function
    /// call's own body finishes (i.e. one that named a level deeper than
    /// any loop lexically inside that function) does *not* escape to a
    /// loop merely *calling* the function — a function call is its own
    /// break/continue boundary too, exactly like the top-level program
    /// is (see [`crate::exec::exec_program`]'s docs) — so the function-
    /// call boundary discards+warns on a leftover `Break`/`Continue` the
    /// same way, rather than ever letting either escape past it.
    Return(i32),
}

use std::collections::HashMap;
use std::env;
use std::io::Write;
use std::path::PathBuf;

/// Mutable state for one shell session.
pub struct Shell {
    /// The shell's current working directory.
    pub cwd: PathBuf,
    /// Exported environment variables, visible to spawned child processes.
    pub env_vars: HashMap<String, String>,
    /// Shell-only variables (set but not exported).
    pub shell_vars: HashMap<String, String>,
    /// The exit status of the most recently run command (`$?`).
    pub last_status: i32,
    /// Whether this session is an interactive REPL rather than a `-c`
    /// string or script file. Defaults to `false`
    /// ([`Shell::new`]/[`Shell::default`]); the `conch` binary's
    /// interactive-mode entry point sets it explicitly. Affects only
    /// whether a POSIX-mandated *fatal* expansion error (`${var:?word}`
    /// on an unset parameter) exits the whole process or just reports a
    /// failure and moves on — see `conch-shell-core`'s `exec.rs`'s
    /// `report_expand_error` for the full rationale.
    pub is_interactive: bool,
    /// How many `while`/`until`/`for` loops currently enclose whatever's
    /// executing right now — incremented/decremented by the executor
    /// around each loop's body. A subshell runs as a genuinely separate
    /// process (see [`crate::exec::exec_subshell`]'s docs) with its own
    /// fresh `Shell`, so — unlike an earlier in-process design this
    /// field went through — nothing here needs to reset or isolate this
    /// counter at a subshell boundary; there's no shared state to
    /// isolate it *from*. The `break`/`continue` builtins
    /// (`conch-shell-builtins`) check this before setting
    /// [`Self::pending_control_flow`] at all, matching real bash
    /// printing "only meaningful in a `for', `while', or `until' loop"
    /// and otherwise doing nothing when used outside any loop.
    pub loop_depth: u32,
    /// See [`ControlFlow`]'s docs.
    pub pending_control_flow: Option<ControlFlow>,
    /// `$0` — confirmed against real bash: `-c '...'`/interactive mode
    /// uses the shell's own invocation name (`"bash"`), while script-file
    /// mode uses the script path exactly as given on the command line,
    /// regardless of the script's own arguments. Defaults to `"conch"`
    /// (`Shell::new`/`Shell::default`), matching the `-c`/interactive
    /// case; the `conch` binary's script-file entry point overwrites this
    /// with the script path. Never changed by a function call (confirmed
    /// against real bash: `$0` inside a function is still the *shell's*
    /// name/script path, not the function's own name) — contrast
    /// [`Shell::positional_params`], which a function call *does*
    /// temporarily replace.
    pub arg0: String,
    /// `$$` — POSIX: "the decimal value of the process number of the
    /// invoking shell," and — critically — confirmed against real bash
    /// this is the *top-level* shell's own PID, unchanged inside a
    /// subshell environment even though a subshell is a genuinely
    /// separate OS process here (see `exec::exec_subshell`'s docs);
    /// bash's own non-POSIX `$BASHPID` extension exists specifically to
    /// expose the *actual current* process's PID when that's what's
    /// wanted instead. Defaults to this process's own real PID
    /// ([`Shell::new`]); every one of this crate's re-exec sites
    /// (subshells, command substitution, a backgrounded job's wrapper —
    /// see `conch-shell-core::exec`'s module docs) propagates the *original* value
    /// through via the `__CONCH_PID` environment variable rather than
    /// letting each child default to its own new, different real PID —
    /// see [`Shell::new`]'s own docs for the receiving half of that.
    pub pid: nix::unistd::Pid,
    /// `$1`, `$2`, ... / `$@`/`$*`/`$#` (POSIX 2.5.2) — index `0` is `$1`.
    /// A function call temporarily replaces this with its own arguments
    /// for the duration of the call (confirmed against real bash:
    /// positional parameters are call-local, not shared with the
    /// caller's), restoring the caller's own afterward — see
    /// `conch-shell-core::exec`'s function-call execution path.
    pub positional_params: Vec<String>,
    /// Shell functions defined so far (`name() { ...; }`, or the bash
    /// `function name { ...; }` extension), keyed by name — POSIX 2.9.5 /
    /// bash manual §3.3. A later definition of the same name silently
    /// replaces the earlier one (confirmed against real bash: no error,
    /// no warning).
    pub functions: HashMap<String, conch_shell_parser::FunctionDefinition>,
    /// How many function calls are currently on the call stack — `return`
    /// (`conch-shell-builtins`) only makes sense with this `> 0` (POSIX:
    /// outside a function — or a sourced script, which conch doesn't
    /// implement yet — `return` is an error; confirmed against real bash
    /// this doesn't just silently no-op the way `break`/`continue` do
    /// outside a loop, unlike those it returns a genuine nonzero exit
    /// status). Incremented/decremented by
    /// `conch-shell-core::exec::exec_function_call`, exactly mirroring
    /// [`Self::loop_depth`]'s own convention.
    pub function_depth: u32,
    /// One frame per currently-active function call (pushed on entry,
    /// popped on return — `conch-shell-core::exec::exec_function_call`),
    /// each holding the `(name, previous binding)` pairs the `local`
    /// builtin (`conch-shell-builtins`) has shadowed *during that call*,
    /// in declaration order, restored in reverse once the call ends (see
    /// [`Shell::restore_var`]'s docs for why declaration-order LIFO
    /// restoration is correct even for two `local`s of the same name in
    /// one call).
    ///
    /// Deliberately *not* a separate "local scope" lookup layer
    /// ([`Self::get_var`]/[`Self::env_vars`]/[`Self::shell_vars`] stay
    /// completely unchanged, with no scope-chain lookup added to any of
    /// them) — `local` instead temporarily overwrites the *same*
    /// `env_vars`/`shell_vars` entry a plain assignment would, and this
    /// stack exists purely to remember what to put back once the
    /// declaring call returns. This is deliberately what makes an
    /// ordinary (non-`local`) assignment inside a function body correctly
    /// mutate whatever's currently live — the global, or an outer
    /// caller's own still-active `local`, whichever is closer — with zero
    /// extra code at every existing assignment site: confirmed against
    /// real bash `local` is *dynamically* scoped, not lexically (`X=g;
    /// outer() { local X=o; inner; }; inner() { X=set_by_inner; };
    /// outer` leaves the global `X` at `g`, unchanged, exactly as if
    /// `inner`'s assignment had targeted `outer`'s own `local X` — which,
    /// under this single-shared-map design, it structurally does, for
    /// free).
    local_stack: Vec<Vec<(String, PreviousVar)>>,
    builtins: HashMap<String, Box<dyn Builtin>>,
    /// Which builtins in [`Self::builtins`] are POSIX 2.9.1's "special"
    /// builtins (`break`, `continue`, `exit`, `export`, `return`, `set`,
    /// `shift`, ...) rather than "regular" ones (`cd`, `echo`, `pwd`,
    /// `local`, ...) — kept purely as a structural classification (see
    /// [`Self::is_special_builtin`]/[`Self::register_special_builtin`]),
    /// *not* currently used to stop a same-named function from shadowing
    /// one.
    ///
    /// POSIX 2.9.1's command search order actually puts special builtins
    /// *ahead* of functions specifically because 2.9.5 says a function
    /// must never be allowed to shadow one — an earlier version of
    /// `conch-shell-core::exec`'s command lookup enforced exactly that.
    /// But confirmed against real bash 5.3: outside `--posix` mode, bash
    /// actually lets a function shadow even a special builtin like
    /// `export`/`break` (`export() { echo fake; }; export FOO=bar` runs
    /// the fake function and never sets `FOO`; only `bash --posix`
    /// refuses, with "`export': is a special builtin"). Since this
    /// project already tracks bash's real default behavior over strict
    /// POSIX-only where the two diverge elsewhere (see e.g. the
    /// arithmetic module's own docs for the same policy), and the
    /// differential suite's primary oracle is real, non-`--posix` bash,
    /// a function is allowed to shadow *any* builtin — see
    /// `conch-shell-core::exec`'s command-lookup docs for exactly where
    /// this is decided.
    special_builtins: std::collections::HashSet<String>,

    // ---- Phase 4: job control -------------------------------------------
    /// Background/stopped jobs — see [`job`]'s module docs.
    pub job_table: JobTable,
    /// The process-group leader PID of whatever `&`-backgrounded job most
    /// recently started — `$!` (`conch-shell-core::expand`).
    pub last_background_pid: Option<nix::unistd::Pid>,
    /// Whether [`Self::init_job_control`] (`conch-shell-core::signals`)
    /// succeeded — `true` only for the one interactive session with a
    /// real controlling terminal; `false` for `-c`/script-file
    /// invocations, subshell/command-substitution re-exec'd children, and
    /// an interactive session run without a controlling terminal at all
    /// (e.g. under a test harness). Gates every terminal-ownership
    /// (`tcsetpgrp`) and process-group-creation call in
    /// `conch-shell-core::exec` — see that crate's module docs for why
    /// scoping job control to "interactive only" this way is a
    /// deliberate, bash-matching choice, not a shortcut.
    pub job_control_active: bool,
    /// The shell's own process group, once [`Self::init_job_control`]
    /// establishes it — what every foreground-job terminal handoff hands
    /// the terminal *back* to.
    pub shell_pgid: Option<nix::unistd::Pid>,
    /// Set when the foreground job most recently run didn't simply
    /// finish — either suspended (`SIGTSTP`) or killed by `SIGINT` — so
    /// every "runs a sequence of things" function in
    /// `conch-shell-core::exec` can unwind back to the prompt the same
    /// way a pending [`ControlFlow`] already does, without folding this
    /// into that enum itself (see [`ForegroundInterrupt`]'s own docs for
    /// why keeping it a separate, additive field was a deliberate call,
    /// not an oversight).
    pub foreground_interrupt: Option<ForegroundInterrupt>,
    /// Registered `trap` actions, keyed by signal — see
    /// [`crate::signals::TrapAction`] and [`Self::set_trap`].
    pub(crate) traps: HashMap<nix::sys::signal::Signal, TrapAction>,
    /// `trap 'cmd' EXIT`'s command text, if any — see
    /// [`Self::run_exit_trap`].
    pub(crate) exit_trap: Option<String>,
    /// Every signal this shell has installed its own real handler for
    /// (`SIGCHLD` always; anything else only once `trap`'d) — tracked so
    /// [`Self::prepare_child_for_job_control`] knows exactly which
    /// dispositions a spawned child must reset before it execs, rather
    /// than resetting every signal unconditionally (which would be
    /// needlessly broad and, worse, silently paper over a missing entry
    /// here instead of the reset list ever being wrong in an
    /// *observable* way).
    pub(crate) handled_signals: std::collections::HashSet<nix::sys::signal::Signal>,
    /// The self-pipe's read end — see `conch-shell-core::signals`'
    /// module docs.
    pub(crate) signal_pipe_read: Option<std::os::fd::OwnedFd>,
    /// The self-pipe's write end — kept alive here for the process's
    /// whole life; never read from again after
    /// [`Self::init_signal_handling`] publishes its raw fd for
    /// `handle_signal` to write to.
    pub(crate) signal_pipe_write: Option<std::os::fd::OwnedFd>,
}

/// See [`Shell::foreground_interrupt`]'s docs for why this is a separate
/// field/type rather than a new [`ControlFlow`] variant: `ControlFlow`'s
/// existing consumers (loop boundaries decrementing `break n`/`continue
/// n` by one level, a function call's own boundary consuming `return`)
/// have specific, level-aware interception semantics that a "the
/// foreground job stopped/was interrupted — abandon *everything* and
/// return to the prompt, unconditionally, skipping every one of those
/// boundaries" signal must never be subject to. Keeping it a wholly
/// separate, always-fully-propagating field means every existing
/// `ControlFlow` test/behavior in `conch-shell-core::exec` needed zero
/// changes — confirmed exactly right for this design, not merely
/// convenient, by the same reasoning that already keeps a subshell's
/// `exit`/`break`/`continue` isolation free of any special-casing in
/// that module (see `exec::exec_subshell`'s docs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForegroundInterrupt {
    /// The foreground job was suspended (`SIGTSTP`/`SIGTTIN`/`SIGTTOU`)
    /// and is now `Stopped` in [`Shell::job_table`] under this job id,
    /// resumable via `fg`/`bg`.
    Stopped(u32),
    /// The foreground job was killed by `SIGINT` specifically —
    /// confirmed against real bash this aborts the rest of the current
    /// command list/script the same way a stopped job does, rather than
    /// continuing to the next `;`-separated command or loop iteration
    /// the way an ordinary nonzero exit status would; no other
    /// terminating signal gets this treatment (an external command
    /// killed by, say, `SIGTERM` just reports its ordinary `128+n` exit
    /// status and execution continues normally).
    Interrupted,
}

/// What a variable's binding was, if anything, immediately before a
/// `local` declaration shadowed it — captured by [`Shell::capture_var`]
/// (used by the `local` builtin, `conch-shell-builtins`), restored by
/// [`Shell::restore_var`] (used by `conch-shell-core::exec`'s
/// function-call execution path once the declaring call returns).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviousVar {
    /// `name` wasn't bound in either [`Shell::env_vars`] or
    /// [`Shell::shell_vars`] at all before the `local` that shadowed it.
    Unset,
    /// `name` held this value in [`Shell::shell_vars`] (i.e. was a
    /// shell-only, non-exported variable).
    ShellVar(String),
    /// `name` held this value in [`Shell::env_vars`] (i.e. was exported).
    EnvVar(String),
}

impl Shell {
    /// Creates a new shell session, inheriting the current process's
    /// working directory and environment.
    pub fn new() -> Self {
        let mut env_vars: HashMap<String, String> = env::vars().collect();
        // `$$` (`Self::pid`) — see that field's own docs. `__CONCH_PID`
        // is this crate's own internal re-exec-propagation channel, not
        // a real user-facing variable, so it's removed from `env_vars`
        // here rather than left to leak into e.g. `export -p`'s
        // eventual output or a child process this shell itself spawns
        // (which should never see it — it's meaningful only as a
        // one-hop signal from a re-exec'ing parent to the exact child it
        // just spawned, consumed exactly once, right here). Carries only
        // a plain decimal PID, never interpreted as anything else,
        // deliberately unlike the `shell_vars`-via-env-var idea this
        // crate's own subshell docs already explain the (Shellshock-
        // shaped) reasons *not* to do for actual shell state/command
        // text.
        let pid = env_vars
            .remove("__CONCH_PID")
            .and_then(|value| value.parse::<i32>().ok())
            .map(nix::unistd::Pid::from_raw)
            .unwrap_or_else(nix::unistd::getpid);
        // See `Self::ignored_trap_names`'s docs — captured and stripped
        // here for the same "internal one-hop re-exec channel, not a
        // real user-facing variable" reason `__CONCH_PID` is; applied
        // below, once `shell` itself exists (needs `&mut self` — see
        // `Self::set_trap`).
        let inherited_ignored_signals = env_vars
            .remove("__CONCH_IGNORED_SIGNALS")
            .map(|value| Self::parse_ignored_trap_names(&value))
            .unwrap_or_default();

        let mut shell = Self {
            cwd: env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            env_vars,
            shell_vars: HashMap::new(),
            last_status: 0,
            is_interactive: false,
            loop_depth: 0,
            pending_control_flow: None,
            arg0: "conch".to_string(),
            pid,
            positional_params: Vec::new(),
            functions: HashMap::new(),
            function_depth: 0,
            local_stack: Vec::new(),
            builtins: HashMap::new(),
            special_builtins: std::collections::HashSet::new(),
            job_table: JobTable::new(),
            last_background_pid: None,
            job_control_active: false,
            shell_pgid: None,
            foreground_interrupt: None,
            traps: HashMap::new(),
            exit_trap: None,
            handled_signals: std::collections::HashSet::new(),
            signal_pipe_read: None,
            signal_pipe_write: None,
        };
        for signal in inherited_ignored_signals {
            shell.set_trap(Some(signal), signals::TrapAction::Ignore);
        }
        shell
    }

    /// Looks up a variable, preferring exported environment variables and
    /// falling back to shell-only variables.
    pub fn get_var(&self, name: &str) -> Option<&str> {
        self.env_vars
            .get(name)
            .or_else(|| self.shell_vars.get(name))
            .map(String::as_str)
    }

    /// Registers a *regular* builtin under `name` (POSIX 2.9.1's
    /// non-special category) — replacing any existing builtin with the
    /// same name. See [`Self::register_special_builtin`] for the other
    /// kind, and [`Self::special_builtins`]'s docs for why this
    /// distinction no longer affects whether a same-named function can
    /// shadow either one.
    pub fn register_builtin(&mut self, name: impl Into<String>, builtin: Box<dyn Builtin>) {
        self.builtins.insert(name.into(), builtin);
    }

    /// Registers a *special* builtin under `name` (POSIX 2.9.1's special
    /// category — `break`, `continue`, `exit`, `export`, `return`, `set`,
    /// `shift`, ...) — replacing any existing builtin with the same name.
    /// See [`Self::special_builtins`]'s docs for what this classification
    /// is (and isn't) used for.
    pub fn register_special_builtin(&mut self, name: impl Into<String>, builtin: Box<dyn Builtin>) {
        let name = name.into();
        self.special_builtins.insert(name.clone());
        self.builtins.insert(name, builtin);
    }

    /// Whether `name` was registered via [`Self::register_special_builtin`]
    /// rather than [`Self::register_builtin`] — see
    /// [`Self::special_builtins`]'s docs.
    pub fn is_special_builtin(&self, name: &str) -> bool {
        self.special_builtins.contains(name)
    }

    /// Returns the builtin registered under `name`, if any (special or
    /// regular alike — see [`Self::is_special_builtin`] to distinguish).
    pub fn builtin(&self, name: &str) -> Option<&dyn Builtin> {
        self.builtins.get(name).map(AsRef::as_ref)
    }

    /// Removes and returns the builtin registered under `name`, if any.
    ///
    /// Exists so callers can run a builtin with a `&mut Shell` in hand
    /// without a self-referential borrow: take it out, call it, put it
    /// back with [`Shell::register_builtin`].
    pub fn take_builtin(&mut self, name: &str) -> Option<Box<dyn Builtin>> {
        self.builtins.remove(name)
    }

    /// Captures `name`'s current binding, for a `local` declaration
    /// (`conch-shell-builtins`) to later hand to [`Self::restore_var`]
    /// once the declaring function call returns. See
    /// [`Shell::local_stack`]'s docs for the full mechanism.
    pub fn capture_var(&self, name: &str) -> PreviousVar {
        if let Some(value) = self.env_vars.get(name) {
            PreviousVar::EnvVar(value.clone())
        } else if let Some(value) = self.shell_vars.get(name) {
            PreviousVar::ShellVar(value.clone())
        } else {
            PreviousVar::Unset
        }
    }

    /// Restores a binding previously captured by [`Self::capture_var`],
    /// overwriting whatever `name` is currently bound to (if anything) in
    /// either map.
    pub fn restore_var(&mut self, name: &str, previous: PreviousVar) {
        self.env_vars.remove(name);
        self.shell_vars.remove(name);
        match previous {
            PreviousVar::Unset => {}
            PreviousVar::ShellVar(value) => {
                self.shell_vars.insert(name.to_string(), value);
            }
            PreviousVar::EnvVar(value) => {
                self.env_vars.insert(name.to_string(), value);
            }
        }
    }

    /// Applies a `local NAME[=value]` declaration's *new* binding — the
    /// capture/restore-list bookkeeping is the caller's job (the `local`
    /// builtin, `conch-shell-builtins`: capture via [`Self::capture_var`]
    /// first, push it onto the current call's [`Self::push_local_frame`]
    /// frame, *then* call this). `value: None` is a bare `local NAME` (no
    /// `=`) — confirmed against real bash this makes the name genuinely
    /// *unset* within the new scope (`${NAME+is-set}` is empty), not
    /// merely set to an empty string, so this removes `name` from both
    /// maps rather than inserting `""`. `value: Some(v)` writes `v` into
    /// whichever of [`Self::env_vars`]/[`Self::shell_vars`] `name`
    /// already lived in — so an already-exported variable stays exported
    /// once shadowed (confirmed against real bash: `export X=1; f() {
    /// local X=2; ...; }` still shows `X=2` in a child process's
    /// inherited environment) — defaulting to `shell_vars` (not exported)
    /// for a name with no prior binding at all, matching bash's own
    /// default for a brand new `local`.
    pub fn set_local(&mut self, name: &str, value: Option<String>) {
        let was_exported = self.env_vars.contains_key(name);
        self.env_vars.remove(name);
        self.shell_vars.remove(name);
        if let Some(value) = value {
            if was_exported {
                self.env_vars.insert(name.to_string(), value);
            } else {
                self.shell_vars.insert(name.to_string(), value);
            }
        }
    }

    /// Pushes a fresh, empty `local` restore-list frame for a function
    /// call that's just starting — see [`Shell::local_stack`]'s docs.
    pub fn push_local_frame(&mut self) {
        self.local_stack.push(Vec::new());
    }

    /// Records that `name`'s binding was `previous` immediately before
    /// the *current* (innermost active) function call's `local`
    /// declaration overwrote it — appends to the frame
    /// [`Self::push_local_frame`] most recently pushed. Does nothing if
    /// called with no function call active (the `local` builtin checks
    /// for that itself and never calls this in that case — see
    /// [`Self::in_function_call`]).
    pub fn record_local(&mut self, name: String, previous: PreviousVar) {
        if let Some(frame) = self.local_stack.last_mut() {
            frame.push((name, previous));
        }
    }

    /// Whether a `local` declaration is currently valid (POSIX/bash:
    /// `local` — like `return` — is only meaningful inside a function
    /// call).
    pub fn in_function_call(&self) -> bool {
        !self.local_stack.is_empty()
    }

    /// Pops the innermost function call's `local` restore-list frame and
    /// restores every binding it shadowed, in reverse (LIFO) declaration
    /// order — called once that call's body finishes running, regardless
    /// of how it ended (normal completion or `return`). Reverse order is
    /// what makes two `local`s of the *same* name within one call restore
    /// correctly: each restore only ever needs to undo the most recent
    /// shadow, one step at a time, which — applied repeatedly — correctly
    /// unwinds all the way back to the true pre-call binding no matter
    /// how many times that call re-shadowed the same name.
    pub fn pop_local_frame(&mut self) {
        if let Some(frame) = self.local_stack.pop() {
            for (name, previous) in frame.into_iter().rev() {
                self.restore_var(&name, previous);
            }
        }
    }
}

impl Default for Shell {
    fn default() -> Self {
        Self::new()
    }
}

/// A command conch runs in-process instead of spawning an external
/// program — `cd`, `exit`, `export`, and similar commands that must
/// mutate the shell's own state.
///
/// `stdout`/`stderr` are passed explicitly (rather than the builtin
/// writing to the real process streams directly) so that redirecting a
/// builtin's output (`echo hi > file`) works the same way it does for an
/// external command, and so builtins are testable against an in-memory
/// buffer instead of asserting on real stdout.
pub trait Builtin {
    /// Runs the builtin with the given arguments (not including the
    /// builtin's own name) and returns its exit status.
    fn run(
        &self,
        shell: &mut Shell,
        args: &[String],
        stdout: &mut dyn Write,
        stderr: &mut dyn Write,
    ) -> i32;
}

#[cfg(test)]
mod tests {
    use super::*;

    struct AlwaysZero;
    impl Builtin for AlwaysZero {
        fn run(
            &self,
            _shell: &mut Shell,
            _args: &[String],
            _stdout: &mut dyn Write,
            _stderr: &mut dyn Write,
        ) -> i32 {
            0
        }
    }

    #[test]
    fn register_and_look_up_builtin() {
        let mut shell = Shell::new();
        shell.register_builtin("noop", Box::new(AlwaysZero));
        assert!(shell.builtin("noop").is_some());
        assert!(shell.builtin("missing").is_none());
    }

    #[test]
    fn env_var_takes_precedence_over_shell_var() {
        let mut shell = Shell::new();
        shell
            .shell_vars
            .insert("FOO".to_string(), "shell".to_string());
        shell.env_vars.insert("FOO".to_string(), "env".to_string());
        assert_eq!(shell.get_var("FOO"), Some("env"));
    }
}
