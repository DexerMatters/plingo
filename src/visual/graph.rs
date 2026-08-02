//! Structural rendering of graph-native scope facts.
//!
//! The renderer keeps scope identities available as dim suffixes while making
//! domain-defined scope keys, directed edges, and mapped scope data the
//! primary visual structure.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::{self, Debug},
};

use color_print::cformat;

use crate::{
    Graph, ReadGraph,
    component::{
        scope::{
            ScopeAllocation, ScopeAllocations, ScopeDomain, ScopeEdge, ScopeId, ScopeProperty,
            ScopeStructure,
        },
        structural::{StructureEdges, StructureNode},
    },
    utils::PrettyDisplay,
};

/// A point-in-time, renderable view of one domain's scope graph.
#[derive(Clone)]
pub struct ScopeGraph<D: ScopeDomain> {
    allocations: Vec<ScopeAllocation<D>>,
    edges: Vec<ScopeEdge<D>>,
    data: BTreeMap<ScopeId<D>, D::ScopeData>,
}

impl<D: ScopeDomain> ScopeGraph<D> {
    /// Captures all visible scope facts for `D` from `graph`.
    pub fn from_graph(graph: &Graph) -> Self {
        let allocations = graph.scan_all::<ScopeAllocations<D>>();
        let data = allocations
            .iter()
            .filter_map(|allocation| {
                graph
                    .get::<StructureNode<ScopeStructure<D>>>(allocation.scope)
                    .and_then(|artifact| artifact.deref::<D::ScopeData>())
                    .map(|data| (allocation.scope, (*data).clone()))
            })
            .collect();
        Self {
            allocations,
            edges: graph.scan_all::<StructureEdges<ScopeStructure<D>>>(),
            data,
        }
    }
}

impl<D> PrettyDisplay for ScopeGraph<D>
where
    D: ScopeDomain,
    D::ScopeKey: Debug,
    D::ScopeData: Debug,
    D::Label: Debug,
{
    fn pretty_fmt(&self, formatter: &mut fmt::Formatter<'_>, _: &()) -> fmt::Result {
        writeln!(
            formatter,
            "{}",
            cformat!(
                "<bold,blue>scope graph</> <dim>{} {} · {} {} · {} {}</>",
                self.allocations.len(),
                plural(self.allocations.len(), "scope"),
                self.edges.len(),
                plural(self.edges.len(), "edge"),
                self.data.len(),
                plural(self.data.len(), "datum"),
            )
        )?;

        let mut scopes = BTreeSet::new();
        let mut keys = BTreeMap::new();
        for allocation in &self.allocations {
            scopes.insert(allocation.scope);
            keys.entry(allocation.scope)
                .or_insert_with(|| compact_debug(&allocation.key));
        }

        let mut outgoing: BTreeMap<ScopeId<D>, Vec<&ScopeEdge<D>>> = BTreeMap::new();
        let mut incoming = BTreeSet::new();
        for edge in &self.edges {
            scopes.insert(edge.source);
            scopes.insert(edge.target);
            outgoing.entry(edge.source).or_default().push(edge);
            incoming.insert(edge.target);
        }
        for edges in outgoing.values_mut() {
            edges.sort_by_key(|edge| {
                (
                    format_args!("{:?}", edge.label).to_string(),
                    edge.target.id(),
                    format_args!("{:?}", edge.property).to_string(),
                )
            });
        }

        if scopes.is_empty() {
            return writeln!(formatter, "{}", cformat!("<dim>empty</>"));
        }

        let mut visited = BTreeSet::new();
        let roots = scopes
            .iter()
            .copied()
            .filter(|scope| !incoming.contains(scope))
            .collect::<Vec<_>>();
        for root in roots {
            render_scope(
                formatter,
                root,
                None,
                "",
                true,
                &keys,
                &self.data,
                &outgoing,
                &mut visited,
            )?;
        }
        while let Some(&root) = scopes.iter().find(|scope| !visited.contains(scope)) {
            if !visited.is_empty() {
                writeln!(
                    formatter,
                    "{}",
                    cformat!("<dim>↳ disconnected or cyclic component</>")
                )?;
            }
            render_scope(
                formatter,
                root,
                None,
                "",
                true,
                &keys,
                &self.data,
                &outgoing,
                &mut visited,
            )?;
        }

        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn render_scope<D>(
    formatter: &mut fmt::Formatter<'_>,
    scope: ScopeId<D>,
    incoming: Option<&ScopeEdge<D>>,
    prefix: &str,
    is_last: bool,
    keys: &BTreeMap<ScopeId<D>, String>,
    data: &BTreeMap<ScopeId<D>, D::ScopeData>,
    outgoing: &BTreeMap<ScopeId<D>, Vec<&ScopeEdge<D>>>,
    visited: &mut BTreeSet<ScopeId<D>>,
) -> fmt::Result
where
    D: ScopeDomain,
    D::ScopeData: Debug,
    D::Label: Debug,
{
    let key = keys
        .get(&scope)
        .map(String::as_str)
        .unwrap_or("unallocated scope");
    let id = scope.id();

    if let Some(edge) = incoming {
        let branch = if is_last { "└─" } else { "├─" };
        let marker = if edge.property == ScopeProperty::Cyclic {
            " ↺"
        } else {
            ""
        };
        write!(
            formatter,
            "{}{} {}",
            prefix,
            branch,
            cformat!(
                "<yellow>{:?}</> <dim>→</> <bold,blue>{}</> <dim>⟨{}⟩</>{}",
                edge.label,
                key,
                id,
                marker,
            )
        )?;
    } else {
        write!(
            formatter,
            "{}",
            cformat!("<dim>•</> <bold,blue>{}</> <dim>⟨{}⟩</>", key, id)
        )?;
    }

    if !visited.insert(scope) {
        writeln!(formatter, " <dim>(see above)</>")?;
        return Ok(());
    }
    writeln!(formatter)?;

    let child_prefix = if incoming.is_none() {
        format!("{prefix}  ")
    } else if is_last {
        format!("{prefix}   ")
    } else {
        format!("{prefix}│  ")
    };

    if let Some(scope_data) = data.get(&scope) {
        let scope_data = compact_debug(scope_data);
        writeln!(
            formatter,
            "{}",
            cformat!("<dim>{}·</> <green>{}</>", child_prefix, scope_data)
        )?;
    }

    if let Some(edges) = outgoing.get(&scope) {
        for (index, edge) in edges.iter().enumerate() {
            render_scope(
                formatter,
                edge.target,
                Some(edge),
                &child_prefix,
                index + 1 == edges.len(),
                keys,
                data,
                outgoing,
                visited,
            )?;
        }
    }

    Ok(())
}

fn compact_debug(value: &impl Debug) -> String {
    let mut rendered = format!("{value:?}");
    compact_nested_debug(&mut rendered, "AstKey", |body| {
        body.split_once("id:")
            .and_then(|(_, rest)| rest.split([',', '}']).next())
            .map(|id| format!("ast#{}", id.trim()))
    });
    compact_nested_debug(&mut rendered, "Uri", |_| Some("uri".to_owned()));
    rendered
}

fn compact_nested_debug<F>(rendered: &mut String, type_name: &str, replacement: F)
where
    F: Fn(&str) -> Option<String>,
{
    let marker = format!("{type_name} {{");
    let mut search_from = 0;
    while let Some(relative_start) = rendered[search_from..].find(&marker) {
        let start = search_from + relative_start;
        let body_start = start + marker.len();
        let mut depth = 1;
        let mut end = body_start;
        for (offset, character) in rendered[body_start..].char_indices() {
            match character {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        end = body_start + offset;
                        break;
                    }
                }
                _ => {}
            }
        }
        if end == body_start {
            break;
        }
        let body = &rendered[body_start..end];
        let Some(replacement) = replacement(body) else {
            search_from = end + 1;
            continue;
        };
        rendered.replace_range(start..=end, &replacement);
        search_from = start + replacement.len();
    }
}

fn plural(count: usize, singular: &str) -> String {
    if count == 1 {
        singular.to_owned()
    } else {
        format!("{singular}s")
    }
}
