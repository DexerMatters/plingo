#[doc(hidden)]
pub mod ast;
#[doc(hidden)]
pub mod green;
pub(crate) mod gss;
#[doc(hidden)]
pub mod product;

#[doc(hidden)]
pub type AstArena = ast::AstArena;
#[doc(hidden)]
pub type AstBox<T> = ast::AstBox<T>;
#[doc(hidden)]
pub type AstToken<T> = ast::AstToken<T>;
#[doc(hidden)]
pub type ErrorKind = green::ErrorKind;
#[doc(hidden)]
pub type GreenId = green::GreenId;
#[doc(hidden)]
pub type GreenTree = green::GreenTree;
#[doc(hidden)]
pub type ParseErrorInfo = green::ParseErrorInfo;
#[doc(hidden)]
pub type TreeArena = green::TreeArena;
#[doc(hidden)]
pub type TreeData = green::TreeData;
#[doc(hidden)]
pub type Product = product::Product;
#[doc(hidden)]
pub type ProductArena = product::ProductArena;
#[doc(hidden)]
pub type ProductData = product::ProductData;
#[doc(hidden)]
pub type ProductId = product::ProductId;
