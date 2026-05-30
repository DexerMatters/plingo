use bitvec::vec::BitVec;

use crate::component::parse::grammar::{Grammar, NonTerminalId, ProductionId, Symbol, TerminalId};

impl Grammar {
    pub fn analyze(&mut self) {
        self.sort_productions_by_lhs();
        self.index_terminals();
        self.compute_nullable_sets();
        self.compute_first_sets();
    }

    fn index_terminals(&mut self) {
        self.terminal_indices = self
            .terminals
            .iter()
            .enumerate()
            .map(|(index, terminal)| (terminal.id, index))
            .collect();
    }

    fn union_nonterminal_first_into(&self, target: &mut BitVec, non_terminal: NonTerminalId) {
        for terminal in self.is_at_first[non_terminal as usize].iter_ones() {
            target.set(terminal, true);
        }
    }

    fn extend_first_from_sequence(&self, target: &mut BitVec, sequence: &[Symbol]) -> bool {
        for symbol in sequence {
            match symbol {
                Symbol::T(id) => {
                    target.set(self.terminal_index(*id), true);
                    return false;
                }
                Symbol::N(id) => {
                    self.union_nonterminal_first_into(target, *id);

                    if !self.is_nullable[*id as usize] {
                        return false;
                    }
                }
                Symbol::Epsilon => {}
            }
        }

        true
    }

    #[inline]
    pub fn production_ids_by_lhs(&self, lhs: NonTerminalId) -> &[ProductionId] {
        let range = &self.productions_for_lhs[lhs as usize];
        &self.production_ids_by_lhs[range.start as usize..range.end as usize]
    }

    #[inline]
    pub fn production_rhs_len(&self, production: ProductionId) -> usize {
        self.productions[production as usize].rhs_len as usize
    }

    #[inline]
    pub fn production_rhs(&self, production: ProductionId) -> &[Symbol] {
        let production = &self.productions[production as usize];
        let start = production.rhs_start as usize;
        let end = start + production.rhs_len as usize;
        &self.rhs_symbols[start..end]
    }

    #[inline]
    pub fn production_lhs(&self, production: ProductionId) -> NonTerminalId {
        self.productions[production as usize].lhs
    }

    pub fn first_of_sequence(&self, sequence: &[Symbol], lookahead: TerminalId) -> BitVec {
        let mut first = bitvec::bitvec![0; self.terminals.len()];

        // Canonical LR(1) needs FIRST(beta a): if every symbol in `sequence`
        // can disappear, the inherited lookahead must also be included.
        if self.extend_first_from_sequence(&mut first, sequence) {
            first.set(self.terminal_index(lookahead), true);
        }
        first
    }

    fn compute_nullable_sets(&mut self) {
        self.is_nullable = bitvec::bitvec![0; self.non_terminals.len()];

        loop {
            let mut changed = false;

            for production in &self.productions {
                let is_nullable =
                    self.production_rhs(production.id)
                        .iter()
                        .all(|symbol| match symbol {
                            Symbol::N(id) => self.is_nullable[*id as usize],
                            Symbol::T(_) => false,
                            Symbol::Epsilon => true,
                        });

                if is_nullable && !self.is_nullable[production.lhs as usize] {
                    self.is_nullable.set(production.lhs as usize, true);
                    changed = true;
                }
            }

            if !changed {
                break;
            }
        }
    }

    fn compute_first_sets(&mut self) {
        self.is_at_first = vec![bitvec::bitvec![0; self.terminals.len()]; self.non_terminals.len()];

        loop {
            let mut changed = false;

            for production in &self.productions {
                let lhs = production.lhs as usize;
                let mut next = self.is_at_first[lhs].clone();
                self.extend_first_from_sequence(&mut next, self.production_rhs(production.id));

                if next != self.is_at_first[lhs] {
                    self.is_at_first[lhs] = next;
                    changed = true;
                }
            }

            if !changed {
                break;
            }
        }
    }

    fn sort_productions_by_lhs(&mut self) {
        self.productions.sort_by_key(|production| production.lhs);

        for (index, production) in self.productions.iter_mut().enumerate() {
            production.id = index as u32;
        }

        self.productions_for_lhs = vec![0..0; self.non_terminals.len()];
        self.production_ids_by_lhs.clear();

        let mut start = 0usize;
        while start < self.productions.len() {
            let lhs = self.productions[start].lhs as usize;
            let mut end = start + 1;
            while end < self.productions.len()
                && self.productions[end].lhs == self.productions[start].lhs
            {
                end += 1;
            }

            let ids_start = self.production_ids_by_lhs.len() as u32;
            self.production_ids_by_lhs.extend(
                self.productions[start..end]
                    .iter()
                    .map(|production| production.id),
            );
            let ids_end = self.production_ids_by_lhs.len() as u32;
            self.productions_for_lhs[lhs] = ids_start..ids_end;
            start = end;
        }
    }
}
