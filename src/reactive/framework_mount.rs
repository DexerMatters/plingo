//! Public workspace mounting contracts used by generated components.

use std::marker::PhantomData;

use super::{Engine, Result};

/// A generated component's mount implementation.
///
/// Implementations are emitted by `#[component]`; application code only
/// supplies the selector and does not install internal runtime graphs.
/// Map-entry mounts return the keyed-family handle so a caller can remove
/// the family later; every other mount yields `()`.
pub trait MountComponent<S>: Sized + 'static {
    /// The removable mount handle (`KeyedFamily<V>` for map entries).
    type Output;
    /// Installs this component at the supplied external selector.
    fn mount(engine: &mut Engine, selector: S) -> Result<Self::Output>;
}

/// Component implementation whose initial value parameters are supplied by
/// the generated mount token. Props belong to the mounted invocation and are
/// not part of its stable key.
pub trait MountComponentWithProps<S, P>: Sized + 'static {
    fn mount_with_props(engine: &mut Engine, selector: S, props: P) -> Result<()>;
}

/// A typed mount request returned by `component::on(selector)`.
#[derive(Clone, Debug)]
pub struct MountToken<C, S> {
    selector: S,
    marker: PhantomData<fn() -> C>,
}

impl<C, S> MountToken<C, S> {
    pub fn new(selector: S) -> Self {
        Self {
            selector,
            marker: PhantomData,
        }
    }
}

/// A typed mount request returned by `component::on(selector, props...)`.
#[derive(Clone, Debug)]
pub struct MountTokenWithProps<C, S, P> {
    selector: S,
    props: P,
    marker: PhantomData<fn() -> C>,
}

impl<C, S, P> MountTokenWithProps<C, S, P> {
    pub fn new(selector: S, props: P) -> Self {
        Self {
            selector,
            props,
            marker: PhantomData,
        }
    }
}

impl<C, S> MountComponent<MountToken<C, S>> for C
where
    C: MountComponent<S>,
    S: 'static,
{
    type Output = C::Output;

    fn mount(engine: &mut Engine, request: MountToken<C, S>) -> Result<Self::Output> {
        C::mount(engine, request.selector)
    }
}

impl<C, S, P> MountComponent<MountTokenWithProps<C, S, P>> for C
where
    C: MountComponentWithProps<S, P>,
    S: 'static,
    P: 'static,
{
    type Output = ();

    fn mount(engine: &mut Engine, request: MountTokenWithProps<C, S, P>) -> Result<()> {
        C::mount_with_props(engine, request.selector, request.props)
    }
}

/// Mount implementation for a tree whose output family uses a different
/// domain from the selector.  The projection is supplied by the builder and
/// is pure application data, not a reactive effect.
pub trait MountComponentWithDomain<S, D>: Sized + 'static {
    /// Installs this component and assigns its output roots to `domain`.
    fn mount_with_domain(engine: &mut Engine, selector: S, domain: D) -> Result<()>;
}

/// Selector for one component instance per present map entry.
#[derive(Clone, Copy, Debug, Default)]
pub struct MapEntries<V: ?Sized>(PhantomData<fn() -> V>);

impl<V: ?Sized> MapEntries<V> {
    /// Creates a map-entry selector.
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}

/// Selector for one component instance per committed box cell.
#[derive(Clone, Copy, Debug, Default)]
pub struct BoxCell<V: ?Sized>(PhantomData<fn() -> V>);

impl<V: ?Sized> BoxCell<V> {
    /// Creates a box-cell selector.
    pub const fn new() -> Self {
        Self(PhantomData)
    }
}
