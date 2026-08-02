//! Direct AST-child traversal for parser-owned syntax values.

use crate::component::parse::AstKey;
use crate::component::parse::data::AstBox;

/// Visits the direct AST-box children of one concrete syntax value.
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
        if let Some(value) = self {
            value.ast_children(visitor);
        }
    }
}

impl<T: AstWalkField> AstWalkField for Vec<T> {
    fn ast_children(&self, visitor: &mut dyn FnMut(AstKey)) {
        for value in self {
            value.ast_children(visitor);
        }
    }
}

impl<T: AstWalkField> AstWalkField for Box<T> {
    fn ast_children(&self, visitor: &mut dyn FnMut(AstKey)) {
        self.as_ref().ast_children(visitor);
    }
}
