//! Byte-safe rendering of process output for human-readable diagnostics.
//!
//! `String::from_utf8_lossy` collapses *every* invalid byte (or invalid
//! byte run) into the same U+FFFD replacement character, which means two
//! genuinely different malformed outputs can render identically -- exactly
//! the failure mode this harness must not have, since a mismatch involving
//! non-UTF-8 output is precisely the kind of thing this comparison exists
//! to catch. [`render_bytes`] instead renders each invalid byte as its own
//! `\xHH` escape (a surrogateescape-equivalent, reversible representation,
//! following the same principle brush documents using for its bash
//! upstream-script e2e suite), so distinct invalid sequences stay visibly
//! distinct in a failure report.
//!
//! This module is presentation-only: the actual pass/fail comparison in
//! [`crate::compare`] always operates on raw `&[u8]`, never on a
//! lossily-decoded `String`.

/// Renders `bytes` for display, preserving valid UTF-8 as-is and escaping
/// each invalid byte individually.
pub fn render_bytes(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len());
    for chunk in bytes.utf8_chunks() {
        out.push_str(chunk.valid());
        for &byte in chunk.invalid() {
            out.push_str(&format!("\\x{byte:02x}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_utf8_renders_unchanged() {
        assert_eq!(render_bytes("hello\n".as_bytes()), "hello\n");
    }

    #[test]
    fn invalid_byte_is_rendered_as_a_hex_escape() {
        assert_eq!(render_bytes(&[0xff]), "\\xff");
    }

    #[test]
    fn distinct_invalid_bytes_render_distinctly() {
        // The exact pitfall this module exists to avoid: with
        // `String::from_utf8_lossy` both of these collapse to the same
        // single U+FFFD character and become indistinguishable.
        let a = render_bytes(&[0xff]);
        let b = render_bytes(&[0xfe]);
        assert_ne!(a, b);
        assert_eq!(a, "\\xff");
        assert_eq!(b, "\\xfe");
    }

    #[test]
    fn mixes_valid_and_invalid_runs() {
        let mut bytes = b"before-".to_vec();
        bytes.push(0xff);
        bytes.extend_from_slice(b"-after");
        assert_eq!(render_bytes(&bytes), "before-\\xff-after");
    }
}
