use smallvec::SmallVec;

use super::LexInterrupt;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct State {
    pub id: usize,
    pub key: Option<String>,
}

impl State {
    pub fn new(id: usize) -> Self {
        Self { id, key: None }
    }

    pub fn with_key(id: usize, key: String) -> Self {
        Self {
            id,
            key: Some(key),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StateAction {
    None,
    Enter(State),
    Leave,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LexerState {
    pub offset: usize,
    pub state_stack: SmallVec<[State; 4]>,
}

impl LexerState {
    pub fn new(root: State) -> Self {
        let mut state_stack = SmallVec::new();
        state_stack.push(root);
        Self {
            offset: 0,
            state_stack,
        }
    }

    pub fn current_state(&self) -> Result<State, LexInterrupt> {
        self.state_stack
            .last()
            .cloned()
            .ok_or(LexInterrupt::MissingState)
    }

    pub fn current_key(&self) -> Option<&str> {
        self.state_stack.last().and_then(|s| s.key.as_deref())
    }

    pub fn apply_action(&mut self, action: StateAction) {
        match action {
            StateAction::None => {}
            StateAction::Enter(s) => self.state_stack.push(s),
            StateAction::Leave => {
                self.state_stack.pop();
            }
        }
    }

    pub fn parent_state(&self) -> Option<State> {
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
