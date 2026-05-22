use smallvec::SmallVec;

use super::LexInterrupt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum State {
    Id(usize),
    IdWithCapture { id: usize, start: usize, end: usize },
}

impl State {
    pub fn id(&self) -> usize {
        match self {
            Self::Id(id) | Self::IdWithCapture { id, .. } => *id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StateAction {
    None,
    Enter(State),
    Leave,
}

#[derive(Debug, Clone)]
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
            .copied()
            .ok_or(LexInterrupt::MissingState)
    }

    pub fn current_capture(&self) -> Option<(usize, usize)> {
        let s = *self.state_stack.last()?;
        match s {
            State::IdWithCapture { start, end, .. } => Some((start, end)),
            State::Id(_) => None,
        }
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
