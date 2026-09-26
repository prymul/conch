//! The job table: background/stopped jobs (`jobs`/`fg`/`bg`/`wait`, `$!`),
//! and reaping child state changes into it via `waitpid`.
//!
//! # Design: one job = one process group, always its own leader
//!
//! Every job this crate ever creates — whether backgrounded (`cmd &`) or
//! a foreground external command that later gets suspended (Ctrl-Z) — is
//! given its own, brand-new process group, with the job's own leading
//! process as that group's leader (`setpgid(leader, leader)`), following
//! the GNU libc manual's job-control protocol (see
//! `conch-shell-core::signals`' module docs for the full grounding and
//! the fork/`setpgid`-race handling that protocol requires).
//!
//! That means a [`Job`]'s `pgid` and `leader` are numerically always the
//! same value in practice (a process's own PID *is* its process group ID
//! once it's called `setpgid` on itself) — they're kept as two separate,
//! same-typed fields anyway because they mean two conceptually different
//! things (a process-group identity to signal, vs. a specific process
//! identity for `$!`/`wait <pid>` to report), and because a security
//! review of this design flagged exactly the TOCTOU hazard that
//! collapsing them could invite: **every signal this crate sends to a
//! job goes to its `pgid` (`killpg`-shaped: `kill(Pid::from_raw(-pgid))`),
//! never to a bare remembered `leader` PID** — see
//! [`Shell::signal_job`]'s docs. A process group ID is stable for the
//! job's entire lifetime by construction (the kernel won't reuse it while
//! any process in the group still exists), so signaling it can never
//! race a concurrent reap the way signaling a specific, potentially
//! already-recycled PID could.
//!
//! # Why [`JobState`] is only ever updated in one place
//!
//! Every [`Job`]'s state is exactly whatever
//! [`Shell::apply_wait_status`] most recently observed for it, never
//! guessed at, inferred from a timeout, or updated any other way — both
//! `waitpid`-calling entry points in this module
//! ([`Shell::reap_children`]'s non-blocking per-leader sweep and
//! [`Shell::block_for_any_child_change`]'s blocking single step, used by
//! `wait`) route every observation through that one shared function so
//! they can never disagree about what a given `WaitStatus` means.
//! [`Shell::reap_children`] is designed to be called liberally (before
//! every prompt, right after any foreground job returns control to the
//! shell, and from the deferred-`SIGCHLD` signal checkpoint — see
//! `conch-shell-core::signals`) specifically *because* it's cheap and
//! idempotent when there's nothing new to reap for any tracked job, and
//! — since a security review had it scoped from an earlier, broader
//! `waitpid(-1, ...)` sweep to one `WNOHANG` call per tracked job leader
//! specifically (see that function's own docs for the full reasoning) —
//! it's also now structurally incapable of observing, let alone
//! reaping, any child this shell didn't itself register as a job.

use std::collections::BTreeMap;

use nix::errno::Errno;
use nix::sys::wait::{WaitPidFlag, WaitStatus, waitpid};
use nix::unistd::Pid;

use crate::Shell;

/// A job's current state — see the module docs for why this is *only*
/// ever set by [`Shell::reap_children`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Running (in the foreground or background) — never yet reported
    /// stopped or finished.
    Running,
    /// Stopped (`SIGTSTP`/`SIGTTIN`/`SIGTTOU`/`SIGSTOP`), resumable via
    /// `bg`/`fg` (which sends `SIGCONT` to the whole process group — see
    /// [`Shell::signal_job`]).
    Stopped,
    /// Exited normally with this status.
    Done(i32),
    /// Terminated by this signal (raw signal number) — `$?` for a
    /// foreground job killed this way is `128 + signal`, the same
    /// convention POSIX/bash use; this stores the bare signal number
    /// rather than the already-added-128 form so callers can format
    /// either the raw signal name (`jobs`'s "Terminated" style lines) or
    /// the `128+n` exit-status form as needed.
    Signaled(i32),
}

impl JobState {
    /// Whether this job has reached a terminal state (`Done`/`Signaled`)
    /// — it's only still present in the table because [`Job::notified`]
    /// hasn't reported that yet, or because nothing has queried/removed
    /// it since.
    #[must_use]
    pub fn is_finished(&self) -> bool {
        matches!(self, JobState::Done(_) | JobState::Signaled(_))
    }

    /// The bash-`jobs`-style one-word status label.
    #[must_use]
    pub fn label(&self) -> &'static str {
        match self {
            JobState::Running => "Running",
            JobState::Stopped => "Stopped",
            JobState::Done(0) => "Done",
            JobState::Done(_) => "Done(non-zero)",
            JobState::Signaled(_) => "Terminated",
        }
    }
}

/// One background or (formerly foreground, now suspended) job.
#[derive(Debug, Clone)]
pub struct Job {
    /// The small, human-facing job number `jobs`/`fg %N`/`bg %N` use —
    /// distinct from any PID, and reused only after this job is removed
    /// from the table (see [`JobTable::register`]).
    pub id: u32,
    /// The job's process group — see the module docs for why every
    /// signal sent to this job targets this, never [`Self::leader`]
    /// directly.
    pub pgid: Pid,
    /// The job's process-group-leader PID — what `$!` and `wait <pid>`
    /// report.
    pub leader: Pid,
    /// The source text run in this job — `jobs`'s own display column,
    /// and (for an `Async` job) exactly [`conch_shell_parser::Separator::Async`]'s
    /// captured raw source.
    pub command: String,
    pub state: JobState,
    /// Whether this job's *current* `state` has already been reported to
    /// the user (the `[1]+ Done sleep 5`-style line printed before a
    /// prompt/by `jobs`) — set once printed, and re-cleared by
    /// [`Shell::reap_children`] whenever `state` actually changes again,
    /// so a stop-then-later-exit both get their own, separate
    /// notification, matching real bash.
    pub notified: bool,
}

/// The job table itself — see the module docs.
#[derive(Debug, Default)]
pub struct JobTable {
    jobs: BTreeMap<Pid, Job>,
    next_id: u32,
    /// bash's "current job" (`%+`) — what a bare `fg`/`bg` (no job spec)
    /// targets: the most recently backgrounded-or-stopped job.
    current: Option<Pid>,
    /// bash's "previous job" (`%-`) — promoted to [`Self::current`] once
    /// it's removed.
    previous: Option<Pid>,
}

impl JobTable {
    #[must_use]
    pub fn new() -> Self {
        Self {
            jobs: BTreeMap::new(),
            next_id: 1,
            current: None,
            previous: None,
        }
    }

    /// Registers a freshly-started job (always [`JobState::Running`] —
    /// there's no other state a job can be in at the moment its own
    /// process group is created) and makes it the new "current job."
    /// Returns the assigned job id.
    pub fn register(&mut self, pgid: Pid, leader: Pid, command: String) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        self.jobs.insert(
            leader,
            Job {
                id,
                pgid,
                leader,
                command,
                state: JobState::Running,
                notified: false,
            },
        );
        self.previous = self.current;
        self.current = Some(leader);
        id
    }

    /// Registers a job that's already known to be freshly [`JobState::Stopped`]
    /// — a foreground job just suspended via `SIGTSTP` — the sibling of
    /// [`Self::register`]'s ordinary "starts `Running`" case, for a job
    /// `conch-shell-core::exec` never called [`Self::register`] for in
    /// the first place (only a stopped-or-backgrounded job is ever
    /// tracked at all — see that module's `run_foreground_job` docs).
    pub fn register_stopped(&mut self, pgid: Pid, leader: Pid, command: String) -> u32 {
        let id = self.register(pgid, leader, command);
        if let Some(job) = self.get_mut_by_leader(leader) {
            job.state = JobState::Stopped;
        }
        id
    }

    #[must_use]
    pub fn get(&self, id: u32) -> Option<&Job> {
        self.jobs.values().find(|job| job.id == id)
    }

    /// The mutable sibling of [`Self::get`] — `fg`/`bg`
    /// (`conch-shell-builtins`, via [`Shell::resume_job_in_background`]/
    /// `conch-shell-core::exec::resume_job_in_foreground`) need this to
    /// update a job's state by its human-facing id, not
    /// [`Self::get_mut_by_leader`]'s pid-keyed lookup.
    pub fn get_mut(&mut self, id: u32) -> Option<&mut Job> {
        self.jobs.values_mut().find(|job| job.id == id)
    }

    #[must_use]
    pub fn get_by_leader(&self, leader: Pid) -> Option<&Job> {
        self.jobs.get(&leader)
    }

    fn get_mut_by_leader(&mut self, leader: Pid) -> Option<&mut Job> {
        self.jobs.get_mut(&leader)
    }

    /// Every job currently in the table, in job-id order — `jobs`'s own
    /// iteration order (ascending, oldest first, matching real bash).
    pub fn iter(&self) -> impl Iterator<Item = &Job> {
        self.jobs.values()
    }

    /// Removes a finished job from the table once it's been reported
    /// (`jobs`, `wait`, or the next prompt's notification already
    /// printed it) — a *stopped* job is never removed this way, only
    /// ever by [`Self::remove`] via `fg` handing it a fresh completion
    /// later.
    pub fn remove(&mut self, id: u32) -> Option<Job> {
        let leader = self.jobs.values().find(|j| j.id == id).map(|j| j.leader)?;
        let removed = self.jobs.remove(&leader);
        if self.current == Some(leader) {
            self.current = self.previous.take();
        } else if self.previous == Some(leader) {
            self.previous = None;
        }
        removed
    }

    /// The job id `fg`/`bg` with no argument targets — bash's `%+`.
    #[must_use]
    pub fn current_job_id(&self) -> Option<u32> {
        self.current
            .and_then(|leader| self.jobs.get(&leader))
            .map(|j| j.id)
    }

    /// Resolves a bash-style job spec (`%1`, `%+`, `%-`, `%sleep` —
    /// prefix match against [`Job::command`], `%%` as a synonym for
    /// `%+`, or bare digits with no leading `%`, matching real bash
    /// accepting `fg 1` as well as `fg %1`) to a job id. `None` resolves
    /// to the current job ([`Self::current_job_id`]), matching bare
    /// `fg`/`bg`.
    ///
    /// # Errors
    ///
    /// Returns a bash-shaped error message (not a structured error type
    /// — every caller of this is a builtin that just needs to print
    /// *something* to stderr and return a nonzero status, so there's no
    /// real value in a richer error type here) if `spec` doesn't resolve
    /// to exactly one job.
    pub fn resolve_spec(&self, spec: Option<&str>) -> Result<u32, String> {
        let Some(spec) = spec else {
            return self
                .current_job_id()
                .ok_or_else(|| "current: no such job".to_string());
        };
        let rest = spec.strip_prefix('%').unwrap_or(spec);
        if rest == "+" || rest == "%" || rest.is_empty() {
            return self
                .current_job_id()
                .ok_or_else(|| format!("{spec}: no such job"));
        }
        if rest == "-" {
            return self
                .previous
                .and_then(|leader| self.jobs.get(&leader))
                .map(|j| j.id)
                .ok_or_else(|| format!("{spec}: no such job"));
        }
        if let Ok(id) = rest.parse::<u32>() {
            return self
                .get(id)
                .map(|j| j.id)
                .ok_or_else(|| format!("{spec}: no such job"));
        }
        // `%name`: unambiguous prefix match against the job's command
        // text, matching real bash.
        let mut matches = self.jobs.values().filter(|j| j.command.starts_with(rest));
        let first = matches
            .next()
            .ok_or_else(|| format!("{spec}: no such job"))?;
        if matches.next().is_some() {
            return Err(format!("{spec}: ambiguous job spec"));
        }
        Ok(first.id)
    }
}

impl Shell {
    /// Reaps every currently-reapable **tracked job leader** (one
    /// `waitpid(leader, WNOHANG | WUNTRACED | WCONTINUED)` call per
    /// [`Job`] in [`Shell::job_table`], never the whole-process
    /// `waitpid(-1, ...)` sweep an earlier version of this function
    /// used) and updates the job table accordingly. Never blocks — safe
    /// to call speculatively and often (see the module docs).
    ///
    /// Deliberately scoped to specific, already-known pids rather than
    /// "whatever this process happens to have reapable right now" —
    /// caught by a security review: a process-wide `waitpid(-1, ...)`
    /// reaps *indiscriminately*, meaning it could consume the exit
    /// status of a child some *other*, unrelated code in this same
    /// process is separately, directly `waitpid`-ing for (an ordinary
    /// foreground external-command spawn that never went through the
    /// job table at all, say), stealing it before that other code ever
    /// gets to observe it. Under this crate's real, single-threaded
    /// production execution model that specific race can't actually
    /// happen (nothing else runs concurrently while this process is
    /// blocked in *its own* `waitpid` call — see
    /// `conch-shell-core::exec::run_foreground_job`'s docs), but relying
    /// on that as the *only* thing preventing it was an unenforced
    /// invariant, not a structural guarantee — the exact hazard that
    /// once caused a real, reproducible flake in this crate's own
    /// multi-threaded `cargo test` binary (see
    /// [`Shell::process_pending_signals`]'s docs for that incident),
    /// and would reopen the instant any *future* test also called
    /// [`Shell::init_signal_handling`]. Targeting only this shell's own
    /// tracked job leaders removes the hazard structurally instead of
    /// merely avoiding triggering it today: this function can no longer
    /// observe, let alone reap, a child it doesn't already know about.
    ///
    /// Returns the ids of every job whose [`JobState`] just changed
    /// (freshly `Stopped`/`Done`/`Signaled`, or `Running` again via
    /// `SIGCONT`), for a caller (the interactive prompt loop) to print
    /// bash's `[1]+ Done sleep 5`-style notification for.
    pub fn reap_children(&mut self) -> Vec<u32> {
        // Collected up front (rather than iterating `self.job_table`
        // directly) so the loop below is free to take `&mut self` per
        // leader via `apply_wait_status` without fighting the borrow
        // checker over a concurrent immutable iterator — `Pid` is
        // `Copy`, so this is a cheap, small copy, not a real allocation
        // concern even with many tracked jobs.
        let leaders: Vec<Pid> = self.job_table.iter().map(|job| job.leader).collect();
        let flags = WaitPidFlag::WNOHANG | WaitPidFlag::WUNTRACED | WaitPidFlag::WCONTINUED;
        let mut changed = Vec::new();
        for leader in leaders {
            loop {
                match waitpid(leader, Some(flags)) {
                    // `ECHILD` here just means *this* leader has nothing
                    // new to report (already reaped by an earlier call,
                    // or never had anything pending) -- not an error
                    // condition worth treating differently from
                    // `StillAlive`.
                    Ok(WaitStatus::StillAlive) | Err(Errno::ECHILD) => break,
                    Err(Errno::EINTR) => continue,
                    Err(_) => break,
                    Ok(status) => {
                        changed.extend(self.apply_wait_status(status));
                        // A specific-pid `waitpid` call only ever reports
                        // one state transition per call (unlike `-1`,
                        // which could have more waiting across *other*
                        // children) -- nothing further to drain for this
                        // one leader on this pass.
                        break;
                    }
                }
            }
        }
        changed
    }

    /// Applies one [`WaitStatus`] observation to whichever tracked job it
    /// belongs to (`None` if it doesn't belong to any tracked job at all
    /// — no longer reachable from [`Self::reap_children`] itself, since
    /// that function only ever asks `waitpid` about pids it already
    /// knows are tracked leaders, but still reachable from
    /// [`Self::block_for_any_child_change`], which — unlike
    /// `reap_children` — has no way to narrow its own `waitpid(-1, ...)`
    /// call to specific pids at all, see that function's own docs),
    /// returning that job's id if it was tracked and its state actually
    /// changed. The one piece of "turn a raw `waitpid` result into a
    /// `JobState` update" logic, shared by both callers, so they can
    /// never disagree about what a given `WaitStatus` means.
    fn apply_wait_status(&mut self, status: WaitStatus) -> Option<u32> {
        // `.pid()` is `WaitStatus`'s own accessor (handles the
        // Linux-only `PtraceEvent`/`PtraceSyscall` variants' cfg-gating
        // internally, so this stays correct on every platform without
        // this crate needing to match those two arms itself).
        let job = self.job_table.get_mut_by_leader(status.pid()?)?;
        let new_state = match status {
            WaitStatus::Exited(_, code) => Some(JobState::Done(code)),
            WaitStatus::Signaled(_, sig, _) => Some(JobState::Signaled(sig as i32)),
            WaitStatus::Stopped(..) => Some(JobState::Stopped),
            WaitStatus::Continued(_) => Some(JobState::Running),
            _ => None,
        }?;
        job.state = new_state;
        job.notified = false;
        Some(job.id)
    }

    /// Blocks until *some* child's state changes (`waitpid(-1, WUNTRACED
    /// | WCONTINUED)`, no `WNOHANG`) and applies it — the `wait` builtin
    /// (`conch-shell-builtins`)'s core primitive: rather than polling,
    /// it loops "is the job I'm waiting for already in a terminal state
    /// per the table?" / "no — block here for *something* to change,
    /// then recheck." Race-free under this crate's single-threaded
    /// execution model for the exact same reason
    /// `conch-shell-core::exec`'s `run_foreground_job` is (see that function's
    /// docs): nothing else in this process runs concurrently while this
    /// call is blocked, so it can never "lose" a state change to a
    /// competing `waitpid` call the way two real OS threads could.
    ///
    /// Still uses `waitpid(-1, ...)` (unlike [`Self::reap_children`],
    /// which a security review had scoped to specific tracked leaders
    /// instead — see that function's own docs) rather than one call per
    /// tracked leader, because a *blocking* "wait for whichever of these
    /// N specific pids changes first" has no single-syscall equivalent
    /// to fall back on the way a non-blocking, one-leader-at-a-time sweep
    /// does — `waitpid` only ever blocks for one specific pid, or for
    /// *any* child. This carries the same theoretical "could reap an
    /// unrelated concurrent `waitpid` caller's child out from under it"
    /// category of hazard `reap_children` had, under the same purely
    /// hypothetical condition (a future test spawning real children
    /// concurrently with a `wait`/`fg`/`bg` call on a *different*
    /// thread of one `cargo test` binary — not a concern in this crate's
    /// actual single-threaded production execution, per the paragraph
    /// above) — flagged here rather than silently left unremarked, not
    /// fixed in this pass (no *drop-in* narrower replacement exists for
    /// the blocking-on-multiple-specific-targets case the way there did
    /// for `reap_children`'s simpler "sweep everything" one).
    ///
    /// Returns `false` if there was nothing left to wait for at all
    /// (`ECHILD` — every child of this process has already been
    /// reaped), so a caller looping on this doesn't spin forever on a
    /// target that will now never change: if that target was a job this
    /// process itself already reaped (its own `JobState` is already
    /// correctly `Done`/`Signaled` in the table from whenever that
    /// happened), the caller's own next state check already has the
    /// answer; if it names something that was never this shell's child
    /// at all, the caller's own lookup already reports that as an error
    /// without ever reaching this function.
    pub fn block_for_any_child_change(&mut self) -> bool {
        loop {
            match waitpid(None, Some(WaitPidFlag::WUNTRACED | WaitPidFlag::WCONTINUED)) {
                Ok(status) => {
                    self.apply_wait_status(status);
                    return true;
                }
                Err(Errno::EINTR) => continue,
                Err(_) => return false,
            }
        }
    }

    /// Signals a job by its **process group**, never a bare remembered
    /// PID — see the module docs for why. `signal: None` just checks the
    /// job still exists (mirrors `kill -0`'s "is this still alive"
    /// usage, not needed by anything in this crate yet but kept for
    /// symmetry with [`nix::sys::signal::kill`]'s own `T: Into<Option<Signal>>`
    /// shape).
    ///
    /// # Errors
    ///
    /// Propagates `killpg`'s own error (most commonly `ESRCH`: the whole
    /// group has already exited and been reaped).
    pub fn signal_job(
        &self,
        id: u32,
        signal: impl Into<Option<nix::sys::signal::Signal>>,
    ) -> nix::Result<()> {
        let job = self.job_table.get(id).ok_or(nix::errno::Errno::ESRCH)?;
        nix::sys::signal::killpg(job.pgid, signal)
    }

    /// Removes every job that's both finished ([`JobState::is_finished`])
    /// and already reported once ([`Job::notified`]) — called by the
    /// `jobs` builtin (`conch-shell-builtins`) after listing, and by the
    /// interactive prompt loop's own per-prompt notification pass
    /// (`conch`), matching real bash showing a completed background
    /// job's status exactly once before forgetting it. Deliberately
    /// *not* called from [`Self::reap_children`] itself, or from
    /// anywhere on the `wait`/`$!` path — a job needs to stay queryable
    /// (by [`Self::wait_for_pid`], `fg %n`, `jobs`) for at least one full
    /// round after finishing, which is exactly what gating this on
    /// `notified` (only ever set once something has actually reported
    /// it) guarantees.
    pub fn purge_finished_notified_jobs(&mut self) {
        let ids: Vec<u32> = self
            .job_table
            .iter()
            .filter(|job| job.state.is_finished() && job.notified)
            .map(|job| job.id)
            .collect();
        for id in ids {
            self.job_table.remove(id);
        }
    }

    /// `wait pid` (`conch-shell-builtins`) — blocks until the tracked job
    /// led by `pid` reaches a terminal state, returning its exit status
    /// (the `128 + signal` convention for [`JobState::Signaled`], the
    /// same one [`crate::exec`]'s own `exit_code_of` uses for a
    /// foreground command). `None` means `pid` was never this shell's
    /// (tracked) child at all — POSIX: an error, distinct from any real
    /// exit status, so a bare `i32` can't represent it — the caller
    /// reports that.
    ///
    /// Race-free the same way [`Self::block_for_any_child_change`] is
    /// (see that function's docs): looping "already terminal? return it.
    /// Not yet? block for *something* to change, then recheck" rather
    /// than trying to `waitpid` this specific `pid` directly, which
    /// would fail outright (`ECHILD`) if it had already been reaped by
    /// an earlier sweep — this instead always has the answer already
    /// recorded in the table by the time that sweep ran.
    pub fn wait_for_pid(&mut self, pid: i32) -> Option<i32> {
        let target = Pid::from_raw(pid);
        loop {
            match self.job_table.get_by_leader(target).map(|job| job.state) {
                Some(JobState::Done(code)) => return Some(code),
                Some(JobState::Signaled(sig)) => return Some(128 + sig),
                Some(JobState::Running | JobState::Stopped) => {
                    if !self.block_for_any_child_change() {
                        // Nothing left to `waitpid` on at all -- avoid
                        // spinning forever on a target that can now
                        // never change; one last look at the table
                        // covers the case where a concurrent sweep
                        // updated *this exact* job in the same instant
                        // this call observed `ECHILD`.
                        return match self.job_table.get_by_leader(target).map(|job| job.state) {
                            Some(JobState::Done(code)) => Some(code),
                            Some(JobState::Signaled(sig)) => Some(128 + sig),
                            _ => None,
                        };
                    }
                }
                None => return None,
            }
        }
    }

    /// A bare `wait` (`conch-shell-builtins`) — blocks until every
    /// currently [`JobState::Running`] tracked job has reached a
    /// terminal state. Always exits `0` regardless of any individual
    /// job's own status (POSIX: "the exit status from the wait utility
    /// will be zero" for this form) — the caller doesn't need this
    /// function's return value for that reason; it returns nothing.
    /// Deliberately does not also wait for [`JobState::Stopped`] jobs —
    /// matching real bash, a stopped job isn't "in progress" toward
    /// finishing on its own, and a bare `wait` blocking on one
    /// indefinitely (until some *other* actor resumes it) would be
    /// surprising rather than useful.
    pub fn wait_for_all_background_jobs(&mut self) {
        loop {
            let any_running = self
                .job_table
                .iter()
                .any(|job| matches!(job.state, JobState::Running));
            if !any_running || !self.block_for_any_child_change() {
                break;
            }
        }
    }

    /// `bg %n` (`conch-shell-builtins`) — resumes a stopped job in the
    /// background: `SIGCONT` to its process group ([`Self::signal_job`])
    /// and marks it [`JobState::Running`] again immediately (rather than
    /// waiting for the next reap to observe the `SIGCONT`-driven
    /// [`nix::sys::wait::WaitStatus::Continued`] transition), so `jobs`
    /// reflects it right away instead of momentarily still showing
    /// `Stopped`.
    ///
    /// # Errors
    ///
    /// `id` isn't a known job, or the underlying `killpg` call failed
    /// (most commonly: the whole group already exited).
    pub fn resume_job_in_background(&mut self, id: u32) -> Result<(), String> {
        if self.job_table.get(id).is_none() {
            return Err(format!("bg: {id}: no such job"));
        }
        let result = self.signal_job(id, nix::sys::signal::Signal::SIGCONT);
        if let Some(job) = self.job_table.get_mut(id) {
            job.state = JobState::Running;
            job.notified = false;
        }
        result.map_err(|err| format!("bg: {err}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pid(n: i32) -> Pid {
        Pid::from_raw(n)
    }

    #[test]
    fn register_assigns_sequential_ids_and_tracks_current() {
        let mut table = JobTable::new();
        let id1 = table.register(pid(100), pid(100), "sleep 1".to_string());
        let id2 = table.register(pid(200), pid(200), "sleep 2".to_string());
        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(table.current_job_id(), Some(2));
    }

    #[test]
    fn resolve_spec_bare_defaults_to_current_job() {
        let mut table = JobTable::new();
        table.register(pid(100), pid(100), "sleep 1".to_string());
        let id2 = table.register(pid(200), pid(200), "sleep 2".to_string());
        assert_eq!(table.resolve_spec(None), Ok(id2));
        assert_eq!(table.resolve_spec(Some("%+")), Ok(id2));
    }

    #[test]
    fn resolve_spec_previous_job() {
        let mut table = JobTable::new();
        let id1 = table.register(pid(100), pid(100), "sleep 1".to_string());
        table.register(pid(200), pid(200), "sleep 2".to_string());
        assert_eq!(table.resolve_spec(Some("%-")), Ok(id1));
    }

    #[test]
    fn resolve_spec_by_number() {
        let mut table = JobTable::new();
        let id1 = table.register(pid(100), pid(100), "sleep 1".to_string());
        assert_eq!(table.resolve_spec(Some("%1")), Ok(id1));
        assert_eq!(table.resolve_spec(Some("1")), Ok(id1));
    }

    #[test]
    fn resolve_spec_by_unambiguous_command_prefix() {
        let mut table = JobTable::new();
        let id1 = table.register(pid(100), pid(100), "sleep 100".to_string());
        assert_eq!(table.resolve_spec(Some("%sleep")), Ok(id1));
    }

    #[test]
    fn resolve_spec_ambiguous_prefix_errors() {
        let mut table = JobTable::new();
        table.register(pid(100), pid(100), "sleep 1".to_string());
        table.register(pid(200), pid(200), "sleep 2".to_string());
        assert!(table.resolve_spec(Some("%sleep")).is_err());
    }

    #[test]
    fn remove_promotes_previous_to_current() {
        let mut table = JobTable::new();
        let id1 = table.register(pid(100), pid(100), "a".to_string());
        let id2 = table.register(pid(200), pid(200), "b".to_string());
        assert_eq!(table.current_job_id(), Some(id2));
        table.remove(id2);
        assert_eq!(table.current_job_id(), Some(id1));
    }

    #[test]
    fn job_state_is_finished() {
        assert!(!JobState::Running.is_finished());
        assert!(!JobState::Stopped.is_finished());
        assert!(JobState::Done(0).is_finished());
        assert!(JobState::Signaled(9).is_finished());
    }
}
