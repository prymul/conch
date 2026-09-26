//! Signal handling: the shell's own disposition table, deferred
//! `SIGCHLD`/trap processing, and getting/giving back the controlling
//! terminal — the GNU libc manual's "Job Control" chapter (specifically
//! "Initializing the Shell" and "Foreground and Background Processes")
//! is the grounding for every disposition/ordering choice below; deviate
//! from it and get it right by inspection, not folk knowledge.
//!
//! # Why the shell ignores `SIGINT`/`SIGQUIT`/`SIGTSTP`/`SIGTTIN`/`SIGTTOU`
//! rather than "catching" them
//!
//! The instinct to install a real handler for `SIGINT`/`SIGTSTP` so the
//! shell can "notice" a foreground job being interrupted/stopped is
//! backwards for a *correctly pgrp-separated* job-control shell: once
//! every job has its own process group and the terminal's foreground
//! pgrp is correctly handed to it before it runs (see
//! `conch-shell-core::exec`'s `run_foreground_job`), the
//! kernel only ever delivers a terminal-generated `SIGINT`/`SIGTSTP` to
//! *that job's* process group, never to the shell's own — the shell
//! simply never receives them while a job is in the foreground, and
//! learns what happened the ordinary way, via the `waitpid` result of
//! blocking on that job. So the shell's own disposition for these five
//! only matters for what happens *at the prompt* (no foreground job:
//! the shell itself is the terminal's foreground pgrp) — and the
//! GNU libc manual's answer there is exactly `SIG_IGN`, not a handler:
//! an idle interactive shell must survive `^C`/`^\`/`^Z` from its own
//! terminal, full stop, not "catch and do something with" them.
//!
//! [`Shell::set_trap`] is the one thing that changes this per-signal:
//! `trap 'cmds' INT` explicitly asks the shell itself to notice `SIGINT`
//! even while otherwise idle, which switches that *one* signal's
//! disposition from `SIG_IGN` to the shared deferred-dispatch handler —
//! see that function's docs.
//!
//! # Why `SIGCHLD` is the one signal always given a real handler
//!
//! Everything else in this module exists to keep [`Shell::job_table`]
//! ([`crate::job`]) accurate without blocking anything: a real (if
//! minimal) `SIGCHLD` handler is what lets a blocked foreground
//! `waitpid` — or, when idle, the interactive prompt loop itself — wake
//! up promptly when an *unrelated* background job changes state, rather
//! than only ever finding out the next time something else happens to
//! poll.
//!
//! # The deferred-dispatch design (security-reviewed)
//!
//! A signal handler can run at any point, so it may only call
//! async-signal-safe operations — see `nix::sys::signal`'s own
//! documentation and `signal-safety(7)`. Concretely, that rules out
//! *any* real work happening inside one: no allocation (a `HashMap`
//! lookup, `String` construction, or the parser/executor themselves all
//! allocate), no non-reentrant libc calls. So [`handle_signal`], the one
//! `extern "C" fn` every signal this module ever "catches" (as opposed
//! to flatly `SIG_IGN`s) is installed with, does exactly two things,
//! both async-signal-safe: sets a bit in [`PENDING_SIGNALS`] (a lock-free
//! atomic — no allocation, no libc call at all), and makes a single
//! best-effort, non-blocking `write` of one byte to a self-pipe.
//!
//! The self-pipe's only job is waking up a *blocked* `read`/`waitpid`
//! elsewhere in the process (a plain atomic flag alone can't do that —
//! nothing would ever notice it changed until the next time some
//! unrelated code happened to poll it). Both ends are set `O_NONBLOCK`
//! at creation specifically so the handler's write can never block: a
//! burst of `SIGCHLD`s (e.g. several background jobs exiting at once)
//! filling the pipe faster than it's drained is normal, not exceptional,
//! and the write is expected to sometimes fail with `EWOULDBLOCK` and be
//! silently dropped. This is safe *by construction*, not by luck: every
//! reader ([`Shell::process_pending_signals`]) fully drains the pipe and
//! then fully re-scans [`PENDING_SIGNALS`] (which is authoritative — the
//! pipe is only ever a wake-up nudge, never itself the record of what
//! happened) on every wake, so a coalesced-into-one-byte or entirely
//! dropped wake-up write can never cause a real signal to be silently
//! missed — only, at worst, processed one poll later than it could have
//! been, which is already true of ordinary signal coalescing (two
//! `SIGCHLD`s that arrive before the handler runs for the first one
//! collapse into a single pending notification at the OS level too).
//!
//! No actual trap *execution* (parsing `trap`'s stored command text,
//! mutating [`Shell`], running builtins/external commands) ever happens
//! from inside [`handle_signal`] — [`Shell::process_pending_signals`]
//! does that, called only from ordinary control flow at safe checkpoints
//! (`conch-shell-core::exec`'s command-list loop, and the interactive
//! prompt loop), reusing the exact same "checked between commands"
//! deferred-signal shape [`crate::ControlFlow`] already established for
//! `break`/`continue`/`return` in Phase 3.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd};
use std::sync::atomic::{AtomicI32, AtomicU32, Ordering};

use nix::errno::Errno;
use nix::libc::c_int;
use nix::sys::signal::{SaFlags, SigAction, SigHandler, SigSet, Signal, killpg, sigaction};
use nix::unistd::{Pid, getpgrp, getpid, pipe, setpgid, tcgetpgrp, tcsetpgrp};

use crate::Shell;

/// The five signals the shell ignores for *itself* while interactive —
/// see the module docs' "why ignore, not catch" section. Installed in
/// this exact order (`SIGTTOU`/`SIGTTIN` ignored *before* the initial
/// [`Shell::init_job_control`] ever calls `tcsetpgrp`) because attempting
/// a terminal-control operation from a background process group would
/// otherwise raise `SIGTTOU` against the shell itself mid-startup.
const IGNORED_AT_TOP_LEVEL: [Signal; 5] = [
    Signal::SIGINT,
    Signal::SIGQUIT,
    Signal::SIGTSTP,
    Signal::SIGTTIN,
    Signal::SIGTTOU,
];

/// The self-pipe's write end — a raw fd rather than an owned handle
/// because [`handle_signal`] (a bare `extern "C" fn`, no captured state)
/// can only reach it through a `static`. `-1` means "not yet
/// initialized" (before [`Shell::init_signal_handling`] runs, or in a
/// process — like a `cargo test` binary — that never calls it at all);
/// [`handle_signal`] checks for that and no-ops the write in that case.
static SELF_PIPE_WRITE: AtomicI32 = AtomicI32::new(-1);

/// One bit per signal number, set by [`handle_signal`] and drained by
/// [`Shell::process_pending_signals`] — see the module docs. A signal
/// number is always well under 32 for everything this shell ever
/// installs a real handler for (the highest standard signal number on
/// every platform `nix` targets is in the low 30s, and this crate never
/// traps a realtime signal), so a fixed-width bitmask needs no
/// allocation and thus no async-signal-safety concern of its own.
static PENDING_SIGNALS: AtomicU32 = AtomicU32::new(0);

/// The shared handler every "caught" (as opposed to flatly `SIG_IGN`'d)
/// signal in this module is installed with. See the module docs for the
/// full async-signal-safety reasoning; every operation below is on that
/// list (`signal-safety(7)`): an atomic fetch-or, and a single
/// non-blocking `write(2)`.
extern "C" fn handle_signal(signum: c_int) {
    if let Ok(bit) = u32::try_from(signum)
        && bit < 32
    {
        PENDING_SIGNALS.fetch_or(1 << bit, Ordering::SeqCst);
    }
    let write_fd = SELF_PIPE_WRITE.load(Ordering::SeqCst);
    if write_fd >= 0 {
        // SAFETY: `write_fd` was published by `init_signal_handling`
        // from a real, still-open `OwnedFd` it keeps alive for the rest
        // of the process's life (see `Shell::signal_pipe_write`) — valid
        // for the whole time any handler using this static could run.
        let fd = unsafe { BorrowedFd::borrow_raw(write_fd) };
        // Best-effort, non-blocking, tolerates a short write or EAGAIN —
        // see the module docs for why a dropped wake-up byte is
        // harmless by construction.
        let _ = nix::unistd::write(fd, &[0u8]);
    }
}

/// What a trapped signal (or `EXIT`) should do when
/// [`Shell::process_pending_signals`]/[`Shell::run_exit_trap`] reaches it
/// — POSIX `trap`'s three forms (`trap CMD SIG`, `trap SIG` alone is a
/// shell-specific extension some shells treat as `-`; conch, like bash,
/// requires an explicit `trap - SIG`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrapAction {
    /// `trap - SIG` (or never trapped at all): the signal's original,
    /// pre-`trap` disposition — see [`Shell::set_trap`]'s docs for what
    /// that means concretely per signal.
    Default,
    /// `trap '' SIG`: ignore the signal — POSIX-distinct from `Default`
    /// because, unlike an ordinary trapped command, this disposition
    /// *is* inherited across `exec` by child processes (matching real
    /// bash/`nohup`-style usage) — see
    /// [`Shell::prepare_child_for_job_control`]'s docs.
    Ignore,
    /// `trap 'command text' SIG`: run this shell source text (parsed and
    /// executed fresh each time, exactly like any other input) the next
    /// time [`Shell::process_pending_signals`]/[`Shell::run_exit_trap`]
    /// observes this signal/condition pending — never from inside
    /// [`handle_signal`] itself (see the module docs).
    Command(String),
}

impl Shell {
    /// Sets up the self-pipe and installs [`handle_signal`] for
    /// `SIGCHLD` — the one signal this crate *always* wants a real
    /// handler for, interactive or not (POSIX `trap`/background jobs are
    /// meaningful in a script too, not just interactively — see the
    /// module docs' "why SIGCHLD" section). Idempotent-ish in the sense
    /// that calling it twice just re-does the same setup; every entry
    /// point (`-c`, script-file, interactive) calls this exactly once,
    /// early, regardless of whether [`Self::init_job_control`] (terminal
    /// ownership, interactive-only) is ever also called.
    ///
    /// Failure (pipe creation, `fcntl`, or `sigaction` itself failing) is
    /// reported but not fatal to starting the shell at all — worst case,
    /// `SIGCHLD`-driven reaping degrades to "only ever happens when
    /// something else polls it," which every foreground-job wait already
    /// does regardless (a blocked `waitpid` on a *specific* pid doesn't
    /// need the self-pipe to wake up correctly for *that* job; only
    /// cross-job notifications lose their "reasonably prompt" property).
    pub fn init_signal_handling(&mut self) {
        match pipe() {
            Ok((read, write)) => {
                if let Err(err) = set_nonblocking(&read).and_then(|()| set_nonblocking(&write)) {
                    eprintln!("conch: warning: signal self-pipe setup failed: {err}");
                }
                // The write end's raw fd is published to `handle_signal`
                // via the static; the `OwnedFd` itself is kept alive in
                // `self.signal_pipe_write` for the rest of the process's
                // life (never read from directly — only `write`s from
                // inside the handler ever touch it again), so the raw fd
                // published above stays valid for exactly as long as any
                // handler installed against it could possibly run.
                SELF_PIPE_WRITE.store(write.as_raw_fd(), Ordering::SeqCst);
                self.signal_pipe_read = Some(read);
                self.signal_pipe_write = Some(write);
            }
            Err(err) => {
                eprintln!("conch: warning: signal self-pipe setup failed: {err}");
            }
        }
        self.install_handler(Signal::SIGCHLD);
    }

    /// Installs [`handle_signal`] for `signal`, tracking that it's
    /// installed (see [`Self::handled_signals`]) so
    /// [`Self::prepare_child_for_job_control`] knows to reset it back to
    /// `SIG_DFL` in a spawned child (a child must never inherit the
    /// *shell's* deferred-dispatch handler — see that function's docs).
    /// Safe to call more than once for the same signal (re-installs the
    /// same handler, a no-op in effect).
    fn install_handler(&mut self, signal: Signal) {
        let action = SigAction::new(
            SigHandler::Handler(handle_signal),
            SaFlags::SA_RESTART,
            SigSet::empty(),
        );
        // SAFETY: `handle_signal` only performs async-signal-safe
        // operations (see the module docs) and is a plain `extern "C"
        // fn` with `'static` lifetime — the two preconditions
        // `nix::sys::signal::sigaction`'s own docs call out.
        match unsafe { sigaction(signal, &action) } {
            Ok(_) => {
                self.handled_signals.insert(signal);
            }
            Err(err) => {
                eprintln!("conch: warning: installing a handler for {signal}: {err}");
            }
        }
    }

    /// Puts the shell in its own process group and claims the
    /// controlling terminal, following the GNU libc manual's
    /// "Initializing the Shell" sequence exactly (ignore the five
    /// terminal-generated signals *first*, so the terminal-control calls
    /// below can't raise `SIGTTOU` against the shell itself; then loop
    /// sending itself `SIGTTIN` until it's the terminal's foreground
    /// process group, for the case where it was started in the
    /// background under another job-control shell; then `setpgid`+
    /// `tcsetpgrp`, both redundant with what's likely already true for
    /// an ordinary directly-launched interactive shell, but required for
    /// correctness in the general case).
    ///
    /// Only ever called from the `conch` binary's interactive entry
    /// point — `-c`/script-file invocations have no controlling terminal
    /// to claim and no business claiming one. Returns `false` (and
    /// leaves [`Self::job_control_active`] `false`) if stdin isn't a
    /// terminal at all (`tcgetpgrp` fails with `ENOTTY`) — e.g. under a
    /// test harness or with stdin redirected — matching real bash
    /// disabling job control the same way rather than erroring.
    #[must_use]
    pub fn init_job_control(&mut self) -> bool {
        for signal in IGNORED_AT_TOP_LEVEL {
            // SAFETY: `SigHandler::SigIgn` performs no work at all when
            // "invoked" (the kernel just drops the signal) — trivially
            // satisfies `sigaction`'s async-signal-safety precondition.
            if let Err(err) = unsafe {
                sigaction(
                    signal,
                    &SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty()),
                )
            } {
                eprintln!("conch: warning: ignoring {signal}: {err}");
            }
        }

        let stdin = std::io::stdin();
        let Ok(mut foreground) = tcgetpgrp(&stdin) else {
            // Not a controlling terminal at all (ENOTTY) -- job control
            // stays off, matching bash.
            return false;
        };

        let shell_pid = getpid();
        // Loop sending ourselves SIGTTIN until we're the terminal's
        // foreground process group -- the exact GNU libc manual pattern
        // for a shell that might have been launched in the background
        // by another job-control shell. SIGTTIN is ignored (just set
        // above), so this suspends the *group*, not this call; once
        // whatever eventually foregrounds it (another shell's `fg`)
        // sends SIGCONT, execution resumes here and the loop re-checks.
        while foreground != getpgrp() {
            let _ = killpg(getpgrp(), Signal::SIGTTIN);
            let Ok(pgrp) = tcgetpgrp(&stdin) else {
                return false;
            };
            foreground = pgrp;
        }

        // Only actually call `setpgid` if it would change anything:
        // POSIX explicitly permits an implementation to reject
        // `setpgid` outright (`EPERM`) when the calling process is
        // already a *session leader* — which it already will be in the
        // extremely common case of a shell launched directly inside a
        // freshly created pty (confirmed empirically, not assumed: a
        // real pty-backed interactive session — `pty.fork()` in Python,
        // and equally the shape every real terminal emulator sets up —
        // already makes the shell both its own session leader *and* its
        // own process group leader before it ever runs a line of shell
        // code, so `setpgid(0, 0)` here would be asking to move to
        // exactly the group it's already in, which some platforms still
        // refuse outright for a session leader regardless of whether
        // the *target* group is a no-op). Skipping the call entirely
        // when `getpgrp() == shell_pid` avoids a spurious "conch:
        // warning: setpgid: EPERM" on what is, in practice, the
        // *typical* interactive startup shape, not an edge case.
        if getpgrp() != shell_pid
            && let Err(err) = setpgid(Pid::from_raw(0), Pid::from_raw(0))
        {
            eprintln!("conch: warning: setpgid: {err}");
        }
        let shell_pgid = getpgrp();
        if let Err(err) = tcsetpgrp(&stdin, shell_pgid) {
            eprintln!("conch: warning: tcsetpgrp: {err}");
            return false;
        }

        self.shell_pgid = Some(shell_pgid);
        debug_assert_eq!(shell_pgid, shell_pid, "setpgid(0,0) makes pid == pgid");
        self.job_control_active = true;
        true
    }

    /// Drains the self-pipe and [`PENDING_SIGNALS`] and acts on whatever
    /// was pending — reaps children ([`Shell::reap_children`]) *only* if
    /// `SIGCHLD` was actually among the pending bits, and runs any
    /// [`TrapAction::Command`] registered for any other signal that
    /// fired. Safe (and cheap) to call speculatively and often; see the
    /// module docs for why draining the pipe itself is unconditional
    /// rather than conditioned on "did it actually have a byte."
    ///
    /// The `SIGCHLD`-gating here is a plain, uncontroversial efficiency
    /// win (skip a pointless sweep when nothing changed) rather than a
    /// safety-load-bearing one: an earlier version of this doc comment
    /// leaned on it to *also* avoid a cross-child reaping race in this
    /// crate's own multi-threaded `cargo test` binary (unrelated tests
    /// spawning real children on different threads of one process, which
    /// share that process's child-PID namespace) — a real, observed
    /// flake at the time, caught by this test suite itself going red.
    /// That race is now closed *structurally*, one layer down, in
    /// [`Shell::reap_children`] itself (scoped to specific tracked job
    /// leaders rather than a process-wide `waitpid(-1, ...)` sweep, per
    /// a later security review — see that function's own docs for the
    /// full reasoning), which means this function no longer depends on
    /// the `SIGCHLD`-gate for correctness at all, only for avoiding
    /// needless work.
    ///
    /// Returns the ids of jobs whose state changed (see
    /// [`Shell::reap_children`]), for a caller to print notifications
    /// for.
    pub fn process_pending_signals(&mut self) -> Vec<u32> {
        if let Some(read) = &self.signal_pipe_read {
            let mut buf = [0u8; 64];
            // Correctness: the fd is O_NONBLOCK (set at creation in
            // `init_signal_handling`), so this loop terminates on
            // `EAGAIN` rather than ever hanging. Reads via
            // `nix::unistd::read` directly (rather than wrapping in a
            // `std::fs::File`) so nothing ever takes ownership of/closes
            // this fd out from under `self.signal_pipe_read`, which
            // needs to keep owning it for the process's whole life.
            loop {
                match nix::unistd::read(read.as_fd(), &mut buf) {
                    Ok(0) => break,
                    Ok(_) => continue,
                    Err(Errno::EAGAIN) => break,
                    Err(Errno::EINTR) => continue,
                    Err(_) => break,
                }
            }
        }

        let pending = PENDING_SIGNALS.swap(0, Ordering::SeqCst);
        let sigchld_pending = u32::try_from(Signal::SIGCHLD as i32)
            .is_ok_and(|bit| bit < 32 && (pending >> bit) & 1 != 0);
        let changed = if sigchld_pending {
            self.reap_children()
        } else {
            Vec::new()
        };

        for signal in self.handled_signals.clone() {
            if signal == Signal::SIGCHLD {
                continue; // already handled via reap_children above.
            }
            let Ok(bit) = u32::try_from(signal as i32) else {
                continue;
            };
            if bit >= 32 || (pending >> bit) & 1 == 0 {
                continue;
            }
            if let Some(TrapAction::Command(source)) = self.traps.get(&signal).cloned() {
                self.run_trap_source(&source);
            }
        }

        changed
    }

    /// `trap 'command' SIG` / `trap - SIG` / `trap '' SIG` — see
    /// [`TrapAction`]. `signal: None` means `EXIT` (POSIX's one
    /// non-signal "condition" `trap` accepts) — see
    /// [`Self::run_exit_trap`].
    ///
    /// Switches `signal`'s live disposition immediately when needed: a
    /// fresh `Command`/`Ignore` trap on a signal this shell wasn't
    /// already catching installs [`handle_signal`] for it (so it's
    /// actually noticed — see the module docs' "why ignore, not catch"
    /// section for why that's *not* already the case for most signals);
    /// `Default` restores whatever [`Self::init_job_control`] originally
    /// set (`SIG_IGN` for the five terminal-generated signals, the real
    /// handler for `SIGCHLD`, `SIG_DFL` for anything else); `Ignore`
    /// installs `SIG_IGN` directly rather than going through
    /// [`Self::install_handler`] at all — there's nothing to defer, the
    /// kernel already does the ignoring.
    pub fn set_trap(&mut self, signal: Option<Signal>, action: TrapAction) {
        let Some(signal) = signal else {
            match action {
                TrapAction::Command(source) => self.exit_trap = Some(source),
                TrapAction::Default | TrapAction::Ignore => self.exit_trap = None,
            }
            return;
        };

        match &action {
            TrapAction::Command(_) => self.install_handler(signal),
            TrapAction::Ignore => {
                if let Err(err) = unsafe {
                    sigaction(
                        signal,
                        &SigAction::new(SigHandler::SigIgn, SaFlags::empty(), SigSet::empty()),
                    )
                } {
                    eprintln!("conch: trap: {signal}: {err}");
                }
                self.handled_signals.remove(&signal);
            }
            TrapAction::Default => {
                let restored = if signal == Signal::SIGCHLD {
                    self.install_handler(signal);
                    return;
                } else if IGNORED_AT_TOP_LEVEL.contains(&signal) && self.job_control_active {
                    SigHandler::SigIgn
                } else {
                    SigHandler::SigDfl
                };
                if let Err(err) = unsafe {
                    sigaction(
                        signal,
                        &SigAction::new(restored, SaFlags::empty(), SigSet::empty()),
                    )
                } {
                    eprintln!("conch: trap: {signal}: {err}");
                }
                self.handled_signals.remove(&signal);
            }
        }

        if matches!(action, TrapAction::Default) {
            self.traps.remove(&signal);
        } else {
            self.traps.insert(signal, action);
        }
    }

    /// The trap currently registered for `signal` (or `None` for `EXIT`)
    /// — `trap -p`'s listing needs this; `Default` (nothing explicitly
    /// trapped) is represented by absence rather than an explicit table
    /// entry, matching [`Self::set_trap`]'s own bookkeeping.
    #[must_use]
    pub fn trap_for(&self, signal: Option<Signal>) -> Option<&TrapAction> {
        match signal {
            None => None, // see run_exit_trap/exit_trap instead.
            Some(signal) => self.traps.get(&signal),
        }
    }

    pub fn traps_iter(&self) -> impl Iterator<Item = (Signal, &TrapAction)> {
        self.traps.iter().map(|(sig, action)| (*sig, action))
    }

    /// The comma-joined names of every signal currently `trap
    /// ''`-ignored ([`TrapAction::Ignore`]) — POSIX 2.9.3.1: unlike a
    /// *caught* trap ([`TrapAction::Command`]), an *ignored* one is
    /// inherited into a subshell/async-list/command-substitution
    /// environment. `conch-shell-core::exec`'s three re-exec sites
    /// propagate this via the `__CONCH_IGNORED_SIGNALS` environment
    /// variable, read back by [`Shell::new`] — safe to propagate this
    /// way specifically *because* it's an inert list of signal names,
    /// carrying no command text at all to (mis-)interpret, unlike a
    /// `Command` trap (see [`Shell::new`]'s own docs for why *that* one
    /// is deliberately never propagated this way).
    #[must_use]
    pub fn ignored_trap_names(&self) -> String {
        self.traps
            .iter()
            .filter(|(_, action)| matches!(action, TrapAction::Ignore))
            .map(|(signal, _)| signal.as_ref())
            .collect::<Vec<_>>()
            .join(",")
    }

    /// The receiving half of [`Self::ignored_trap_names`] — parses its
    /// comma-joined output back into real [`Signal`]s, silently skipping
    /// anything unparseable rather than erroring (this is internal
    /// re-exec plumbing between two versions of this same binary, not
    /// user input to validate strictly).
    pub(crate) fn parse_ignored_trap_names(value: &str) -> Vec<Signal> {
        value
            .split(',')
            .filter(|name| !name.is_empty())
            .filter_map(|name| parse_signal_spec(name).ok().flatten())
            .collect()
    }

    #[must_use]
    pub fn exit_trap(&self) -> Option<&str> {
        self.exit_trap.as_deref()
    }

    /// The string-based entry point the `trap` builtin
    /// (`conch-shell-builtins`) uses instead of [`Self::set_trap`] +
    /// [`Signal`] directly — that crate deliberately doesn't depend on
    /// `nix` at all (keeping every OS-signal *type* contained to this
    /// crate, per this phase's design; [`TrapAction`] itself has no
    /// `Signal` in it anywhere, so it's fine for that crate to construct
    /// one directly), so it only ever needs to hand this function a raw
    /// `trap` argument (`"INT"`, `"SIGINT"`, `"2"`, or `"EXIT"`).
    ///
    /// # Errors
    ///
    /// A bash-shaped message if `spec` isn't a recognized signal
    /// name/number or `EXIT`.
    pub fn trap_set_by_name(&mut self, spec: &str, action: TrapAction) -> Result<(), String> {
        let signal = parse_signal_spec(spec)?;
        self.set_trap(signal, action);
        Ok(())
    }

    /// `trap -p spec` — `spec`'s currently registered trap, formatted as
    /// `trap -- 'command text' SIGNAME` (bash's own `trap -p` output
    /// shape; POSIX doesn't mandate an exact format, only that it be
    /// suitable for re-input), or `None` if nothing's explicitly trapped
    /// there (`TrapAction::Default`).
    ///
    /// # Errors
    ///
    /// See [`Self::trap_set_by_name`].
    pub fn trap_describe_by_name(&self, spec: &str) -> Result<Option<String>, String> {
        let signal = parse_signal_spec(spec)?;
        let action = match signal {
            None => self.exit_trap.clone().map(TrapAction::Command),
            Some(signal) => self.traps.get(&signal).cloned(),
        };
        let name = signal.map_or_else(|| "EXIT".to_string(), |s| s.to_string());
        Ok(action.map(|action| format_trap_line(&name, &action)))
    }

    /// A bare `trap` (no arguments) — every currently registered trap,
    /// one already-formatted [`Self::trap_describe_by_name`]-shaped line
    /// per entry (`EXIT` last, matching bash's own ordering, for no
    /// deeper reason than it's a reasonable, stable convention).
    pub fn trap_list(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .traps
            .iter()
            .map(|(signal, action)| format_trap_line(signal.as_ref(), action))
            .collect();
        lines.sort();
        if let Some(source) = &self.exit_trap {
            lines.push(format_trap_line(
                "EXIT",
                &TrapAction::Command(source.clone()),
            ));
        }
        lines
    }

    /// Runs the `EXIT` trap, if any, exactly once — the `exit` builtin
    /// and every one of `conch`'s own top-level exit paths (falling off
    /// the end of a `-c`/script, the interactive loop ending) call this
    /// immediately before actually terminating, matching POSIX's "the
    /// trap on EXIT shall be executed... prior to the shell
    /// terminating." `take()`s the stored source so a trap action that
    /// itself somehow triggers another exit path can't re-run it.
    pub fn run_exit_trap(&mut self) {
        if let Some(source) = self.exit_trap.take() {
            self.run_trap_source(&source);
        }
    }

    fn run_trap_source(&mut self, source: &str) {
        match conch_shell_parser::parse(source) {
            Ok(list) => {
                crate::exec_program(&list, self);
            }
            Err(err) => eprintln!("conch: trap: {err}"),
        }
    }

    /// Configures `command` for job control right before spawning it —
    /// the *one* place every external-process spawn in this crate routes
    /// through, so the fork/`setpgid`-race handling and signal-reset
    /// logic only exist once. `new_process_group: true` makes the
    /// spawned child the leader of its own brand-new process group
    /// (`setpgid(0, 0)`, done via `std::process::Command::process_group`
    /// — the safe, std-provided equivalent of a `pre_exec` `setpgid`
    /// call, preferred over hand-rolling one: see the module docs'
    /// pre_exec-vs-raw-fork note in `conch-shell-core::exec`); `false`
    /// leaves the child in whatever process group it would've inherited
    /// anyway (used for every spawn that isn't itself a job's own
    /// leader — e.g. a stage nested inside an already-`setpgid`'d job).
    ///
    /// *Always* (regardless of `new_process_group`) installs a
    /// `pre_exec` hook that resets every signal disposition this shell
    /// itself changed back to its OS default before the child actually
    /// execs — required so a spawned program doesn't inherit e.g. the
    /// shell's own `SIG_IGN` on `SIGINT`/`SIGTSTP` (which would make an
    /// ordinary external command un-interruptible) or a `trap`'s
    /// deferred-dispatch handler (which would be actively wrong: the
    /// child is a different program entirely, running the parent
    /// shell's trap text on receipt of a signal makes no sense and can't
    /// even work, since the handler's own state lives in *this*
    /// process's statics). The one deliberate exception, matching POSIX:
    /// a signal explicitly `trap '' SIG`'d to [`TrapAction::Ignore`] is
    /// *not* reset — that disposition is specified to survive `exec`
    /// (the classic `nohup`-style use case), and since fork already
    /// inherits it as `SIG_IGN` with nothing further to do, this simply
    /// omits it from the reset list.
    ///
    /// # Safety-review note on `pre_exec`
    /// The closure below runs in the forked child between `fork` and
    /// `execve`, the exact same async-signal-safety-constrained window a
    /// raw `fork()` call would have — `sigaction` is on the documented
    /// async-signal-safe list (`signal-safety(7)`), and every signal to
    /// reset is captured by value into the closure *before* the fork (no
    /// `self`/`Shell` access, no allocation, after it starts running),
    /// per the exact review guidance this function was written to
    /// satisfy.
    pub fn prepare_child_for_job_control(
        &self,
        command: &mut std::process::Command,
        new_process_group: bool,
    ) {
        use std::os::unix::process::CommandExt as _;

        if new_process_group {
            command.process_group(0);
        }

        let mut to_reset: Vec<Signal> = self.handled_signals.iter().copied().collect();
        for signal in IGNORED_AT_TOP_LEVEL {
            if !to_reset.contains(&signal) {
                to_reset.push(signal);
            }
        }
        to_reset.retain(|signal| self.traps.get(signal) != Some(&TrapAction::Ignore));

        // SAFETY: the closure only calls `sigaction` with a `SigDfl`
        // handler (no allocation, no non-reentrant libc call, no access
        // to `self`/any `Shell` state) -- see the doc comment above.
        unsafe {
            command.pre_exec(move || {
                for &signal in &to_reset {
                    let _ = sigaction(
                        signal,
                        &SigAction::new(SigHandler::SigDfl, SaFlags::empty(), SigSet::empty()),
                    );
                }
                Ok(())
            });
        }
    }
}

/// Sets `O_NONBLOCK` on `fd` via raw `fcntl(2)` — `nix::fcntl::fcntl`
/// itself needs the (otherwise-unneeded, broader) `fs` feature just for
/// its `FcntlArg` enum, so this calls the two-step `F_GETFL`/`F_SETFL`
/// sequence directly against `nix::libc` (already a dependency
/// transitively via every other `nix` feature this crate enables, and
/// re-exported at `nix::libc` for exactly this kind of case) instead.
fn set_nonblocking<Fd: AsFd>(fd: &Fd) -> nix::Result<()> {
    let raw = fd.as_fd().as_raw_fd();
    // SAFETY: `raw` is a valid, open fd for the duration of this call
    // (borrowed from a live `AsFd`); `fcntl(F_GETFL)`/`fcntl(F_SETFL,
    // _)` are ordinary, non-allocating syscalls.
    let flags = Errno::result(unsafe { nix::libc::fcntl(raw, nix::libc::F_GETFL) })?;
    let res = unsafe { nix::libc::fcntl(raw, nix::libc::F_SETFL, flags | nix::libc::O_NONBLOCK) };
    Errno::result(res).map(drop)
}

/// Parses one `trap`-argument-shaped signal spec: a bare number
/// (`"2"`), a name with or without the `SIG` prefix (`"INT"`/`"SIGINT"`,
/// case-insensitive — matching real bash accepting either), or `"EXIT"`
/// (`Ok(None)` — POSIX's one non-signal "condition" `trap` accepts, see
/// [`Shell::set_trap`]'s own `signal: Option<Signal>` shape).
fn parse_signal_spec(spec: &str) -> Result<Option<Signal>, String> {
    if spec.eq_ignore_ascii_case("exit") {
        return Ok(None);
    }
    if let Ok(number) = spec.parse::<i32>() {
        return Signal::try_from(number)
            .map(Some)
            .map_err(|_| format!("trap: {spec}: invalid signal number"));
    }
    let upper = spec.to_ascii_uppercase();
    let full_name = if upper.starts_with("SIG") {
        upper
    } else {
        format!("SIG{upper}")
    };
    full_name
        .parse::<Signal>()
        .map(Some)
        .map_err(|_| format!("trap: {spec}: invalid signal specification"))
}

/// `trap -p`'s per-entry output shape — see
/// [`Shell::trap_describe_by_name`]/[`Shell::trap_list`].
fn format_trap_line(name: &str, action: &TrapAction) -> String {
    let command = match action {
        TrapAction::Command(source) => source.as_str(),
        TrapAction::Ignore => "",
        TrapAction::Default => "-",
    };
    format!("trap -- '{command}' {name}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Shell;

    // Every test below exercises only `sigaction`/table bookkeeping
    // (never `kill`/`waitpid`) and deliberately uses SIGUSR1/SIGUSR2 —
    // real, harmless-to-install-a-handler-for signals that neither the
    // test harness nor anything else in this binary cares about the
    // disposition of, avoiding any cross-test interference the way
    // `job.rs`'s own tests avoid the SIGCHLD-reaping race (see
    // `Shell::process_pending_signals`' own docs).

    #[test]
    fn trap_set_by_name_accepts_name_with_or_without_sig_prefix() {
        let mut shell = Shell::new();
        shell
            .trap_set_by_name("USR1", TrapAction::Command("echo a".to_string()))
            .unwrap();
        shell
            .trap_set_by_name("SIGUSR2", TrapAction::Command("echo b".to_string()))
            .unwrap();
        assert_eq!(
            shell.trap_for(Some(Signal::SIGUSR1)),
            Some(&TrapAction::Command("echo a".to_string()))
        );
        assert_eq!(
            shell.trap_for(Some(Signal::SIGUSR2)),
            Some(&TrapAction::Command("echo b".to_string()))
        );
    }

    #[test]
    fn trap_set_by_name_rejects_an_unrecognized_signal() {
        let mut shell = Shell::new();
        assert!(
            shell
                .trap_set_by_name("NOTASIGNAL", TrapAction::Ignore)
                .is_err()
        );
    }

    #[test]
    fn trap_set_by_name_exit_stores_the_exit_trap_not_a_signal_trap() {
        let mut shell = Shell::new();
        shell
            .trap_set_by_name("EXIT", TrapAction::Command("echo bye".to_string()))
            .unwrap();
        assert_eq!(shell.exit_trap(), Some("echo bye"));
        assert!(shell.traps_iter().next().is_none());
    }

    #[test]
    fn trap_describe_by_name_is_none_when_nothing_is_trapped() {
        let shell = Shell::new();
        assert_eq!(shell.trap_describe_by_name("USR1").unwrap(), None);
    }

    #[test]
    fn trap_describe_by_name_reports_a_registered_command_trap() {
        let mut shell = Shell::new();
        shell
            .trap_set_by_name("USR1", TrapAction::Command("echo caught".to_string()))
            .unwrap();
        let line = shell.trap_describe_by_name("USR1").unwrap().unwrap();
        assert!(line.contains("echo caught"));
        assert!(line.contains("USR1") || line.contains("SIGUSR1"));
    }

    #[test]
    fn trap_describe_by_name_reports_an_ignored_signal_as_non_empty() {
        // `trap '' SIG` is distinct from never trapping it at all --
        // `trap -p` must still report *something* (an empty-command
        // trap), matching real bash and exactly what the differential
        // corpus's `[ -n "$(trap -p ...)" ]`-style checks rely on.
        let mut shell = Shell::new();
        shell.trap_set_by_name("USR1", TrapAction::Ignore).unwrap();
        assert!(shell.trap_describe_by_name("USR1").unwrap().is_some());
    }

    #[test]
    fn trap_default_after_ignore_clears_the_registration() {
        let mut shell = Shell::new();
        shell.trap_set_by_name("USR1", TrapAction::Ignore).unwrap();
        shell.trap_set_by_name("USR1", TrapAction::Default).unwrap();
        assert_eq!(shell.trap_describe_by_name("USR1").unwrap(), None);
    }

    #[test]
    fn trap_list_includes_every_registered_signal_and_exit() {
        let mut shell = Shell::new();
        shell
            .trap_set_by_name("USR1", TrapAction::Command("echo a".to_string()))
            .unwrap();
        shell
            .trap_set_by_name("EXIT", TrapAction::Command("echo bye".to_string()))
            .unwrap();
        let lines = shell.trap_list();
        assert!(lines.iter().any(|l| l.contains("echo a")));
        assert!(
            lines
                .iter()
                .any(|l| l.contains("echo bye") && l.contains("EXIT"))
        );
    }

    #[test]
    fn parse_signal_spec_accepts_a_number() {
        assert_eq!(parse_signal_spec("9"), Ok(Some(Signal::SIGKILL)));
    }

    #[test]
    fn parse_signal_spec_accepts_exit_case_insensitively() {
        assert_eq!(parse_signal_spec("exit"), Ok(None));
        assert_eq!(parse_signal_spec("EXIT"), Ok(None));
    }
}
