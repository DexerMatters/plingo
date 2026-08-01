use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};

use indexmap::IndexSet;

use super::{ParseColumn, ParseError, ParseToken, ReplayPlan, SessionContext, checkpoint};
use crate::{
    component::{
        lex::LexerRoot,
        parse::{
            IncrementalParseStats, Parser, ParserSnapshotState,
            data::{
                ast::AstArena,
                green::TreeArena,
                gss::{GssArena, GssNodeId},
                product::{ProductArena, ProductData, ProductId},
            },
            types::SessionArenas,
        },
    },
    scheme::change::AddressChange,
};

struct ReusableSuffix {
    columns: Vec<ParseColumn>,
    boundary_columns: Vec<Option<usize>>,
    /// A convergence candidate may only enter a suffix containing ordinary
    /// token columns. Recovery columns are replayed, not structurally reused.
    clean: Vec<bool>,
    column_base: usize,
    token_columns: HashMap<usize, usize>,
}

/// Phase accounting for exact parser convergence. These durations are emitted
/// with each replay so a logical suffix reuse can be distinguished from its
/// physical mapping/rebase cost.
#[derive(Default)]
struct ReuseTiming {
    checkpoint: Duration,
    frontier_match: Duration,
    tail_validation: Duration,
    product_remap: Duration,
    rebase: Duration,
}

/// Dense parser-arena IDs make vector-backed remapping substantially cheaper
/// than hashing every retained node/product during suffix rebasing.
struct ProductRemap {
    values: Vec<Option<ProductId>>,
}

impl ProductRemap {
    fn from_mapping(mapping: HashMap<ProductId, ProductId>, capacity: usize) -> Self {
        let mut values = vec![None; capacity];
        for (old, new) in mapping {
            if old >= values.len() {
                values.resize(old + 1, None);
            }
            values[old] = Some(new);
        }
        Self { values }
    }

    fn get(&self, old: ProductId) -> Option<ProductId> {
        self.values.get(old).copied().flatten()
    }

    fn insert(&mut self, old: ProductId, new: ProductId) {
        if old >= self.values.len() {
            self.values.resize(old + 1, None);
        }
        self.values[old] = Some(new);
    }
}

struct NodeRemap {
    values: Vec<Option<GssNodeId>>,
}

impl NodeRemap {
    fn from_mapping(mapping: HashMap<GssNodeId, GssNodeId>, capacity: usize) -> Self {
        let mut values = vec![None; capacity];
        for (old, new) in mapping {
            if old >= values.len() {
                values.resize(old + 1, None);
            }
            values[old] = Some(new);
        }
        Self { values }
    }

    fn get(&self, old: GssNodeId) -> Option<GssNodeId> {
        self.values.get(old).copied().flatten()
    }

    fn insert(&mut self, old: GssNodeId, new: GssNodeId) {
        if old >= self.values.len() {
            self.values.resize(old + 1, None);
        }
        self.values[old] = Some(new);
    }
}

fn remap_product(
    old: ProductId,
    mapping: &mut ProductRemap,
    session_ctx: &mut SessionContext<'_>,
) -> Result<Option<ProductId>, ParseError> {
    if let Some(product) = mapping.get(old) {
        return Ok(Some(product));
    }
    let Some(data) = session_ctx
        .products
        .get(old)
        .map(|product| product.data.clone())
    else {
        return Ok(None);
    };
    if matches!(data, ProductData::Token { .. }) {
        mapping.insert(old, old);
        return Ok(Some(old));
    }
    let Some(origin) = session_ctx.state.reduction_origins.get(&old).cloned() else {
        return Ok(None);
    };
    let mut children = Vec::with_capacity(origin.children.len());
    for &child in &origin.children {
        let Some(child) = remap_product(child, mapping, session_ctx)? else {
            return Ok(None);
        };
        children.push(child);
    }
    let product =
        session_ctx.reduce_cached(origin.production, &children, origin.boundary.unwrap_or(0))?;
    mapping.insert(old, product);
    Ok(Some(product))
}

fn maybe_reuse_suffix(
    plan: &ReplayPlan,
    old: &mut ReusableSuffix,
    session_ctx: &mut SessionContext<'_>,
    current: (usize, usize),
    stats: &mut IncrementalParseStats,
    timing: &mut ReuseTiming,
) -> Result<bool, ParseError> {
    let (current_boundary, current_token_boundary) = current;
    // Only the final-coordinate suffix can correspond to the base revision's
    // unchanged suffix, so reuse is never considered inside a changed range.
    if current_token_boundary < plan.new_reuse_start {
        return Ok(false);
    }
    stats.convergence_checks += 1;
    let old_token_boundary = plan.old_reuse_start + (current_token_boundary - plan.new_reuse_start);
    let Some(old_boundary) = old
        .boundary_columns
        .get(old_token_boundary)
        .copied()
        .flatten()
    else {
        return Ok(false);
    };
    let Some(old_index) = old_boundary.checked_sub(old.column_base).filter(|&index| {
        old.columns.get(index).is_some() && old.clean.get(index).copied().unwrap_or(false)
    }) else {
        return Ok(false);
    };

    let checkpoint_start = Instant::now();
    let current_frontier = {
        let current_column = &mut session_ctx.state.columns[current_boundary];
        checkpoint::frontier_checkpoint_for_column(current_column, session_ctx.gss).clone()
    };
    let old_frontier = {
        let old_column = &mut old.columns[old_index];
        checkpoint::frontier_checkpoint_for_column(old_column, session_ctx.gss).clone()
    };
    if current_frontier != old_frontier {
        timing.checkpoint += checkpoint_start.elapsed();
        return Ok(false);
    }
    timing.checkpoint += checkpoint_start.elapsed();
    stats.checkpoint_matches += 1;

    let frontier_start = Instant::now();
    let old_column = &old.columns[old_index];
    let current_column = &session_ctx.state.columns[current_boundary];
    let old_base = old_column.base_active_nodes().collect::<Vec<_>>();
    let old_active = old_column.active_nodes().collect::<Vec<_>>();
    let new_base = current_column.base_active_nodes().collect::<Vec<_>>();
    let new_active = current_column.active_nodes().collect::<Vec<_>>();
    let Some((frontier_nodes, frontier_products, shared_prefix)) = session_ctx
        .gss
        .match_frontiers((&old_base, &old_active), (&new_base, &new_active))
    else {
        timing.frontier_match += frontier_start.elapsed();
        return Ok(false);
    };
    timing.frontier_match += frontier_start.elapsed();
    stats.frontier_matches += 1;

    let mut nodes = NodeRemap::from_mapping(frontier_nodes, session_ctx.gss.node_count());
    let mut products =
        ProductRemap::from_mapping(frontier_products, session_ctx.products.products.len());

    let tail = &old.columns[old_index + 1..];
    let tail_validation_start = Instant::now();
    if tail.iter().any(|column| {
        column
            .token
            .is_none_or(|token| !old.token_columns.contains_key(&token))
    }) {
        timing.tail_validation += tail_validation_start.elapsed();
        return Ok(false);
    }
    timing.tail_validation += tail_validation_start.elapsed();

    let rebase_start = Instant::now();
    let mut scheduled_nodes = vec![false; session_ctx.gss.node_count()];
    let mut planned_nodes = Vec::<(GssNodeId, usize, usize)>::new();
    for (offset, column) in tail.iter().enumerate() {
        for node in column.base_active_nodes().chain(column.active_nodes()) {
            if nodes.get(node).is_some() || scheduled_nodes[node] {
                continue;
            }
            let Some(state) = session_ctx.gss.get_node(node).map(|node| node.state) else {
                return Ok(false);
            };
            scheduled_nodes[node] = true;
            planned_nodes.push((node, state, current_boundary + offset + 1));
        }
    }

    let mut planned_edges = Vec::new();
    for &(node, _, _) in &planned_nodes {
        for edge in session_ctx.gss.outgoing_edges(node) {
            if nodes.get(edge.to).is_none() && !scheduled_nodes[edge.to] {
                if !shared_prefix {
                    return Ok(false);
                }
                // A reused reduction may pop beneath the matched frontier.
                // Such nodes are descendants of the persistent identity anchor.
                nodes.insert(edge.to, edge.to);
            }
            planned_edges.push((node, edge.to, edge.product));
        }
    }

    timing.rebase += rebase_start.elapsed();

    let product_remap_start = Instant::now();
    for &(_, _, product) in &planned_edges {
        if remap_product(product, &mut products, session_ctx)?.is_none() {
            return Ok(false);
        }
    }
    for column in tail {
        for &product in column.products.iter().chain(column.accepted()) {
            if remap_product(product, &mut products, session_ctx)?.is_none() {
                return Ok(false);
            }
        }
    }

    timing.product_remap += product_remap_start.elapsed();

    let rebase_start = Instant::now();
    for &(old_node, state, column) in &planned_nodes {
        let new_node = session_ctx
            .gss
            .node(state, column, session_ctx.state.generation);
        nodes.insert(old_node, new_node);
    }
    for (from, to, product) in planned_edges {
        session_ctx.gss.add_edge(
            nodes.get(from).expect("proven node correspondence"),
            nodes.get(to).expect("proven node correspondence"),
            products
                .get(product)
                .expect("proven product correspondence"),
            session_ctx.state.generation,
        );
    }

    // The suffix was moved out of the working session during replay setup;
    // transfer its reusable tail directly rather than cloning every column.
    let mut reused_columns = old.columns.split_off(old_index + 1);
    for column in &mut reused_columns {
        column.token = column.token.map(|token| old.token_columns[&token]);
        column.base_active = column
            .base_active
            .iter()
            .map(|node| nodes.get(*node).expect("proven node correspondence"))
            .collect();
        column.active = column
            .active
            .iter()
            .map(|node| nodes.get(*node).expect("proven node correspondence"))
            .collect();
        column.products = column
            .products
            .iter()
            .map(|product| {
                products
                    .get(*product)
                    .expect("proven product correspondence")
            })
            .collect();
        column.accepted = column
            .accepted
            .iter()
            .map(|product| {
                products
                    .get(*product)
                    .expect("proven product correspondence")
            })
            .collect();
        column.checkpoint_cache = Default::default();
    }

    timing.rebase += rebase_start.elapsed();

    stats.reconverged_new_boundary = Some(current_boundary);
    stats.reconverged_old_boundary = Some(old_boundary);
    session_ctx.state.append_reused_columns(reused_columns);
    Ok(true)
}

impl<Root: LexerRoot + Clone> Parser<Root> {
    pub(crate) fn parse_delta_batch(
        &mut self,
        working: &mut ParserSnapshotState,
        change: AddressChange<fluent_uri::Uri<&'static str>, crate::component::parse::TokenData>,
    ) -> Result<(), ParseError> {
        let total_start = Instant::now();
        let uri = change.address;

        let plan_start = Instant::now();
        let plan = ReplayPlan::from_change(
            &change,
            working
                .tokens
                .get(&uri)
                .map(|tokens| tokens.as_ref().clone())
                .unwrap_or_default(),
        );
        let plan_elapsed = plan_start.elapsed();

        let session_setup_start = Instant::now();
        let arenas = self
            .session_arenas
            .entry(uri)
            .or_insert_with(|| SessionArenas {
                trees: TreeArena::new(),
                products: ProductArena::new(),
                ast: AstArena::new(uri),
                gss: GssArena::new(),
            });
        let state = Arc::make_mut(working.sessions.entry(uri).or_default());
        if state.columns.is_empty() {
            let start = arenas.gss.node(0, 0, 0);
            state.columns = vec![ParseColumn::new(None, IndexSet::from([start]))];
        }
        let mut session_ctx = SessionContext {
            state,
            trees: &mut arenas.trees,
            products: &mut arenas.products,
            ast: &mut arenas.ast,
            gss: &mut arenas.gss,
            grammar: &self.grammar,
            actions: &self.actions,
            gotos: &self.gotos,
            error_recovery: self.config.error_recovery,
        };
        let session_setup_elapsed = session_setup_start.elapsed();

        // Recovery may add physical columns without consuming a token. Keep the
        // token-to-column map explicit instead of treating both axes as equal.
        let old_boundary_columns = std::iter::once(Some(0))
            .chain(
                plan.old_units
                    .iter()
                    .map(|token| session_ctx.state.token_columns.get(&token.column).copied()),
            )
            .collect::<Vec<_>>();
        // Replay begins at the nearest checkpoint at or before the first
        // changed token. Recovery columns are valid deterministic checkpoints;
        // their exact frontier state participates in later convergence proof.
        let restart_token_boundary = (0..=plan
            .restart_boundary
            .min(old_boundary_columns.len().saturating_sub(1)))
            .rev()
            .find(|&boundary| old_boundary_columns[boundary].is_some())
            .unwrap_or(0);
        let restart_boundary = old_boundary_columns[restart_token_boundary].unwrap_or(0);
        let old_column_base = old_boundary_columns
            .get(plan.old_reuse_start..)
            .and_then(|boundaries| boundaries.iter().flatten().next())
            .copied()
            .unwrap_or(session_ctx.state.columns.len());
        let checkpoint_start = Instant::now();
        // Move only a suffix strictly beyond the retained restart checkpoint.
        // When the old reusable boundary includes that checkpoint (notably an
        // initial EOF replacement), preserve it in the working session and
        // snapshot the suffix instead; `truncate_to_column` requires it.
        let old_suffix_columns = if old_column_base <= restart_boundary {
            session_ctx.state.columns[old_column_base..].to_vec()
        } else {
            session_ctx.state.columns.split_off(old_column_base)
        };
        let mut old_suffix_is_clean = vec![true; old_suffix_columns.len() + 1];
        for index in (0..old_suffix_columns.len()).rev() {
            old_suffix_is_clean[index] =
                old_suffix_is_clean[index + 1] && old_suffix_columns[index].token.is_some();
        }
        let token_columns = plan
            .old_units
            .get(plan.old_reuse_start..)
            .unwrap_or_default()
            .iter()
            .zip(
                plan.new_units
                    .get(plan.new_reuse_start..)
                    .unwrap_or_default(),
            )
            .map(|(old, new)| (old.column, new.column))
            .collect();
        let mut old_suffix = ReusableSuffix {
            columns: old_suffix_columns,
            boundary_columns: old_boundary_columns,
            clean: old_suffix_is_clean,
            column_base: old_column_base,
            token_columns,
        };
        let checkpoint_elapsed = checkpoint_start.elapsed();
        let old_suffix_len = old_suffix.columns.len();

        let truncate_start = Instant::now();
        session_ctx.state.truncate_to_column(restart_boundary);
        let truncate_elapsed = truncate_start.elapsed();

        let token_materialization_start = Instant::now();
        let parse_tokens = plan
            .new_units
            .get(restart_token_boundary..)
            .unwrap_or_default()
            .iter()
            .map(|data| ParseToken {
                entry: data.id,
                column: data.column,
                start: data.start,
                terminal: session_ctx.resolve_terminal(data),
                length: data.length,
                fingerprint: data.fingerprint,
                merge_source_terminal: None,
            })
            .collect::<Vec<_>>();
        let token_materialization_elapsed = token_materialization_start.elapsed();

        let eof = self.grammar.eof;
        let mut stats = IncrementalParseStats::default();
        let replay_start = Instant::now();
        let mut reduce_elapsed = Duration::default();
        let mut shift_elapsed = Duration::default();
        let mut recover_elapsed = Duration::default();
        let mut converge_elapsed = Duration::default();
        let mut reuse_timing = ReuseTiming::default();
        let mut i = 0usize;
        while i < parse_tokens.len() {
            let token = &parse_tokens[i];
            let column = session_ctx.state.current_column();
            let reduce_start = Instant::now();
            session_ctx.reduce_until_stable(column, token.terminal, token.column)?;
            reduce_elapsed += reduce_start.elapsed();

            // Stored columns include reductions selected by the next lookahead,
            // so convergence is checked at the same post-reduction moment.
            let converge_start = Instant::now();
            if maybe_reuse_suffix(
                &plan,
                &mut old_suffix,
                &mut session_ctx,
                (column, restart_token_boundary + i),
                &mut stats,
                &mut reuse_timing,
            )? {
                converge_elapsed += converge_start.elapsed();
                break;
            }
            converge_elapsed += converge_start.elapsed();

            if token.terminal == eof && !session_ctx.state.accepted().is_empty() {
                break;
            }

            if token.terminal == session_ctx.grammar.error_terminal && session_ctx.error_recovery {
                let recover_start = Instant::now();
                if let Some(next) = session_ctx.recover_tokens(i, &parse_tokens)? {
                    recover_elapsed += recover_start.elapsed();
                    if next == i {
                        continue;
                    }
                    i = next;
                    continue;
                }
                recover_elapsed += recover_start.elapsed();
            }

            let shift_start = Instant::now();
            if let Err(ParseError::NoActiveStacks { .. }) =
                session_ctx.shift_parse_token(column, token)
            {
                shift_elapsed += shift_start.elapsed();
                let recover_start = Instant::now();
                if let Some(next) = session_ctx.recover_tokens(i, &parse_tokens)? {
                    recover_elapsed += recover_start.elapsed();
                    if next == i {
                        continue;
                    }
                    i = next;
                    continue;
                }
                recover_elapsed += recover_start.elapsed();
                // An unrecoverable token still becomes a persistent error product.
                // Continue replaying so later valid tokens, diagnostics, and
                // partial roots remain observable in this same revision.
                session_ctx.delete_parse_token(column, token)?;
                i += 1;
                continue;
            }
            shift_elapsed += shift_start.elapsed();

            if token.terminal == eof {
                let next_column = session_ctx.state.current_column();
                let reduce_start = Instant::now();
                session_ctx.reduce_until_stable(next_column, token.terminal, token.column)?;
                reduce_elapsed += reduce_start.elapsed();
            }

            i += 1;
        }
        let replay_elapsed = replay_start.elapsed();
        let replay_misc_elapsed = replay_elapsed
            .saturating_sub(reduce_elapsed + shift_elapsed + recover_elapsed + converge_elapsed);

        let roots_after = session_ctx.state.accepted().to_vec();

        let reused = stats
            .reconverged_old_boundary
            .map(|old_boundary| {
                old_suffix_len.saturating_sub(
                    old_boundary
                        .saturating_sub(old_suffix.column_base)
                        .saturating_add(1),
                )
            })
            .unwrap_or(0);
        let reparsed = stats
            .reconverged_new_boundary
            .map(|new_boundary| new_boundary.saturating_sub(restart_boundary))
            .unwrap_or_else(|| {
                session_ctx
                    .state
                    .current_column()
                    .saturating_sub(restart_boundary)
            });
        let recovery_columns = session_ctx
            .state
            .columns
            .iter()
            .skip(restart_boundary.saturating_add(1))
            .filter(|c| c.error_derived)
            .count();
        let stats_start = Instant::now();
        stats.restart_boundary = restart_boundary;
        stats.reparsed = reparsed;
        stats.reused = reused;
        stats.recovery_columns = recovery_columns;
        let replay_status = if stats.reconverged_old_boundary.is_some() {
            "reused-suffix"
        } else if recovery_columns > 0 {
            "recovered-to-eof"
        } else {
            "replayed-to-eof"
        };
        working.incremental_stats.insert(uri, stats);
        let stats_elapsed = stats_start.elapsed();

        working.roots.insert(uri, Arc::new(roots_after));
        working.tokens.insert(uri, Arc::new(plan.new_units.clone()));

        let total_elapsed = total_start.elapsed();
        log::debug!(
            "[parse-replay] uri={} total={:?} plan={:?} session={:?} checkpoints={:?} truncate={:?} tokens={:?} replay={:?} reduce={:?} shift={:?} recover={:?} converge={:?} reuse_checkpoint={:?} frontier_match={:?} tail_validate={:?} product_remap={:?} suffix_rebase={:?} replay_misc={:?} stats={:?} status={} restart={} reparsed={} reused={} recovery_columns={} checks={} checkpoint_matches={} frontier_matches={} old_suffix={} replay_tokens={} suffix_rebased={} old_tokens={} new_tokens={} prefix={} suffix={}",
            uri,
            total_elapsed,
            plan_elapsed,
            session_setup_elapsed,
            checkpoint_elapsed,
            truncate_elapsed,
            token_materialization_elapsed,
            replay_elapsed,
            reduce_elapsed,
            shift_elapsed,
            recover_elapsed,
            converge_elapsed,
            reuse_timing.checkpoint,
            reuse_timing.frontier_match,
            reuse_timing.tail_validation,
            reuse_timing.product_remap,
            reuse_timing.rebase,
            replay_misc_elapsed,
            stats_elapsed,
            replay_status,
            restart_boundary,
            reparsed,
            reused,
            recovery_columns,
            stats.convergence_checks,
            stats.checkpoint_matches,
            stats.frontier_matches,
            old_suffix_len,
            parse_tokens.len(),
            stats.reconverged_old_boundary.is_some(),
            plan.old_units.len(),
            plan.new_units.len(),
            plan.prefix_len,
            plan.suffix_len,
        );

        Ok(())
    }
}
