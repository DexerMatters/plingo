#[test]
fn terminal_derive_ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/terminal-pass-scopes.rs");
    t.pass("tests/ui/terminal-pass-empty.rs");
    t.pass("tests/ui/terminal-pass-scope-slots.rs");
    t.compile_fail("tests/ui/terminal-reject-*.rs");
}
