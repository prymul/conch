fn main() {
    let mut shell = conch_shell_core::Shell::new();
    conch_shell_builtins::register_all(&mut shell);
    println!("conch: Phase 1 REPL pending lexer/parser integration");
}
