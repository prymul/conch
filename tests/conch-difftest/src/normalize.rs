//! Byte-level normalization applied identically to both sides of a
//! comparison before diffing.
//!
//! Phase 1's corpus only needs [`NormalizeRule::Workdir`] (every run gets
//! its own fresh temp directory, so `pwd`-shaped output never matches
//! literally even when it's semantically correct) and
//! [`NormalizeRule::TrailingNewline`]. Non-determinism that later phases
//! will hit -- `$$`/`$!` PIDs once job control (Phase 4) lands, `$RANDOM`,
//! hostnames, TTY-dependent formatting -- is exactly this same mechanism:
//! add a variant here, plus (for anything that isn't a literal known
//! string like the workdir is) a masking rule backed by a small pattern
//! match rather than pulling in a full regex engine for one or two fixed
//! shapes. Keep normalization explicit and opt-in per case rather than
//! applied globally, so a case's `normalize` list documents exactly which
//! kind of non-determinism it's accounting for.

use std::path::Path;

use crate::case::NormalizeRule;

/// Per-run data a normalization rule may need (e.g. the exact temp
/// directory a particular invocation ran in). Both sides of a comparison
/// get normalized with their *own* context -- that's the whole point of
/// [`NormalizeRule::Workdir`].
pub struct NormalizeContext<'a> {
    pub workdir: &'a Path,
}

pub fn apply(rules: &[NormalizeRule], bytes: &[u8], ctx: &NormalizeContext<'_>) -> Vec<u8> {
    let mut current = bytes.to_vec();
    for rule in rules {
        current = apply_one(*rule, &current, ctx);
    }
    current
}

fn apply_one(rule: NormalizeRule, bytes: &[u8], ctx: &NormalizeContext<'_>) -> Vec<u8> {
    match rule {
        NormalizeRule::TrailingNewline => strip_single_trailing_newline(bytes),
        NormalizeRule::Workdir => replace_workdir(bytes, ctx.workdir),
    }
}

fn strip_single_trailing_newline(bytes: &[u8]) -> Vec<u8> {
    match bytes.last() {
        Some(b'\n') => bytes[..bytes.len() - 1].to_vec(),
        _ => bytes.to_vec(),
    }
}

fn replace_workdir(bytes: &[u8], workdir: &Path) -> Vec<u8> {
    let mut out = bytes.to_vec();
    for needle in workdir_needles(workdir) {
        out = replace_bytes(&out, needle.as_bytes(), b"<CWD>");
    }
    out
}

/// The string(s) a shell's `pwd`/`$PWD` might plausibly emit for
/// `workdir`. On platforms where the temp directory lives under a
/// symlink (notably macOS's `/tmp` -> `/private/tmp`), a real shell's
/// `getcwd()`-backed `pwd` reports the *canonicalized* path even though
/// `Command::current_dir` was given the symlinked one -- so both forms
/// need to be tried, canonical first since it's what `pwd` actually
/// prints in that case.
fn workdir_needles(workdir: &Path) -> Vec<String> {
    // `to_string_lossy` is fine here: temp directory paths are created by
    // this harness (via `tempfile`) and are always plain ASCII in
    // practice. Any lossy/failed substitution just leaves the
    // placeholder unsubstituted, which surfaces as an honest comparison
    // failure rather than a silent miscompare.
    let mut needles = Vec::new();
    if let Ok(canonical) = workdir.canonicalize() {
        let canonical = canonical.to_string_lossy().into_owned();
        if !canonical.is_empty() {
            needles.push(canonical);
        }
    }
    let literal = workdir.to_string_lossy().into_owned();
    if !literal.is_empty() && !needles.contains(&literal) {
        needles.push(literal);
    }
    needles
}

fn replace_bytes(haystack: &[u8], needle: &[u8], replacement: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i..].starts_with(needle) {
            out.extend_from_slice(replacement);
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trailing_newline_rule_strips_exactly_one_newline() {
        let ctx = NormalizeContext {
            workdir: Path::new(""),
        };
        let normalized = apply(&[NormalizeRule::TrailingNewline], b"hello\n\n", &ctx);
        assert_eq!(normalized, b"hello\n");
    }

    #[test]
    fn trailing_newline_rule_is_a_no_op_without_a_trailing_newline() {
        let ctx = NormalizeContext {
            workdir: Path::new(""),
        };
        let normalized = apply(&[NormalizeRule::TrailingNewline], b"hello", &ctx);
        assert_eq!(normalized, b"hello");
    }

    #[test]
    fn workdir_rule_replaces_every_occurrence() {
        let workdir = Path::new("/tmp/conch-abc123");
        let ctx = NormalizeContext { workdir };
        let input = b"/tmp/conch-abc123/sub\nnested: /tmp/conch-abc123/sub/deeper\n";
        let normalized = apply(&[NormalizeRule::Workdir], input, &ctx);
        assert_eq!(normalized, b"<CWD>/sub\nnested: <CWD>/sub/deeper\n");
    }

    #[cfg(unix)]
    #[test]
    fn workdir_rule_also_substitutes_the_canonicalized_form() {
        // Mirrors macOS's `/tmp` -> `/private/tmp` symlink: `workdir` is
        // the symlinked path handed to `Command::current_dir`, but a real
        // shell's `pwd` reports the canonicalized path instead.
        let real_dir = tempfile::tempdir().unwrap();
        let link_path = real_dir
            .path()
            .parent()
            .unwrap()
            .join(format!("conch-difftest-symlink-{}", std::process::id()));
        std::os::unix::fs::symlink(real_dir.path(), &link_path).unwrap();

        let ctx = NormalizeContext {
            workdir: &link_path,
        };
        let canonical = real_dir.path().canonicalize().unwrap();
        let input = format!("{}\n", canonical.display());
        let normalized = apply(&[NormalizeRule::Workdir], input.as_bytes(), &ctx);

        std::fs::remove_file(&link_path).unwrap();

        assert_eq!(normalized, b"<CWD>\n");
    }

    #[test]
    fn rules_compose_in_order() {
        let workdir = Path::new("/tmp/conch-abc123");
        let ctx = NormalizeContext { workdir };
        let input = b"/tmp/conch-abc123\n";
        let normalized = apply(
            &[NormalizeRule::Workdir, NormalizeRule::TrailingNewline],
            input,
            &ctx,
        );
        assert_eq!(normalized, b"<CWD>");
    }
}
