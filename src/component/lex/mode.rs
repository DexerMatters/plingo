use std::{fmt, hash::Hash};

use smallvec::SmallVec;

use super::{LexInterrupt, LexerRoot, SlotStore};

pub struct State<Root>
where
    Root: LexerRoot,
{
    pub id: usize,
    pub slots: SlotStore<Root>,
}

impl<Root> Clone for State<Root>
where
    Root: LexerRoot,
{
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            slots: self.slots.clone(),
        }
    }
}

impl<Root> PartialEq for State<Root>
where
    Root: LexerRoot,
{
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id && self.slots == other.slots
    }
}

impl<Root> Eq for State<Root> where Root: LexerRoot {}

impl<Root> Hash for State<Root>
where
    Root: LexerRoot,
{
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
        self.slots.hash(state);
    }
}

impl<Root> fmt::Debug for State<Root>
where
    Root: LexerRoot,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("State")
            .field("id", &self.id)
            .field("slots", &self.slots)
            .finish()
    }
}

impl<Root> State<Root>
where
    Root: LexerRoot,
{
    pub fn new(id: usize) -> Self {
        Self {
            id,
            slots: SlotStore::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateAction<Root>
where
    Root: LexerRoot,
{
    None,
    Enter(State<Root>),
    Exit,
}

pub struct LexerState<Root>
where
    Root: LexerRoot,
{
    pub offset: usize,
    pub state_stack: SmallVec<[State<Root>; 4]>,
}

impl<Root> Clone for LexerState<Root>
where
    Root: LexerRoot,
{
    fn clone(&self) -> Self {
        Self {
            offset: self.offset,
            state_stack: self.state_stack.clone(),
        }
    }
}

impl<Root> PartialEq for LexerState<Root>
where
    Root: LexerRoot,
{
    fn eq(&self, other: &Self) -> bool {
        self.offset == other.offset && self.state_stack == other.state_stack
    }
}

impl<Root> Eq for LexerState<Root> where Root: LexerRoot {}

impl<Root> Hash for LexerState<Root>
where
    Root: LexerRoot,
{
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.offset.hash(state);
        self.state_stack.hash(state);
    }
}

impl<Root> fmt::Debug for LexerState<Root>
where
    Root: LexerRoot,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LexerState")
            .field("offset", &self.offset)
            .field("state_stack", &self.state_stack)
            .finish()
    }
}

impl<Root> LexerState<Root>
where
    Root: LexerRoot,
{
    pub fn new(root: State<Root>) -> Self {
        let mut state_stack = SmallVec::new();
        state_stack.push(root);
        Self {
            offset: 0,
            state_stack,
        }
    }

    pub fn current_state(&self) -> Result<State<Root>, LexInterrupt> {
        self.state_stack
            .last()
            .cloned()
            .ok_or(LexInterrupt::MissingState)
    }

    pub fn current_slots(&self) -> Result<&SlotStore<Root>, LexInterrupt> {
        self.state_stack
            .last()
            .map(|state| &state.slots)
            .ok_or(LexInterrupt::MissingState)
    }

    pub fn current_key(&self) -> Option<&str> {
        self.current_slots()
            .ok()
            .and_then(|slots| Root::recover_key(slots))
    }

    pub fn current_slots_mut(&mut self) -> Result<&mut SlotStore<Root>, LexInterrupt> {
        self.state_stack
            .last_mut()
            .map(|state| &mut state.slots)
            .ok_or(LexInterrupt::MissingState)
    }

    pub fn parent_slots(&self) -> Option<&SlotStore<Root>> {
        self.state_stack
            .get(self.state_stack.len().checked_sub(2)?)
            .map(|state| &state.slots)
    }

    pub fn parent_slots_cloned(&self) -> Option<SlotStore<Root>> {
        self.parent_slots().cloned()
    }

    pub fn depth(&self) -> usize {
        self.state_stack.len().saturating_sub(1)
    }

    pub fn apply_action(&mut self, action: StateAction<Root>) {
        match action {
            StateAction::None => {}
            StateAction::Enter(s) => self.state_stack.push(s),
            StateAction::Exit => {
                self.state_stack.pop();
            }
        }
    }

    pub fn parent_state(&self) -> Option<State<Root>> {
        self.state_stack
            .get(self.state_stack.len().checked_sub(2)?)
            .cloned()
    }
}

#[derive(Debug, Clone)]
pub struct StateInfo {
    pub name: &'static str,
    pub type_name: String,
}
