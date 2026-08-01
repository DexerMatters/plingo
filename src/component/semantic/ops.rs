//! Rule-facing AST traversal helpers.

use crate::component::parse::{AstKey, data::AstBox};

pub trait AstWalk {
    fn direct_children(&self, visitor: &mut dyn FnMut(AstKey));
}
#[doc(hidden)]
pub trait AstWalkField {
    fn ast_children(&self, visitor: &mut dyn FnMut(AstKey));
}
impl<T> AstWalkField for AstBox<T> {
    fn ast_children(&self, visitor: &mut dyn FnMut(AstKey)) {
        visitor(self.key());
    }
}
impl<T: AstWalkField> AstWalkField for Option<T> {
    fn ast_children(&self, visitor: &mut dyn FnMut(AstKey)) {
        if let Some(v) = self {
            v.ast_children(visitor);
        }
    }
}
impl<T: AstWalkField> AstWalkField for Vec<T> {
    fn ast_children(&self, visitor: &mut dyn FnMut(AstKey)) {
        for v in self {
            v.ast_children(visitor);
        }
    }
}
impl<T: AstWalkField> AstWalkField for Box<T> {
    fn ast_children(&self, visitor: &mut dyn FnMut(AstKey)) {
        self.as_ref().ast_children(visitor);
    }
}
