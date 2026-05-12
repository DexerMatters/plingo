use std::marker::PhantomData;

/// Initial state: no layer registered yet; the next registration must be a
/// TopLayer.
pub struct NeedsTop;

/// One or more layers registered; the "open edge" is the output delta type of
/// the most-recently-registered layer. The next registration must accept a
/// delta of the same key / value types.
///
/// `Upper` is the most recently registered concrete layer. `Edge` is the
/// currently open delta shape that the next layer must accept.
pub struct Linked<Upper, Edge>(pub(super) PhantomData<fn() -> (Upper, Edge)>);

/// The pipeline is complete: a BottomLayer has been registered. No further
/// registrations are allowed.
///
/// Only the `Runtime` in this state is complete and runnable.
pub struct Sealed;
