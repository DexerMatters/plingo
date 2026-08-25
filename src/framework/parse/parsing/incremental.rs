use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
    time::{Duration, Instant},
};
use indexmap::IndexSet;

use super::{
    ParseColumn, ParseError, ParseToken, ParserSessionState, ReplayPlan, SessionContext,
    TokenTail, checkpoint, product_direct_record,
};
use crate::{
    framework::{
        lex::LexerRoot,
        parse::{
            IncrementalParseStats, Parser, ParserSnapshotState,
            data::{
                ast::{AnchoredSpan, AstArena},
                green::{ParseErrorInfo, TreeArena},
                gss::{GssArena, GssNodeId},
                product::{ProductArena, ProductData, ProductId},
            },
            diagnostics::collect_parse_diagnostics,
            delta::{ChildSplice, KeyDelta, OrderedDelta, ParseDelta, ParsedStatus,
                ParseDiagnosticKey, SyntheticTokenId, SyntaxNodeId},
            types::{ParserTreeFacts, SessionArenas},
        },
    },
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

pub(super) fn decode_data(
    data: crate::framework::parse::TokenData,
    grammar: &crate::framework::parse::grammar::Grammar,
) -> ParseToken {
    let terminal = match data.terminal {
        Some(terminal) => terminal,
        None if data.fingerprint == crate::framework::parse::identity::eof_fingerprint() => {
            grammar.eof
        }
        None => grammar.error_terminal,
    };
    ParseToken {
        entry: data.id,
        column: data.column,
        start: data.start,
        terminal,
        length: data.length,
        merge_source_terminal: None,
    }
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
        // Cache-stable fast path (plan §8.6): when every product of this
        // retained-suffix column remapped to itself, its gss nodes, product
        // ids, and record segment are already correct in the immutable old
        // column — attaching it verbatim skips the O(suffix) rewrite. Only
        // the token anchor and the (now invalid) checkpoint cache change.
        let cache_stable = column
            .products
            .iter()
            .chain(column.accepted.iter())
            .all(|&product| {
                products
                    .get(product)
                    .is_some_and(|mapped| mapped == product)
            });
        if cache_stable {
            column.checkpoint_cache = Default::default();
            continue;
        }
        stats.suffix_rewritten += 1;
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
        // Remapped products may own new direct AST records (the replay
        // rebuilt them); the segment must list exactly the records its
        // products hold (plan §9.2), so reattachment restores precise
        // liveness and the journal sees genuinely live records.
        let mut records = Vec::new();
        for &product in column.products.iter().chain(column.accepted.iter()) {
            if let Some(record) = product_direct_record(session_ctx.products, product)
                && !records.contains(&record)
            {
                records.push(record);
            }
        }
        column.records = records;
    }

    timing.rebase += rebase_start.elapsed();

    stats.reconverged_new_boundary = Some(current_boundary);
    stats.reconverged_old_boundary = Some(old_boundary);
    session_ctx.state.append_reused_columns(reused_columns);
    Ok(true)
}


/// Freezes one command into the canonical [`ParseDelta`] (plan §9).
///
/// Membership is authoritative from the accepted roots: the union of the
/// accepted products' transitive AST records IS the live tree, so a
/// transiently live recovery region can never reach the published
/// domains. Journal-derived classification refines this where proven
/// (currently: none — record-granular republication relies on fact
/// equality downstream); lineage-keyed updates arrive with Phase 9.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
fn freeze_parse_delta(
    roots_after: &[ProductId],
    state: &mut ParserSessionState,
    products: &ProductArena,
    ast: &AstArena,
    previous: &ParserTreeFacts,
    tree_root: Option<u64>,
    mut live: crate::reactive::store::RadixMap<()>,
    previous_infos: &[ParseErrorInfo],
    current_infos: Vec<ParseErrorInfo>,
    previous_status: Option<ParsedStatus>,
    current_status: ParsedStatus,
) -> Arc<ParseDelta> {
    // Authoritative membership: every record transitively owned by an
    // accepted root product.
    let mut current: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    for &root in roots_after {
        if let Some(product) = products.get(root) {
            for &id in product.ast_ids.iter() {
                current.insert(id as u64);
            }
        }
    }
    let mut fresh_live = crate::reactive::store::RadixMap::default();
    for record in &current {
        fresh_live.insert(*record, ());
    }
    live = fresh_live;

    let mut inserted: Vec<u64> = current
        .iter()
        .copied()
        .filter(|record| !previous.contains(*record))
        .collect();
    let mut removed: Vec<u64> = previous
        .records
        .iter()
        .map(|(record, ())| record)
        .filter(|record| !current.contains(record))
        .collect();

    // ---- payload-update classification via stable lineage -------------
    // A record whose identity a dead predecessor provably carries is an
    // UPDATE under a retained key unless its shape (green + extent) is
    // identical, in which case no fact changed anywhere.
    let mut payload_updated: Vec<SyntaxNodeId> = Vec::new();
    let mut updated_records: Vec<(SyntaxNodeId, u64)> = Vec::new();
    let mut silent: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    {
        let dead_lookup = |record: u64| -> Option<u64> {
            state.lineage.lineage_of(record as usize)
        };
        let _ = dead_lookup;
        for &record in &inserted {
            let rec = record as usize;
            let Some(old_record) = state.lineage.inherited_from(rec) else {
                continue;
            };
            // The counterpart must have left the live tree; a surviving
            // twin resolves to local replacement (freshened in lineage).
            if current.contains(&(old_record as u64)) && old_record as u64 != record {
                continue;
            }
            let shape_equal = record_signature(ast, products, old_record as u64)
                == record_signature(ast, products, record);
            if shape_equal {
                silent.insert(record);
                continue;
            }
            let lineage = state
                .lineage
                .lineage_of(rec)
                .expect("published record carries a lineage");
            payload_updated.push(SyntaxNodeId(lineage));
            updated_records.push((SyntaxNodeId(lineage), record));
        }
    }


    // Release dead registrations first so removal mapping can consult the
    // death journal (plan §9.1: final values after coalescing).
    state
        .lineage
        .finalize_deaths(removed.iter().map(|record| *record as usize).collect());

    let root_carried =
        previous.root.is_some() && tree_root.is_some() && previous.root != tree_root;


    let mut syntax_inserted: Vec<SyntaxNodeId> = Vec::new();
    let mut inserted_pairs: Vec<(SyntaxNodeId, u64)> = Vec::new();
    for record in &inserted {
        // Domain disjointness: an inherited identity is either an update
        // under its retained key or, when the shape is identical, no fact
        // change at all. Only genuinely fresh identities insert.
        if silent.contains(record) || state.lineage.inherited_from(*record as usize).is_some() {
            continue;
        }
        let Some(lineage) = state.lineage.lineage_of(*record as usize) else {
            continue;
        };
        syntax_inserted.push(SyntaxNodeId(lineage));
        inserted_pairs.push((SyntaxNodeId(lineage), *record));
    }
    // Any inherited identity remains represented by its new bearer,
    // including shape-identical ("silent") replacements. Its dead arena
    // record must not retract a stable tree fact before the bearer is
    // reattached; otherwise the same child-link key would be removed and
    // upserted in one patch.
    let inherited_carriers: std::collections::BTreeSet<u64> = inserted
        .iter()
        .filter_map(|record| {
            state
                .lineage
                .inherited_from(*record as usize)
                .and_then(|_| state.lineage.lineage_of(*record as usize))
        })
        .collect();
    let carriers: std::collections::BTreeSet<u64> = payload_updated
        .iter()
        .chain(syntax_inserted.iter())
        .map(|SyntaxNodeId(lineage)| *lineage)
        .chain(inherited_carriers)
        .collect();
    let mut syntax_removed: Vec<SyntaxNodeId> = Vec::new();
    let mut removed_pairs: Vec<(SyntaxNodeId, u64)> = Vec::new();
    for record in &removed {
        // A replaced document root keeps its stable published node.
        if root_carried && Some(*record) == previous.root {
            continue;
        }
        let Some(lineage) = state.lineage.died_lineage_of(*record) else {
            continue;
        };
        if carriers.contains(&lineage) {
            continue;
        }
        syntax_removed.push(SyntaxNodeId(lineage));
        removed_pairs.push((SyntaxNodeId(lineage), *record));
    }
    {
        let mut sorted = syntax_removed.clone();
        sorted.sort_unstable();
        sorted.dedup();
        syntax_removed = sorted;
        removed_pairs.sort_unstable();
        removed_pairs.dedup();
    // ---- parents + child splices (plan §9 canonical domains) ---------
    // For each record whose identity a dead predecessor provably carries
    // (an update under a retained key), compare the predecessor's children
    // against the new record's children by stable lineage; a differing
    // ordered sequence yields one OrderedDelta, and a differing parent
    // lineages yields a parent update.
    use super::lineage::direct_child_records;
    let mut parent_updated: Vec<SyntaxNodeId> = Vec::new();
    let mut parent_removed: Vec<SyntaxNodeId> = Vec::new();
    let mut parent_inserted: Vec<SyntaxNodeId> = Vec::new();
    let mut child_splices: Vec<ChildSplice> = Vec::new();
    let resolve_lineage = |record: usize| -> Option<u64> {
        state
            .lineage
            .lineage_of(record)
            .or_else(|| state.lineage.died_lineage_of(record as u64))
    };
    for &(SyntaxNodeId(_lin), record) in &updated_records {
        let rec = record as usize;
        let Some(old_record) = state.lineage.inherited_from(rec) else {
            continue;
        };
        let Some(node_lineage) = resolve_lineage(rec) else {
            continue;
        };
        // Parent fact.
        let old_parent_lin = ast
            .parent_of(old_record as usize)
            .and_then(|parent| resolve_lineage(parent as usize));
        let new_parent_lin = ast.parent_of(rec).and_then(|parent| resolve_lineage(parent));
        if old_parent_lin != new_parent_lin {
            parent_updated.push(SyntaxNodeId(node_lineage));
        }
        // Child list splice.
        let old_child_records = direct_child_records(products, ast, old_record as usize);
        let old_children = old_child_records
            .iter()
            .filter_map(|&child| resolve_lineage(child as usize).map(SyntaxNodeId))
            .collect::<Vec<_>>();
        let new_children = direct_child_records(products, ast, rec)
            .into_iter()
            .filter_map(|child| resolve_lineage(child as usize).map(SyntaxNodeId))
            .collect::<Vec<_>>();
        if old_children != new_children {
            // Alignment for retraction: the removed middle children, paired
            // with their (possibly dead) arena record so the publisher can
            // derive their old node identity.
            let mut prefix = 0;
            while prefix < old_children.len() && prefix < new_children.len()
                && old_children[prefix] == new_children[prefix]
            {
                prefix += 1;
            }
            let mut suffix = 0;
            while suffix < old_children.len().saturating_sub(prefix)
                && suffix < new_children.len().saturating_sub(prefix)
                && old_children[old_children.len() - 1 - suffix]
                    == new_children[new_children.len() - 1 - suffix]
            {
                suffix += 1;
            }
            let removed_children: Vec<(SyntaxNodeId, u64)> = old_child_records
                [prefix..old_child_records.len() - suffix]
                .iter()
                .filter_map(|&child| {
                    resolve_lineage(child as usize).map(|lin| (SyntaxNodeId(lin), child as u64))
                })
                .collect();
            child_splices.push(ChildSplice {
                parent: SyntaxNodeId(node_lineage),
                delta: ordered_splice(&old_children, &new_children),
                removed_children: removed_children.into(),
            });
        }
    }
    for &(node, _) in &inserted_pairs {
        parent_inserted.push(node);
    }
    for &(node, _) in &removed_pairs {
        parent_removed.push(node);
    }
    let dedup_sorted = |v: &mut Vec<SyntaxNodeId>| {
        v.sort_unstable();
        v.dedup();
    };
    dedup_sorted(&mut parent_updated);
    dedup_sorted(&mut parent_removed);
    dedup_sorted(&mut parent_inserted);
    child_splices.sort_unstable_by_key(|splice| splice.parent);
    }

    sort_dedup(&mut inserted);
    sort_dedup(&mut removed);



    // ---- diagnostics + status domains --------------------------------
    let diagnostics = diagnostic_delta(previous_infos, &current_infos);
    let status = (previous_status != Some(current_status)).then_some(current_status);

    let delta = ParseDelta {
        ast_records: KeyDelta {
            inserted: inserted.into(),
            updated: Arc::from([]),
            removed: removed.into(),
        },
        syntax_payloads: KeyDelta {
            inserted: syntax_inserted.into(),
            updated: {
                let mut sorted = payload_updated.clone();
                sorted.sort_unstable();
                sorted.dedup();
                sorted.into()
            },
            removed: syntax_removed.into(),
        },
        parents: KeyDelta::default(),
        child_splices: Arc::from([]),
        roots: OrderedDelta::default(),
        // Deterministic synthesized-token identities recorded during
        // recovery this command (plan §14), keyed by stable occurrence.
        synthesized_tokens: {
            let inserted: Vec<SyntheticTokenId> = state
                .synthetic_tokens
                .values()
                .map(|identity| SyntheticTokenId(*identity))
                .collect();
            let mut sorted = inserted;
            sorted.sort_unstable();
            sorted.dedup();
            KeyDelta {
                inserted: sorted.into(),
                updated: Arc::from([]),
                removed: Arc::from([]),
            }
        },
        diagnostics,
        status,
        inserted_records: inserted_pairs.into(),
        removed_records: removed_pairs.into(),
        updated_records: {
            let mut pairs = updated_records.clone();
            pairs.sort_unstable();
            pairs.dedup();
            pairs.into()
        },
        live_records: Arc::new(live),
    };
    #[cfg(debug_assertions)]
    delta.assert_valid();
    Arc::new(delta)
}

/// Computes the deterministic ordered child splice transforming `old` into
/// `new` (plan §9): trims the common prefix/suffix and reports the bounded
/// middle plus the full resulting order.
fn ordered_splice(old: &[SyntaxNodeId], new: &[SyntaxNodeId]) -> OrderedDelta<SyntaxNodeId> {
    let mut prefix = 0;
    while prefix < old.len() && prefix < new.len() && old[prefix] == new[prefix] {
        prefix += 1;
    }
    let mut suffix = 0;
    while suffix < old.len().saturating_sub(prefix)
        && suffix < new.len().saturating_sub(prefix)
        && old[old.len() - 1 - suffix] == new[new.len() - 1 - suffix]
    {
        suffix += 1;
    }
    let removed = old[prefix..old.len() - suffix].to_vec();
    let inserted = new[prefix..new.len() - suffix].to_vec();
    OrderedDelta {
        before: prefix.checked_sub(1).and_then(|i| new.get(i).copied()),
        removed: removed.into(),
        inserted: inserted.into(),
        after: new.get(new.len() - suffix).copied(),
        order_after: new.to_vec().into(),
    }
}
fn status_of(infos: &[ParseErrorInfo], no_acceptance: bool) -> ParsedStatus {
    if infos.is_empty() {
        ParsedStatus::Clean
    } else if no_acceptance {
        ParsedStatus::Unrecovered { regions: infos.len() }
    } else {
        ParsedStatus::Recovered { segments: infos.len() }
    }
}

/// Shape proxy for proven-corresponding records: equal green structure
/// and extent means every parser-visible fact is identical (token values
/// live outside parser identity — plan §21).
fn record_signature(
    ast: &AstArena,
    products: &ProductArena,
    record: u64,
) -> Option<(usize, AnchoredSpan)> {
    let owner = ast.product_of(record as usize)?;
    let green = products.get(owner)?.green;
    let extent = ast.extent_of_id(record as usize)?;
    Some((green, extent))
}

fn sort_dedup(values: &mut Vec<u64>) {
    values.sort_unstable();
    values.dedup();
}

/// Exact multiset delta over content-addressed diagnostic identities
/// (plan §14): equal facts never appear; ordinal disambiguates byte-equal
/// duplicates deterministically.
fn diagnostic_delta(
    previous: &[ParseErrorInfo],
    current: &[ParseErrorInfo],
) -> KeyDelta<ParseDiagnosticKey> {
    fn keys(infos: &[ParseErrorInfo]) -> BTreeSet<ParseDiagnosticKey> {
        infos
            .iter()
            .map(|info| {
                // Content-addressed identity: equal facts share one key and
                // can never appear as spurious updates; different facts get
                // different keys deterministically regardless of list order.
                let content = format!(
                    "{:?}|{:?}|{:?}|{:?}|{}|{:?}",
                    info.kind,
                    info.node,
                    info.unexpected,
                    info.expected,
                    info.recovered,
                    info.location
                );
                ParseDiagnosticKey {
                    document_id: 0,
                    ordinal: fnv64(content.as_bytes()),
                }
            })
            .collect()
    }
    let previous_keys = keys(previous);
    let current_keys = keys(current);
    KeyDelta {
        inserted: current_keys.difference(&previous_keys).copied().collect(),
        updated: Arc::from([]),
        removed: previous_keys.difference(&current_keys).copied().collect(),
    }
}

fn fnv64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

impl<Root: LexerRoot + Clone> Parser<Root> {
    pub(crate) fn parse_delta_batch(
        &mut self,
        working: &mut ParserSnapshotState,
        uri: fluent_uri::Uri<String>,
        plan: ReplayPlan,
    ) -> Result<(), ParseError> {
        let total_start = Instant::now();
        let plan_elapsed = Duration::default();

        let session_setup_start = Instant::now();
        let previous_tree_facts = working
            .tree_facts
            .get(&uri)
            .cloned()
            .unwrap_or_else(|| Arc::new(ParserTreeFacts::default()));
        let arenas = self
            .session_arenas
            .entry(uri.clone())
            .or_insert_with(|| SessionArenas {
                trees: TreeArena::new(),
                products: ProductArena::new(),
                ast: Arc::new(AstArena::new(uri.clone())),
                gss: GssArena::new(),
            });
        let state = Arc::make_mut(working.sessions.entry(uri.clone()).or_default());
        if state.columns.is_empty() {
            let start = arenas.gss.node(0, 0, 0);
            state.columns = vec![ParseColumn::new(None, IndexSet::from([start]))];
        }
        let mut session_ctx = SessionContext {
            uri: uri.clone(),
            state,
            trees: &mut arenas.trees,
            products: &mut arenas.products,
            ast: Arc::make_mut(&mut arenas.ast),
            gss: &mut arenas.gss,
            grammar: &self.grammar,
            actions: &self.actions,
            gotos: &self.gotos,
            error_recovery: self.config.error_recovery,
        };
        // The record journal and lineage windows are command-scoped
        // (plan §9.1): stale entries from a prior command would corrupt
        // this command's delta.
        session_ctx.state.record_journal.clear();
        session_ctx.state.lineage.begin_command();
        // Deterministic synthetic-token identity (plan §14): the document
        // serial is the stable URI hash; this command's synthetic tokens
        // restart empty.
        session_ctx.state.document_serial =
            crate::framework::parse::data::ast::document_key(&uri);
        session_ctx.state.synthetic_tokens.clear();
        session_ctx.state.active_recovery_segment = None;
        let gss_before = session_ctx.gss.node_count();
        let products_before = session_ctx.products.products.len();
        let ast_before = session_ctx.ast.len();
        let session_setup_elapsed = session_setup_start.elapsed();

        // Recovery may add physical columns without consuming a token. Keep the
        // token-to-column map explicit instead of treating both axes as equal.
        let old_boundary_columns = std::iter::once(Some(0))
            .chain((0..plan.old_extent).map(|rank| {
                plan.old_unit(rank)
                    .and_then(|token| session_ctx.state.token_columns.get(&token.column).copied())
            }))
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
            // The staged copy shares its columns with the kept prefix;
            // truncate_to_column (below) releases the overlapping range's
            // record segments when it drops the tail.
            session_ctx.state.columns[old_column_base..].to_vec()
        } else {
            // The split removes these columns from the working session:
            // their record segments leave the live set until suffix
            // reattachment restores them (plan §9.2).
            let split = session_ctx.state.columns.split_off(old_column_base);
            for column in &split {
                session_ctx.state.drop_column_records(column);
            }
            split
        };
        let mut old_suffix_is_clean = vec![true; old_suffix_columns.len() + 1];
        for index in (0..old_suffix_columns.len()).rev() {
            old_suffix_is_clean[index] =
                old_suffix_is_clean[index + 1] && old_suffix_columns[index].token.is_some();
        }
        let reusable_len = plan
            .old_extent
            .saturating_sub(plan.old_reuse_start)
            .min(plan.new_extent.saturating_sub(plan.new_reuse_start));
        let token_columns = (0..reusable_len)
            .filter_map(|offset| {
                let old = plan.old_unit(plan.old_reuse_start + offset)?;
                let new = plan.new_unit(plan.new_reuse_start + offset)?;
                Some((old.column, new.column))
            })
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

        let token_materialization_elapsed = Duration::default();

        let eof = self.grammar.eof;
        let mut stats = IncrementalParseStats::default();
        let replay_start = Instant::now();
        let mut reduce_elapsed = Duration::default();
        let mut shift_elapsed = Duration::default();
        let mut recover_elapsed = Duration::default();
        let mut converge_elapsed = Duration::default();
        let mut reuse_timing = ReuseTiming::default();
        let replay_len = plan.new_extent.saturating_sub(restart_token_boundary);
        let mut cursor = plan.new.cursor_at(restart_token_boundary);
        let mut decoded = 0usize;
        let mut recovery_decoded = 0usize;
        let mut i = 0usize;
        while i < replay_len {
            let rank = cursor.rank();
            let token = decode_data(
                cursor
                    .current()
                    .expect("replay rank is within the parser token root"),
                session_ctx.grammar,
            );
            decoded = decoded.saturating_add(1);
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
                (column, rank),
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

            if token.terminal == session_ctx.grammar.error_terminal {
                let recover_start = Instant::now();
                let recovery_state = session_ctx.state.clone();
                let mut tail = TokenTail::new(&plan.new, rank, session_ctx.grammar);
                let recovery = session_ctx.recover_tokens(0, &mut tail);
                recovery_decoded = recovery_decoded.saturating_add(tail.decoded());
                let recovery = match recovery {
                    Ok(value) => value,
                    Err(ParseError::NoActiveStacks { .. }) => {
                        *session_ctx.state = recovery_state;
                        None
                    }
                    Err(error) => return Err(error),
                };
                if let Some(consumed) = recovery {
                    recover_elapsed += recover_start.elapsed();
                    if consumed == 0 {
                        continue;
                    }
                    for _ in 0..consumed {
                        let _ = cursor.advance();
                    }
                    i = i.saturating_add(consumed);
                    continue;
                }
                recover_elapsed += recover_start.elapsed();
            }

            let shift_start = Instant::now();
            if let Err(ParseError::NoActiveStacks { .. }) =
                session_ctx.shift_parse_token(column, &token)
            {
                shift_elapsed += shift_start.elapsed();
                let recover_start = Instant::now();
                let recovery_state = session_ctx.state.clone();
                let mut tail = TokenTail::new(&plan.new, rank, session_ctx.grammar);
                let recovery = session_ctx.recover_tokens(0, &mut tail);
                recovery_decoded = recovery_decoded.saturating_add(tail.decoded());
                let recovery = match recovery {
                    Ok(value) => value,
                    Err(ParseError::NoActiveStacks { .. }) => {
                        *session_ctx.state = recovery_state;
                        None
                    }
                    Err(error) => return Err(error),
                };
                if let Some(consumed) = recovery {
                    recover_elapsed += recover_start.elapsed();
                    if consumed == 0 {
                        continue;
                    }
                    for _ in 0..consumed {
                        let _ = cursor.advance();
                    }
                    i = i.saturating_add(consumed);
                    continue;
                }
                recover_elapsed += recover_start.elapsed();
                session_ctx.delete_parse_token(column, &token)?;
                let _ = cursor.advance();
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

            let _ = cursor.advance();
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
        working.incremental_stats.insert(uri.clone(), stats);
        crate::framework::workspace::record_parser_work(&uri.to_string(), |work| {
            work.restart_columns += restart_boundary as u64;
            work.tokens_decoded += (decoded + recovery_decoded) as u64;
            work.tokens_replayed += decoded as u64;
            work.columns_replayed += reparsed as u64;
            work.columns_reused += reused as u64;
            work.segments_attached += u64::from(stats.reconverged_old_boundary.is_some());
            // Honest §19 gate: the number of retained suffix columns the
            // reuse path physically rewrites (currently the whole reused
            // tail; the ParseSegment Arc-share design in handoff §12 is what
            // drives this to zero).
            work.suffix_columns_physically_visited += stats.suffix_rewritten as u64;
            work.checkpoint_comparisons += stats.checkpoint_matches as u64;
            work.frontier_comparisons += stats.frontier_matches as u64;
            work.gss_records_created +=
                (session_ctx.gss.node_count().saturating_sub(gss_before)) as u64;
            work.product_records_created +=
                (session_ctx.products.products.len().saturating_sub(products_before)) as u64;
            work.ast_records_created += (session_ctx.ast.len().saturating_sub(ast_before)) as u64;
            work.eof_replays += u64::from(stats.reconverged_old_boundary.is_none());
            work.recovery_columns += recovery_columns as u64;
        });

        // Journal-first record delta (plan §9.1–§9.2). The replay journal
        // holds every record whose segment reference count changed, mapped
        // to its final live state. Applying the journal to the previous
        // live set costs O(delta × log n) — never a whole-tree walk. The
        // root record still comes from the accepted root product.
        let mut tree_root = None;
        for (index, root) in roots_after.iter().copied().enumerate() {
            let Some(product) = session_ctx.products.get(root) else {
                continue;
            };
            if index == 0 {
                tree_root = match &product.data {
                    ProductData::Node { ast, .. }
                    | ProductData::Token { ast: Some(ast), .. } => Some(*ast as u64),
                    ProductData::Error { .. } | ProductData::Token { ast: None, .. } => {
                        product.ast_ids.first().map(|id| *id as u64)
                    }
                };
            }
        }
        // Freeze (plan §9): coalesce the journal into the canonical exact
        // ParseDelta. The persistent live set applies only journaled keys.
        session_ctx
            .state
            .lineage
            .resolve_contexts(session_ctx.products, session_ctx.ast);
        let current_records = (*previous_tree_facts.records).clone();
        let previous_infos = working
            .published_diagnostics
            .get(&uri)
            .map(|infos| infos.as_ref().clone())
            .unwrap_or_default();
        let current_infos = collect_parse_diagnostics(
            session_ctx.state,
            Some(crate::framework::parse::diagnostics::DiagnosticArenas {
                trees: &*session_ctx.trees,
                products: &*session_ctx.products,
            }),
            &roots_after,
        );
        let previous_status = working.published_status.get(&uri).copied();
        let current_status = status_of(
            &current_infos,
            roots_after.is_empty(),
        );
        session_ctx
            .state
            .lineage
            .resolve_contexts(session_ctx.products, session_ctx.ast);
        let tree_delta = freeze_parse_delta(
            &roots_after,
            &mut *session_ctx.state,
            session_ctx.products,
            session_ctx.ast,
            &previous_tree_facts,
            tree_root,
            current_records,
            &previous_infos,
            current_infos.clone(),
            previous_status,
            current_status.clone(),
        );
        if !tree_delta.is_empty() {
            let next = working
                .semantic_revisions
                .entry(uri.clone())
                .or_insert(0);
            *next = next.saturating_add(1);
        }
        crate::framework::workspace::record_parser_work(&uri.to_string(), |work| {
            work.parser_records_inserted = tree_delta.ast_records.inserted.len() as u64;
            work.parser_records_removed = tree_delta.ast_records.removed.len() as u64;
        });
        working
            .published_status
            .insert(uri.clone(), current_status);
        working
            .published_diagnostics
            .insert(uri.clone(), Arc::new(current_infos));
        let current_tree_facts = Arc::new(ParserTreeFacts {
            records: Arc::clone(tree_delta.live_records()),
            root: tree_root,
        });
        // The journal is command-scoped: clear it so the next command's
        // delta starts from a clean slate.
        session_ctx.state.record_journal.clear();
        working
            .tree_facts
            .insert(uri.clone(), current_tree_facts);
        working.tree_deltas.insert(uri.clone(), tree_delta);
        let stats_elapsed = stats_start.elapsed();

        working.roots.insert(uri.clone(), Arc::new(roots_after));
        working.tokens.insert(uri.clone(), Arc::clone(&plan.new));

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
            decoded,
            stats.reconverged_old_boundary.is_some(),
            plan.old_extent,
            plan.new_extent,
            plan.prefix_len,
            plan.suffix_len,
        );

        Ok(())
    }
}
