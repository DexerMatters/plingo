use std::{
    collections::{BTreeSet, HashMap, VecDeque},
    marker::PhantomData,
};

use indexmap::{IndexMap, IndexSet};
use smallvec::SmallVec;

use crate::framework::parse::{
    Parser, ParserConfig, ParserSnapshotState,
    grammar::{Grammar, NonTerminalId, ProductionId, Symbol, TerminalId},
};

pub type LRStateId = usize;

#[derive(Copy, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct LR1Item {
    pub production: ProductionId,
    pub dot: usize,
    pub lookahead: TerminalId,
}

impl LR1Item {
    pub fn move_dot(&self, offset: isize) -> Self {
        Self {
            production: self.production,
            dot: (self.dot as isize + offset) as usize,
            lookahead: self.lookahead,
        }
    }
}

type StateKey = Vec<LR1Item>;

#[derive(Clone)]
pub struct LR1State {
    pub id: LRStateId,
    pub items: Vec<LR1Item>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Shift(LRStateId),
    Reduce(ProductionId),
    Accept,
    Error,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Conflict {
    ShiftReduce {
        state: LRStateId,
        terminal: TerminalId,
        shift: LRStateId,
        reduce: ProductionId,
    },
    ReduceReduce {
        state: LRStateId,
        terminal: TerminalId,
        reduces: Vec<ProductionId>,
    },
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActionSet {
    pub inner: SmallVec<[Action; 2]>,
}

impl ActionSet {
    fn push(&mut self, action: Action) {
        if !self.inner.contains(&action) {
            self.inner.push(action);
        }
    }

    pub fn has_shift(&self) -> bool {
        self.inner.iter().any(|a| matches!(a, Action::Shift(_)))
    }
}

impl Grammar {
    pub fn build_lr1<Root>(&self) -> Parser<Root> {
        self.build_lr1_with_config::<Root>(ParserConfig::default())
    }

    pub fn build_lr1_with_config<Root>(&self, config: ParserConfig) -> Parser<Root> {
        let mut states = Vec::new();
        let mut state_keys = IndexSet::new();
        let mut queue = VecDeque::new();
        let mut transitions = IndexMap::new();

        let start_state = self.closure([LR1Item {
            production: 0,
            dot: 0,
            lookahead: self.eof,
        }]);
        let (start_id, _) = intern_state(&mut states, &mut state_keys, start_state);
        queue.push_back(start_id);

        while let Some(state_id) = queue.pop_front() {
            let symbols = self.transition_symbols(&states[state_id].items);
            for symbol in symbols {
                let next_items = self.goto(states[state_id].items.iter().copied(), symbol);
                if next_items.is_empty() {
                    continue;
                }
                let (next_id, is_new) = intern_state(&mut states, &mut state_keys, next_items);
                if is_new {
                    queue.push_back(next_id);
                }
                transitions.insert((state_id, symbol), next_id);
            }
        }

        let mut runtime: Parser<Root> = Parser {
            grammar: self.clone(),
            actions: vec![ActionSet::default(); states.len() * self.terminal_count()],
            gotos: vec![None; states.len() * self.non_terminals.len()],
            conflicts: Vec::new(),
            transitions,
            states,
            session_arenas: HashMap::new(),
            config,
            latest: ParserSnapshotState::default().into(),
            next_snapshot: 0,
            _root: PhantomData,
        };

        self.fill_tables(&mut runtime);
        runtime
    }

    pub fn symbol_after_dot(&self, item: &LR1Item) -> Option<Symbol> {
        self.production_rhs(item.production).get(item.dot).copied()
    }

    pub fn suffix_after_dot(&self, item: &LR1Item) -> &[Symbol] {
        self.production_rhs(item.production)
            .get(item.dot + 1..)
            .unwrap_or_default()
    }

    pub fn closure(&self, seed: impl IntoIterator<Item = LR1Item>) -> Vec<LR1Item> {
        let mut result: IndexSet<LR1Item> = seed.into_iter().collect();

        let mut i = 0;
        while i < result.len() {
            let item = result[i];
            let Some(Symbol::N(b)) = self.symbol_after_dot(&item) else {
                i += 1;
                continue;
            };
            let beta = self.suffix_after_dot(&item);
            let lookaheads = self.first_of_sequence(beta, item.lookahead);
            for &prod in self.production_ids_by_lhs(b) {
                for lookahead in lookaheads.iter_ones() {
                    let new_item = LR1Item {
                        production: prod,
                        dot: 0,
                        lookahead: self.terminal_at(lookahead),
                    };
                    result.insert(new_item);
                }
            }
            i += 1;
        }

        result.sort();
        result.into_iter().collect()
    }

    pub fn goto(&self, from: impl Iterator<Item = LR1Item>, symbol: Symbol) -> Vec<LR1Item> {
        let moved = from.filter_map(|item| {
            if self.symbol_after_dot(&item) == Some(symbol) {
                Some(item.move_dot(1))
            } else {
                None
            }
        });

        self.closure(moved)
    }

    fn transition_symbols(&self, items: &[LR1Item]) -> BTreeSet<Symbol> {
        items
            .iter()
            .filter_map(|item| self.symbol_after_dot(item))
            .collect()
    }

    fn fill_tables<Root>(&self, runtime: &mut Parser<Root>) {
        let transitions = runtime
            .transitions
            .iter()
            .map(|(&(from, symbol), &to)| (from, symbol, to))
            .collect::<Vec<_>>();
        for (from, symbol, to) in transitions {
            match symbol {
                Symbol::T(terminal) => runtime
                    .action_set_mut(from, terminal)
                    .push(Action::Shift(to)),
                Symbol::N(non_terminal) => runtime.set_goto(from, non_terminal, to),
                Symbol::Epsilon => {}
            }
        }

        let completed_items = runtime
            .states
            .iter()
            .map(|state| (state.id, state.items.clone()))
            .collect::<Vec<_>>();
        for (state_id, items) in completed_items {
            for item in &items {
                if self.symbol_after_dot(item).is_some() {
                    continue;
                }

                if item.production == 0 {
                    runtime
                        .action_set_mut(state_id, self.eof)
                        .push(Action::Accept);
                } else {
                    runtime
                        .action_set_mut(state_id, item.lookahead)
                        .push(Action::Reduce(item.production));
                }
            }
        }

        runtime.collect_conflicts();
    }
}

impl<Root> Parser<Root> {
    pub fn action_set(&self, state: LRStateId, terminal: TerminalId) -> &ActionSet {
        &self.actions[self.grammar.action_index(state, terminal)]
    }

    pub fn goto_state(&self, state: LRStateId, non_terminal: NonTerminalId) -> Option<LRStateId> {
        self.gotos[self.grammar.goto_index(state, non_terminal)]
    }

    pub fn shift_terminals_for_state(&self, state: LRStateId) -> Vec<TerminalId> {
        let count = self.grammar.terminal_count();
        let mut result = Vec::new();
        for ti in 0..count {
            let terminal = self.grammar.terminal_at(ti);
            let actions = &self.actions[self.grammar.action_index(state, terminal)];
            if actions.has_shift() {
                result.push(terminal);
            }
        }
        result
    }

    fn action_set_mut(&mut self, state: LRStateId, terminal: TerminalId) -> &mut ActionSet {
        let i = self.grammar.action_index(state, terminal);
        &mut self.actions[i]
    }

    fn set_goto(&mut self, state: LRStateId, non_terminal: NonTerminalId, next: LRStateId) {
        self.gotos[self.grammar.goto_index(state, non_terminal)] = Some(next);
    }

    fn collect_conflicts(&mut self) {
        self.conflicts.clear();

        let terminal_count = self.grammar.terminal_count();
        for state in 0..self.states.len() {
            for terminal in 0..terminal_count {
                let actions = &self.actions[self
                    .grammar
                    .action_index(state, self.grammar.terminal_at(terminal))]
                .inner;
                if actions.len() <= 1 {
                    continue;
                }

                let mut shift = None;
                let mut reduces = Vec::new();
                for action in actions {
                    match action {
                        Action::Shift(target) => shift = Some(*target),
                        Action::Reduce(production) => reduces.push(*production),
                        Action::Accept | Action::Error => {}
                    }
                }

                if let Some(shift) = shift {
                    for reduce in &reduces {
                        self.conflicts.push(Conflict::ShiftReduce {
                            state,
                            terminal: self.grammar.terminal_at(terminal),
                            shift,
                            reduce: *reduce,
                        });
                    }
                }

                if reduces.len() > 1 {
                    self.conflicts.push(Conflict::ReduceReduce {
                        state,
                        terminal: self.grammar.terminal_at(terminal),
                        reduces,
                    });
                }
            }
        }
    }
}

fn intern_state(
    states: &mut Vec<LR1State>,
    state_keys: &mut IndexSet<StateKey>,
    items: Vec<LR1Item>,
) -> (LRStateId, bool) {
    if let Some((id, _)) = state_keys.get_full(&items) {
        return (id, false);
    }

    let id = states.len();
    state_keys.insert(items.clone());
    states.push(LR1State { id, items });
    (id, true)
}
