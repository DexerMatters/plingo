use std::{collections::HashSet, hash::Hash};

use fluent_uri::Uri;

use super::{
    ScopeProperty,
    data::{
        AstOwner, FrameDraft, FrameRecord, PatchBuilder, Scope, ScopeDatum, ScopeEdge, ScopeError,
        ScopeFrameKey, ScopeSnapshot,
    },
};

impl<Anchor, Label, Datum, Reference, Request>
    ScopeSnapshot<Anchor, Label, Datum, Reference, Request>
where
    Anchor: Clone + Eq + Hash,
    Label: Clone + Eq + Hash,
    Datum: Clone + Eq + Hash,
    Reference: Clone + Eq + Hash,
    Request: Clone + Eq + Hash,
{
    fn allocate_scope(
        &mut self,
        patch: &mut PatchBuilder<Label, Datum, Reference, Request>,
    ) -> Scope {
        let scope = Scope(self.next_scope);
        self.next_scope += 1;
        self.graph.scopes.insert(scope);
        patch.add_scope(scope);
        scope
    }

    pub(crate) fn root_scope(
        &mut self,
        uri: Uri<&'static str>,
        patch: &mut PatchBuilder<Label, Datum, Reference, Request>,
    ) -> Scope {
        if let Some(scope) = self.root_scopes.get(&uri) {
            return *scope;
        }
        let scope = self.allocate_scope(patch);
        self.root_scopes.insert(uri, scope);
        scope
    }

    pub(crate) fn ast_scope(
        &mut self,
        owner: &AstOwner,
        patch: &mut PatchBuilder<Label, Datum, Reference, Request>,
    ) -> Scope {
        if let Some(scope) = self.ast_scopes.get(owner) {
            return *scope;
        }
        let scope = self.allocate_scope(patch);
        self.ast_scopes.insert(owner.clone(), scope);
        scope
    }

    pub(crate) fn external_scope(
        &mut self,
        anchor: Anchor,
        patch: &mut PatchBuilder<Label, Datum, Reference, Request>,
    ) -> Scope {
        if let Some(scope) = self.external_scopes.get(&anchor) {
            return *scope;
        }
        let scope = self.allocate_scope(patch);
        self.external_scopes.insert(anchor, scope);
        scope
    }

    fn reaches_acyclic(&self, start: Scope, target: Scope) -> bool {
        let mut pending = vec![start];
        let mut seen = HashSet::new();
        while let Some(scope) = pending.pop() {
            if scope == target {
                return true;
            }
            if !seen.insert(scope) {
                continue;
            }
            pending.extend(self.graph.edges.iter().filter_map(|(edge, count)| {
                (*count > 0 && edge.source == scope && edge.property == ScopeProperty::Acyclic)
                    .then_some(edge.target)
            }));
        }
        false
    }

    fn add_edge(
        &mut self,
        edge: ScopeEdge<Label>,
        patch: &mut PatchBuilder<Label, Datum, Reference, Request>,
    ) -> Result<(), ScopeError> {
        if !self.graph.scopes.contains(&edge.source) {
            return Err(ScopeError::MissingScope(edge.source));
        }
        if !self.graph.scopes.contains(&edge.target) {
            return Err(ScopeError::MissingScope(edge.target));
        }
        if edge.property == ScopeProperty::Acyclic && self.reaches_acyclic(edge.target, edge.source)
        {
            return Err(ScopeError::Cycle {
                from: edge.source,
                to: edge.target,
            });
        }
        let count = self.graph.edges.entry(edge.clone()).or_default();
        if *count == 0 {
            patch.add_edge(edge);
        }
        *count += 1;
        Ok(())
    }

    fn remove_edge(
        &mut self,
        edge: &ScopeEdge<Label>,
        patch: &mut PatchBuilder<Label, Datum, Reference, Request>,
    ) {
        let Some(count) = self.graph.edges.get_mut(edge) else {
            return;
        };
        *count -= 1;
        if *count == 0 {
            self.graph.edges.remove(edge);
            patch.remove_edge(edge.clone());
        }
    }

    fn remove_datum(
        &mut self,
        datum: &ScopeDatum<Datum>,
        patch: &mut PatchBuilder<Label, Datum, Reference, Request>,
    ) {
        let Some(count) = self.graph.datums.get_mut(datum) else {
            return;
        };
        *count -= 1;
        if *count == 0 {
            self.graph.datums.remove(datum);
            patch.remove_datum(datum.clone());
        }
    }

    fn remove_record_facts(
        &mut self,
        record: &FrameRecord<Label, Datum, Reference, Request>,
        patch: &mut PatchBuilder<Label, Datum, Reference, Request>,
    ) {
        for edge in &record.edges {
            self.remove_edge(edge, patch);
        }
        for datum in &record.datums {
            self.remove_datum(datum, patch);
        }
        for (id, reference) in &record.references {
            self.references.remove(id);
            patch.remove_reference(reference.clone());
        }
        for request in &record.requests {
            let Some(count) = self.request_counts.get_mut(request) else {
                continue;
            };
            *count -= 1;
            if *count == 0 {
                self.request_counts.remove(request);
                patch.release_source(request.clone());
            }
        }
    }

    fn install_draft(
        &mut self,
        owner: AstOwner,
        draft: FrameDraft<Label, Datum, Reference, Request>,
        patch: &mut PatchBuilder<Label, Datum, Reference, Request>,
    ) -> Result<FrameRecord<Label, Datum, Reference, Request>, ScopeError> {
        for datum in &draft.datums {
            let count = self.graph.datums.entry(datum.clone()).or_default();
            if *count == 0 {
                patch.add_datum(datum.clone());
            }
            *count += 1;
        }
        for edge in &draft.edges {
            self.add_edge(edge.clone(), patch)?;
        }
        let mut references = Vec::with_capacity(draft.references.len());
        for reference in draft.references {
            let id = self.next_fact;
            self.next_fact += 1;
            self.references.insert(id, reference.clone());
            patch.add_reference(reference.clone());
            references.push((id, reference));
        }
        for request in &draft.requests {
            let count = self.request_counts.entry(request.clone()).or_default();
            if *count == 0 {
                patch.require_source(request.clone());
            }
            *count += 1;
        }
        Ok(FrameRecord {
            owner,
            children: draft.children,
            edges: draft.edges,
            datums: draft.datums,
            references,
            requests: draft.requests,
        })
    }

    pub(crate) fn replace_frame(
        &mut self,
        key: ScopeFrameKey,
        owner: AstOwner,
        draft: FrameDraft<Label, Datum, Reference, Request>,
        patch: &mut PatchBuilder<Label, Datum, Reference, Request>,
    ) -> Result<(), ScopeError> {
        let old = self.frames.remove(&key);
        let old_children = old
            .as_ref()
            .map(|record| record.children.clone())
            .unwrap_or_default();
        if let Some(record) = &old {
            self.remove_record_facts(record, patch);
        }

        for child in &draft.children {
            self.parents
                .entry(child.clone())
                .or_default()
                .insert(key.clone());
        }

        let record = self.install_draft(owner, draft, patch)?;
        let new_children = record.children.clone();
        self.frames.insert(key.clone(), record);
        patch.rebuilt_frames += 1;

        let removed_children = old_children
            .difference(&new_children)
            .cloned()
            .collect::<Vec<_>>();
        for child in &removed_children {
            if let Some(parents) = self.parents.get_mut(child) {
                parents.remove(&key);
                if parents.is_empty() {
                    self.parents.remove(child);
                }
            }
        }
        for child in removed_children {
            self.retract_if_orphan(&child, patch);
        }
        Ok(())
    }

    pub(crate) fn replace_roots(
        &mut self,
        uri: Uri<&'static str>,
        roots: HashSet<ScopeFrameKey>,
        patch: &mut PatchBuilder<Label, Datum, Reference, Request>,
    ) {
        let old = self.roots.insert(uri, roots.clone()).unwrap_or_default();
        for stale in old.difference(&roots).cloned().collect::<Vec<_>>() {
            self.retract_if_orphan(&stale, patch);
        }
        self.collect_unreferenced_ast_scopes(patch);
    }

    /// Releases cached scope-frame state after the node runtime has dropped a
    /// frame task. Published facts are runtime-owned; this only keeps private
    /// ownership counts and allocation maps from retaining stale frames.
    pub(crate) fn forget_frame(
        &mut self,
        key: &ScopeFrameKey,
        patch: &mut PatchBuilder<Label, Datum, Reference, Request>,
    ) {
        for roots in self.roots.values_mut() {
            roots.remove(key);
        }
        self.forget_frame_record(key, patch);
        self.collect_unreferenced_ast_scopes(patch);
    }

    /// Releases one document's root ownership while retaining contextual frame
    /// records that still have an independently live graph task.
    pub(crate) fn forget_root<F>(
        &mut self,
        uri: Uri<&'static str>,
        mut frame_is_live: F,
        patch: &mut PatchBuilder<Label, Datum, Reference, Request>,
    ) where
        F: FnMut(&ScopeFrameKey) -> bool,
    {
        let roots = self.roots.remove(&uri).unwrap_or_default();
        for frame in roots {
            if !frame_is_live(&frame) {
                self.forget_frame_record(&frame, patch);
            }
        }
        self.collect_unreferenced_ast_scopes(patch);
    }

    fn forget_frame_record(
        &mut self,
        key: &ScopeFrameKey,
        patch: &mut PatchBuilder<Label, Datum, Reference, Request>,
    ) {
        let Some(record) = self.frames.remove(key) else {
            return;
        };
        self.remove_record_facts(&record, patch);
        patch.removed_frames += 1;

        if let Some(parents) = self.parents.remove(key) {
            for parent in parents {
                if let Some(record) = self.frames.get_mut(&parent) {
                    record.children.remove(key);
                }
            }
        }
        for child in record.children {
            if let Some(parents) = self.parents.get_mut(&child) {
                parents.remove(key);
                if parents.is_empty() {
                    self.parents.remove(&child);
                }
            }
            self.retract_if_orphan(&child, patch);
        }
    }

    fn is_root(&self, key: &ScopeFrameKey) -> bool {
        self.roots.values().any(|roots| roots.contains(key))
    }

    fn retract_if_orphan(
        &mut self,
        key: &ScopeFrameKey,
        patch: &mut PatchBuilder<Label, Datum, Reference, Request>,
    ) {
        if self.is_root(key)
            || self
                .parents
                .get(key)
                .is_some_and(|parents| !parents.is_empty())
        {
            return;
        }
        let Some(record) = self.frames.remove(key) else {
            return;
        };
        self.remove_record_facts(&record, patch);
        self.parents.remove(key);
        patch.removed_frames += 1;
        for child in record.children {
            if let Some(parents) = self.parents.get_mut(&child) {
                parents.remove(key);
                if parents.is_empty() {
                    self.parents.remove(&child);
                }
            }
            self.retract_if_orphan(&child, patch);
        }
    }

    fn collect_unreferenced_ast_scopes(
        &mut self,
        patch: &mut PatchBuilder<Label, Datum, Reference, Request>,
    ) {
        let live_owners = self
            .frames
            .values()
            .map(|frame| frame.owner.clone())
            .collect::<HashSet<_>>();
        let stale = self
            .ast_scopes
            .iter()
            .filter_map(|(owner, scope)| {
                (!live_owners.contains(owner)
                    && !self.graph.datums.keys().any(|datum| datum.scope == *scope)
                    && !self
                        .graph
                        .edges
                        .keys()
                        .any(|edge| edge.source == *scope || edge.target == *scope))
                .then_some((owner.clone(), *scope))
            })
            .collect::<Vec<_>>();
        for (owner, scope) in stale {
            self.ast_scopes.remove(&owner);
            if self.graph.scopes.remove(&scope) {
                patch.remove_scope(scope);
            }
        }
    }
}
