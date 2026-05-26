use std::marker::PhantomData;

use bitvec::vec::BitVec;

type SymbolId = u32;
type TerminalId = u32;
type NonTerminalId = u32;
type ProductionId = u32;
type TerminalStateId = u32;
type TerminalTokenId = u32;

type BuildFn = PhantomData<()>; // TODO

#[repr(u8)]
#[derive(Copy, Clone)]
pub enum Symbol {
    T(TerminalId),
    N(NonTerminalId),
}

#[repr(u8)]
#[derive(Copy, Clone)]
pub enum Associativity {
    Left,
    Right,
}

pub struct Precedence {
    pub level: u8,
    pub assoc: Associativity,
}
pub struct NonTerminal {
    pub label: &'static str,
    pub build: BuildFn,
}

pub struct Terminal {
    pub state_id: TerminalStateId,
    pub token_id: TerminalTokenId,
    pub precedence: Option<Precedence>,
}

pub struct Production {
    pub lhs: NonTerminalId,
    pub rhs_start: u32,
    pub rhs_len: u16,
    pub precedence: Option<Precedence>,
}

#[allow(dead_code)]
pub struct Grammar {
    pub(crate) terminals: Vec<Terminal>,
    pub(crate) non_terminals: Vec<NonTerminal>,
    pub(crate) productions: Vec<Production>,
    pub(crate) rhs_symbols: Vec<Symbol>,
    pub(crate) start: NonTerminalId,
    pub(crate) augmented_start: NonTerminalId,
    pub(crate) productions_for_lhs: Vec<std::ops::Range<u32>>,
    pub(crate) eof: TerminalId,

    // analysis
    pub(crate) is_nullable: BitVec,
    pub(crate) first_sets: Vec<BitVec>,
}
