#[test]
fn terminal_derive_ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/terminal-pass-scopes.rs");
    t.compile_fail("tests/ui/terminal-reject-*.rs");
}
