use crate::component::{lex::TokenBatch, parse::TokenData};

#[derive(Debug, Clone)]
pub(crate) struct ReplayPlan {
    pub batch: TokenBatch,
    pub restart_boundary: usize,
    pub old_reuse_start: usize,
    pub new_reuse_start: usize,
}

impl ReplayPlan {
    pub(crate) fn from_batch(batch: TokenBatch) -> Self {
        let restart_boundary = batch.prefix_len;
        let old_reuse_start = batch.old_units.len().saturating_sub(batch.suffix_len);
        let new_reuse_start = batch.new_units.len().saturating_sub(batch.suffix_len);
        Self {
            batch,
            restart_boundary,
            old_reuse_start,
            new_reuse_start,
        }
    }

    pub(crate) fn replay_tokens(&self) -> &[TokenData] {
        &self.batch.new_units[self.restart_boundary.min(self.batch.new_units.len())..]
    }

    pub(crate) fn translated_old_boundary(&self, current_new_boundary: usize) -> Option<usize> {
        if current_new_boundary < self.new_reuse_start {
            return None;
        }
        Some(self.old_reuse_start + (current_new_boundary - self.new_reuse_start))
    }
}
