use std::{
    cmp::Ordering,
    collections::{BinaryHeap, HashMap, HashSet, VecDeque},
    sync::Arc,
    time::Instant,
};

use crate::component::parse::{
    build::{Action, ActionSet},
    grammar::{Grammar, TerminalId},
    parsing::{ParseToken, SessionContext},
    recovery::{RecoveryError, RecoveryResult, Repair},
};

const MIN_REAL_SHIFTS: usize = 1;

#[derive(Debug, Clone)]
struct SearchConfig {
    stack: Vec<usize>,
    input: usize,
    repairs: Vec<Repair>,
    score: SearchScore,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct SearchKey {
    stack: Vec<usize>,
    input: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct SearchScore {
    deletes: usize,
    error_shifts: usize,
    shifts: usize,
    inserts: usize,
}

impl SearchScore {
    fn dominates(self, other: Self) -> bool {
        self.error_shifts > other.error_shifts
            || (self.error_shifts == other.error_shifts && self.shifts > other.shifts)
            || (self.error_shifts == other.error_shifts
                && self.shifts == other.shifts
                && self.deletes < other.deletes)
            || (self.error_shifts == other.error_shifts
                && self.shifts == other.shifts
                && self.deletes == other.deletes
                && self.inserts < other.inserts)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SearchRecord {
    cost: usize,
    score: SearchScore,
}

impl SearchRecord {
    fn dominates(self, other: Self) -> bool {
        self.cost < other.cost || (self.cost == other.cost && self.score.dominates(other.score))
    }
}

#[derive(Debug, Clone)]
struct QueueItem {
    cost: usize,
    order: usize,
    config: SearchConfig,
}

impl PartialEq for QueueItem {
    fn eq(&self, other: &Self) -> bool {
        self.cost == other.cost && self.order == other.order
    }
}

impl Eq for QueueItem {}

impl PartialOrd for QueueItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for QueueItem {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .cost
            .cmp(&self.cost)
            .then_with(|| other.order.cmp(&self.order))
    }
}

#[derive(Debug, Clone)]
struct ClosedStack {
    stack: Vec<usize>,
    accepted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ClosedStackKey {
    stack: Vec<usize>,
    lookahead: TerminalId,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ShiftSuffixKey {
    stack: Vec<usize>,
    input: usize,
    remaining_shifts: usize,
}

#[derive(Default)]
struct RecoverySearchCache {
    closed_stacks: HashMap<ClosedStackKey, Arc<[ClosedStack]>>,
    insert_terminals: HashMap<Vec<usize>, Arc<[TerminalId]>>,
    can_shift_suffix: HashMap<ShiftSuffixKey, bool>,
}

pub(super) fn find_recovery(
    ctx: &SessionContext<'_>,
    column: usize,
    tokens: &[ParseToken],
    timeout: std::time::Duration,
) -> Result<Option<RecoveryResult>, RecoveryError> {
    let start = Instant::now();
    let stacks = active_stack_paths(ctx, column);
    if stacks.is_empty() || tokens.is_empty() {
        return Ok(None);
    }

    let mut queue = BinaryHeap::new();
    let mut best_seen: HashMap<SearchKey, SearchRecord> = HashMap::new();
    let mut cache = RecoverySearchCache::default();
    let mut enqueue_order = 0usize;
    for stack in stacks {
        let config = SearchConfig {
            stack,
            input: 0,
            repairs: Vec::new(),
            score: SearchScore::default(),
        };
        push_config(&mut queue, &mut best_seen, config, 0, &mut enqueue_order);
    }

    let mut solution_cost: Option<usize> = None;
    let mut best_solution: Option<Vec<Repair>> = None;

    while let Some(item) = queue.pop() {
        if start.elapsed() >= timeout {
            return Err(RecoveryError::Timeout {
                elapsed: start.elapsed(),
            });
        }
        if solution_cost.is_some_and(|best| item.cost > best) {
            break;
        }

        let key = key(&item.config);
        if best_seen
            .get(&key)
            .is_none_or(|best| best.cost != item.cost || best.score != item.config.score)
        {
            continue;
        }

        let lookahead = token_at(ctx.grammar, tokens, item.config.input);
        let closed = close_stacks(
            ctx.grammar,
            ctx.actions,
            ctx.gotos,
            &item.config.stack,
            lookahead,
            &mut cache,
        );
        if is_viable_completion(
            ctx,
            tokens,
            item.config.input,
            &closed,
            &item.config,
            &mut cache,
        ) {
            let repairs = item.config.repairs.clone();
            if solution_cost.is_none() {
                solution_cost = Some(item.cost);
            }
            if best_solution
                .as_ref()
                .is_none_or(|current| repairs_better(&repairs, current))
            {
                best_solution = Some(repairs);
            }
            continue;
        }

        for closed_stack in closed.iter() {
            push_shift_neighbours(
                ctx,
                tokens,
                &item.config,
                &closed_stack.stack,
                item.cost,
                &mut queue,
                &mut best_seen,
                &mut enqueue_order,
            );
            push_shift_as_error_neighbours(
                ctx,
                tokens,
                &item.config,
                &closed_stack.stack,
                item.cost,
                &mut queue,
                &mut best_seen,
                &mut enqueue_order,
            );
            push_insert_neighbours(
                ctx,
                &item.config,
                &closed_stack.stack,
                item.cost,
                &mut cache,
                &mut queue,
                &mut best_seen,
                &mut enqueue_order,
            );
            push_delete_neighbour(
                ctx,
                tokens,
                &item.config,
                &closed_stack.stack,
                item.cost,
                &mut queue,
                &mut best_seen,
                &mut enqueue_order,
            );
        }
    }

    Ok(best_solution.map(|repairs| RecoveryResult { repairs }))
}

fn repairs_better(candidate: &[Repair], current: &[Repair]) -> bool {
    let (cand_shifts, cand_error_shifts, cand_inserts, cand_deletes) = repair_quality(candidate);
    let (cur_shifts, cur_error_shifts, cur_inserts, cur_deletes) = repair_quality(current);
    if cand_shifts != cur_shifts {
        return cand_shifts > cur_shifts;
    }
    if cand_error_shifts != cur_error_shifts {
        return cand_error_shifts < cur_error_shifts;
    }
    if cand_inserts != cur_inserts {
        return cand_inserts < cur_inserts;
    }
    if cand_deletes != cur_deletes {
        return cand_deletes < cur_deletes;
    }
    compare_repairs(candidate, current).is_lt()
}

fn repair_quality(repairs: &[Repair]) -> (usize, usize, usize, usize) {
    let mut shifts = 0usize;
    let mut error_shifts = 0usize;
    let mut inserts = 0usize;
    let mut deletes = 0usize;
    for repair in repairs {
        match repair {
            Repair::Shift => shifts += 1,
            Repair::ShiftAsError => error_shifts += 1,
            Repair::Insert(_) => inserts += 1,
            Repair::Delete => deletes += 1,
        }
    }
    (shifts, error_shifts, inserts, deletes)
}

fn compare_repairs(a: &[Repair], b: &[Repair]) -> Ordering {
    a.iter()
        .zip(b.iter())
        .map(|(left, right)| repair_rank(left).cmp(&repair_rank(right)))
        .find(|ordering| *ordering != Ordering::Equal)
        .unwrap_or(Ordering::Equal)
}

fn repair_rank(repair: &Repair) -> (u8, u32) {
    match repair {
        Repair::Shift => (0, 0),
        Repair::Delete => (1, 0),
        Repair::Insert(_) => (2, 0),
        Repair::ShiftAsError => (3, 0),
    }
}

fn is_viable_completion(
    ctx: &SessionContext<'_>,
    tokens: &[ParseToken],
    input: usize,
    closed: &Arc<[ClosedStack]>,
    config: &SearchConfig,
    cache: &mut RecoverySearchCache,
) -> bool {
    if config.repairs.is_empty() {
        return false;
    }
    for stack in closed.iter() {
        if stack.accepted
            || can_shift_suffix(ctx, tokens, input, &stack.stack, MIN_REAL_SHIFTS, cache)
        {
            return true;
        }
    }
    false
}

fn can_shift_suffix(
    ctx: &SessionContext<'_>,
    tokens: &[ParseToken],
    input: usize,
    stack: &[usize],
    remaining_shifts: usize,
    cache: &mut RecoverySearchCache,
) -> bool {
    let key = ShiftSuffixKey {
        stack: stack.to_vec(),
        input,
        remaining_shifts,
    };
    if let Some(&cached) = cache.can_shift_suffix.get(&key) {
        return cached;
    }

    if remaining_shifts == 0 {
        cache.can_shift_suffix.insert(key, true);
        return true;
    }

    let lookahead = token_at(ctx.grammar, tokens, input);
    let result = close_stacks(ctx.grammar, ctx.actions, ctx.gotos, stack, lookahead, cache)
        .iter()
        .any(|closed| {
            if closed.accepted {
                return true;
            }

            let Some(&state) = closed.stack.last() else {
                return false;
            };
            if !action_set(ctx.grammar, ctx.actions, state, lookahead).has_shift() {
                return false;
            }

            for next_state in shift_targets(ctx.grammar, ctx.actions, state, lookahead) {
                let next_stack = pushed(&closed.stack, next_state);
                let next_remaining = if lookahead == ctx.grammar.error_terminal {
                    remaining_shifts
                } else {
                    remaining_shifts - 1
                };
                if can_shift_suffix(
                    ctx,
                    tokens,
                    input.saturating_add(1),
                    &next_stack,
                    next_remaining,
                    cache,
                ) {
                    return true;
                }
            }

            false
        });
    cache.can_shift_suffix.insert(key, result);
    result
}

fn push_shift_neighbours(
    ctx: &SessionContext<'_>,
    tokens: &[ParseToken],
    config: &SearchConfig,
    stack: &[usize],
    cost: usize,
    queue: &mut BinaryHeap<QueueItem>,
    best_seen: &mut HashMap<SearchKey, SearchRecord>,
    enqueue_order: &mut usize,
) {
    let terminal = token_at(ctx.grammar, tokens, config.input);
    let Some(&state) = stack.last() else {
        return;
    };
    for next_state in shift_targets(ctx.grammar, ctx.actions, state, terminal) {
        let mut next = SearchConfig {
            stack: pushed(stack, next_state),
            input: config.input.saturating_add(1),
            repairs: config.repairs.clone(),
            score: config.score,
        };
        next.repairs.push(Repair::Shift);
        next.score.shifts += 1;
        push_config(queue, best_seen, next, cost, enqueue_order);
    }
}

fn push_shift_as_error_neighbours(
    ctx: &SessionContext<'_>,
    tokens: &[ParseToken],
    config: &SearchConfig,
    stack: &[usize],
    cost: usize,
    queue: &mut BinaryHeap<QueueItem>,
    best_seen: &mut HashMap<SearchKey, SearchRecord>,
    enqueue_order: &mut usize,
) {
    if token_at(ctx.grammar, tokens, config.input) == ctx.grammar.eof {
        return;
    }
    let Some(&state) = stack.last() else {
        return;
    };
    let terminal = ctx.grammar.error_terminal;
    for next_state in shift_targets(ctx.grammar, ctx.actions, state, terminal) {
        let mut next = SearchConfig {
            stack: pushed(stack, next_state),
            input: config.input.saturating_add(1),
            repairs: config.repairs.clone(),
            score: config.score,
        };
        next.repairs.push(Repair::ShiftAsError);
        next.score.error_shifts += 1;
        push_config(queue, best_seen, next, cost + 1, enqueue_order);
    }
}

fn push_insert_neighbours(
    ctx: &SessionContext<'_>,
    config: &SearchConfig,
    stack: &[usize],
    cost: usize,
    cache: &mut RecoverySearchCache,
    queue: &mut BinaryHeap<QueueItem>,
    best_seen: &mut HashMap<SearchKey, SearchRecord>,
    enqueue_order: &mut usize,
) {
    for terminal in insert_terminals(ctx, stack, cache).iter().copied() {
        for closed in
            close_stacks(ctx.grammar, ctx.actions, ctx.gotos, stack, terminal, cache).iter()
        {
            let Some(&state) = closed.stack.last() else {
                continue;
            };
            for next_state in shift_targets(ctx.grammar, ctx.actions, state, terminal) {
                let mut next = SearchConfig {
                    stack: pushed(&closed.stack, next_state),
                    input: config.input,
                    repairs: config.repairs.clone(),
                    score: config.score,
                };
                next.repairs.push(Repair::Insert(terminal));
                next.score.inserts += 1;
                push_config(queue, best_seen, next, cost + 1, enqueue_order);
            }
        }
    }
}

fn push_delete_neighbour(
    ctx: &SessionContext<'_>,
    tokens: &[ParseToken],
    config: &SearchConfig,
    stack: &[usize],
    cost: usize,
    queue: &mut BinaryHeap<QueueItem>,
    best_seen: &mut HashMap<SearchKey, SearchRecord>,
    enqueue_order: &mut usize,
) {
    if token_at(ctx.grammar, tokens, config.input) == ctx.grammar.eof {
        return;
    }

    let mut next = SearchConfig {
        stack: stack.to_vec(),
        input: config.input.saturating_add(1),
        repairs: config.repairs.clone(),
        score: config.score,
    };
    next.repairs.push(Repair::Delete);
    next.score.deletes += 1;
    push_config(queue, best_seen, next, cost + 1, enqueue_order);
}

fn push_config(
    queue: &mut BinaryHeap<QueueItem>,
    best_seen: &mut HashMap<SearchKey, SearchRecord>,
    config: SearchConfig,
    cost: usize,
    enqueue_order: &mut usize,
) {
    let key = key(&config);
    let record = SearchRecord {
        cost,
        score: config.score,
    };
    if best_seen
        .get(&key)
        .is_some_and(|best| !record.dominates(*best))
    {
        return;
    }
    best_seen.insert(key, record);
    let item = QueueItem {
        cost,
        order: *enqueue_order,
        config,
    };
    *enqueue_order += 1;
    queue.push(item);
}

fn key(config: &SearchConfig) -> SearchKey {
    SearchKey {
        stack: config.stack.clone(),
        input: config.input,
    }
}

fn active_stack_paths(ctx: &SessionContext<'_>, column: usize) -> Vec<Vec<usize>> {
    let Some(column) = ctx.state.column(column) else {
        return Vec::new();
    };

    let mut paths = Vec::new();
    for node in column.active_nodes() {
        let mut suffix = Vec::new();
        let mut seen_nodes = HashSet::new();
        collect_stack_paths(ctx, node, &mut suffix, &mut seen_nodes, &mut paths);
    }
    paths.sort();
    paths.dedup();
    paths
}

fn collect_stack_paths(
    ctx: &SessionContext<'_>,
    node: usize,
    suffix: &mut Vec<usize>,
    seen_nodes: &mut HashSet<usize>,
    paths: &mut Vec<Vec<usize>>,
) {
    if !seen_nodes.insert(node) {
        return;
    }
    let Some(gss_node) = ctx.gss.get_node(node) else {
        seen_nodes.remove(&node);
        return;
    };
    suffix.push(gss_node.state);

    let edges = ctx.gss.outgoing_edges(node).collect::<Vec<_>>();
    if edges.is_empty() {
        let mut path = suffix.clone();
        path.reverse();
        paths.push(path);
    } else {
        for edge in edges {
            collect_stack_paths(ctx, edge.to, suffix, seen_nodes, paths);
        }
    }

    suffix.pop();
    seen_nodes.remove(&node);
}

fn close_stacks(
    grammar: &Grammar,
    actions: &[ActionSet],
    gotos: &[Option<usize>],
    stack: &[usize],
    lookahead: TerminalId,
    cache: &mut RecoverySearchCache,
) -> Arc<[ClosedStack]> {
    let key = ClosedStackKey {
        stack: stack.to_vec(),
        lookahead,
    };
    if let Some(cached) = cache.closed_stacks.get(&key) {
        return Arc::clone(cached);
    }

    let out = Arc::<[ClosedStack]>::from(compute_closed_stacks(
        grammar, actions, gotos, stack, lookahead,
    ));
    cache.closed_stacks.insert(key, Arc::clone(&out));
    out
}

fn compute_closed_stacks(
    grammar: &Grammar,
    actions: &[ActionSet],
    gotos: &[Option<usize>],
    stack: &[usize],
    lookahead: TerminalId,
) -> Vec<ClosedStack> {
    let mut out = Vec::new();
    let mut queue = VecDeque::from([stack.to_vec()]);
    let mut seen = HashSet::new();

    while let Some(stack) = queue.pop_front() {
        if !seen.insert(stack.clone()) {
            continue;
        }

        let Some(&state) = stack.last() else {
            continue;
        };
        let mut reduced = false;
        let mut accepted = false;
        for action in action_set(grammar, actions, state, lookahead)
            .inner
            .iter()
            .cloned()
        {
            match action {
                Action::Reduce(production) => {
                    let rhs_len = grammar.production_rhs_len(production);
                    if stack.len() <= rhs_len {
                        continue;
                    }
                    let pred = stack[stack.len() - rhs_len - 1];
                    let lhs = grammar.production_lhs(production);
                    let Some(goto) = gotos[grammar.goto_index(pred, lhs)] else {
                        continue;
                    };
                    let mut next = stack[..stack.len() - rhs_len].to_vec();
                    next.push(goto);
                    queue.push_back(next);
                    reduced = true;
                }
                Action::Accept => accepted = true,
                Action::Shift(_) | Action::Error => {}
            }
        }

        if accepted || !reduced {
            out.push(ClosedStack { stack, accepted });
        }
    }

    out
}

fn insert_terminals(
    ctx: &SessionContext<'_>,
    stack: &[usize],
    cache: &mut RecoverySearchCache,
) -> Arc<[TerminalId]> {
    if let Some(cached) = cache.insert_terminals.get(stack) {
        return Arc::clone(cached);
    }

    let mut terminals = Vec::new();
    for index in 0..ctx.grammar.terminal_count() {
        let terminal = ctx.grammar.terminal_at(index);
        if terminal == ctx.grammar.eof {
            continue;
        }
        let can_shift = close_stacks(ctx.grammar, ctx.actions, ctx.gotos, stack, terminal, cache)
            .iter()
            .any(|closed| {
                closed.stack.last().is_some_and(|state| {
                    action_set(ctx.grammar, ctx.actions, *state, terminal).has_shift()
                })
            });
        if can_shift {
            terminals.push(terminal);
        }
    }
    let terminals = Arc::<[TerminalId]>::from(terminals);
    cache
        .insert_terminals
        .insert(stack.to_vec(), Arc::clone(&terminals));
    terminals
}

fn shift_targets(
    grammar: &Grammar,
    actions: &[ActionSet],
    state: usize,
    terminal: TerminalId,
) -> Vec<usize> {
    action_set(grammar, actions, state, terminal)
        .inner
        .iter()
        .filter_map(|action| match action {
            Action::Shift(next) => Some(*next),
            _ => None,
        })
        .collect()
}

fn action_set<'a>(
    grammar: &Grammar,
    actions: &'a [ActionSet],
    state: usize,
    terminal: TerminalId,
) -> &'a ActionSet {
    &actions[grammar.action_index(state, terminal)]
}

fn pushed(stack: &[usize], state: usize) -> Vec<usize> {
    let mut next = stack.to_vec();
    next.push(state);
    next
}

fn token_at(grammar: &Grammar, tokens: &[ParseToken], input: usize) -> TerminalId {
    tokens
        .get(input)
        .map_or(grammar.eof, |token| token.terminal)
}
