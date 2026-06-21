#[test]
fn context_callable_ui() {
    let t = trybuild::TestCases::new();
    t.pass("tests/ui/context_callable/pass_generic_ctx_call.rs");
    t.pass("tests/ui/context_callable/pass_no_await.rs");
    t.pass("tests/ui/context_callable/pass_emit.rs");
    t.compile_fail("tests/ui/context_callable/fail_non_async.rs");
    t.compile_fail("tests/ui/context_callable/fail_wrong_arity.rs");
    t.compile_fail("tests/ui/context_callable/fail_missing_context.rs");
    t.compile_fail("tests/ui/context_callable/fail_return_type.rs");
}
