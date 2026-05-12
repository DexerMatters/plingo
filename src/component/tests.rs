use enum_iterator::Sequence;

#[test]
fn test_regex() {
    let hir = regex_syntax::parse(r"^[^]").unwrap();
    let props = hir.properties();
    println!("Properties: {:#?}", props);
}
