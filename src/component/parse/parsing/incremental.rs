use std::time::{Duration, Instant};

use indexmap::IndexSet;

use super::{ParseColumn, ParseError, ParseToken, SessionContext};
use crate::component::{
    lex::{LexerRoot, TokenChange},
    parse::{
        IncrementalParseStats, ParseChange, Parser, ParserSnapshotState, SessionArenas, checkpoint,
        data::{ast::AstArena, green::TreeArena, gss::GssArena, product::ProductArena},
        emit,
        incremental::ReplayPlan,
    },
};
use crate::scheme::{
    change::{LayerChange, LayerChanges},
    layer::NonTopLayer,
};

fn maybe_reuse_suffix(
    plan: &ReplayPlan,
    old_suffix_columns: &mut [ParseColumn],
    session_ctx: &mut SessionContext<'_>,
    current_boundary: usize,
    frontier_converged: &mut bool,
    semantic_reused: &mut bool,
    reconverged_new_boundary: &mut Option<usize>,
    reconverged_old_boundary: &mut Option<usize>,
) -> Result<bool, ParseError> {
    if current_boundary < plan.new_reuse_start {
        return Ok(false);
    }
    let Some(old_boundary) = plan.translated_old_boundary(current_boundary) else {
        return Ok(false);
    };
    let old_index = old_boundary.saturating_sub(plan.old_reuse_start);
    if old_suffix_columns.get(old_index).is_none() {
        return Ok(false);
    }
    if session_ctx.state.column(current_boundary).is_none() {
        return Ok(false);
    }

    let current_frontier = {
        let current_column = session_ctx
            .state
            .column_mut(current_boundary)
            .expect("current parse column must exist");
        checkpoint::frontier_checkpoint_for_column(current_column, session_ctx.gss).clone()
    };
    let old_frontier = {
        let old_column = old_suffix_columns
            .get_mut(old_index)
            .expect("old suffix column must exist");
        checkpoint::frontier_checkpoint_for_column(old_column, session_ctx.gss).clone()
    };
    if current_frontier != old_frontier {
        return Ok(false);
    }
    *frontier_converged = true;

    let current_checkpoint = {
        let current_column = session_ctx
            .state
            .column_mut(current_boundary)
            .expect("current parse column must exist");
        checkpoint::checkpoint_for_column(current_column, session_ctx.gss, session_ctx.products)
            .clone()
    };
    let old_checkpoint = {
        let old_column = old_suffix_columns
            .get_mut(old_index)
            .expect("old suffix column must exist");
        checkpoint::checkpoint_for_column(old_column, session_ctx.gss, session_ctx.products).clone()
    };
    if current_checkpoint == old_checkpoint {
        *semantic_reused = true;
        *reconverged_new_boundary = Some(current_boundary);
        *reconverged_old_boundary = Some(old_boundary);
        let reused_columns = old_suffix_columns
            .iter()
            .skip(old_index)
            .cloned()
            .collect::<Vec<_>>();
        session_ctx.state.discard_columns_from(current_boundary);
        session_ctx.state.append_reused_columns(reused_columns);
        return Ok(true);
    }
    Ok(false)
}

impl<Root: LexerRoot + Clone, Lower> Parser<Root, Lower> {
    pub(crate) async fn parse_delta_batch(
        &mut self,
        working: &mut ParserSnapshotState,
        change: TokenChange,
    ) -> Result<LayerChanges<Lower>, ParseError>
    where
        Lower: NonTopLayer<Change = ParseChange>,
    {
        let total_start = Instant::now();
        let uri = *change.address();
        let roots_before = working.roots.get(&uri).cloned().unwrap_or_default();
        let batch = change.batch;

        let plan_start = Instant::now();
        let plan = ReplayPlan::from_batch(batch.clone());
        let plan_elapsed = plan_start.elapsed();

        if !plan.batch.is_changed() {
            let sessions = working.sessions.get(&uri);
            let current_boundary = sessions.map(|state| state.current_column()).unwrap_or(0);
            let recovery_columns = sessions
                .map(|state| {
                    state
                        .columns
                        .iter()
                        .skip(1)
                        .filter(|column| column.error_derived)
                        .count()
                })
                .unwrap_or(0);
            self.latest_incremental_stats.insert(
                uri,
                IncrementalParseStats {
                    restart_boundary: plan.restart_boundary,
                    reconverged_new_boundary: batch.new_units.len().checked_sub(1),
                    reconverged_old_boundary: batch.old_units.len().checked_sub(1),
                    reparsed: 0,
                    reused: current_boundary,
                    recovery_columns,
                    frontier_converged: true,
                    semantic_reused: true,
                    converged: true,
                },
            );
            log::debug!(
                target: "Measure",
                "parse {} total={:?} plan={:?} changed=false restart={} reused={} recovery_columns={} old_tokens={} new_tokens={} prefix={} suffix={}",
                uri,
                total_start.elapsed(),
                plan_elapsed,
                plan.restart_boundary,
                current_boundary,
                recovery_columns,
                batch.old_units.len(),
                batch.new_units.len(),
                batch.prefix_len,
                batch.suffix_len,
            );
            return Ok(Vec::new());
        }

        let session_setup_start = Instant::now();
        let arenas = self
            .session_arenas
            .entry(uri.clone())
            .or_insert_with(|| SessionArenas {
                trees: TreeArena::new(),
                products: ProductArena::new(),
                ast: AstArena::new(uri.clone()),
                gss: GssArena::new(),
            });
        let state = working.sessions.entry(uri.clone()).or_default();
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
            error_recovery_timeout: self.config.error_recovery_timeout,
        };
        let session_setup_elapsed = session_setup_start.elapsed();

        let restart_boundary = plan
            .restart_boundary
            .min(session_ctx.state.current_column());
        let old_reuse_start = plan.old_reuse_start.min(session_ctx.state.columns.len());
        let checkpoint_start = Instant::now();
        let mut old_suffix_columns = session_ctx.state.columns_from(old_reuse_start);
        let checkpoint_elapsed = checkpoint_start.elapsed();
        let old_suffix_len = old_suffix_columns.len();

        let truncate_start = Instant::now();
        session_ctx.state.truncate_to_column(restart_boundary);
        let truncate_elapsed = truncate_start.elapsed();

        let token_materialization_start = Instant::now();
        let parse_tokens = plan
            .replay_tokens()
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
        let mut frontier_converged = false;
        let mut semantic_reused = false;
        let mut reconverged_new_boundary = None;
        let mut reconverged_old_boundary = None;
        let replay_start = Instant::now();
        let mut reduce_elapsed = Duration::default();
        let mut shift_elapsed = Duration::default();
        let mut recover_elapsed = Duration::default();
        let mut converge_elapsed = Duration::default();
        let mut i = 0usize;
        while i < parse_tokens.len() {
            let token = &parse_tokens[i];
            let column = session_ctx.state.current_column();
            let reduce_start = Instant::now();
            session_ctx.reduce_until_stable(column, token.terminal)?;
            reduce_elapsed += reduce_start.elapsed();
            if token.terminal == eof && !session_ctx.state.accepted().is_empty() {
                session_ctx.compact_accepted_roots();
                let current_boundary = session_ctx.state.current_column();
                let converge_start = Instant::now();
                if maybe_reuse_suffix(
                    &plan,
                    &mut old_suffix_columns,
                    &mut session_ctx,
                    current_boundary,
                    &mut frontier_converged,
                    &mut semantic_reused,
                    &mut reconverged_new_boundary,
                    &mut reconverged_old_boundary,
                )? {
                    converge_elapsed += converge_start.elapsed();
                    break;
                }
                converge_elapsed += converge_start.elapsed();
                break;
            }

            if token.terminal == session_ctx.grammar.error_terminal && session_ctx.error_recovery {
                let recover_start = Instant::now();
                if let Some(next) = session_ctx.recover_tokens(i, &parse_tokens)? {
                    recover_elapsed += recover_start.elapsed();
                    let current_boundary = session_ctx.state.current_column();
                    let converge_start = Instant::now();
                    if maybe_reuse_suffix(
                        &plan,
                        &mut old_suffix_columns,
                        &mut session_ctx,
                        current_boundary,
                        &mut frontier_converged,
                        &mut semantic_reused,
                        &mut reconverged_new_boundary,
                        &mut reconverged_old_boundary,
                    )? {
                        converge_elapsed += converge_start.elapsed();
                        break;
                    }
                    converge_elapsed += converge_start.elapsed();
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
                    let current_boundary = session_ctx.state.current_column();
                    let converge_start = Instant::now();
                    if maybe_reuse_suffix(
                        &plan,
                        &mut old_suffix_columns,
                        &mut session_ctx,
                        current_boundary,
                        &mut frontier_converged,
                        &mut semantic_reused,
                        &mut reconverged_new_boundary,
                        &mut reconverged_old_boundary,
                    )? {
                        converge_elapsed += converge_start.elapsed();
                        break;
                    }
                    converge_elapsed += converge_start.elapsed();
                    if next == i {
                        continue;
                    }
                    i = next;
                    continue;
                }
                recover_elapsed += recover_start.elapsed();
                return Err(ParseError::NoActiveStacks {
                    column: Some(token.column),
                });
            }
            shift_elapsed += shift_start.elapsed();

            if token.terminal == eof {
                let next_column = session_ctx.state.current_column();
                let reduce_start = Instant::now();
                session_ctx.reduce_until_stable(next_column, token.terminal)?;
                reduce_elapsed += reduce_start.elapsed();
            }

            let current_boundary = session_ctx.state.current_column();
            let converge_start = Instant::now();
            if maybe_reuse_suffix(
                &plan,
                &mut old_suffix_columns,
                &mut session_ctx,
                current_boundary,
                &mut frontier_converged,
                &mut semantic_reused,
                &mut reconverged_new_boundary,
                &mut reconverged_old_boundary,
            )? {
                converge_elapsed += converge_start.elapsed();
                break;
            }
            converge_elapsed += converge_start.elapsed();
            i += 1;
        }
        let replay_elapsed = replay_start.elapsed();
        let replay_misc_elapsed = replay_elapsed
            .saturating_sub(reduce_elapsed + shift_elapsed + recover_elapsed + converge_elapsed);

        let compact_start = Instant::now();
        session_ctx.compact_accepted_roots();
        let compact_elapsed = compact_start.elapsed();
        let roots_after = session_ctx.state.accepted().to_vec();
        working.roots.insert(uri.clone(), roots_after.clone());

        let reused = reconverged_old_boundary
            .map(|old_boundary| {
                old_suffix_len.saturating_sub(old_boundary.saturating_sub(old_reuse_start))
            })
            .unwrap_or(0);
        let reparsed = reconverged_new_boundary
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
        self.latest_incremental_stats.insert(
            uri.clone(),
            IncrementalParseStats {
                restart_boundary,
                reconverged_new_boundary,
                reconverged_old_boundary,
                reparsed,
                reused,
                recovery_columns,
                frontier_converged,
                semantic_reused,
                converged: frontier_converged,
            },
        );
        let stats_elapsed = stats_start.elapsed();
        drop(session_ctx);

        let diff_start = Instant::now();
        let lower_deltas = if roots_before.is_empty() {
            if roots_after.is_empty() {
                Vec::new()
            } else {
                vec![emit::insert_root(uri.clone(), roots_after.clone())]
            }
        } else if roots_after.is_empty() {
            vec![emit::delete_root(uri.clone(), roots_before.clone())]
        } else {
            super::super::diff::compact(super::super::diff::diff_trees(
                &arenas.products,
                &arenas.trees,
                &roots_before,
                &roots_after,
                uri.clone(),
            ))
        };
        let diff_elapsed = diff_start.elapsed();

        let total_elapsed = total_start.elapsed();
        log::debug!(
            target: "Measure",
            "parse {} total={:?} plan={:?} session={:?} checkpoints={:?} truncate={:?} tokens={:?} replay={:?} reduce={:?} shift={:?} recover={:?} converge={:?} replay_misc={:?} compact={:?} stats={:?} diff={:?} restart={} reparsed={} reused={} old_suffix={} replay_tokens={} frontier={} semantic={} old_tokens={} new_tokens={} prefix={} suffix={}",
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
            replay_misc_elapsed,
            compact_elapsed,
            stats_elapsed,
            diff_elapsed,
            restart_boundary,
            reparsed,
            reused,
            old_suffix_len,
            parse_tokens.len(),
            frontier_converged,
            semantic_reused,
            batch.old_units.len(),
            batch.new_units.len(),
            batch.prefix_len,
            batch.suffix_len,
        );

        Ok(lower_deltas)
    }
}
