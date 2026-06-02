use crate::component::{lex::VisibleTokenBatch, parse::TokenData};

#[derive(Debug, Clone)]
pub(crate) struct ReplayPlan {
    pub batch: VisibleTokenBatch,
    pub restart_boundary: usize,
    pub old_reuse_start: usize,
    pub new_reuse_start: usize,
}

impl ReplayPlan {
    pub(crate) fn from_batch(batch: VisibleTokenBatch) -> Self {
        let restart_boundary = batch.prefix_len;
        let old_reuse_start = batch.old_tokens.len().saturating_sub(batch.suffix_len);
        let new_reuse_start = batch.new_tokens.len().saturating_sub(batch.suffix_len);
        Self {
            batch,
            restart_boundary,
            old_reuse_start,
            new_reuse_start,
        }
    }

    pub(crate) fn replay_tokens(&self) -> &[TokenData] {
        &self.batch.new_tokens[self.restart_boundary.min(self.batch.new_tokens.len())..]
    }

    pub(crate) fn translated_old_boundary(&self, current_new_boundary: usize) -> Option<usize> {
        if current_new_boundary < self.new_reuse_start {
            return None;
        }
        Some(self.old_reuse_start + (current_new_boundary - self.new_reuse_start))
    }
}
