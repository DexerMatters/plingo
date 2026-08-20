//! Slot storage and the hook contexts exposed to declarative lexer states.

use std::{fmt, hash::Hash, marker::PhantomData};

use super::{LexMoment, LexerRoot};

pub struct Slot<Root, T>
where
    Root: LexerRoot,
    T: Clone + Eq + Hash + Send + Sync + 'static,
{
    index: usize,
    pack: fn(T) -> Root::SlotValue,
    as_ref: for<'a> fn(&'a Root::SlotValue) -> Option<&'a T>,
    _root: PhantomData<fn() -> Root>,
}

impl<Root, T> Copy for Slot<Root, T>
where
    Root: LexerRoot,
    T: Clone + Eq + Hash + Send + Sync + 'static,
{
}

impl<Root, T> Clone for Slot<Root, T>
where
    Root: LexerRoot,
    T: Clone + Eq + Hash + Send + Sync + 'static,
{
    fn clone(&self) -> Self {
        *self
    }
}

impl<Root, T> Slot<Root, T>
where
    Root: LexerRoot,
    T: Clone + Eq + Hash + Send + Sync + 'static,
{
    pub const fn new(
        index: usize,
        pack: fn(T) -> Root::SlotValue,
        as_ref: for<'a> fn(&'a Root::SlotValue) -> Option<&'a T>,
    ) -> Self {
        Self {
            index,
            pack,
            as_ref,
            _root: PhantomData,
        }
    }

    pub const fn index(self) -> usize {
        self.index
    }
}

pub struct SlotStore<Root>
where
    Root: LexerRoot,
{
    values: Vec<Option<Root::SlotValue>>,
}

impl<Root> Clone for SlotStore<Root>
where
    Root: LexerRoot,
{
    fn clone(&self) -> Self {
        Self {
            values: self.values.clone(),
        }
    }
}

impl<Root> PartialEq for SlotStore<Root>
where
    Root: LexerRoot,
{
    fn eq(&self, other: &Self) -> bool {
        self.values == other.values
    }
}

impl<Root> Eq for SlotStore<Root> where Root: LexerRoot {}

impl<Root> Hash for SlotStore<Root>
where
    Root: LexerRoot,
{
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.values.hash(state);
    }
}

impl<Root> fmt::Debug for SlotStore<Root>
where
    Root: LexerRoot,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SlotStore")
            .field("len", &self.values.len())
            .finish()
    }
}

impl<Root> Default for SlotStore<Root>
where
    Root: LexerRoot,
{
    fn default() -> Self {
        Self {
            values: (0..Root::slot_count()).map(|_| None).collect(),
        }
    }
}

impl<Root> SlotStore<Root>
where
    Root: LexerRoot,
{
    pub fn get<T>(&self, slot: Slot<Root, T>) -> Option<&T>
    where
        T: Clone + Eq + Hash + Send + Sync + 'static,
    {
        self.values
            .get(slot.index)
            .and_then(|value| value.as_ref())
            .and_then(|value| (slot.as_ref)(value))
    }

    pub fn set<T>(&mut self, slot: Slot<Root, T>, value: T)
    where
        T: Clone + Eq + Hash + Send + Sync + 'static,
    {
        if let Some(entry) = self.values.get_mut(slot.index) {
            *entry = Some((slot.pack)(value));
        }
    }

    pub fn remove<T>(&mut self, slot: Slot<Root, T>)
    where
        T: Clone + Eq + Hash + Send + Sync + 'static,
    {
        if let Some(entry) = self.values.get_mut(slot.index) {
            *entry = None;
        }
    }
}

pub struct WhenCx<'a, Root>
where
    Root: LexerRoot,
{
    lexeme: &'a str,
    moment: LexMoment,
    depth: usize,
    current: &'a SlotStore<Root>,
    parent: Option<&'a SlotStore<Root>>,
}

impl<'a, Root> WhenCx<'a, Root>
where
    Root: LexerRoot,
{
    pub fn new(
        lexeme: &'a str,
        moment: LexMoment,
        depth: usize,
        current: &'a SlotStore<Root>,
        parent: Option<&'a SlotStore<Root>>,
    ) -> Self {
        Self {
            lexeme,
            moment,
            depth,
            current,
            parent,
        }
    }

    pub fn lexeme(&self) -> &str {
        self.lexeme
    }

    pub fn moment(&self) -> LexMoment {
        self.moment
    }

    pub fn depth(&self) -> usize {
        self.depth
    }

    pub fn get<T>(&self, slot: Slot<Root, T>) -> Option<&T>
    where
        T: Clone + Eq + Hash + Send + Sync + 'static,
    {
        self.current.get(slot)
    }

    pub fn parent_get<T>(&self, slot: Slot<Root, T>) -> Option<&T>
    where
        T: Clone + Eq + Hash + Send + Sync + 'static,
    {
        self.parent.and_then(|parent| parent.get(slot))
    }
}

pub struct WithCx<'a, Root>
where
    Root: LexerRoot,
{
    lexeme: &'a str,
    moment: LexMoment,
    depth: usize,
    target: &'a mut SlotStore<Root>,
    source: SlotStore<Root>,
    parent: Option<SlotStore<Root>>,
}

impl<'a, Root> WithCx<'a, Root>
where
    Root: LexerRoot,
{
    pub fn new(
        lexeme: &'a str,
        moment: LexMoment,
        depth: usize,
        target: &'a mut SlotStore<Root>,
        source: SlotStore<Root>,
        parent: Option<SlotStore<Root>>,
    ) -> Self {
        Self {
            lexeme,
            moment,
            depth,
            target,
            source,
            parent,
        }
    }

    pub fn lexeme(&self) -> &str {
        self.lexeme
    }

    pub fn moment(&self) -> LexMoment {
        self.moment
    }

    pub fn depth(&self) -> usize {
        self.depth
    }

    pub fn get<T>(&self, slot: Slot<Root, T>) -> Option<&T>
    where
        T: Clone + Eq + Hash + Send + Sync + 'static,
    {
        self.target.get(slot)
    }

    pub fn set<T>(&mut self, slot: Slot<Root, T>, value: T)
    where
        T: Clone + Eq + Hash + Send + Sync + 'static,
    {
        self.target.set(slot, value);
    }

    pub fn remove<T>(&mut self, slot: Slot<Root, T>)
    where
        T: Clone + Eq + Hash + Send + Sync + 'static,
    {
        self.target.remove(slot);
    }

    pub fn source_get<T>(&self, slot: Slot<Root, T>) -> Option<&T>
    where
        T: Clone + Eq + Hash + Send + Sync + 'static,
    {
        self.source.get(slot)
    }

    pub fn parent_get<T>(&self, slot: Slot<Root, T>) -> Option<&T>
    where
        T: Clone + Eq + Hash + Send + Sync + 'static,
    {
        self.parent.as_ref().and_then(|parent| parent.get(slot))
    }
}
