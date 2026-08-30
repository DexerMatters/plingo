use indexmap::IndexSet;
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    sync::Arc,
    time::{Duration, Instant},
};

use super::{
    OccurrenceKey, OriginKey, ParseColumn, ParseError, ParseToken, ParserSessionState,
    RecoveryJournalEntry, ReplayPlan, SeamBinding, SessionContext, TokenTail, checkpoint,
};
use crate::framework::{
    lex::{LexerRoot, TokenOccurrenceId},
    parse::{
        IncrementalParseStats, Parser,
        data::{
            ast::{AnchoredSpan, AstArena},
            green::ParseErrorInfo,
            gss::CanonicalFrontierCache,
            product::{ProductArena, ProductData, ProductId},
        },
        delta::{
            ChildSplice, KeyDelta, OrderedDelta, ParseDelta, ParseDiagnosticKey, ParsedStatus,
            SyntaxNodeId, SyntheticTokenId,
        },
        diagnostics::collect_parse_diagnostics,
        types::{
            ParserBoundaryId, ParserDocumentRoot, ParserTreeFacts, ProductReachKey, RecordReachKey,
            RecordTransition,
        },
    },
};

struct ReusableSuffix {
    segment: Arc<super::ParseSegment>,
    column_base: usize,
}

impl ReusableSuffix {
    fn len(&self) -> usize {
        self.segment.len()
    }

    fn column(&self, index: usize) -> Option<&ParseColumn> {
        self.segment.column(index)
    }

    fn frontier(&self, index: usize) -> Option<&checkpoint::FrontierCheckpoint> {
        self.segment.frontier(index)
    }

    fn is_clean_from(&self, index: usize) -> bool {
        self.segment.is_clean_from(index)
    }

    fn products_cache_stable(&self) -> bool {
        self.segment.products_cache_stable()
    }
}

/// Phase accounting for exact parser convergence. These durations are emitted
/// with each replay so a logical suffix reuse can be distinguished from its
/// bounded seam-binding cost.
#[derive(Default)]
struct ReuseTiming {
    checkpoint: Duration,
    frontier_match: Duration,
    tail_validation: Duration,
    seam_binding: Duration,
}

/// Materialize only the bounded product closure needed to bind a retained
/// segment's logical root to the current replay generation. The source
/// columns and their products stay immutable; this map is the seam overlay.
fn bind_product(
    old: ProductId,
    mapping: &mut HashMap<ProductId, ProductId>,
    session_ctx: &mut SessionContext<'_>,
    token_remap: &dyn Fn(ProductId) -> Option<TokenOccurrenceId>,
) -> Result<Option<ProductId>, ParseError> {
    if let Some(product) = mapping.get(&old).copied() {
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
        // A retained token product resolves through its occurrence: an
        // unchanged occurrence keeps the cache-stable product; the edited
        // occurrence maps to the current replay's replacement product.
        let target = token_remap(old).and_then(|occ| session_ctx.state.token_product(occ));
        let target = target.unwrap_or(old);
        mapping.insert(old, target);
        return Ok(Some(target));
    }
    if matches!(data, ProductData::Error { .. }) {
        // Recovery products carry no reduction origin. The seam only lands
        // after every edited occurrence, so a retained error region is
        // validated-unchanged and keeps its products verbatim.
        mapping.insert(old, old);
        return Ok(Some(old));
    }
    let Some(origin) = session_ctx
        .state
        .reduction_origins
        .get(&OriginKey(old))
        .cloned()
    else {
        return Ok(None);
    };
    let mut children = Vec::with_capacity(origin.children.len());
    for &child in &origin.children {
        let Some(child) = bind_product(child, mapping, session_ctx, token_remap)? else {
            return Ok(None);
        };
        children.push(child);
    }
    let product = session_ctx.reduce_cached(
        origin.production,
        &children,
        origin.boundary.unwrap_or(TokenOccurrenceId(u64::MAX)),
    )?;
    mapping.insert(old, product);
    Ok(Some(product))
}

/// Tries to replay a journaled recovery region instead of re-running the
/// bounded search (plan §14). The proof is exact: the triggering occurrence,
/// per-witness token identity (existence plus terminal), and exact frontier
/// equality at the error column. A dirty witness invalidates the entry.
fn try_reuse_recovery_journal(
    plan: &ReplayPlan,
    session_ctx: &mut SessionContext<'_>,
    trigger: TokenOccurrenceId,
    tail: &mut TokenTail,
    _stats: &mut IncrementalParseStats,
    frontier_cache: &mut CanonicalFrontierCache,
) -> Result<Option<usize>, ParseError> {
    use crate::framework::parse::recovery::Repair;
    let candidates: Vec<(u64, Arc<RecoveryJournalEntry>)> = session_ctx
        .state
        .recovery_journal
        .iter()
        .filter(|(_, entry)| entry.anchor == trigger)
        .map(|(key, entry)| (key.0, Arc::clone(entry)))
        .collect();
    for (serial, entry) in candidates {
        // Witness identity: every consumed token must still exist under the
        // same occurrence with the same terminal; otherwise the region is
        // dirty and the journal entry is invalidated.
        let mut dirty = false;
        for (occurrence, terminal) in &entry.witnesses {
            let matches = plan
                .new
                .rank_of_occurrence(*occurrence)
                .and_then(|r| plan.new.token_at(r))
                .is_some_and(|data| data.terminal == Some(*terminal));
            if !matches {
                dirty = true;
                break;
            }
        }
        if dirty {
            session_ctx
                .state
                .recovery_journal
                .remove(&OccurrenceKey(serial));
            crate::framework::workspace::record_parser_work(&session_ctx.uri.to_string(), |work| {
                work.recovery_segments_invalidated += 1;
            });
            continue;
        }
        // Anchor alignment: each repair must land on exactly the token the
        // proven plan named, in order.
        let mut aligned = true;
        let mut probe = 0usize;
        for (repair, anchor) in &entry.repairs {
            match tail.get(probe) {
                Some(token) if token.column == *anchor => {
                    if !matches!(repair, Repair::Insert(_)) {
                        probe += 1;
                    }
                }
                _ => {
                    aligned = false;
                    break;
                }
            }
        }
        if !aligned {
            continue;
        }
        // Exact frontier equality at the re-entered error column.
        let frontier = {
            let column_index = session_ctx.state.current_column();
            checkpoint::frontier_checkpoint_for_column(
                &mut session_ctx.state.columns[column_index],
                session_ctx.gss,
                session_ctx.products,
                frontier_cache,
            )
        };
        if !frontier.exact_match(&entry.frontier) {
            continue;
        }
        return session_ctx.replay_recovery_journal(serial, &entry, 0, tail);
    }
    Ok(None)
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
    frontier_cache: &mut CanonicalFrontierCache,
) -> Result<bool, ParseError> {
    let (current_column, current_rank) = current;
    if current_rank < plan.new_reuse_start {
        return Ok(false);
    }
    stats.convergence_checks += 1;
    crate::framework::workspace::record_parser_work(&session_ctx.uri.to_string(), |work| {
        work.convergence_candidates += 1;
    });

    let current_occurrence = (current_rank < plan.new.semantic_len())
        .then(|| plan.new_unit(current_rank).map(|token| token.column))
        .flatten();
    let boundary = ParserBoundaryId::Source(plan.new.boundary_at_rank(current_rank));
    session_ctx.state.columns[current_column].set_boundary(Some(boundary));
    let old_index = old.segment.boundary_column(boundary);
    let old_occurrence = old_index
        .and_then(|index| old.column(index))
        .and_then(|column| column.token);
    let selected_old_column_anchor = old_occurrence.map(|occurrence| occurrence.0);
    let current_column_anchor = session_ctx.state.columns[current_column]
        .token
        .map(|occurrence| occurrence.0);
    stats.boundary_trace = Some(crate::framework::parse::BoundaryTrace {
        current_lookahead_occurrence: current_occurrence.map(|occurrence| occurrence.0),
        current_column_anchor,
        selected_old_occurrence: old_occurrence.map(|occurrence| occurrence.0),
        selected_old_column_anchor,
    });
    // Retained recovery columns no longer block reuse: the state-frontier
    // proof below establishes an equal LR configuration, and the parser is
    // deterministic over identical inputs, so the frozen recovery regions
    // are exactly what a fresh replay would produce (plan §14 reuse).
    let Some(old_index) = old_index.filter(|&index| index < old.len()) else {
        return Ok(false);
    };

    let checkpoint_start = Instant::now();
    let current_frontier = {
        let current_column = &mut session_ctx.state.columns[current_column];
        checkpoint::frontier_checkpoint_for_column(
            current_column,
            session_ctx.gss,
            session_ctx.products,
            frontier_cache,
        )
        .clone()
    };
    let Some(old_frontier) = old.frontier(old_index).cloned() else {
        return Ok(false);
    };
    crate::framework::workspace::record_parser_work(&session_ctx.uri.to_string(), |work| {
        work.checkpoint_exact_comparisons += 1;
        work.checkpoint_comparisons += 1;
    });
    let exact_checkpoint = current_frontier.exact_match(&old_frontier);
    if !exact_checkpoint {
        // State-level convergence (plan §5.6): the exact product key differs
        // (or is absent across cyclic frontiers), but an equal post-reduction
        // LR configuration parses the retained suffix identically — the
        // parser is deterministic over identical states and inputs. The
        // paired traversal maps the differing token products by position;
        // the reduction cache and the token remap resolve the rest during
        // attachment.
        if current_frontier.anchor != old_frontier.anchor
            || current_frontier.error_derived != old_frontier.error_derived
        {
            timing.checkpoint += checkpoint_start.elapsed();
            return Ok(false);
        }
        let Some(old_column) = old.column(old_index) else {
            timing.checkpoint += checkpoint_start.elapsed();
            return Ok(false);
        };
        let current_parse_column = &session_ctx.state.columns[current_column];
        let old_active = old_column.active_nodes().collect::<Vec<_>>();
        let new_active = current_parse_column.active_nodes().collect::<Vec<_>>();
        let Some(seeded) = session_ctx.gss.match_state_frontiers(
            &old_active,
            &new_active,
            session_ctx.products,
            session_ctx.trees,
            frontier_cache,
        ) else {
            timing.checkpoint += checkpoint_start.elapsed();
            return Ok(false);
        };
        timing.checkpoint += checkpoint_start.elapsed();
        stats.checkpoint_matches += 1;
        stats.frontier_matches += 1;
        crate::framework::workspace::record_parser_work(&session_ctx.uri.to_string(), |work| {
            work.checkpoint_matches += 1;
            work.frontier_matches += 1;
        });
        let seam_start = Instant::now();
        let tail = old.segment.slice(old_index + 1..old.len());
        let mut products = seeded;
        let mut accepted = Vec::with_capacity(tail.raw_accepted().len());
        for &product in tail.raw_accepted() {
            // A bind failure — including a rebuild whose mapped child no
            // longer satisfies the old production's token kinds — rejects
            // the seam; the replay continues conservatively.
            let mapped = match bind_product(product, &mut products, session_ctx, &|id| {
                old.segment.product_occurrence(id)
            }) {
                Ok(Some(mapped)) => mapped,
                Ok(None) | Err(ParseError::Build(_)) => return Ok(false),
                Err(error) => return Err(error),
            };
            accepted.push(mapped);
        }
        let seam = Arc::new(SeamBinding::from_map(products));
        let attached = tail.attach_seam(seam, accepted.into());
        timing.seam_binding += seam_start.elapsed();
        stats.reconverged_new_boundary = Some(current_column);
        stats.reconverged_old_boundary = Some(old.column_base + old_index);
        session_ctx.state.append_reused_segment(attached);
        return Ok(true);
    }
    timing.checkpoint += checkpoint_start.elapsed();
    stats.checkpoint_matches += 1;
    crate::framework::workspace::record_parser_work(&session_ctx.uri.to_string(), |work| {
        work.checkpoint_matches += 1;
    });

    let frontier_start = Instant::now();
    let Some(old_column) = old.column(old_index) else {
        return Ok(false);
    };
    let current_parse_column = &session_ctx.state.columns[current_column];
    let old_base = old_column.base_active_nodes().collect::<Vec<_>>();
    let old_active = old_column.active_nodes().collect::<Vec<_>>();
    let new_base = current_parse_column.base_active_nodes().collect::<Vec<_>>();
    let new_active = current_parse_column.active_nodes().collect::<Vec<_>>();
    let Some((frontier_nodes, frontier_products, shared_prefix)) =
        session_ctx.gss.match_canonical_frontiers_cached(
            (&old_base, &old_active),
            (&new_base, &new_active),
            session_ctx.products,
            frontier_cache,
        )
    else {
        // State-level convergence (plan §5.6): the exact product key differs
        // because the edited value produced different products below the
        // seam, but the paired LR configuration is equal, so the retained
        // suffix parses identically. The paired traversal maps the differing
        // token products by position; the reduction cache plus the token
        // remap resolve the rest during attachment.
        let Some(seeded) = session_ctx.gss.match_state_frontiers(
            &old_active,
            &new_active,
            session_ctx.products,
            session_ctx.trees,
            frontier_cache,
        ) else {
            timing.frontier_match += frontier_start.elapsed();
            return Ok(false);
        };
        timing.frontier_match += frontier_start.elapsed();
        stats.frontier_matches += 1;
        let seam_start = Instant::now();
        let tail = old.segment.slice(old_index + 1..old.len());
        let mut products = seeded;
        let mut accepted = Vec::with_capacity(tail.raw_accepted().len());
        for &product in tail.raw_accepted() {
            // A bind failure — including a rebuild whose mapped child no
            // longer satisfies the old production's token kinds — rejects
            // the seam; the replay continues conservatively.
            let mapped = match bind_product(product, &mut products, session_ctx, &|id| {
                old.segment.product_occurrence(id)
            }) {
                Ok(Some(mapped)) => mapped,
                Ok(None) | Err(ParseError::Build(_)) => return Ok(false),
                Err(error) => return Err(error),
            };
            accepted.push(mapped);
        }
        let seam = Arc::new(SeamBinding::from_map(products));
        let attached = tail.attach_seam(seam, accepted.into());
        timing.seam_binding += seam_start.elapsed();
        stats.reconverged_new_boundary = Some(current_column);
        stats.reconverged_old_boundary = Some(old.column_base + old_index);
        session_ctx.state.append_reused_segment(attached);
        return Ok(true);
    };
    timing.frontier_match += frontier_start.elapsed();
    stats.frontier_matches += 1;
    crate::framework::workspace::record_parser_work(&session_ctx.uri.to_string(), |work| {
        work.frontier_matches += 1;
    });
    let identity_frontier = shared_prefix
        && frontier_nodes.iter().all(|(old, new)| old == new)
        && frontier_products.iter().all(|(old, new)| old == new);
    // An identity frontier proves that the immutable tail's existing GSS
    // edges remain valid. No retained column needs to be read.
    if identity_frontier && old.products_cache_stable() {
        let tail = old.segment.slice(old_index + 1..old.len());
        stats.reconverged_new_boundary = Some(current_column);
        stats.reconverged_old_boundary = Some(old.column_base + old_index);
        session_ctx.state.append_reused_segment(tail);
        return Ok(true);
    }

    // Attach a bounded seam overlay to the immutable tail. The source
    // columns are never rewritten; only the accepted roots' product closure
    // is materialized in the current generation.
    let seam_start = Instant::now();
    let tail = old.segment.slice(old_index + 1..old.len());
    let mut products = frontier_products;
    let mut accepted = Vec::with_capacity(tail.raw_accepted().len());
    for &product in tail.raw_accepted() {
        let Some(mapped) = bind_product(product, &mut products, session_ctx, &|id| {
            old.segment.product_occurrence(id)
        })?
        else {
            return Ok(false);
        };
        accepted.push(mapped);
    }
    let seam = Arc::new(SeamBinding::from_map(products));
    let attached = tail.attach_seam(seam, accepted.into());
    timing.seam_binding += seam_start.elapsed();

    stats.reconverged_new_boundary = Some(current_column);
    stats.reconverged_old_boundary = Some(old.column_base + old_index);
    session_ctx.state.append_reused_segment(attached);
    Ok(true)
}
/// The accepted-root reachability update for one parser command.
///
/// Parser columns and retained suffixes are a cache domain. Only the
/// symmetric difference of the old/new accepted-root multisets changes this
/// domain, so a value-only edit that leaves the accepted roots unchanged does
/// not walk parser records or the AST.
struct ReachabilityUpdate {
    live_records: crate::reactive::store::RadixMap<()>,
    product_reach_counts: crate::reactive::store::Hamt<ProductReachKey, u32>,
    record_reach_counts: crate::reactive::store::Hamt<RecordReachKey, u32>,
    record_journal: BTreeMap<u64, RecordTransition>,
}

fn checked_reach_count(
    kind: &'static str,
    key: u64,
    before: u32,
    delta: i64,
) -> Result<u32, ParseError> {
    let after = i64::from(before)
        .checked_add(delta)
        .ok_or(ParseError::InvalidReachability {
            kind,
            key,
            before,
            delta,
        })?;
    if !(0..=i64::from(u32::MAX)).contains(&after) {
        return Err(ParseError::InvalidReachability {
            kind,
            key,
            before,
            delta,
        });
    }
    Ok(after as u32)
}

fn add_pending_product(
    pending: &mut BTreeMap<ProductId, i64>,
    product: ProductId,
    delta: i64,
) -> Result<(), ParseError> {
    if delta == 0 {
        return Ok(());
    }
    let entry = pending.entry(product).or_default();
    *entry = entry
        .checked_add(delta)
        .ok_or(ParseError::InvalidReachability {
            kind: "product-pending",
            key: product as u64,
            before: 0,
            delta,
        })?;
    if *entry == 0 {
        pending.remove(&product);
    }
    Ok(())
}

/// Applies an accepted-root multiset delta to the persistent product/record
/// reach domains. A product's children are adopted/released only when that
/// product crosses zero; this preserves multiplicity for shared parents
/// without recursively re-walking an already reachable subtree.
fn apply_accepted_root_delta(
    previous: &ParserTreeFacts,
    previous_roots: &[ProductId],
    current_roots: &[ProductId],
    products: &ProductArena,
) -> Result<ReachabilityUpdate, ParseError> {
    let mut product_reach_counts = (*previous.product_reach_counts).clone();
    let mut record_reach_counts = (*previous.record_reach_counts).clone();
    let mut pending = BTreeMap::<ProductId, i64>::new();
    for &product in previous_roots {
        add_pending_product(&mut pending, product, -1)?;
    }
    for &product in current_roots {
        add_pending_product(&mut pending, product, 1)?;
    }

    let mut record_journal = BTreeMap::<u64, RecordTransition>::new();
    while let Some(product) = pending.keys().next_back().copied() {
        let delta = pending
            .remove(&product)
            .expect("pending product key disappeared");
        if delta == 0 {
            continue;
        }
        let key = ProductReachKey(product);
        let before = product_reach_counts.get(&key).copied().unwrap_or(0);
        let after = checked_reach_count("product", product as u64, before, delta)?;
        if after == before {
            continue;
        }

        let crossed_into_live = before == 0 && after > 0;
        let crossed_dead = before > 0 && after == 0;
        if after == 0 {
            product_reach_counts.remove(&key);
        } else {
            product_reach_counts.insert(key, after);
        }

        if crossed_into_live || crossed_dead {
            if let Some(record) = super::product_direct_record(products, product) {
                let record_key = RecordReachKey(record);
                let record_before = record_reach_counts.get(&record_key).copied().unwrap_or(0);
                let record_delta = if crossed_into_live { 1 } else { -1 };
                let record_after =
                    checked_reach_count("record", record, record_before, record_delta)?;
                if record_after == 0 {
                    record_reach_counts.remove(&record_key);
                } else {
                    record_reach_counts.insert(record_key, record_after);
                }
                record_journal
                    .entry(record)
                    .and_modify(|transition| transition.after_count = record_after)
                    .or_insert(RecordTransition {
                        before_count: record_before,
                        after_count: record_after,
                    });
            }

            let Some(product_data) = products.get(product).map(|product| &product.data) else {
                return Err(ParseError::InvalidReachability {
                    kind: "missing-product",
                    key: product as u64,
                    before,
                    delta,
                });
            };
            let children: &[ProductId] = match product_data {
                ProductData::Error { children } | ProductData::Node { children, .. } => children,
                ProductData::Token { .. } => &[],
            };
            for &child in children {
                if child >= product {
                    return Err(ParseError::InvalidReachability {
                        kind: "non-topological-edge",
                        key: product as u64,
                        before,
                        delta: child as i64,
                    });
                }
                add_pending_product(&mut pending, child, if crossed_into_live { 1 } else { -1 })?;
            }
        }
    }

    let mut live_records = (*previous.records).clone();
    for (&record, transition) in &record_journal {
        match (transition.before_count, transition.after_count) {
            (0, after) if after > 0 => {
                live_records.insert(record, ());
            }
            (before, 0) if before > 0 => {
                if !live_records.remove(record) {
                    return Err(ParseError::InvalidReachability {
                        kind: "record-live-map",
                        key: record,
                        before,
                        delta: -i64::from(before),
                    });
                }
            }
            _ => {}
        }
    }
    #[cfg(debug_assertions)]
    {
        let (expected_products, expected_records) =
            slow_accepted_root_reach(current_roots, products)?;
        let actual_products: BTreeMap<ProductId, u32> = product_reach_counts
            .iter()
            .map(|(key, count)| (key.0, *count))
            .collect();
        let actual_records: BTreeMap<u64, u32> = record_reach_counts
            .iter()
            .map(|(key, count)| (key.0, *count))
            .collect();
        debug_assert_eq!(
            actual_products, expected_products,
            "accepted-root product reach counts diverged from slow oracle"
        );
        debug_assert_eq!(
            actual_records, expected_records,
            "accepted-root record reach counts diverged from slow oracle"
        );
        debug_assert_eq!(
            live_records
                .iter()
                .map(|(record, ())| record)
                .collect::<Vec<_>>(),
            expected_records.keys().copied().collect::<Vec<_>>(),
            "persistent live-record map diverged from accepted-root oracle"
        );
    }

    Ok(ReachabilityUpdate {
        live_records,
        product_reach_counts,
        record_reach_counts,
        record_journal,
    })
}
#[cfg(any(test, debug_assertions))]
/// Recomputes accepted-root reachability from scratch for debug and unit-test
/// validation. A product is expanded once when it first becomes reachable;
/// incoming root/edge multiplicity is still retained in its count.
fn slow_accepted_root_reach(
    roots: &[ProductId],
    products: &ProductArena,
) -> Result<(BTreeMap<ProductId, u32>, BTreeMap<u64, u32>), ParseError> {
    let mut product_counts = BTreeMap::<ProductId, u32>::new();
    let mut pending = Vec::new();
    for &root in roots {
        let before = product_counts.get(&root).copied().unwrap_or(0);
        let after = checked_reach_count("product", root as u64, before, 1)?;
        product_counts.insert(root, after);
        if before == 0 {
            pending.push(root);
        }
    }

    while let Some(product) = pending.pop() {
        let Some(product_data) = products.get(product).map(|product| &product.data) else {
            return Err(ParseError::InvalidReachability {
                kind: "missing-product",
                key: product as u64,
                before: 0,
                delta: 1,
            });
        };
        let children: &[ProductId] = match product_data {
            ProductData::Error { children } | ProductData::Node { children, .. } => children,
            ProductData::Token { .. } => &[],
        };
        for &child in children {
            if child >= product {
                return Err(ParseError::InvalidReachability {
                    kind: "non-topological-edge",
                    key: product as u64,
                    before: product_counts[&product],
                    delta: child as i64,
                });
            }
            let before = product_counts.get(&child).copied().unwrap_or(0);
            let after = checked_reach_count("product", child as u64, before, 1)?;
            product_counts.insert(child, after);
            if before == 0 {
                pending.push(child);
            }
        }
    }

    let mut record_counts = BTreeMap::<u64, u32>::new();
    for &product in product_counts.keys() {
        if let Some(record) = super::product_direct_record(products, product) {
            let before = record_counts.get(&record).copied().unwrap_or(0);
            let after = checked_reach_count("record", record, before, 1)?;
            record_counts.insert(record, after);
        }
    }
    Ok((product_counts, record_counts))
}

/// Freezes one command into the canonical [`ParseDelta`] (plan §9).
///
/// Membership is authoritative from the accepted roots: the union of the
/// accepted products' transitive AST records IS the live tree, so a
/// transiently live recovery region can never reach the published
/// domains. Journal-derived classification refines this where proven:
/// lineage-keyed payload/order classification is live, and the frozen delta
/// carries only genuinely inserted, updated, or removed keys.
#[allow(clippy::too_many_arguments)]
fn freeze_parse_delta(
    state: &mut ParserSessionState,
    products: &ProductArena,
    ast: &AstArena,
    previous: &ParserTreeFacts,
    previous_roots: &[ProductId],
    current_roots: &[ProductId],
    tree_root: Option<u64>,
    previous_infos: &[ParseErrorInfo],
    current_infos: Vec<ParseErrorInfo>,
    previous_status: Option<ParsedStatus>,
    current_status: ParsedStatus,
    tree_member_kind: Option<fn(&AstArena, u64) -> Option<u8>>,
    tree_child_records_fn: Option<fn(&AstArena, u64) -> Vec<u64>>,
) -> Result<(Arc<ParseDelta>, ReachabilityUpdate), ParseError> {
    let reachability =
        apply_accepted_root_delta(previous, previous_roots, current_roots, products)?;
    if std::env::var_os("PLINGO_TRACE_PARSER").is_some() {
        eprintln!(
            "freeze roots prev={previous_roots:?} curr={current_roots:?} journal={journal:?} live_before={live_before} live_after={live_after}",
            previous_roots = previous_roots,
            current_roots = current_roots,
            journal = reachability.record_journal,
            live_before = previous.records.len(),
            live_after = reachability.live_records.len()
        );
    }
    let live = reachability.live_records.clone();
    let current = &live;

    let mut inserted = Vec::new();
    let mut removed = Vec::new();
    for (&record, transition) in &reachability.record_journal {
        match (transition.before_count, transition.after_count) {
            (0, after) if after > 0 => inserted.push(record),
            (before, 0) if before > 0 => removed.push(record),
            _ => {}
        }
    }

    // Settle only the records whose accepted-root reachability changed.
    // Walking `previous.records` and `current` here made every local edit
    // proportional to the whole document.
    state.lineage.settle(
        &previous.records,
        current,
        &inserted,
        &removed,
        products,
        ast,
    );

    let mut computed_child_splices: Vec<ChildSplice> = Vec::new();
    // Retained-record splice oracle (Cut E): last published order per
    // parent lineage, held in a persistent path-copying radix root. A
    // command clones the root handle (O(1)) and path-copies only the
    // touched parents' entries.
    let tree_child_records = |record: usize| {
        let raw = tree_child_records_fn
            .map(|children| children(ast, record as u64))
            .unwrap_or_else(|| {
                crate::framework::parse::parsing::lineage::direct_tree_child_records(
                    products, ast, record,
                )
                .into_iter()
                .map(|child| child as u64)
                .collect()
            });
        raw.into_iter()
            .filter(|child| {
                tree_member_kind
                    .map(|kind| kind(ast, *child as u64).is_some())
                    .unwrap_or(true)
            })
            .collect::<Vec<_>>()
    };
    let mut next_child_orders = (*previous.child_orders).clone();
    // Dropped-lineage record resolution: prefer a LIVE record bearing the
    // lineage (a child that moved), else the command's death journal.
    let reverse_died: std::collections::HashMap<u64, u64> = state
        .lineage
        .iter_died()
        .map(|(record, lin)| (lin, record as u64))
        .collect();
    // ---- payload-update classification via stable lineage -------------
    // A record whose identity a dead predecessor provably carries is an
    // UPDATE under a retained key unless its shape (green + extent) is
    // identical, in which case no fact changed anywhere.
    let mut payload_updated: Vec<SyntaxNodeId> = Vec::new();
    let mut updated_records: Vec<(SyntaxNodeId, u64)> = Vec::new();
    let mut silent: std::collections::BTreeSet<u64> = std::collections::BTreeSet::new();
    {
        for &record in &inserted {
            let rec = record as usize;
            let Some(old_record) = state.lineage.inherited_from(rec) else {
                continue;
            };
            // The counterpart must have left the live tree; a surviving
            // twin resolves to local replacement (freshened in lineage).
            if current.get(old_record as u64).is_some() && old_record as u64 != record {
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
    syntax_inserted.sort_unstable();
    syntax_inserted.dedup();
    inserted_pairs.sort_unstable();
    inserted_pairs.dedup();
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
        .chain(inherited_carriers.iter().copied())
        .collect();
    let mut syntax_removed: Vec<SyntaxNodeId> = Vec::new();
    let mut removed_pairs: Vec<crate::framework::parse::delta::RemovedRecord> = Vec::new();
    for record in &removed {
        // Prefer the committed record→lineage map. The mutable lineage
        // journal can have already revived the cached arena record during
        // this replay, while the previous tree facts remain authoritative
        // for the exact removed key.
        let Some(lineage) = previous
            .record_lineages
            .get(*record)
            .copied()
            .or_else(|| state.lineage.died_lineage_of(*record))
        else {
            continue;
        };
        if carriers.contains(&lineage) {
            continue;
        }
        syntax_removed.push(SyntaxNodeId(lineage));
        let child_records: Vec<u64> = tree_child_records(*record as usize)
            .into_iter()
            .map(|child| child as u64)
            .collect();
        let parent_record = ast.parent_of(*record as usize).map(|parent| parent as u64);
        let parent_lineage = parent_record.and_then(|parent| {
            previous
                .record_lineages
                .get(parent)
                .copied()
                .or_else(|| state.lineage.died_lineage_of(parent))
                .or_else(|| state.lineage.lineage_of(parent as usize))
        });
        removed_pairs.push(crate::framework::parse::delta::RemovedRecord {
            lineage: SyntaxNodeId(lineage),
            record: *record,
            parent_record,
            parent_lineage,
            child_records: child_records.into(),
        });
    }
    {
        let mut sorted = syntax_removed.clone();
        sorted.sort_unstable();
        sorted.dedup();
        syntax_removed = sorted;
        removed_pairs.sort_unstable();
        removed_pairs.dedup();
        // ---- parents + child splices (plan §9 canonical domains) ---------
        // (child_splices declared at function scope so the delta carries them)
        // For each record whose identity a dead predecessor provably carries
        // (an update under a retained key), compare the predecessor's children

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
            let old_child_records = tree_child_records(old_record as usize);
            // Child list splice (Cut E): retain only the bounded changed
            // middle. Publication reconstructs the order from its previous
            // opaque node ids and these lineage/record pairs.
            let new_child_records_all = tree_child_records(rec);
            let old_pairs: Vec<(SyntaxNodeId, u64)> = old_child_records
                .iter()
                .filter_map(|&child| {
                    resolve_lineage(child as usize).map(|lin| (SyntaxNodeId(lin), child as u64))
                })
                .collect();
            let new_pairs: Vec<(SyntaxNodeId, u64)> = new_child_records_all
                .iter()
                .filter_map(|&child| {
                    resolve_lineage(child as usize).map(|lin| (SyntaxNodeId(lin), child as u64))
                })
                .collect();
            let old_children: Vec<SyntaxNodeId> =
                old_pairs.iter().map(|(lineage, _)| *lineage).collect();
            let new_children: Vec<SyntaxNodeId> =
                new_pairs.iter().map(|(lineage, _)| *lineage).collect();
            if old_children != new_children {
                let (prefix, suffix) = common_edges(&old_children, &new_children);
                let removed_children: Vec<(SyntaxNodeId, u64)> = old_pairs
                    [prefix..old_pairs.len() - suffix]
                    .iter()
                    .copied()
                    .collect();
                let inserted_children: Vec<(SyntaxNodeId, u64)> = new_pairs
                    [prefix..new_pairs.len() - suffix]
                    .iter()
                    .copied()
                    .collect();
                let current_lineages: std::collections::HashSet<u64> =
                    new_children.iter().map(|id| id.0).collect();
                let removed_children: Vec<(SyntaxNodeId, u64)> = removed_children
                    .into_iter()
                    .filter(|(lin, _record)| !current_lineages.contains(&lin.0))
                    .collect();
                computed_child_splices.push(ChildSplice {
                    parent: SyntaxNodeId(node_lineage),
                    delta: ordered_splice(&old_children, &new_children),
                    removed_children: removed_children.into(),
                    inserted_children: inserted_children.into(),
                });
            }
        }
        // ---- Cut E candidate parents (retained topology changes) --------
        // A RETAINED record never enters updated_records, so its child-list
        // change is detected through the touched records' parents compared
        // against the published-order oracle.
        let mut candidate_parents: Vec<(u64, u64)> = Vec::new();
        {
            let mut note = |parent_record: Option<usize>,
                            resolve: &dyn Fn(usize) -> Option<u64>| {
                let Some(parent_record) = parent_record else {
                    return;
                };
                let Some(parent_lineage) = resolve(parent_record) else {
                    return;
                };
                // The parent record may itself be an inherited replacement.
                // Compare against the published-order bearer so a reverse
                // reconstructs the retained parent's child order even when
                // every record in the local tree was recreated.
                let Some(parent_bearer) = state.lineage.holder_of(parent_lineage) else {
                    return;
                };
                candidate_parents.push((parent_lineage, parent_bearer));
            };
            for &record in inserted.iter() {
                note(ast.parent_of(record as usize), &|parent| {
                    resolve_lineage(parent)
                });
            }
            for &record in removed.iter() {
                note(ast.parent_of(record as usize), &|parent| {
                    resolve_lineage(parent)
                });
            }
            for &(_lineage, record) in updated_records.iter() {
                note(ast.parent_of(record as usize), &|parent| {
                    resolve_lineage(parent)
                });
            }
            // A newly reached record can itself own a generated child field.
            // Seed its current order directly; relying only on a touched
            // child's arena parent misses a reappearing retained record whose
            // arena parent still describes the previous parse.
            for &record in inserted.iter() {
                let Some(lineage) = resolve_lineage(record as usize) else {
                    continue;
                };
                if state.lineage.holder_of(lineage) == Some(record) {
                    candidate_parents.push((lineage, record));
                }
            }
        }
        if std::env::var_os("PLINGO_TRACE_PARSER").is_some() {
            eprintln!(
                "candidate parents inserted={inserted:?} removed={removed:?} pairs={candidate_parents:?}"
            );
        }
        candidate_parents.sort_unstable();
        candidate_parents.dedup();
        let spliced_already: std::collections::HashSet<u64> =
            computed_child_splices.iter().map(|s| s.parent.0).collect();
        if std::env::var_os("PLINGO_TRACE_PARSER").is_some() {
            eprintln!(
                "candidate-sorted={candidate_parents:?} previous-orders={:?}",
                previous.child_orders
            );
            for &(lineage, record) in &candidate_parents {
                let children = tree_child_records(record as usize);
                eprintln!(
                    "candidate-detail lineage={lineage} record={record} children={children:?} child-lineages={:?}",
                    children
                        .iter()
                        .filter_map(|child| resolve_lineage(*child as usize))
                        .collect::<Vec<_>>()
                );
            }
        }
        for (parent_lin, parent_record) in candidate_parents {
            let new_list = tree_child_records(parent_record as usize);
            let new_pairs: Vec<(SyntaxNodeId, u64)> = new_list
                .iter()
                .filter_map(|&child| {
                    resolve_lineage(child as usize).map(|lin| (SyntaxNodeId(lin), child as u64))
                })
                .collect();
            let new_children: Vec<SyntaxNodeId> =
                new_pairs.iter().map(|(lineage, _)| *lineage).collect();
            next_child_orders.insert(
                parent_lin,
                new_children.iter().map(|lineage| lineage.0).collect(),
            );
            if spliced_already.contains(&parent_lin) {
                continue;
            }
            if let Some(old_order) = previous.child_orders.get(parent_lin) {
                let old_children: Vec<SyntaxNodeId> =
                    old_order.iter().map(|&lin| SyntaxNodeId(lin)).collect();
                if old_children != new_children {
                    let (prefix, suffix) = common_edges(&old_children, &new_children);
                    let current_lineages: std::collections::HashSet<u64> =
                        new_children.iter().map(|id| id.0).collect();
                    let removed_children: Vec<(SyntaxNodeId, u64)> = old_order
                        [prefix..old_order.len() - suffix]
                        .iter()
                        .filter(|&lin| !current_lineages.contains(lin))
                        .filter_map(|&lin| {
                            dropped_record(lin, &state.lineage, &reverse_died)
                                .map(|rec| (SyntaxNodeId(lin), rec))
                        })
                        .collect();
                    let inserted_children: Vec<(SyntaxNodeId, u64)> = new_pairs
                        [prefix..new_pairs.len() - suffix]
                        .iter()
                        .copied()
                        .collect();
                    computed_child_splices.push(ChildSplice {
                        parent: SyntaxNodeId(parent_lin),
                        delta: ordered_splice(&old_children, &new_children),
                        removed_children: removed_children.into(),
                        inserted_children: inserted_children.into(),
                    });
                }
            }
        }

        // The first command has no prior splice oracle. Building its complete
        // order map is an initialization pass; every later command updates
        // only touched parents and bounded splice middles.
        if previous.child_orders.is_empty() {
            for (record, ()) in current.iter() {
                let Some(lineage) = resolve_lineage(record as usize) else {
                    continue;
                };
                let order = tree_child_records(record as usize)
                    .iter()
                    .filter_map(|&child| resolve_lineage(child as usize))
                    .collect();
                next_child_orders.insert(lineage, order);
            }
        }

        for removed in &removed_pairs {
            next_child_orders.remove(removed.lineage.0);
        }
    }

    sort_dedup(&mut inserted);
    sort_dedup(&mut removed);

    // Root transitions are a small ordered domain of syntax lineages. A
    // retained root lineage keeps the document-stable public node and does
    // not produce a root splice.
    let previous_root_lineage = previous
        .root
        .and_then(|record| previous.record_lineages.get(record).copied())
        .map(SyntaxNodeId);
    let current_root_lineage = tree_root
        .and_then(|record| {
            state
                .lineage
                .lineage_of(record as usize)
                .or_else(|| state.lineage.died_lineage_of(record))
        })
        .map(SyntaxNodeId);
    let roots = if previous_root_lineage == current_root_lineage {
        OrderedDelta::default()
    } else {
        OrderedDelta {
            before: None,
            removed: previous_root_lineage.into_iter().collect(),
            inserted: current_root_lineage.into_iter().collect(),
            after: None,
        }
    };

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
        child_splices: Arc::from(computed_child_splices),
        child_orders_next: Arc::new(next_child_orders),
        roots,
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
    Ok((Arc::new(delta), reachability))
}
/// `new` (plan §9): trims the common prefix/suffix and reports the bounded
/// middle plus its retained anchors.
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
    }
}
/// Returns the shared prefix and suffix lengths of two child orders.
fn common_edges(old: &[SyntaxNodeId], new: &[SyntaxNodeId]) -> (usize, usize) {
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
    (prefix, suffix)
}

/// Resolves a dropped child lineage to a usable arena record: a live record
/// bearing the lineage (the child moved), else the death journal.
fn dropped_record(
    lineage: u64,
    lineage_state: &crate::framework::parse::parsing::lineage::LineageState,
    reverse_died: &std::collections::HashMap<u64, u64>,
) -> Option<u64> {
    // Preferred: the lineage's CURRENT live bearer (a dropped child that
    // merely moved elsewhere is still alive).
    if let Some(bearer) = lineage_state.holder_of(lineage) {
        return Some(bearer);
    }
    reverse_died.get(&lineage).copied()
}

fn status_of(infos: &[ParseErrorInfo], no_acceptance: bool) -> ParsedStatus {
    if infos.is_empty() {
        ParsedStatus::Clean
    } else if no_acceptance {
        ParsedStatus::Unrecovered {
            regions: infos.len(),
        }
    } else {
        ParsedStatus::Recovered {
            segments: infos.len(),
        }
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
        &self,
        previous_root: Option<Arc<ParserDocumentRoot>>,
        uri: fluent_uri::Uri<String>,
        plan: ReplayPlan,
    ) -> Result<Arc<ParserDocumentRoot>, ParseError> {
        let total_start = Instant::now();
        if std::env::var_os("PLINGO_TRACE_PARSER").is_some() {
            eprintln!(
                "parse batch uri={} old_len={:?} new_len={} terminals={:?}",
                uri,
                plan.old.as_ref().map(|old| old.semantic_len()),
                plan.new.semantic_len(),
                (0..plan.new.semantic_len())
                    .filter_map(|rank| plan.new.token_at(rank).map(|token| token.terminal))
                    .collect::<Vec<_>>()
            );
        }
        let plan_elapsed = Duration::default();

        let session_setup_start = Instant::now();
        let previous_tree_facts = previous_root.as_ref().map_or_else(
            || Arc::new(ParserTreeFacts::default()),
            |root| Arc::clone(&root.tree_facts),
        );
        let previous_roots = previous_root
            .as_ref()
            .map_or_else(|| Arc::new(Vec::new()), |root| Arc::clone(&root.roots));
        let mut root = previous_root
            .as_deref()
            .cloned()
            .unwrap_or_else(|| ParserDocumentRoot::with_document(&uri, plan.new.document));
        let state = Arc::make_mut(&mut root.session);
        let arenas = Arc::make_mut(&mut root.arenas);
        if state.columns.is_empty() && state.retained_suffix.is_none() {
            let start = arenas.gss.node(0, 0, 0);
            let mut initial = ParseColumn::new(None, IndexSet::from([start]));
            let boundary = plan.new.boundary_at_rank(0);
            initial.set_boundary(Some(ParserBoundaryId::Source(boundary)));
            state.columns = vec![initial];
            state
                .boundary_columns
                .insert(ParserBoundaryId::Source(boundary), 0);
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
            active_scratch: Vec::new(),
        };
        session_ctx.state.lineage.begin_command();
        // Deterministic synthetic-token identity (plan §14): the document
        // serial is the stable URI hash; this command's synthetic tokens
        // restart empty.
        session_ctx.state.document_serial = plan.new.document.0;
        session_ctx.state.synthetic_tokens.clear();
        session_ctx.state.synthetic_log.clear();
        session_ctx.state.active_recovery_segment = None;
        let gss_before = session_ctx.gss.node_count();
        let gss_edges_before = session_ctx.gss.edge_count();
        let products_before = session_ctx.products.len();
        let ast_before = session_ctx.ast.len();
        let session_setup_elapsed = session_setup_start.elapsed();
        let restart_token_boundary = plan.restart_boundary.min(plan.old_extent);
        let restart_column = plan
            .old
            .as_ref()
            .and_then(|old| {
                (0..=restart_token_boundary).rev().find_map(|rank| {
                    let boundary = ParserBoundaryId::Source(old.boundary_at_rank(rank));
                    session_ctx.state.column_for_boundary(boundary)
                })
            })
            .unwrap_or(0);
        let restart_boundary = (0..=restart_column)
            .rev()
            .find(|&column| {
                session_ctx
                    .state
                    .column_at(column)
                    .is_some_and(|column| !column.error_derived)
            })
            .unwrap_or(0);
        // A committed parse may be represented entirely by an immutable
        // segment. Materialize only the checkpoint prefix needed by this
        // replay before detaching the old suffix.
        session_ctx.state.ensure_prefix(restart_boundary);
        let old_column_base = if plan.suffix_len > 0 {
            let reuse_boundary =
                ParserBoundaryId::Source(plan.new.boundary_at_rank(plan.new_reuse_start));
            session_ctx
                .state
                .column_for_boundary(reuse_boundary)
                .unwrap_or_else(|| session_ctx.state.column_count())
        } else {
            session_ctx.state.column_count()
        };
        let checkpoint_start = Instant::now();
        let detached = session_ctx.state.detach_suffix(
            old_column_base,
            session_ctx.gss,
            session_ctx.products,
            plan.new.document,
        );
        let empty_segment = || {
            super::ParseSegment::from_columns(
                Vec::new(),
                session_ctx.gss,
                session_ctx.products,
                plan.new.document,
            )
        };
        let detached = detached.unwrap_or_else(empty_segment);
        // If the restart checkpoint lies inside the detached range, keep only
        // that bounded overlap mutable for replay. The retained segment stays
        // shared and remains the old-side convergence oracle.
        if old_column_base <= restart_boundary {
            let keep = restart_boundary
                .saturating_sub(old_column_base)
                .saturating_add(1)
                .min(detached.len());
            let overlap = detached.slice(0..keep).materialize();
            session_ctx.state.append_reused_columns(overlap);
        }
        let mut old_suffix = ReusableSuffix {
            segment: detached,
            column_base: old_column_base,
        };
        let checkpoint_elapsed = checkpoint_start.elapsed();
        let old_suffix_len = old_suffix.len();

        let truncate_start = Instant::now();
        session_ctx.state.truncate_to_column(restart_boundary);
        let truncate_elapsed = truncate_start.elapsed();

        let token_materialization_elapsed = Duration::default();

        let eof = self.grammar.eof;
        let mut stats = IncrementalParseStats::default();
        let replay_start = Instant::now();
        let mut reduce_elapsed = Duration::default();
        let mut shift_elapsed = Duration::default();
        let mut reuse_timing = ReuseTiming::default();
        let mut recover_elapsed = Duration::default();
        let mut converge_elapsed = Duration::default();
        let mut frontier_cache = CanonicalFrontierCache::default();
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
            let boundary = ParserBoundaryId::Source(plan.new.boundary_at_rank(rank));
            session_ctx
                .state
                .set_column_boundary(column, Some(boundary));
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
                &mut frontier_cache,
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
                let recovery_mark = session_ctx.state.mark();
                let mut tail = TokenTail::new(&plan.new, rank, session_ctx.grammar);
                let recovery = match try_reuse_recovery_journal(
                    &plan,
                    &mut session_ctx,
                    token.column,
                    &mut tail,
                    &mut stats,
                    &mut frontier_cache,
                )? {
                    Some(consumed) => Ok(Some(consumed)),
                    None => session_ctx.recover_tokens(0, &mut tail, token.column),
                };
                recovery_decoded = recovery_decoded.saturating_add(tail.decoded());
                let recovery = match recovery {
                    Ok(value) => value,
                    Err(ParseError::NoActiveStacks { .. }) => {
                        session_ctx.state.rollback_to(recovery_mark);
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
                let recovery_mark = session_ctx.state.mark();
                let mut tail = TokenTail::new(&plan.new, rank, session_ctx.grammar);
                let recovery = match try_reuse_recovery_journal(
                    &plan,
                    &mut session_ctx,
                    token.column,
                    &mut tail,
                    &mut stats,
                    &mut frontier_cache,
                )? {
                    Some(consumed) => Ok(Some(consumed)),
                    None => session_ctx.recover_tokens(0, &mut tail, token.column),
                };
                recovery_decoded = recovery_decoded.saturating_add(tail.decoded());
                let recovery = match recovery {
                    Ok(value) => value,
                    Err(ParseError::NoActiveStacks { .. }) => {
                        session_ctx.state.rollback_to(recovery_mark);
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
        let recovery_columns = session_ctx.state.recovery_columns_after(restart_boundary);
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
        let full_replay_reason = (!stats.reconverged_old_boundary.is_some()).then(|| {
            if plan.old.is_none() {
                crate::framework::parse::FullReplayReason::ExplicitColdParse
            } else if recovery_columns > 0 {
                crate::framework::parse::FullReplayReason::RecoveryProofFailed
            } else if plan.suffix_len == 0 {
                crate::framework::parse::FullReplayReason::NoRetainedRightBoundary
            } else {
                crate::framework::parse::FullReplayReason::NoEqualFrontierBeforeEof
            }
        });
        let gss_nodes_created = session_ctx.gss.node_count().saturating_sub(gss_before);
        let gss_edges_created = session_ctx
            .gss
            .edge_count()
            .saturating_sub(gss_edges_before);
        let products_created = session_ctx.products.len().saturating_sub(products_before);
        let ast_records_created = session_ctx.ast.len().saturating_sub(ast_before);
        crate::framework::workspace::record_parser_work(&uri.to_string(), |work| {
            work.restart_columns += restart_boundary as u64;
            work.restart_boundary_lookups += 1;
            work.restart_lookup_depth += restart_token_boundary as u64;
            work.restart_occurrences += u64::from(plan.old.is_some());
            work.tokens_decoded += (decoded + recovery_decoded) as u64;
            work.tokens_replayed += (decoded + recovery_decoded) as u64;
            work.semantic_tokens_decoded += decoded as u64;
            work.source_boundaries_replayed += reparsed as u64;
            work.recovery_boundaries_replayed += recovery_decoded as u64;
            work.columns_replayed += reparsed as u64;
            work.columns_reused += reused as u64;
            work.segments_split += u64::from(old_suffix_len > 0);
            work.segments_attached += u64::from(stats.reconverged_old_boundary.is_some());
            work.suffix_columns_physically_visited += stats.suffix_rewritten as u64;
            work.gss_nodes_created += gss_nodes_created as u64;
            work.gss_records_created += gss_nodes_created as u64;
            work.gss_edges_created += gss_edges_created as u64;
            work.products_created += products_created as u64;
            work.product_records_created += products_created as u64;
            work.ast_records_created += ast_records_created as u64;
            if let Some(reason) = full_replay_reason {
                work.record_full_replay(reason);
            }
        });
        let mut tree_root = None;
        for (index, root) in roots_after.iter().copied().enumerate() {
            let Some(product) = session_ctx.products.get(root) else {
                continue;
            };
            if index == 0 {
                tree_root = match &product.data {
                    ProductData::Node { ast, .. } | ProductData::Token { ast: Some(ast), .. } => {
                        Some(*ast as u64)
                    }
                    ProductData::Error { .. } | ProductData::Token { ast: None, .. } => {
                        product.ast_ids.first().map(|id| *id as u64)
                    }
                };
            }
        }
        let previous_infos = previous_root
            .as_ref()
            .map_or_else(Vec::new, |root| root.published_diagnostics.as_ref().clone());
        let previous_status = previous_root
            .as_ref()
            .and_then(|root| root.published_status);
        let current_infos = collect_parse_diagnostics(
            session_ctx.state,
            Some(crate::framework::parse::diagnostics::DiagnosticArenas {
                trees: session_ctx.trees,
                products: session_ctx.products,
            }),
            &roots_after,
        );
        let current_status = status_of(&current_infos, roots_after.is_empty());
        let (tree_delta, reachability) = freeze_parse_delta(
            &mut *session_ctx.state,
            session_ctx.products,
            session_ctx.ast,
            &previous_tree_facts,
            previous_roots.as_slice(),
            &roots_after,
            tree_root,
            &previous_infos,
            current_infos.clone(),
            previous_status,
            current_status.clone(),
            self.tree_member_kind,
            self.tree_child_records,
        )?;
        let semantic_revision_changed = !tree_delta.is_empty();
        crate::framework::workspace::record_parser_work(&uri.to_string(), |work| {
            let payload_ops = tree_delta.syntax_payloads.len() as u64;
            let parent_ops = tree_delta.parents.len() as u64;
            let field_ops = tree_delta.child_splices.len() as u64;
            let order_ops = tree_delta.child_splices.len() as u64;
            let journal_entries = tree_delta.ast_records.len() as u64
                + payload_ops
                + parent_ops
                + field_ops
                + tree_delta.roots.removed.len() as u64
                + tree_delta.roots.inserted.len() as u64
                + tree_delta.diagnostics.len() as u64;
            work.parser_records_inserted += tree_delta.ast_records.inserted.len() as u64;
            work.parser_records_removed += tree_delta.ast_records.removed.len() as u64;
            work.parser_records_updated += tree_delta.ast_records.updated.len() as u64;
            work.syntax_journal_entries += journal_entries;
            work.syntax_payload_ops += payload_ops;
            work.syntax_parent_ops += parent_ops;
            work.syntax_field_ops += field_ops;
            work.syntax_order_splices += order_ops;
            work.record_journal_touches += journal_entries;
        });
        let mut current_record_lineages = (*previous_tree_facts.record_lineages).clone();
        for &record in &*tree_delta.ast_records.removed {
            current_record_lineages.remove(record);
        }
        for &record in &*tree_delta.ast_records.inserted {
            if let Some(lineage) = session_ctx.state.lineage.lineage_of(record as usize) {
                current_record_lineages.insert(record, lineage);
            }
        }
        let current_tree_facts = Arc::new(ParserTreeFacts {
            records: Arc::clone(tree_delta.live_records()),
            root: tree_root,
            product_reach_counts: Arc::new(reachability.product_reach_counts),
            record_reach_counts: Arc::new(reachability.record_reach_counts),
            record_lineages: Arc::new(current_record_lineages),
            published_child_orders: Arc::clone(&previous_tree_facts.published_child_orders),
            child_orders: Arc::clone(&tree_delta.child_orders_next),
        });
        session_ctx
            .state
            .seal(session_ctx.gss, session_ctx.products, plan.new.document);
        drop(session_ctx);
        arenas.seal_generations();
        root.incremental_stats = stats;
        root.published_status = Some(current_status);
        root.published_diagnostics = Arc::new(current_infos);
        root.tree_facts = current_tree_facts;
        root.tree_delta = tree_delta;
        root.roots = Arc::new(roots_after);
        root.token = Some(Arc::clone(&plan.new));
        if semantic_revision_changed {
            root.semantic_revision = root.semantic_revision.saturating_add(1);
        }
        let stats_elapsed = stats_start.elapsed();

        let total_elapsed = total_start.elapsed();
        log::debug!(
            "[parse-replay] uri={} total={:?} plan={:?} session={:?} checkpoints={:?} truncate={:?} tokens={:?} replay={:?} reduce={:?} shift={:?} recover={:?} converge={:?} reuse_checkpoint={:?} frontier_match={:?} tail_validate={:?} seam_binding={:?} replay_misc={:?} stats={:?} status={} restart={} reparsed={} reused={} recovery_columns={} checks={} checkpoint_matches={} frontier_matches={} old_suffix={} replay_tokens={} suffix_rebased={} old_tokens={} new_tokens={} prefix={} suffix={}",
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
            reuse_timing.seam_binding,
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

        Ok(Arc::new(root))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::framework::parse::data::{
        ast::{AnchoredSpan, AstArena},
        product::{Product, ProductArena},
    };

    fn arenas() -> (AstArena, ProductArena) {
        let uri: fluent_uri::Uri<String> = "test://accepted-root".parse().unwrap();
        (AstArena::new(uri), ProductArena::new())
    }

    fn node(
        ast: &mut AstArena,
        products: &mut ProductArena,
        value: usize,
        children: Vec<ProductId>,
    ) -> ProductId {
        let record = ast.insert(value, AnchoredSpan::point(0));
        products.insert(Product::node(0, record, children))
    }

    fn product_counts(update: &ReachabilityUpdate) -> BTreeMap<ProductId, u32> {
        update
            .product_reach_counts
            .iter()
            .map(|(key, count)| (key.0, *count))
            .collect()
    }

    fn record_counts(update: &ReachabilityUpdate) -> BTreeMap<u64, u32> {
        update
            .record_reach_counts
            .iter()
            .map(|(key, count)| (key.0, *count))
            .collect()
    }

    fn facts(update: &ReachabilityUpdate) -> ParserTreeFacts {
        ParserTreeFacts {
            records: Arc::new(update.live_records.clone()),
            product_reach_counts: Arc::new(update.product_reach_counts.clone()),
            record_reach_counts: Arc::new(update.record_reach_counts.clone()),
            ..ParserTreeFacts::default()
        }
    }

    #[test]
    fn accepted_root_reachability_preserves_shared_and_duplicate_edges() {
        let (mut ast, mut products) = arenas();
        let child = node(&mut ast, &mut products, 0, Vec::new());
        let parent = node(&mut ast, &mut products, 1, vec![child, child]);

        let update =
            apply_accepted_root_delta(&ParserTreeFacts::default(), &[], &[parent], &products)
                .unwrap();
        assert_eq!(
            product_counts(&update),
            BTreeMap::from([(child, 2), (parent, 1)])
        );
        assert_eq!(record_counts(&update), BTreeMap::from([(0, 1), (1, 1)]));
        assert_eq!(
            slow_accepted_root_reach(&[parent], &products).unwrap(),
            (product_counts(&update), record_counts(&update))
        );

        let duplicate_roots = apply_accepted_root_delta(
            &ParserTreeFacts::default(),
            &[],
            &[parent, parent],
            &products,
        )
        .unwrap();
        assert_eq!(
            product_counts(&duplicate_roots),
            BTreeMap::from([(child, 2), (parent, 2)])
        );
        assert_eq!(
            record_counts(&duplicate_roots),
            BTreeMap::from([(0, 1), (1, 1)])
        );
    }

    #[test]
    fn accepted_root_reachability_counts_shared_parent_edges_once_per_parent() {
        let (mut ast, mut products) = arenas();
        let child = node(&mut ast, &mut products, 0, Vec::new());
        let left = node(&mut ast, &mut products, 1, vec![child]);
        let right = node(&mut ast, &mut products, 2, vec![child]);

        let update =
            apply_accepted_root_delta(&ParserTreeFacts::default(), &[], &[left, right], &products)
                .unwrap();
        assert_eq!(
            product_counts(&update),
            BTreeMap::from([(child, 2), (left, 1), (right, 1)])
        );
        assert_eq!(
            record_counts(&update),
            BTreeMap::from([(0, 1), (1, 1), (2, 1)])
        );

        let reverted =
            apply_accepted_root_delta(&facts(&update), &[left, right], &[], &products).unwrap();
        assert!(product_counts(&reverted).is_empty());
        assert!(record_counts(&reverted).is_empty());
        assert!(reverted.live_records.is_empty());
        assert_eq!(
            reverted.record_journal.get(&0),
            Some(&RecordTransition {
                before_count: 1,
                after_count: 0,
            })
        );
    }

    #[test]
    fn accepted_root_reachability_excludes_unreachable_cache_products() {
        let (mut ast, mut products) = arenas();
        let orphan = node(&mut ast, &mut products, 0, Vec::new());
        let root = node(&mut ast, &mut products, 1, Vec::new());

        let update =
            apply_accepted_root_delta(&ParserTreeFacts::default(), &[], &[root], &products)
                .unwrap();
        assert_eq!(product_counts(&update), BTreeMap::from([(root, 1)]));
        assert_eq!(record_counts(&update), BTreeMap::from([(1, 1)]));
        assert!(!product_counts(&update).contains_key(&orphan));
        assert_eq!(
            update
                .live_records
                .iter()
                .map(|(key, _)| key)
                .collect::<Vec<_>>(),
            vec![1]
        );
    }

    #[test]
    fn accepted_root_reachability_rejects_underflow_and_non_topological_edges() {
        let (_ast, mut products) = arenas();
        products.insert(Product::error(0));
        let underflow =
            match apply_accepted_root_delta(&ParserTreeFacts::default(), &[0], &[], &products) {
                Ok(_) => panic!("root removal at zero must fail"),
                Err(error) => error,
            };
        assert!(matches!(
            underflow,
            ParseError::InvalidReachability {
                kind: "product",
                key: 0,
                before: 0,
                delta: -1
            }
        ));

        let (_ast, mut products) = arenas();
        products.insert(Product::error_with_children(0, vec![0]));
        let non_topological =
            match apply_accepted_root_delta(&ParserTreeFacts::default(), &[], &[0], &products) {
                Ok(_) => panic!("self-referential product edge must fail"),
                Err(error) => error,
            };
        assert!(matches!(
            non_topological,
            ParseError::InvalidReachability {
                kind: "non-topological-edge",
                key: 0,
                ..
            }
        ));
    }
}
