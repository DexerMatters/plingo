use indexmap::IndexSet;

use super::{ParseColumn, ParserSessionState, checkpoint::FrontierCheckpoint};
use crate::framework::parse::{
    data::{gss::GssNodeId, product::ProductId},
    types::TokenOccurrenceId,
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
        self.columns.last().map_or(&[], ParseColumn::accepted)
    }

    pub fn current_column(&self) -> usize {
        self.columns.len().saturating_sub(1)
    }

    pub fn truncate_to_column(&mut self, column: usize) {
        assert!(column < self.columns.len(), "parse column out of range");
        self.columns.truncate(column + 1);
        self.generation += 1;
        self.columns[column].reset_for_replay();
        self.diagnostics
            .retain(|info| info.location.is_some_and(|loc| loc < column));

        self.token_columns.retain(|_, c| *c <= column);
        self.token_products
            .retain(|token, _| self.token_columns.contains_key(token));
    }

    pub(crate) fn append_reused_columns(&mut self, columns: impl IntoIterator<Item = ParseColumn>) {
        for column in columns {
            let index = self.columns.len();
            if let Some(token) = column.token {
                self.token_columns.insert(token, index);
                if !column.error_derived
                    && let Some(&product) = column.products.first()
                {
                    self.token_products.insert(token, product);
                }
            }
            for diagnostic in &column.diagnostics {
                if !self.diagnostics.contains(diagnostic) {
                    self.diagnostics.push(diagnostic.clone());
                }
            }
            self.columns.push(column);
        }
    }
}
