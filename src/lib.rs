//! Core library for the conch shell.
//!
//! This crate is consumed by the `conch` binary ([`main.rs`](../src/main.rs))
//! and exists as its own library target so its public API can be
//! documented and published on [docs.rs](https://docs.rs/conch-shell).

/// Runs conch.
///
/// This is the single entry point the `conch` binary calls; keeping it in
/// the library rather than inline in `main` keeps `main.rs` a thin wrapper
/// and makes this function directly documented, testable, and reusable.
pub fn run() {
    println!("Hello, world!");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_does_not_panic() {
        run();
    }
}
