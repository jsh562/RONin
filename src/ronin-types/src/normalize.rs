//! Multi-source normalization into one [`TypeModel`] {TR-010, TR-011, TR-015}.
//!
//! [`normalize`] is the merge step of the acquisition pipeline: given a set of
//! [`TypeSource`] adapters (any mix of `syn`, schemars, user-schema, or future
//! sources), it calls [`TypeSource::acquire`] on each, **ranks** their partial
//! models by [`SourcePrecedence`], and folds them into a single normalized
//! [`TypeModel`] (OBJ4).
//!
//! # Precedence & conflict resolution (TR-010)
//!
//! The total order is exactly **user-schema > schemars > syn** (and any future
//! source slots into the same [`SourcePrecedence`] order). When two sources
//! define the same named type:
//!
//! - If the shapes **agree**, the type is kept once (no diagnostic).
//! - If the shapes **disagree**, the higher-precedence source wins and one
//!   [`DiagnosticCategory::SourceConflict`] diagnostic is emitted naming the
//!   winner and the loser (their `source_id`s).
//! - Ties at equal precedence are broken **last-writer-wins** in iteration order,
//!   also recording a conflict diagnostic so the ambiguity is auditable.
//!
//! Merging is deterministic and independent of the order sources are supplied:
//! sources are sorted by descending precedence (stable within a rank) before the
//! fold, so a winner is chosen the same way every run.
//!
//! # Provenance (TR-010)
//!
//! Every accepted named type records which source contributed it as an
//! `info`-severity [`AcquisitionDiagnostic`] (`subject` = type name, `source_id`
//! = the winning source). This is the auditable provenance trail a consumer
//! (E006) can surface.
//!
//! # Diagnostics & never-fail (TR-011, TR-015)
//!
//! Every adapter's own diagnostics, plus the merge-time conflict/provenance
//! diagnostics, accumulate into [`TypeModel::diagnostics`]. Normalization never
//! fails: with **no sources** (or sources that contribute nothing) the result is
//! an empty / structural-only model ([`TypeModel::is_empty`] is `true`), which is
//! a legal fallback state, not an error (TR-015, ADR-0004 Progressive
//! Intelligence).

use std::collections::BTreeMap;

use crate::diagnostics::{AcquisitionDiagnostic, DiagnosticCategory, DiagnosticSeverity};
use crate::model::{TypeModel, TypeNode};
use crate::source::{SourcePrecedence, TypeSource};

/// Merge a set of [`TypeSource`] adapters into one normalized [`TypeModel`]
/// (TR-010, TR-011, TR-015).
///
/// Each source is acquired, ranked by [`SourcePrecedence`] (user-schema >
/// schemars > syn), and folded into a single model. Conflicting definitions of
/// the same named type resolve in the higher-precedence source's favour with a
/// recorded [`DiagnosticCategory::SourceConflict`]; per-type provenance and every
/// adapter's diagnostics accumulate into [`TypeModel::diagnostics`].
///
/// With no sources, or sources that contribute no named types, the result is a
/// valid empty / structural-only model — never an error (TR-015).
#[must_use]
pub fn normalize(sources: &[Box<dyn TypeSource>]) -> TypeModel {
    // Acquire every source first; pair each result with its declared precedence
    // and stable source id so the fold is order-independent (TR-010).
    let mut acquisitions: Vec<Acquisition> = sources
        .iter()
        .map(|source| {
            let acquired = source.acquire();
            Acquisition {
                source_id: source.source_id(),
                precedence: source.precedence(),
                model: acquired.model,
                diagnostics: acquired.diagnostics,
            }
        })
        .collect();

    // Highest precedence first; `sort_by_key` is stable, so ties within a rank
    // fall back to the supplied order deterministically.
    acquisitions.sort_by_key(|a| std::cmp::Reverse(a.precedence));

    let mut merged = TypeModel::new();
    let mut diagnostics: Vec<AcquisitionDiagnostic> = Vec::new();
    // name → the source that currently owns the winning definition.
    let mut owners: BTreeMap<String, Owner> = BTreeMap::new();

    for acquisition in &acquisitions {
        // Carry every adapter's own findings through unchanged (TR-011).
        diagnostics.extend(acquisition.diagnostics.iter().cloned());

        // Fold the partial model's named types in its deterministic order.
        for (name, node) in acquisition.model.iter_ordered() {
            match owners.get(name) {
                None => {
                    merged.insert_named(name.to_string(), node.clone());
                    owners.insert(
                        name.to_string(),
                        Owner {
                            source_id: acquisition.source_id.clone(),
                            precedence: acquisition.precedence,
                            node: node.clone(),
                        },
                    );
                    diagnostics.push(provenance_diagnostic(name, &acquisition.source_id));
                }
                Some(existing) => {
                    // The incumbent always wins (acquired at >= the incoming
                    // precedence, earlier in the stable order on ties), so the
                    // merged model is unchanged; only disagreements are recorded.
                    if let Some(conflict) = conflict_diagnostic(name, node, acquisition, existing) {
                        diagnostics.push(conflict);
                    }
                }
            }
        }

        // Preserve active RON extension flags surfaced by any source.
        for flag in &acquisition.model.ron_extensions_active {
            merged.add_active_extension(flag.clone());
        }
    }

    merged.diagnostics = diagnostics;
    merged
}

/// One acquired source: its identity, rank, partial model, and own diagnostics.
struct Acquisition {
    source_id: String,
    precedence: SourcePrecedence,
    model: TypeModel,
    diagnostics: Vec<AcquisitionDiagnostic>,
}

/// The source that currently owns a winning named-type definition.
struct Owner {
    source_id: String,
    precedence: SourcePrecedence,
    node: TypeNode,
}

/// Build the [`DiagnosticCategory::SourceConflict`] for a redefinition of an
/// already-owned named type, or `None` if the two definitions agree (TR-010).
///
/// Because acquisitions are processed highest-precedence-first, the incumbent
/// owner is always at >= the incoming source's precedence and therefore always
/// the winner:
///
/// - **Same shape:** no conflict (`None`).
/// - **Higher incumbent + different shape:** incumbent wins; the diagnostic names
///   winner (incumbent) and loser (incoming).
/// - **Equal precedence + different shape:** the incumbent (earlier in the stable
///   order) wins; the diagnostic flags the equal-precedence ambiguity.
fn conflict_diagnostic(
    name: &str,
    incoming: &TypeNode,
    acquisition: &Acquisition,
    existing: &Owner,
) -> Option<AcquisitionDiagnostic> {
    if existing.node == *incoming {
        // Sources agree on the shape — keep the incumbent silently.
        return None;
    }

    let detail = if existing.precedence == acquisition.precedence {
        format!(
            "type `{name}` is defined with conflicting shapes by two equal-precedence \
             sources `{winner}` and `{loser}` (both `{rank}`); keeping the first \
             (`{winner}`)",
            winner = existing.source_id,
            loser = acquisition.source_id,
            rank = existing.precedence,
        )
    } else {
        format!(
            "type `{name}` is defined with conflicting shapes by `{winner}` ({win_rank}) \
             and `{loser}` ({lose_rank}); higher-precedence `{winner}` wins",
            winner = existing.source_id,
            win_rank = existing.precedence,
            loser = acquisition.source_id,
            lose_rank = acquisition.precedence,
        )
    };

    Some(
        AcquisitionDiagnostic::new(DiagnosticCategory::SourceConflict, name.to_string(), detail)
            .with_source_id(existing.source_id.clone()),
    )
}

/// An `info` provenance diagnostic recording which source contributed a type.
fn provenance_diagnostic(name: &str, source_id: &str) -> AcquisitionDiagnostic {
    let mut diag = AcquisitionDiagnostic::new(
        DiagnosticCategory::UnresolvedType,
        name.to_string(),
        format!("type `{name}` contributed by source `{source_id}`"),
    )
    .with_source_id(source_id.to_string());
    // Provenance is informational, not a warning about an unresolved type.
    diag.severity = DiagnosticSeverity::Info;
    diag
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{NodeKind, Primitive, TypeRef};
    use crate::source::Acquired;

    /// A minimal in-test source contributing one named type of a given shape.
    struct StubSource {
        id: String,
        precedence: SourcePrecedence,
        type_name: String,
        node: TypeNode,
    }

    impl TypeSource for StubSource {
        fn source_id(&self) -> String {
            self.id.clone()
        }
        fn precedence(&self) -> SourcePrecedence {
            self.precedence
        }
        fn acquire(&self) -> Acquired {
            let mut model = TypeModel::new();
            model.insert_named(self.type_name.clone(), self.node.clone());
            Acquired {
                model,
                diagnostics: Vec::new(),
            }
        }
    }

    fn int_node() -> TypeNode {
        TypeNode::primitive(Primitive::Integer)
    }
    fn string_node() -> TypeNode {
        TypeNode::primitive(Primitive::String)
    }

    #[test]
    fn empty_sources_yield_empty_model() {
        let sources: Vec<Box<dyn TypeSource>> = Vec::new();
        let model = normalize(&sources);
        assert!(model.is_empty());
        assert!(model.diagnostics.is_empty());
    }

    #[test]
    fn single_source_passes_through_with_provenance() {
        let sources: Vec<Box<dyn TypeSource>> = vec![Box::new(StubSource {
            id: "syn".into(),
            precedence: SourcePrecedence::Syn,
            type_name: "Foo".into(),
            node: int_node(),
        })];
        let model = normalize(&sources);
        assert!(model.contains("Foo"));
        // Provenance recorded.
        assert!(model.diagnostics.iter().any(|d| d.subject == "Foo"
            && d.source_id.as_deref() == Some("syn")
            && d.severity == DiagnosticSeverity::Info));
    }

    #[test]
    fn higher_precedence_wins_on_conflict() {
        // syn says Foo: integer; user-schema says Foo: string. User wins.
        let sources: Vec<Box<dyn TypeSource>> = vec![
            Box::new(StubSource {
                id: "syn".into(),
                precedence: SourcePrecedence::Syn,
                type_name: "Foo".into(),
                node: int_node(),
            }),
            Box::new(StubSource {
                id: "user-schema".into(),
                precedence: SourcePrecedence::UserSchema,
                type_name: "Foo".into(),
                node: string_node(),
            }),
        ];
        let model = normalize(&sources);
        let foo = model.lookup("Foo").unwrap();
        assert!(matches!(
            foo.kind,
            NodeKind::Primitive {
                primitive: Primitive::String
            }
        ));
        let conflict = model
            .diagnostics
            .iter()
            .find(|d| d.category == DiagnosticCategory::SourceConflict)
            .expect("a source-conflict diagnostic is emitted");
        assert_eq!(conflict.subject, "Foo");
        assert!(conflict.detail.contains("user-schema"));
        assert!(conflict.detail.contains("syn"));
        // Winner is recorded as the diagnostic's source.
        assert_eq!(conflict.source_id.as_deref(), Some("user-schema"));
    }

    #[test]
    fn order_independent_result() {
        // Swapping the order of the same two sources yields the same winner.
        let build = |reverse: bool| {
            let syn: Box<dyn TypeSource> = Box::new(StubSource {
                id: "syn".into(),
                precedence: SourcePrecedence::Syn,
                type_name: "Foo".into(),
                node: int_node(),
            });
            let user: Box<dyn TypeSource> = Box::new(StubSource {
                id: "user-schema".into(),
                precedence: SourcePrecedence::UserSchema,
                type_name: "Foo".into(),
                node: string_node(),
            });
            let sources: Vec<Box<dyn TypeSource>> = if reverse {
                vec![user, syn]
            } else {
                vec![syn, user]
            };
            normalize(&sources)
        };
        let a = build(false);
        let b = build(true);
        assert_eq!(a.lookup("Foo"), b.lookup("Foo"));
    }

    #[test]
    fn agreeing_sources_emit_no_conflict() {
        let sources: Vec<Box<dyn TypeSource>> = vec![
            Box::new(StubSource {
                id: "syn".into(),
                precedence: SourcePrecedence::Syn,
                type_name: "Foo".into(),
                node: int_node(),
            }),
            Box::new(StubSource {
                id: "schemars".into(),
                precedence: SourcePrecedence::Schemars,
                type_name: "Foo".into(),
                node: int_node(),
            }),
        ];
        let model = normalize(&sources);
        assert!(!model
            .diagnostics
            .iter()
            .any(|d| d.category == DiagnosticCategory::SourceConflict));
    }

    #[test]
    fn disjoint_sources_union_all_types() {
        let sources: Vec<Box<dyn TypeSource>> = vec![
            Box::new(StubSource {
                id: "syn".into(),
                precedence: SourcePrecedence::Syn,
                type_name: "A".into(),
                node: int_node(),
            }),
            Box::new(StubSource {
                id: "user-schema".into(),
                precedence: SourcePrecedence::UserSchema,
                type_name: "B".into(),
                node: TypeNode::new(NodeKind::Sequence {
                    element: TypeRef::inline(string_node()),
                }),
            }),
        ];
        let model = normalize(&sources);
        assert!(model.contains("A"));
        assert!(model.contains("B"));
    }

    #[test]
    fn adapter_diagnostics_flow_through() {
        struct DiagSource;
        impl TypeSource for DiagSource {
            fn source_id(&self) -> String {
                "diag".into()
            }
            fn precedence(&self) -> SourcePrecedence {
                SourcePrecedence::Syn
            }
            fn acquire(&self) -> Acquired {
                let mut model = TypeModel::new();
                model.insert_named("X", TypeNode::unknown());
                let d =
                    AcquisitionDiagnostic::new(DiagnosticCategory::UnresolvedType, "X", "from syn")
                        .with_source_id("diag");
                model.diagnostics = vec![d.clone()];
                Acquired {
                    model,
                    diagnostics: vec![d],
                }
            }
        }
        let sources: Vec<Box<dyn TypeSource>> = vec![Box::new(DiagSource)];
        let model = normalize(&sources);
        assert!(model
            .diagnostics
            .iter()
            .any(|d| d.detail == "from syn" && d.source_id.as_deref() == Some("diag")));
    }
}
