use smallvec::SmallVec;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StateId(pub usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateAction {
    None,
    Enter(StateId),
    Leave,
}

#[derive(Debug, Clone)]
pub struct LexerState {
    pub offset: usize,
    pub state_stack: SmallVec<[StateId; 4]>,
}

impl LexerState {
    pub fn new(root: StateId) -> Self {
        let mut state_stack = SmallVec::new();
        state_stack.push(root);
        Self {
            offset: 0,
            state_stack,
        }
    }

    pub fn current_state(&self) -> Option<StateId> {
        self.state_stack.last().copied()
    }

    pub fn apply_action(&mut self, action: StateAction) {
        match action {
            StateAction::None => {}
            StateAction::Enter(state_id) => self.state_stack.push(state_id),
            StateAction::Leave => {
                self.state_stack.pop();
            }
        }
    }
}

#[derive(Debug, Clone)]
pub struct StateInfo {
    pub name: &'static str,
    pub type_name: &'static str,
}
