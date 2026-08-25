#[path = "../examples/stlc/syntax.rs"]
mod syntax;

#[test]
fn syntax_compiles() {
    let _ = std::any::TypeId::of::<syntax::StlcDocument>();
}
