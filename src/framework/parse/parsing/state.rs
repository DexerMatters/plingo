use super::{ParseColumn, ParserSessionState, checkpoint::FrontierCheckpoint};
use crate::framework::lex::TokenOccurrenceId;
use crate::framework::parse::{
    data::{gss::GssNodeId, product::ProductId},
    types::ParserBoundaryId,
};

impl ParseColumn {
    pub(crate) fn active_nodes(&self) -> impl Iterator<Item = GssNodeId> + '_ {
        self.active.iter().copied()
    }

    pub(crate) fn base_active_nodes(&self) -> impl Iterator<Item = GssNodeId> + '_ {
        self.base_active.iter().copied()
    }

    pub fn accepted(&self) -> &[ProductId] {
        &self.accepted
    }
    pub(crate) fn set_boundary(&mut self, boundary: Option<ParserBoundaryId>) {
        if self.boundary != boundary {
            self.boundary = boundary;
            self.invalidate_checkpoint_cache();
        }
    }

    pub(crate) fn boundary(&self) -> Option<ParserBoundaryId> {
        self.boundary
    }

    pub(crate) fn push_product(&mut self, product: ProductId) -> bool {
        if self.products.contains(&product) {
            return false;
        }
        self.products.push(product);
        self.invalidate_checkpoint_cache();
        true
    }

    pub(crate) fn push_accepted(&mut self, product: ProductId) -> bool {
        if self.accepted.contains(&product) {
            return false;
        }
        self.accepted.push(product);
        self.invalidate_checkpoint_cache();
        true
    }

    pub(crate) fn insert_active(&mut self, node: GssNodeId) -> bool {
        let inserted = self.active.insert(node);
        if inserted {
            self.invalidate_checkpoint_cache();
        }
        inserted
    }

    pub(crate) fn set_error_derived(&mut self) -> bool {
        if self.error_derived {
            return false;
        }
        self.error_derived = true;
        self.invalidate_checkpoint_cache();
        true
    }

    pub(crate) fn cached_frontier_checkpoint(&self) -> Option<&FrontierCheckpoint> {
        self.checkpoint_cache.frontier()
    }

    pub(crate) fn cache_frontier_checkpoint(&mut self, checkpoint: FrontierCheckpoint) {
        self.checkpoint_cache.store(checkpoint);
    }

    fn invalidate_checkpoint_cache(&mut self) {
        self.checkpoint_cache.invalidate();
    }

    fn reset_for_replay(&mut self) {
        self.active = self.base_active.clone();
        self.accepted.clear();
        self.products.clear();
        self.diagnostics.clear();
        self.error_derived = false;
        self.invalidate_checkpoint_cache();
    }
}

impl ParserSessionState {
    pub fn accepted(&self) -> &[ProductId] {
        if let Some(segment) = &self.retained_suffix {
            segment.accepted()
        } else {
            self.columns.last().map_or(&[], ParseColumn::accepted)
        }
    }

    pub fn current_column(&self) -> usize {
        self.column_count().saturating_sub(1)
    }

    pub(crate) fn column_count(&self) -> usize {
        self.columns.len()
            + self
                .retained_suffix
                .as_ref()
                .map_or(0, |segment| segment.len())
    }

    pub(crate) fn set_column_boundary(&mut self, index: usize, boundary: Option<ParserBoundaryId>) {
        let Some(column) = self.columns.get_mut(index) else {
            return;
        };
        if let Some(previous) = column.boundary() {
            if self.boundary_columns.get(&previous).copied() == Some(index) {
                self.boundary_columns.remove(&previous);
            }
        }
        column.set_boundary(boundary);
        if let Some(boundary) = boundary {
            self.boundary_columns.insert(boundary, index);
        }
    }

    pub(crate) fn column_for_boundary(&self, boundary: ParserBoundaryId) -> Option<usize> {
        self.boundary_columns.get(&boundary).copied().or_else(|| {
            self.retained_suffix
                .as_ref()
                .and_then(|segment| segment.boundary_column(boundary))
                .map(|column| self.columns.len() + column)
        })
    }

    pub(crate) fn token_product(&self, token: TokenOccurrenceId) -> Option<ProductId> {
        self.token_products.get(&token).copied().or_else(|| {
            self.retained_suffix
                .as_ref()
                .and_then(|segment| segment.token_product(token))
        })
    }

    pub(crate) fn column_at(&self, index: usize) -> Option<&ParseColumn> {
        self.columns.get(index).or_else(|| {
            self.retained_suffix
                .as_ref()
                .and_then(|segment| segment.column(index.saturating_sub(self.columns.len())))
        })
    }

    /// Materializes only the immutable prefix needed to restart replay.
    /// Retained suffix pieces remain shared and are reattached after the
    /// mutable prefix is prepared.
    pub(crate) fn ensure_prefix(&mut self, end: usize) {
        let needed = end.saturating_add(1);
        if needed <= self.columns.len() {
            return;
        }
        let Some(existing) = self.retained_suffix.take() else {
            return;
        };
        let take = needed
            .saturating_sub(self.columns.len())
            .min(existing.len());
        let prefix = existing.slice(0..take).materialize();
        let rest = existing.slice(take..existing.len());
        self.append_materialized_prefix(prefix);
        if !rest.is_empty() {
            self.retained_suffix = Some(rest);
        }
    }

    /// Freezes the mutable prefix into one immutable segment. Segment
    /// construction is charged once to the materialized prefix; an already
    /// retained suffix is concatenated by metadata only.
    pub(crate) fn seal(
        &mut self,
        gss: &crate::framework::parse::data::gss::GssArena,
        products: &crate::framework::parse::data::product::ProductArena,
        document: crate::framework::lex::StableDocumentId,
    ) {
        if self.columns.is_empty() {
            return;
        }
        let columns = std::mem::take(&mut self.columns);
        let prefix = super::ParseSegment::from_columns(columns, gss, products, document);
        self.retained_suffix = Some(match self.retained_suffix.take() {
            Some(suffix) => super::ParseSegment::concat(prefix, suffix),
            None => prefix,
        });
        self.boundary_columns.clear();
        self.token_columns.clear();
        self.token_products.clear();
    }

    /// Detaches columns at `start` into a persistent suffix. Existing
    pub(crate) fn detach_suffix(
        &mut self,
        start: usize,
        gss: &crate::framework::parse::data::gss::GssArena,
        products: &crate::framework::parse::data::product::ProductArena,
        document: crate::framework::lex::StableDocumentId,
    ) -> Option<std::sync::Arc<super::ParseSegment>> {
        let total = self.column_count();
        let start = start.min(total);
        let existing = self.retained_suffix.take();
        let detached = if start < self.columns.len() {
            let tail = self.columns.split_off(start);
            let prefix_segment = super::ParseSegment::from_columns(tail, gss, products, document);
            existing.map_or(prefix_segment.clone(), |segment| {
                super::ParseSegment::concat(prefix_segment.clone(), segment)
            })
        } else if let Some(segment) = existing {
            segment.slice(start - self.columns.len()..segment.len())
        } else {
            return None;
        };
        self.columns.truncate(start.min(self.columns.len()));
        self.boundary_columns
            .retain(|_, column| *column < self.columns.len());
        self.token_columns.retain(|_, column| *column < start);
        self.token_products.retain(|token, _| {
            self.token_columns.contains_key(token)
                || detached
                    .token_column(*token)
                    .is_some_and(|column| self.columns.len() + column < start)
        });
        Some(detached)
    }
    pub fn truncate_to_column(&mut self, column: usize) {
        assert!(column < self.columns.len(), "parse column out of range");
        self.retained_suffix = None;
        self.generation += 1;
        self.columns.truncate(column.saturating_add(1));
        self.columns[column].reset_for_replay();
        self.diagnostics
            .retain(|info| info.location.is_some_and(|loc| loc < column));
        self.diagnostic_index = self.diagnostics.iter().cloned().collect();

        self.boundary_columns.retain(|_, c| *c <= column);
        self.token_columns.retain(|_, c| *c <= column);
        self.token_products
            .retain(|token, _| self.token_columns.contains_key(token));
    }

    fn append_columns(&mut self, columns: impl IntoIterator<Item = ParseColumn>) {
        debug_assert!(self.retained_suffix.is_none());
        for column in columns {
            let index = self.columns.len();
            if let Some(boundary) = column.boundary() {
                self.boundary_columns.insert(boundary, index);
            }
            if let Some(token) = column.token {
                self.token_columns.insert(token, index);
                if !column.error_derived
                    && let Some(&product) = column.products.first()
                {
                    self.token_products.insert(token, product);
                }
            }
            for diagnostic in &column.diagnostics {
                if self.diagnostic_index.insert(diagnostic.clone()) {
                    self.diagnostics.push(diagnostic.clone());
                }
            }
            self.columns.push(column);
        }
    }

    pub(crate) fn append_reused_columns(&mut self, columns: impl IntoIterator<Item = ParseColumn>) {
        self.append_columns(columns);
    }

    fn append_materialized_prefix(&mut self, columns: impl IntoIterator<Item = ParseColumn>) {
        self.append_columns(columns);
    }

    pub(crate) fn append_reused_segment(&mut self, segment: std::sync::Arc<super::ParseSegment>) {
        debug_assert!(self.retained_suffix.is_none());
        if segment.is_empty() {
            return;
        }
        self.retained_suffix = Some(segment);
    }

    pub(crate) fn recovery_columns_after(&self, start: usize) -> usize {
        let prefix_start = start.saturating_add(1).min(self.columns.len());
        let prefix = self.columns[prefix_start..]
            .iter()
            .filter(|column| column.error_derived)
            .count();
        let suffix_start = start.saturating_add(1).saturating_sub(self.columns.len());
        prefix
            + self
                .retained_suffix
                .as_ref()
                .map_or(0, |segment| segment.error_count_after(suffix_start))
    }
}
