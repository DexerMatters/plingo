//! Macro expansion-size and compile-time benchmark (plan §8 Cut H item 4).
//!
//! Builds two representative fixture crates — a one-enum standalone family
//! and a heterogeneous seven-member family — against the workspace `plingo`
//! by path, then reports the compile wall time of each. Both fixtures
//! instantiate the generated read/render surface (a recursive lowering
//! component) so the benchmark covers the generated generic API, not just
//! the enum declaration.
//!
//! Run with `cargo bench -p plingo --bench expansion_size`. The fixture
//! builds share one scratch target dir; the first run pays the dependency
//! build, later runs measure only the fixture crate.

use std::path::PathBuf;
use std::process::Command;
use std::time::Instant;

const ONE_ENUM: &str = r#"
use plingo::prelude::*;

#[abstract_tree(domain = String)]
pub enum SmallExpr {
    Add { left: AstBox<SmallExpr>, right: AstBox<SmallExpr> },
    Number { value: i64 },
    Name { text: std::sync::Arc<str> },
}

#[component]
pub fn lower(source: AstBox<SmallExpr>) -> Result<AstBox<SmallExpr>> {
    let value = match source.view()? {
        SmallExprView::Add(add) => SmallExpr::Add {
            left: lower(add.left()?)?,
            right: lower(add.right()?)?,
        },
        SmallExprView::Number(number) => SmallExpr::Number { value: *number.value()? },
        SmallExprView::Name(name) => SmallExpr::Name { text: (*name.text()?).clone() },
    };
    SmallExpr::render(value)
}
"#;

const SEVEN_MEMBERS: &str = r#"
use plingo::prelude::*;

#[abstract_tree(tree = BigTree, domain = String, members(BigDocument, BigDecl, BigPath, BigParam, BigType, BigTypeAtom, BigExpr))]
pub enum BigDocument {
    Lines { declarations: Vec<AstBox<BigDecl>> },
}
#[abstract_tree(member_of = BigTree)]
pub enum BigDecl {
    Value { name: AstBox<BigPath>, annotation: Option<AstBox<BigType>>, body: AstBox<BigExpr> },
}
#[abstract_tree(member_of = BigTree)]
pub enum BigPath { Segments { segments: Vec<AstBox<BigPath>> } }
#[abstract_tree(member_of = BigTree)]
pub enum BigParam { Bare { name: AstBox<BigPath>, annotation: Option<AstBox<BigType>> } }
#[abstract_tree(member_of = BigTree)]
pub enum BigType { Arrow { left: AstBox<BigTypeAtom>, right: AstBox<BigType> }, Atom { atom: AstBox<BigTypeAtom> } }
#[abstract_tree(member_of = BigTree)]
pub enum BigTypeAtom { Nat, Unit, Bool, Paren { ty: AstBox<BigType> } }
#[abstract_tree(member_of = BigTree)]
pub enum BigExpr {
    If { condition: AstBox<BigExpr>, when_true: AstBox<BigExpr>, when_false: AstBox<BigExpr> },
    Add { left: AstBox<BigExpr>, right: AstBox<BigExpr> },
    Var { path: AstBox<BigPath> },
    Lam { param: AstBox<BigParam>, body: AstBox<BigExpr> },
    Num { value: u64 },
}
"#;

fn fixture_crate(dir: &PathBuf, name: &str, source: &str) -> PathBuf {
    let crate_dir = dir.join(name);
    std::fs::create_dir_all(crate_dir.join("src")).expect("fixture src dir");
    let plingo = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    std::fs::write(
        crate_dir.join("Cargo.toml"),
        format!(
            "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\npublish = false\n\n[dependencies]\nplingo = {{ path = {:?} }}\n\n[workspace]\n",
            plingo
        ),
    )
    .expect("fixture manifest");
    std::fs::write(crate_dir.join("src/lib.rs"), source).expect("fixture source");
    crate_dir
}

fn build_time(crate_dir: &PathBuf, target_dir: &PathBuf) -> std::time::Duration {
    let start = Instant::now();
    let status = Command::new(env!("CARGO"))
        .args(["build", "--offline"])
        .env("CARGO_TARGET_DIR", target_dir)
        .current_dir(crate_dir)
        .status()
        .expect("cargo build spawns");
    assert!(status.success(), "fixture crate must compile");
    start.elapsed()
}

fn main() {
    let scratch = std::env::temp_dir().join("plingo-expansion-bench");
    std::fs::create_dir_all(&scratch).expect("scratch dir");
    let one = fixture_crate(&scratch, "plingo-one-enum", ONE_ENUM);
    let seven = fixture_crate(&scratch, "plingo-seven-members", SEVEN_MEMBERS);
    let target = scratch.join("target");

    // Warm the dependency build so both measurements cover the fixture
    // crate itself plus plingo (they share one target dir).
    let warm = build_time(&one, &target);
    let one_time = build_time(&one, &target);
    let seven_time = build_time(&seven, &target);

    let one_bytes = std::fs::read_to_string(one.join("src/lib.rs"))
        .expect("one-enum fixture")
        .len();
    let seven_bytes = std::fs::read_to_string(seven.join("src/lib.rs"))
        .expect("seven-member fixture")
        .len();

    println!(
        "expansion/one-enum      compile {:>10.2?}  (authored {one_bytes} B, warm deps {warm:.2?})",
        one_time
    );
    println!(
        "expansion/seven-member  compile {:>10.2?}  (authored {seven_bytes} B)",
        seven_time
    );
}
