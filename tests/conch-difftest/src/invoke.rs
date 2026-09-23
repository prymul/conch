//! Spawning a shell (real oracle or conch-under-test) and capturing its
//! output, byte-for-byte.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::case::Invocation;

/// The raw, unnormalized outcome of running a script under one shell.
///
/// Deliberately `Vec<u8>`, not `String`: shell scripts can legitimately
/// produce output that isn't valid UTF-8, and comparisons must happen on
/// raw bytes so two different invalid byte sequences are never silently
/// treated as equal (see [`crate::report::render_bytes`] for how these are
/// rendered for a human without losing that distinction).
#[derive(Debug, Clone)]
pub struct RunOutcome {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// `None` when the process was terminated by a signal rather than
    /// exiting normally, mirroring `std::process::ExitStatus::code()`.
    pub exit_code: Option<i32>,
}

/// Which shell binary to invoke.
#[derive(Debug, Clone)]
pub enum ShellUnderTest {
    /// A real oracle shell, resolved via `PATH` (`"bash"` or `"sh"`).
    Oracle(&'static str),
    /// The conch binary under test, at a concrete path.
    Conch(PathBuf),
}

impl ShellUnderTest {
    fn program(&self) -> &Path {
        match self {
            ShellUnderTest::Oracle(name) => Path::new(name),
            ShellUnderTest::Conch(path) => path.as_path(),
        }
    }
}

/// Runs `script` under `shell` using the given [`Invocation`] mode, inside
/// `workdir` (which the caller is responsible for creating fresh per run --
/// see the crate README on why every run gets its own temp directory).
pub fn run(
    shell: &ShellUnderTest,
    invocation: Invocation,
    script: &str,
    workdir: &Path,
) -> std::io::Result<RunOutcome> {
    let mut cmd = Command::new(shell.program());
    cmd.current_dir(workdir);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    // Keeps the tempfile alive (if any) until after the child has been
    // spawned; the child only needs the path to exist at spawn time.
    let mut _script_file_guard = None;
    let mut stdin_payload = None;

    match invocation {
        Invocation::DashC => {
            cmd.arg("-c").arg(script);
            cmd.stdin(Stdio::null());
        }
        Invocation::ScriptFile => {
            let mut file = tempfile::NamedTempFile::new_in(workdir)?;
            file.write_all(script.as_bytes())?;
            file.flush()?;
            cmd.arg(file.path());
            cmd.stdin(Stdio::null());
            _script_file_guard = Some(file);
        }
        Invocation::StdinPipe => {
            cmd.stdin(Stdio::piped());
            stdin_payload = Some(script.as_bytes().to_vec());
        }
    }

    let mut child = cmd.spawn()?;

    if let Some(payload) = stdin_payload {
        // `Stdio::piped()` above guarantees `stdin` is `Some`.
        let mut stdin = child.stdin.take().expect("child stdin was piped");
        stdin.write_all(&payload)?;
        // Dropping `stdin` here closes the write end so the child sees EOF
        // instead of hanging waiting for more input.
    }

    let output = child.wait_with_output()?;

    Ok(RunOutcome {
        stdout: output.stdout,
        stderr: output.stderr,
        exit_code: output.status.code(),
    })
}

/// Locates the conch binary under test, or `None` if it isn't available
/// yet.
///
/// This deliberately does *not* rely on Cargo's usual `CARGO_BIN_EXE_<name>`
/// trick (setting it automatically because the binary's package is a
/// dependency): that mechanism requires the dependency to have a `[lib]`
/// target to link against, and `conch-shell` is bin-only (confirmed by
/// trying it -- Cargo silently drops a path dependency on a lib-less
/// package with just a warning, never setting the env var). The other
/// standard alternative, Cargo's artifact-dependencies (`bindeps`)
/// feature, is still nightly-only (`-Z bindeps`, confirmed against this
/// toolchain) and not usable from a stable-toolchain CI job. So: plain
/// path discovery relative to the workspace's `target/` directory, which
/// this crate can locate reliably since `tests/conch-difftest` always
/// lives exactly two directories below the workspace root in this repo's
/// layout.
pub fn find_conch_binary() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("CONCH_BIN") {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }

    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| workspace_root().join("target"));

    // Prefer a release build if both happen to exist (e.g. a local
    // `cargo build --release` run) since it's the more likely one to be
    // intentionally under test; fall back to debug otherwise.
    for profile in ["release", "debug"] {
        let candidate = target_dir.join(profile).join("conch");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn workspace_root() -> PathBuf {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest_dir
        .parent()
        .and_then(Path::parent)
        .unwrap_or(&manifest_dir)
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dash_c_invocation_runs_bash_and_captures_stdout() {
        let workdir = tempfile::tempdir().unwrap();
        let outcome = run(
            &ShellUnderTest::Oracle("bash"),
            Invocation::DashC,
            "echo hello",
            workdir.path(),
        )
        .unwrap();
        assert_eq!(outcome.stdout, b"hello\n");
        assert_eq!(outcome.exit_code, Some(0));
    }

    #[test]
    fn script_file_invocation_runs_bash_and_captures_stdout() {
        let workdir = tempfile::tempdir().unwrap();
        let outcome = run(
            &ShellUnderTest::Oracle("bash"),
            Invocation::ScriptFile,
            "echo one\necho two\n",
            workdir.path(),
        )
        .unwrap();
        assert_eq!(outcome.stdout, b"one\ntwo\n");
        assert_eq!(outcome.exit_code, Some(0));
    }

    #[test]
    fn stdin_pipe_invocation_runs_bash_and_captures_stdout() {
        let workdir = tempfile::tempdir().unwrap();
        let outcome = run(
            &ShellUnderTest::Oracle("bash"),
            Invocation::StdinPipe,
            "echo via-stdin\n",
            workdir.path(),
        )
        .unwrap();
        assert_eq!(outcome.stdout, b"via-stdin\n");
        assert_eq!(outcome.exit_code, Some(0));
    }

    #[test]
    fn exit_code_is_captured() {
        let workdir = tempfile::tempdir().unwrap();
        let outcome = run(
            &ShellUnderTest::Oracle("bash"),
            Invocation::DashC,
            "exit 7",
            workdir.path(),
        )
        .unwrap();
        assert_eq!(outcome.exit_code, Some(7));
    }

    #[test]
    fn runs_in_the_given_workdir() {
        let workdir = tempfile::tempdir().unwrap();
        let outcome = run(
            &ShellUnderTest::Oracle("bash"),
            Invocation::DashC,
            "pwd",
            workdir.path(),
        )
        .unwrap();
        let canonical = std::fs::canonicalize(workdir.path()).unwrap();
        let printed = String::from_utf8(outcome.stdout).unwrap();
        assert_eq!(printed.trim_end(), canonical.to_string_lossy());
    }

    #[test]
    fn find_conch_binary_honors_conch_bin_override() {
        let workdir = tempfile::tempdir().unwrap();
        let fake_bin = workdir.path().join("fake-conch");
        std::fs::write(&fake_bin, b"").unwrap();

        // SAFETY-ish: this mutates process-global state, but the test
        // restores it immediately and doesn't run concurrently with other
        // tests that read `CONCH_BIN` (none currently do).
        unsafe { std::env::set_var("CONCH_BIN", &fake_bin) };
        let found = find_conch_binary();
        unsafe { std::env::remove_var("CONCH_BIN") };

        assert_eq!(found, Some(fake_bin));
    }

    #[test]
    fn workspace_root_points_at_the_real_workspace_manifest() {
        let manifest = std::fs::read_to_string(workspace_root().join("Cargo.toml")).unwrap();
        assert!(manifest.contains("[workspace]"));
    }

    #[test]
    fn find_conch_binary_locates_a_build_when_present() {
        // Environment-coupled by nature (it's checking what's actually on
        // disk, and dev machines may have both a debug and a release
        // build lying around), so this mirrors `find_conch_binary`'s own
        // release-before-debug preference to compute what it *should*
        // find, and only asserts anything when there's a build to find
        // and no override is already pointing elsewhere.
        if std::env::var_os("CONCH_BIN").is_some() || std::env::var_os("CARGO_TARGET_DIR").is_some()
        {
            return;
        }
        let root = workspace_root();
        let expected = ["release", "debug"]
            .into_iter()
            .map(|profile| root.join("target").join(profile).join("conch"))
            .find(|path| path.is_file());
        let Some(expected) = expected else {
            return;
        };
        assert_eq!(find_conch_binary(), Some(expected));
    }
}
