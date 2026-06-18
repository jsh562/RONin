//! TR-009 / SC-004 — the [`TypeSource`] adapter contract is open/closed.
//!
//! A brand-new source type is defined entirely *here in the test* — RONin's
//! library code is untouched — and it both:
//!
//! 1. implements [`TypeSource`] producing a partial [`TypeModel`], and
//! 2. flows through [`normalize`] alongside a built-in adapter,
//!
//! with **no change** to [`TypeModel`], [`TypeNode`], or any existing adapter.
//! This proves the seam is genuinely extensible: a future Bevy-registry source
//! (or any other) plugs in the same way.

use ronin_types::diagnostics::{AcquisitionDiagnostic, DiagnosticCategory};
use ronin_types::model::{NodeKind, Primitive, TypeModel, TypeNode};
use ronin_types::normalize;
use ronin_types::source::{Acquired, SourcePrecedence};
use ronin_types::{SynSource, TypeSource};

/// A mock source representing a hypothetical external registry. It is defined
/// only in this test crate and depends on nothing source-specific in the
/// library — just the public [`TypeSource`] contract.
struct RegistryMockSource {
    types: Vec<(String, TypeNode)>,
}

impl TypeSource for RegistryMockSource {
    fn source_id(&self) -> String {
        "registry-mock".to_string()
    }

    fn precedence(&self) -> SourcePrecedence {
        // Reuses an existing precedence rank without adding a new variant.
        SourcePrecedence::Schemars
    }

    fn acquire(&self) -> Acquired {
        let mut model = TypeModel::new();
        for (name, node) in &self.types {
            model.insert_named(name.clone(), node.clone());
        }
        let diagnostics = vec![AcquisitionDiagnostic::new(
            DiagnosticCategory::UnresolvedType,
            "registry-note",
            "mock registry acquired its types",
        )
        .with_source_id("registry-mock")];
        Acquired { model, diagnostics }
    }
}

#[test]
fn mock_source_produces_partial_model() {
    let src = RegistryMockSource {
        types: vec![(
            "Widget".to_string(),
            TypeNode::primitive(Primitive::Integer),
        )],
    };
    let acquired = src.acquire();
    assert!(acquired.model.contains("Widget"));
    assert!(matches!(
        acquired.model.lookup("Widget").unwrap().kind,
        NodeKind::Primitive {
            primitive: Primitive::Integer
        }
    ));
    assert!(!acquired.diagnostics.is_empty());
}

#[test]
fn mock_source_flows_through_normalize_with_builtins() {
    // The mock source sits beside a real, library-provided SynSource. Both are
    // held as `dyn TypeSource` and merged without any adapter-specific handling.
    let syn = SynSource::from_source("struct Gadget { id: u32 }");
    let mock = RegistryMockSource {
        types: vec![("Widget".to_string(), TypeNode::primitive(Primitive::String))],
    };

    let sources: Vec<Box<dyn TypeSource>> = vec![Box::new(syn), Box::new(mock)];
    let model = normalize(&sources);

    // Both the built-in and the mock contributed their types.
    assert!(model.contains("Gadget"), "syn type present");
    assert!(model.contains("Widget"), "mock type present");

    // The mock's own diagnostic flowed into the merged model untouched.
    assert!(
        model.diagnostics.iter().any(
            |d| d.subject == "registry-note" && d.source_id.as_deref() == Some("registry-mock")
        ),
        "mock adapter diagnostics flow through normalize"
    );

    // Provenance for the mock-contributed type names the mock source.
    assert!(
        model
            .diagnostics
            .iter()
            .any(|d| d.subject == "Widget" && d.source_id.as_deref() == Some("registry-mock")),
        "provenance records the contributing source"
    );
}

#[test]
fn mock_source_can_outrank_lower_precedence_builtin() {
    // Same type defined by syn (low) and the mock (schemars-rank). The mock wins.
    let syn = SynSource::from_source("struct Widget { n: u32 }");
    let mock = RegistryMockSource {
        types: vec![("Widget".to_string(), TypeNode::primitive(Primitive::String))],
    };
    let sources: Vec<Box<dyn TypeSource>> = vec![Box::new(syn), Box::new(mock)];
    let model = normalize(&sources);

    // The higher-precedence mock's shape wins.
    assert!(matches!(
        model.lookup("Widget").unwrap().kind,
        NodeKind::Primitive {
            primitive: Primitive::String
        }
    ));
    assert!(
        model
            .diagnostics
            .iter()
            .any(|d| d.category == DiagnosticCategory::SourceConflict && d.subject == "Widget"),
        "the disagreement is recorded as a source conflict"
    );
}
