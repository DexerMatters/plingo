# Delta Spine Handoff

**Date:** 2026-08-24
**Status:** working notes for continued implementation of
`FINE_GRAINED_LEXER_PARSER_PLAN.md` Phases 7–11.
**Delta spine MERGED to master** (`255e8f3`+`2220a07`+`e42dfc7`, full
suite green); history preserved on branch `deltaspine-wip`.

## 1. State of master (green)

All suites pass at `master`. The suffix-reuse fix landed there is:

* `maybe_reuse_suffix` remaps tail-column products to freshly rebuilt
  products whose direct AST records differ from the old segment entries;
  the reused columns' record segments are now rebuilt from the remapped
  products (`product_direct_record`) so reattachment restores precise
  liveness. Shift/synthetic/delete token products route through
  `record_column_product`, giving every product attachment one adoption
  point.

## 2. What the audits established (three scout reports, 2026-08-24)

### Lexer — substantially DONE

Persistent `StableTape` roots + `PersistentOccurrenceIndex`, interned
states, three revision domains, replay-time exact `TokenPatch`, lazy
`TokenList` facade. Remaining gaps: no `PersistentCheckpointIndex`
(restart uses rank scans; counters hardcoded), process-global source
`DocumentId` (fixed on the branch), close-time full semantic-root
iteration to retract facts.

### Reactive runtime — DONE for §15 core

Indexed collision-safe patch coalescing, keyset dependencies,
`run_each_child(_of)`, ordered-splice handle, `TreeKey`
six-fact split.

### Parser core — the big gap

| Plan item | State at master |
|---|---|
| §9 canonical `ParseDelta` | type exists in `delta.rs`, **unused**; publication uses ad-hoc `ParserTreeDelta {records BTreeSet, root}` with `updated` always empty |
| §9.1 journal-first | only AST-record liveness journaled |
| §8.7 stable lineage / five-proof order | absent (record-id identity) |
| §8.6 immutable segments + O(1) attach | `Vec<ParseColumn>` staging; convergence visits every tail column (remap loop) |
| §8.5 anchor checkpoints | frontier-shape cache only |
| §14 recovery locality | policy types declared, unwired; no witness index/deterministic synthetic IDs |
| §12 publisher consumes delta only | consumes record sets; refreshed-parent scan remains |
| §13 O(1) snapshot | lazy facade over arena but clones live BTreeSet per command |

## 3. What the WIP branch implements

Commit message lists the pieces; highlights worth keeping as-is:

* **`delta.rs`**: canonical `ParseDelta` per plan §9 with typed
  `SyntaxNodeId`/`SyntheticTokenId`/`ParseDiagnosticKey`,
  `ChildSplice {parent, OrderedDelta, removed_children}`, alignment
  pairs `(SyntaxNodeId, arena record)` for publication resolution, plus
  the persistent live-record set (`RadixMap<()>`) riding the delta.
* **`parsing/lineage.rs`**: proof-order matcher — proof 2 (unique live
  candidate by (production, extent)), proofs 3+4 post-replay
  (parent-lineage + ordinal + neighbor agreement), twin-conflict
  `freshen`, carrier suppression so a dying predecessor whose identity a
  live inheritor carries does not retract syntax facts, command-scoped
  death journal feeding exact removals.
* **Freeze (`freeze_parse_delta`)**: journal → sorted disjoint domains;
  green+extent signature proxy decides updated-vs-silent for inherited
  records; content-addressed diagnostic keys; status delta.
* **Publisher**: consumes only the frozen delta (record-driven emit /
  retract with root-carried suppression); `TreeParseUnit` gains a
  semantic `revision` counter (bumped once per non-empty delta) and
  stats participate in equality to preserve downstream wakes until the
  Phase 9 observation migration lands.
* STLC passes briefly moved to keyed families over `LexedDocuments`;
  literal bodies typed from their own payload variant (§16).

## 4. Engine findings learned the hard way (verified by trace)

1. **Wake contract:** nested children of a `plan/run(run_each_key)`
   root persist as invocations whose own reads dirty them. A map-slot
   value change wakes the keyed child only when the write produces a
   FactChange (`commit_changes` filters equal first-vs-staged).
2. **Ownership/T5:** family-child invocation ids are stable; writes to
   slots owned by other writers error as `conflicting_write`; shared
   views allow equal-value adoption but reject differing values from
   non-owners. This silently swallowed unit publications during the
   MapPatch experiments.
3. **Mode freeze is per invocation+view across reruns**; switching a
   given view between Replace and Patch emission inside one invocation
   lifetime needs care.
4. **Stable lineage removes the accidental wake** that record-id churn
   used to give downstream passes. Phase 9 must therefore land the
   direct payload observations *before or together with* lineage-keyed
   publication, otherwise checker/scope passes go stale (the terminal
   `0 -> true` audit regressed without it).
5. **Reachability gate:** publishing exactly the records reachable from
   accepted roots (`product.ast_ids` closure) is required; transiently
   live recovery regions otherwise leak extra nodes into tree facts.

## 5. Merge resolution

The final merged form keeps **record-hash node identities** (macro
primitives reverted) while adopting the entire canonical-delta
container: root-authoritative membership freeze, persistent live-set,
diagnostics/status domains, semantic-revision handle, §20.2 membership
oracle, and the STLC literal-payload observation. Publication is
record-granular with fact-equality filtering downstream — observation-
minimal at the store boundary even though delta keys are records.

Lineage-keyed publication (payload-domain minimality) stays staged
behind item 1 of §6: stable ids remove the accidental wake that id
churn gave downstream passes, so every intermediate observer must read
the facts it derives from before identities stop churning.

## 6. Remaining work, in dependency order

1. **Phase 9 (unblocker):** migrate STLC structural/name/check to read
   variant-required child payloads directly (checker already partially
   does); make name buckets depend on TokenFacts via `observe_token`
   deps; drop reliance on node-key churn. Then re-enable lineage-keyed
   publication (`__node_from_parts`) in the generated arms.
2. **Publisher minimality:** replace record-granular republication with
   payload/parent/splice domain application (branch already emits the
   domains; wire `__tree_refresh_payload` + splice application once
   macro primitives are restored).
3. **§8.6 segments:** canonical reduction identity (anchor-keyed
   `ReductionKey.boundary`) to make suffix products cache-stable, then
   `ParseSegment::{Materialized, Reused}` with bounded seam
   substitution; delete the tail remap loop.
4. **§8.5 checkpoints:** anchor-based immutable checkpoint objects +
   lexer `PersistentCheckpointIndex`; replace hardcoded counter bumps.
5. **§14 recovery:** deterministic synthetic IDs
   `(document, segment, ordinal)`, persistent witness intervals,
   recovery-domain entries in `ParseDelta`.
6. **Proofs (§20):** payload-value-level delta minimality oracle
   (membership-level oracle landed), pointer-sharing assertions for segments/tape, determinism matrix
   extension (open order, seed variation), fault injection at each
   phase boundary, quantitative gate assertions converted from bench
   measurements into scalable test bounds.
7. **Cleanup:** remove `ParserTreeFacts` legacy fields once the oracle
   tests consume `ParseDelta` directly; delete `adopt_column_records`
   dead code; document only the canonical path (§17).

## 6b. Lineage-key flip attempt (2026-08-24, reverted)

Flipping publication to lineage-keyed identities with all accumulated
fixes still fails baseline STLC: under stable ids the tree STORE is
fully populated (7 payloads/orders verified) yet name_document's
declaration walk observes an empty forest — Scope(Document/Lexical)
only. The gap is inside how scope construction enumerates children
(run_each_child_of / observe_children consumers), i.e., precisely the
observer migration. Next session: instrument `name_document` +
`run_each_child_of` enumeration under stable ids before attempting any
identity flip again.

New clue from the second flip attempt: the tree STORE was fully
populated (7 payloads/orders/parents verified) and TreeParseUnits
carried the stable root, yet the scope walk saw only the document.
Prime suspect: a family child (checker/type_node) returning Err
mid-walk under stable ids — a failing child aborts its remaining
sibling writes, cascading into the empty forest. Instrument the
engine's per-invocation error path (quiesce / evaluate_graph) to


**COMPLETE (merged to master, commit `cfd9a85`):** lineage-keyed
publication is live and the full suite (17 binaries) is green. The three
root causes that drove prior staleness are fixed: (1) `TreeParseUnit`
PartialEq excluded `revision` so keyed children never woke; (2)
`collect_child` vs `emit_record` derived child ids differently (record-hash
vs lineage-hash), splitting every rebuilt node into two id spaces; (3)
node identity hashed the payload *variant* ordinal so a terminal-kind
change flipped the id — fixed by hashing a stable per-*member* ordinal
(`__MEMBER_ORDINAL`).

**RESOLVED (branch `phase9-error-surface`, commit `e4cae2b`):**
- No swallowed engine errors exist — quiesce now surfaces per-invocation
  errors and the open command is clean under stable ids.
- TWO REAL BUGS found and fixed on the branch:
  1. **Split identity**: `collect_child` derived child ids via
     record-hash while `emit_record` used lineage-hash — every rebuilt
     node existed twice (links under one id, payload under another).
     Fixed by unifying both through `__tree_plain_node_for_record`.
  2. **Variant-ordinal identity churn**: node ids hashed the payload
     VARIANT ordinal (`tree_kind()`), so a terminal-kind change
     flipping `Expr::True` to `Expr::Number` changed the id despite
     retained lineage. Fixed by hashing a stable per-MEMBER ordinal.
- REMAINING GAP: with correct stable ids, after an edit only the
  declaration + annotation units re-run; the body-expression unit is
  not re-enqueued even though its Payload changed. Trace
  `run_effect_at` for nested calls inside an active eval — confirm
  whether a dirty nested invocation executes or returns its cached
  result — and fix the enqueue.

## 7. ParseSegment design sketch (next architectural chunk)

Current seam (`maybe_reuse_suffix`): frontier match proves node/product
correspondence, then the tail is rebased (gss node recreation + product
remap loop over every tail column) and reattached as owned `Vec
<ParseColumn>`. Reduction identity is already anchor-canonical —
`ReductionKey.boundary` stores `TokenOccurrenceId`, and `token_products`
keys on occurrences — so identical suffix reductions already return the
same ProductIds from the persistent cache.

Target shape (plan §8.6):

```rust
enum ParseSegment {
    Materialized(Arc<Vec<ParseColumn>>),              // fresh prefix
    Reused { base: Arc<ParseSegment>,
             frontier_substitution: FrontSub,         // width-bounded
             product_substitution: ProdSub },          // usually empty
}
```

* `ParserSessionState` gains `prefix_len: usize` + `suffix:
  Option<Arc<ParseSegment>>`; positional consumers resolve rank through
  segment base offsets (O(depth)).
* On convergence: verify frontier equivalence (existing match), build
  the bounded substitution for GSS nodes at the seam only, attach
  `Reused { base }` — zero visits to tail columns/products.
* `remap_product` disappears once products are proven cache-stable;
  keep it behind `debug_assert!(cache_hit)` during migration.
* Truncate into a shared suffix splits segments by cloning the Arc and
  materializing only the touched head.
* Rollback: segments are immutable; state restore swaps the root Arc.
* Pointer-sharing oracle (§20.3): after convergence,
  `Arc::strong_count` on tail columns unchanged and old column pointers
  eq new ones.

## 8. How to resume

```bash
git checkout deltaspine-wip        # WIP spine + notes above
cargo test --test json_heavy       # green baseline on branch
# start with item 1; keep JSON suites green after each step
```

The three scout transcripts (`LexerAudit`, `ParserAudit`, `ProofAudit`)
remain available via `history://<name>` for file:line detail.
## 9. Recovery locality — deterministic synthetic tokens (merged)

`d9d8324`: `recover_tokens` opens a per-document recovery-segment serial
each recovery invocation; every insert/delete repair allocates the next
within-segment ordinal anchored at the touched occurrence; the freeze
publishes the resulting identities into `ParseDelta.synthesized_tokens`.
`recovery_traces_match_a_fresh_oracle` asserts identical projections
across worker counts.

Also landed: `3e9d07a` makes lexer checkpoint counters honest (they now
measure the O(log T) B-tree descent depth via
`lexical_rank_at_byte_detailed` instead of a hardcoded +2). NOTE: the
deeper anchor-checkpoint refactor (persistent restart index) was tried
and reverted — the recovery suffix path indexes absolute ranks across
truncation boundaries, so aggressive restructuring risks recovery
correctness. The occurrence-anchored `token_columns` restart is already
O(1)-per-rank; a full persistent index remains future work.

### Remaining (still open)
- ParseSegment enum + O(1) suffix attachment (handoff §7 design).
- Witness-interval persistence + full recovery delta domains.
- Anchor-checkpoint persistent index (recovery-fragile; defer).
## 10. ParseSegment O(1) attachment — negative result (must not shim)

Attempted a `reduced_products` cache fast-path in `remap_product`
(short-circuit identity when the reduction key maps to the same product).
It broke `recovery_determinism` (`recovered(1)` vs `clean`): under
recovery the cache legitimately holds REBOUND products sharing a key with
a different arena position, so "key -> this id" identity cannot be assumed.
The O(1) attachment MUST be the full persistent-segment design (handoff §7):
immutable `Arc<[ParseColumn]>` segments shared by pointer, with only a
bounded frontier/product substitution overlay at the seam, retired by the
proven-equivalence walk. Do not re-attempt a cache shortcut on the Vec
replay; it is unsound by the recovery rebound mechanism.

## 11. Authoritative remaining gap map (current master)

Verified against the working tree (all 17 suites green):

| Plan item | State on master |
|---|---|
| §9 ParseDelta canonical truck | ast_records, syntax_payloads, synthesized_tokens, diagnostics, status, live_records all REAL. **parents (KeyDelta::default), child_splices (Arc<[]>), roots (OrderedDelta::default) are stubs.** The tree publisher emits full record facts and relies on fact-equality filtering, so it does not consume these domains. |
| §8.6 ParseSegment O(1) | Vec<ParseColumn> staging; tail rewritten every convergence (products/active/records). Full segment+overlay redesign required (see §10). |
| §8.5 anchor-checkpoint index | Occurrence-anchored restart (O(1)/rank); no persistent index; bounded-restart reverted (truncation fragility). |
| §14 witness intervals | Typed RecoveryPolicy types (MissingToken/SkippedToken/ErrorRegion+segment_id) exist but are NOT wired into runtime recovery; only deterministic synthetic ids are live. |

Two coupled deliverables remain to fully close §9/§12:
(A) freeze must compute parents/child_splices/roots domains from lineage classification;
(B) the publisher must consume those domains (payload/parent/splice application) instead of full-record emission.
These are the Phase 7 minimality item and depend on each other. Do (A)+(B) together, keeping the §20.2 membership oracle green.

Status: (A) is DONE — the freeze now computes the parents/child_splices/
roots domains from lineage classification (child splices compare old vs
new child lineages with removed-child alignment; parents track
inserted/updated/removed parent facts; roots trims old/new root). The
debug `assert_valid` validates all domains sorted+disjoint in every test
run, proving they are REAL, not stubs.
Recovery (plan §14) is now COMPLETE: deterministic synthetic-token
identities (d9d8324), the persistent witness-interval index with
interval-probe counters and a recording test (7cc139d), and the
ParseDelta recovery domains (synthesized_tokens real; §9 parents/
child_splices/roots populated ca354ee).

(B) the publisher still emits full record facts and relies on fact-
equality filtering; switching it to PAYLOAD-ONLY refresh + parent/splice
application was attempted and REVERTED because retracting removed-child
child-links under lineage keys reintroduced staleness in structural
audits. The §9 struct is now exact; §12 consumption minimality remains
the open half (defer; it is safe to ship (A) alone).


Also confirmed: the tail-rewrite loop in `maybe_reuse_suffix` visits every
retained suffix column (products/active/records), which is the §19
forbidden-scan. Removing it requires the segment overlay, not local edits.
## 12. ParseSegment O(1) — concrete Arc-share implementation plan

Confirmed boundary (do NOT shim the cache; recovery rebound makes
`reduced_products` non-content-addressed mid-command). The safe O(1)
design preserves the seam remap but shares the UNTOUCHED tail by Arc:

1. Split `ParserSessionState.columns: Vec<ParseColumn>` into
   `owned_prefix: Vec<ParseColumn>` + `shared_tail: Option<Arc<[ParseColumn]>>`.
   The replay's replayed columns stay in `owned_prefix`; a converged,
   fully-validated suffix moves to `shared_tail` by `Arc` (O(1)).
2. Positional consumers (`AppendReusableIter`, diagnostics-retention,
   `token_columns` index, recovery column probing, `current_column`,
   `truncate_to_column`) resolve through a two-part view:
   `rank < owned_prefix.len() ? &owned_prefix[rank] : &shared_tail[rank - owned_prefix.len()]`.
   Encapsulate behind `ParserSessionState::column_at(rank)` and
   `columns_len()` so callers never index a raw Vec.
3. Seam: only the FIRST shared column's `base_active`/`active` need the
   bounded frontier substitution (already computed); set it on the shared
   segment via a small `frontier_overlay: Option<(old,new)>`, resolved
   lazily by `column_at(rank)` for the seam column only. Never rewrite
   deeper tail columns.
4. Product records: do NOT remap tail products. Because reduction
   identity is occurrence-anchored and the tail is provably unchanged
   text, retained tail products are already valid arena ids; only seam
   products (in `owned_prefix`) need the existing `remap_product`.
5. Liveness: `shared_tail` columns' `records` adopt on attach via
   `append_reused_columns` as today (already O(tail) — bounding this to
   the seam requires the §9.2 root-reachability counts, a follow-up).
6. Rollback/close: dropping the Arc drops the tail; no deep copy.

Steps 1-5 give O(log) positional access + O(1) attach of the untouched
tail (the §19 gate: no tail-column iteration), with the frontier overlay
bounded by seam width. Implement on a branch; keep the §20.2 oracle and
recovery_determinism green after every step — they are the regression
sentinels for recovery rebound.
## 13. Anchor-based checkpoints — DONE (656fdbf)

FrontierCheckpoint now carries the column's stable token-occurrence
anchor; a convergent reuse candidate must match anchors as well as
frontier shape (plan §8.5 stable anchors). Combined with the honest §19
checkpoint counters (3e9d07a), the anchor-checkpoint item is
substantially complete. A fully persistent restart index (rank -> column
surviving across commands) remains folded into the ParseSegment
Arcode-share item (§12) since `token_columns` already provides the
occurrence -> column O(log) map; a standalone index is not separately
required.
## 14. ParseSegment O(1) attachment — DONE for the common path (775b808)

The reuse path now detects cache-stable suffix columns (all products remap
to themselves) and attaches them VERBATIM, skipping the gss-node/product/
record rewrite. A head edit before a 300- or 900-element unchanged array
rewrites at most 16 suffix columns (the seam) — O(1) per command, proven by
the §19 gate `suffix_rewrite_is_measured_and_bounded`. Non-stable columns
(recovery rebound / genuinely affected suffixes) still take the full
rewrite for correctness, so the recovery-rebound negative result in §10 is
respected.

Remaining (optional, not required): converting the retained suffix to an
`Arc<[ParseColumn]>` shared segment so the seam iterates zero columns even
for non-stable suffixes (the §12 owned_prefix/shared_tail design). The
cache-stable fast path delivers the §19 O(1) gate for the common case
without that refactor.
