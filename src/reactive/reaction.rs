//! Command-local reaction-graph capture (follow-up plan §4 item 6).
//!
//! A [`ReactionDigest`] records exactly which computation evaluated, which
//! view element drove it, which exact elements it read, and which output
//! elements it wrote or retracted. The engine appends one entry per
//! successful evaluation and one per retirement; the digest rides out of the
//! command as a typed [`crate::reactive::CommandReport::metric`].
//!
//! This is observation-only instrumentation: recording adds no facts,
//! installs no dependencies, and rolls back with a failed command because
//! the metric frame is discarded on error.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};

/// Capture gate: recording builds exact edge sets, so it runs only when a
/// consumer opted in (correctness tests). Release benchmarks keep it off;
/// the engine's other counters stay unconditional.
static CAPTURE_ENABLED: AtomicBool = AtomicBool::new(true);

/// Enables reaction-digest recording for the whole process. Recording is
/// ON by default so correctness suites observe exact edges transparently;
/// release benchmarks call [`disable_capture`] because capture allocates.
pub fn enable_capture() {
    CAPTURE_ENABLED.store(true, Ordering::Relaxed);
}

/// Disables reaction-digest recording.
pub fn disable_capture() {
    CAPTURE_ENABLED.store(false, Ordering::Relaxed);
}

/// Whether recording is currently on.
pub fn capture_enabled() -> bool {
    CAPTURE_ENABLED.load(Ordering::Relaxed)
}

/// One exact view element named by a read or output edge.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ElementEdge {
    /// Stable view name (`View::name()`).
    pub view: &'static str,
    /// Deterministic rendering of the exact encoded element key. Absence
    /// reads render the domain marker instead of a key.
    pub element: String,
}

impl ElementEdge {
    pub(crate) fn keyed<V: crate::reactive::View>(
        key: &dyn crate::reactive::value::KeyValue,
    ) -> Self {
        Self {
            view: V::name(),
            element: format!("{key:?}"),
        }
    }

    pub(crate) fn domain<V: crate::reactive::View>() -> Self {
        Self {
            view: V::name(),
            element: "<domain>".to_owned(),
        }
    }
}

impl std::fmt::Display for ElementEdge {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}::{}", self.view, self.element)
    }
}

/// One output element written by an evaluation.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct OutputEdge {
    pub view: &'static str,
    pub element: String,
    /// The committed transition this evaluation produced, if any:
    /// `insert`, `update`, or `retract`. `None` marks an equal-value
    /// rewrite that committed nothing.
    pub committed: Option<&'static str>,
}

/// One evaluated computation invocation with its exact edges.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvaluatedComponent {
    /// The authored function name (the component definition until Phase 2
    /// introduces first-class descriptors).
    pub definition: &'static str,
    /// Definition callsite: `file:line:column`.
    pub callsite: String,
    /// The exact driving element (the semantic input key).
    pub driving_element: String,
    /// Exact read element edges, sorted and deduplicated. Temporal reads
    /// are suffixed `@previous`.
    pub reads: Vec<ElementEdge>,
    /// Exact output element edges with their committed transitions, sorted
    /// by `(view, element)`.
    pub outputs: Vec<OutputEdge>,
}

/// One retired computation invocation with its retraction domain.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RetiredComponent {
    pub definition: &'static str,
    pub callsite: String,
    pub driving_element: String,
    /// Exact output elements retracted by the retirement.
    pub retracted_outputs: Vec<ElementEdge>,
}

/// The command's complete reaction graph capture.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReactionDigest {
    /// Evaluations in scheduling order (deterministic under the ordered
    /// dirty queue).
    pub evaluations: Vec<EvaluatedComponent>,
    /// Retirements in retraction order.
    pub retirements: Vec<RetiredComponent>,
    /// Broad enumerations: whole-view/keyset reads without an exact
    /// element. Sorted and deduplicated.
    pub broad_enumerations: Vec<ElementEdge>,
}

impl ReactionDigest {
    /// Appends one evaluation capture, canonicalizing its edge sets.
    pub(crate) fn push_evaluation(&mut self, evaluation: EvaluatedComponent) {
        let mut evaluation = evaluation;
        evaluation.reads.sort();
        evaluation.reads.dedup();
        evaluation.outputs.sort();
        self.evaluations.push(evaluation);
    }

    /// Appends one retirement capture.
    pub(crate) fn push_retirement(&mut self, retirement: RetiredComponent) {
        let mut retirement = retirement;
        retirement.retracted_outputs.sort();
        retirement.retracted_outputs.dedup();
        self.retirements.push(retirement);
    }

    /// Records one broad enumeration.
    pub(crate) fn push_broad_enumeration(&mut self, edge: ElementEdge) {
        if !self.broad_enumerations.contains(&edge) {
            self.broad_enumerations.push(edge);
            self.broad_enumerations.sort();
        }
    }

    /// Evaluations of one definition name.
    pub fn evaluations_of(&self, definition: &str) -> impl Iterator<Item = &EvaluatedComponent> {
        self.evaluations
            .iter()
            .filter(move |e| e.definition == definition)
    }

    /// True when nothing was recorded.
    pub fn is_empty(&self) -> bool {
        self.evaluations.is_empty()
            && self.retirements.is_empty()
            && self.broad_enumerations.is_empty()
    }

    /// Renders the canonical multi-line digest form used by tests.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for evaluation in &self.evaluations {
            let _ = writeln!(
                out,
                "eval {} ({}) drive={}",
                evaluation.definition, evaluation.callsite, evaluation.driving_element
            );
            for read in &evaluation.reads {
                let _ = writeln!(out, "  read {}", read);
            }
            for output in &evaluation.outputs {
                match output.committed {
                    Some(kind) => {
                        let _ = writeln!(out, "  write {} [{kind}]", output.element);
                    }
                    None => {
                        let _ = writeln!(out, "  touch {}", output.element);
                    }
                }
                let _ = writeln!(out, "    on {}", output.view);
            }
        }
        for retirement in &self.retirements {
            let _ = writeln!(
                out,
                "retire {} ({}) drive={}",
                retirement.definition, retirement.callsite, retirement.driving_element
            );
            for edge in &retirement.retracted_outputs {
                let _ = writeln!(out, "  retract {}", edge);
            }
        }
        for edge in &self.broad_enumerations {
            let _ = writeln!(out, "enumerate {}", edge);
        }
        out
    }

    /// Canonical single-line summary hashable across warm/cold runs.
    pub fn fingerprint(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.render().hash(&mut hasher);
        std::hash::Hasher::finish(&hasher)
    }
}
